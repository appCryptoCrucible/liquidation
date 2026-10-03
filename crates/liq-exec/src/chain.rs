//! Chain reads for the governance path: head, nonce, and `eth_simulateV1`.
//!
//! Off the hot path. Against the co-located node, `eth_simulateV1` runs the
//! calls in order on the state after `base_block`, with the next block's
//! number and timestamp. That is the state the target block starts from,
//! and the payload's own logs come back exactly as the chain will emit them.

use crate::error::{ExecError, Result};
use alloy_primitives::{Address, Bytes, B256, U256};
use serde_json::{json, Value};

/// One call in a simulated block. `gas` of `None` lets the node cap it.
/// `gas_price` sets `tx.gasprice` (the Executor charges a governance call's
/// gas at it); `None` leaves it zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimCall {
    pub from: Address,
    pub to: Address,
    pub data: Bytes,
    pub gas: Option<u64>,
    pub gas_price: Option<u128>,
}

/// A log a simulated call emitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimLog {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Bytes,
}

/// One call's result. `logs` is empty when the call reverted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimResult {
    pub success: bool,
    pub gas_used: u64,
    pub return_data: Bytes,
    pub logs: Vec<SimLog>,
}

/// Head block number and timestamp.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub number: u64,
    pub timestamp: u64,
}

/// Plain JSON-RPC over the process's HTTP client.
#[derive(Clone, Debug)]
pub struct ChainClient {
    http: reqwest::Client,
    url: String,
}

impl ChainClient {
    pub fn new(url: &str) -> Result<Self> {
        if url.is_empty() {
            return Err(ExecError::Rpc("empty rpc url".into()));
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| ExecError::Http(e.to_string()))?;
        Ok(Self {
            http,
            url: url.to_owned(),
        })
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let resp = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ExecError::Http(e.to_string()))?;
        let v: Value = resp
            .json()
            .await
            .map_err(|e| ExecError::Serde(e.to_string()))?;
        if let Some(err) = v.get("error") {
            return Err(ExecError::Rpc(format!("{method}: {err}")));
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| ExecError::Rpc(format!("{method}: no result")))
    }

    /// `eth_getBlockByNumber("latest")`: number and timestamp.
    pub async fn head(&self) -> Result<Head> {
        let b = self
            .request("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        Ok(Head {
            number: quantity(b.get("number"), "number")?,
            timestamp: quantity(b.get("timestamp"), "timestamp")?,
        })
    }

    /// Timestamp of block `n`.
    pub async fn block_timestamp(&self, n: u64) -> Result<u64> {
        let b = self
            .request("eth_getBlockByNumber", json!([hex_q(n), false]))
            .await?;
        quantity(b.get("timestamp"), "timestamp")
    }

    /// Confirmed nonce of `addr` after `block`: the nonce its next
    /// transaction must carry to land in `block + 1`.
    pub async fn nonce_at(&self, addr: Address, block: u64) -> Result<u64> {
        let v = self
            .request("eth_getTransactionCount", json!([addr, hex_q(block)]))
            .await?;
        quantity(Some(&v), "nonce")
    }

    /// `eth_call` to `to` at `block`. A revert is an error.
    pub async fn call_at(&self, to: Address, data: &Bytes, block: u64) -> Result<Bytes> {
        let v = self
            .request("eth_call", json!([{"to": to, "input": data}, hex_q(block)]))
            .await?;
        bytes(Some(&v), "eth_call return")
    }

    /// Run `calls` in order in one simulated block on top of `base_block`,
    /// numbered `number` at `timestamp`. `code` places runtime code at each
    /// address first (an Executor and its modules that are not deployed yet).
    pub async fn simulate(
        &self,
        base_block: u64,
        number: u64,
        timestamp: u64,
        code: &[(Address, Bytes)],
        calls: &[SimCall],
    ) -> Result<Vec<SimResult>> {
        let calls_json: Vec<Value> = calls
            .iter()
            .map(|c| {
                let mut o = json!({"from": c.from, "to": c.to, "input": c.data});
                if let Some(m) = o.as_object_mut() {
                    if let Some(g) = c.gas {
                        m.insert("gas".into(), json!(hex_q(g)));
                    }
                    if let Some(p) = c.gas_price {
                        m.insert("gasPrice".into(), json!(format!("{p:#x}")));
                    }
                }
                o
            })
            .collect();
        let mut block = json!({
            "blockOverrides": {"number": hex_q(number), "time": hex_q(timestamp)},
            "calls": calls_json,
        });
        if let (false, Some(m)) = (code.is_empty(), block.as_object_mut()) {
            let mut over = serde_json::Map::new();
            for (addr, runtime) in code {
                over.insert(format!("{addr:#x}"), json!({"code": runtime}));
            }
            m.insert("stateOverrides".into(), Value::Object(over));
        }
        let v = self
            .request(
                "eth_simulateV1",
                json!([{"blockStateCalls": [block], "validation": false}, hex_q(base_block)]),
            )
            .await?;
        let results = v
            .get(0)
            .and_then(|b| b.get("calls"))
            .and_then(Value::as_array)
            .ok_or_else(|| ExecError::Rpc("eth_simulateV1: no calls".into()))?;
        if results.len() != calls.len() {
            return Err(ExecError::Rpc(format!(
                "eth_simulateV1 returned {} results for {} calls",
                results.len(),
                calls.len()
            )));
        }
        results.iter().map(sim_result).collect()
    }
}

