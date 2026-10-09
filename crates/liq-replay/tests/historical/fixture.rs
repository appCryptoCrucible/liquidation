//! The liquidations the test replays: real events, found by scanning the
//! free endpoint (`liquidations.json` records the range and method).

use alloy_primitives::{Address, Bytes, B256};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Event {
    pub family: String,
    pub block: u64,
    pub tx: B256,
    pub tx_index: u64,
    pub log_index: u64,
    pub emitter: Address,
    pub topics: Vec<B256>,
    pub data: Bytes,
}

/// The committed fixture, or the file `LIQ_HISTORY_EVENTS` names (same
/// shape: `{"liquidations": [...]}`), e.g. events drawn from the scan.
pub fn load() -> Vec<Event> {
    let raw = match std::env::var("LIQ_HISTORY_EVENTS") {
        Ok(path) => std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")),
        Err(_) => include_str!("liquidations.json").to_string(),
    };
    let v: Value = serde_json::from_str(&raw).unwrap();
    v["liquidations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| Event {
            family: e["family"].as_str().unwrap().to_string(),
            block: e["block"].as_u64().unwrap(),
            tx: e["tx"].as_str().unwrap().parse().unwrap(),
            tx_index: e["tx_index"].as_u64().unwrap(),
            log_index: e["log_index"].as_u64().unwrap(),
            emitter: e["emitter"].as_str().unwrap().parse().unwrap(),
            topics: e["topics"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_str().unwrap().parse().unwrap())
                .collect(),
            data: e["data"].as_str().unwrap().parse().unwrap(),
        })
        .collect()
}
