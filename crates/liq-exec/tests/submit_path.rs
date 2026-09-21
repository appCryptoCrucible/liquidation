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
use liq_exec::submit::SubmitEnabled;
use liq_exec::template::PrecomputedSigner;
use liq_obs::ShadowRecorder;
use liq_oracle::mevshare::SearcherKey;
use liq_types::{
    AssetId, BuilderId, FlashProvider, HaltReason, MarketId, PositionKey, ProtocolId,
    SubmitReceipt, Submitter, TraceId, TriggerKind,
};
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
        refund_address: OPERATOR,
        calldata: Bytes::from_static(&[0x11, 0x22]),
        gas_limit: 200_000,
        chain_id: 1,
        slot: 0,
    }
}

fn path(
    enabled: bool,
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
    )
    .unwrap()
}

fn set_from_urls(builder: &'static str, relay: &'static str) -> BuilderSet {
    BuilderSet::from_parts(
        vec![
            BuilderEndpoint {
                id: BuilderId(1),
                name: "mock-a",
                endpoint: builder,
            },
            BuilderEndpoint {
                id: BuilderId(2),
                name: "mock-b",
                endpoint: builder,
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
    let p = path(false, builders, AllowAll);
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
    assert_eq!(mock.hits.load(std::sync::atomic::Ordering::Relaxed), 0);
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
    let builders = set_from_urls(relay, relay);
    let p = path(true, builders, AllowAll);
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn risk_deny_does_not_send() {
    let mock = spawn_mock(Duration::ZERO).await;
    let relay = leak_str(mock.url.clone());
    let builders = set_from_urls(relay, relay);
    let p = path(
        true,
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interest_drift_intended_bid_is_not_auction() {
    let mock = spawn_mock(Duration::ZERO).await;
    let ep = leak_str(mock.url.clone());
    let builders = set_from_urls(ep, ep);
    let p = path(false, builders, AllowAll);
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
            },
            BuilderEndpoint {
                id: BuilderId(2),
                name: "b",
                endpoint: ub,
            },
        ],
        ua,
    )
    .unwrap();
    let p = path(true, set, AllowAll);
    let t0 = Instant::now();
    let rec = p
        .submit_path(&job(TriggerKind::InterestDrift, None, None))
        .await
        .unwrap();
    let dt = t0.elapsed();
    assert_eq!(rec, SubmitReceipt::Accepted);
    assert_eq!(a.hits.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(b.hits.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(
        dt < Duration::from_millis(350),
        "JoinSet must overlap; sequential would be ~400ms, got {dt:?}"
    );
    let body = &a.captured.lock()[0].body;
    let v: serde_json::Value = serde_json::from_slice(body).unwrap();
    assert_eq!(v["method"], "eth_sendBundle");
    assert_eq!(v["params"][0]["txs"].as_array().unwrap().len(), 1);
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
