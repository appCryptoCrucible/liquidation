//! 13A path: `submit_enabled`, RiskAllow deny, venue routing, JoinSet, ShadowRecorder.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use alloy_primitives::{address, b256, Address, Bytes, B256};
use common::spawn_mock;
use liq_exec::builders::{leak_str, BuilderEndpoint, BuilderSet};
use liq_exec::fee::FeeQuote;
use liq_exec::nonce::{NonceAllocator, NonceMode};
use liq_exec::path::{AllowAll, CaptureRecorder, DenyAll, ExecPath};
use liq_exec::submit::{LiveSendBits, SubmitEnabled};
use liq_exec::template::PrecomputedSigner;
use liq_obs::ShadowRecorder;
use liq_oracle::mevshare::SearcherKey;
use liq_types::{
    AssetId, BuilderId, FlashProvider, HaltReason, MarketId, PositionKey, ProtocolId,
    SubmitReceipt, Submitter, TraceId, TriggerKind,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SECRET: B256 = b256!("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
const OPERATOR: Address = address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

fn quote(parent: u64) -> FeeQuote {
    FeeQuote {
        parent_block: parent,
        next_base_fee: 1_000,
        priority_wei: 10,
        modest_priority_wei: 2,
    }
}

fn job(kind: TriggerKind, hint: Option<B256>, backrun: Option<Bytes>) -> liq_exec::path::ExecJob {
    let (target, max) = if kind == TriggerKind::SvrAuction {
        (101, 103)
    } else {
        (101, 101)
    };
    liq_exec::path::ExecJob {
        trace: TraceId::from_raw(7),
        plan: Bytes::from_static(&[0xab, 0xcd]),
        trigger: kind,
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
        hint_hash: hint,
        backrun_tx: backrun,
        target_block: target,
        max_block: max,
        fee: quote(100),
        auction_bps: 9_000,
        calldata: Bytes::from_static(&[0x11, 0x22]),
        gas_limit: 200_000,
        chain_id: 1,
        slot: 0,
        rpc_verify: false,
    }
}

fn live_bits(held: bool, nonce_resync: bool) -> LiveSendBits {
    LiveSendBits {
        held: Arc::new(AtomicBool::new(held)),
        nonce_resync: Arc::new(AtomicBool::new(nonce_resync)),
    }
}

fn path(
    enabled: bool,
    live: LiveSendBits,
    builders: BuilderSet,
    gate: impl liq_types::RiskAllow,
) -> ExecPath<CaptureRecorder, impl liq_types::RiskAllow> {
    let signer = Arc::new(PrecomputedSigner::from_secret(SECRET).unwrap());
    let nonces = NonceAllocator::from_addresses(vec![signer.address()]).unwrap();
    let identity = SearcherKey::from_secret(SECRET).unwrap();
    let flag = Arc::new(SubmitEnabled::new(enabled));
    ExecPath::new(
        CaptureRecorder::default(),
        gate,
        flag,
        NonceMode::Allocate,
        nonces,
        vec![signer],
        builders,
        identity,
        live,
    )
    .unwrap()
    .with_executor(DEPLOYED)
}

/// Stands in for a deployed Executor. The placeholder never sends.
const DEPLOYED: alloy_primitives::Address =
    alloy_primitives::address!("0x1111111111111111111111111111111111111111");

fn set_from_urls(builder: &'static str, relay: &'static str) -> BuilderSet {
    BuilderSet::from_parts(
        vec![
            BuilderEndpoint {
                id: BuilderId(1),
                name: "mock-a",
                endpoint: builder,
                warm: false,
            },
            BuilderEndpoint {
                id: BuilderId(2),
                name: "mock-b",
                endpoint: builder,
                warm: false,
            },
        ],
        relay,
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_enabled_false_zero_http() {
    let mock = spawn_mock(Duration::ZERO).await;
    let relay = leak_str(mock.url.clone());
    let builders = set_from_urls(relay, relay);
    let p = path(false, LiveSendBits::closed(), builders, AllowAll);
    let rec = p
        .submit_path(&job(
            TriggerKind::SvrAuction,
            Some(b256!(
                "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            )),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(rec, SubmitReceipt::Recorded);
    assert_eq!(mock.hits.load(Ordering::Relaxed), 0);
    assert_eq!(p.recorder.rows.lock().len(), 1);
    assert_eq!(
        p.nonces.next_of(0).unwrap(),
        1,
        "Allocate consumes when disabled"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_enabled_true_mock_receives_signed_body_and_header() {
    let mock = spawn_mock(Duration::ZERO).await;
    let relay = leak_str(mock.url.clone());
    let builders =
        set_from_urls(relay, relay).with_mevshare_builders(&["flashbots", "titan", "quasar"]);
    // Test-only force of held+resync. Production has no such force.
    let p = path(true, live_bits(true, true), builders, AllowAll);
    let rec = p
        .submit_path(&job(
            TriggerKind::SvrAuction,
            Some(b256!(
                "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            )),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(rec, SubmitReceipt::Accepted);
    assert!(mock.hits.load(std::sync::atomic::Ordering::Relaxed) >= 1);
    let cap = mock.captured.lock();
    let last = cap.last().expect("captured POST");
    assert!(
        last.header
            .as_ref()
            .is_some_and(|h| h.contains(':') && h.starts_with("0x")),
        "X-Flashbots-Signature missing: {:?}",
        last.header
    );
    let v: serde_json::Value = serde_json::from_slice(&last.body).unwrap();
    assert_eq!(v["method"], "mev_sendBundle");
    assert_eq!(v["params"][0]["body"][1]["canRevert"], false);
    // mev-share-node refuses a bundle whose version is not "v0.1"/"beta-1".
    assert_eq!(v["params"][0]["version"], "v0.1");
    // The configured builder names reach the wire as `privacy.builders`.
    assert_eq!(
        v["params"][0]["privacy"]["builders"],
        serde_json::json!(["flashbots", "titan", "quasar"])
    );
}

fn svr_job() -> liq_exec::path::ExecJob {
    job(
        TriggerKind::SvrAuction,
        Some(b256!(
            "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        )),
        None,
    )
}

/// Old path POSTed when only `submit_enabled` was true (unbound bits false).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enabled_held_resync_false_zero_http() {
    let mock = spawn_mock(Duration::ZERO).await;
    let relay = leak_str(mock.url.clone());
    let builders = set_from_urls(relay, relay);
    let p = path(true, live_bits(true, false), builders, AllowAll);
    let rec = p.submit_path(&svr_job()).await.unwrap();
    assert_eq!(rec, SubmitReceipt::Recorded);
    assert_eq!(mock.hits.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enabled_unheld_resync_true_zero_http() {
    let mock = spawn_mock(Duration::ZERO).await;
    let relay = leak_str(mock.url.clone());
    let builders = set_from_urls(relay, relay);
    let p = path(true, live_bits(false, true), builders, AllowAll);
    let rec = p.submit_path(&svr_job()).await.unwrap();
    assert_eq!(rec, SubmitReceipt::Recorded);
    assert_eq!(mock.hits.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_held_resync_true_zero_http() {
    let mock = spawn_mock(Duration::ZERO).await;
    let relay = leak_str(mock.url.clone());
    let builders = set_from_urls(relay, relay);
    let p = path(false, live_bits(true, true), builders, AllowAll);
    let rec = p.submit_path(&svr_job()).await.unwrap();
    assert_eq!(rec, SubmitReceipt::Recorded);
    assert_eq!(mock.hits.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unbound_exec_path_cannot_post() {
    let mock = spawn_mock(Duration::ZERO).await;
    let relay = leak_str(mock.url.clone());
    let builders = set_from_urls(relay, relay);
    let p = path(true, LiveSendBits::closed(), builders, AllowAll);
    let rec = p.submit_path(&svr_job()).await.unwrap();
    assert_eq!(rec, SubmitReceipt::Recorded);
    assert_eq!(mock.hits.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn risk_deny_does_not_send() {
    let mock = spawn_mock(Duration::ZERO).await;
    let relay = leak_str(mock.url.clone());
    let builders = set_from_urls(relay, relay);
    let p = path(
        true,
        live_bits(true, true),
        builders,
        DenyAll {
            reason: HaltReason::NodeLag,
        },
    );
    let rec = p
        .submit_path(&job(TriggerKind::InterestDrift, None, None))
        .await
        .unwrap();
    assert_eq!(rec, SubmitReceipt::Denied);
    assert_eq!(p.denied_count(), 1);
    assert_eq!(mock.hits.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(p.recorder.rows.lock().len(), 1);
    let infl = p.nonces.in_flight(0).unwrap();
    assert!(
        infl.get(&0).unwrap().dropped,
        "deny after allocate must mark_dropped (gap visible)"
    );
}

struct FailRecorder;

impl Submitter for FailRecorder {
    type Error = liq_exec::error::ExecError;

    fn submit(
        &self,
        _submission: &liq_types::IntendedSubmission,
    ) -> Result<SubmitReceipt, liq_exec::error::ExecError> {
        Err(liq_exec::error::ExecError::Record(
            "test record fail".into(),
        ))
    }
}

/// 17C residual: sign/record errors after allocate must mark_dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_fail_after_allocate_marks_dropped() {
    let mock = spawn_mock(Duration::ZERO).await;
    let relay = leak_str(mock.url.clone());
    let builders = set_from_urls(relay, relay);
    let signer = Arc::new(PrecomputedSigner::from_secret(SECRET).unwrap());
    let nonces = NonceAllocator::from_addresses(vec![signer.address()]).unwrap();
    let identity = SearcherKey::from_secret(SECRET).unwrap();
    let p = ExecPath::new(
        FailRecorder,
        AllowAll,
        Arc::new(SubmitEnabled::new(false)),
        NonceMode::Allocate,
        nonces,
        vec![signer],
        builders,
        identity,
        LiveSendBits::closed(),
    )
    .unwrap();
    let err = p
        .submit_path(&job(TriggerKind::InterestDrift, None, None))
        .await
        .expect_err("record fail");
    assert!(matches!(err, liq_exec::error::ExecError::Record(_)));
    let infl = p.nonces.in_flight(0).unwrap();
    assert!(
        infl.get(&0).unwrap().dropped,
        "record error after allocate must mark_dropped"
    );
    assert_eq!(mock.hits.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_fail_after_allocate_marks_dropped() {
    let dead = leak_str("http://127.0.0.1:1/".to_owned());
    let builders = set_from_urls(dead, dead);
    let p = path(true, live_bits(true, true), builders, AllowAll);
    let err = p
        .submit_path(&job(TriggerKind::InterestDrift, None, None))
        .await
        .expect_err("dead builder must fail send");
    assert!(matches!(
        err,
        liq_exec::error::ExecError::AllBuildersUnreachable | liq_exec::error::ExecError::Http(_)
    ));
    let infl = p.nonces.in_flight(0).unwrap();
    assert!(
        infl.get(&0).unwrap().dropped,
        "send-fail after allocate must mark_dropped (gap visible)"
    );
    assert_eq!(p.nonces.next_of(0).unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interest_drift_intended_bid_is_not_auction() {
    let mock = spawn_mock(Duration::ZERO).await;
    let ep = leak_str(mock.url.clone());
    let builders = set_from_urls(ep, ep);
    let p = path(false, LiveSendBits::closed(), builders, AllowAll);
    let rec = p
        .submit_path(&job(TriggerKind::InterestDrift, None, None))
        .await
        .unwrap();
    assert_eq!(rec, SubmitReceipt::Recorded);
    let row = p.recorder.rows.lock()[0].clone();
    assert_eq!(row.bid, alloy_primitives::U256::from(2u64));
    assert_ne!(row.bid, alloy_primitives::U256::from(9_000u64));
    match row.venue {
        liq_types::Venue::BuilderBundle { .. } => {}
        liq_types::Venue::MevShare { .. } => panic!("InterestDrift routed to MevShare"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joinset_fans_out_two_builders() {
    let a = spawn_mock(Duration::from_millis(200)).await;
    let b = spawn_mock(Duration::from_millis(200)).await;
    let ua = leak_str(a.url.clone());
    let ub = leak_str(b.url.clone());
    let set = BuilderSet::from_parts(
        vec![
            BuilderEndpoint {
                id: BuilderId(1),
                name: "a",
                endpoint: ua,
                warm: false,
            },
            BuilderEndpoint {
                id: BuilderId(2),
                name: "b",
                endpoint: ub,
                warm: false,
            },
        ],
        ua,
    )
    .unwrap();
    let p = path(true, live_bits(true, true), set, AllowAll);
    let t0 = Instant::now();
    let rec = p
        .submit_path(&job(TriggerKind::InterestDrift, None, None))
        .await
        .unwrap();
    let dt = t0.elapsed();
    assert_eq!(rec, SubmitReceipt::Accepted);
    assert!(
        dt < Duration::from_millis(350),
        "JoinSet must overlap; sequential would be ~400ms, got {dt:?}"
    );
    // The fan-out returns at the first accept; the other POST is in flight.
    let both = wait_hits(&[&a.hits, &b.hits], 1, Duration::from_secs(2)).await;
    assert!(both, "each builder received the bundle once");
    let body = &a.captured.lock()[0].body;
    let v: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert_eq!(v["method"], "eth_sendBundle");
    assert_eq!(v["params"][0]["txs"].as_array().unwrap().len(), 1);
}

/// Poll until every counter reaches `n` or `limit` passes.
async fn wait_hits(hits: &[&std::sync::atomic::AtomicU64], n: u64, limit: Duration) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < limit {
        if hits
            .iter()
            .all(|h| h.load(std::sync::atomic::Ordering::Relaxed) >= n)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// A far builder does not hold the submit: the fan-out returns at the near
/// builder's accept, and the far one still receives the same bundle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fanout_returns_at_first_accept_and_far_builder_still_gets_it() {
    let near = spawn_mock(Duration::ZERO).await;
    let far = spawn_mock(Duration::from_millis(800)).await;
    let un = leak_str(near.url.clone());
    let uf = leak_str(far.url.clone());
    let set = BuilderSet::from_parts(
        vec![
            BuilderEndpoint {
                id: BuilderId(1),
                name: "near",
                endpoint: un,
                warm: false,
            },
            BuilderEndpoint {
                id: BuilderId(2),
                name: "far",
                endpoint: uf,
                warm: false,
            },
        ],
        un,
    )
    .unwrap();
    let p = path(true, live_bits(true, true), set, AllowAll);
    let t0 = Instant::now();
    let rec = p
        .submit_path(&job(TriggerKind::InterestDrift, None, None))
        .await
        .unwrap();
    let dt = t0.elapsed();
    assert_eq!(rec, SubmitReceipt::Accepted);
    assert!(
        dt < Duration::from_millis(400),
        "waited on the far builder: {dt:?} (it answers at ~800 ms)"
    );
    assert_eq!(
        far.hits.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "far not read yet"
    );
    assert!(
        wait_hits(&[&far.hits], 1, Duration::from_secs(3)).await,
        "the far builder still received the bundle"
    );
    let a = near.captured.lock()[0].body.clone();
    let b = far.captured.lock()[0].body.clone();
    assert_eq!(a, b, "same signed bytes to both");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_nonce_does_not_consume() {
    let mock = spawn_mock(Duration::ZERO).await;
    let ep = leak_str(mock.url.clone());
    let signer = Arc::new(PrecomputedSigner::from_secret(SECRET).unwrap());
    let nonces = NonceAllocator::from_addresses(vec![signer.address()]).unwrap();
    let identity = SearcherKey::from_secret(SECRET).unwrap();
    let p = ExecPath::new(
        CaptureRecorder::default(),
        AllowAll,
        Arc::new(SubmitEnabled::new(false)),
        NonceMode::DryRun,
        nonces,
        vec![signer],
        set_from_urls(ep, ep),
        identity,
        LiveSendBits::closed(),
    )
    .unwrap();
    p.submit_path(&job(TriggerKind::InterestDrift, None, None))
        .await
        .unwrap();
    assert_eq!(p.nonces.next_of(0).unwrap(), 0);
}

#[test]
fn shadow_recorder_still_shadow() {
    let dir = std::env::temp_dir().join(format!(
        "liq-exec-shadow-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let rec = ShadowRecorder::open(&dir).unwrap();
    let sub = liq_types::IntendedSubmission {
        plan: Bytes::from_static(&[0x01]),
        bid: alloy_primitives::U256::from(1u64),
        venue: liq_types::Venue::BuilderBundle {
            endpoint: "https://rpc.beaverbuild.org/",
            builder: BuilderId(1),
        },
        deadline: 1,
        trace: TraceId::from_raw(1),
    };
    assert_eq!(rec.submit(&sub).unwrap(), SubmitReceipt::Shadow);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undeployed_executor_is_recorded_not_sent() {
    let mock = spawn_mock(Duration::ZERO).await;
    let relay = leak_str(mock.url.clone());
    let builders = set_from_urls(relay, relay);
    let p = path(true, live_bits(true, true), builders, AllowAll)
        .with_executor(liq_exec::builders::PLANNED_EXECUTOR);
    let rec = p
        .submit_path(&job(TriggerKind::InterestDrift, None, None))
        .await
        .unwrap();
    assert_eq!(rec, SubmitReceipt::Recorded);
    assert_eq!(mock.hits.load(std::sync::atomic::Ordering::Relaxed), 0);
}
