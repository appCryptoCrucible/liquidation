//! Log replay from a pinned start to a pinned end, then snapshot (GUIDE 03 §5).
//! Uses [`RpcPoll`] — same fold as live ingest, different feed.

use alloy_provider::Provider;
use liq_protocol::BlockNum;
use liq_state::StateStore;
use liq_types::LogFilter;

use crate::apply::{apply_block, ApplyCtx};
use crate::source::{LogSource, OwnedBlock, Poll, RpcPoll, DEFAULT_PAGE_BLOCKS};
use crate::{IngestError, Result};

/// 02B implements this. 03A only invokes it after the pinned block is folded.
pub trait SnapshotSink {
    fn persist(&mut self, store: &StateStore, at: BlockNum) -> Result<()>;
}

/// Replay `[from, to]` through the same [`apply_block`] as the live path,
/// then snapshot at `to`. The store's tip must be `from - 1`.
pub async fn backfill<P: Provider, S: SnapshotSink>(
    poll: &mut RpcPoll<P>,
    ctx: &mut ApplyCtx<'_>,
    from: BlockNum,
    to: BlockNum,
    snapshot: &mut S,
) -> Result<()> {
    if from > to {
        return Err(IngestError::Truncated { from, to });
    }
    let expect_tip = from
        .checked_sub(1)
        .ok_or(IngestError::Truncated { from, to })?;
    if ctx.store.tip() != expect_tip {
        return Err(IngestError::BlockGap {
            tip: ctx.store.tip(),
            got: from,
        });
    }

    let mut block = OwnedBlock::with_capacity(256);
    loop {
        match poll.fetch_page().await? {
            Poll::Exhausted => break,
            Poll::Idle => {
                if poll.cursor() > to {
                    break;
                }
            }
            Poll::Ready => {
                while let Poll::Ready = poll.poll_block(&mut block)? {
                    if block.number > to {
                        break;
                    }
                    fold_replay_block(ctx, &block)?;
                }
            }
        }
        if poll.cursor() > to {
            break;
        }
    }
    // eth_getLogs omits log-free blocks. Catch up so tip == `to` after a
    // complete range; apply_block itself stays tip+1-strict (live ExEx).
    catch_up_empty(ctx.store, to)?;
    if ctx.store.tip() != to {
        return Err(IngestError::BlockGap {
            tip: ctx.store.tip(),
            got: to,
        });
    }
    snapshot.persist(ctx.store, to)
}

/// Build a poller over `filters` for `[from, to]` with the default page size.
#[must_use]
pub fn poller<P>(provider: P, filters: &[LogFilter], from: BlockNum, to: BlockNum) -> RpcPoll<P> {
    RpcPoll::new(provider, filters, from, Some(to), DEFAULT_PAGE_BLOCKS)
}

/// Fold already-buffered blocks (no RPC). Test-only: 05F's brief names
/// [`RpcPoll`], not this helper.
#[cfg(test)]
pub(crate) fn backfill_ready(
    src: &mut dyn LogSource,
    ctx: &mut ApplyCtx<'_>,
    to: BlockNum,
    snapshot: &mut dyn SnapshotSink,
) -> Result<()> {
    let mut block = OwnedBlock::with_capacity(256);
    while let Poll::Ready = src.poll_block(&mut block)? {
        if block.number > to {
            break;
        }
        fold_replay_block(ctx, &block)?;
    }
    catch_up_empty(ctx.store, to)?;
    if ctx.store.tip() != to {
        return Err(IngestError::BlockGap {
            tip: ctx.store.tip(),
            got: to,
        });
    }
    snapshot.persist(ctx.store, to)
}

/// Advance `store` over log-free blocks up to and including `through`.
/// `eth_getLogs` never emits those blocks; the live ExEx path must still
/// see a gap as corruption, so this is **not** inside [`apply_block`].
fn catch_up_empty(store: &mut StateStore, through: BlockNum) -> Result<()> {
    loop {
        let tip = store.tip();
        if tip >= through {
            return Ok(());
        }
        let next = tip
            .checked_add(1)
            .ok_or(IngestError::BlockGap { tip, got: through })?;
        store.begin_block(next)?;
    }
}

