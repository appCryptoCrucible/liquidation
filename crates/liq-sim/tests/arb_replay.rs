//! Live: backrun plans (`liq-arb`) simulated by `liq_sim` as production
//! will run them — `ArbExecutor` placed beside the shared swap modules, the
//! block priced at its real base fee — and held to the chain.
//!
//! The cases are the fork suite's (`contracts/test/fixtures/arb/
//! busy_hour.json`): plans built on the state just before a winning backrun
//! of the 2026-10-11 busy hour, whose WETH the mainnet fork already proved
//! equal to the models, to the wei. Here the simulator starts from the
//! parent block's state over RPC, replays the block's transactions before
//! the winner in their order ([`liq_sim::verify_ordered`], each must
//! succeed), runs the plan, and must find that same WETH.
//!
//! Only cases whose winner sits within `ARB_SIM_MAX_PRIOR` (default 8)
//! transactions of the top of its block run: every earlier transaction is
//! replayed over RPC. Flash-loan cases only (a flash-swap case is held to a
//! bound, which the fork suite covers).
//!
//! `MAINNET_RPC_URL`: an archive endpoint.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use alloy_primitives::{Address, Bytes, B256, I256, U256};
use liq_sim::{
    arb_executor_stack, block_env_at, execute_calldata, mainnet_spec, verify_ordered, BlockState,
    ExecutorSpec, Ordered, SimEnv, SimTx, Simulator, PLANNED_ARB_EXECUTOR,
};
use serde_json::{json, Value};

mod common;
use common::{access_list, hex, num, parse, word, AtBlock, Rpc};

const WETH: Address = alloy_primitives::address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

fn fixtures() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/test/fixtures/arb/busy_hour.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture file"))
        .expect("fixture json")
}

fn sim_tx(tx: &Value) -> SimTx {
    SimTx {
        caller: parse(&tx["from"]).unwrap(),
        to: parse(&tx["to"]).unwrap(),
        value: word(&tx["value"]).unwrap(),
        data: parse(&tx["input"]).unwrap(),
        gas_limit: num(&tx["gas"]).unwrap(),
        access_list: access_list(&tx["accessList"]),
    }
}

#[test]
fn arb_plans_simulate_as_the_fork_proved() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    let Ok(url) = std::env::var("MAINNET_RPC_URL") else {
        eprintln!("MAINNET_RPC_URL unset: skipped");
        return;
    };
    let max_prior: u64 = std::env::var("ARB_SIM_MAX_PRIOR")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let rpc = Arc::new(Rpc {
        http: reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap(),
        url,
        calls: AtomicU64::new(0),
    });
    let operator = Address::repeat_byte(0x0a);
    let sink = Address::repeat_byte(0x5c);
    let spec = ExecutorSpec::mainnet(operator, operator, sink);
    let code = arb_executor_stack(&spec)
        .expect("forge build ArbExecutor")
        .placements(PLANNED_ARB_EXECUTOR);

    let fx = fixtures();
    let mut ran = 0;
    for c in fx["cases"].as_array().unwrap() {
        if c["flash_swap"].as_bool().unwrap() {
            continue;
        }
        let txh: B256 = parse(&c["tx"]).unwrap();
        let tx = rpc
            .call("eth_getTransactionByHash", json!([format!("{txh:#x}")]))
            .unwrap();
        let index = num(&tx["transactionIndex"]).unwrap();
        if index > max_prior {
            continue;
        }
        let block = num(&tx["blockNumber"]).unwrap();
        let header = rpc
            .call("eth_getBlockByNumber", json!([hex(block), true]))
            .unwrap();
        let ts = num(&header["timestamp"]).unwrap();
        let base_fee = num(&header["baseFeePerGas"]).unwrap();
        // The block's transactions before the winner, in order; a reverted
        // one changed nothing a backrun reads and is left out.
        let mut prior = Vec::new();
        for t in header["transactions"]
            .as_array()
            .unwrap()
            .iter()
            .take(usize::try_from(index).unwrap())
        {
            let r = rpc
                .call("eth_getTransactionReceipt", json!([t["hash"]]))
                .unwrap();
            if num(&r["status"]).unwrap() == 1
                && !t["to"].is_null()
                && num(&t["type"]).unwrap_or(0) < 3
            {
                prior.push(sim_tx(t));
            }
        }
        let state = BlockState::locked(AtBlock {
            rpc: Arc::clone(&rpc),
            tag: hex(block - 1),
        });
        let mut sim =
            Simulator::with_executor(Arc::new(state), PLANNED_ARB_EXECUTOR, &code, WETH, sink)
                .unwrap();
        let mut benv = block_env_at(block, ts);
        benv.gas_limit = num(&header["gasLimit"]).unwrap();
        benv.beneficiary = parse(&header["miner"]).unwrap();
        benv.prevrandao = Some(parse(&header["mixHash"]).unwrap());
        let env = SimEnv::new(mainnet_spec(ts).unwrap(), benv).with_gas_price(base_fee, 0);
        let plan: Bytes = parse(&c["plan"]).unwrap();
        let ours = SimTx {
            caller: operator,
            to: PLANNED_ARB_EXECUTOR,
            value: U256::ZERO,
            data: execute_calldata(&plan),
            gas_limit: 3_000_000,
            access_list: Vec::new(),
        };
        let expected = I256::try_from(
            U256::from_str_radix(c["expected_gross_wei"].as_str().unwrap(), 10).unwrap(),
        )
        .unwrap();
        match verify_ordered(&mut sim, &prior, &[ours], &env)
            .unwrap_or_else(|e| panic!("{txh}: simulator refused: {e}"))
        {
            Ordered::Ran(o) => {
                eprintln!(
                    "{block}/tx{index}: {} prior txs, gross {} expected {expected}, gas {}",
                    prior.len(),
                    o.net_profit_wei,
                    o.gas_used
                );
                assert_eq!(
                    o.net_profit_wei, expected,
                    "{txh}: simulated WETH differs from the fork-proved figure"
                );
            }
            Ordered::VictimReverted(k) => {
                panic!("{txh}: prior tx {k} reverted in the simulator but succeeded on chain")
            }
        }
        ran += 1;
    }
    eprintln!(
        "{ran} cases simulated, {} RPC calls",
        rpc.calls.load(std::sync::atomic::Ordering::Relaxed)
    );
    assert!(ran > 0, "no case within ARB_SIM_MAX_PRIOR");
}
