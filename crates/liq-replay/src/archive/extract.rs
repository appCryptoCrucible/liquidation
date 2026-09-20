//! `eth_getLogs` over the C3 address set → partitioned parquet.
//!
//! ActualLiquidation rows come from [`liq_watch::batch::extract_range`] as-is
//! (inherits W-D1: inferred_bid is priority-fee tip only).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use alloy_primitives::{hex, Address, B256, I256, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::{BlockNumberOrTag, Filter, Log as RpcLog};
use alloy_sol_types::SolEvent;
use liq_config::Intern;
use liq_protocol::FeedId;
use liq_types::fixed::Ray;
use liq_watch::abi::chainlink;
use liq_watch::batch::{extract_range, write_parquet};
use liq_watch::decode::WatchDecoder;
use liq_watch::source::{OwnedBlock, OwnedLog, DEFAULT_PAGE_BLOCKS};
use thiserror::Error;

use super::store::{
    actual_path, append_reorgs, apply_reorg_headers, events_path, headers_path, mark_events_reorg,
    mark_prices_reorg, prices_path, read_events, read_headers, read_prices, write_events,
    write_headers, write_prices, ArchivedPrice, EventRow, HeaderRow, DEFAULT_PARTITION_BLOCKS,
};

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("LIQ_ARCHIVE_RPC unset")]
    NoRpc,
    #[error("rpc: {0}")]
    Rpc(String),
    #[error("io: {0}")]
    Io(String),
    #[error("parquet: {0}")]
    Parquet(String),
    #[error("filter: {0}")]
    Filter(String),
    #[error("watch: {0}")]
    Watch(String),
    #[error("archive truncated {from}..={to}")]
    Truncated { from: u64, to: u64 },
    #[error("block {block} hash mismatch stored={stored:#x} seen={seen:#x}")]
    HashMismatch {
        block: u64,
        stored: B256,
        seen: B256,
    },
    #[error("A3 second-source verification deferred (D60)")]
    A3Deferred,
}

#[derive(Clone, Debug, Default)]
pub struct ExtractReport {
    pub from: u64,
    pub to: u64,
    pub headers: u64,
    pub events: u64,
    pub prices: u64,
    pub actuals: u64,
    pub reorgs: u64,
}

pub(super) fn addr_chunk() -> usize {
    std::env::var("LIQ_ARCHIVE_ADDR_CHUNK")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(64)
}

pub(super) fn partition_blocks() -> u64 {
    std::env::var("LIQ_ARCHIVE_PART_BLOCKS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_PARTITION_BLOCKS)
}

/// GUIDE 05 §1 second-source check — A3 local node is deferred (D60).
pub fn second_source_verify() -> Result<(), ExtractError> {
    tracing::error!("A3 deferred: second-source liquidation check not run");
    Err(ExtractError::A3Deferred)
}

pub async fn extract_window<P: Provider + Clone>(
    provider: P,
    decoder: &WatchDecoder,
    intern: &Intern,
    c3: &[Address],
    root: &Path,
    from: u64,
    to: u64,
) -> Result<ExtractReport, ExtractError> {
    if from > to {
        return Err(ExtractError::Truncated { from, to });
    }
    if c3.is_empty() {
        return Err(ExtractError::Filter("C3 address list empty".into()));
    }
    let part = partition_blocks();
    let chunk = addr_chunk();
    let feeds = feed_by_agg(intern);
    let mut report = ExtractReport {
        from,
        to,
        ..ExtractReport::default()
    };
    let mut start = from;
    while start <= to {
        let end = start.saturating_add(part.saturating_sub(1)).min(to);
        let r = extract_partition(&provider, decoder, &feeds, c3, chunk, root, start, end).await?;
        report.headers = report.headers.saturating_add(r.headers);
        report.events = report.events.saturating_add(r.events);
        report.prices = report.prices.saturating_add(r.prices);
        report.reorgs = report.reorgs.saturating_add(r.reorgs);
        start = match end.checked_add(1) {
            Some(n) => n,
            None => break,
        };
    }
    let (_decoded, actuals) = extract_range(provider, decoder, from, to)
        .await
        .map_err(|e| ExtractError::Watch(e.to_string()))?;
    let ap = actual_path(root, from, to);
    if let Some(parent) = ap.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ExtractError::Io(e.to_string()))?;
    }
    write_parquet(&ap, &actuals).map_err(|e| ExtractError::Watch(e.to_string()))?;
    report.actuals =
        u64::try_from(actuals.len()).map_err(|_| ExtractError::Watch("actuals len".into()))?;
    Ok(report)
}

