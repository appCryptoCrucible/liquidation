//! Independent copy of 03A's [`LogSource`] / RpcPoll fold. `liq-watch` must
//! not depend on `liq-node` (that crate pulls `liq-state`). Same `eth_getLogs`
//! pagination: JSON-RPC error → halve span; transport error → unavailable.

use std::collections::HashMap;

use alloy_primitives::{Address, B256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::{BlockNumberOrTag, Filter, Log as RpcLog};
use arrayvec::ArrayVec;
use liq_types::LogFilter;

use crate::error::{Result, WatchError};

pub const DEFAULT_PAGE_BLOCKS: u64 = 10;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Poll {
    Ready,
    Idle,
    Exhausted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedLog {
    pub address: Address,
    pub topics: ArrayVec<B256, 4>,
    pub data: Vec<u8>,
    pub block: u64,
    pub block_hash: B256,
    pub tx_hash: B256,
    pub timestamp: u64,
    pub tx_index: u32,
    pub log_index: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnedBlock {
    pub number: u64,
    pub hash: B256,
    pub timestamp: u64,
    pub logs: Vec<OwnedLog>,
}

pub trait LogSource {
    fn poll_block(&mut self, out: &mut OwnedBlock) -> Result<Poll>;
}

pub struct RpcPoll<P> {
    provider: P,
    addresses: Vec<Address>,
    topic0s: Vec<B256>,
    cursor: u64,
    end: Option<u64>,
    page: u64,
    ready: Vec<OwnedBlock>,
    ready_at: usize,
}

impl<P> RpcPoll<P> {
    #[must_use]
    pub fn new(provider: P, filters: &[LogFilter], from: u64, end: Option<u64>, page: u64) -> Self {
        let mut addresses = Vec::new();
        let mut topic0s = Vec::new();
        for f in filters {
            if !f.address.is_zero() && !addresses.contains(&f.address) {
                addresses.push(f.address);
            }
            if !topic0s.contains(&f.topic0) {
                topic0s.push(f.topic0);
            }
        }
        Self {
            provider,
            addresses,
            topic0s,
            cursor: from,
            end,
            page: page.max(1),
            ready: Vec::new(),
            ready_at: 0,
        }
    }

    #[inline]
    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    #[inline]
    #[must_use]
    pub fn provider(&self) -> &P {
        &self.provider
    }
}

impl<P: Provider> RpcPoll<P> {
    pub async fn fetch_page(&mut self) -> Result<Poll> {
        if self.ready_at < self.ready.len() {
            return Ok(Poll::Ready);
        }
        self.ready.clear();
        self.ready_at = 0;
        let cap = match self.end {
            Some(to) if self.cursor > to => return Ok(Poll::Exhausted),
            Some(to) => to,
            None => {
                let head = self
                    .provider
                    .get_block_number()
                    .await
                    .map_err(|e| WatchError::Rpc(e.to_string()))?;
                if self.cursor > head {
                    return Ok(Poll::Idle);
                }
                head
            }
        };
        let mut span = self.page;
        loop {
            let last = match self.cursor.checked_add(span.saturating_sub(1)) {
                Some(n) => n.min(cap),
                None => cap,
            };
            if last < self.cursor {
                return Ok(Poll::Exhausted);
            }
            match self.get_logs_range(self.cursor, last).await {
                Ok(logs) => {
                    self.group_by_block(logs).await?;
                    self.cursor = match last.checked_add(1) {
                        Some(n) => n,
                        None => return Ok(Poll::Exhausted),
                    };
                    if self.ready.is_empty() {
                        if self.end.is_some_and(|to| self.cursor > to) {
                            return Ok(Poll::Exhausted);
                        }
                        return Ok(Poll::Idle);
                    }
                    return Ok(Poll::Ready);
                }
                Err(WatchError::Truncated { .. }) if span > 1 => {
                    span = span.saturating_add(1).saturating_div(2).max(1);
                    self.page = span;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn get_logs_range(&self, from: u64, to: u64) -> Result<Vec<RpcLog>> {
        let mut filter = Filter::new()
            .from_block(BlockNumberOrTag::Number(from))
            .to_block(BlockNumberOrTag::Number(to));
        if !self.addresses.is_empty() {
            filter = filter.address(self.addresses.clone());
        }
        if !self.topic0s.is_empty() {
            filter = filter.event_signature(self.topic0s.clone());
        }
        match self.provider.get_logs(&filter).await {
            Ok(logs) => Ok(logs),
            Err(e) if e.is_error_resp() => Err(WatchError::Truncated { from, to }),
            Err(e) => Err(WatchError::Rpc(e.to_string())),
        }
    }

    async fn group_by_block(&mut self, mut logs: Vec<RpcLog>) -> Result<()> {
        logs.sort_by_key(|l| {
            (
                l.block_number.unwrap_or(0),
                l.transaction_index.unwrap_or(0),
                l.log_index.unwrap_or(0),
            )
        });
        let mut ts_cache: HashMap<u64, (u64, B256)> = HashMap::new();
        let mut current: Option<OwnedBlock> = None;
        for rpc in logs {
            let block = rpc.block_number.ok_or(WatchError::MalformedLog)?;
            let (ts, hash) = match (rpc.block_timestamp, rpc.block_hash) {
                (Some(t), Some(h)) => (t, h),
                _ => self.header(block, &mut ts_cache).await?,
            };
            let owned = rpc_to_owned(&rpc, ts, hash)?;
            match current.as_mut() {
                Some(cur) if cur.number == block => cur.logs.push(owned),
                Some(_) => {
                    if let Some(done) = current.replace(OwnedBlock {
                        number: block,
                        hash,
                        timestamp: ts,
                        logs: vec![owned],
                    }) {
                        self.ready.push(done);
                    }
                }
                None => {
                    current = Some(OwnedBlock {
                        number: block,
                        hash,
                        timestamp: ts,
                        logs: vec![owned],
                    });
                }
            }
        }
        if let Some(done) = current {
            self.ready.push(done);
        }
        Ok(())
    }

    async fn header(
        &self,
        block: u64,
        cache: &mut HashMap<u64, (u64, B256)>,
    ) -> Result<(u64, B256)> {
        if let Some(&v) = cache.get(&block) {
            return Ok(v);
        }
        let b = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(block))
            .await
            .map_err(|e| WatchError::Rpc(e.to_string()))?
            .ok_or_else(|| WatchError::Rpc("missing header".into()))?;
        let t = b.header.timestamp;
        let h = b.header.hash;
        cache.insert(block, (t, h));
        Ok((t, h))
    }
}

impl<P> LogSource for RpcPoll<P> {
    fn poll_block(&mut self, out: &mut OwnedBlock) -> Result<Poll> {
        if self.ready_at >= self.ready.len() {
            return Ok(if self.end.is_some_and(|to| self.cursor > to) {
                Poll::Exhausted
            } else {
                Poll::Idle
            });
        }
        let Some(block) = self.ready.get(self.ready_at) else {
            return Ok(Poll::Idle);
        };
        *out = block.clone();
        self.ready_at = self.ready_at.saturating_add(1);
        Ok(Poll::Ready)
    }
}

fn rpc_to_owned(rpc: &RpcLog, timestamp: u64, block_hash: B256) -> Result<OwnedLog> {
    let mut topics = ArrayVec::new();
    for t in rpc.topics() {
        topics.try_push(*t).map_err(|_| WatchError::MalformedLog)?;
    }
    let block = rpc.block_number.ok_or(WatchError::MalformedLog)?;
    let tx_index = rpc
        .transaction_index
        .ok_or(WatchError::MalformedLog)
        .and_then(|i| u32::try_from(i).map_err(|_| WatchError::MalformedLog))?;
    let log_index = rpc
        .log_index
        .ok_or(WatchError::MalformedLog)
        .and_then(|i| u32::try_from(i).map_err(|_| WatchError::MalformedLog))?;
    let tx_hash = rpc.transaction_hash.ok_or(WatchError::MalformedLog)?;
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
