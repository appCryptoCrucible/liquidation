//! Mock builder/relay HTTP/1.1 server. Counts hits; captures body + header.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Debug)]
pub(crate) struct Captured {
    pub(crate) header: Option<String>,
    pub(crate) body: Vec<u8>,
}

pub(crate) struct MockRelay {
    pub(crate) url: String,
    pub(crate) hits: Arc<AtomicU64>,
    pub(crate) captured: Arc<Mutex<Vec<Captured>>>,
}

pub(crate) async fn spawn_mock(delay: Duration) -> MockRelay {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicU64::new(0));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let hits_t = Arc::clone(&hits);
    let cap_t = Arc::clone(&captured);
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                break;
            };
            let hits_t = Arc::clone(&hits_t);
            let cap_t = Arc::clone(&cap_t);
            tokio::spawn(async move {
                handle(sock, hits_t, cap_t, delay).await;
            });
        }
    });
    MockRelay {
        url: format!("http://{addr}/"),
        hits,
        captured,
    }
}

async fn handle(
    mut sock: tokio::net::TcpStream,
    hits: Arc<AtomicU64>,
    captured: Arc<Mutex<Vec<Captured>>>,
    delay: Duration,
) {
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let mut buf = vec![0u8; 16_384];
    let mut got = Vec::new();
    loop {
        match sock.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                got.extend_from_slice(&buf[..n]);
                if headers_and_body_complete(&got) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    hits.fetch_add(1, Ordering::Relaxed);
    let header = flashbots_header(&got);
    let body = body_of(&got);
    captured.lock().push(Captured { header, body });
    let payload = br#"{"jsonrpc":"2.0","id":1,"result":{"bundleHash":"0x00"}}"#;
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let mut out = resp.into_bytes();
    out.extend_from_slice(payload);
    let _ = sock.write_all(&out).await;
}

fn headers_and_body_complete(got: &[u8]) -> bool {
    let Some(pos) = got.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let headers = &got[..pos];
    let body = &got[pos + 4..];
    let Some(len) = content_length(headers) else {
        return true;
    };
    body.len() >= len
}

fn content_length(headers: &[u8]) -> Option<usize> {
    let s = std::str::from_utf8(headers).ok()?;
    for line in s.split("\r\n") {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            return v.trim().parse().ok();
        }
    }
    None
}

fn flashbots_header(got: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(got).ok()?;
    for line in s.split("\r\n") {
        if let Some(v) = line.strip_prefix("X-Flashbots-Signature:") {
            return Some(v.trim().to_string());
        }
        if let Some(v) = line.strip_prefix("x-flashbots-signature:") {
            return Some(v.trim().to_string());
        }
    }
    None
}

fn body_of(got: &[u8]) -> Vec<u8> {
    match got.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(pos) => got[pos + 4..].to_vec(),
        None => got.to_vec(),
    }
}
