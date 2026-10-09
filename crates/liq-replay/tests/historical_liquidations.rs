//! Live: real liquidations replayed through our own searcher and Executor at
//! the moment each happened. State comes from the free endpoint, read at the
//! block before and cached on disk (`data/history-cache/`); no archive node
//! and no running bot. Needs `MAINNET_RPC_URL`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::float_arithmetic,
    unreachable_pub,
    dead_code
)]

mod historical;

use historical::fixture;
use historical::rpc::Upstream;
use historical::state::{Mode, PreState, Tx};
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn upstream() -> Option<Arc<Upstream>> {
    let url = std::env::var("MAINNET_RPC_URL")
        .ok()
        .filter(|s| !s.is_empty())?;
    let cache =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/history-cache/rpc.jsonl");
    Some(Arc::new(Upstream::new(url, &cache)))
}

/// The pre-state is the chain's: each real liquidation reproduces its own
/// event on it. Most work at the bot's tip (the block before); a backrun
/// needs its block's earlier oracle updates, and anything else the block's
/// whole prefix.
#[test]
#[ignore = "needs MAINNET_RPC_URL"]
fn their_liquidations_reproduce_on_the_pre_state() {
    let Some(up) = upstream() else { return };
    let events = fixture::load();
    let mut reproduced = 0;
    for ev in &events {
        let (pre, basis) = match PreState::establish(&up, ev.block, ev.tx, ev.tx_index) {
            Ok(v) => v,
            Err(e) => {
                println!("{} {:<12} tx #{:<3}: {e}", ev.block, ev.family, ev.tx_index);
                continue;
            }
        };
        let tx = Tx::fetch(&up, ev.tx).unwrap();
        let r = pre.try_run(tx.env, Mode::Chain).unwrap();
        let same = r.logs.iter().any(|l| {
            l.address == ev.emitter && l.data.data == ev.data && l.topics() == ev.topics.as_slice()
        });
        if r.success && same {
            reproduced += 1;
        }
        println!(
            "{} {:<12} tx #{:<3}: {:?}, {}",
            ev.block,
            ev.family,
            ev.tx_index,
            basis,
            if same { "same event" } else { "event differs" }
        );
    }
    println!(
        "{reproduced}/{} reproduce their own event; {} upstream requests",
        events.len(),
        up.fetched.load(Ordering::Relaxed)
    );
    assert_eq!(reproduced, events.len());
}

/// Aave V3 liquidations through our own searcher and Executor: the bot's
/// store seeded at the block before, its drain run there, and the job it
/// builds compared with what the real liquidator captured on the same
/// state. `LIQ_HISTORY_BLOCK` limits the run to one block.
#[test]
#[ignore = "needs MAINNET_RPC_URL"]
fn aave_v3_liquidations_through_our_searcher() {
    through_our_searcher(Some("aave-v3"));
}

/// Every captured liquidation, whatever its protocol: Aave V3, Morpho Blue
/// and Compound V2 through our searcher; the rest say why they are out of
/// scope. `LIQ_HISTORY_FAMILY` limits the run to one family.
#[test]
#[ignore = "needs MAINNET_RPC_URL"]
fn all_liquidations_through_our_searcher() {
    let family = std::env::var("LIQ_HISTORY_FAMILY").ok();
    through_our_searcher(family.as_deref());
}

