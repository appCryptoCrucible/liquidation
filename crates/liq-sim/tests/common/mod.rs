//! Chain state over JSON-RPC for the live simulator tests: a throttle-aware
//! client, the quantity parsers, and [`AtBlock`], the state after a block
//! read lazily the way the production simulator reads it from Reth.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, B256, U256};
use revm::database_interface::{DBErrorMarker, DatabaseRef};
use revm::state::{AccountInfo, Bytecode};
use serde_json::{json, Value};

pub(crate) struct Rpc {
    pub(crate) http: reqwest::blocking::Client,
    pub(crate) url: String,
    pub(crate) calls: AtomicU64,
}

impl Rpc {
    /// One JSON-RPC call. Throttling is retried; any other error is the
    /// answer.
    pub(crate) fn call(&self, method: &str, params: Value) -> Result<Value, String> {
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

pub(crate) fn hex(n: u64) -> String {
    format!("{n:#x}")
}

pub(crate) fn num(v: &Value) -> Result<u64, String> {
    let s = v.as_str().ok_or_else(|| format!("not a quantity: {v}"))?;
    u64::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| e.to_string())
}

pub(crate) fn word(v: &Value) -> Result<U256, String> {
    let s = v.as_str().ok_or_else(|| format!("not a quantity: {v}"))?;
    U256::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| e.to_string())
}

pub(crate) fn parse<T: std::str::FromStr>(v: &Value) -> Result<T, String>
where
    T::Err: std::fmt::Display,
{
    let s = v.as_str().ok_or_else(|| format!("not a string: {v}"))?;
    s.parse().map_err(|e: T::Err| e.to_string())
}

/// A transaction's `accessList` field; absent on a legacy transaction.
pub(crate) fn access_list(v: &Value) -> Vec<(Address, Vec<B256>)> {
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
pub(crate) struct ReadFailed(pub(crate) String);

impl std::fmt::Display for ReadFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ReadFailed {}

impl DBErrorMarker for ReadFailed {}

/// Chain state after block `tag`, read lazily over RPC.
pub(crate) struct AtBlock {
    pub(crate) rpc: Arc<Rpc>,
    pub(crate) tag: String,
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
