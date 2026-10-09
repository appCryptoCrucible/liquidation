//! A local JSON-RPC endpoint over the pre-state. The bot's own readers
//! (pool seeding, oracle seeding, protocol prices, state reads) point here
//! and see the chain at that moment, without knowing it is history. Older
//! blocks and transactions are forwarded upstream; a method nothing here
//! answers is recorded and refused.

use super::rpc::{hex, Upstream};
use super::state::{call, AtBlock, Header};
use alloy_primitives::{Address, Bytes, B256, U256};
use revm::database::CacheDB;
use revm::database_interface::DatabaseRef;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub struct Served {
    pub up: Arc<Upstream>,
    /// The pre-state, frozen.
    pub db: Arc<CacheDB<AtBlock>>,
    /// The bot's tip: block N−1.
    pub parent: Header,
    /// What the pre-state changed over block N−1 (`PreState::overrides`).
    /// `eth_call` is answered by the upstream node at block N−1 with these
    /// as state overrides: the same answer as running it here, in one
    /// request instead of one per storage slot it reads.
    pub overrides: Option<Value>,
    pub unknown: Mutex<BTreeSet<String>>,
    pub requests: AtomicU64,
}

pub struct Server {
    pub url: String,
    pub served: Arc<Served>,
}

impl Server {
    pub fn start(served: Served) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}/", listener.local_addr()?);
        let served = Arc::new(served);
        let s = Arc::clone(&served);
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let s = Arc::clone(&s);
                std::thread::spawn(move || {
                    let _ = serve(conn, &s);
                });
            }
        });
        Ok(Self { url, served })
    }

    pub fn unknown_methods(&self) -> Vec<String> {
        self.served
            .unknown
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect()
    }
}

/// HTTP/1.1 with keep-alive: one JSON-RPC request (or batch) per message.
fn serve(mut conn: TcpStream, s: &Served) -> std::io::Result<()> {
    let mut reader = BufReader::new(conn.try_clone()?);
    loop {
        let mut length = 0usize;
        let mut started = false;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let l = line.trim_end();
            if l.is_empty() {
                if started {
                    break;
                }
                continue;
            }
            started = true;
            if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                length = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;
        let reply = match serde_json::from_slice::<Value>(&body) {
            Ok(Value::Array(reqs)) => Value::Array(reqs.iter().map(|r| answer(s, r)).collect()),
            Ok(req) => answer(s, &req),
            Err(_) => {
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}})
            }
        };
        let out = reply.to_string();
        write!(
            conn,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            out.len(),
            out
        )?;
        conn.flush()?;
    }
}

fn answer(s: &Served, req: &Value) -> Value {
    s.requests.fetch_add(1, Ordering::Relaxed);
    let id = req["id"].clone();
    let method = req["method"].as_str().unwrap_or("");
    match handle(s, method, &req["params"]) {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
    }
}

fn rpc_err(msg: impl std::fmt::Display) -> Value {
    json!({"code": -32000, "message": msg.to_string()})
}

/// A block tag that means the pre-state: `latest`/`pending`/`safe`/
/// `finalized`, or any number from the bot's tip on.
fn current(s: &Served, tag: &Value) -> bool {
    let n = match tag {
        Value::Null => return true,
        Value::String(t) if !t.starts_with("0x") => return true,
        Value::String(t) => t.clone(),
        Value::Object(o) => match o.get("blockNumber").and_then(Value::as_str) {
            Some(t) => t.to_string(),
            None => return true,
        },
        _ => return true,
    };
    u64::from_str_radix(n.trim_start_matches("0x"), 16).map_or(true, |n| n >= s.parent.number)
}

fn addr(v: &Value) -> Result<Address, Value> {
    v.as_str()
        .and_then(|a| a.parse().ok())
        .ok_or_else(|| rpc_err("bad address"))
}

fn forward(s: &Served, method: &str, params: &Value) -> Result<Value, Value> {
    s.up.call(method, params.clone()).map_err(rpc_err)
}