fn through_our_searcher(family: Option<&str>) {
    let Some(up) = upstream() else { return };
    // `LIQ_HISTORY_DEBUG=liq_router::exact,...`: those targets at debug.
    let debug = std::env::var("LIQ_HISTORY_DEBUG").unwrap_or_default();
    let targets = debug.split(',').filter(|t| !t.is_empty()).fold(
        tracing_subscriber::filter::Targets::new().with_default(tracing::Level::INFO),
        |t, name| t.with_target(name.to_owned(), tracing::Level::DEBUG),
    );
    {
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .finish()
            .with(targets)
            .try_init();
    }
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    let only: Option<u64> = std::env::var("LIQ_HISTORY_BLOCK")
        .ok()
        .and_then(|s| s.parse().ok());
    let eth = |w: i128| w as f64 / 1e18;
    let mut seen = std::collections::HashSet::new();
    for ev in fixture::load()
        .iter()
        .filter(|e| family.is_none_or(|f| e.family == f))
    {
        // One transaction can carry several events of one borrower.
        if !seen.insert((ev.tx, ev.emitter, ev.topics.clone(), ev.data.clone())) {
            continue;
        }
        if only.is_some_and(|b| b != ev.block) {
            continue;
        }
        let scratch =
            std::env::temp_dir().join(format!("liq-history-{}-{}", ev.block, ev.log_index));
        let r = historical::drive::replay(&up, ev, &repo, &scratch);
        let theirs = r.theirs.as_ref().map_or("?".to_string(), |o| {
            format!(
                "{:.5} ETH captured, {:.5} to builder, ok {}, gas {}, unpriced {:?}",
                eth(o.captured),
                eth(o.to_builder),
                o.success,
                o.gas_used,
                o.unpriced
            )
        });
        let wad = |v: alloy_primitives::U256| v.saturating_to::<u128>() as f64 / 1e18;
        let health = r
            .health
            .as_ref()
            .map_or(r.health_note.clone().unwrap_or("?".into()), |h| {
                let (hf, coll, debt) = h.chain;
                let pool = format!("hf {:.6} coll {coll} debt {debt}", wad(hf));
                match (&h.ours, &h.ours_error) {
                    (Some(_), _) if h.agrees() && h.hf_only => {
                        format!("agrees with the pool on hf (V2: its values are ETH): {pool}")
                    }
                    (Some(_), _) if h.agrees() => format!("agrees with the pool: {pool}"),
                    (Some((hf, coll, debt)), _) => format!(
                        "DIFFERS: ours hf {:.6} coll {coll} debt {debt}; pool {pool}",
                        wad(*hf)
                    ),
                    (None, e) => format!("ours failed ({e:?}); pool {pool}"),
                }
            });
        println!(
            "{} {} tx #{:<3} {:?}\n    health: {health}\n    theirs: {theirs}\n    ours:   {:?}\n    {}s, {} upstream, {} served, unknown methods {:?}",
            ev.block, ev.family, ev.tx_index, r.basis, r.verdict, r.seconds, r.upstream, r.served, r.unknown
        );
        for line in &r.diag {
            println!("    {line}");
        }
    }
}

/// What winners paid the builder, per liquidation transaction (the bid
/// question): each one replayed alone on the state before it, as the replay
/// measures `theirs`. `LIQ_BIDS_TXS` is a JSON list of `{tx, block, index,
/// family}`; one JSON line per transaction is appended to `LIQ_BIDS_OUT`,
/// and transactions already there are skipped, so a run resumes.
#[test]
#[ignore = "needs MAINNET_RPC_URL, LIQ_BIDS_TXS and LIQ_BIDS_OUT"]
fn winning_bids() {
    use std::io::Write as _;
    let Some(up) = upstream() else { return };
    let txs_path = std::env::var("LIQ_BIDS_TXS").expect("LIQ_BIDS_TXS");
    let out_path = std::env::var("LIQ_BIDS_OUT").expect("LIQ_BIDS_OUT");
    let txs: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(&txs_path).unwrap()).unwrap();
    let done: std::collections::HashSet<String> = std::fs::read_to_string(&out_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v["tx"].as_str().map(str::to_owned))
        .collect();
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&out_path)
        .unwrap();
    let (mut ran, total) = (0usize, txs.len());
    for t in &txs {
        let tx = t["tx"].as_str().unwrap().to_owned();
        if done.contains(&tx) {
            continue;
        }
        let block = t["block"].as_u64().unwrap();
        let index = t["index"].as_u64().unwrap();
        let hash: alloy_primitives::B256 = tx.parse().unwrap();
        let line = match historical::drive::winner(&up, block, hash, index) {
            Ok((basis, o)) => serde_json::json!({
                "tx": tx, "block": block, "family": t["family"],
                "basis": format!("{basis:?}"),
                "success": o.success, "gas": o.gas_used,
                "captured": o.captured.to_string(), "to_builder": o.to_builder.to_string(),
                "unpriced": o.unpriced.iter().map(|a| format!("{a:#x}")).collect::<Vec<_>>(),
            }),
            Err(e) => {
                serde_json::json!({"tx": tx, "block": block, "family": t["family"], "err": e})
            }
        };
        writeln!(out, "{line}").unwrap();
        out.flush().unwrap();
        ran += 1;
        if ran % 25 == 0 {
            eprintln!(
                "winning_bids: {ran} run this pass, {} of {total} done",
                done.len() + ran
            );
        }
    }
    eprintln!(
        "winning_bids: {ran} run this pass, {} of {total} done",
        done.len() + ran
    );
}