fn sim_result(c: &Value) -> Result<SimResult> {
    let success = quantity(c.get("status"), "status")? == 1;
    let logs = match c.get("logs").and_then(Value::as_array) {
        Some(ls) => ls.iter().map(sim_log).collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };
    Ok(SimResult {
        success,
        gas_used: quantity(c.get("gasUsed"), "gasUsed")?,
        return_data: bytes(c.get("returnData"), "returnData").unwrap_or_default(),
        logs,
    })
}

fn sim_log(l: &Value) -> Result<SimLog> {
    let address = l
        .get("address")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<Address>().ok())
        .ok_or_else(|| ExecError::Rpc("log address".into()))?;
    let topics = l
        .get("topics")
        .and_then(Value::as_array)
        .ok_or_else(|| ExecError::Rpc("log topics".into()))?
        .iter()
        .map(|t| {
            t.as_str()
                .and_then(|s| s.parse::<B256>().ok())
                .ok_or_else(|| ExecError::Rpc("log topic".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(SimLog {
        address,
        topics,
        data: bytes(l.get("data"), "log data")?,
    })
}

fn hex_q(n: u64) -> String {
    format!("{n:#x}")
}

fn quantity(v: Option<&Value>, what: &str) -> Result<u64> {
    let s = v
        .and_then(Value::as_str)
        .ok_or_else(|| ExecError::Rpc(format!("{what} missing")))?;
    let u: U256 = s
        .parse()
        .map_err(|_| ExecError::Rpc(format!("{what} not a quantity: {s}")))?;
    u64::try_from(u).map_err(|_| ExecError::Rpc(format!("{what} exceeds u64")))
}

fn bytes(v: Option<&Value>, what: &str) -> Result<Bytes> {
    v.and_then(Value::as_str)
        .and_then(|s| s.parse::<Bytes>().ok())
        .ok_or_else(|| ExecError::Rpc(format!("{what} not hex bytes")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const CONTROLLER: Address = crate::executor::mainnet::AAVE_PAYLOADS_CONTROLLER;

    fn rpc() -> Option<String> {
        std::env::var("MAINNET_RPC_URL")
            .ok()
            .filter(|s| !s.is_empty())
    }

    #[test]
    fn quantity_and_bytes_parse_and_refuse() {
        assert_eq!(quantity(Some(&json!("0x1a")), "x").unwrap(), 26);
        assert!(quantity(Some(&json!("zz")), "x").is_err());
        assert!(quantity(None, "x").is_err());
        assert_eq!(
            bytes(Some(&json!("0x0102")), "b").unwrap().as_ref(),
            &[1, 2]
        );
        assert!(bytes(Some(&json!(5)), "b").is_err());
    }

    /// Payload 469 simulated on the state after block 26019517, as block
    /// 26019518 at its real timestamp, emits exactly the 12 logs the real
    /// execution (tx 0x4dfd6d9b…, block 26019518) emitted before the keeper's
    /// own two. Oracle: that receipt, fetched here at the same time.
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn payload_469_simulation_matches_the_real_receipt() {
        let Some(url) = rpc() else { return };
        let c = ChainClient::new(&url).unwrap();
        let data = crate::executor::execute_payload_calldata(469);
        assert_eq!(
            &data[..4],
            &[0x92, 0xcd, 0xb8, 0x34],
            "cast sig executePayload(uint40)"
        );
        let ts = c.block_timestamp(26_019_518).await.unwrap();
        assert_eq!(ts, 1_789_916_831);
        let out = c
            .simulate(
                26_019_517,
                26_019_518,
                ts,
                &[],
                &[SimCall {
                    from: address!("000000000000000000000000000000000000dEaD"),
                    to: CONTROLLER,
                    data,
                    gas: None,
                    gas_price: None,
                }],
            )
            .await
            .unwrap();
        let r = &out[0];
        assert!(r.success);
        assert_eq!(r.logs.len(), 12);
        let receipt = c
            .request(
                "eth_getTransactionReceipt",
                json!(["0x4dfd6d9b00dfb6f72bf0ca52d512303f2247b6c2d2dc4b6f05b412c3fc0547f2"]),
            )
            .await
            .unwrap();
        let real: Vec<SimLog> = receipt["logs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| sim_log(l).unwrap())
            .collect();
        assert_eq!(&real[..12], &r.logs[..], "byte-for-byte");
        assert_eq!(
            real[11].address, CONTROLLER,
            "PayloadExecuted is the last own log"
        );
    }
}
