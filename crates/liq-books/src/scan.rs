//! One finalized scan. A transport failure leaves the cursor where it was.
//! A receipt that is missing `gross_bonus_wei`, `flash_fee_wei`,
//! `swap_cost_wei`, `coinbase_bid_wei`, `net_retained_wei`, or either FX
//! source is logged and not written.

use crate::{
    append_row, bundles_path, decode_latest_round, ecb_value_for_date, is_final, iso_date, iso_utc,
    last_row_hash, missing_bundle_fields, tx_already_booked, write_sidecar, BundleDraft,
    BUNDLES_HEADER, ETH_USD_FEED, EUR_USD_FEED,
};
use alloy_primitives::{keccak256, Address, U256};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

const PAGE: u64 = 2_000;
const LATEST_ROUND: &str = "0xfeaf968c";
const ECB_URL: &str = "https://data-api.ecb.europa.eu/service/data/EXR/D.USD.EUR.SP00.A";

/// Mainnet WETH9. Same constant as `liq-sim`.
const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";

pub struct ScanOutput {
    pub finalized: u64,
    pub appended: u64,
    pub skipped: Vec<Skipped>,
}

pub struct Skipped {
    pub tx_hash: String,
    pub missing: Vec<&'static str>,
}

struct Candidate {
    tx_hash: String,
    block: u64,
}

#[derive(Clone)]
struct Header {
    hash: String,
    timestamp: u64,
    beneficiary: String,
}

struct FxCache {
    rounds: BTreeMap<(u64, String), crate::ChainlinkRound>,
    ecb: BTreeMap<String, String>,
}

/// `executor` and `rpc_url` are required by the binary before this is called.
/// `profit_sink` unset means `net_retained_wei` stays missing and the row is skipped.
pub async fn scan_once(
    client: &reqwest::Client,
    rpc_url: &str,
    books_dir: &Path,
    executor: Address,
    profit_sink: Option<Address>,
) -> Result<ScanOutput, String> {
    let head = finalized_header(client, rpc_url).await?;
    let number = head.number;
    if number == 0 {
        return Err("finalized head is zero — scan refused".into());
    }
    let cursor_path = books_dir.join("cursor");
    let from = match read_cursor(&cursor_path)? {
        Some(n) => n,
        None => {
            tracing::error!(
                finalized = number,
                "books cursor absent — starting at the current finalized head, not backfilling"
            );
            number
        }
    };
    if from > number {
        return Ok(ScanOutput {
            finalized: number,
            appended: 0,
            skipped: Vec::new(),
        });
    }
    let logs = logs_from_executor(client, rpc_url, executor, from, number).await?;
    let mut skipped = Vec::new();
    let mut appended = 0u64;
    let mut blocks: BTreeMap<u64, Header> = BTreeMap::new();
    let mut fx = FxCache {
        rounds: BTreeMap::new(),
        ecb: BTreeMap::new(),
    };
    for cand in logs {
        if !is_final(cand.block, number) {
            return Err(format!(
                "log block {} is above finalized {number}",
                cand.block
            ));
        }
        let header = if let Some(h) = blocks.get(&cand.block) {
            h.clone()
        } else {
            let h = block_header(client, rpc_url, cand.block).await?;
            blocks.insert(cand.block, h.clone());
            h
        };
        let date = iso_date(header.timestamp)
            .ok_or_else(|| format!("block {} timestamp has no UTC date", cand.block))?;
        let (y, m, _, _, _, _) = crate::utc_parts(header.timestamp)
            .ok_or_else(|| format!("block {} timestamp has no civil date", cand.block))?;
        let path = bundles_path(books_dir, y, m);
        if tx_already_booked(&path, &cand.tx_hash).map_err(|e| e.to_string())? {
            continue;
        }
        let draft = draft_for(client, rpc_url, &cand, &header, profit_sink, &date, &mut fx).await?;
        let missing = missing_bundle_fields(&draft);
        if !missing.is_empty() {
            tracing::error!(tx = %cand.tx_hash, ?missing, "books row skipped — component unattested");
            skipped.push(Skipped {
                tx_hash: cand.tx_hash,
                missing,
            });
            continue;
        }
        let prev = last_row_hash(&path)
            .map_err(|e| e.to_string())?
            .unwrap_or([0u8; 32]);
        let Some(row) = draft.finish(&prev) else {
            tracing::error!(tx = %cand.tx_hash, "books row skipped — finish refused");
            skipped.push(Skipped {
                tx_hash: cand.tx_hash,
                missing: vec!["finish"],
            });
            continue;
        };
        append_row(&path, BUNDLES_HEADER, &row.line).map_err(|e| e.to_string())?;
        write_sidecar(&path).map_err(|e| e.to_string())?;
        appended = appended.checked_add(1).ok_or("append count overflow")?;
    }
    let next = number.checked_add(1).ok_or("finalized cursor overflow")?;
    write_cursor(&cursor_path, next)?;
    Ok(ScanOutput {
        finalized: number,
        appended,
        skipped,
    })
}