fn feed_by_agg(intern: &Intern) -> HashMap<Address, (FeedId, u8)> {
    let mut m = HashMap::new();
    for f in intern.feeds() {
        m.insert(f.aggregator, (f.id, f.decimals));
    }
    m
}

#[allow(clippy::too_many_arguments)]
async fn extract_partition<P: Provider>(
    provider: &P,
    decoder: &WatchDecoder,
    feeds: &HashMap<Address, (FeedId, u8)>,
    c3: &[Address],
    chunk: usize,
    root: &Path,
    from: u64,
    to: u64,
) -> Result<ExtractReport, ExtractError> {
    let mut blocks: BTreeMap<u64, OwnedBlock> = BTreeMap::new();
    let mut i = 0;
    while i < c3.len() {
        let end = i.saturating_add(chunk).min(c3.len());
        let addrs = c3
            .get(i..end)
            .ok_or_else(|| ExtractError::Filter("addr chunk".into()))?;
        pull_logs(provider, addrs, from, to, &mut blocks).await?;
        i = end;
    }
    fill_headers(provider, from, to, &mut blocks).await?;

    let hp = headers_path(root, from, to);
    let mut old_headers = read_headers(&hp)?;
    let mut reorgs = Vec::new();
    for (n, b) in &blocks {
        let old_hash = old_headers
            .iter()
            .find(|h| h.block == *n && !h.superseded)
            .map(|h| h.hash);
        if let Some(old_hash) = old_hash {
            if old_hash != b.hash {
                if let Some(row) = apply_reorg_headers(&mut old_headers, *n, old_hash, b.hash) {
                    reorgs.push(row);
                }
            }
        }
    }
    let ep = events_path(root, from, to);
    let pp = prices_path(root, from, to);
    let mut old_events = read_events(&ep)?;
    let mut old_prices = read_prices(&pp)?;
    for r in &reorgs {
        mark_events_reorg(&mut old_events, r.at_block, r.old_tip);
        mark_prices_reorg(&mut old_prices, r.at_block, r.old_tip);
    }

    let mut headers = old_headers;
    let mut events = old_events;
    let mut prices = old_prices;
    events.retain(|e| e.superseded);
    prices.retain(|p| p.superseded);
    for (n, b) in &blocks {
        headers.retain(|h| h.block != *n || h.superseded);
        headers.push(HeaderRow {
            block: *n,
            hash: b.hash,
            timestamp: b.timestamp,
            superseded: false,
        });
        let vol = decoder.coverage_from_oracle_logs(&b.logs, u32::MAX).1;
        for log in &b.logs {
            let (trig, _) = decoder.coverage_from_oracle_logs(&b.logs, log.tx_index);
            match decoder.decode_log(log, trig, vol) {
                Ok(_) => {}
                Err(e) => {
                    tracing::error!(error = %e, block = log.block, "W decoder failed");
                    return Err(ExtractError::Watch(e.to_string()));
                }
            }
            events.push(event_row(log, false));
            if let Some(p) = price_row(decoder, feeds, log)? {
                prices.push(p);
            }
        }
    }
    headers.sort_by_key(|h| h.block);
    write_headers(&root.join("headers"), from, to, &headers)?;
    write_events(&root.join("events"), from, to, &events)?;
    write_prices(&root.join("prices"), from, to, &prices)?;
    append_reorgs(&root.join("reorgs"), &reorgs)?;
    Ok(ExtractReport {
        from,
        to,
        headers: u64::try_from(headers.iter().filter(|h| !h.superseded).count())
            .map_err(|_| ExtractError::Parquet("headers count".into()))?,
        events: u64::try_from(events.iter().filter(|e| !e.superseded).count())
            .map_err(|_| ExtractError::Parquet("events count".into()))?,
        prices: u64::try_from(prices.iter().filter(|p| !p.superseded).count())
            .map_err(|_| ExtractError::Parquet("prices count".into()))?,
        actuals: 0,
        reorgs: u64::try_from(reorgs.len())
            .map_err(|_| ExtractError::Parquet("reorgs count".into()))?,
    })
}

