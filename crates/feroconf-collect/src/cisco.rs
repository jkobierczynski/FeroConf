//! Make a Cisco device upload its running-config to our TFTP server.
//!
//! The preferred way is CISCO-CONFIG-COPY-MIB: create a row in `ccCopyTable` that copies
//! `runningConfig` to a `networkFile` over TFTP, then poll `ccCopyState`. Old IOS releases that do
//! not have that MIB fall back to OLD-CISCO-SYS-MIB `writeNet` (IPv4 only).

use std::future::Future;
use std::net::IpAddr;
use std::time::Duration;

use tokio::time::sleep;

use crate::snmp::{status, SetValue, SnmpClient, SnmpError};
use crate::tftp::{TftpError, Upload};

/// CISCO-CONFIG-COPY-MIB::ccCopyEntry
pub const CC_COPY_ENTRY: [u64; 13] = [1, 3, 6, 1, 4, 1, 9, 9, 96, 1, 1, 1, 1];
const COL_PROTOCOL: u64 = 2;
const COL_SOURCE_FILE_TYPE: u64 = 3;
const COL_DEST_FILE_TYPE: u64 = 4;
const COL_SERVER_ADDRESS: u64 = 5;
const COL_FILE_NAME: u64 = 6;
const COL_STATE: u64 = 10;
const COL_FAIL_CAUSE: u64 = 13;
const COL_ROW_STATUS: u64 = 14;
const COL_SERVER_ADDRESS_TYPE: u64 = 15;
const COL_SERVER_ADDRESS_REV1: u64 = 16;

const PROTOCOL_TFTP: i64 = 1;
const FILE_TYPE_NETWORK: i64 = 1;
const FILE_TYPE_RUNNING_CONFIG: i64 = 4;
const INET_ADDRESS_IPV6: i64 = 2;
const ROW_CREATE_AND_GO: i64 = 4;
const ROW_DESTROY: i64 = 6;

const STATE_SUCCESSFUL: i64 = 3;
const STATE_FAILED: i64 = 4;

/// OLD-CISCO-SYS-MIB::writeNet, indexed by the TFTP server's IPv4 address.
pub const WRITE_NET: [u64; 10] = [1, 3, 6, 1, 4, 1, 9, 2, 1, 55];

/// How long to wait for the upload after the device reports success.
const UPLOAD_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMethod {
    ConfigCopyMib,
    WriteNet,
}

impl CopyMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            CopyMethod::ConfigCopyMib => "CISCO-CONFIG-COPY-MIB",
            CopyMethod::WriteNet => "OLD-CISCO-SYS-MIB writeNet",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    #[error(transparent)]
    Snmp(#[from] SnmpError),
    #[error(transparent)]
    Tftp(#[from] TftpError),
    #[error("the device reports the copy failed: {0}")]
    Failed(&'static str),
    #[error(
        "the device reports success but no upload arrived (is tftp_server reachable from it?)"
    )]
    NoUpload,
    #[error("{0}")]
    Unsupported(String),
}

/// `ccCopyFailCause` as text.
pub fn fail_cause(v: Option<i64>) -> &'static str {
    match v {
        Some(2) => "bad file name",
        Some(3) => "timeout (the device could not reach the TFTP server)",
        Some(4) => "not enough memory",
        Some(5) => "no config",
        Some(6) => "unsupported protocol",
        Some(7) => "some config apply failed",
        Some(8) => "system not ready",
        Some(9) => "request aborted",
        _ => "unknown cause",
    }
}

fn col(column: u64, row: u32) -> Vec<u64> {
    let mut o = CC_COPY_ENTRY.to_vec();
    o.push(column);
    o.push(u64::from(row));
    o
}

/// Ask the device to upload its running-config as `filename` to `tftp_server`, and wait for the
/// upload. `upload` must be the expectation registered for `filename`.
///
/// Callers should put an overall time limit around this.
pub async fn fetch_running_config<C, U>(
    client: &mut C,
    tftp_server: IpAddr,
    filename: &str,
    upload: U,
    poll_interval: Duration,
) -> Result<(Upload, CopyMethod), CopyError>
where
    C: SnmpClient,
    U: Future<Output = Result<Upload, TftpError>>,
{
    let row = random_row();
    match start_config_copy(client, row, tftp_server, filename).await {
        Ok(()) => {
            let result = wait_config_copy(client, row, upload, poll_interval).await;
            // Best effort: the device also ages the row out after a few minutes.
            let _ = client
                .set(&[(&col(COL_ROW_STATUS, row), SetValue::Integer(ROW_DESTROY))])
                .await;
            result.map(|u| (u, CopyMethod::ConfigCopyMib))
        }
        Err(e) if is_missing_mib(&e) => match tftp_server {
            IpAddr::V4(v4) => write_net(client, v4.octets(), filename, upload)
                .await
                .map(|u| (u, CopyMethod::WriteNet)),
            IpAddr::V6(_) => Err(CopyError::Unsupported(
                "the device has no CISCO-CONFIG-COPY-MIB, and writeNet only supports IPv4".into(),
            )),
        },
        Err(e) => Err(e.into()),
    }
}