async fn draft_for(
    client: &reqwest::Client,
    rpc_url: &str,
    cand: &Candidate,
    header: &Header,
    profit_sink: Option<Address>,
    date: &str,
    fx: &mut FxCache,
) -> Result<BundleDraft, String> {
    let receipt = rpc(
        client,
        rpc_url,
        "eth_getTransactionReceipt",
        json!([cand.tx_hash]),
    )
    .await?;
    if receipt.is_null() {
        return Err(format!("receipt missing for {}", cand.tx_hash));
    }
    let gas_used = receipt
        .get("gasUsed")
        .and_then(|v| v.as_str())
        .and_then(crate::parse_hex_u64);
    let gas_price = receipt
        .get("effectiveGasPrice")
        .and_then(|v| v.as_str())
        .and_then(crate::parse_hex_u128);
    let gas_cost = match (gas_used, gas_price) {
        (Some(g), Some(p)) => u128::from(g).checked_mul(p),
        _ => None,
    };
    let coinbase = coinbase_wei(client, rpc_url, &cand.tx_hash, &header.beneficiary).await?;
    let net = match profit_sink {
        Some(sink) => weth_to(receipt.get("logs"), sink)?,
        None => {
            tracing::error!(tx = %cand.tx_hash, "PROFIT_SINK unset — net_retained_wei withheld");
            None
        }
    };
    let eth = chainlink_at(client, rpc_url, cand.block, ETH_USD_FEED, &mut fx.rounds).await?;
    let eur = chainlink_at(client, rpc_url, cand.block, EUR_USD_FEED, &mut fx.rounds).await?;
    let ecb_rate = ecb_for(client, date, &mut fx.ecb).await?;
    let utc = iso_utc(header.timestamp);
    Ok(BundleDraft {
        finalized_at_utc: utc,
        block_number: Some(cand.block),
        block_hash: Some(header.hash.clone()),
        tx_hash: Some(cand.tx_hash.clone()),
        liquidation_count: None,
        gross_bonus_wei: None,
        flash_fee_wei: None,
        swap_cost_wei: None,
        gas_used,
        gas_price_wei: gas_price,
        gas_cost_wei: gas_cost,
        coinbase_bid_wei: coinbase,
        net_retained_wei: net,
        eth_usd_price: eth.as_ref().map(|r| r.answer),
        eth_usd_round_id: eth.as_ref().map(|r| r.round_id),
        eth_usd_updated_at: eth.as_ref().map(|r| r.updated_at),
        eur_usd_price: eur.as_ref().map(|r| r.answer),
        eur_usd_round_id: eur.as_ref().map(|r| r.round_id),
        eur_usd_updated_at: eur.as_ref().map(|r| r.updated_at),
        ecb_rate_date: ecb_rate.as_ref().map(|_| date.to_string()),
        ecb_eur_usd: ecb_rate,
        liquidation_utc_date: iso_date(header.timestamp),
    })
}

async fn chainlink_at(
    client: &reqwest::Client,
    rpc_url: &str,
    block: u64,
    feed: &str,
    cache: &mut BTreeMap<(u64, String), crate::ChainlinkRound>,
) -> Result<Option<crate::ChainlinkRound>, String> {
    let key = (block, feed.to_string());
    if let Some(r) = cache.get(&key) {
        return Ok(Some(r.clone()));
    }
    let tag = format!("0x{block:x}");
    let raw = rpc(
        client,
        rpc_url,
        "eth_call",
        json!([{ "to": feed, "data": LATEST_ROUND }, tag]),
    )
    .await?;
    let hex = raw.as_str().ok_or("eth_call result is not a string")?;
    let bytes = decode_hex_bytes(hex)?;
    match decode_latest_round(&bytes) {
        Some(r) => {
            cache.insert(key, r.clone());
            Ok(Some(r))
        }
        None => {
            tracing::error!(block, feed, "chainlink round unreadable or non-positive");
            Ok(None)
        }
    }
}

