//! Bind 13A [`ExecPath`]: [`RiskGate`] as [`RiskAllow`], [`ShadowRecorder`]
//! as the recorder. `submit_enabled` stays default false. Live HTTP also
//! requires the lease (resync ABSENT).

use std::sync::Arc;

use liq_exec::builders::BuilderSet;
use liq_exec::nonce::{NonceAllocator, NonceMode};
use liq_exec::path::ExecPath;
use liq_exec::submit::SubmitEnabled;
use liq_exec::template::PrecomputedSigner;
use liq_obs::ShadowRecorder;
use liq_oracle::mevshare::SearcherKey;
use liq_risk::RiskGate;

use crate::lease::SubmitLease;

/// Production bind. Signer↔nonce check lives in [`ExecPath::new`].
pub fn bind(
    recorder: ShadowRecorder,
    gate: &'static RiskGate,
    submit_enabled: Arc<SubmitEnabled>,
    nonces: NonceAllocator,
    signers: Vec<Arc<PrecomputedSigner>>,
    builders: BuilderSet,
    identity: SearcherKey,
) -> liq_exec::error::Result<ExecPath<ShadowRecorder, &'static RiskGate>> {
    ExecPath::new(
        recorder,
        gate,
        submit_enabled,
        NonceMode::Allocate,
        nonces,
        signers,
        builders,
        identity,
    )
}

/// Live POST gate at the wiring layer. Resync ABSENT → always false in prod.
#[must_use]
pub fn live_http_allowed(lease: &SubmitLease, flag: &SubmitEnabled) -> bool {
    lease.live_send_permitted(flag.get())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256, Address, Bytes, B256};
    use liq_exec::builders::{leak_str, BuilderEndpoint};
    use liq_exec::fee::FeeQuote;
    use liq_exec::path::{AllowAll, CaptureRecorder, ExecJob};
    use liq_types::{
        AssetId, BuilderId, FlashProvider, MarketId, PositionKey, ProtocolId, SubmitReceipt,
        TraceId, TriggerKind,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const SECRET: B256 =
        b256!("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
    const OPERATOR: Address = address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    fn job() -> ExecJob {
        ExecJob {
            trace: TraceId::from_raw(7),
            plan: Bytes::from_static(&[0xab]),
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
            refund_address: OPERATOR,
            calldata: Bytes::from_static(&[0x11]),
            gas_limit: 200_000,
            chain_id: 1,
            slot: 0,
        }
    }

    async fn spawn_mock() -> (String, Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicU64::new(0));
        let hits_t = Arc::clone(&hits);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let hits_t = Arc::clone(&hits_t);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16_384];
                    let mut got = Vec::new();
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                got.extend_from_slice(buf.get(..n).unwrap_or(&[]));
                                if got.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    hits_t.fetch_add(1, Ordering::Relaxed);
                    let payload = br#"{"jsonrpc":"2.0","id":1,"result":{"bundleHash":"0x00"}}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let mut out = resp.into_bytes();
                    out.extend_from_slice(payload);
                    let _ = sock.write_all(&out).await;
                });
            }
        });
        (format!("http://{addr}/"), hits)
    }

    /// Toggle false→true→false without restarting; 13A POSTs only while true.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hot_reload_toggle_posts_only_while_true() {
        let (url, hits) = spawn_mock().await;
        let relay = leak_str(url);
        let builders = BuilderSet::from_parts(
            vec![BuilderEndpoint {
                id: BuilderId(1),
                name: "mock",
                endpoint: relay,
            }],
            relay,
        )
        .unwrap();
        let signer = Arc::new(PrecomputedSigner::from_secret(SECRET).unwrap());
        let nonces = NonceAllocator::from_addresses(vec![signer.address()]).unwrap();
        let identity = SearcherKey::from_secret(SECRET).unwrap();
        let flag = Arc::new(SubmitEnabled::new(false));
        let path = ExecPath::new(
            CaptureRecorder::default(),
            AllowAll,
            Arc::clone(&flag),
            NonceMode::Allocate,
            nonces,
            vec![signer],
            builders,
            identity,
        )
        .unwrap();
        let lease = SubmitLease::granted_shadow();
        assert!(!live_http_allowed(&lease, &flag));

        let rec = path.submit_path(&job()).await.unwrap();
        assert_eq!(rec, SubmitReceipt::Recorded);
        assert_eq!(hits.load(Ordering::Relaxed), 0);

        flag.set(true);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let rec = path.submit_path(&job()).await.unwrap();
        assert_eq!(rec, SubmitReceipt::Accepted);
        assert!(hits.load(Ordering::Relaxed) >= 1);
        // lease still blocks production live send (resync ABSENT)
        assert!(!live_http_allowed(&lease, &flag));

        flag.set(false);
        let before = hits.load(Ordering::Relaxed);
        let rec = path.submit_path(&job()).await.unwrap();
        assert_eq!(rec, SubmitReceipt::Recorded);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(hits.load(Ordering::Relaxed), before);
    }
}