fn is_missing_mib(e: &SnmpError) -> bool {
    matches!(
        e.status(),
        Some(status::NO_SUCH_NAME | status::NOT_WRITABLE | status::NO_CREATION)
    )
}

async fn start_config_copy<C: SnmpClient>(
    client: &mut C,
    row: u32,
    server: IpAddr,
    filename: &str,
) -> Result<(), SnmpError> {
    let (c_proto, c_src, c_dst, c_name, c_row) = (
        col(COL_PROTOCOL, row),
        col(COL_SOURCE_FILE_TYPE, row),
        col(COL_DEST_FILE_TYPE, row),
        col(COL_FILE_NAME, row),
        col(COL_ROW_STATUS, row),
    );
    let (c_addr, c_addr_type, c_addr_rev1) = (
        col(COL_SERVER_ADDRESS, row),
        col(COL_SERVER_ADDRESS_TYPE, row),
        col(COL_SERVER_ADDRESS_REV1, row),
    );
    let mut vbs: Vec<(&[u64], SetValue)> = vec![
        (&c_proto, SetValue::Integer(PROTOCOL_TFTP)),
        (&c_src, SetValue::Integer(FILE_TYPE_RUNNING_CONFIG)),
        (&c_dst, SetValue::Integer(FILE_TYPE_NETWORK)),
    ];
    match server {
        // The deprecated IpAddress column works on every IOS release that has the MIB.
        IpAddr::V4(v4) => vbs.push((&c_addr, SetValue::IpAddress(v4.octets()))),
        IpAddr::V6(v6) => {
            vbs.push((&c_addr_type, SetValue::Integer(INET_ADDRESS_IPV6)));
            vbs.push((&c_addr_rev1, SetValue::OctetString(v6.octets().to_vec())));
        }
    }
    vbs.push((&c_name, SetValue::OctetString(filename.as_bytes().to_vec())));
    vbs.push((&c_row, SetValue::Integer(ROW_CREATE_AND_GO)));
    client.set(&vbs).await
}

async fn wait_config_copy<C, U>(
    client: &mut C,
    row: u32,
    upload: U,
    poll_interval: Duration,
) -> Result<Upload, CopyError>
where
    C: SnmpClient,
    U: Future<Output = Result<Upload, TftpError>>,
{
    let state_oid = col(COL_STATE, row);
    tokio::pin!(upload);
    let mut received: Option<Upload> = None;
    loop {
        tokio::select! {
            r = &mut upload, if received.is_none() => {
                received = Some(r?);
            }
            _ = sleep(poll_interval) => {
                match client.get_integer(&state_oid).await? {
                    Some(STATE_SUCCESSFUL) => {
                        return match received {
                            Some(u) => Ok(u),
                            None => match tokio::time::timeout(UPLOAD_GRACE, &mut upload).await {
                                Ok(r) => Ok(r?),
                                Err(_) => Err(CopyError::NoUpload),
                            },
                        };
                    }
                    Some(STATE_FAILED) => {
                        let cause = client.get_integer(&col(COL_FAIL_CAUSE, row)).await.ok().flatten();
                        return Err(CopyError::Failed(fail_cause(cause)));
                    }
                    // waiting(1) or running(2). A missing row is treated the same, since some
                    // releases only show it once the copy is under way.
                    _ => {}
                }
            }
        }
    }
}

async fn write_net<C, U>(
    client: &mut C,
    server: [u8; 4],
    filename: &str,
    upload: U,
) -> Result<Upload, CopyError>
where
    C: SnmpClient,
    U: Future<Output = Result<Upload, TftpError>>,
{
    let mut oid = WRITE_NET.to_vec();
    oid.extend(server.iter().map(|b| u64::from(*b)));
    tokio::pin!(upload);
    let vbs = [(
        oid.as_slice(),
        SetValue::OctetString(filename.as_bytes().to_vec()),
    )];
    let set = client.set(&vbs);
    tokio::pin!(set);
    // Old IOS answers the SET only after the transfer, so the upload may finish first and the SET
    // may even time out. The upload is what counts.
    let mut set_done = false;
    loop {
        tokio::select! {
            r = &mut upload => return Ok(r?),
            r = &mut set, if !set_done => {
                set_done = true;
                match r {
                    Ok(()) | Err(SnmpError::Timeout(_)) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
}

fn random_row() -> u32 {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    (u32::from_ne_bytes(b) & 0x7fff_ffff).max(1)
}
