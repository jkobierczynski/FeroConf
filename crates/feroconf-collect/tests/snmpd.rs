//! Checks the real SNMP client against a Net-SNMP agent (`scripts/fake-cisco/run.sh` starts one).
//! Skipped unless `FEROCONF_TEST_SNMPD=host:port` is set. The agent needs:
//!
//! ```text
//! rwcommunity private 127.0.0.1
//! createUser fero SHA-256 authpass123 AES privpass123
//! rwuser fero priv
//! ```

use std::time::Duration;

use feroconf_collect::snmp::{SetValue, Snmp2Client, SnmpClient, SnmpError};
use feroconf_core::inventory::{AuthProtocol, PrivProtocol};
use feroconf_core::secret::Secret;
use feroconf_core::SnmpCredentials;

const SYS_CONTACT: [u64; 9] = [1, 3, 6, 1, 2, 1, 1, 4, 0];
const ENGINE_BOOTS: [u64; 11] = [1, 3, 6, 1, 6, 3, 10, 2, 1, 2, 0];
const MISSING: [u64; 9] = [1, 3, 6, 1, 2, 1, 1, 99, 0];

fn agent() -> Option<(String, u16)> {
    let v = std::env::var("FEROCONF_TEST_SNMPD").ok()?;
    let (h, p) = v.rsplit_once(':')?;
    Some((h.to_string(), p.parse().ok()?))
}

fn v2c(community: &str) -> SnmpCredentials {
    SnmpCredentials::V2c {
        write_community: Secret::Inline(community.into()),
        read_community: None,
    }
}

async fn exercise(creds: SnmpCredentials) {
    let Some((host, port)) = agent() else {
        eprintln!("FEROCONF_TEST_SNMPD not set, skipping");
        return;
    };
    let mut c = Snmp2Client::connect(&host, port, &creds, Duration::from_secs(1), 1)
        .await
        .unwrap();
    c.set(&[(
        &SYS_CONTACT,
        SetValue::OctetString(b"noc@example.net".to_vec()),
    )])
    .await
    .unwrap();
    let boots = c.get_integer(&ENGINE_BOOTS).await.unwrap();
    assert!(boots.is_some_and(|b| b >= 1), "{boots:?}");
    assert_eq!(c.get_integer(&MISSING).await.unwrap(), None);
}

#[tokio::test]
async fn v2c_set_and_get() {
    exercise(v2c("private")).await;
}

#[tokio::test]
async fn v3_auth_priv_set_and_get() {
    exercise(SnmpCredentials::V3 {
        user: "fero".into(),
        auth_protocol: Some(AuthProtocol::Sha256),
        auth_password: Some(Secret::Inline("authpass123".into())),
        priv_protocol: Some(PrivProtocol::Aes128),
        priv_password: Some(Secret::Inline("privpass123".into())),
    })
    .await;
}

#[tokio::test]
async fn wrong_community_times_out() {
    let Some((host, port)) = agent() else { return };
    let mut c = Snmp2Client::connect(&host, port, &v2c("wrong"), Duration::from_millis(300), 0)
        .await
        .unwrap();
    let err = c.get_integer(&ENGINE_BOOTS).await.unwrap_err();
    assert!(matches!(err, SnmpError::Timeout(_)), "{err}");
}
