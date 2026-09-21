//! Sync fold: decode → route → `apply_log` → dirty merge. No `.await`
//! (GUIDE 03 §4, §4b).
//!
//! A mid-fold `Err` leaves the store holding a partially applied block.
//! GUIDE 03 §4b requires an undo-ring unwind before halt. WP 03B owns that
//! unwind for **both** `catch_unwind` panics **and** `apply_block` `Err`
//! — not panic-only.

use liq_protocol::{DecodedLog, DirtySet, ProtocolError, StateWriter};
use liq_state::StateStore;
use liq_types::LogSubscriber;

use crate::decode::DecodeArena;
use crate::dirty::DirtyAccumulator;
use crate::router::{LogRouter, Route};
use crate::source::OwnedBlock;
use crate::{AllocMeter, IngestError, Result};

/// Anything the router can dispatch: protocols today, flash sources in 07A.
pub trait LogHandler: LogSubscriber {
    fn apply_log(
        &self,
        st: &mut dyn StateWriter,
        log: &DecodedLog<'_>,
    ) -> core::result::Result<DirtySet, ProtocolError>;
}

impl<T: liq_protocol::Protocol> LogHandler for T {
    #[inline]
    fn apply_log(
        &self,
        st: &mut dyn StateWriter,
        log: &DecodedLog<'_>,
    ) -> core::result::Result<DirtySet, ProtocolError> {
        liq_protocol::Protocol::apply_log(self, st, log)
    }
}

/// Borrowed pieces of one fold. Lifetimes are the caller's.
pub struct ApplyCtx<'a> {
    pub store: &'a mut StateStore,
    pub router: &'a LogRouter,
    pub handlers: &'a [&'a dyn LogHandler],
    pub arena: &'a mut DecodeArena,
    pub dirty: &'a mut DirtyAccumulator,
}

