//! Live: recent mainnet liquidations replayed through the simulator.
//!
//! Each liquidation of the last `LIQ_REPLAY_BLOCKS` blocks (the three Aave V3
//! pools, Spark, Morpho Blue) runs through `liq_sim::verify` on the state
//! after its parent block, read over RPC at that block the way the
//! production simulator reads it from Reth, in its own block's header,
//! under the simulator's rules: fork by timestamp, gas price and base fee
//! zero, nonces unchecked.
//!
//! - The node's own `eth_simulateV1` of the same call on the same parent
//!   must agree: same outcome, same gas.
//! - A liquidation that opened its block must match its receipt: no earlier
//!   transaction of that block changed what it read.
//!
//! `MAINNET_RPC_URL`: an archive endpoint, or the bot's own node for blocks
//! it still holds. `LIQ_REPLAY_BLOCKS` (default 600), `LIQ_REPLAY_MAX`
//! (default 8).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use liq_sim::{
    block_env_at, mainnet_spec, verify, BlockState, Bundle, SimEnv, SimError, SimTx, Simulator,
    Trigger,
};
use revm::database_interface::{DBErrorMarker, DatabaseRef};
use revm::state::{AccountInfo, Bytecode};
use serde_json::{json, Value};

/// Liquidation events, by emitter (`config/protocols/*.toml`).
const POOLS: [(Address, &str); 5] = [
    (
        address!("87870bca3f3fd6335c3f4ce8392d69350b4fa4e2"),
        "aave-v3 core",
    ),
    (
        address!("4e033931ad43597d96d6bcc25c280717730b58b1"),
        "aave-v3 prime",
    ),
    (
        address!("0aa97c284e98396202b6a04024f5e2c65026f3c0"),
        "aave-v3 etherfi",
    ),
    (
        address!("c13e21b648a5ee794902342038ff3adab66be987"),
        "spark",
    ),
    (
        address!("bbbbbbbbbb9cc5e90e3b3af64bdaf62c37eeffcb"),
        "morpho-blue",
    ),
];

struct Rpc {
    http: reqwest::blocking::Client,
    url: String,
    calls: AtomicU64,
}

impl Rpc {
    /// One JSON-RPC call. Throttling is retried; any other error is the
    /// answer.
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut wait = 200u64;
        for _ in 0..14 {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let reply = self
                .http
                .post(&self.url)
                .json(&body)
                .send()
                .and_then(reqwest::blocking::Response::json::<Value>);
            // A transport error or an unparseable body is retried too.
            if let Ok(v) = reply {
                match v.get("error") {
                    None => return Ok(v["result"].clone()),
                    Some(e) => {
                        let msg = e.to_string();
                        let throttled = msg.contains("429")
                            || msg.contains("exceeded")
                            || msg.contains("rate limit")
                            || msg.contains("compute units")
                            || msg.contains("-32001")
                            || msg.contains("Unable to complete request");
                        if !throttled {
                            return Err(format!("{method}: {msg}"));
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(wait));
            wait = (wait * 2).min(5_000);
        }
        Err(format!("{method}: retries exhausted"))
    }
}

fn hex(n: u64) -> String {
    format!("{n:#x}")
}

fn num(v: &Value) -> Result<u64, String> {
    let s = v.as_str().ok_or_else(|| format!("not a quantity: {v}"))?;
    u64::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| e.to_string())
}

fn word(v: &Value) -> Result<U256, String> {
    let s = v.as_str().ok_or_else(|| format!("not a quantity: {v}"))?;
    U256::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| e.to_string())
}

fn parse<T: std::str::FromStr>(v: &Value) -> Result<T, String>
where
    T::Err: std::fmt::Display,
{
    let s = v.as_str().ok_or_else(|| format!("not a string: {v}"))?;
    s.parse().map_err(|e: T::Err| e.to_string())
}

