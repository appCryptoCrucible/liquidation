//! Warm sockets on the submit client itself (`ExecPath.http`), measured with
//! an accept counter on a keep-alive mock: re-warms and a signed submit ride
//! one socket, and the re-warm loop keeps that socket open past the server's
//! idle close. This is the proof behind
//! `liq_obs::thirteen_a_http_pool_seam().handshake_free_critical`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use alloy_primitives::{address, b256, Address, Bytes, B256};
use liq_exec::builders::{leak_str, BuilderEndpoint, BuilderSet};
use liq_exec::fee::FeeQuote;
use liq_exec::nonce::{NonceAllocator, NonceMode};
use liq_exec::path::{AllowAll, CaptureRecorder, ExecPath};
use liq_exec::submit::{LiveSendBits, SubmitEnabled};
use liq_exec::template::PrecomputedSigner;
use liq_oracle::mevshare::SearcherKey;
use liq_types::{
    AssetId, BuilderId, FlashProvider, MarketId, PositionKey, ProtocolId, SubmitReceipt, TraceId,
    TriggerKind,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const SECRET: B256 = b256!("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
const OPERATOR: Address = address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
const DEPLOYED: Address = address!("0x1111111111111111111111111111111111111111");

/// HTTP/1.1 keep-alive mock. `accepted` counts TCP sockets, `requests`
/// counts HTTP requests served; `idle` closes a socket that sends nothing
/// for that long, the way a builder's load balancer does.
struct KeepAlive {
    url: &'static str,
    accepted: Arc<AtomicU64>,
    requests: Arc<AtomicU64>,
}

async fn spawn_keepalive(idle: Option<Duration>) -> KeepAlive {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicU64::new(0));
    let requests = Arc::new(AtomicU64::new(0));
    let accepted_t = Arc::clone(&accepted);
    let requests_t = Arc::clone(&requests);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            accepted_t.fetch_add(1, Ordering::Relaxed);
            let requests_t = Arc::clone(&requests_t);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16_384];
                let mut got: Vec<u8> = Vec::new();
                loop {
                    let n = match idle {
                        Some(d) => match tokio::time::timeout(d, sock.read(&mut buf)).await {
                            Ok(r) => r,
                            Err(_) => break, // idle close
                        },
                        None => sock.read(&mut buf).await,
                    };
                    match n {
                        Ok(0) | Err(_) => break,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                    }
                    while let Some(end) = complete_request(&got) {
                        got.drain(..end);
                        requests_t.fetch_add(1, Ordering::Relaxed);
                        let payload = br#"{"jsonrpc":"2.0","id":1,"result":{"bundleHash":"0x00"}}"#;
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                            payload.len()
                        );
                        let mut out = head.into_bytes();
                        out.extend_from_slice(payload);
                        if sock.write_all(&out).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    KeepAlive {
        url: leak_str(format!("http://{addr}/")),
        accepted,
        requests,
    }
}

/// Length of the first complete request in `got`, if one is there.
fn complete_request(got: &[u8]) -> Option<usize> {
    let pos = got.windows(4).position(|w| w == b"\r\n\r\n")?;
    let headers = std::str::from_utf8(&got[..pos]).ok()?;
    let len = headers
        .split("\r\n")
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().ok())
        })
        .flatten()
        .unwrap_or(0);
    let end = pos + 4 + len;
    (got.len() >= end).then_some(end)
}

fn path(set: BuilderSet) -> Arc<ExecPath<CaptureRecorder, AllowAll>> {
    let signer = Arc::new(PrecomputedSigner::from_secret(SECRET).unwrap());
    let nonces = NonceAllocator::from_addresses(vec![signer.address()]).unwrap();
    let identity = SearcherKey::from_secret(SECRET).unwrap();
    let live = LiveSendBits {
        held: Arc::new(AtomicBool::new(true)),
        nonce_resync: Arc::new(AtomicBool::new(true)),
    };
    Arc::new(
        ExecPath::new(
            CaptureRecorder::default(),
            AllowAll,
            Arc::new(SubmitEnabled::new(true)),
            NonceMode::Allocate,
            nonces,
            vec![signer],
            set,
            identity,
            live,
        )
        .unwrap()
        .with_executor(DEPLOYED),
    )
}

fn endpoint(id: u16, name: &'static str, url: &'static str, warm: bool) -> BuilderEndpoint {
    BuilderEndpoint {
        id: BuilderId(id),
        name,
        endpoint: url,
        warm,
    }
}

