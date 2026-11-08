//! A small SNMP client interface plus its implementation on top of the `snmp2` crate.
//!
//! The backup logic only needs SET and integer GET, so it talks to [`SnmpClient`]. That keeps the
//! Cisco copy logic testable without a real agent.

use std::future::Future;
use std::time::Duration;

use feroconf_core::inventory::{AuthProtocol, PrivProtocol};
use feroconf_core::secret::SecretError;
use feroconf_core::SnmpCredentials;
use snmp2::{v3, AsyncSession, Oid, Value};
use tokio::time::timeout;

/// SNMP error-status values (RFC 3416) that the backup logic looks at.
pub mod status {
    pub const NO_ERROR: u32 = 0;
    pub const NO_SUCH_NAME: u32 = 2;
    pub const NO_ACCESS: u32 = 6;
    pub const NOT_WRITABLE: u32 = 17;
    pub const NO_CREATION: u32 = 11;
    pub const AUTHORIZATION_ERROR: u32 = 16;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetValue {
    Integer(i64),
    OctetString(Vec<u8>),
    IpAddress([u8; 4]),
}

#[derive(Debug, thiserror::Error)]
pub enum SnmpError {
    #[error("no SNMP response from {0} (check the address, credentials and any SNMP ACL)")]
    Timeout(String),
    #[error("the agent answered with error-status {status} at varbind {index}")]
    Status { status: u32, index: u32 },
    #[error("SNMP: {0}")]
    Protocol(String),
    #[error("SNMP credentials: {0}")]
    Secret(#[from] SecretError),
    #[error("SNMP: {0}")]
    Config(String),
    #[error("cannot reach {0}: {1}")]
    Io(String, std::io::Error),
}

impl SnmpError {
    /// The SNMP error-status, when the agent answered with one.
    pub fn status(&self) -> Option<u32> {
        match self {
            SnmpError::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
}

pub trait SnmpClient {
    /// Set all values in one PDU.
    fn set(&mut self, values: &[(&[u64], SetValue)])
        -> impl Future<Output = Result<(), SnmpError>>;

    /// Get an INTEGER. `Ok(None)` means the object or instance does not exist.
    fn get_integer(&mut self, oid: &[u64]) -> impl Future<Output = Result<Option<i64>, SnmpError>>;
}

/// [`SnmpClient`] for a real agent.
pub struct Snmp2Client {
    session: AsyncSession,
    target: String,
    timeout: Duration,
    retries: u32,
}

impl Snmp2Client {
    pub async fn connect(
        host: &str,
        port: u16,
        creds: &SnmpCredentials,
        timeout_per_try: Duration,
        retries: u32,
    ) -> Result<Self, SnmpError> {
        let target = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let req_id = random_req_id();
        let io_err = |e| SnmpError::Io(target.clone(), e);
        let session = match creds {
            SnmpCredentials::V2c {
                write_community, ..
            } => {
                let community = write_community.resolve()?;
                AsyncSession::new_v2c(target.as_str(), community.as_bytes(), req_id)
                    .await
                    .map_err(io_err)?
            }
            SnmpCredentials::V3 {
                user,
                auth_protocol,
                auth_password,
                priv_protocol,
                priv_password,
            } => {
                let auth_pass = auth_password.as_ref().map(|s| s.resolve()).transpose()?;
                let mut sec = v3::Security::new(
                    user.as_bytes(),
                    auth_pass.as_deref().unwrap_or("").as_bytes(),
                );
                sec = match (auth_protocol, priv_protocol, priv_password) {
                    (None, _, _) => sec.with_auth(v3::Auth::NoAuthNoPriv),
                    (Some(a), None, _) => sec
                        .with_auth_protocol(map_auth(*a))
                        .with_auth(v3::Auth::AuthNoPriv),
                    (Some(a), Some(p), Some(pp)) => {
                        sec.with_auth_protocol(map_auth(*a))
                            .with_auth(v3::Auth::AuthPriv {
                                cipher: map_priv(*p),
                                privacy_password: pp.resolve()?.into_bytes(),
                            })
                    }
                    (Some(_), Some(_), None) => {
                        return Err(SnmpError::Config(
                            "priv_protocol without priv_password".into(),
                        ))
                    }
                };
                let mut s = AsyncSession::new_v3(target.as_str(), req_id, sec)
                    .await
                    .map_err(io_err)?;
                // Discover the engine ID and time.
                let mut last = None;
                for _ in 0..=retries {
                    match tokio::time::timeout(timeout_per_try, s.init()).await {
                        Ok(Ok(())) => {
                            last = None;
                            break;
                        }
                        Ok(Err(e)) => last = Some(SnmpError::Protocol(format!("{e:?}"))),
                        Err(_) => last = Some(SnmpError::Timeout(target.clone())),
                    }
                }
                if let Some(e) = last {
                    return Err(e);
                }
                s
            }
        };
        Ok(Self {
            session,
            target,
            timeout: timeout_per_try,
            retries,
        })
    }
}

impl SnmpClient for Snmp2Client {
    async fn set(&mut self, values: &[(&[u64], SetValue)]) -> Result<(), SnmpError> {
        let oids: Vec<Oid<'static>> = values
            .iter()
            .map(|(o, _)| make_oid(o))
            .collect::<Result<_, _>>()?;
        let vbs: Vec<(&Oid<'_>, Value<'_>)> = oids
            .iter()
            .zip(values)
            .map(|(oid, (_, v))| {
                let v = match v {
                    SetValue::Integer(i) => Value::Integer(*i),
                    SetValue::OctetString(s) => Value::OctetString(s),
                    SetValue::IpAddress(a) => Value::IpAddress(*a),
                };
                (oid, v)
            })
            .collect();
        // A SET is not retried: when only the response was lost, resending a createAndGo
        // would fail on the row that now exists. Wait as long as all retries would instead.
        let wait = self.timeout * (self.retries + 1);
        let pdu = match timeout(wait, self.session.set(&vbs)).await {
            Err(_) => return Err(SnmpError::Timeout(self.target.clone())),
            Ok(Err(e)) => return Err(SnmpError::Protocol(format!("{e:?}"))),
            Ok(Ok(pdu)) => pdu,
        };
        if pdu.error_status != status::NO_ERROR {
            return Err(SnmpError::Status {
                status: pdu.error_status,
                index: pdu.error_index,
            });
        }
        Ok(())
    }

    async fn get_integer(&mut self, oid: &[u64]) -> Result<Option<i64>, SnmpError> {
        let oid = make_oid(oid)?;
        let mut last = SnmpError::Timeout(self.target.clone());
        for _ in 0..=self.retries {
            match timeout(self.timeout, self.session.get(&oid)).await {
                Err(_) => last = SnmpError::Timeout(self.target.clone()),
                Ok(Err(e)) => last = SnmpError::Protocol(format!("{e:?}")),
                Ok(Ok(mut pdu)) => {
                    if pdu.error_status == status::NO_SUCH_NAME {
                        return Ok(None);
                    }
                    if pdu.error_status != status::NO_ERROR {
                        return Err(SnmpError::Status {
                            status: pdu.error_status,
                            index: pdu.error_index,
                        });
                    }
                    return match pdu.varbinds.next() {
                        Some((_, Value::Integer(i))) => Ok(Some(i)),
                        Some((_, Value::NoSuchObject | Value::NoSuchInstance)) | None => Ok(None),
                        Some((_, other)) => Err(SnmpError::Protocol(format!(
                            "expected an INTEGER, got {other:?}"
                        ))),
                    };
                }
            }
        }
        Err(last)
    }
}

fn make_oid(parts: &[u64]) -> Result<Oid<'static>, SnmpError> {
    Oid::from(parts).map_err(|e| SnmpError::Config(format!("bad OID {parts:?}: {e:?}")))
}

fn map_auth(a: AuthProtocol) -> v3::AuthProtocol {
    match a {
        AuthProtocol::Md5 => v3::AuthProtocol::Md5,
        AuthProtocol::Sha => v3::AuthProtocol::Sha1,
        AuthProtocol::Sha224 => v3::AuthProtocol::Sha224,
        AuthProtocol::Sha256 => v3::AuthProtocol::Sha256,
        AuthProtocol::Sha384 => v3::AuthProtocol::Sha384,
        AuthProtocol::Sha512 => v3::AuthProtocol::Sha512,
    }
}

fn map_priv(p: PrivProtocol) -> v3::Cipher {
    match p {
        PrivProtocol::Des => v3::Cipher::Des,
        PrivProtocol::Aes128 => v3::Cipher::Aes128,
        PrivProtocol::Aes192 => v3::Cipher::Aes192,
        PrivProtocol::Aes256 => v3::Cipher::Aes256,
    }
}

fn random_req_id() -> i32 {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    (i32::from_ne_bytes(b) & 0x3fff_ffff).max(1)
}
