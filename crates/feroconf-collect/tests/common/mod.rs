#![allow(dead_code)]

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::time::timeout;

/// Minimal TFTP client that uploads `data` as `name` (what a router does during a copy).
pub async fn tftp_put(
    server: SocketAddr,
    name: &str,
    data: &[u8],
    blksize: Option<usize>,
) -> Result<(), String> {
    let sock = UdpSocket::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let mut wrq = vec![0, 2];
    wrq.extend_from_slice(name.as_bytes());
    wrq.push(0);
    wrq.extend_from_slice(b"octet\0");
    if let Some(b) = blksize {
        wrq.extend_from_slice(format!("blksize\0{b}\0").as_bytes());
    }
    sock.send_to(&wrq, server)
        .await
        .map_err(|e| e.to_string())?;

    let mut buf = [0u8; 1024];
    let (n, tid) = recv(&sock, &mut buf).await?;
    let pkt = &buf[..n];
    let block_size = match (pkt[1], blksize) {
        (4, _) if pkt[2..4] == [0, 0] => 512,
        (6, Some(b)) => {
            let text = String::from_utf8_lossy(&pkt[2..]).to_string();
            assert!(text.contains(&format!("blksize\0{b}")), "OACK {text:?}");
            b
        }
        (5, _) => return Err(format!("error {}", String::from_utf8_lossy(&pkt[4..n - 1]))),
        _ => return Err(format!("unexpected reply {pkt:?}")),
    };

    let mut block: u16 = 1;
    let mut offset = 0;
    loop {
        let end = (offset + block_size).min(data.len());
        let mut pkt = vec![0, 3];
        pkt.extend_from_slice(&block.to_be_bytes());
        pkt.extend_from_slice(&data[offset..end]);
        sock.send_to(&pkt, tid).await.map_err(|e| e.to_string())?;
        let (n, _) = recv(&sock, &mut buf).await?;
        if buf[1] != 4 || u16::from_be_bytes([buf[2], buf[3]]) != block {
            return Err(format!("bad ack {:?}", &buf[..n]));
        }
        let last = end - offset < block_size;
        offset = end;
        if last {
            return Ok(());
        }
        block = block.wrapping_add(1);
    }
}

async fn recv(sock: &UdpSocket, buf: &mut [u8]) -> Result<(usize, SocketAddr), String> {
    timeout(Duration::from_secs(5), sock.recv_from(buf))
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|e| e.to_string())
}
