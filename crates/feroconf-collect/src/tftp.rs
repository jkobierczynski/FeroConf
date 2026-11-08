//! A small TFTP server that only accepts uploads (WRQ) it was told to expect.
//!
//! Routers push their config to it during an SNMP-triggered copy. Every backup registers a
//! random, single-use file name with [`TftpReceiver::expect`]; uploads under any other name, and
//! all downloads (RRQ), are refused. Received data stays in memory and is never written to disk.
//!
//! Implements RFC 1350 with the `blksize` (RFC 2348), `timeout` and `tsize` (RFC 2349) options.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const OP_RRQ: u16 = 1;
const OP_WRQ: u16 = 2;
const OP_DATA: u16 = 3;
const OP_ACK: u16 = 4;
const OP_ERROR: u16 = 5;
const OP_OACK: u16 = 6;

const ERR_NOT_DEFINED: u16 = 0;
const ERR_ACCESS_VIOLATION: u16 = 2;
const ERR_DISK_FULL: u16 = 3;
const ERR_ILLEGAL_OP: u16 = 4;

const DEFAULT_BLKSIZE: usize = 512;
const MAX_BLKSIZE: usize = 8192;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RETRIES: u32 = 5;
/// Larger uploads are refused. Real configs are far smaller.
pub const MAX_FILE_SIZE: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum TftpError {
    #[error("the device reported a TFTP error {code}: {message}")]
    Remote { code: u16, message: String },
    #[error("TFTP transfer timed out after block {0}")]
    Timeout(u16),
    #[error("upload is larger than {MAX_FILE_SIZE} bytes")]
    TooLarge,
    #[error("TFTP protocol error: {0}")]
    Protocol(String),
    #[error("TFTP I/O error: {0}")]
    Io(String),
    #[error("the TFTP server stopped")]
    Stopped,
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Upload, TftpError>>>>>;

/// A finished upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    pub data: Vec<u8>,
    pub peer: SocketAddr,
}

pub struct TftpReceiver {
    local: SocketAddr,
    pending: Pending,
    task: JoinHandle<()>,
}

impl TftpReceiver {
    /// Bind the server and start answering requests in the background.
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        let local = socket.local_addr()?;
        let pending: Pending = Arc::default();
        let task = tokio::spawn(serve(socket, pending.clone()));
        Ok(Self {
            local,
            pending,
            task,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Accept exactly one upload under `filename`. The returned future resolves when it has been
    /// received completely. Dropping it withdraws the expectation.
    pub fn expect(&self, filename: impl Into<String>) -> ExpectedUpload {
        let filename = filename.into();
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending lock")
            .insert(filename.clone(), tx);
        ExpectedUpload {
            filename,
            rx,
            pending: self.pending.clone(),
        }
    }
}

impl Drop for TftpReceiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Resolves to the upload registered with [`TftpReceiver::expect`].
pub struct ExpectedUpload {
    filename: String,
    rx: oneshot::Receiver<Result<Upload, TftpError>>,
    pending: Pending,
}

impl ExpectedUpload {
    pub fn filename(&self) -> &str {
        &self.filename
    }
}

impl Future for ExpectedUpload {
    type Output = Result<Upload, TftpError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.rx)
            .poll(cx)
            .map(|r| r.unwrap_or(Err(TftpError::Stopped)))
    }
}

impl Drop for ExpectedUpload {
    fn drop(&mut self) {
        if let Ok(mut p) = self.pending.lock() {
            p.remove(&self.filename);
        }
    }
}

