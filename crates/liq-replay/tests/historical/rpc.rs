//! The upstream node: blocking JSON-RPC that retries throttling and keeps
//! every answer pinned to a block (or a transaction hash) on disk, so a
//! rerun reads nothing twice from the free endpoint.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub struct Upstream {
    http: reqwest::blocking::Client,
    url: String,
    mem: Mutex<HashMap<String, Value>>,
    disk: Mutex<Option<File>>,
    /// Requests that reached the network.
    pub fetched: AtomicU64,
}

impl Upstream {
    /// `cache` is a JSON-lines file of `{"k": key, "v": answer}`; missing is
    /// fine, an unreadable line is skipped.
    pub fn new(url: String, cache: &Path) -> Self {
        let mut mem = HashMap::new();
        if let Ok(f) = File::open(cache) {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                if let Ok(Value::Object(mut o)) = serde_json::from_str::<Value>(&line) {
                    if let (Some(Value::String(k)), Some(v)) = (o.remove("k"), o.remove("v")) {
                        mem.insert(k, v);
                    }
                }
            }
        }
        if let Some(dir) = cache.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let disk = OpenOptions::new()
            .create(true)
            .append(true)
            .open(cache)
            .ok();
        Self {
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap(),
            url,
            mem: Mutex::new(mem),
            disk: Mutex::new(disk),
            fetched: AtomicU64::new(0),
        }
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let key = format!("{method} {params}");
        let pinned = pinned(method, &params);
        if pinned {
            if let Some(v) = self.mem.lock().unwrap().get(&key) {
                return Ok(v.clone());
            }
        }
        let v = self.fetch(method, &params)?;
        if pinned {
            self.mem.lock().unwrap().insert(key.clone(), v.clone());
            if let Some(f) = self.disk.lock().unwrap().as_mut() {
                let _ = writeln!(f, "{}", json!({"k": key, "v": v}));
            }
        }
        Ok(v)
    }

    /// One call whose error is part of the answer: `Ok(Err(error object))`
    /// for a revert or any other non-transient error, kept as the node
    /// returned it (an `eth_call` revert's data is what the caller decodes).
    /// Successes are cached as [`Self::call`] caches them.
    pub fn call_keeping_errors(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Result<Value, Value>, String> {
        let key = format!("{method} {params}");
        let pinned = pinned(method, &params);
        if pinned {
            if let Some(v) = self.mem.lock().unwrap().get(&key) {
                return Ok(Ok(v.clone()));
            }
        }
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut wait = 200u64;
        let mut last = String::new();
        for _ in 0..14 {
            self.fetched.fetch_add(1, Ordering::Relaxed);
            let reply = self
                .http
                .post(&self.url)
                .json(&body)
                .send()
                .and_then(reqwest::blocking::Response::json::<Value>);
            match reply {
                Ok(v) => match v.get("error") {
                    None => {
                        let r = v["result"].clone();
                        if pinned {
                            self.mem.lock().unwrap().insert(key.clone(), r.clone());
                            if let Some(f) = self.disk.lock().unwrap().as_mut() {
                                let _ = writeln!(f, "{}", json!({"k": key, "v": r}));
                            }
                        }
                        return Ok(Ok(r));
                    }
                    Some(e) => {
                        if !transient(&e.to_string()) {
                            return Ok(Err(e.clone()));
                        }
                        last = e.to_string();
                    }
                },
                Err(e) => last = e.without_url().to_string(),
            }
            std::thread::sleep(Duration::from_millis(wait));
            wait = (wait * 2).min(5_000);
        }
        Err(format!("{method}: retries exhausted ({last})"))
    }

    /// Several calls in one HTTP request (a JSON-RPC batch); answers in
    /// order. Cached answers are not asked again.
    pub fn call_many(&self, calls: &[(&str, Value)]) -> Result<Vec<Value>, String> {
        let keys: Vec<String> = calls.iter().map(|(m, p)| format!("{m} {p}")).collect();
        let mut out: Vec<Option<Value>> = {
            let mem = self.mem.lock().unwrap();
            keys.iter().map(|k| mem.get(k).cloned()).collect()
        };
        let missing: Vec<usize> = (0..calls.len()).filter(|&i| out[i].is_none()).collect();
        if !missing.is_empty() {
            let batch: Vec<Value> = missing
                .iter()
                .map(|&i| json!({"jsonrpc": "2.0", "id": i, "method": calls[i].0, "params": calls[i].1}))
                .collect();
            let replies = self.fetch_batch(&Value::Array(batch))?;
            for r in replies {
                let i = r["id"].as_u64().ok_or("batch reply without id")? as usize;
                if let Some(e) = r.get("error") {
                    return Err(format!("{}: {e}", calls[i].0));
                }
                let v = r["result"].clone();
                if pinned(calls[i].0, &calls[i].1) {
                    self.mem.lock().unwrap().insert(keys[i].clone(), v.clone());
                    if let Some(f) = self.disk.lock().unwrap().as_mut() {
                        let _ = writeln!(f, "{}", json!({"k": keys[i], "v": v}));
                    }
                }
                out[i] = Some(v);
            }
        }
        out.into_iter()
            .map(|v| v.ok_or_else(|| "batch reply missing an answer".to_string()))
            .collect()
    }

    fn fetch_batch(&self, body: &Value) -> Result<Vec<Value>, String> {
        let mut wait = 200u64;
        let mut last = String::new();
        for _ in 0..14 {
            self.fetched.fetch_add(1, Ordering::Relaxed);
            match self
                .http
                .post(&self.url)
                .json(body)
                .send()
                .and_then(reqwest::blocking::Response::json::<Value>)
            {
                Ok(Value::Array(replies)) => {
                    let throttled = replies
                        .iter()
                        .any(|r| r.get("error").is_some_and(|e| transient(&e.to_string())));
                    if !throttled {
                        return Ok(replies);
                    }
                    last = "throttled".into();
                }
                Ok(other) => last = other.to_string(),
                Err(e) => last = e.without_url().to_string(),
            }
            std::thread::sleep(Duration::from_millis(wait));
            wait = (wait * 2).min(5_000);
        }
        Err(format!("batch: retries exhausted ({last})"))
    }

    /// One request, retried while the node throttles or the transport
    /// fails; any other error is the answer.
    fn fetch(&self, method: &str, params: &Value) -> Result<Value, String> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut wait = 200u64;
        let mut last = String::new();
        for _ in 0..14 {
            self.fetched.fetch_add(1, Ordering::Relaxed);
            let reply = self
                .http
                .post(&self.url)
                .json(&body)
                .send()
                .and_then(reqwest::blocking::Response::json::<Value>);
            match reply {
                Ok(v) => match v.get("error") {
                    None => return Ok(v["result"].clone()),
                    Some(e) => {
                        let msg = e.to_string();
                        if !transient(&msg) {
                            return Err(format!("{method}: {msg}"));
                        }
                        last = msg;
                    }
                },
                Err(e) => last = e.without_url().to_string(),
            }
            std::thread::sleep(Duration::from_millis(wait));
            wait = (wait * 2).min(5_000);
        }
        Err(format!("{method}: retries exhausted ({last})"))
    }
}