fn event_row(log: &OwnedLog, superseded: bool) -> EventRow {
    let mut topics = String::new();
    for (i, t) in log.topics.iter().enumerate() {
        if i != 0 {
            topics.push(',');
        }
        topics.push_str(&format!("{t:#x}"));
    }
    EventRow {
        block: log.block,
        block_hash: log.block_hash,
        timestamp: log.timestamp,
        tx_hash: log.tx_hash,
        tx_index: log.tx_index,
        log_index: log.log_index,
        address: log.address,
        topics,
        data_hex: format!("0x{}", hex::encode(&log.data)),
        superseded,
    }
}

fn price_row(
    decoder: &WatchDecoder,
    feeds: &HashMap<Address, (FeedId, u8)>,
    log: &OwnedLog,
) -> Result<Option<ArchivedPrice>, ExtractError> {
    let t0 = log.topics.first().copied();
    if t0 != Some(chainlink::AnswerUpdated::SIGNATURE_HASH) {
        return Ok(None);
    }
    if !decoder.is_aggregator(log.address) {
        return Ok(None);
    }
    let Some(&(feed, decimals)) = feeds.get(&log.address) else {
        tracing::error!(agg = %log.address, "AnswerUpdated aggregator not interned; price row withheld");
        return Ok(None);
    };
    let ev = chainlink::AnswerUpdated::decode_raw_log(log.topics.iter().copied(), &log.data)
        .map_err(|_| {
            tracing::error!(agg = %log.address, "AnswerUpdated ABI decode failed");
            ExtractError::Watch("AnswerUpdated".into())
        })?;
    let ray = answer_to_ray(ev.current, decimals)?;
    Ok(Some(ArchivedPrice {
        block: log.block,
        block_hash: log.block_hash,
        timestamp: log.timestamp,
        tx: log.tx_hash,
        feed: feed.0,
        aggregator: log.address,
        price_ray: ray.raw().to_string(),
        decimals: u16::from(decimals),
        superseded: false,
    }))
}

fn answer_to_ray(answer: I256, decimals: u8) -> Result<Ray, ExtractError> {
    if !answer.is_positive() {
        tracing::error!("non-positive AnswerUpdated; withheld");
        return Err(ExtractError::Watch("non-positive answer".into()));
    }
    let mag = answer.unsigned_abs();
    let Some(exp) = 27u32.checked_sub(u32::from(decimals)) else {
        return Err(ExtractError::Watch("oracle decimals > 27".into()));
    };
    let factor = pow10(exp)?;
    let raw = mag
        .checked_mul(factor)
        .ok_or_else(|| ExtractError::Watch("ray scale overflow".into()))?;
    Ok(Ray::from_raw(raw))
}

fn pow10(exp: u32) -> Result<U256, ExtractError> {
    let mut v = U256::from(1u8);
    let ten = U256::from(10u8);
    let mut i = 0u32;
    while i < exp {
        v = v
            .checked_mul(ten)
            .ok_or_else(|| ExtractError::Watch("pow10 overflow".into()))?;
        i = i.saturating_add(1);
    }
    Ok(v)
}