async fn serve(socket: UdpSocket, pending: Pending) {
    let mut buf = vec![0u8; 2048];
    loop {
        let (n, peer) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            // On some platforms an ICMP port unreachable from an earlier send shows up here.
            Err(_) => continue,
        };
        let pkt = &buf[..n];
        match opcode(pkt) {
            Some(OP_WRQ) => {
                let req = match parse_request(&pkt[2..]) {
                    Ok(r) => r,
                    Err(msg) => {
                        let _ = socket
                            .send_to(&error_packet(ERR_ILLEGAL_OP, &msg), peer)
                            .await;
                        continue;
                    }
                };
                let waiter = pending.lock().expect("pending lock").remove(&req.filename);
                match waiter {
                    Some(tx) => {
                        let local_ip = socket.local_addr().map(|a| a.ip()).ok();
                        tokio::spawn(async move {
                            let result = receive(local_ip, peer, req).await;
                            let _ = tx.send(result);
                        });
                    }
                    None => {
                        let _ = socket
                            .send_to(&error_packet(ERR_ACCESS_VIOLATION, "unexpected file"), peer)
                            .await;
                    }
                }
            }
            Some(OP_RRQ) => {
                let _ = socket
                    .send_to(
                        &error_packet(ERR_ACCESS_VIOLATION, "downloads are not allowed"),
                        peer,
                    )
                    .await;
            }
            // Stray DATA/ACK/ERROR on the main port: ignore.
            _ => {}
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Request {
    filename: String,
    mode: String,
    blksize: Option<usize>,
    timeout: Option<u64>,
    tsize: Option<u64>,
}

fn opcode(pkt: &[u8]) -> Option<u16> {
    (pkt.len() >= 2).then(|| u16::from_be_bytes([pkt[0], pkt[1]]))
}

fn parse_request(body: &[u8]) -> Result<Request, String> {
    let mut fields = body.split(|b| *b == 0);
    let mut next = || -> Option<String> {
        fields
            .next()
            .map(|f| String::from_utf8_lossy(f).into_owned())
    };
    let filename = next()
        .filter(|f| !f.is_empty())
        .ok_or("missing file name")?;
    let mode = next().ok_or("missing mode")?.to_ascii_lowercase();
    if mode != "octet" && mode != "netascii" {
        return Err(format!("unsupported mode {mode:?}"));
    }
    let mut req = Request {
        filename,
        mode,
        blksize: None,
        timeout: None,
        tsize: None,
    };
    while let (Some(name), Some(value)) = (next(), next()) {
        if name.is_empty() {
            break;
        }
        match name.to_ascii_lowercase().as_str() {
            "blksize" => {
                if let Ok(v) = value.parse::<usize>() {
                    if v >= 8 {
                        req.blksize = Some(v.min(MAX_BLKSIZE));
                    }
                }
            }
            "timeout" => {
                if let Ok(v) = value.parse::<u64>() {
                    if (1..=255).contains(&v) {
                        req.timeout = Some(v);
                    }
                }
            }
            "tsize" => req.tsize = value.parse().ok(),
            _ => {} // Unknown options are ignored, as RFC 2347 asks.
        }
    }
    Ok(req)
}

fn error_packet(code: u16, msg: &str) -> Vec<u8> {
    let mut p = Vec::with_capacity(5 + msg.len());
    p.extend_from_slice(&OP_ERROR.to_be_bytes());
    p.extend_from_slice(&code.to_be_bytes());
    p.extend_from_slice(msg.as_bytes());
    p.push(0);
    p
}

fn ack_packet(block: u16) -> Vec<u8> {
    let mut p = OP_ACK.to_be_bytes().to_vec();
    p.extend_from_slice(&block.to_be_bytes());
    p
}

fn oack_packet(req: &Request) -> Option<Vec<u8>> {
    let mut opts = Vec::new();
    if let Some(b) = req.blksize {
        opts.push(("blksize", b.to_string()));
    }
    if let Some(t) = req.timeout {
        opts.push(("timeout", t.to_string()));
    }
    if let Some(t) = req.tsize {
        opts.push(("tsize", t.to_string()));
    }
    if opts.is_empty() {
        return None;
    }
    let mut p = OP_OACK.to_be_bytes().to_vec();
    for (k, v) in opts {
        p.extend_from_slice(k.as_bytes());
        p.push(0);
        p.extend_from_slice(v.as_bytes());
        p.push(0);
    }
    Some(p)
}

/// Receive one file on a fresh socket (the transfer ID), as RFC 1350 requires.
async fn receive(
    local_ip: Option<std::net::IpAddr>,
    peer: SocketAddr,
    req: Request,
) -> Result<Upload, TftpError> {
    let io = |e: io::Error| TftpError::Io(e.to_string());
    let bind_ip = local_ip.unwrap_or_else(|| match peer {
        SocketAddr::V4(_) => [0, 0, 0, 0].into(),
        SocketAddr::V6(_) => std::net::Ipv6Addr::UNSPECIFIED.into(),
    });
    let sock = UdpSocket::bind(SocketAddr::new(bind_ip, 0))
        .await
        .map_err(io)?;
    sock.connect(peer).await.map_err(io)?;

    if req.tsize.is_some_and(|t| t > MAX_FILE_SIZE as u64) {
        let _ = sock
            .send(&error_packet(ERR_DISK_FULL, "file too large"))
            .await;
        return Err(TftpError::TooLarge);
    }

    let blksize = req.blksize.unwrap_or(DEFAULT_BLKSIZE);
    let wait = req
        .timeout
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TIMEOUT);
    let mut last_sent = oack_packet(&req).unwrap_or_else(|| ack_packet(0));
    sock.send(&last_sent).await.map_err(io)?;

    let mut data = Vec::new();
    let mut expected: u16 = 1;
    let mut retries = 0;
    let mut buf = vec![0u8; blksize + 4];
    loop {
        let n = match timeout(wait, sock.recv(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(io(e)),
            Err(_) => {
                retries += 1;
                if retries > MAX_RETRIES {
                    return Err(TftpError::Timeout(expected.wrapping_sub(1)));
                }
                sock.send(&last_sent).await.map_err(io)?;
                continue;
            }
        };
        let pkt = &buf[..n];
        match opcode(pkt) {
            Some(OP_DATA) if n >= 4 => {
                let block = u16::from_be_bytes([pkt[2], pkt[3]]);
                let payload = &pkt[4..];
                if block == expected {
                    if data.len() + payload.len() > MAX_FILE_SIZE {
                        let _ = sock
                            .send(&error_packet(ERR_DISK_FULL, "file too large"))
                            .await;
                        return Err(TftpError::TooLarge);
                    }
                    data.extend_from_slice(payload);
                    last_sent = ack_packet(block);
                    sock.send(&last_sent).await.map_err(io)?;
                    retries = 0;
                    if payload.len() < blksize {
                        return Ok(Upload { data, peer });
                    }
                    expected = expected.wrapping_add(1);
                } else if block == expected.wrapping_sub(1) {
                    // Our ACK got lost: send it again.
                    sock.send(&last_sent).await.map_err(io)?;
                }
            }
            Some(OP_ERROR) if n >= 4 => {
                let code = u16::from_be_bytes([pkt[2], pkt[3]]);
                let message = String::from_utf8_lossy(&pkt[4..])
                    .trim_end_matches('\0')
                    .to_string();
                return Err(TftpError::Remote { code, message });
            }
            _ => {
                let _ = sock
                    .send(&error_packet(ERR_NOT_DEFINED, "unexpected packet"))
                    .await;
                return Err(TftpError::Protocol(format!(
                    "unexpected packet with opcode {:?}",
                    opcode(pkt)
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wrq_with_options() {
        let body = b"cfg.txt\0octet\0blksize\x001428\0tsize\x00900\0foo\0bar\0";
        let r = parse_request(body).unwrap();
        assert_eq!(r.filename, "cfg.txt");
        assert_eq!(r.mode, "octet");
        assert_eq!(r.blksize, Some(1428));
        assert_eq!(r.tsize, Some(900));
        assert_eq!(r.timeout, None);
    }

    #[test]
    fn caps_blksize_and_rejects_bad_mode() {
        let r = parse_request(b"f\0NETASCII\0blksize\x0065000\0").unwrap();
        assert_eq!(r.mode, "netascii");
        assert_eq!(r.blksize, Some(MAX_BLKSIZE));
        assert!(parse_request(b"f\0mail\0").is_err());
        assert!(parse_request(b"\0octet\0").is_err());
    }
}
