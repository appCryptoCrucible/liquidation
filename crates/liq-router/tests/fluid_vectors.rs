//! The Rust Fluid DEX T1 math against the live pools: every state and answer
//! in `fixtures/fluid_vectors.json` was recorded from mainnet at one block
//! by `tools/registry/fluid_vectors.py` — the pool's raw `dexVariables`, the
//! center price hook's answer, the Liquidity layer's words for both tokens,
//! and the pool's own `swapIn` estimate (`to = ADDRESS_DEAD`: the DEX math
//! up to, not including, the Liquidity `operate` checks) for both directions
//! at seven sizes. Wherever this port answers it must answer exactly what
//! the pool did; it may refuse a size the estimate answers only because of a
//! limit the estimate cannot see (oracle price band, borrow limit, the
//! layer's balance), which the live seeding test proves against real
//! execution.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use alloy_primitives::{Address, U256};
use liq_router::{FluidState, LiqToken};
use serde_json::Value;

const NATIVE: &str = "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE";

fn u(v: &Value) -> U256 {
    v.as_str().unwrap().parse().unwrap()
}

fn liq(t: &Value) -> LiqToken {
    LiqToken {
        ep_cfg: u(&t["ep_cfg"]),
        totals: u(&t["totals"]),
        configs2: u(&t["configs2"]),
        rate_data: u(&t["rate_data"]),
        supply: u(&t["supply"]),
        borrow: u(&t["borrow"]),
        balance: u(&t["balance"]),
    }
}

fn state(p: &Value) -> FluidState {
    let tokens: Vec<Address> = ["token0", "token1"]
        .iter()
        .map(|k| p[k].as_str().unwrap().parse().unwrap())
        .collect();
    let prec: Vec<U256> = p["precisions"].as_array().unwrap().iter().map(u).collect();
    let toks = p["tokens"].as_array().unwrap();
    FluidState {
        prec: [prec[0], prec[1], prec[2], prec[3]],
        native: [
            p["token0"].as_str().unwrap() == NATIVE,
            p["token1"].as_str().unwrap() == NATIVE,
        ],
        tokens: tokens.into_iter().collect(),
        deployer: Address::ZERO,
        dex_vars: u(&p["dex_vars"]),
        dex_vars2: u(&p["dex_vars2"]),
        center_ext: p["center_ext"].as_str().map(|s| s.parse().unwrap()),
        liq: [liq(&toks[0]), liq(&toks[1])],
        exec_ts: p["timestamp"].as_u64().unwrap(),
        stale: false,
        stale_block: 0,
        read_block: p["block"].as_u64().unwrap(),
    }
}

/// What the chain really did with the same swap (`fixtures/fluid_exec.json`,
/// the same cases executed with the caller funded and the pool approved):
/// `Some(out)` or `None` for a revert.
fn executed(exec: &Value, pool_id: &Value, zfo: bool, amount: U256) -> Option<Option<U256>> {
    let list = exec["executions"].get(pool_id.to_string())?.as_array()?;
    let rec = list
        .iter()
        .find(|c| c["zero_to_one"].as_bool() == Some(zfo) && u(&c["amount"]) == amount)?;
    Some(rec["chain"].as_str().map(|s| s.parse::<U256>().unwrap()))
}

#[test]
fn fluid_math_matches_every_recorded_pool_answer() {
    let doc: Value = serde_json::from_str(include_str!("fixtures/fluid_vectors.json")).unwrap();
    let exec: Value = serde_json::from_str(include_str!("fixtures/fluid_exec.json")).unwrap();
    let pools = doc["pools"].as_array().unwrap();
    assert!(pools.len() >= 30, "the live pools");
    let (mut checked, mut refused, mut wrong) = (0usize, 0usize, Vec::new());
    let mut native_pools = 0usize;
    for p in pools {
        let st = state(p);
        if st.native.iter().any(|n| *n) {
            native_pools += 1;
        }
        for c in p["cases"].as_array().unwrap() {
            let Some(want) = c.get("out").map(u) else {
                continue;
            };
            let zfo = c["zero_to_one"].as_bool().unwrap();
            let amount = u(&c["amount"]);
            let (i, j) = if zfo { (0u8, 1u8) } else { (1u8, 0u8) };
            match st.dy(i, j, amount) {
                Ok(got) if got == want => checked += 1,
                Ok(got) => wrong.push(format!(
                    "pool {} zfo={zfo} amount={amount}: got {got}, chain {want}",
                    p["id"]
                )),
                // The estimate stops before the Liquidity layer's checks; a
                // size this port refuses must be one the real swap reverts
                // on (a borrow or withdrawal limit, the layer's balance, the
                // oracle band).
                Err(e) => match executed(&exec, &p["id"], zfo, amount) {
                    Some(None) => refused += 1,
                    other => wrong.push(format!(
                        "pool {} zfo={zfo} amount={amount}: refused ({e}) but the chain {}",
                        p["id"],
                        match other {
                            Some(Some(o)) => format!("pays {o}"),
                            _ => "was not executed".to_owned(),
                        }
                    )),
                },
            }
        }
    }
    assert!(wrong.is_empty(), "wrong answers:\n{}", wrong.join("\n"));
    assert!(native_pools >= 1, "a pool holding native ETH");
    assert!(checked >= 300, "only {checked} vectors answered exactly");
    assert!(refused >= 5, "the limit cases: {refused}");
}