/// Replay one polled block: fill any preceding log-free numbers, then the
/// strict [`apply_block`] fold.
fn fold_replay_block(ctx: &mut ApplyCtx<'_>, block: &OwnedBlock) -> Result<()> {
    let precede = block.number.checked_sub(1).ok_or(IngestError::BlockGap {
        tip: ctx.store.tip(),
        got: block.number,
    })?;
    catch_up_empty(ctx.store, precede)?;
    apply_block(ctx, block, None)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::{backfill, backfill_ready, SnapshotSink};
    use crate::apply::{ApplyCtx, LogHandler};
    use crate::decode::{encode_v4_supply, DecodeArena, ISpoke};
    use crate::dirty::DirtyAccumulator;
    use crate::router::LogRouter;
    use crate::source::{ExExSource, OwnedBlock, OwnedLog, RpcPoll};
    use alloy_primitives::{Address, Log as ConsensusLog, LogData, U256};
    use alloy_provider::ProviderBuilder;
    use alloy_rpc_types_eth::Log as RpcLog;
    use alloy_sol_types::SolEvent;
    use liq_protocol::{DecodedLog, DirtySet, ProtocolError, StateWriter};
    use liq_state::{StateStore, StoreConfig, UndoCapacity};
    use liq_types::{LogFilter, LogSubscriber};

    struct Rec {
        filters: Vec<LogFilter>,
    }
    impl LogSubscriber for Rec {
        fn subscriptions(&self) -> Vec<LogFilter> {
            self.filters.clone()
        }
    }
    impl LogHandler for Rec {
        fn apply_log(
            &self,
            _st: &mut dyn StateWriter,
            _log: &DecodedLog<'_>,
        ) -> core::result::Result<DirtySet, ProtocolError> {
            Ok(DirtySet::None)
        }
    }

    struct Snap {
        at: Option<u64>,
    }
    impl SnapshotSink for Snap {
        fn persist(&mut self, _store: &StateStore, at: u64) -> crate::Result<()> {
            self.at = Some(at);
            Ok(())
        }
    }

    fn cfg(base: u64) -> StoreConfig {
        StoreConfig {
            base,
            positions: 8,
            markets: 4,
            undo: UndoCapacity {
                ops: 64,
                extras: 8,
                rows: 8,
            },
        }
    }

    fn harness(
        base: u64,
        addr: Address,
    ) -> (Rec, LogRouter, StateStore, DecodeArena, DirtyAccumulator) {
        let rec = Rec {
            filters: vec![LogFilter {
                address: addr,
                topic0: ISpoke::Supply::SIGNATURE_HASH,
            }],
        };
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        (
            rec,
            router,
            StateStore::new(cfg(base)),
            DecodeArena::with_capacity(1 << 16),
            DirtyAccumulator::new(),
        )
    }

    fn to_rpc(log: &OwnedLog) -> RpcLog {
        let data = LogData::new(log.topics.to_vec(), log.data.clone().into()).unwrap();
        RpcLog {
            inner: ConsensusLog {
                address: log.address,
                data,
            },
            block_hash: None,
            block_number: Some(log.block),
            block_timestamp: Some(log.timestamp),
            transaction_hash: None,
            transaction_index: Some(u64::from(log.tx_index)),
            log_index: Some(u64::from(log.log_index)),
            removed: false,
        }
    }

    /// Oracle: GUIDE 03 §5 — replay to the pinned block, then snapshot.
    /// Negative: a store whose tip is not `from - 1` is BlockGap (the
    /// previous "short fold" case is now a successful empty-block catch-up).
    #[test]
    fn backfill_ready_snapshots_at_pinned_block() {
        let a = Address::repeat_byte(5);
        let (rec, router, mut store, mut arena, mut dirty) = harness(0, a);
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let log =
            encode_v4_supply(a, U256::from(1), a, a, U256::from(1), U256::from(1), 1, 1).unwrap();
        let (mut prod, cons) = rtrb::RingBuffer::new(8);
        prod.push(OwnedBlock {
            number: 1,
            timestamp: 1,
            gas_limit: 0,
            gas_used: 0,
            base_fee_per_gas: 0,
            logs: vec![log],
        })
        .unwrap();
        let mut src = ExExSource::new(cons);
        let mut ctx = ApplyCtx {
            store: &mut store,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        let mut snap = Snap { at: None };
        backfill_ready(&mut src, &mut ctx, 1, &mut snap).unwrap();
        assert_eq!(snap.at, Some(1));
        assert_eq!(ctx.store.tip(), 1);
    }

    /// Oracle: GUIDE 03 §5 — tip must be `from - 1` before replay starts.
    /// Negative: snapshot is not taken on a tip mismatch.
    #[tokio::test(flavor = "current_thread")]
    async fn backfill_rejects_wrong_starting_tip() {
        let a = Address::repeat_byte(5);
        let (rec, router, mut store, mut arena, mut dirty) = harness(0, a);
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let mut ctx = ApplyCtx {
            store: &mut store,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        let mut poll = RpcPoll::new(
            ProviderBuilder::new()
                .disable_recommended_fillers()
                .connect_mocked_client(alloy_provider::transport::mock::Asserter::new()),
            &rec.filters,
            9,
            Some(9),
            8,
        );
        let mut snap = Snap { at: None };
        let err = backfill(&mut poll, &mut ctx, 9, 9, &mut snap)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            crate::IngestError::BlockGap { tip: 0, got: 9 }
        ));
        assert_eq!(snap.at, None, "must not snapshot on a tip mismatch");
    }

    /// Oracle: eth_getLogs returns only blocks that contain matching logs.
    /// Blocks 11 and 12 are absent from the page; the store must still
    /// advance through them and snapshot at the pinned `to`. Mutation-red:
    /// removing `catch_up_empty` / `fold_replay_block` yields
    /// `BlockGap { tip: 10, got: 13 }`.
    #[tokio::test(flavor = "current_thread")]
    async fn backfill_crosses_log_free_blocks() {
        let a = Address::repeat_byte(6);
        let (rec, router, mut store, mut arena, mut dirty) = harness(9, a);
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let log10 =
            encode_v4_supply(a, U256::from(1), a, a, U256::from(1), U256::from(1), 10, 10).unwrap();
        let mut log13 =
            encode_v4_supply(a, U256::from(1), a, a, U256::from(2), U256::from(2), 13, 13).unwrap();
        log13.log_index = 1;
        let asserter = alloy_provider::transport::mock::Asserter::new();
        asserter.push_success(&vec![to_rpc(&log10), to_rpc(&log13)]);
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_mocked_client(asserter);
        let mut poll = RpcPoll::new(provider, &rec.filters, 10, Some(13), 2_000);
        let mut ctx = ApplyCtx {
            store: &mut store,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        let mut snap = Snap { at: None };
        backfill(&mut poll, &mut ctx, 10, 13, &mut snap)
            .await
            .unwrap();
        assert_eq!(ctx.store.tip(), 13, "store must walk 11 and 12");
        assert_eq!(snap.at, Some(13));
    }

    /// Oracle: GUIDE 03 §5 — the snapshot is pinned at `to`, and `to` is
    /// chosen by the operator, not by where the last log happens to sit. The
    /// whole tail 11..=13 is log-free here, which is the ordinary case.
    /// Negative: without the catch-up that precedes the snapshot, tip stays
    /// at 10 and this is `BlockGap { tip: 10, got: 13 }` with no snapshot.
    #[tokio::test(flavor = "current_thread")]
    async fn backfill_snapshots_when_the_pinned_tail_is_log_free() {
        let a = Address::repeat_byte(7);
        let (rec, router, mut store, mut arena, mut dirty) = harness(9, a);
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let log10 =
            encode_v4_supply(a, U256::from(1), a, a, U256::from(1), U256::from(1), 10, 10).unwrap();
        let asserter = alloy_provider::transport::mock::Asserter::new();
        asserter.push_success(&vec![to_rpc(&log10)]);
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_mocked_client(asserter);
        let mut poll = RpcPoll::new(provider, &rec.filters, 10, Some(13), 2_000);
        let mut ctx = ApplyCtx {
            store: &mut store,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        let mut snap = Snap { at: None };
        backfill(&mut poll, &mut ctx, 10, 13, &mut snap)
            .await
            .unwrap();
        assert_eq!(ctx.store.tip(), 13, "store must walk the log-free tail");
        assert_eq!(snap.at, Some(13), "snapshot is pinned at `to`, not at 10");
    }
}
