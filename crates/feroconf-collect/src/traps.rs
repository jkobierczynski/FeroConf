//! Receive SNMP traps (v1 and v2c) and recognize Cisco config-change notifications.
//!
//! A Cisco device sends `ciscoConfigManEvent` (CISCO-CONFIG-MAN-MIB) when its config is written
//! somewhere: after leaving config mode (destination `running`), on `write memory` (`startup`),
//! and also when the running-config is copied to a TFTP/FTP/SCP server. That last case is what
//! our own backups do, so [`classify`] reports it as [`TrapEvent::ConfigCopiedOut`] and callers
//! must not back up again on it, or every backup would trigger the next one.

use std::io;
use std::net::{IpAddr, SocketAddr};

use snmp2::{MessageType, Oid, Pdu, Value};
use tokio::net::UdpSocket;

/// SNMPv2-MIB::snmpTrapOID.0, the varbind that names a v2c trap.
pub const SNMP_TRAP_OID: [u64; 11] = [1, 3, 6, 1, 6, 3, 1, 1, 4, 1, 0];
/// CISCO-CONFIG-MAN-MIB::ciscoConfigManEvent
pub const CISCO_CONFIG_MAN_EVENT: [u64; 12] = [1, 3, 6, 1, 4, 1, 9, 9, 43, 2, 0, 1];
/// CISCO-CONFIG-MAN-MIB::ccmCLIRunningConfigChanged
pub const CCM_CLI_RUNNING_CONFIG_CHANGED: [u64; 12] = [1, 3, 6, 1, 4, 1, 9, 9, 43, 2, 0, 2];
/// CISCO-CONFIG-MAN-MIB::ccmHistoryEventConfigDestination (column; the row index follows).
pub const CCM_HISTORY_EVENT_CONFIG_DESTINATION: [u64; 14] =
    [1, 3, 6, 1, 4, 1, 9, 9, 43, 1, 1, 6, 1, 5];
/// SNMPv2-MIB::snmpTraps, the prefix of the generic v1 traps in v2 form.
const SNMP_TRAPS: [u64; 9] = [1, 3, 6, 1, 6, 3, 1, 1, 5];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrapVersion {
    V1,
    V2c,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrapValue {
    Integer(i64),
    Text(String),
    Oid(Vec<u64>),
    Other(String),
}

/// A received trap. v1 traps are converted to the v2 form (RFC 3584), so `trap_oid` always
/// names the notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trap {
    pub source: IpAddr,
    pub version: TrapVersion,
    pub community: Vec<u8>,
    pub trap_oid: Vec<u64>,
    /// v1 only: the agent address inside the PDU.
    pub agent_addr: Option<IpAddr>,
    pub varbinds: Vec<(Vec<u64>, TrapValue)>,
}

impl Trap {
    /// Human-readable name of the notification, or its dotted OID.
    pub fn name(&self) -> String {
        trap_name(&self.trap_oid)
            .map(String::from)
            .unwrap_or_else(|| dotted(&self.trap_oid))
    }

    /// The INTEGER value of the first varbind whose OID starts with `prefix`.
    pub fn integer(&self, prefix: &[u64]) -> Option<i64> {
        self.varbinds.iter().find_map(|(oid, v)| match v {
            TrapValue::Integer(i) if oid.starts_with(prefix) => Some(*i),
            _ => None,
        })
    }
}

/// What a trap means for FeroConf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrapEvent {
    /// The config changed (or was saved): back it up. `destination` is the
    /// `ccmHistoryEventConfigDestination` value when the trap carries one.
    ConfigChanged { destination: Option<i64> },
    /// The config was copied to a network server (a backup, maybe our own). Nothing changed.
    ConfigCopiedOut,
    /// Anything else, such as linkDown or coldStart.
    Other,
}

pub fn classify(trap: &Trap) -> TrapEvent {
    if trap.trap_oid == CISCO_CONFIG_MAN_EVENT {
        match trap.integer(&CCM_HISTORY_EVENT_CONFIG_DESTINATION) {
            // networkTftp(6), networkRcp(7), networkFtp(8), networkScp(9)
            Some(6..=9) => TrapEvent::ConfigCopiedOut,
            destination => TrapEvent::ConfigChanged { destination },
        }
    } else if trap.trap_oid == CCM_CLI_RUNNING_CONFIG_CHANGED {
        TrapEvent::ConfigChanged {
            destination: Some(3),
        }
    } else {
        TrapEvent::Other
    }
}

