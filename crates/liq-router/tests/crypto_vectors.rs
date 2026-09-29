//! The Rust crypto-pool math against live pools: every state and `get_dy`
//! in `fixtures/crypto_vectors.json` was read from mainnet at one block by
//! `tools/registry/crypto_vectors.py` (57 admitted pools, three sizes, every
//! ordered pair). Each output must match to the wei, and a size the pool
//! refuses must be refused here too.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use alloy_primitives::U256;
use liq_router::{CryptoKind, CryptoState};
use serde_json::Value;

fn u(v: &Value) -> U256 {
    v.as_str().unwrap().parse().unwrap()
}

fn us(v: &Value) -> smallvec::SmallVec<[U256; 4]> {
    v.as_array().unwrap().iter().map(u).collect()
}

#[test]
fn crypto_math_matches_every_recorded_pool() {
    let raw = include_str!("fixtures/crypto_vectors.json");
    let doc: Value = serde_json::from_str(raw).unwrap();
    let pools = doc["pools"].as_array().unwrap();
    assert!(pools.len() >= 50, "fixture holds the admitted pools");
    let mut checked = 0usize;
    let mut wrong = Vec::new();
    for p in pools {
        let kind = match p["kind"].as_str().unwrap() {
            "two_v1" => CryptoKind::TwoV1,
            "two_v200" => CryptoKind::TwoV200,
            "two_v210" => CryptoKind::TwoV210,
            "tri" => CryptoKind::Tri,
            k => panic!("kind {k}"),
        };
        let st = CryptoState {
            kind,
            balances: us(&p["balances"]),
            precisions: us(&p["precisions"]),
            price_scale: us(&p["price_scale"]),
            d: u(&p["d"]),
            ann: u(&p["ann"]),
            gamma: u(&p["gamma"]),
            mid_fee: u(&p["mid_fee"]),
            out_fee: u(&p["out_fee"]),
            fee_gamma: u(&p["fee_gamma"]),
            stale: false,
            stale_block: 0,
            read_block: 0,
        };
        for c in p["cases"].as_array().unwrap() {
            let i = u8::try_from(c[0].as_u64().unwrap()).unwrap();
            let j = u8::try_from(c[1].as_u64().unwrap()).unwrap();
            let dx = u(&c[2]);
            let want = if c[3].is_null() { None } else { Some(u(&c[3])) };
            let got = st.dy(i, j, dx).ok();
            checked += 1;
            if got != want {
                wrong.push(format!(
                    "{} {:?} {i}->{j} dx {dx}: rust {got:?} chain {want:?}",
                    p["pool"], kind
                ));
            }
        }
    }
    assert!(checked >= 400, "{checked} cases");
    assert!(
        wrong.is_empty(),
        "{} mismatches:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// The water-fill inverts `ρ(x)`, so it must not rise with size: every
/// recorded pool, every ordered pair, from zero through a twentieth of the
/// input coin's balance.
#[test]
fn crypto_marginal_price_falls_with_size() {
    let doc: Value = serde_json::from_str(include_str!("fixtures/crypto_vectors.json")).unwrap();
    let mut rising = Vec::new();
    for p in doc["pools"].as_array().unwrap() {
        let kind = match p["kind"].as_str().unwrap() {
            "two_v1" => CryptoKind::TwoV1,
            "two_v200" => CryptoKind::TwoV200,
            "two_v210" => CryptoKind::TwoV210,
            _ => CryptoKind::Tri,
        };
        let st = CryptoState {
            kind,
            balances: us(&p["balances"]),
            precisions: us(&p["precisions"]),
            price_scale: us(&p["price_scale"]),
            d: u(&p["d"]),
            ann: u(&p["ann"]),
            gamma: u(&p["gamma"]),
            mid_fee: u(&p["mid_fee"]),
            out_fee: u(&p["out_fee"]),
            fee_gamma: u(&p["fee_gamma"]),
            stale: false,
            stale_block: 0,
            read_block: 0,
        };
        let n = st.balances.len();
        for i in 0..n {
            for j in 0..n {
                if i == j {
                    continue;
                }
                let (i8_, j8) = (u8::try_from(i).unwrap(), u8::try_from(j).unwrap());
                let b = st.balances[i];
                // Dust pools (under a million raw units of the input coin)
                // quote in single output units; their ρ is rounding noise.
                if b < U256::from(1_000_000u64) {
                    continue;
                }
                let mut prev: Option<U256> = None;
                for x in [
                    U256::ZERO,
                    b / U256::from(1000u64),
                    b / U256::from(100u64),
                    b / U256::from(20u64),
                ] {
                    let Ok(r) = st.rho(i8_, j8, x) else { break };
                    if let Some(pr) = prev {
                        // A forward difference of integer outputs can tick up
                        // by rounding; allow 0.01 %.
                        if r > pr + pr / U256::from(10_000u64) {
                            rising.push(format!("{} {i}->{j} at {x}: {pr} -> {r}", p["pool"]));
                        }
                    }
                    prev = Some(r);
                }
            }
        }
    }
    assert!(rising.is_empty(), "ρ rises:\n{}", rising.join("\n"));
}
