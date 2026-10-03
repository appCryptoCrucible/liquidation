//! Governance bundles: simulation decides which transactions are signed,
//! nonces come from the chain and are consecutive, every hash is allowed to
//! revert, and the live-send conjunction still gates the POST.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;

use alloy_consensus::{Transaction, TxEnvelope};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{address, b256, keccak256, Address, Bytes, B256};
use common::{spawn_mock, spawn_rpc, Responder};
use liq_exec::builders::{leak_str, BuilderEndpoint, BuilderSet};
use liq_exec::chain::ChainClient;
use liq_exec::fee::FeeQuote;
use liq_exec::gov::{GovAction, GovJob, GovReceipt, GovTarget, GovTx};
use liq_exec::nonce::{NonceAllocator, NonceMode};
use liq_exec::path::{AllowAll, CaptureRecorder, DenyAll, ExecPath};
use liq_exec::submit::{LiveSendBits, SubmitEnabled};
use liq_exec::template::PrecomputedSigner;
use liq_oracle::mevshare::SearcherKey;
use liq_types::{
    AssetId, BuilderId, FlashProvider, HaltReason, MarketId, PositionKey, ProtocolId, TraceId,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const SECRET: B256 = b256!("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
const OPERATOR: Address = address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
/// Stands in for a deployed Executor (the placeholder never sends).
const EXECUTOR: Address = address!("0x1111111111111111111111111111111111111111");
const EXEC_GAS: u64 = 2_436_297;
const CHAIN_NONCE: u64 = 42;

fn tx(i: u8) -> GovTx {
    GovTx {
        trace: TraceId::from_raw(u64::from(i)),
        plan: Bytes::from(vec![0xa0, i]),
        calldata: Bytes::from(vec![0xc0, i]),
        protocol: ProtocolId(1),
        market: MarketId(1),
        collateral: AssetId(0),
        debt: AssetId(1),
        flash: FlashProvider::Aave,
        position: PositionKey {
            protocol: ProtocolId(1),
            market: MarketId(1),
            user: Address::repeat_byte(i),
        },
        auction_bps: 9_900,
    }
}

fn job(n: u8, base: u64) -> GovJob {
    GovJob {
        action: GovAction::Payload {
            controller: liq_exec::executor::mainnet::AAVE_PAYLOADS_CONTROLLER,
            id: 469,
        },
        base_block: base,
        target_block: 101,
        target_ts: 1_789_916_831,
        exec_gas: EXEC_GAS,
        fee: FeeQuote {
            parent_block: 100,
            next_base_fee: 1_000,
            priority_wei: 10,
            modest_priority_wei: 2,
        },
        chain_id: 1,
        txs: (0..n).map(tx).collect(),
    }
}

/// Node stand-in: `sims` are `(success, gasUsed)` per call, in order.
fn node(sims: Vec<(bool, u64)>) -> Responder {
    Arc::new(move |req: &Value| match req["method"].as_str().unwrap() {
        "eth_getTransactionCount" => json!(format!("{CHAIN_NONCE:#x}")),
        "eth_simulateV1" => {
            let calls: Vec<Value> = sims
                .iter()
                .map(|(ok, gas)| {
                    json!({
                        "status": if *ok { "0x1" } else { "0x0" },
                        "gasUsed": format!("{gas:#x}"),
                        "returnData": "0x",
                        "logs": [],
                    })
                })
                .collect();
            json!([{ "calls": calls }])
        }
        m => panic!("unexpected {m}"),
    })
}

fn path(
    open: bool,
    builder: &'static str,
    gate: impl liq_types::RiskAllow,
) -> ExecPath<CaptureRecorder, impl liq_types::RiskAllow> {
    let signer = Arc::new(PrecomputedSigner::from_secret(SECRET).unwrap());
    assert_eq!(signer.address(), OPERATOR);
    let nonces = NonceAllocator::from_addresses(vec![signer.address()]).unwrap();
    let builders = BuilderSet::from_parts(
        vec![BuilderEndpoint {
            id: BuilderId(1),
            name: "mock",
            endpoint: builder,
        }],
        builder,
    )
    .unwrap();
    ExecPath::new(
        CaptureRecorder::default(),
        gate,
        Arc::new(SubmitEnabled::new(open)),
        NonceMode::Allocate,
        nonces,
        vec![signer],
        builders,
        SearcherKey::from_secret(SECRET).unwrap(),
        LiveSendBits {
            held: Arc::new(AtomicBool::new(open)),
            nonce_resync: Arc::new(AtomicBool::new(open)),
        },
    )
    .unwrap()
    .with_executor(EXECUTOR)
}

fn target() -> GovTarget {
    GovTarget {
        executor: EXECUTOR,
        code: vec![(EXECUTOR, Bytes::from_static(&[0x60, 0x00]))],
    }
}

#[tokio::test(flavor = "current_thread")]
async fn only_survivors_are_signed_with_consecutive_chain_nonces() {
    let rpc = spawn_rpc(node(vec![
        (true, 2_900_000),
        (false, 60_000),
        (true, 480_000),
    ]))
    .await;
    let builder = spawn_mock(Duration::ZERO).await;
    let p = path(true, leak_str(builder.url.clone()), AllowAll);
    let chain = ChainClient::new(&rpc.url).unwrap();

    let r = p.submit_gov(&job(3, 100), &chain, &target()).await.unwrap();
    assert_eq!(r, GovReceipt::Accepted { txs: 2 });

    // What the node was asked: three calls from the operator to the
    // executor, on block 100, as block 101, with the executor's code.
    let reqs: Vec<Value> = rpc
        .captured
        .lock()
        .iter()
        .map(|c| serde_json::from_slice(&c.body).unwrap())
        .collect();
    let sim = reqs
        .iter()
        .find(|r| r["method"] == "eth_simulateV1")
        .unwrap();
    assert_eq!(sim["params"][1], "0x64");
    let block = &sim["params"][0]["blockStateCalls"][0];
    assert_eq!(block["blockOverrides"]["number"], "0x65");
    assert_eq!(block["calls"].as_array().unwrap().len(), 3);
    assert_eq!(
        block["calls"][0]["from"].as_str().unwrap().to_lowercase(),
        format!("{OPERATOR:#x}")
    );
    assert!(block["stateOverrides"][format!("{EXECUTOR:#x}")]["code"].is_string());
    assert_eq!(
        block["calls"][0]["gasPrice"],
        format!("{:#x}", 1_000u128 + 10),
        "the Executor charges governance gas at tx.gasprice"
    );
    let nonce_req = reqs
        .iter()
        .find(|r| r["method"] == "eth_getTransactionCount")
        .unwrap();
    assert_eq!(nonce_req["params"][1], "0x64", "nonce after the base block");

    // What the builder got.
    let sent = builder.captured.lock().clone();
    assert_eq!(sent.len(), 1);
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    let params = &body["params"][0];
    assert_eq!(params["blockNumber"], "0x65");
    let raws: Vec<Bytes> = params["txs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(raws.len(), 2);
    let reverting: Vec<B256> = params["revertingTxHashes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h.as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(
        reverting,
        raws.iter().map(keccak256).collect::<Vec<_>>(),
        "every transaction may revert"
    );
    let txs: Vec<TxEnvelope> = raws
        .iter()
        .map(|r| TxEnvelope::decode_2718(&mut r.as_ref()).unwrap())
        .collect();
    assert_eq!(txs[0].nonce(), CHAIN_NONCE);
    assert_eq!(
        txs[1].nonce(),
        CHAIN_NONCE + 1,
        "no gap for the reverted one"
    );
    assert_eq!(txs[0].to(), Some(EXECUTOR));
    assert_eq!(txs[0].input().as_ref(), &[0xc0, 0]);
    assert_eq!(
        txs[1].input().as_ref(),
        &[0xc0, 2],
        "tx 1 reverted in simulation"
    );
    assert_eq!(
        txs[0].gas_limit(),
        3_480_000,
        "ran the payload: no second reserve"
    );
    assert_eq!(txs[1].gas_limit(), 576_000 + EXEC_GAS);
    assert_eq!(p.recorder.rows.lock().len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn closed_gate_records_and_does_not_post() {
    let rpc = spawn_rpc(node(vec![(true, 2_900_000), (true, 480_000)])).await;
    let builder = spawn_mock(Duration::ZERO).await;
    let p = path(false, leak_str(builder.url.clone()), AllowAll);
    let chain = ChainClient::new(&rpc.url).unwrap();
    let r = p.submit_gov(&job(2, 100), &chain, &target()).await.unwrap();
    assert_eq!(r, GovReceipt::Recorded { txs: 2 });
    assert_eq!(builder.hits.load(Ordering::Relaxed), 0);
    assert_eq!(p.recorder.rows.lock().len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn all_reverting_or_denied_sends_nothing() {
    let builder = spawn_mock(Duration::ZERO).await;
    let rpc = spawn_rpc(node(vec![(false, 50_000), (false, 50_000)])).await;
    let p = path(true, leak_str(builder.url.clone()), AllowAll);
    let chain = ChainClient::new(&rpc.url).unwrap();
    let r = p.submit_gov(&job(2, 100), &chain, &target()).await.unwrap();
    assert_eq!(r, GovReceipt::NoneSurvived);

    let rpc = spawn_rpc(node(vec![(true, 2_900_000)])).await;
    let p = path(
        true,
        leak_str(builder.url.clone()),
        DenyAll {
            reason: HaltReason::NodeLag,
        },
    );
    let chain = ChainClient::new(&rpc.url).unwrap();
    let r = p.submit_gov(&job(1, 100), &chain, &target()).await.unwrap();
    assert_eq!(r, GovReceipt::Denied);
    assert_eq!(builder.hits.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn a_target_that_is_not_the_next_block_is_refused() {
    let rpc = spawn_rpc(node(vec![(true, 1)])).await;
    let builder = spawn_mock(Duration::ZERO).await;
    let p = path(true, leak_str(builder.url.clone()), AllowAll);
    let chain = ChainClient::new(&rpc.url).unwrap();
    assert!(p.submit_gov(&job(1, 99), &chain, &target()).await.is_err());
    assert_eq!(
        rpc.hits.load(Ordering::Relaxed),
        0,
        "refused before any read"
    );
}

/// Node whose confirmed nonce depends on the block asked about.
fn node_nonces(sims: Vec<(bool, u64)>, nonces: Vec<(u64, u64)>) -> Responder {
    let inner = node(sims);
    Arc::new(move |req: &Value| {
        if req["method"] == "eth_getTransactionCount" {
            let block = u64::from_str_radix(
                req["params"][1].as_str().unwrap().trim_start_matches("0x"),
                16,
            )
            .unwrap();
            let n = nonces
                .iter()
                .find(|(b, _)| *b == block)
                .map(|(_, n)| *n)
                .unwrap();
            return json!(format!("{n:#x}"));
        }
        inner(req)
    })
}

fn ordinary(target: u64) -> liq_exec::path::ExecJob {
    liq_exec::path::ExecJob {
        trace: TraceId::from_raw(99),
        plan: Bytes::from_static(&[0xab]),
        trigger: liq_types::TriggerKind::InterestDrift,
        protocol: ProtocolId(1),
        market: MarketId(1),
        collateral: AssetId(0),
        debt: AssetId(1),
        flash: FlashProvider::Aave,
        operator_key: OPERATOR,
        position: tx(9).position,
        hint_hash: None,
        backrun_tx: None,
        target_block: target,
        max_block: target,
        fee: FeeQuote {
            parent_block: target - 1,
            next_base_fee: 1_000,
            priority_wei: 10,
            modest_priority_wei: 2,
        },
        auction_bps: 0,
        calldata: Bytes::from_static(&[0xdd]),
        gas_limit: 300_000,
        chain_id: 1,
        slot: 0,
        rpc_verify: false,
    }
}

fn sent_nonces(builder: &common::MockRelay) -> Vec<u64> {
    builder
        .captured
        .lock()
        .iter()
        .flat_map(|c| {
            let body: Value = serde_json::from_slice(&c.body).unwrap();
            body["params"][0]["txs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| {
                    let raw: Bytes = t.as_str().unwrap().parse().unwrap();
                    TxEnvelope::decode_2718(&mut raw.as_ref()).unwrap().nonce()
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// One nonce sequence per block, shared by governance bundles and ordinary
/// jobs, restarted from the chain each block: nothing reuses a nonce within
/// a block, and a bundle that did not land leaves no gap in the next.
#[tokio::test(flavor = "current_thread")]
async fn ordinary_and_governance_share_the_resynced_nonces() {
    let rpc = spawn_rpc(node_nonces(
        vec![(true, 2_900_000), (true, 480_000)],
        vec![(100, 42), (101, 43)],
    ))
    .await;
    let builder = spawn_mock(Duration::ZERO).await;
    let p = path(true, leak_str(builder.url.clone()), AllowAll);
    let chain = ChainClient::new(&rpc.url).unwrap();

    p.submit_gov(&job(2, 100), &chain, &target()).await.unwrap();
    p.sync_nonce(&chain, 101).await.unwrap(); // same block: no reset
    p.submit_path(&ordinary(101)).await.unwrap();
    assert_eq!(sent_nonces(&builder), vec![42, 43, 44]);

    // Only one of those landed: block 102 starts from the chain's 43.
    p.sync_nonce(&chain, 102).await.unwrap();
    p.submit_path(&ordinary(102)).await.unwrap();
    assert_eq!(sent_nonces(&builder), vec![42, 43, 44, 43]);
}

/// Anvil account 1: the second operator in these tests.
const SECOND_SECRET: B256 =
    b256!("0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d");
const SECOND: Address = address!("0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

/// Two operator keys, two nonce slots: each resyncs to its own key's
/// confirmed nonce, and allocating on one does not move the other.
#[tokio::test(flavor = "current_thread")]
async fn each_operator_slot_resyncs_to_its_own_chain_nonce() {
    let answer: Responder = Arc::new(|req: &Value| {
        if req["method"] != "eth_getTransactionCount" {
            return Value::Null;
        }
        let who: Address = req["params"][0].as_str().unwrap().parse().unwrap();
        let n: u64 = if who == OPERATOR {
            42
        } else if who == SECOND {
            7
        } else {
            0
        };
        json!(format!("{n:#x}"))
    });
    let rpc = spawn_rpc(answer).await;
    let builder = spawn_mock(Duration::ZERO).await;
    let first = Arc::new(PrecomputedSigner::from_secret(SECRET).unwrap());
    let second = Arc::new(PrecomputedSigner::from_secret(SECOND_SECRET).unwrap());
    assert_eq!(second.address(), SECOND);
    let nonces = NonceAllocator::from_addresses(vec![first.address(), second.address()]).unwrap();
    let url = leak_str(builder.url.clone());
    let builders = BuilderSet::from_parts(
        vec![BuilderEndpoint {
            id: BuilderId(1),
            name: "mock",
            endpoint: url,
        }],
        url,
    )
    .unwrap();
    let p = ExecPath::new(
        CaptureRecorder::default(),
        AllowAll,
        Arc::new(SubmitEnabled::new(false)),
        NonceMode::Allocate,
        nonces,
        vec![first, second],
        builders,
        SearcherKey::from_secret(SECRET).unwrap(),
        LiveSendBits {
            held: Arc::new(AtomicBool::new(false)),
            nonce_resync: Arc::new(AtomicBool::new(false)),
        },
    )
    .unwrap();
    p.sync_nonce(&ChainClient::new(&rpc.url).unwrap(), 101)
        .await
        .unwrap();
    assert!(p.nonce_resync.load(Ordering::Acquire));
    assert_eq!(p.nonces.next_of(0).unwrap(), 42);
    assert_eq!(p.nonces.next_of(1).unwrap(), 7);
    assert_eq!(p.nonces.allocate(1).unwrap().nonce, 7);
    assert_eq!(
        p.nonces.next_of(0).unwrap(),
        42,
        "the first key's sequence is untouched"
    );
}

/// The live-send resync bit follows the chain resync: on after a successful
/// read, off when the node cannot answer, and POSTs follow it.
#[tokio::test(flavor = "current_thread")]
async fn nonce_resync_bit_follows_the_chain_read() {
    let good = spawn_rpc(node_nonces(vec![], vec![(100, 42)])).await;
    let builder = spawn_mock(Duration::ZERO).await;
    let p = path(true, leak_str(builder.url.clone()), AllowAll);
    p.nonce_resync.store(false, Ordering::Release);

    p.sync_nonce(&ChainClient::new(&good.url).unwrap(), 101)
        .await
        .unwrap();
    assert!(
        p.nonce_resync.load(Ordering::Acquire),
        "on after a chain read"
    );

    let down: Responder = Arc::new(|_req: &Value| Value::Null);
    let bad = spawn_rpc(down).await;
    assert!(p
        .sync_nonce(&ChainClient::new(&bad.url).unwrap(), 102)
        .await
        .is_err());
    assert!(
        !p.nonce_resync.load(Ordering::Acquire),
        "off when the read fails"
    );
    let r = p.submit_path(&ordinary(102)).await.unwrap();
    assert_eq!(
        r,
        liq_types::SubmitReceipt::Recorded,
        "no POST without a resync"
    );
    assert_eq!(builder.hits.load(Ordering::Relaxed), 0);
}
