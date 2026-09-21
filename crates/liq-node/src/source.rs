//! [`LogSource`]: ExEx seam (03B fills the producer) and real [`RpcPoll`]
//! (`eth_getLogs`, paginated). Same fold, different feed (GUIDE 03 §2, §5).

use std::collections::HashMap;

use alloy_primitives::{Address, B256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::{BlockNumberOrTag, Filter, Log as RpcLog};
use arrayvec::ArrayVec;
use liq_protocol::{Archive, ArchiveError, BlockNum, DecodedLog, Timestamp};
use liq_types::LogFilter;
use rtrb::PopError;

use crate::{decode::DecodeArena, IngestError, Result};

/// Default `eth_getLogs` block span; halved on a too-large response.
pub const DEFAULT_PAGE_BLOCKS: u64 = 2_000;

/// What [`LogSource::poll_block`] produced.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Poll {
    /// A block was written into `out`.
    Ready,
    /// Nothing ready (ExEx empty; RpcPoll waiting on the next fetch).
    Idle,
    /// RpcPoll has passed its pinned end (or the chain head when following).
    Exhausted,
}

/// One receipt log, owned. Allocated by the source (ExEx forwarder or RPC),
/// never on the decode/route/fold hot path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedLog {
    pub address: Address,
    pub topics: ArrayVec<B256, 4>,
    pub data: Vec<u8>,
    pub block: BlockNum,
    pub timestamp: Timestamp,
    pub tx_index: u32,
    pub log_index: u32,
}

/// One block of owned logs. Reused by [`LogSource::poll_block`].
///
/// `gas_limit` is the header value (GUIDE 12 §4f). `0` means absent — never
/// a compiled-in 30M stand-in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnedBlock {
    pub number: BlockNum,
    pub timestamp: Timestamp,
    pub gas_limit: u64,
    pub logs: Vec<OwnedLog>,
}

impl OwnedBlock {
    #[must_use]
    pub fn with_capacity(n: usize) -> Self {
        Self {
            number: 0,
            timestamp: 0,
            gas_limit: 0,
            logs: Vec::with_capacity(n),
        }
    }

    /// Drop contents, keep allocations.
    pub fn clear(&mut self) {
        self.number = 0;
        self.timestamp = 0;
        self.gas_limit = 0;
        self.logs.clear();
    }
}

/// Sync source of canonical logs. The hot thread drains this; it must not
/// `.await`.
pub trait LogSource {
    /// Write the next ready block into `out` (which the caller reuses).
    fn poll_block(&mut self, out: &mut OwnedBlock) -> Result<Poll>;
}

/// Consumer side of the ExEx → hot-thread SPSC. WP 03B owns the producer and
/// the Reth future; this crate only drains owned payloads.
///
/// [`LogSource::poll_block`] does `*out = block`, which drops the caller's
/// reused [`OwnedBlock`] (and frees its `Vec`s) every block. There is no
/// recycle path because the ring is one-way. **03B must ship a return ring**
/// so `OwnedBlock` buffers are recycled instead of freed on the hot path.
pub struct ExExSource {
    rx: rtrb::Consumer<OwnedBlock>,
}

impl ExExSource {
    /// 03B calls this with the consumer half of `rtrb::RingBuffer`.
    #[must_use]
    pub fn new(rx: rtrb::Consumer<OwnedBlock>) -> Self {
        Self { rx }
    }
}

impl LogSource for ExExSource {
    fn poll_block(&mut self, out: &mut OwnedBlock) -> Result<Poll> {
        match self.rx.pop() {
            Ok(block) => {
                *out = block;
                Ok(Poll::Ready)
            }
            Err(PopError::Empty) => Ok(Poll::Idle),
        }
    }
}

/// `eth_getLogs` poller. Used by backfill, 05F lite validation, and 05B
/// extraction. Pagination is by block range; a too-large response halves the
/// span. An empty successful page is a complete empty range, not truncation.
pub struct RpcPoll<P> {
    provider: P,
    addresses: Vec<Address>,
    topic0s: Vec<B256>,
    cursor: BlockNum,
    end: Option<BlockNum>,
    page: u64,
    ready: Vec<OwnedBlock>,
    ready_at: usize,
}

