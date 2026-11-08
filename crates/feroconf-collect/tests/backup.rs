//! The Cisco copy flow against a simulated device: the fake answers SNMP like IOS does and
//! uploads its config to the real TFTP server over UDP.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use feroconf_collect::cisco::{CopyError, CopyMethod, CC_COPY_ENTRY, WRITE_NET};
use feroconf_collect::snmp::{status, SetValue, SnmpClient, SnmpError};
use feroconf_collect::{BackupError, Collector, Target};
use feroconf_core::inventory::BackupSettings;
use feroconf_core::secret::Secret;
use feroconf_core::{SnmpCredentials, Vendor};

const CONFIG: &str = "Building configuration...\r\n!\r\nhostname r1\r\nend\r\n";

#[derive(Clone, Copy, PartialEq)]
enum Behavior {
    /// Modern IOS: ccCopyTable works.
    CopyMib,
    /// Old IOS: no ccCopyTable, writeNet works.
    WriteNetOnly,
    /// ccCopyState goes to failed(4) with ccCopyFailCause timeout(3).
    CopyFails,
    /// Reports success but never uploads.
    SilentSuccess,
}

/// Every SET PDU the fake received, in order.
type SetLog = Arc<Mutex<Vec<Vec<(Vec<u64>, SetValue)>>>>;

struct FakeCisco {
    behavior: Behavior,
    tftp: SocketAddr,
    /// (row, state) of the copy in progress.
    row: Option<(u64, i64)>,
    sets: SetLog,
}

impl FakeCisco {
    fn upload(&self, filename: String) {
        let tftp = self.tftp;
        tokio::spawn(async move {
            common::tftp_put(tftp, &filename, CONFIG.as_bytes(), None)
                .await
                .unwrap();
        });
    }
}

impl SnmpClient for FakeCisco {
    async fn set(&mut self, values: &[(&[u64], SetValue)]) -> Result<(), SnmpError> {
        self.sets.lock().unwrap().push(
            values
                .iter()
                .map(|(o, v)| (o.to_vec(), v.clone()))
                .collect(),
        );
        let first = values[0].0;
        if first.starts_with(&CC_COPY_ENTRY) {
            if self.behavior == Behavior::WriteNetOnly {
                return Err(SnmpError::Status {
                    status: status::NO_SUCH_NAME,
                    index: 1,
                });
            }
            let row = *first.last().unwrap();
            let col = first[CC_COPY_ENTRY.len()];
            if col == 14 {
                return Ok(()); // destroy
            }
            let filename = values
                .iter()
                .find_map(|(o, v)| match v {
                    SetValue::OctetString(s) if o[CC_COPY_ENTRY.len()] == 6 => {
                        Some(String::from_utf8(s.clone()).unwrap())
                    }
                    _ => None,
                })
                .expect("file name column");
            match self.behavior {
                Behavior::CopyMib => {
                    self.upload(filename);
                    self.row = Some((row, 3));
                }
                Behavior::CopyFails => self.row = Some((row, 4)),
                Behavior::SilentSuccess => self.row = Some((row, 3)),
                Behavior::WriteNetOnly => unreachable!(),
            }
            Ok(())
        } else if first.starts_with(&WRITE_NET) {
            assert_eq!(&first[WRITE_NET.len()..], &[127, 0, 0, 1]);
            let SetValue::OctetString(name) = &values[0].1 else {
                panic!("writeNet takes a file name")
            };
            self.upload(String::from_utf8(name.clone()).unwrap());
            Ok(())
        } else {
            panic!("unexpected SET {first:?}")
        }
    }

    async fn get_integer(&mut self, oid: &[u64]) -> Result<Option<i64>, SnmpError> {
        let Some((row, state)) = self.row else {
            return Ok(None);
        };
        assert_eq!(*oid.last().unwrap(), row);
        match oid[CC_COPY_ENTRY.len()] {
            10 => Ok(Some(state)),
            13 => Ok(Some(3)),
            c => panic!("unexpected GET of column {c}"),
        }
    }
}

async fn setup(behavior: Behavior) -> (Collector, FakeCisco, Target) {
    let settings = BackupSettings {
        tftp_listen: "127.0.0.1:0".parse().unwrap(),
        ..BackupSettings::default()
    };
    let collector = Collector::start(settings).await.unwrap();
    let fake = FakeCisco {
        behavior,
        tftp: collector.tftp_addr(),
        row: None,
        sets: Arc::default(),
    };
    let target = Target {
        name: "r1".into(),
        host: "127.0.0.1".into(),
        vendor: Vendor::CiscoIos,
        snmp_port: 161,
        snmp: SnmpCredentials::V2c {
            write_community: Secret::Inline("private".into()),
            read_community: None,
        },
        tftp_server: "127.0.0.1".parse().unwrap(),
    };
    (collector, fake, target)
}

const POLL: Duration = Duration::from_millis(20);

#[tokio::test]
async fn backs_up_with_config_copy_mib() {
    let (collector, mut fake, target) = setup(Behavior::CopyMib).await;
    let sets = fake.sets.clone();
    let got = collector
        .fetch_with(&mut fake, &target, POLL)
        .await
        .unwrap();
    assert_eq!(got.method, CopyMethod::ConfigCopyMib);
    assert_eq!(got.config, CONFIG);

    let sets = sets.lock().unwrap();
    // createAndGo row, then destroy.
    assert_eq!(sets.len(), 2);
    let create = &sets[0];
    let col = |c: u64| {
        create
            .iter()
            .find(|(o, _)| o[CC_COPY_ENTRY.len()] == c)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(col(2), Some(SetValue::Integer(1)), "protocol tftp");
    assert_eq!(col(3), Some(SetValue::Integer(4)), "source runningConfig");
    assert_eq!(col(4), Some(SetValue::Integer(1)), "dest networkFile");
    assert_eq!(col(5), Some(SetValue::IpAddress([127, 0, 0, 1])));
    assert_eq!(
        create.last().unwrap().1,
        SetValue::Integer(4),
        "createAndGo last"
    );
    assert_eq!(sets[1][0].1, SetValue::Integer(6), "row destroyed");
}

#[tokio::test]
async fn falls_back_to_write_net() {
    let (collector, mut fake, target) = setup(Behavior::WriteNetOnly).await;
    let got = collector
        .fetch_with(&mut fake, &target, POLL)
        .await
        .unwrap();
    assert_eq!(got.method, CopyMethod::WriteNet);
    assert_eq!(got.config, CONFIG);
}

#[tokio::test]
async fn reports_device_failure_cause() {
    let (collector, mut fake, target) = setup(Behavior::CopyFails).await;
    let err = collector
        .fetch_with(&mut fake, &target, POLL)
        .await
        .unwrap_err();
    assert!(
        matches!(err, BackupError::Copy(CopyError::Failed(c)) if c.starts_with("timeout")),
        "{err}"
    );
}

#[tokio::test(start_paused = true)]
async fn success_without_upload_is_an_error() {
    let (collector, mut fake, target) = setup(Behavior::SilentSuccess).await;
    let err = collector
        .fetch_with(&mut fake, &target, POLL)
        .await
        .unwrap_err();
    assert!(
        matches!(err, BackupError::Copy(CopyError::NoUpload)),
        "{err}"
    );
}