/// An error the free endpoint gives for a request it would answer if asked
/// again: throttling, or its `-32000 "Internal error"` on an account read.
/// A revert or a bad request is the answer, not retried.
fn transient(msg: &str) -> bool {
    msg.contains("429")
        || msg.contains("exceeded")
        || msg.contains("rate limit")
        || msg.contains("compute units")
        || msg.contains("-32001")
        || msg.contains("Unable to complete request")
        || msg.contains("Internal error")
}

/// Answers that cannot change: state at a numbered block, a block by
/// number, a transaction or receipt by hash.
fn pinned(method: &str, params: &Value) -> bool {
    let numbered = |v: &Value| v.as_str().is_some_and(|t| t.starts_with("0x"));
    match method {
        "eth_getTransactionByHash" | "eth_getTransactionReceipt" | "eth_getBlockByHash" => true,
        "eth_getBlockByNumber" | "eth_getBlockReceipts" => numbered(&params[0]),
        "eth_getBalance" | "eth_getCode" | "eth_getTransactionCount" => numbered(&params[1]),
        "eth_getStorageAt" => numbered(&params[2]),
        "eth_call" => numbered(&params[1]),
        "eth_getLogs" => numbered(&params[0]["fromBlock"]) && numbered(&params[0]["toBlock"]),
        _ => false,
    }
}

pub fn hex(n: u64) -> String {
    format!("{n:#x}")
}

pub fn num(v: &Value) -> Result<u64, String> {
    let s = v.as_str().ok_or_else(|| format!("not a quantity: {v}"))?;
    u64::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| e.to_string())
}

pub fn big(v: &Value) -> Result<alloy_primitives::U256, String> {
    let s = v.as_str().ok_or_else(|| format!("not a quantity: {v}"))?;
    alloy_primitives::U256::from_str_radix(s.trim_start_matches("0x"), 16)
        .map_err(|e| e.to_string())
}

pub fn parse<T: std::str::FromStr>(v: &Value) -> Result<T, String>
where
    T::Err: std::fmt::Display,
{
    let s = v.as_str().ok_or_else(|| format!("not a string: {v}"))?;
    s.parse().map_err(|e: T::Err| e.to_string())
}