impl<P> RpcPoll<P> {
    /// `end = None` follows the chain head (05F). `end = Some(to)` stops at
    /// the pinned block (backfill).
    #[must_use]
    pub fn new(
        provider: P,
        filters: &[LogFilter],
        from: BlockNum,
        end: Option<BlockNum>,
        page: u64,
    ) -> Self {
        let mut addresses = Vec::with_capacity(filters.len());
        let mut topic0s = Vec::with_capacity(filters.len());
        for f in filters {
            if !addresses.contains(&f.address) {
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
    pub fn cursor(&self) -> BlockNum {
        self.cursor
    }
}

impl<P: Provider> RpcPoll<P> {
    /// Pull the next `eth_getLogs` page, group by block, fill [`Self::ready`].
    /// Call until [`Poll::Exhausted`], draining with [`LogSource::poll_block`].
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
                    .map_err(|_| IngestError::SourceUnavailable)?;
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
                Err(IngestError::Truncated { .. }) if span > 1 => {
                    let Some(next) = split_span(span) else {
                        return Err(IngestError::Truncated {
                            from: self.cursor,
                            to: last,
                        });
                    };
                    span = next;
                    self.page = span;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn get_logs_range(&self, from: BlockNum, to: BlockNum) -> Result<Vec<RpcLog>> {
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
            // JSON-RPC error response: node refused the range (too large).
            // Halve. A transport fault (timeout, refused, reset) is not that.
            Err(e) if e.is_error_resp() => Err(IngestError::Truncated { from, to }),
            Err(_) => Err(IngestError::SourceUnavailable),
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
        let mut header_cache: HashMap<u64, (u64, u64)> = HashMap::new();
        let mut current: Option<OwnedBlock> = None;
        for rpc in logs {
            let Some(block) = rpc.block_number else {
                return Err(IngestError::MalformedLog);
            };
            let (ts, gas_limit) = if let Some(t) = rpc.block_timestamp {
                let g = self.header_gas_or_zero(block, &mut header_cache).await;
                (t, g)
            } else {
                self.header_meta(block, &mut header_cache).await?
            };
            let owned = rpc_to_owned(&rpc, ts)?;
            match current.as_mut() {
                Some(cur) if cur.number == block => cur.logs.push(owned),
                Some(_) => {
                    if let Some(done) = current.replace(OwnedBlock {
                        number: block,
                        timestamp: ts,
                        gas_limit,
                        logs: vec![owned],
                    }) {
                        self.ready.push(done);
                    }
                }
                None => {
                    current = Some(OwnedBlock {
                        number: block,
                        timestamp: ts,
                        gas_limit,
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

    /// Header timestamp and `gasLimit`. Missing header is unavailable, not a
    /// defaulted 30M gas limit.
    async fn header_meta(
        &self,
        block: u64,
        cache: &mut HashMap<u64, (u64, u64)>,
    ) -> Result<(u64, u64)> {
        if let Some(&v) = cache.get(&block) {
            return Ok(v);
        }
        let b = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(block))
            .await
            .map_err(|_| IngestError::SourceUnavailable)?
            .ok_or(IngestError::SourceUnavailable)?;
        let t = b.header.timestamp;
        let gas_limit = b.header.gas_limit;
        cache.insert(block, (t, gas_limit));
        Ok((t, gas_limit))
    }

    /// Observed header gas, or `0` if the header is unavailable. Never a 30M
    /// default. A missing gas limit does not drop logs we already have.
    async fn header_gas_or_zero(
        &self,
        block: u64,
        cache: &mut HashMap<u64, (u64, u64)>,
    ) -> u64 {
        match self.header_meta(block, cache).await {
            Ok((_, g)) => g,
            Err(_) => {
                tracing::error!(
                    block,
                    "header gas_limit unavailable — leaving 0 (no 30M default)"
                );
                0
            }
        }
    }

    /// Fetch `[from, to]` completely (Archive / backfill). Errors rather than
    /// returning a silently empty truncated range.
    pub async fn fetch_range(&mut self, from: BlockNum, to: BlockNum) -> Result<Vec<OwnedLog>> {
        if from > to {
            return Err(IngestError::Truncated { from, to });
        }
        self.cursor = from;
        self.end = Some(to);
        self.ready.clear();
        self.ready_at = 0;
        let mut out = Vec::new();
        loop {
            match self.fetch_page().await? {
                Poll::Exhausted => break,
                Poll::Idle => {
                    if self.cursor > to {
                        break;
                    }
                }
                Poll::Ready => {
                    while self.ready_at < self.ready.len() {
                        if let Some(b) = self.ready.get(self.ready_at) {
                            out.extend_from_slice(&b.logs);
                        }
                        self.ready_at = self.ready_at.saturating_add(1);
                    }
                    self.ready.clear();
                    self.ready_at = 0;
                }
            }
        }
        Ok(out)
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
        out.number = block.number;
        out.timestamp = block.timestamp;
        out.gas_limit = block.gas_limit;
        out.logs.clear();
        out.logs.extend_from_slice(&block.logs);
        self.ready_at = self.ready_at.saturating_add(1);
        Ok(Poll::Ready)
    }
}

impl<P: Provider + Clone + Send + Sync + 'static> Archive for RpcPoll<P> {
    fn logs(
        &self,
        filters: &[LogFilter],
        from: BlockNum,
        to: BlockNum,
        visit: &mut dyn FnMut(&DecodedLog<'_>) -> liq_protocol::Result<()>,
    ) -> liq_protocol::Result<()> {
        let provider = self.provider.clone();
        let filters = filters.to_vec();
        let fetched = std::thread::Builder::new()
            .name("liq-node-archive".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| ArchiveError::Unavailable)?;
                rt.block_on(async move {
                    let mut poll =
                        RpcPoll::new(provider, &filters, from, Some(to), DEFAULT_PAGE_BLOCKS);
                    poll.fetch_range(from, to).await.map_err(|e| match e {
                        IngestError::Truncated { from, .. } => ArchiveError::Truncated { from },
                        IngestError::MalformedLog => ArchiveError::Malformed { block: from },
                        _ => ArchiveError::Unavailable,
                    })
                })
            })
            .map_err(|_| ArchiveError::Unavailable)?
            .join()
            .map_err(|_| ArchiveError::Unavailable)??;

        let mut arena = DecodeArena::with_capacity(1 << 16);
        for log in &fetched {
            arena.reset();
            visit(&arena.copy(log))?;
        }
        Ok(())
    }
}

fn rpc_to_owned(rpc: &RpcLog, timestamp: Timestamp) -> Result<OwnedLog> {
    let mut topics = ArrayVec::new();
    for t in rpc.topics() {
        topics.try_push(*t).map_err(|_| IngestError::MalformedLog)?;
    }
    let block = rpc.block_number.ok_or(IngestError::MalformedLog)?;
    // Mined logs always carry both indices; absent means the node handed back
    // a pending log. Defaulting to 0 would silently reorder the fold, so fail.
    let tx_index = rpc
        .transaction_index
        .ok_or(IngestError::MalformedLog)
        .and_then(|i| u32::try_from(i).map_err(|_| IngestError::MalformedLog))?;
    let log_index = rpc
        .log_index
        .ok_or(IngestError::MalformedLog)
        .and_then(|i| u32::try_from(i).map_err(|_| IngestError::MalformedLog))?;
    Ok(OwnedLog {
        address: rpc.address(),
        topics,
        data: rpc.data().data.to_vec(),
        block,
        timestamp,
        tx_index,
        log_index,
    })
}

/// Bisect helper. Oracle: range arithmetic — a span of 1 cannot split.
#[must_use]
pub(crate) fn split_span(span: u64) -> Option<u64> {
    if span <= 1 {
        None
    } else {
        Some(span.saturating_add(1).saturating_div(2).max(1))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::{split_span, RpcPoll};
    use crate::IngestError;
    use alloy_provider::ProviderBuilder;

    /// Header gas_limit is observed or absent. `0` is not a 30M stand-in.
    #[test]
    fn owned_block_gas_limit_absent_is_zero() {
        let b = crate::source::OwnedBlock::default();
        assert_eq!(b.gas_limit, 0);
        assert_ne!(b.gas_limit, 30_000_000);
        let mut c = crate::source::OwnedBlock {
            number: 1,
            timestamp: 1,
            gas_limit: 45_000_000,
            logs: Vec::new(),
        };
        assert_eq!(c.gas_limit, 45_000_000);
        c.clear();
        assert_eq!(c.gas_limit, 0);
    }

    /// Oracle: integer halving. Negative: a one-block span does not split
    /// (that is Truncated, not an infinite loop).
    #[test]
    fn split_span_halves_and_stops_at_one() {
        assert_eq!(split_span(2_000), Some(1_000));
        assert_eq!(split_span(3), Some(2));
        assert_eq!(split_span(1), None);
        assert_eq!(split_span(0), None);
    }

    /// Oracle: empty mock-transport queue is `RpcError::Transport`, not an
    /// `ErrorResp`. Negative: classifying it as Truncated would bisect the
    /// span (~11 wasted round-trips from 2000) then lie to the operator.
    #[tokio::test(flavor = "current_thread")]
    async fn transport_error_is_source_unavailable_not_truncated() {
        let asserter = alloy_provider::transport::mock::Asserter::new();
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_mocked_client(asserter);
        let mut poll = RpcPoll::new(provider, &[], 100, Some(200), 8);
        let err = poll.fetch_page().await.unwrap_err();
        assert_eq!(err, IngestError::SourceUnavailable);
    }

    /// Oracle: a JSON-RPC error payload (`ErrorResp`) is "range too large"
    /// and must bisect. Negative: treating it as SourceUnavailable would
    /// abort a recoverable oversize query on the first page.
    #[tokio::test(flavor = "current_thread")]
    async fn json_rpc_error_resp_halves_then_truncated() {
        let asserter = alloy_provider::transport::mock::Asserter::new();
        // span 4 → 2 → 1, then Truncated. Three ErrorResp payloads.
        asserter.push_failure_msg("query returned more than 10000 results");
        asserter.push_failure_msg("query returned more than 10000 results");
        asserter.push_failure_msg("query returned more than 10000 results");
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_mocked_client(asserter);
        let mut poll = RpcPoll::new(provider, &[], 100, Some(200), 4);
        let err = poll.fetch_page().await.unwrap_err();
        assert_eq!(err, IngestError::Truncated { from: 100, to: 100 });
    }
}
