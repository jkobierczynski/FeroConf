//! Collectors that fetch configs from devices.
//!
//! Right now that is SNMP+TFTP for Cisco: [`Collector`] runs a built-in [`tftp`] server and uses
//! SNMP SETs ([`cisco`]) to make each device upload its running-config to it.

pub mod cisco;
pub mod snmp;
pub mod tftp;

use std::io;
use std::net::IpAddr;
use std::time::Duration;

use feroconf_core::inventory::{BackupSettings, Inventory};
use feroconf_core::{Device, SnmpCredentials, Vendor};

use crate::cisco::{CopyError, CopyMethod};
use crate::snmp::{Snmp2Client, SnmpClient, SnmpError};
use crate::tftp::TftpReceiver;

const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Everything needed to back up one device, with the inventory defaults applied.
#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub host: String,
    pub vendor: Vendor,
    pub snmp_port: u16,
    pub snmp: SnmpCredentials,
    pub tftp_server: IpAddr,
}

impl Target {
    pub fn from_inventory(inv: &Inventory, device: &Device) -> Result<Self, BackupError> {
        let r = inv.resolve(device);
        Ok(Self {
            name: device.name.clone(),
            host: device.host.clone(),
            vendor: device.vendor,
            snmp_port: device.snmp_port.unwrap_or(161),
            snmp: r.snmp.cloned().ok_or(BackupError::NoSnmp)?,
            tftp_server: r.tftp_server.ok_or(BackupError::NoTftpServer)?,
        })
    }
}

/// A config fetched from a device, not yet stored.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub device: String,
    pub vendor: Vendor,
    pub config: String,
    pub method: CopyMethod,
}

impl Fetched {
    /// For the commit message.
    pub fn source(&self) -> String {
        format!("snmp+tftp ({})", self.method.as_str())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("no SNMP credentials configured")]
    NoSnmp,
    #[error("no tftp_server address configured")]
    NoTftpServer,
    #[error(transparent)]
    Snmp(#[from] SnmpError),
    #[error(transparent)]
    Copy(#[from] CopyError),
    #[error("backup did not finish within {0} seconds")]
    Timeout(u64),
    #[error("the uploaded config is empty")]
    Empty,
}

pub struct Collector {
    tftp: TftpReceiver,
    settings: BackupSettings,
}

impl Collector {
    /// Start the TFTP server on `settings.tftp_listen`.
    pub async fn start(settings: BackupSettings) -> io::Result<Self> {
        let tftp = TftpReceiver::bind(settings.tftp_listen).await?;
        Ok(Self { tftp, settings })
    }

    pub fn tftp_addr(&self) -> std::net::SocketAddr {
        self.tftp.local_addr()
    }

    /// Back up one device over SNMP.
    pub async fn backup(&self, target: &Target) -> Result<Fetched, BackupError> {
        let limit = self.settings.timeout_secs;
        tokio::time::timeout(Duration::from_secs(limit), async {
            let mut client = Snmp2Client::connect(
                &target.host,
                target.snmp_port,
                &target.snmp,
                Duration::from_millis(self.settings.snmp_timeout_ms),
                self.settings.snmp_retries,
            )
            .await?;
            self.fetch_with(&mut client, target, POLL_INTERVAL).await
        })
        .await
        .map_err(|_| BackupError::Timeout(limit))?
    }

    /// Back up one device through any [`SnmpClient`] (used by tests). No overall time limit.
    pub async fn fetch_with<C: SnmpClient>(
        &self,
        client: &mut C,
        target: &Target,
        poll_interval: Duration,
    ) -> Result<Fetched, BackupError> {
        let filename = format!("feroconf-{}-{}.cfg", target.name, token());
        let upload = self.tftp.expect(filename.clone());
        let (upload, method) = cisco::fetch_running_config(
            client,
            target.tftp_server,
            &filename,
            upload,
            poll_interval,
        )
        .await?;
        if upload.data.iter().all(u8::is_ascii_whitespace) {
            return Err(BackupError::Empty);
        }
        Ok(Fetched {
            device: target.name.clone(),
            vendor: target.vendor,
            config: String::from_utf8_lossy(&upload.data).into_owned(),
            method,
        })
    }
}

/// 128 random bits as hex, so upload names cannot be guessed.
fn token() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("system random number generator");
    b.iter().map(|x| format!("{x:02x}")).collect()
}
