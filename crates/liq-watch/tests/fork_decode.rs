//! Real RPC fixtures. Fail closed if the node cannot be reached.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::path::PathBuf;
use std::time::Duration;

use alloy_provider::{Provider, ProviderBuilder};
use alloy_sol_types::SolEvent;
use liq_config::{Intern, OnChainId, Registry};
use liq_watch::batch::write_parquet;
use liq_watch::decode::WatchDecoder;
use liq_watch::join::NoEngineJoin;
use liq_watch::source::RpcPoll;
use liq_watch::stream::StreamSink;
use liq_watch::{LogSource, OwnedBlock, Poll};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn rpc() -> String {
    std::env::var("LIQ_RPC_URL")
        .or_else(|_| std::env::var("MAINNET_RPC_URL"))
        .unwrap_or_else(|_| "https://eth.drpc.org".into())
}

/// Live `eth_getLogs` at the tip. Ignored in default CI: public RPCs rate-limit
/// and liquidations are sparse in a 400-block window. Run:
/// `cargo test -p liq-watch --test fork_decode -- --ignored --nocapture`
#[ignore]
#[tokio::test(flavor = "current_thread")]
async fn fork_decodes_real_liquidations_and_consumers() {
    let reg = Registry::from_path(&root().join("registry/registry.json")).unwrap();
    let intern = Intern::from_registry(&reg).unwrap();
    let dec = WatchDecoder::from_registry(&reg, intern).unwrap();
    let url = rpc().parse().expect("rpc url");
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(url);
    let core_pool = reg
        .protocols
        .iter()
        .find(|(k, e)| e.family == "aave-v3" && k.contains("87870bca"))
        .and_then(|(_, e)| match e.market {
            OnChainId::Addr(a) => Some(a),
            OnChainId::Slot(_) => None,
        })
        .expect("registry aave-v3 core pool");
    let t0_v3 = liq_watch::abi::aave_v3::LiquidationCall::SIGNATURE_HASH;
    let v3: Vec<_> = dec
        .subscriptions()
        .into_iter()
        .filter(|f| f.topic0 == t0_v3 && f.address == core_pool)
        .collect();
    assert_eq!(v3.len(), 1, "core pool must be in decoder filters");

    let head = provider.get_block_number().await.expect("eth_blockNumber");
    let found = tokio::time::timeout(Duration::from_secs(180), async {
        let mut start = head.saturating_sub(9);
        let mut steps = 0u32;
        while steps < 40 {
            let end = start.saturating_add(9).min(head);
            if end < start {
                break;
            }
            for filters in [&v3] {
                let mut poll = RpcPoll::new(provider.clone(), filters, start, Some(end), 10);
                match poll.fetch_page().await {
                    Ok(_) => {
                        let mut buf = OwnedBlock::default();
                        while matches!(
                            LogSource::poll_block(&mut poll, &mut buf).unwrap(),
                            Poll::Ready
                        ) {
                            let vol = dec.coverage_from_oracle_logs(&buf.logs, u32::MAX).1;
                            for log in &buf.logs {
                                let (trig, _) =
                                    dec.coverage_from_oracle_logs(&buf.logs, log.tx_index);
                                match dec.decode_log(log, trig, vol) {
                                    Ok(Some(ev)) => return (ev, log.clone()),
                                    Ok(None) => {}
                                    Err(e) => eprintln!("decode skip {e}"),
                                }
                            }
                        }
                    }
                    Err(e) => eprintln!("window {start}-{end} skipped: {e}"),
                }
            }
            if start < 10 {
                break;
            }
            start = start.saturating_sub(10);
            steps = steps.saturating_add(1);
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        panic!("no liquidation in scanned windows via real RPC (head={head})");
    })
    .await
    .expect("rpc timed out — fail closed");

    let (ev, log) = found;
    assert!(ev.family == "aave-v3" || ev.family == "morpho-blue" || ev.family == "spark");
    assert_eq!(ev.block_hash, log.block_hash);
    assert!(!ev.repay_amount.is_empty());
    assert!(!ev.seize_amount.is_empty());
    eprintln!(
        "PINNED_BLOCK={} family={} user={:#x} liquidator={:#x} repay={} seize={} coll_fam={:?} trig={:?}",
        ev.block,
        ev.family,
        ev.user,
        ev.liquidator,
        ev.repay_amount,
        ev.seize_amount,
        ev.coverage.collateral_family,
        ev.coverage.trigger_class
    );

    let dir = std::env::temp_dir().join(format!("liq-watch-{}", ev.block));
    std::fs::create_dir_all(&dir).unwrap();
    let mut sink = StreamSink::open(&dir.join("w.sqlite"), &dir.join("w.jsonl")).unwrap();
    sink.persist(&ev, &NoEngineJoin).unwrap();
    let jsonl = std::fs::read_to_string(dir.join("w.jsonl")).unwrap();
    assert!(jsonl.contains(&ev.instance));

    let repay = dec.asset_id(ev.repay_asset).unwrap();
    let seize = dec.asset_id(ev.seize_asset).unwrap();
    let row = liq_watch::ActualLiquidation::from_decoded(&ev, repay, seize, None, None).unwrap();
    let pq = dir.join("a.parquet");
    write_parquet(&pq, std::slice::from_ref(&row)).unwrap();
    assert!(pq.metadata().unwrap().len() > 0);
}
