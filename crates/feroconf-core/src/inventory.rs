//! The device inventory, loaded from a TOML file.
//!
//! ```toml
//! [defaults]
//! tftp_server = "192.0.2.10"   # address the routers use to reach FeroConf's TFTP server
//!
//! [defaults.snmp]
//! version = "v2c"
//! write_community = { env = "FEROCONF_RW_COMMUNITY" }
//!
//! [[device]]
//! name = "core-rtr1"
//! host = "192.0.2.1"
//! vendor = "cisco-ios"
//! ```
//!
//! A device may override `tftp_server` and `snmp`. Its `snmp` table replaces the default one
//! as a whole, it is not merged field by field.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use serde::Deserialize;

use crate::secret::Secret;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub backup: BackupSettings,
    #[serde(default, rename = "device")]
    pub devices: Vec<Device>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    pub tftp_server: Option<IpAddr>,
    pub snmp: Option<SnmpCredentials>,
}

/// The `[backup]` section: how FeroConf runs SNMP+TFTP backups.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BackupSettings {
    /// Where the built-in TFTP server listens. Routers always upload to port 69.
    pub tftp_listen: SocketAddr,
    /// How long one device's backup may take, from the first SNMP request to the stored file.
    pub timeout_secs: u64,
    /// Time to wait for each SNMP response.
    pub snmp_timeout_ms: u64,
    /// SNMP retries after the first attempt.
    pub snmp_retries: u32,
    /// How many devices are backed up at the same time.
    pub concurrency: usize,
}

impl Default for BackupSettings {
    fn default() -> Self {
        Self {
            tftp_listen: SocketAddr::from(([0, 0, 0, 0], 69)),
            timeout_secs: 120,
            snmp_timeout_ms: 2000,
            snmp_retries: 2,
            concurrency: 8,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    /// Unique name; also used as the directory name in the config store.
    pub name: String,
    /// Management IP address or hostname.
    pub host: String,
    #[serde(default)]
    pub vendor: Vendor,
    pub tftp_server: Option<IpAddr>,
    pub snmp: Option<SnmpCredentials>,
    /// SNMP agent port, 161 when not set.
    pub snmp_port: Option<u16>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Vendor {
    #[default]
    CiscoIos,
    CiscoIosXe,
    CiscoNxos,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "version", rename_all = "lowercase", deny_unknown_fields)]
pub enum SnmpCredentials {
    /// SNMPv2c. The write community is needed to trigger config copies.
    V2c {
        write_community: Secret,
        read_community: Option<Secret>,
    },
    /// SNMPv3 (USM).
    V3 {
        user: String,
        auth_protocol: Option<AuthProtocol>,
        auth_password: Option<Secret>,
        priv_protocol: Option<PrivProtocol>,
        priv_password: Option<Secret>,
    },
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AuthProtocol {
    Md5,
    Sha,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PrivProtocol {
    Des,
    Aes128,
    Aes192,
    Aes256,
}

#[derive(Debug, thiserror::Error)]
pub enum InventoryError {
    #[error("cannot read inventory {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid inventory: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid inventory: {0}")]
    Invalid(String),
}

/// A device with the defaults applied.
#[derive(Debug, Clone)]
pub struct ResolvedDevice<'a> {
    pub device: &'a Device,
    pub tftp_server: Option<IpAddr>,
    pub snmp: Option<&'a SnmpCredentials>,
}

impl Inventory {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, InventoryError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| InventoryError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, InventoryError> {
        let inv: Inventory = toml::from_str(text)?;
        inv.validate()?;
        Ok(inv)
    }

    fn validate(&self) -> Result<(), InventoryError> {
        let mut seen = HashSet::new();
        for d in &self.devices {
            if !is_valid_name(&d.name) {
                return Err(InventoryError::Invalid(format!(
                    "device name {:?} may only contain letters, digits, '.', '-' and '_'",
                    d.name
                )));
            }
            if !seen.insert(d.name.as_str()) {
                return Err(InventoryError::Invalid(format!(
                    "device name {:?} is used more than once",
                    d.name
                )));
            }
            if d.host.trim().is_empty() {
                return Err(InventoryError::Invalid(format!(
                    "device {:?} has an empty host",
                    d.name
                )));
            }
            if let Some(SnmpCredentials::V3 {
                auth_protocol,
                auth_password,
                priv_protocol,
                priv_password,
                ..
            }) = &d.snmp
            {
                check_v3(
                    &d.name,
                    auth_protocol,
                    auth_password,
                    priv_protocol,
                    priv_password,
                )?;
            }
        }
        if self.backup.concurrency == 0 {
            return Err(InventoryError::Invalid(
                "backup.concurrency must be at least 1".into(),
            ));
        }
        if self.backup.timeout_secs == 0 || self.backup.snmp_timeout_ms == 0 {
            return Err(InventoryError::Invalid(
                "backup timeouts must be greater than 0".into(),
            ));
        }
        if let Some(SnmpCredentials::V3 {
            auth_protocol,
            auth_password,
            priv_protocol,
            priv_password,
            ..
        }) = &self.defaults.snmp
        {
            check_v3(
                "defaults",
                auth_protocol,
                auth_password,
                priv_protocol,
                priv_password,
            )?;
        }
        Ok(())
    }

    pub fn device(&self, name: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.name == name)
    }

    /// Apply the `[defaults]` section to a device.
    pub fn resolve<'a>(&'a self, device: &'a Device) -> ResolvedDevice<'a> {
        ResolvedDevice {
            device,
            tftp_server: device.tftp_server.or(self.defaults.tftp_server),
            snmp: device.snmp.as_ref().or(self.defaults.snmp.as_ref()),
        }
    }

    /// Non-fatal problems worth telling the user about.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        for d in &self.devices {
            let r = self.resolve(d);
            if r.snmp.is_none() {
                out.push(format!(
                    "{}: no SNMP credentials (set [defaults.snmp] or [device.snmp])",
                    d.name
                ));
            }
            if r.tftp_server.is_none() {
                out.push(format!("{}: no tftp_server address set", d.name));
            }
            if r.snmp.is_some_and(has_inline_secret) {
                out.push(format!(
                    "{}: SNMP secret is stored inline; consider {{ env = \"...\" }} or {{ file = \"...\" }}",
                    d.name
                ));
            }
        }
        out
    }
}