/// Fold one canonical block. Resets the arena and dirty buffers. `meter`,
/// when present, must not change across the hot loop (AllocMeter seam; D57 /
/// 16A owns `PanicOnAlloc`).
pub fn apply_block(
    ctx: &mut ApplyCtx<'_>,
    block: &OwnedBlock,
    meter: Option<AllocMeter<'_>>,
) -> Result<()> {
    let expect = ctx
        .store
        .tip()
        .checked_add(1)
        .ok_or(IngestError::BlockGap {
            tip: ctx.store.tip(),
            got: block.number,
        })?;
    if block.number != expect {
        return Err(IngestError::BlockGap {
            tip: ctx.store.tip(),
            got: block.number,
        });
    }
    ctx.store.begin_block(block.number)?;
    ctx.arena.reset();
    ctx.dirty.clear();

    let before = meter.map(|m| m());
    for log in &block.logs {
        match ctx.router.route(ctx.arena, log)? {
            Route::Untracked | Route::UnknownTopic => {}
            Route::Hit { decoded, subs, .. } => {
                for &idx in subs {
                    let handler = ctx
                        .handlers
                        .get(usize::from(idx))
                        .ok_or(IngestError::HandlerMissing { idx })?;
                    let set = handler.apply_log(ctx.store, &decoded)?;
                    ctx.dirty.merge(set);
                }
            }
        }
    }
    // The collapse is inside the measured window: it is per-block hot-path
    // work and its output buffers are the ones that must stay pre-sized.
    ctx.dirty.collapse(ctx.store, block.timestamp)?;
    let after = meter.map(|m| m());
    if let (Some(b), Some(a)) = (before, after) {
        if a != b {
            return Err(IngestError::HotPathAlloc);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::{apply_block, ApplyCtx, LogHandler};
    use crate::decode::{encode_v4_supply, DecodeArena, ISpoke};
    use crate::dirty::DirtyAccumulator;
    use crate::router::LogRouter;
    use crate::source::{OwnedBlock, OwnedLog};
    use alloy_primitives::{Address, B256, U256};
    use alloy_sol_types::SolEvent;
    use arrayvec::ArrayVec;
    use core::cell::Cell;
    use liq_protocol::{DecodedLog, DirtySet, ProtocolError, StateWriter};
    use liq_state::{StateStore, StoreConfig, UndoCapacity};
    use liq_types::{LogFilter, LogSubscriber};

    struct Rec {
        filters: Vec<LogFilter>,
        hits: Cell<u32>,
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
            self.hits.set(self.hits.get() + 1);
            Ok(DirtySet::None)
        }
    }

    fn cfg() -> StoreConfig {
        StoreConfig {
            base: 0,
            positions: 8,
            markets: 4,
            undo: UndoCapacity {
                ops: 64,
                extras: 8,
                rows: 8,
            },
        }
    }

    /// Oracle: GUIDE 03 §4 fold + STATE.md unknown-topic skip. Negative: an
    /// unknown topic at a tracked address must not call apply_log and must
    /// not return Err.
    #[test]
    fn fold_dispatches_supply_and_skips_unknown_topic() {
        let a = Address::repeat_byte(7);
        let rec = Rec {
            filters: vec![LogFilter {
                address: a,
                topic0: ISpoke::Supply::SIGNATURE_HASH,
            }],
            hits: Cell::new(0),
        };
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let mut store = StateStore::new(cfg());
        let mut arena = DecodeArena::with_capacity(1 << 16);
        let mut dirty = DirtyAccumulator::new();
        let supply =
            encode_v4_supply(a, U256::from(1), a, a, U256::from(2), U256::from(3), 1, 10).unwrap();
        let mut topics = ArrayVec::new();
        topics.push(B256::from(alloy_primitives::keccak256(
            b"DelegateChanged(address,address,uint256)",
        )));
        let unknown = OwnedLog {
            address: a,
            topics,
            data: Vec::new(),
            block: 1,
            timestamp: 10,
            tx_index: 0,
            log_index: 1,
        };
        let block = OwnedBlock {
            number: 1,
            timestamp: 10,
            gas_limit: 0,
            gas_used: 0,
            base_fee_per_gas: 0,
            logs: vec![supply, unknown],
        };
        let mut ctx = ApplyCtx {
            store: &mut store,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        apply_block(&mut ctx, &block, None).unwrap();
        assert_eq!(rec.hits.get(), 1);
        assert_eq!(router.unknown_topic_skips(), 1);
        // The fold collapses at the end of the block; the engine reads that
        // result rather than collapsing a second time (GUIDE 03 §4).
        let collapsed = ctx.dirty.collapsed();
        assert!(!collapsed.protocol_wide);
        assert!(collapsed.reprices.is_empty());
        assert!(collapsed.accruals.is_empty());
        assert!(collapsed.positions.is_empty());
    }

    /// AllocMeter seam (D57): a meter that reports a delta fails closed.
    /// Oracle: the meter value, not the code under test. Negative: a lying
    /// "always zero" meter is distinguished by `alloc_metered` — this test
    /// supplies a meter that ticks, so HotPathAlloc is required.
    #[test]
    fn meter_delta_is_hot_path_alloc() {
        let a = Address::repeat_byte(3);
        let rec = Rec {
            filters: vec![LogFilter {
                address: a,
                topic0: ISpoke::Supply::SIGNATURE_HASH,
            }],
            hits: Cell::new(0),
        };
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let mut store = StateStore::new(cfg());
        let mut arena = DecodeArena::with_capacity(1 << 16);
        let mut dirty = DirtyAccumulator::new();
        let supply =
            encode_v4_supply(a, U256::from(1), a, a, U256::from(1), U256::from(1), 1, 1).unwrap();
        let block = OwnedBlock {
            number: 1,
            timestamp: 1,
            gas_limit: 0,
            gas_used: 0,
            base_fee_per_gas: 0,
            logs: vec![supply],
        };
        let mut ctx = ApplyCtx {
            store: &mut store,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        let n = Cell::new(0u64);
        let meter = || {
            let v = n.get();
            n.set(v + 1);
            v
        };
        let err = apply_block(&mut ctx, &block, Some(&meter)).unwrap_err();
        assert_eq!(err, crate::IngestError::HotPathAlloc);
    }

    /// Zero-alloc proof: after warmup, a counting meter that is stable across
    /// the hot loop stays stable, and the bump arena does not grow.
    /// Oracle: bumpalo `allocated_bytes` (arena) + caller meter (D57 seam).
    #[test]
    fn decode_route_fold_zero_alloc_after_warmup() {
        let a = Address::repeat_byte(4);
        let rec = Rec {
            filters: vec![LogFilter {
                address: a,
                topic0: ISpoke::Supply::SIGNATURE_HASH,
            }],
            hits: Cell::new(0),
        };
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let mut store = StateStore::new(cfg());
        let mut arena = DecodeArena::with_capacity(1 << 20);
        let mut dirty = DirtyAccumulator::new();
        let mk = |n: u64| {
            encode_v4_supply(a, U256::from(1), a, a, U256::from(n), U256::from(n), n, n).unwrap()
        };
        let b1 = OwnedBlock {
            number: 1,
            timestamp: 1,
            gas_limit: 0,
            gas_used: 0,
            base_fee_per_gas: 0,
            logs: vec![mk(1), mk(1)],
        };
        let mut ctx = ApplyCtx {
            store: &mut store,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        apply_block(&mut ctx, &b1, None).unwrap();
        let high = ctx.arena.allocated_bytes();
        let b2 = OwnedBlock {
            number: 2,
            timestamp: 2,
            gas_limit: 0,
            gas_used: 0,
            base_fee_per_gas: 0,
            logs: vec![mk(2), mk(2), mk(2)],
        };
        let meter = || 0u64;
        apply_block(&mut ctx, &b2, Some(&meter)).unwrap();
        assert_eq!(
            ctx.arena.allocated_bytes(),
            high,
            "arena must not grow after warmup"
        );
        assert!(rec.hits.get() >= 5);
    }
}