async fn ecb_for(
    client: &reqwest::Client,
    date: &str,
    cache: &mut BTreeMap<String, String>,
) -> Result<Option<String>, String> {
    if let Some(v) = cache.get(date) {
        return Ok(Some(v.clone()));
    }
    let url = format!("{ECB_URL}?startPeriod={date}&endPeriod={date}&format=csvdata");
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("ECB HTTP {}", resp.status()));
    }
    let text = resp.text().await.map_err(|e| e.to_string())?;
    match ecb_value_for_date(&text, date) {
        Some(v) => {
            cache.insert(date.to_string(), v.clone());
            Ok(Some(v))
        }
        None => {
            tracing::error!(date, "ECB has no rate for the liquidation UTC date");
            Ok(None)
        }
    }
}

async fn coinbase_wei(
    client: &reqwest::Client,
    rpc_url: &str,
    tx: &str,
    beneficiary: &str,
) -> Result<Option<u128>, String> {
    let traced = rpc(
        client,
        rpc_url,
        "debug_traceTransaction",
        json!([tx, { "tracer": "callTracer" }]),
    )
    .await
    .map_err(|e| format!("coinbase trace unavailable for {tx}: {e}"))?;
    let mut sum = U256::ZERO;
    add_value_to(&traced, beneficiary, &mut sum)?;
    u128::try_from(sum)
        .map(Some)
        .map_err(|_| "coinbase sum exceeds u128".into())
}

fn add_value_to(node: &Value, beneficiary: &str, sum: &mut U256) -> Result<(), String> {
    if let Some(to) = node.get("to").and_then(|v| v.as_str()) {
        if to.eq_ignore_ascii_case(beneficiary) {
            let raw = node
                .get("value")
                .and_then(|v| v.as_str())
                .ok_or("trace call to coinbase is missing value")?;
            let wei = U256::from_str_radix(raw.strip_prefix("0x").unwrap_or(raw), 16)
                .map_err(|_| format!("trace value {raw}"))?;
            *sum = sum.checked_add(wei).ok_or("coinbase sum overflow")?;
        }
    }
    if let Some(calls) = node.get("calls").and_then(|v| v.as_array()) {
        for c in calls {
            add_value_to(c, beneficiary, sum)?;
        }
    }
    Ok(())
}

fn weth_to(logs: Option<&Value>, sink: Address) -> Result<Option<u128>, String> {
    let Some(logs) = logs.and_then(|v| v.as_array()) else {
        return Ok(None);
    };
    let topic0 = transfer_topic();
    let mut sum = U256::ZERO;
    let mut any = false;
    for log in logs {
        let address = log.get("address").and_then(|v| v.as_str()).unwrap_or("");
        if !address.eq_ignore_ascii_case(WETH) {
            continue;
        }
        let topics = log.get("topics").and_then(|v| v.as_array());
        let Some(topics) = topics else {
            continue;
        };
        let t0 = topics.first().and_then(|v| v.as_str()).unwrap_or("");
        if !t0.eq_ignore_ascii_case(&topic0) {
            continue;
        }
        let to = topics.get(2).and_then(|v| v.as_str()).unwrap_or("");
        if !topic_is_address(to, sink) {
            continue;
        }
        let data = log.get("data").and_then(|v| v.as_str()).unwrap_or("0x");
        let wei = U256::from_str_radix(data.strip_prefix("0x").unwrap_or(data), 16)
            .map_err(|_| format!("WETH transfer data {data}"))?;
        sum = sum.checked_add(wei).ok_or("WETH sum overflow")?;
        any = true;
    }
    if !any {
        tracing::error!("no WETH transfer to profit sink — net_retained_wei withheld");
        return Ok(None);
    }
    if sum.bit(255) {
        return Err("WETH sum does not fit a positive signed value".into());
    }
    u128::try_from(sum)
        .map(Some)
        .map_err(|_| "WETH sum exceeds u128".into())
}

struct Finalized {
    number: u64,
}

async fn finalized_header(client: &reqwest::Client, rpc_url: &str) -> Result<Finalized, String> {
    let v = rpc(
        client,
        rpc_url,
        "eth_getBlockByNumber",
        json!(["finalized", false]),
    )
    .await?;
    if v.is_null() {
        return Err("node returned null for finalized — scan refused".into());
    }
    let number = v
        .get("number")
        .and_then(|n| n.as_str())
        .and_then(crate::parse_hex_u64)
        .ok_or("finalized block has no number")?;
    Ok(Finalized { number })
}

