//! FeroConf core library.
//!
//! This crate holds everything the CLI (and later the GUI) share:
//!
//! - [`inventory`]: the TOML device inventory and SNMP credentials.
//! - [`secret`]: secret values that can live inline, in an environment variable or in a file.
//! - [`normalize`]: per-vendor rules that strip volatile lines (timestamps, byte counts) from configs.
//! - [`store`]: a Git-backed store that commits a config snapshot only when it actually changed.
//! - [`diff`]: unified diffs between two config texts.

pub mod diff;
pub mod inventory;
pub mod normalize;
pub mod secret;
pub mod store;

pub use inventory::{BackupSettings, Device, Inventory, SnmpCredentials, Vendor};
pub use store::{ConfigStore, Snapshot};