fn has_inline_secret(c: &SnmpCredentials) -> bool {
    match c {
        SnmpCredentials::V2c {
            write_community,
            read_community,
        } => write_community.is_inline() || read_community.as_ref().is_some_and(Secret::is_inline),
        SnmpCredentials::V3 {
            auth_password,
            priv_password,
            ..
        } => {
            auth_password.as_ref().is_some_and(Secret::is_inline)
                || priv_password.as_ref().is_some_and(Secret::is_inline)
        }
    }
}

fn check_v3(
    who: &str,
    auth_protocol: &Option<AuthProtocol>,
    auth_password: &Option<Secret>,
    priv_protocol: &Option<PrivProtocol>,
    priv_password: &Option<Secret>,
) -> Result<(), InventoryError> {
    if auth_protocol.is_some() != auth_password.is_some() {
        return Err(InventoryError::Invalid(format!(
            "{who}: SNMPv3 auth_protocol and auth_password must be set together"
        )));
    }
    if priv_protocol.is_some() != priv_password.is_some() {
        return Err(InventoryError::Invalid(format!(
            "{who}: SNMPv3 priv_protocol and priv_password must be set together"
        )));
    }
    if priv_protocol.is_some() && auth_protocol.is_none() {
        return Err(InventoryError::Invalid(format!(
            "{who}: SNMPv3 privacy requires authentication (authPriv)"
        )));
    }
    Ok(())
}