async fn pull_logs<P: Provider>(
    provider: &P,
    addrs: &[Address],
    from: u64,
    to: u64,
    out: &mut BTreeMap<u64, OwnedBlock>,
) -> Result<(), ExtractError> {
    let mut cursor = from;
    let mut page = DEFAULT_PAGE_BLOCKS;
    while cursor <= to {
        let last = cursor.saturating_add(page.saturating_sub(1)).min(to);
        match get_logs(provider, addrs, cursor, last).await {
            Ok(logs) => {
                ingest_rpc_logs(provider, logs, out).await?;
                cursor = match last.checked_add(1) {
                    Some(n) => n,
                    None => break,
                };
            }
            Err(ExtractError::Truncated { .. }) if page > 1 => {
                page = page.saturating_add(1).saturating_div(2).max(1);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

async fn get_logs<P: Provider>(
    provider: &P,
    addrs: &[Address],
    from: u64,
    to: u64,
) -> Result<Vec<RpcLog>, ExtractError> {
    let mut filter = Filter::new()
        .from_block(BlockNumberOrTag::Number(from))
        .to_block(BlockNumberOrTag::Number(to));
    if !addrs.is_empty() {
        filter = filter.address(addrs.to_vec());
    }
    match provider.get_logs(&filter).await {
        Ok(logs) => Ok(logs),
        Err(e) if e.is_error_resp() => {
            tracing::error!(from, to, "eth_getLogs error_resp (treat as truncated span)");
            Err(ExtractError::Truncated { from, to })
        }
        Err(e) => Err(ExtractError::Rpc(e.to_string())),
    }
}

async fn ingest_rpc_logs<P: Provider>(
    provider: &P,
    logs: Vec<RpcLog>,
    out: &mut BTreeMap<u64, OwnedBlock>,
) -> Result<(), ExtractError> {
    for rpc in logs {
        let block = rpc
            .block_number
            .ok_or_else(|| ExtractError::Rpc("log missing block".into()))?;
        let (ts, hash) = match (rpc.block_timestamp, rpc.block_hash) {
            (Some(t), Some(h)) => (t, h),
            _ => header(provider, block).await?,
        };
        let owned = rpc_to_owned(&rpc, ts, hash)?;
        match out.get_mut(&block) {
            Some(cur) => {
                if cur.hash != hash {
                    return Err(ExtractError::HashMismatch {
                        block,
                        stored: cur.hash,
                        seen: hash,
                    });
                }
                cur.logs.push(owned);
            }
            None => {
                out.insert(
                    block,
                    OwnedBlock {
                        number: block,
                        hash,
                        timestamp: ts,
                        logs: vec![owned],
                    },
                );
            }
        }
    }
    for b in out.values_mut() {
        b.logs.sort_by_key(|l| (l.tx_index, l.log_index));
    }
    Ok(())
}

async fn fill_headers<P: Provider>(
    provider: &P,
    from: u64,
    to: u64,
    out: &mut BTreeMap<u64, OwnedBlock>,
) -> Result<(), ExtractError> {
    let mut n = from;
    loop {
        if let std::collections::btree_map::Entry::Vacant(e) = out.entry(n) {
            let (ts, hash) = header(provider, n).await?;
            e.insert(OwnedBlock {
                number: n,
                hash,
                timestamp: ts,
                logs: Vec::new(),
            });
        }
        if n == to {
            break;
        }
        n = n
            .checked_add(1)
            .ok_or(ExtractError::Truncated { from, to })?;
    }
    Ok(())
}

async fn header<P: Provider>(provider: &P, block: u64) -> Result<(u64, B256), ExtractError> {
    let b = provider
        .get_block_by_number(BlockNumberOrTag::Number(block))
        .await
        .map_err(|e| ExtractError::Rpc(e.to_string()))?
        .ok_or_else(|| {
            tracing::error!(block, "missing header; extract refused");
            ExtractError::Rpc(format!("missing header {block}"))
        })?;
    Ok((b.header.timestamp, b.header.hash))
}

fn rpc_to_owned(rpc: &RpcLog, timestamp: u64, block_hash: B256) -> Result<OwnedLog, ExtractError> {
    let mut topics = arrayvec::ArrayVec::new();
    for t in rpc.topics() {
        topics
            .try_push(*t)
            .map_err(|_| ExtractError::Rpc("topics>4".into()))?;
    }
    let block = rpc
        .block_number
        .ok_or_else(|| ExtractError::Rpc("log block".into()))?;
    let tx_index = rpc
        .transaction_index
        .ok_or_else(|| ExtractError::Rpc("tx_index".into()))
        .and_then(|i| u32::try_from(i).map_err(|_| ExtractError::Rpc("tx_index".into())))?;
    let log_index = rpc
        .log_index
        .ok_or_else(|| ExtractError::Rpc("log_index".into()))
        .and_then(|i| u32::try_from(i).map_err(|_| ExtractError::Rpc("log_index".into())))?;
    let tx_hash = rpc
        .transaction_hash
        .ok_or_else(|| ExtractError::Rpc("tx_hash".into()))?;
    let hash = rpc.block_hash.unwrap_or(block_hash);
    Ok(OwnedLog {
        address: rpc.address(),
        topics,
        data: rpc.data().data.to_vec(),
        block,
        block_hash: hash,
        tx_hash,
        timestamp,
        tx_index,
        log_index,
    })
}