/// `ccmHistoryEventConfigDestination` as text.
pub fn config_destination_name(v: i64) -> &'static str {
    match v {
        1 => "erase",
        2 => "commandSource",
        3 => "running",
        4 => "startup",
        5 => "local",
        6 => "networkTftp",
        7 => "networkRcp",
        8 => "networkFtp",
        9 => "networkScp",
        _ => "unknown",
    }
}

pub fn trap_name(oid: &[u64]) -> Option<&'static str> {
    if oid == CISCO_CONFIG_MAN_EVENT {
        return Some("ciscoConfigManEvent");
    }
    if oid == CCM_CLI_RUNNING_CONFIG_CHANGED {
        return Some("ccmCLIRunningConfigChanged");
    }
    if oid.len() == SNMP_TRAPS.len() + 1 && oid.starts_with(&SNMP_TRAPS) {
        return match oid[SNMP_TRAPS.len()] {
            1 => Some("coldStart"),
            2 => Some("warmStart"),
            3 => Some("linkDown"),
            4 => Some("linkUp"),
            5 => Some("authenticationFailure"),
            6 => Some("egpNeighborLoss"),
            _ => None,
        };
    }
    None
}

pub fn dotted(oid: &[u64]) -> String {
    oid.iter().map(u64::to_string).collect::<Vec<_>>().join(".")
}

fn oid_parts(oid: &Oid<'_>) -> Vec<u64> {
    oid.iter().map(|i| i.collect()).unwrap_or_default()
}

fn convert(v: &Value<'_>) -> TrapValue {
    match v {
        Value::Integer(i) => TrapValue::Integer(*i),
        Value::Counter32(i) | Value::Unsigned32(i) | Value::Timeticks(i) => {
            TrapValue::Integer(i64::from(*i))
        }
        Value::OctetString(s) => TrapValue::Text(String::from_utf8_lossy(s).into_owned()),
        Value::ObjectIdentifier(o) => TrapValue::Oid(oid_parts(o)),
        Value::IpAddress(a) => TrapValue::Text(IpAddr::from(*a).to_string()),
        other => TrapValue::Other(format!("{other:?}")),
    }
}

/// Parse a datagram. `Ok(None)` for valid SNMP that is not a trap (an inform, a GET...).
pub fn parse(bytes: &[u8], source: IpAddr) -> Result<Option<Trap>, String> {
    let pdu =
        Pdu::from_bytes(bytes).map_err(|e| format!("not a valid SNMP v1/v2c message: {e:?}"))?;
    let community = pdu.community.to_vec();
    match pdu.message_type {
        MessageType::TrapV1 => {
            let info = pdu
                .v1_trap_info
                .as_ref()
                .ok_or("v1 trap without trap info")?;
            let trap_oid = if (0..=5).contains(&info.generic_trap) {
                let mut o = SNMP_TRAPS.to_vec();
                o.push(info.generic_trap as u64 + 1);
                o
            } else {
                let mut o = oid_parts(&info.enterprise);
                o.push(0);
                o.push(info.specific_trap.max(0) as u64);
                o
            };
            let varbinds = pdu
                .varbinds
                .map(|(oid, v)| (oid_parts(&oid), convert(&v)))
                .collect();
            Ok(Some(Trap {
                source,
                version: TrapVersion::V1,
                community,
                trap_oid,
                agent_addr: Some(info.agent_addr),
                varbinds,
            }))
        }
        MessageType::Trap => {
            let mut trap_oid = None;
            let mut varbinds = Vec::new();
            for (oid, v) in pdu.varbinds {
                let oid = oid_parts(&oid);
                match (&v, oid.as_slice() == SNMP_TRAP_OID) {
                    (Value::ObjectIdentifier(t), true) => trap_oid = Some(oid_parts(t)),
                    _ => varbinds.push((oid, convert(&v))),
                }
            }
            Ok(Some(Trap {
                source,
                version: TrapVersion::V2c,
                community,
                trap_oid: trap_oid.ok_or("v2c trap without snmpTrapOID.0")?,
                agent_addr: None,
                varbinds,
            }))
        }
        _ => Ok(None),
    }
}