fn handle(s: &Served, method: &str, p: &Value) -> Result<Value, Value> {
    let db = &*s.db;
    match method {
        "eth_chainId" => Ok(json!("0x1")),
        "net_version" => Ok(json!("1")),
        "web3_clientVersion" => Ok(json!("historical-prestate/1")),
        "eth_syncing" => Ok(json!(false)),
        "eth_blockNumber" => Ok(json!(hex(s.parent.number))),
        "eth_gasPrice" => Ok(json!(hex(s.parent.base_fee + 1_000_000_000))),
        "eth_maxPriorityFeePerGas" => Ok(json!(hex(1_000_000_000))),
        "eth_getBlockByNumber" if current(s, &p[0]) => {
            forward(s, method, &json!([hex(s.parent.number), p[1].clone()]))
        }
        "eth_feeHistory" => forward(
            s,
            method,
            &json!([p[0].clone(), hex(s.parent.number), p[2].clone()]),
        ),
        "eth_getBlockByNumber"
        | "eth_getBlockByHash"
        | "eth_getTransactionByHash"
        | "eth_getTransactionReceipt"
        | "eth_getBlockReceipts" => forward(s, method, p),
        "eth_getBalance" | "eth_getTransactionCount" | "eth_getCode" if current(s, &p[1]) => {
            let info = db
                .basic_ref(addr(&p[0])?)
                .map_err(rpc_err)?
                .unwrap_or_default();
            Ok(match method {
                "eth_getBalance" => json!(format!("{:#x}", info.balance)),
                "eth_getTransactionCount" => json!(hex(info.nonce)),
                _ => json!(info.code.map(|c| c.original_bytes()).unwrap_or_default()),
            })
        }
        "eth_getStorageAt" if current(s, &p[2]) => {
            let slot: U256 = p[1]
                .as_str()
                .and_then(|t| U256::from_str_radix(t.trim_start_matches("0x"), 16).ok())
                .ok_or_else(|| rpc_err("bad slot"))?;
            let v = db.storage_ref(addr(&p[0])?, slot).map_err(rpc_err)?;
            Ok(json!(format!("{:#x}", B256::from(v))))
        }
        "eth_getBalance" | "eth_getTransactionCount" | "eth_getCode" | "eth_getStorageAt" => {
            forward(s, method, p)
        }
        "eth_call" if current(s, &p[1]) => {
            let mut call = p[0].clone();
            // A node refuses a call above its own gas cap; the local run
            // used 50M when none was given, which is also the free
            // endpoint's cap.
            if call["gas"].is_null() {
                call["gas"] = json!(hex(50_000_000));
            }
            let mut params = vec![call, json!(hex(s.parent.number))];
            if let Some(o) = &s.overrides {
                params.push(o.clone());
            }
            match s.up.call_keeping_errors("eth_call", Value::Array(params)) {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(rpc_err(e)),
            }
        }
        "eth_estimateGas" if current(s, &p[1]) => {
            let c = &p[0];
            let from = if c["from"].is_null() {
                Address::ZERO
            } else {
                addr(&c["from"])?
            };
            let to = addr(&c["to"])?;
            let data: Bytes = c["input"]
                .as_str()
                .or_else(|| c["data"].as_str())
                .map_or(Ok(Bytes::new()), str::parse)
                .map_err(|_| rpc_err("bad input"))?;
            let gas = c["gas"]
                .as_str()
                .and_then(|g| u64::from_str_radix(g.trim_start_matches("0x"), 16).ok())
                .unwrap_or(50_000_000);
            let ran = call(db, &s.parent, from, to, data, gas).map_err(rpc_err)?;
            if !ran.success {
                return Err(
                    json!({"code": 3, "message": "execution reverted", "data": ran.output}),
                );
            }
            Ok(json!(hex(ran.gas_used + ran.gas_used / 5)))
        }
        "eth_call" | "eth_estimateGas" => forward(s, method, p),
        "eth_getLogs" => {
            // Nothing after the bot's tip has happened yet.
            let mut f = p[0].clone();
            let cap = hex(s.parent.number);
            let past = |t: &Value| !current(s, t) || t.as_str() == Some(cap.as_str());
            if !past(&f["toBlock"]) {
                f["toBlock"] = json!(cap);
            }
            forward(s, method, &json!([f]))
        }
        other => {
            s.unknown.lock().unwrap().insert(other.to_string());
            Err(
                json!({"code": -32601, "message": format!("{other} is not served by the pre-state")}),
            )
        }
    }
}
