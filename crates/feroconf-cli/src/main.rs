use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use feroconf_core::{diff, ConfigStore, Inventory, Vendor};

mod backup;
mod watch;

/// FeroConf: back up, version and diff network device configs.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Inventory file.
    #[arg(
        short,
        long,
        global = true,
        env = "FEROCONF_INVENTORY",
        default_value = "feroconf.toml"
    )]
    inventory: PathBuf,

    /// Directory of the Git-backed config store (created on first use).
    #[arg(
        short,
        long,
        global = true,
        env = "FEROCONF_STORE",
        default_value = "feroconf-store"
    )]
    store: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate the inventory and list its devices.
    Check,
    /// Store a config read from a file (or "-" for stdin) as a new snapshot.
    Import {
        device: String,
        file: PathBuf,
        /// Where the config came from, recorded in the commit message.
        #[arg(long, default_value = "manual import")]
        source: String,
    },
    /// List the stored snapshots of a device, newest first.
    Log { device: String },
    /// Print a stored config.
    Show {
        device: String,
        /// Revision to show (commit id, HEAD~1, tag...). Defaults to the latest.
        #[arg(long)]
        rev: Option<String>,
    },
    /// Show what changed in a device's config.
    ///
    /// With no revisions: the previous snapshot against the latest.
    /// With one: that revision against the latest. With two: FROM against TO.
    Diff {
        device: String,
        from: Option<String>,
        to: Option<String>,
        /// Lines of context around each change.
        #[arg(short = 'U', long, default_value_t = 3)]
        context: usize,
    },
    /// List the devices that have stored configs.
    Devices,
    /// Fetch configs from devices over SNMP+TFTP and store the ones that changed.
    ///
    /// Runs FeroConf's own TFTP server on `backup.tftp_listen` (port 69 by default, which needs
    /// root or CAP_NET_BIND_SERVICE) while the backups run.
    Backup {
        /// Devices to back up. All devices in the inventory when none are given.
        devices: Vec<String>,
    },
    /// Run until stopped: back up every device at start, then again whenever it sends a
    /// config-change SNMP trap, and on `watch.interval_mins` if set.
    Watch,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Check => {
            let inv = Inventory::load(&cli.inventory)?;
            for d in &inv.devices {
                let r = inv.resolve(d);
                let snmp = match r.snmp {
                    Some(feroconf_core::SnmpCredentials::V2c { .. }) => "snmp v2c",
                    Some(feroconf_core::SnmpCredentials::V3 { .. }) => "snmp v3",
                    None => "no snmp",
                };
                let tftp = r
                    .tftp_server
                    .map(|a| format!("tftp {a}"))
                    .unwrap_or_else(|| "no tftp".into());
                println!(
                    "{:<24} {:<20} {:<14} {snmp}, {tftp}",
                    d.name,
                    d.host,
                    vendor_name(d.vendor)
                );
            }
            let warnings = inv.warnings();
            for w in &warnings {
                eprintln!("warning: {w}");
            }
            println!(
                "{} device(s), {} warning(s)",
                inv.devices.len(),
                warnings.len()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Import {
            device,
            file,
            source,
        } => {
            let vendor = vendor_of(&cli.inventory, &device)?;
            let raw = if file.as_os_str() == "-" {
                std::io::read_to_string(std::io::stdin()).context("reading stdin")?
            } else {
                std::fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))?
            };
            let store = ConfigStore::open_or_init(&cli.store)?;
            match store.commit_config(&device, vendor, &raw, &source)? {
                Some(s) => println!("{} {}", s.short_id(), s.summary),
                None => println!("{device}: no change"),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Log { device } => {
            let store = ConfigStore::open_or_init(&cli.store)?;
            let hist = store.history(&device)?;
            if hist.is_empty() {
                bail!("no stored config for {device}");
            }
            for s in hist {
                println!("{}  {}  {}", s.short_id(), format_time(s.time), s.summary);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Show { device, rev } => {
            let store = ConfigStore::open_or_init(&cli.store)?;
            let text = match rev {
                Some(r) => store.config_at(&device, &r)?,
                None => store.latest(&device)?,
            };
            print!("{text}");
            Ok(ExitCode::SUCCESS)
        }
        Command::Diff {
            device,
            from,
            to,
            context,
        } => {
            let store = ConfigStore::open_or_init(&cli.store)?;
            let (from, to) = match (from, to) {
                (Some(f), Some(t)) => (f, t),
                (Some(f), None) => (f, "HEAD".to_string()),
                (None, _) => {
                    let hist = store.history(&device)?;
                    match hist.as_slice() {
                        [] => bail!("no stored config for {device}"),
                        [_] => bail!("{device} has only one snapshot, nothing to compare"),
                        [latest, prev, ..] => (prev.id.clone(), latest.id.clone()),
                    }
                }
            };
            let old = store.config_at(&device, &from)?;
            let new = store.config_at(&device, &to)?;
            let label = |r: &str| format!("{device}@{}", &r[..r.len().min(10)]);
            let d = diff::unified(&old, &new, &label(&from), &label(&to), context);
            print!("{d}");
            // Like diff(1): exit 1 when there are differences.
            Ok(if d.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Command::Devices => {
            let store = ConfigStore::open_or_init(&cli.store)?;
            for d in store.devices()? {
                println!("{d}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Watch => {
            let inv = Inventory::load(&cli.inventory)?;
            let store = ConfigStore::open_or_init(&cli.store)?;
            tokio::runtime::Runtime::new()?.block_on(watch::run(&inv, &store))?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Backup { devices } => {
            let inv = Inventory::load(&cli.inventory)?;
            let store = ConfigStore::open_or_init(&cli.store)?;
            let failed =
                tokio::runtime::Runtime::new()?.block_on(backup::run(&inv, &store, &devices))?;
            Ok(if failed == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
    }
}

/// The vendor from the inventory, or Cisco IOS when there is no inventory or the device is not
/// in it (useful for importing a config before the inventory exists).
fn vendor_of(inventory: &PathBuf, device: &str) -> Result<Vendor> {
    if !inventory.exists() {
        return Ok(Vendor::default());
    }
    let inv = Inventory::load(inventory)?;
    Ok(inv.device(device).map(|d| d.vendor).unwrap_or_default())
}

fn vendor_name(v: Vendor) -> &'static str {
    match v {
        Vendor::CiscoIos => "cisco-ios",
        Vendor::CiscoIosXe => "cisco-ios-xe",
        Vendor::CiscoNxos => "cisco-nxos",
    }
}

/// UTC time as `YYYY-MM-DD HH:MM`, without pulling in a date crate.
pub(crate) fn format_time(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days, Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::format_time;

    #[test]
    fn formats_utc() {
        assert_eq!(format_time(0), "1970-01-01 00:00");
        assert_eq!(format_time(1_794_096_000), "2026-11-08 00:00");
    }
}
