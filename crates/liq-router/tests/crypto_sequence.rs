//! A 2025 Twocrypto pool's own state change, swap after swap, against the
//! chain: every sequence in `fixtures/crypto_sequence.json` was run by
//! `tools/registry/crypto_sequence.py` with `eth_simulateV1` at one block
//! (three `exchange`s on each admitted `two_stable` pool, both ways), its
//! `tweak_price` state read beside it. Applying each swap here must pay the
//! same `dy` and leave the same balances, `D` and `price_scale`, to the
//! wei: the first swap's oracle update and rebalance included, and the
//! swaps after it, which the pool's once-per-block guard keeps from
//! rebalancing.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use alloy_primitives::U256;
use liq_router::{CryptoKind, CryptoState, TweakState};
use serde_json::Value;

fn u(v: &Value) -> U256 {
    v.as_str().unwrap().parse().unwrap()
}

fn us(v: &Value) -> smallvec::SmallVec<[U256; 4]> {
    v.as_array().unwrap().iter().map(u).collect()
}

/// A storage field absent on v2.1.0d (`null`) is zero.
fn opt(v: &Value) -> U256 {
    if v.is_null() {
        U256::ZERO
    } else {
        u(v)
    }
}

#[test]
fn a_followed_pool_pays_what_the_chain_paid_swap_after_swap() {
    let raw = include_str!("fixtures/crypto_sequence.json");
    let doc: Value = serde_json::from_str(raw).unwrap();
    let seqs = doc["sequences"].as_array().unwrap();
    assert!(seqs.len() >= 4, "both pools, both ways");
    let mut rebalanced = 0usize;
    let mut versions = std::collections::BTreeSet::new();
    for s in seqs {
        assert_eq!(s["kind"].as_str().unwrap(), "two_stable");
        let version = s["version"].as_str().unwrap();
        versions.insert(version.to_owned());
        let st = &s["storage"];
        let tweak = TweakState {
            v3: version == "v3.0.0",
            price_oracle: u(&st["price_oracle"]),
            last_prices: u(&st["last_prices"]),
            last_timestamp: u(&st["last_timestamp"]),
            packed_rebalancing_params: u(&st["packed_rebalancing_params"]),
            total_supply: u(&st["totalSupply"]),
            donation_shares: u(&st["donation_shares"]),
            donation_duration: u(&st["donation_duration"]),
            last_donation_release_ts: u(&st["last_donation_release_ts"]),
            donation_protection_expiry_ts: u(&st["donation_protection_expiry_ts"]),
            donation_protection_period: u(&st["donation_protection_period"]),
            virtual_price: u(&st["virtual_price"]),
            xcp_profit: u(&st["xcp_profit"]),
            lp_xcp_profit: opt(&st["lp_xcp_profit"]),
            reserved_profit_fraction: opt(&st["reserved_profit_fraction"]),
            admin_fee: u(&st["admin_fee"]),
            exec_ts: U256::from(s["timestamp"].as_u64().unwrap()),
            oracle_slot: U256::ZERO,
        };
        let mut pool = CryptoState {
            kind: CryptoKind::TwoStable,
            balances: us(&s["before"]["balances"]),
            precisions: us(&s["precisions"]),
            price_scale: us(&Value::Array(vec![s["before"]["price_scale"].clone()])),
            d: u(&s["before"]["d"]),
            ann: u(&s["ann"]),
            gamma: u(&s["gamma"]),
            mid_fee: u(&s["mid_fee"]),
            out_fee: u(&s["out_fee"]),
            fee_gamma: u(&s["fee_gamma"]),
            stale: false,
            stale_block: 0,
            read_block: 0,
            handler: 0,
            tweak: Some(tweak),
        };
        if s["first_rebalanced"].as_bool().unwrap() {
            rebalanced += 1;
        }
        for (k, step) in s["steps"].as_array().unwrap().iter().enumerate() {
            let i = u8::try_from(step["i"].as_u64().unwrap()).unwrap();
            let j = u8::try_from(step["j"].as_u64().unwrap()).unwrap();
            let dx = u(&step["dx"]);
            let tag = format!("{} {version} step {k} {i}->{j}", s["pool"]);
            // The quote before the swap is the swap's own output.
            assert_eq!(pool.dy(i, j, dx).unwrap(), u(&step["dy"]), "{tag} quote");
            let dy = pool.apply(i, j, dx).expect(&tag);
            assert_eq!(dy, u(&step["dy"]), "{tag} dy");
            assert!(pool.is_live(), "{tag}: still quotable");
            assert_eq!(pool.balances, us(&step["balances"]), "{tag} balances");
            assert_eq!(pool.d, u(&step["d"]), "{tag} D");
            assert_eq!(
                pool.price_scale[0],
                u(&step["price_scale"]),
                "{tag} price_scale"
            );
        }
    }
    assert!(
        rebalanced > 0,
        "no first swap rebalanced: the rebalance is untested"
    );
    assert!(
        versions.contains("v2.1.0d") && versions.contains("v3.0.0"),
        "both versions: {versions:?}"
    );
}
