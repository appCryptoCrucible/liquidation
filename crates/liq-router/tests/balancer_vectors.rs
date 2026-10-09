//! The Rust Balancer weighted-pool math against the live Vault: every state
//! and answer in `fixtures/balancer_vectors.json` was recorded from mainnet
//! at one block by `tools/registry/balancer_vectors.py` — Balancer's own
//! `BalancerQueries.querySwap` for exact input and exact output over every
//! ordered token pair at sizes from a ten-millionth of the balance to past
//! the 30 % ratio. Each answer must match to the wei, and a size the pool
//! refuses must be refused here too. The fixture holds the 2021
//! `WeightedPool2Tokens` pools (no power shortcuts) and a `WeightedPool` v4
//! pool (exponent 4 for one direction, which takes its shortcut).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use alloy_primitives::{Address, B256, U256};
use liq_router::BalancerState;
use serde_json::Value;

fn u(v: &Value) -> U256 {
    v.as_str().unwrap().parse().unwrap()
}

fn state(p: &Value) -> BalancerState {
    let tokens: Vec<Address> = p["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap().parse().unwrap())
        .collect();
    let decimals: Vec<u64> = p["decimals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_u64().unwrap())
        .collect();
    BalancerState {
        pool_id: p["pool_id"].as_str().unwrap().parse::<B256>().unwrap(),
        tokens: tokens.into_iter().collect(),
        balances: p["balances"].as_array().unwrap().iter().map(u).collect(),
        weights: p["weights"].as_array().unwrap().iter().map(u).collect(),
        scaling: decimals
            .iter()
            .map(|d| U256::from(10u64).pow(U256::from(18u64.saturating_sub(*d))))
            .collect(),
        swap_fee: u(&p["swap_fee"]),
        fast_pow: p["kind"].as_str().unwrap() == "weighted_v4",
        stale: false,
        stale_block: 0,
        read_block: 0,
    }
}

#[test]
fn balancer_math_matches_every_recorded_vault_answer() {
    let doc: Value = serde_json::from_str(include_str!("fixtures/balancer_vectors.json")).unwrap();
    let pools = doc["pools"].as_array().unwrap();
    assert!(pools.len() >= 4, "the four reviewed pools");
    let (mut checked, mut refused, mut wrong) = (0usize, 0usize, Vec::new());
    let mut kinds = std::collections::BTreeSet::new();
    for p in pools {
        kinds.insert(p["kind"].as_str().unwrap().to_owned());
        let st = state(p);
        for c in p["cases"].as_array().unwrap() {
            let i = u8::try_from(c[0].as_u64().unwrap()).unwrap();
            let j = u8::try_from(c[1].as_u64().unwrap()).unwrap();
            let exact_in = c[2].as_u64().unwrap() == 0;
            let amount = u(&c[3]);
            let want = if c[4].is_null() { None } else { Some(u(&c[4])) };
            let got = if exact_in {
                st.dy(i, j, amount).ok()
            } else {
                st.dx(i, j, amount).ok()
            };
            checked += 1;
            if want.is_none() {
                refused += 1;
            }
            if got != want {
                wrong.push(format!(
                    "{} {} {i}->{j} {} {amount}: rust {got:?} chain {want:?}",
                    p["pool"],
                    p["kind"],
                    if exact_in { "in" } else { "out" }
                ));
            }
        }
    }
    assert!(checked >= 100, "{checked} cases");
    assert!(
        refused >= 8,
        "the 30 % ratio refusals are covered: {refused}"
    );
    assert!(
        kinds.contains("weighted_v1") && kinds.contains("weighted_v4"),
        "both generations: {kinds:?}"
    );
    assert!(
        wrong.is_empty(),
        "{} mismatches:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// Applying a swap moves the balances as the Vault does (the input joins its
/// token's, the output leaves the other's), so a second swap quotes the
/// displaced pool, and that is what `BalancerQueries` answers after the first
/// swap's state change is applied to the recorded balances. Oracle: the same
/// pool's closed form on the displaced balances, by the same function.
#[test]
fn applying_a_swap_displaces_the_balances() {
    let doc: Value = serde_json::from_str(include_str!("fixtures/balancer_vectors.json")).unwrap();
    for p in doc["pools"].as_array().unwrap() {
        let mut st = state(p);
        let dx = st.balances[0] / U256::from(1_000u64);
        let before = st.clone();
        let out = st.apply(0, 1, dx).unwrap();
        assert_eq!(st.balances[0], before.balances[0] + dx);
        assert_eq!(st.balances[1], before.balances[1] - out);
        // The pool is now worse for the same direction.
        let again = st.dy(0, 1, dx).unwrap();
        assert!(again <= out, "{again} vs {out}");
        // And better for the way back: selling `out` returns less than `dx`
        // (fees), but more than selling it into the untouched pool would.
        let back_fresh = before.dy(1, 0, out).unwrap();
        let back_displaced = st.dy(1, 0, out).unwrap();
        assert!(back_displaced >= back_fresh);
    }
}
