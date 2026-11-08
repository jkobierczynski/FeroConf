//! `feroconf backup`: fetch configs concurrently, store them one by one.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use feroconf_collect::{BackupError, Collector, Fetched, Target};
use feroconf_core::{ConfigStore, Inventory};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// Back up `names` (all devices when empty). Returns how many failed.
pub async fn run(inv: &Inventory, store: &ConfigStore, names: &[String]) -> Result<usize> {
    let devices: Vec<_> = if names.is_empty() {
        inv.devices.iter().collect()
    } else {
        names
            .iter()
            .map(|n| {
                inv.device(n)
                    .with_context(|| format!("{n} is not in the inventory"))
            })
            .collect::<Result<_>>()?
    };
    if devices.is_empty() {
        bail!("the inventory has no devices");
    }

    let listen = inv.backup.tftp_listen;
    let collector = Collector::start(inv.backup.clone()).await.map_err(|e| {
        let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
            "; port 69 needs root or CAP_NET_BIND_SERVICE \
             (sudo setcap cap_net_bind_service=+ep $(which feroconf))"
        } else {
            ""
        };
        anyhow::anyhow!("cannot start the TFTP server on {listen}: {e}{hint}")
    })?;
    let collector = Arc::new(collector);
    let limit = Arc::new(Semaphore::new(inv.backup.concurrency));

    let mut failed = 0;
    let mut jobs: JoinSet<(String, Result<Fetched, BackupError>)> = JoinSet::new();
    for device in devices {
        let target = match Target::from_inventory(inv, device) {
            Ok(t) => t,
            Err(e) => {
                println!("{}: FAILED: {e}", device.name);
                failed += 1;
                continue;
            }
        };
        let (collector, limit) = (collector.clone(), limit.clone());
        jobs.spawn(async move {
            let _permit = limit.acquire_owned().await.expect("semaphore open");
            let result = collector.backup(&target).await;
            (target.name, result)
        });
    }

    // The store is not thread-safe, so commits happen here, one at a time.
    while let Some(joined) = jobs.join_next().await {
        let (name, result) = joined.context("backup task panicked")?;
        match result.map_err(anyhow::Error::from).and_then(|f| {
            store
                .commit_config(&f.device, f.vendor, &f.config, &f.source())
                .map_err(Into::into)
        }) {
            Ok(Some(snap)) => println!("{name}: {} {}", snap.short_id(), snap.summary),
            Ok(None) => println!("{name}: no change"),
            Err(e) => {
                println!("{name}: FAILED: {e:#}");
                failed += 1;
            }
        }
    }
    Ok(failed)
}
