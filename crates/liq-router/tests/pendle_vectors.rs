//! The Rust Pendle market math against live sales: every case in
//! `fixtures/pendle_vectors.json` is a real `swapExactPtForSy` on a
//! `PendleMarketV6` market, run from a real PT holder at one block (the
//! market state and index read at the same block). The port must pay the
//! same SY to the wei.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use alloy_primitives::{I256, U256};
use liq_router::pendle::{sell_pt, MarketSnapshot};
use serde_json::Value;

fn u(v: &Value) -> U256 {
    v.as_str().unwrap().parse().unwrap()
}

fn i(v: &Value) -> I256 {
    v.as_str().unwrap().parse().unwrap()
}

#[test]
fn pendle_math_matches_every_recorded_sale() {
    let doc: Value = serde_json::from_str(include_str!("fixtures/pendle_vectors.json")).unwrap();
    let cases = doc["vectors"].as_array().unwrap();
    assert!(cases.len() >= 20, "fixture holds the recorded sales");
    let mut wrong = Vec::new();
    for c in cases {
        let st = &c["state"];
        let snap = MarketSnapshot {
            total_pt: i(&st["total_pt"]),
            total_sy: i(&st["total_sy"]),
            scalar_root: i(&st["scalar_root"]),
            expiry: st["expiry"].as_str().unwrap().parse().unwrap(),
            ln_fee_rate_root: u(&st["ln_fee_rate_root"]),
            reserve_fee_percent: u(&st["reserve_fee_percent"]),
            last_ln_implied_rate: u(&st["last_ln_implied_rate"]),
            index: u(&st["index"]),
            quote_ts: c["ts"].as_u64().unwrap(),
            out_per_sy_scale: U256::ZERO,
            sy_scale: U256::ZERO,
        };
        let got = sell_pt(&snap, u(&c["pt_in"])).ok();
        let want = u(&c["sy_out"]);
        if got != Some(want) {
            wrong.push(format!(
                "{} {}: port {got:?} chain {want}",
                c["market"], c["pt_in"]
            ));
        }
    }
    assert!(wrong.is_empty(), "port != chain:\n{}", wrong.join("\n"));
}