/// A transaction's `accessList` field; absent on a legacy transaction.
fn access_list(v: &Value) -> Vec<(Address, Vec<B256>)> {
    v.as_array()
        .map(|items| {
            items
                .iter()
                .map(|i| {
                    let keys = i["storageKeys"]
                        .as_array()
                        .map(|ks| ks.iter().map(|k| parse(k).unwrap()).collect())
                        .unwrap_or_default();
                    (parse(&i["address"]).unwrap(), keys)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug)]
struct ReadFailed(String);

impl std::fmt::Display for ReadFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReadFailed {}

impl DBErrorMarker for ReadFailed {}

/// Chain state after block `tag`, read lazily over RPC.
struct AtBlock {
    rpc: Arc<Rpc>,
    tag: String,
}

impl AtBlock {
    fn get(&self, method: &str, params: Value) -> Result<Value, ReadFailed> {
        self.rpc.call(method, params).map_err(|e| {
            eprintln!("state read failed at {}: {e}", self.tag);
            ReadFailed(e)
        })
    }
}

impl DatabaseRef for AtBlock {
    type Error = ReadFailed;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, ReadFailed> {
        let a = format!("{address:#x}");
        let balance =
            word(&self.get("eth_getBalance", json!([a, self.tag]))?).map_err(ReadFailed)?;
        let nonce =
            num(&self.get("eth_getTransactionCount", json!([a, self.tag]))?).map_err(ReadFailed)?;
        let code: Bytes =
            parse(&self.get("eth_getCode", json!([a, self.tag]))?).map_err(ReadFailed)?;
        if balance.is_zero() && nonce == 0 && code.is_empty() {
            return Ok(None);
        }
        let code = Bytecode::new_raw_checked(code).map_err(|e| ReadFailed(format!("{e:?}")))?;
        Ok(Some(AccountInfo {
            balance,
            nonce,
            code_hash: code.hash_slow(),
            code: Some(code),
            account_id: None,
        }))
    }

    fn code_by_hash_ref(&self, _: B256) -> Result<Bytecode, ReadFailed> {
        Err(ReadFailed("code is served with its account".into()))
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, ReadFailed> {
        let slot = format!("{:#x}", B256::from(index));
        word(&self.get(
            "eth_getStorageAt",
            json!([format!("{address:#x}"), slot, self.tag]),
        )?)
        .map_err(ReadFailed)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, ReadFailed> {
        let b = self.get("eth_getBlockByNumber", json!([hex(number), false]))?;
        parse(&b["hash"]).map_err(ReadFailed)
    }
}

struct Found {
    tx: B256,
    block: u64,
    index: u64,
    protocol: &'static str,
}

fn recent_liquidations(rpc: &Rpc, head: u64, span: u64) -> Vec<Found> {
    let liquidation_call =
        keccak256("LiquidationCall(address,address,address,uint256,uint256,address,bool)");
    let liquidate =
        keccak256("Liquidate(bytes32,address,address,uint256,uint256,uint256,uint256,uint256)");
    let emitters: Vec<String> = POOLS.iter().map(|(a, _)| format!("{a:#x}")).collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut from = head.saturating_sub(span);
    while from <= head {
        // Ten blocks: the free-tier `eth_getLogs` range.
        let to = (from + 9).min(head);
        let logs = rpc
            .call(
                "eth_getLogs",
                json!([{
                    "fromBlock": hex(from),
                    "toBlock": hex(to),
                    "address": emitters,
                    "topics": [[format!("{liquidation_call:#x}"), format!("{liquidate:#x}")]],
                }]),
            )
            .unwrap();
        for l in logs.as_array().unwrap() {
            let tx: B256 = parse(&l["transactionHash"]).unwrap();
            if !seen.insert(tx) {
                continue;
            }
            let emitter: Address = parse(&l["address"]).unwrap();
            let protocol = POOLS
                .iter()
                .find(|(a, _)| *a == emitter)
                .map_or("?", |(_, n)| *n);
            out.push(Found {
                tx,
                block: num(&l["blockNumber"]).unwrap(),
                index: num(&l["transactionIndex"]).unwrap(),
                protocol,
            });
        }
        from = to + 1;
    }
    // Newest first, and the ones that opened their block before the rest:
    // only those can be held to their receipt.
    out.reverse();
    out.sort_by_key(|f| f.index != 0);
    out
}

/// What one execution of the call came to.
#[derive(Debug, PartialEq, Eq)]
struct Ran {
    success: bool,
    /// Receipt gas. `None` where the run does not report it (our revert).
    gas: Option<u64>,
}

enum Replay {
    Skipped(&'static str),
    Ran {
        ours: Ran,
        node: Ran,
        receipt: Ran,
        reads: u64,
    },
}

fn replay(rpc: &Arc<Rpc>, f: &Found) -> Replay {
    let tx = rpc
        .call("eth_getTransactionByHash", json!([format!("{:#x}", f.tx)]))
        .unwrap();
    if tx["to"].is_null() {
        return Replay::Skipped("contract creation");
    }
    let kind = num(&tx["type"]).unwrap_or(0);
    if kind >= 3 {
        return Replay::Skipped("blob or set-code transaction");
    }
    let receipt = rpc
        .call("eth_getTransactionReceipt", json!([format!("{:#x}", f.tx)]))
        .unwrap();
    let header = rpc
        .call("eth_getBlockByNumber", json!([hex(f.block), false]))
        .unwrap();
    let ts = num(&header["timestamp"]).unwrap();
    let gas_limit = num(&header["gasLimit"]).unwrap();
    let miner: Address = parse(&header["miner"]).unwrap();
    let mix: B256 = parse(&header["mixHash"]).unwrap();
    let call = SimTx {
        caller: parse(&tx["from"]).unwrap(),
        to: parse(&tx["to"]).unwrap(),
        value: word(&tx["value"]).unwrap(),
        data: parse(&tx["input"]).unwrap(),
        gas_limit: num(&tx["gas"]).unwrap(),
        access_list: access_list(&tx["accessList"]),
    };
    let parent = hex(f.block - 1);

    let before = rpc.calls.load(Ordering::Relaxed);
    let state = BlockState::locked(AtBlock {
        rpc: Arc::clone(rpc),
        tag: parent.clone(),
    });
    let mut sim =
        Simulator::from_provider(Arc::new(state), Address::ZERO, Address::ZERO, Address::ZERO);
    let mut block = block_env_at(f.block, ts);
    block.gas_limit = gas_limit;
    block.beneficiary = miner;
    block.prevrandao = Some(mix);
    let env = SimEnv::new(mainnet_spec(ts).unwrap(), block);
    let bundle = Bundle {
        trigger: Trigger::InterestDrift,
        calls: vec![call.clone()],
        min_profit: U256::ZERO,
        health: None,
    };
    let ours = match verify(&mut sim, &bundle, &env) {
        Ok(o) => Ran {
            success: true,
            gas: Some(o.gas_used),
        },
        Err(SimError::Revert { .. }) => Ran {
            success: false,
            gas: None,
        },
        Err(e) => panic!("{}: simulator refused: {e}", f.tx),
    };
    let reads = rpc.calls.load(Ordering::Relaxed) - before;

    let node = rpc
        .call(
            "eth_simulateV1",
            json!([{
                "blockStateCalls": [{
                    "blockOverrides": {
                        "number": hex(f.block),
                        "time": hex(ts),
                        "gasLimit": hex(gas_limit),
                        "feeRecipient": format!("{miner:#x}"),
                        "prevRandao": format!("{mix:#x}"),
                        "baseFeePerGas": "0x0",
                    },
                    "calls": [{
                        "from": format!("{:#x}", call.caller),
                        "to": format!("{:#x}", call.to),
                        "value": format!("{:#x}", call.value),
                        "input": format!("{}", call.data),
                        "gas": hex(call.gas_limit),
                        "gasPrice": "0x0",
                        "accessList": if tx["accessList"].is_array() {
                            tx["accessList"].clone()
                        } else {
                            json!([])
                        },
                    }],
                }],
                "validation": false,
            }, parent]),
        )
        .unwrap();
    let n = &node[0]["calls"][0];
    let node = Ran {
        success: num(&n["status"]).unwrap() == 1,
        gas: Some(num(&n["gasUsed"]).unwrap()),
    };
    let receipt = Ran {
        success: num(&receipt["status"]).unwrap() == 1,
        gas: Some(num(&receipt["gasUsed"]).unwrap()),
    };
    Replay::Ran {
        ours,
        node,
        receipt,
        reads,
    }
}

/// Two runs agree: same outcome, and the same gas wherever both report it.
fn agree(a: &Ran, b: &Ran) -> bool {
    a.success == b.success
        && match (a.gas, b.gas) {
            (Some(x), Some(y)) => x == y,
            _ => true,
        }
}

#[test]
#[ignore = "needs MAINNET_RPC_URL (archive, or the bot's own node)"]
fn recent_liquidations_simulate_as_the_chain_ran_them() {
    let Some(url) = std::env::var("MAINNET_RPC_URL")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        eprintln!("MAINNET_RPC_URL unset — skipped");
        return;
    };
    let env_u64 = |k: &str, d: u64| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let span = env_u64("LIQ_REPLAY_BLOCKS", 600);
    let max = usize::try_from(env_u64("LIQ_REPLAY_MAX", 8)).unwrap();
    let rpc = Arc::new(Rpc {
        http: reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap(),
        url,
        calls: AtomicU64::new(0),
    });
    let head = num(&rpc.call("eth_blockNumber", json!([])).unwrap()).unwrap();
    let found = recent_liquidations(&rpc, head, span);
    eprintln!(
        "{} liquidation transactions in blocks {}..={head}",
        found.len(),
        head - span
    );
    let mut ran = 0usize;
    let mut first_in_block = 0usize;
    let mut bad = Vec::new();
    for f in found.iter().take(max) {
        match replay(&rpc, f) {
            Replay::Skipped(why) => eprintln!("{} {:>9} skipped: {why}", f.tx, f.block),
            Replay::Ran {
                ours,
                node,
                receipt,
                reads,
            } => {
                ran += 1;
                let vs_node = agree(&ours, &node);
                let vs_receipt = f.index != 0 || agree(&ours, &receipt);
                first_in_block += usize::from(f.index == 0);
                eprintln!(
                    "{} {:>9} #{:<3} {:<15} ours {:?} node {:?} receipt {:?} ({} reads){}",
                    f.tx,
                    f.block,
                    f.index,
                    f.protocol,
                    ours,
                    node,
                    receipt,
                    reads,
                    if vs_node && vs_receipt {
                        ""
                    } else {
                        "  <-- MISMATCH"
                    }
                );
                if !vs_node || !vs_receipt {
                    bad.push(f.tx);
                }
            }
        }
    }
    assert!(
        ran > 0,
        "no liquidation to replay in the last {span} blocks — raise LIQ_REPLAY_BLOCKS"
    );
    eprintln!("{ran} replayed, {first_in_block} opened their block");
    assert!(bad.is_empty(), "disagreements: {bad:?}");
}