/// Names become directory names in the store, so keep them filesystem- and Git-safe.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        [defaults]
        tftp_server = "192.0.2.10"

        [defaults.snmp]
        version = "v2c"
        write_community = { env = "FEROCONF_RW_COMMUNITY" }

        [[device]]
        name = "core-rtr1"
        host = "192.0.2.1"

        [[device]]
        name = "edge-rtr2"
        host = "192.0.2.2"
        vendor = "cisco-ios-xe"
        tftp_server = "198.51.100.5"

        [device.snmp]
        version = "v3"
        user = "feroconf"
        auth_protocol = "sha256"
        auth_password = { file = "/etc/feroconf/auth" }
        priv_protocol = "aes128"
        priv_password = { file = "/etc/feroconf/priv" }
    "#;

    #[test]
    fn parses_and_resolves_defaults() {
        let inv = Inventory::parse(SAMPLE).unwrap();
        assert_eq!(inv.devices.len(), 2);

        let r1 = inv.resolve(inv.device("core-rtr1").unwrap());
        assert_eq!(r1.device.vendor, Vendor::CiscoIos);
        assert_eq!(r1.tftp_server, Some("192.0.2.10".parse().unwrap()));
        assert!(matches!(r1.snmp, Some(SnmpCredentials::V2c { .. })));

        let r2 = inv.resolve(inv.device("edge-rtr2").unwrap());
        assert_eq!(r2.tftp_server, Some("198.51.100.5".parse().unwrap()));
        assert!(matches!(r2.snmp, Some(SnmpCredentials::V3 { .. })));
        assert!(inv.warnings().is_empty(), "{:?}", inv.warnings());
    }

    #[test]
    fn backup_section_defaults_and_overrides() {
        let inv = Inventory::parse("").unwrap();
        assert_eq!(inv.backup.tftp_listen, "0.0.0.0:69".parse().unwrap());
        assert_eq!(inv.backup.concurrency, 8);

        let inv =
            Inventory::parse("[backup]\ntftp_listen = \"127.0.0.1:6969\"\ntimeout_secs = 30\n")
                .unwrap();
        assert_eq!(inv.backup.tftp_listen, "127.0.0.1:6969".parse().unwrap());
        assert_eq!(inv.backup.timeout_secs, 30);
        assert_eq!(inv.backup.snmp_retries, 2);

        assert!(Inventory::parse("[backup]\nconcurrency = 0\n").is_err());
    }

    #[test]
    fn rejects_duplicate_names() {
        let t = r#"
            [[device]]
            name = "a"
            host = "1.1.1.1"
            [[device]]
            name = "a"
            host = "1.1.1.2"
        "#;
        assert!(Inventory::parse(t).is_err());
    }

    #[test]
    fn rejects_unsafe_names() {
        for bad in ["../etc", ".git", "a/b", ""] {
            let t = format!("[[device]]\nname = {bad:?}\nhost = \"1.1.1.1\"\n");
            assert!(Inventory::parse(&t).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn rejects_priv_without_auth() {
        let t = r#"
            [[device]]
            name = "a"
            host = "1.1.1.1"
            [device.snmp]
            version = "v3"
            user = "u"
            priv_protocol = "aes128"
            priv_password = "x"
        "#;
        assert!(Inventory::parse(t).is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let t = "[[device]]\nname = \"a\"\nhost = \"1.1.1.1\"\ncomunity = \"typo\"\n";
        assert!(Inventory::parse(t).is_err());
    }

    #[test]
    fn warns_on_inline_secret_and_missing_settings() {
        let t = r#"
            [[device]]
            name = "a"
            host = "1.1.1.1"
            [device.snmp]
            version = "v2c"
            write_community = "private"
            [[device]]
            name = "b"
            host = "1.1.1.2"
        "#;
        let w = Inventory::parse(t).unwrap().warnings();
        assert!(w
            .iter()
            .any(|m| m.starts_with("a:") && m.contains("inline")));
        assert!(w
            .iter()
            .any(|m| m.starts_with("b:") && m.contains("no SNMP")));
    }
}