async fn block_header(
    client: &reqwest::Client,
    rpc_url: &str,
    block: u64,
) -> Result<Header, String> {
    let tag = format!("0x{block:x}");
    let v = rpc(client, rpc_url, "eth_getBlockByNumber", json!([tag, false])).await?;
    if v.is_null() {
        return Err(format!("block {block} missing"));
    }
    let hash = v
        .get("hash")
        .and_then(|h| h.as_str())
        .ok_or("block hash missing")?
        .to_string();
    let timestamp = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(crate::parse_hex_u64)
        .ok_or("block timestamp missing")?;
    let beneficiary = v
        .get("miner")
        .and_then(|m| m.as_str())
        .ok_or("block miner missing")?
        .to_string();
    Ok(Header {
        hash,
        timestamp,
        beneficiary,
    })
}

async fn logs_from_executor(
    client: &reqwest::Client,
    rpc_url: &str,
    executor: Address,
    from: u64,
    to: u64,
) -> Result<Vec<Candidate>, String> {
    let mut out = BTreeMap::<String, u64>::new();
    let mut cursor = from;
    let topic0 = transfer_topic();
    let topic1 = address_topic(executor);
    while cursor <= to {
        let last = cursor
            .checked_add(PAGE.saturating_sub(1))
            .unwrap_or(to)
            .min(to);
        let filter = json!({
            "fromBlock": format!("0x{cursor:x}"),
            "toBlock": format!("0x{last:x}"),
            "address": WETH,
            "topics": [topic0, topic1],
        });
        let logs = rpc(client, rpc_url, "eth_getLogs", json!([filter])).await?;
        let rows = logs
            .as_array()
            .ok_or("eth_getLogs result is not an array")?;
        for log in rows {
            let tx = log
                .get("transactionHash")
                .and_then(|v| v.as_str())
                .ok_or("log missing transactionHash")?
                .to_string();
            let block = log
                .get("blockNumber")
                .and_then(|v| v.as_str())
                .and_then(crate::parse_hex_u64)
                .ok_or("log missing blockNumber")?;
            out.entry(tx).or_insert(block);
        }
        cursor = last.checked_add(1).ok_or("log cursor overflow")?;
    }
    Ok(out
        .into_iter()
        .map(|(tx_hash, block)| Candidate { tx_hash, block })
        .collect())
}

fn transfer_topic() -> String {
    format!(
        "0x{}",
        alloy_primitives::hex::encode(keccak256("Transfer(address,address,uint256)"))
    )
}

fn address_topic(a: Address) -> String {
    let mut word = [0u8; 32];
    if let Some(dest) = word.get_mut(12..32) {
        dest.copy_from_slice(a.as_slice());
    }
    format!("0x{}", alloy_primitives::hex::encode(word))
}

fn topic_is_address(topic: &str, a: Address) -> bool {
    let raw = topic.strip_prefix("0x").unwrap_or(topic);
    let Ok(bytes) = decode_hex_bytes_raw(raw) else {
        return false;
    };
    if bytes.len() != 32 {
        return false;
    }
    bytes.get(12..32) == Some(a.as_slice())
}

async fn rpc(
    client: &reqwest::Client,
    url: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let resp = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("{method}: {e}"))?;
    let v: Value = resp.json().await.map_err(|e| format!("{method}: {e}"))?;
    if let Some(err) = v.get("error") {
        return Err(format!("{method}: {err}"));
    }
    v.get("result")
        .cloned()
        .ok_or_else(|| format!("{method}: missing result"))
}

fn read_cursor(path: &Path) -> Result<Option<u64>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let text = text.trim();
    if text.is_empty() {
        return Err("books cursor is empty — scan refused".into());
    }
    text.parse::<u64>()
        .map(Some)
        .map_err(|_| "books cursor is not a block number".into())
}

fn write_cursor(path: &Path, next: u64) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, format!("{next}\n")).map_err(|e| e.to_string())
}

fn decode_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    decode_hex_bytes_raw(s)
}

fn decode_hex_bytes_raw(s: &str) -> Result<Vec<u8>, String> {
    if s.len().checked_rem(2) != Some(0) {
        return Err("odd hex length".into());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let hi = crate::hex_val(*bytes.get(i).ok_or("hex")?).ok_or("hex")?;
        let lo = crate::hex_val(*bytes.get(i.checked_add(1).ok_or("hex")?).ok_or("hex")?)
            .ok_or("hex")?;
        out.push(
            hi.checked_mul(16)
                .ok_or("hex")?
                .checked_add(lo)
                .ok_or("hex")?,
        );
        i = i.checked_add(2).ok_or("hex")?;
    }
    Ok(out)
}

/// Address parse used by the binary. Empty and zero are refused by the caller.
#[must_use]
pub fn parse_address(s: &str) -> Option<Address> {
    Address::from_str(s).ok().filter(|a| !a.is_zero())
}