fn svr_job() -> liq_exec::path::ExecJob {
    liq_exec::path::ExecJob {
        trace: TraceId::from_raw(7),
        plan: Bytes::from_static(&[0xab, 0xcd]),
        trigger: TriggerKind::SvrAuction,
        protocol: ProtocolId(1),
        market: MarketId(1),
        collateral: AssetId(0),
        debt: AssetId(1),
        flash: FlashProvider::Aave,
        operator_key: OPERATOR,
        position: PositionKey {
            protocol: ProtocolId(1),
            market: MarketId(1),
            user: address!("0x1111111111111111111111111111111111111111"),
        },
        hint_hash: Some(b256!(
            "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        )),
        backrun_tx: None,
        target_block: 101,
        max_block: 103,
        fee: FeeQuote {
            parent_block: 100,
            next_base_fee: 1_000,
            priority_wei: 10,
            modest_priority_wei: 2,
        },
        auction_bps: 9_000,
        calldata: Bytes::from_static(&[0x11, 0x22]),
        gas_limit: 200_000,
        chain_id: 1,
        slot: 0,
        rpc_verify: false,
    }
}

/// Flagged builder and relay: three re-warms and then a signed MEV-Share
/// submit all ride one accepted socket each. The unflagged builder is never
/// touched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rewarm_and_submit_ride_one_socket_per_warm_target() {
    let warm = spawn_keepalive(None).await;
    let cold = spawn_keepalive(None).await;
    let relay = spawn_keepalive(None).await;
    let set = BuilderSet::from_parts(
        vec![
            endpoint(1, "warm", warm.url, true),
            endpoint(2, "cold", cold.url, false),
        ],
        relay.url,
    )
    .unwrap();
    let p = path(set);
    assert_eq!(p.builders.set.warm_targets(), [warm.url, relay.url]);

    for _ in 0..3 {
        assert!(p.rewarm().await.is_empty(), "every warm target answered");
    }
    assert_eq!(
        warm.accepted.load(Ordering::Relaxed),
        1,
        "one socket to the warm builder"
    );
    assert_eq!(warm.requests.load(Ordering::Relaxed), 3);
    assert_eq!(
        relay.accepted.load(Ordering::Relaxed),
        1,
        "one socket to the relay"
    );
    assert_eq!(relay.requests.load(Ordering::Relaxed), 3);
    assert_eq!(
        cold.accepted.load(Ordering::Relaxed),
        0,
        "unflagged builder untouched"
    );
    assert_eq!(cold.requests.load(Ordering::Relaxed), 0);

    // The bundle itself rides the warm relay socket: no new accept.
    let rec = p.submit_path(&svr_job()).await.unwrap();
    assert_eq!(rec, SubmitReceipt::Accepted);
    assert_eq!(
        relay.accepted.load(Ordering::Relaxed),
        1,
        "submit paid no handshake"
    );
    assert_eq!(relay.requests.load(Ordering::Relaxed), 4);
}

/// A server that closes idle sockets after 300 ms: without the loop the next
/// warm reconnects (the control, proving the mock closes), with the loop at
/// 100 ms the socket is never closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rewarm_loop_keeps_the_socket_open_past_the_server_idle_close() {
    let idle = Duration::from_millis(300);
    let srv = spawn_keepalive(Some(idle)).await;
    let set = BuilderSet::from_parts(vec![endpoint(1, "warm", srv.url, true)], srv.url).unwrap();
    let p = path(set);
    assert_eq!(
        p.builders.set.warm_targets(),
        [srv.url],
        "builder + relay is one socket"
    );

    // Control: idle past the server's limit forces a reconnect.
    assert!(p.rewarm().await.is_empty());
    assert_eq!(srv.accepted.load(Ordering::Relaxed), 1);
    tokio::time::sleep(idle * 3).await;
    assert!(
        p.rewarm().await.is_empty(),
        "a closed idle socket is replaced, not an error"
    );
    assert_eq!(
        srv.accepted.load(Ordering::Relaxed),
        2,
        "the mock closed the idle socket and the client opened a new one"
    );

    // With the loop running faster than the idle limit, no further accept.
    let loop_handle = p.spawn_rewarm_every(Duration::from_millis(100));
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let accepted = srv.accepted.load(Ordering::Relaxed);
    let requests = srv.requests.load(Ordering::Relaxed);
    loop_handle.abort();
    assert_eq!(accepted, 2, "the re-warm loop kept the socket open");
    assert!(requests >= 10, "loop ticked: {requests} requests");
}

/// Live, against the shipped `config/builders.toml`: the submit client
/// negotiates HTTP/2 with every warm target, and a warm request costs less
/// than the cold one that opened the connection. Prints both per target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live network: every builder and the MEV-Share relay"]
async fn live_warm_targets_speak_h2_and_warm_beats_cold() {
    let file =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/builders.toml");
    let set = liq_exec::builders::load_builders(&file).unwrap();
    let p = path(set);
    for url in p.builders.set.warm_targets() {
        let t0 = std::time::Instant::now();
        let version = p.warm_version(url).await.unwrap();
        let cold = t0.elapsed();
        let mut warm = Vec::new();
        for _ in 0..5 {
            let t = std::time::Instant::now();
            assert_eq!(p.warm_version(url).await.unwrap(), version);
            warm.push(t.elapsed());
        }
        warm.sort();
        let median = warm[2];
        println!("{url:40} {version:?} cold {cold:>10.1?} warm p50 {median:>10.1?}");
        assert_eq!(
            version,
            reqwest::Version::HTTP_2,
            "{url} did not negotiate h2"
        );
        assert!(
            median < cold,
            "{url}: warm {median:?} not below cold {cold:?}"
        );
    }
}
