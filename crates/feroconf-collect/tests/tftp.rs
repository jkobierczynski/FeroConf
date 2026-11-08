mod common;

use std::time::Duration;

use common::tftp_put;
use feroconf_collect::tftp::TftpReceiver;
use tokio::net::UdpSocket;
use tokio::time::timeout;

async fn receiver() -> TftpReceiver {
    TftpReceiver::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn receives_expected_upload() {
    let rx = receiver().await;
    let up = rx.expect("r1.cfg");
    let data = b"hostname r1\nend\n".to_vec();
    tftp_put(rx.local_addr(), "r1.cfg", &data, None)
        .await
        .unwrap();
    let got = timeout(Duration::from_secs(5), up).await.unwrap().unwrap();
    assert_eq!(got.data, data);
}

#[tokio::test]
async fn handles_multiple_blocks_and_exact_block_multiple() {
    let rx = receiver().await;
    // 3 * 512 bytes: the end is marked by an empty fourth block.
    let data: Vec<u8> = (0..1536).map(|i| b'a' + (i % 26) as u8).collect();
    let up = rx.expect("big.cfg");
    tftp_put(rx.local_addr(), "big.cfg", &data, None)
        .await
        .unwrap();
    assert_eq!(up.await.unwrap().data, data);
}

#[tokio::test]
async fn negotiates_blksize() {
    let rx = receiver().await;
    let data = vec![b'x'; 5000];
    let up = rx.expect("b.cfg");
    tftp_put(rx.local_addr(), "b.cfg", &data, Some(1428))
        .await
        .unwrap();
    assert_eq!(up.await.unwrap().data, data);
}

#[tokio::test]
async fn refuses_unexpected_and_reused_names() {
    let rx = receiver().await;
    let err = tftp_put(rx.local_addr(), "nope.cfg", b"x", None)
        .await
        .unwrap_err();
    assert!(err.contains("unexpected file"), "{err}");

    let up = rx.expect("once.cfg");
    tftp_put(rx.local_addr(), "once.cfg", b"one", None)
        .await
        .unwrap();
    up.await.unwrap();
    let err = tftp_put(rx.local_addr(), "once.cfg", b"two", None)
        .await
        .unwrap_err();
    assert!(err.contains("unexpected file"), "{err}");
}

#[tokio::test]
async fn dropping_the_expectation_withdraws_it() {
    let rx = receiver().await;
    drop(rx.expect("gone.cfg"));
    assert!(tftp_put(rx.local_addr(), "gone.cfg", b"x", None)
        .await
        .is_err());
}

#[tokio::test]
async fn refuses_downloads() {
    let rx = receiver().await;
    let _up = rx.expect("r1.cfg");
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sock.send_to(b"\0\x01r1.cfg\0octet\0", rx.local_addr())
        .await
        .unwrap();
    let mut buf = [0u8; 512];
    let (n, _) = timeout(Duration::from_secs(5), sock.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf[..4], &[0, 5, 0, 2], "{:?}", &buf[..n]);
}