/// What the chain does when the swap really executes (`eth_call` with the
/// caller funded and the pool approved, `tools/registry/fluid_exec_check.py`):
/// where this port answers it must answer the same wei, and where the chain
/// reverts — the oracle band, a borrow or withdrawal limit, the layer's
/// balance, the 50 % imaginary limit — this port must refuse too, so a plan
/// built on it never fails on a limit. It may not refuse what the chain
/// answers.
#[test]
fn fluid_math_agrees_with_real_executions() {
    let doc: Value = serde_json::from_str(include_str!("fixtures/fluid_vectors.json")).unwrap();
    let exec: Value = serde_json::from_str(include_str!("fixtures/fluid_exec.json")).unwrap();
    let (mut equal, mut both_refuse) = (0usize, 0usize);
    let mut problems = Vec::new();
    for p in doc["pools"].as_array().unwrap() {
        let st = state(p);
        let Some(list) = exec["executions"].get(p["id"].to_string()) else {
            continue;
        };
        for c in list.as_array().unwrap() {
            let zfo = c["zero_to_one"].as_bool().unwrap();
            let amount = u(&c["amount"]);
            let (i, j) = if zfo { (0u8, 1u8) } else { (1u8, 0u8) };
            let model = st.dy(i, j, amount);
            let chain = c["chain"].as_str().map(|s| s.parse::<U256>().unwrap());
            match (model, chain) {
                (Ok(m), Some(ch)) if m == ch => equal += 1,
                (Err(_), None) => both_refuse += 1,
                (m, ch) => problems.push(format!(
                    "pool {} zfo={zfo} amount={amount}: model {m:?}, chain {ch:?}",
                    p["id"]
                )),
            }
        }
    }
    assert!(
        problems.is_empty(),
        "{}",
        problems.join(
            "
"
        )
    );
    assert!(
        equal >= 300 && both_refuse >= 150,
        "{equal} equal, {both_refuse} both refuse"
    );
}

/// Three consecutive swaps in one block: the second and third are quoted off
/// the state `apply` leaves, and must equal what the chain returned (the
/// simulated block runs 12 s after the recorded one).
#[test]
fn applying_swaps_follows_the_chain_through_consecutive_swaps() {
    let doc: Value = serde_json::from_str(include_str!("fixtures/fluid_vectors.json")).unwrap();
    let seqs: Value = serde_json::from_str(include_str!("fixtures/fluid_sequences.json")).unwrap();
    let offset = seqs["time_offset"].as_u64().unwrap();
    let mut swaps = 0usize;
    for q in seqs["sequences"].as_array().unwrap() {
        let p = doc["pools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == q["pool_id"])
            .unwrap();
        let mut st = state(p);
        st.exec_ts += offset;
        for s in q["swaps"].as_array().unwrap() {
            let zfo = s["zero_to_one"].as_bool().unwrap();
            let (i, j) = if zfo { (0u8, 1u8) } else { (1u8, 0u8) };
            let want = u(&s["chain"]);
            let got = st.apply(i, j, u(&s["amount"])).unwrap();
            assert_eq!(got, want, "pool {} swap {zfo} {}", p["id"], s["amount"]);
            swaps += 1;
        }
    }
    assert!(swaps >= 70, "{swaps} swaps");
}
