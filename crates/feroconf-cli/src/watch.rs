//! `feroconf watch`: listen for SNMP traps and back up routers whose config changed.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use feroconf_collect::schedule::Scheduler;
use feroconf_collect::traps::{self, Received, Trap, TrapEvent, TrapListener};
use feroconf_collect::{BackupError, Collector, Fetched, Target};
use feroconf_core::{ConfigStore, Inventory};
use tokio::sync::{mpsc, Semaphore};

fn log(msg: impl AsRef<str>) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let t = crate::format_time(secs);
    let s = secs.rem_euclid(60);
    println!("{t}:{s:02} {}", msg.as_ref());
}

fn bind_hint(e: &std::io::Error, port: u16) -> String {
    if e.kind() == std::io::ErrorKind::PermissionDenied && port < 1024 {
        format!(
            "; port {port} needs root or CAP_NET_BIND_SERVICE \
             (sudo setcap cap_net_bind_service=+ep $(which feroconf))"
        )
    } else {
        String::new()
    }
}

/// Map every address a device's host resolves to onto the device name.
async fn resolve_sources(inv: &Inventory) -> HashMap<IpAddr, String> {
    let mut map = HashMap::new();
    for d in &inv.devices {
        let lookup = if let Ok(ip) = d.host.parse::<IpAddr>() {
            Ok(vec![ip])
        } else {
            tokio::net::lookup_host((d.host.as_str(), 0))
                .await
                .map(|it| it.map(|a| a.ip()).collect())
        };
        match lookup {
            Ok(ips) => {
                for ip in ips {
                    map.insert(ip, d.name.clone());
                }
            }
            Err(e) => log(format!(
                "{}: cannot resolve {} ({e}); its traps will be ignored",
                d.name, d.host
            )),
        }
    }
    map
}

pub async fn run(inv: &Inventory, store: &ConfigStore) -> Result<()> {
    let settings = &inv.watch;
    let collector = Collector::start(inv.backup.clone()).await.map_err(|e| {
        let listen = inv.backup.tftp_listen;
        anyhow::anyhow!(
            "cannot start the TFTP server on {listen}: {e}{}",
            bind_hint(&e, listen.port())
        )
    })?;
    let collector = Arc::new(collector);
    let mut listener = TrapListener::bind(settings.trap_listen)
        .await
        .map_err(|e| {
            let listen = settings.trap_listen;
            anyhow::anyhow!(
                "cannot listen for traps on {listen}: {e}{}",
                bind_hint(&e, listen.port())
            )
        })?;
    let community = settings
        .trap_community
        .as_ref()
        .map(|s| s.resolve())
        .transpose()
        .context("watch.trap_community")?;
    if community.is_none() {
        log("warning: watch.trap_community is not set, traps with any community are accepted");
    }

    let sources = resolve_sources(inv).await;
    let mut targets = HashMap::new();
    for d in &inv.devices {
        match Target::from_inventory(inv, d) {
            Ok(t) => {
                targets.insert(d.name.clone(), t);
            }
            Err(e) => log(format!("{}: cannot be backed up: {e}", d.name)),
        }
    }
    anyhow::ensure!(
        !targets.is_empty(),
        "no device in the inventory can be backed up"
    );

    log(format!(
        "watching {} device(s): traps on {}, TFTP on {}",
        targets.len(),
        listener.local_addr()?,
        collector.tftp_addr()
    ));

    let debounce = Duration::from_secs(settings.debounce_secs);
    let mut sched = Scheduler::new(debounce);
    let limit = Arc::new(Semaphore::new(inv.backup.concurrency));
    let (done_tx, mut done_rx) =
        mpsc::unbounded_channel::<(String, String, Result<Fetched, BackupError>)>();

    // Start with a full backup, so later changes have something to be compared with.
    let now = Instant::now();
    for name in targets.keys() {
        sched.trigger_now(name, "startup", now);
    }
    let mut interval = settings.interval_mins.map(|m| {
        let period = Duration::from_secs(m * 60);
        tokio::time::interval_at(tokio::time::Instant::now() + period, period)
    });

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    loop {
        for (name, reason) in sched.take_due(Instant::now()) {
            let target = targets[&name].clone();
            let (collector, limit, done_tx) = (collector.clone(), limit.clone(), done_tx.clone());
            tokio::spawn(async move {
                let _permit = limit.acquire_owned().await.expect("semaphore open");
                let result = collector.backup(&target).await;
                let _ = done_tx.send((target.name, reason, result));
            });
        }

        let wake = sched
            .next_due()
            .map(tokio::time::Instant::from_std)
            .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));

        tokio::select! {
            _ = &mut shutdown => {
                log("stopping");
                return Ok(());
            }
            r = listener.recv() => match r? {
                Received::Trap(trap) => on_trap(&trap, community.as_deref(), &sources, &targets, &mut sched),
                Received::Ignored { source, reason } => log(format!("ignored datagram from {source}: {reason}")),
            },
            Some((name, reason, result)) = done_rx.recv() => {
                sched.finished(&name);
                let stored = result.map_err(anyhow::Error::from).and_then(|f| {
                    let source = format!("{}, triggered by {reason}", f.source());
                    store.commit_config(&f.device, f.vendor, &f.config, &source).map_err(Into::into)
                });
                match stored {
                    Ok(Some(s)) => log(format!("{name}: {} {}", s.short_id(), s.summary)),
                    Ok(None) => log(format!("{name}: no change")),
                    Err(e) => log(format!("{name}: backup FAILED: {e:#}")),
                }
            }
            _ = tokio::time::sleep_until(wake) => {}
            _ = async { interval.as_mut().expect("guarded").tick().await }, if interval.is_some() => {
                let now = Instant::now();
                for name in targets.keys() {
                    sched.trigger_now(name, "scheduled backup", now);
                }
            }
        }
    }
}

fn on_trap(
    trap: &Trap,
    community: Option<&str>,
    sources: &HashMap<IpAddr, String>,
    targets: &HashMap<String, Target>,
    sched: &mut Scheduler,
) {
    if community.is_some_and(|c| c.as_bytes() != trap.community.as_slice()) {
        log(format!(
            "ignored {} from {}: wrong community",
            trap.name(),
            trap.source
        ));
        return;
    }
    let device = sources
        .get(&trap.source)
        .or_else(|| trap.agent_addr.and_then(|a| sources.get(&a)));
    let Some(device) = device.filter(|d| targets.contains_key(*d)) else {
        log(format!(
            "ignored {} from {}: not a device in the inventory",
            trap.name(),
            trap.source
        ));
        return;
    };

    match traps::classify(trap) {
        TrapEvent::ConfigChanged { destination } => {
            let what = destination
                .map(traps::config_destination_name)
                .unwrap_or("unknown destination");
            if destination.is_none() && sched.is_running(device) {
                // Probably our own copy, from a device that leaves out the destination.
                log(format!(
                    "{device}: {} ({what}) during a backup, ignored",
                    trap.name()
                ));
                return;
            }
            let reason = format!("{} trap ({what})", trap.name());
            sched.trigger(device, &reason, Instant::now());
            log(format!(
                "{device}: {reason}, backup in {}s",
                sched.debounce().as_secs()
            ));
        }
        TrapEvent::ConfigCopiedOut => log(format!(
            "{device}: {} (config copied to a server), no backup needed",
            trap.name()
        )),
        TrapEvent::Other => log(format!("{device}: trap {}", trap.name())),
    }
}