/// What the listener got.
#[derive(Debug)]
pub enum Received {
    Trap(Trap),
    /// A datagram that is not a v1/v2c trap, with the reason.
    Ignored {
        source: SocketAddr,
        reason: String,
    },
}

pub struct TrapListener {
    socket: UdpSocket,
    buf: Vec<u8>,
}

impl TrapListener {
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        Ok(Self {
            socket: UdpSocket::bind(addr).await?,
            buf: vec![0; 65_535],
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub async fn recv(&mut self) -> io::Result<Received> {
        loop {
            let (n, from) = match self.socket.recv_from(&mut self.buf).await {
                Ok(v) => v,
                // Windows reports ICMP errors from earlier sends here; keep listening.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
                Err(e) => return Err(e),
            };
            return Ok(match parse(&self.buf[..n], from.ip()) {
                Ok(Some(t)) => Received::Trap(t),
                Ok(None) => Received::Ignored {
                    source: from,
                    reason: "not a trap (informs and SNMPv3 are not supported yet)".into(),
                },
                Err(reason) => Received::Ignored {
                    source: from,
                    reason,
                },
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // Captured from Net-SNMP `snmptrap`, see the test names for what each one is.
    const V2C_CONFIG_RUNNING: &str = "308188020101040874726170636f6d6da779020431a5cd2a020100020100306b300f06082b06010201010300430301b4cd3019060a2b060106030101040100060b2b0601040109092b0200013013060e2b0601040109092b0101060103050201013013060e2b0601040109092b0101060104050201023013060e2b0601040109092b010106010505020103";
    const V1_CONFIG_TO_TFTP: &str = "3042020100040874726170636f6d6da43306092b0601040109092b024004c0000201020106020101430301b4ce30153013060e2b0601040109092b010106010505020106";
    const V2C_LINK_DOWN: &str = "3058020101040874726170636f6d6da749020401ab4461020100020100303b300f06082b06010201010300430301b4ce3017060a2b06010603010104010006092b0601060301010503300f060a2b060102010202010102020102";

    fn src() -> IpAddr {
        "192.0.2.1".parse().unwrap()
    }

    #[test]
    fn v2c_config_change_to_running() {
        let t = parse(&hex(V2C_CONFIG_RUNNING), src()).unwrap().unwrap();
        assert_eq!(t.version, TrapVersion::V2c);
        assert_eq!(t.community, b"trapcomm");
        assert_eq!(t.trap_oid, CISCO_CONFIG_MAN_EVENT);
        assert_eq!(t.name(), "ciscoConfigManEvent");
        assert_eq!(
            classify(&t),
            TrapEvent::ConfigChanged {
                destination: Some(3)
            }
        );
    }

    #[test]
    fn v1_copy_to_tftp_is_not_a_change() {
        let t = parse(&hex(V1_CONFIG_TO_TFTP), src()).unwrap().unwrap();
        assert_eq!(t.version, TrapVersion::V1);
        assert_eq!(t.trap_oid, CISCO_CONFIG_MAN_EVENT, "enterprise.0.specific");
        assert_eq!(t.agent_addr, Some("192.0.2.1".parse().unwrap()));
        assert_eq!(classify(&t), TrapEvent::ConfigCopiedOut);
    }

    #[test]
    fn link_down_is_other() {
        let t = parse(&hex(V2C_LINK_DOWN), src()).unwrap().unwrap();
        assert_eq!(t.name(), "linkDown");
        assert_eq!(classify(&t), TrapEvent::Other);
        assert_eq!(t.integer(&[1, 3, 6, 1, 2, 1, 2, 2, 1, 1]), Some(2));
    }

    #[test]
    fn config_change_without_destination_still_counts() {
        let t = Trap {
            source: src(),
            version: TrapVersion::V2c,
            community: vec![],
            trap_oid: CISCO_CONFIG_MAN_EVENT.to_vec(),
            agent_addr: None,
            varbinds: vec![],
        };
        assert_eq!(classify(&t), TrapEvent::ConfigChanged { destination: None });
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(parse(b"hello", src()).is_err());
    }
}
