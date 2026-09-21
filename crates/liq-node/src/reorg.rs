//! Reorg + panic containment (GUIDE 03 §4b; RUST-CONV §2.5).
//!
//! `ChainReverted` / `ChainReorged` drive 02A's undo ring. A reorg deeper
//! than the ring is [`liq_state::StateError::ReorgTooDeep`] and **touches
//! nothing**. A panic or `Err` mid-block unwinds the partial block, then
//! [`HaltSink::halt`] — never folds the next block onto corrupt state.

#[cfg(test)]
use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};

use liq_state::{StateError, StateStore};
use liq_types::{HaltReason, HaltScope, HaltSink, ProtocolId};

use crate::apply::{apply_block, ApplyCtx};
use crate::exex::{FinishedUpTo, Notification, NumHash};
use crate::source::OwnedBlock;
use crate::{BlockNum, IngestError, Result};

#[cfg(test)]
thread_local! {
    static INJECT_INCONSISTENT: Cell<bool> = const { Cell::new(false) };
}

/// Next [`unwind_to`] performs a real one-block unwind (when `tip > target`)
/// then returns [`StateError::Inconsistent`] without reaching `target`.
#[cfg(test)]
pub(crate) fn inject_inconsistent_on_next_unwind() {
    INJECT_INCONSISTENT.set(true);
}

fn map_unwind_err(store: &StateStore, target: BlockNum, e: StateError) -> Result<()> {
    match e {
        StateError::ReorgTooDeep { .. } => {
            tracing::error!(
                target,
                tip = store.tip(),
                floor = store.floor(),
                "reorg deeper than undo ring; store not unwound"
            );
        }
        _ => {
            tracing::error!(?e, target, tip = store.tip(), "unwind failed");
        }
    }
    Err(IngestError::State(e))
}

/// Unwind the store to `target` (inclusive). Deep reorg → error, no partial.
pub fn unwind_to(store: &mut StateStore, target: BlockNum) -> Result<()> {
    #[cfg(test)]
    if INJECT_INCONSISTENT.get() {
        INJECT_INCONSISTENT.set(false);
        let tip = store.tip();
        if tip > target {
            let mid = tip.saturating_sub(1);
            if let Err(e) = store.unwind_to(mid) {
                return map_unwind_err(store, target, e);
            }
        }
        tracing::error!(
            target,
            tip = store.tip(),
            "Inconsistent after partial unwind (test seam)"
        );
        return Err(IngestError::State(StateError::Inconsistent));
    }
    match store.unwind_to(target) {
        Ok(()) => Ok(()),
        Err(e) => map_unwind_err(store, target, e),
    }
}

/// Parent of the first reverted block. `first == 0` is refused (no parent).
#[must_use]
pub fn unwind_parent(first: BlockNum) -> Option<BlockNum> {
    first.checked_sub(1)
}

/// Halt every protocol this ingest session tracks.
///
/// **ACCEPTED deviation** from protocol-local halt (GUIDE 14 / RUST-CONV §2.5):
/// the undo ring is **block-granular**. Unwinding a partial block restores every
/// protocol's writes in that block. Halting only the panicking adapter would
/// leave sibling protocols running against a tip that has already been rewound
/// for them (or, if unwind were skipped, against corrupt cells). Reserved for
/// [`HaltReason::AdapterPanic`]. Deep reorg / unwind holes use
/// [`halt_reorg_too_deep`] once at [`HaltScope::Global`] (`RiskGate` already
/// normalizes `ReorgTooDeep` to Global).
pub fn halt_protocols(sink: &dyn HaltSink, ids: &[ProtocolId], reason: HaltReason) {
    for id in ids {
        sink.halt(HaltScope::Protocol(*id), reason);
    }
}

/// Store-wide unwind failure. One Global hit — not a per-protocol loop.
pub fn halt_reorg_too_deep(sink: &dyn HaltSink) {
    sink.halt(HaltScope::Global, HaltReason::ReorgTooDeep);
}

/// Fold one block under `catch_unwind`. On panic or `Err`, unwind to `prev`
/// (the tip before this block) then halt. Returns whether the block is
/// consistent (and therefore eligible for [`FinishedUpTo`]).
pub fn apply_contained(
    ctx: &mut ApplyCtx<'_>,
    block: &OwnedBlock,
    sink: &dyn HaltSink,
    protocols: &[ProtocolId],
) -> Result<bool> {
    let prev = ctx.store.tip();
    let result = catch_unwind(AssertUnwindSafe(|| apply_block(ctx, block, None)));
    match result {
        Ok(Ok(())) => Ok(true),
        Ok(Err(e)) => {
            tracing::error!(?e, block = block.number, "apply_block failed");
            recover_partial(ctx.store, prev, sink, protocols, HaltReason::AdapterPanic)?;
            Ok(false)
        }
        Err(_) => {
            tracing::error!(block = block.number, "adapter panicked mid-block");
            match protocols.first() {
                Some(id) => {
                    metrics::counter!("adapter_panic", "protocol" => format!("{}", id.0))
                        .increment(1);
                }
                None => {
                    metrics::counter!("adapter_panic", "protocol" => "unknown").increment(1);
                }
            }
            recover_partial(ctx.store, prev, sink, protocols, HaltReason::AdapterPanic)?;
            Ok(false)
        }
    }
}

fn recover_partial(
    store: &mut StateStore,
    prev: BlockNum,
    sink: &dyn HaltSink,
    protocols: &[ProtocolId],
    reason: HaltReason,
) -> Result<()> {
    match unwind_to(store, prev) {
        Ok(()) => {
            halt_protocols(sink, protocols, reason);
            Ok(())
        }
        Err(e) => {
            halt_reorg_too_deep(sink);
            Err(e)
        }
    }
}

/// Drive one owned notification. [`FinishedUpTo`] is `Some` only after a
/// committed/reorged chain is fully consistent.
pub fn handle_notification(
    ctx: &mut ApplyCtx<'_>,
    n: &Notification,
    sink: &dyn HaltSink,
    protocols: &[ProtocolId],
) -> Result<Option<FinishedUpTo>> {
    match n {
        Notification::Committed { new } => apply_chain(ctx, new, sink, protocols),
        Notification::Reverted { first, last } => {
            revert(ctx.store, *first, *last, sink, protocols)?;
            Ok(None)
        }
        Notification::Reorged {
            old_first,
            old_last,
            new,
        } => {
            revert(ctx.store, *old_first, *old_last, sink, protocols)?;
            apply_chain(ctx, new, sink, protocols)
        }
    }
}

fn revert(
    store: &mut StateStore,
    first: BlockNum,
    last: NumHash,
    sink: &dyn HaltSink,
    _protocols: &[ProtocolId],
) -> Result<()> {
    let Some(parent) = unwind_parent(first) else {
        let tip = store.tip();
        let floor = store.floor();
        tracing::error!(
            first,
            last = last.number,
            tip,
            floor,
            "revert of block 0 has no parent"
        );
        halt_reorg_too_deep(sink);
        return Err(IngestError::CannotUnwindGenesis { tip, floor });
    };
    match unwind_to(store, parent) {
        Ok(()) => Ok(()),
        Err(e) => {
            halt_reorg_too_deep(sink);
            Err(e)
        }
    }
}

fn apply_chain(
    ctx: &mut ApplyCtx<'_>,
    chain: &crate::exex::OwnedChain,
    sink: &dyn HaltSink,
    protocols: &[ProtocolId],
) -> Result<Option<FinishedUpTo>> {
    if chain.blocks.is_empty() {
        return Ok(None);
    }
    for block in &chain.blocks {
        if !apply_contained(ctx, block, sink, protocols)? {
            return Ok(None);
        }
    }
    if chain.tip.number != ctx.store.tip() {
        tracing::error!(
            claimed = chain.tip.number,
            store_tip = ctx.store.tip(),
            "FinishedUpTo chain.tip does not match store.tip"
        );
        halt_reorg_too_deep(sink);
        return Err(IngestError::BlockGap {
            tip: ctx.store.tip(),
            got: chain.tip.number,
        });
    }
    Ok(Some(FinishedUpTo {
        num_hash: chain.tip,
    }))
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
    use super::{apply_contained, handle_notification, unwind_to};
    use crate::apply::{ApplyCtx, LogHandler};
    use crate::decode::{encode_v4_supply, DecodeArena, ISpoke};
    use crate::dirty::DirtyAccumulator;
    use crate::exex::{Notification, NumHash, OwnedChain};
    use crate::router::LogRouter;
    use crate::source::{OwnedBlock, OwnedLog};
    use alloy_primitives::{Address, B256, U256};
    use alloy_sol_types::SolEvent;
    use liq_protocol::{DecodedLog, DirtySet, MarketRow, ProtocolError, StateWriter};
    use liq_state::{StateError, StateStore, StoreConfig, UndoCapacity, UNDO_DEPTH};
    use liq_types::{
        AssetId, HaltReason, HaltScope, HaltSink, LogFilter, LogSubscriber, MarketId, PositionId,
        PositionKey, ProtocolId,
    };
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    const PROTO: ProtocolId = ProtocolId(1);
    const MKT: MarketId = MarketId(1);

    struct RecHalt {
        hits: AtomicU32,
        last: Mutex<Option<(HaltScope, HaltReason)>>,
    }
    impl RecHalt {
        fn new() -> Self {
            Self {
                hits: AtomicU32::new(0),
                last: Mutex::new(None),
            }
        }
    }
    impl HaltSink for RecHalt {
        fn halt(&self, scope: HaltScope, reason: HaltReason) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            *self.last.lock().unwrap() = Some((scope, reason));
        }
    }

    struct Mut {
        filters: Vec<LogFilter>,
        /// 1-based log index in this handler that panics. `None` = never.
        panic_at: Option<u32>,
        seen: AtomicU32,
    }
    impl LogSubscriber for Mut {
        fn subscriptions(&self) -> Vec<LogFilter> {
            self.filters.clone()
        }
    }
    impl LogHandler for Mut {
        fn apply_log(
            &self,
            st: &mut dyn StateWriter,
            log: &DecodedLog<'_>,
        ) -> core::result::Result<DirtySet, ProtocolError> {
            let n = self.seen.fetch_add(1, Ordering::Relaxed).saturating_add(1);
            if self.panic_at == Some(n) {
                panic!("adapter");
            }
            let key = PositionKey {
                protocol: PROTO,
                market: MKT,
                user: log.address,
            };
            let p = st.intern(&key)?;
            let prev = st.supply(p, 0)?;
            st.set_supply(p, 0, prev.saturating_add(1))?;
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

    fn seeded() -> StateStore {
        let mut st = StateStore::new(cfg());
        st.push_market(MKT, MarketRow::blank(AssetId(1), 18))
            .unwrap();
        st
    }

    fn supply_log(addr: Address, n: u64) -> OwnedLog {
        encode_v4_supply(
            addr,
            U256::from(1),
            addr,
            addr,
            U256::from(n),
            U256::from(n),
            n,
            n,
        )
        .unwrap()
    }

    fn block(n: u64, logs: Vec<OwnedLog>) -> OwnedBlock {
        OwnedBlock {
            number: n,
            timestamp: n,
            gas_limit: 0,
            logs,
        }
    }

    fn tip(n: u64) -> NumHash {
        NumHash {
            number: n,
            hash: B256::repeat_byte(n as u8),
        }
    }

    fn supply_of(st: &StateStore) -> u128 {
        st.view(0)
            .position(PositionId(0))
            .map(|p| p.supply.first().copied().unwrap_or(0))
            .unwrap_or(0)
    }

    /// Oracle: undo(apply(x))==x via the public read path.
    /// Negative: a revert that skips the ring would leave the +1 share.
    #[test]
    fn revert_restores_pre_block_supply() {
        let addr = Address::repeat_byte(9);
        let rec = Mut {
            filters: vec![LogFilter {
                address: addr,
                topic0: ISpoke::Supply::SIGNATURE_HASH,
            }],
            panic_at: None,
            seen: AtomicU32::new(0),
        };
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let mut store = seeded();
        let mut arena = DecodeArena::with_capacity(1 << 16);
        let mut dirty = DirtyAccumulator::new();
        let mut ctx = ApplyCtx {
            store: &mut store,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        let sink = RecHalt::new();
        let ids = [PROTO];
        let b1 = block(1, vec![supply_log(addr, 1)]);
        assert!(apply_contained(&mut ctx, &b1, &sink, &ids).unwrap());
        assert_eq!(supply_of(ctx.store), 1);
        assert_eq!(ctx.store.tip(), 1);
        let n = Notification::Reverted {
            first: 1,
            last: tip(1),
        };
        let fin = handle_notification(&mut ctx, &n, &sink, &ids).unwrap();
        assert!(fin.is_none());
        assert_eq!(ctx.store.tip(), 0);
        assert_eq!(supply_of(ctx.store), 0);
        assert_eq!(sink.hits.load(Ordering::Relaxed), 0);
    }

    /// Oracle: UNDO_DEPTH+1 below floor is ReorgTooDeep and the digest is
    /// unchanged. Negative: a partial unwind would move tip.
    #[test]
    fn deep_reorg_is_reorg_too_deep_and_not_partial() {
        let rec = Mut {
            filters: vec![],
            panic_at: None,
            seen: AtomicU32::new(0),
        };
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let mut store = seeded();
        let mut arena = DecodeArena::with_capacity(64);
        let mut dirty = DirtyAccumulator::new();
        let mut ctx = ApplyCtx {
            store: &mut store,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        let sink = RecHalt::new();
        let ids = [PROTO];
        let last = UNDO_DEPTH as u64 + 1;
        for n in 1..=last {
            let b = block(n, vec![]);
            assert!(apply_contained(&mut ctx, &b, &sink, &ids).unwrap());
        }
        let tip_before = ctx.store.tip();
        let floor = ctx.store.floor();
        assert_eq!(tip_before, last);
        let err = unwind_to(ctx.store, 0).unwrap_err();
        assert_eq!(
            err,
            crate::IngestError::State(StateError::ReorgTooDeep {
                depth: tip_before,
                cap: tip_before.saturating_sub(floor),
            })
        );
        assert_eq!(
            ctx.store.tip(),
            tip_before,
            "deep reorg must not partial-unwind"
        );
        let n = Notification::Reverted {
            first: 1,
            last: tip(last),
        };
        let err = handle_notification(&mut ctx, &n, &sink, &ids).unwrap_err();
        assert!(matches!(
            err,
            crate::IngestError::State(StateError::ReorgTooDeep { .. })
        ));
        assert_eq!(ctx.store.tip(), tip_before);
        assert_eq!(sink.hits.load(Ordering::Relaxed), 1);
        let last_h = *sink.last.lock().unwrap();
        assert_eq!(last_h, Some((HaltScope::Global, HaltReason::ReorgTooDeep)));
    }

    /// Oracle: panic mid-fold → unwind partial block → HaltScope::Protocol.
    /// Negative: leaving the interned share would poison the next block.
    #[test]
    fn catch_unwind_unwinds_and_halts_protocol() {
        let addr = Address::repeat_byte(4);
        let rec = Mut {
            filters: vec![LogFilter {
                address: addr,
                topic0: ISpoke::Supply::SIGNATURE_HASH,
            }],
            panic_at: Some(1),
            seen: AtomicU32::new(0),
        };
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let mut store = seeded();
        let mut arena = DecodeArena::with_capacity(1 << 16);
        let mut dirty = DirtyAccumulator::new();
        let mut ctx = ApplyCtx {
            store: &mut store,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        let sink = RecHalt::new();
        let ids = [PROTO];
        let ok = Mut {
            filters: rec.filters.clone(),
            panic_at: None,
            seen: AtomicU32::new(0),
        };
        let handlers_ok: [&dyn LogHandler; 1] = [&ok];
        let subs_ok: [&dyn LogSubscriber; 1] = [&ok];
        let router_ok = LogRouter::from_subscribers(&subs_ok).unwrap();
        ctx.router = &router_ok;
        ctx.handlers = &handlers_ok;
        let b1 = block(1, vec![supply_log(addr, 1)]);
        assert!(apply_contained(&mut ctx, &b1, &sink, &ids).unwrap());
        assert_eq!(supply_of(ctx.store), 1);
        ctx.router = &router;
        ctx.handlers = &handlers;
        let b2 = block(2, vec![supply_log(addr, 2)]);
        assert!(!apply_contained(&mut ctx, &b2, &sink, &ids).unwrap());
        assert_eq!(ctx.store.tip(), 1, "partial block 2 must be unwound");
        assert_eq!(supply_of(ctx.store), 1);
        assert_eq!(sink.hits.load(Ordering::Relaxed), 1);
        assert_eq!(
            *sink.last.lock().unwrap(),
            Some((HaltScope::Protocol(PROTO), HaltReason::AdapterPanic))
        );
    }

    /// Oracle: FinishedHeight follows consistency, never a failed apply.
    #[test]
    fn finished_height_only_after_consistent_chain() {
        let addr = Address::repeat_byte(2);
        let rec = Mut {
            filters: vec![LogFilter {
                address: addr,
                topic0: ISpoke::Supply::SIGNATURE_HASH,
            }],
            panic_at: None,
            seen: AtomicU32::new(0),
        };
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let mut store = seeded();
        let mut arena = DecodeArena::with_capacity(1 << 16);
        let mut dirty = DirtyAccumulator::new();
        let mut ctx = ApplyCtx {
            store: &mut store,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        let sink = RecHalt::new();
        let ids = [PROTO];
        let n1 = Notification::Committed {
            new: OwnedChain {
                blocks: vec![block(1, vec![supply_log(addr, 1)])],
                tip: tip(1),
            },
        };
        let d1 = handle_notification(&mut ctx, &n1, &sink, &ids)
            .unwrap()
            .unwrap();
        assert_eq!(d1.num_hash.number, 1);
        assert_eq!(ctx.store.tip(), 1);
        let boom = Mut {
            filters: rec.filters.clone(),
            panic_at: Some(1),
            seen: AtomicU32::new(0),
        };
        let handlers_b: [&dyn LogHandler; 1] = [&boom];
        let subs_b: [&dyn LogSubscriber; 1] = [&boom];
        let router_b = LogRouter::from_subscribers(&subs_b).unwrap();
        ctx.router = &router_b;
        ctx.handlers = &handlers_b;
        let n2 = Notification::Committed {
            new: OwnedChain {
                blocks: vec![block(2, vec![supply_log(addr, 2)])],
                tip: tip(2),
            },
        };
        let d2 = handle_notification(&mut ctx, &n2, &sink, &ids).unwrap();
        assert!(d2.is_none());
        assert_eq!(ctx.store.tip(), 1);
    }

    /// Replay of the canonical branch after a depth-1 reorg matches applying
    /// that branch on a fresh store (byte-identical supply + tip).
    #[test]
    fn reorg_depth_1_matches_fresh_replay() {
        let addr = Address::repeat_byte(8);
        let rec = Mut {
            filters: vec![LogFilter {
                address: addr,
                topic0: ISpoke::Supply::SIGNATURE_HASH,
            }],
            panic_at: None,
            seen: AtomicU32::new(0),
        };
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let sink = RecHalt::new();
        let ids = [PROTO];
        let mut live = seeded();
        let mut arena = DecodeArena::with_capacity(1 << 16);
        let mut dirty = DirtyAccumulator::new();
        let mut ctx = ApplyCtx {
            store: &mut live,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        let b1 = block(1, vec![supply_log(addr, 1)]);
        let fork = block(2, vec![supply_log(addr, 2)]);
        let canon = block(2, vec![supply_log(addr, 9)]);
        assert!(apply_contained(&mut ctx, &b1, &sink, &ids).unwrap());
        assert!(apply_contained(&mut ctx, &fork, &sink, &ids).unwrap());
        let n = Notification::Reorged {
            old_first: 2,
            old_last: tip(2),
            new: OwnedChain {
                blocks: vec![canon.clone()],
                tip: tip(2),
            },
        };
        handle_notification(&mut ctx, &n, &sink, &ids)
            .unwrap()
            .unwrap();
        let live_tip = ctx.store.tip();
        let live_s = supply_of(ctx.store);

        let mut fresh = seeded();
        let mut arena2 = DecodeArena::with_capacity(1 << 16);
        let mut dirty2 = DirtyAccumulator::new();
        let mut ctx2 = ApplyCtx {
            store: &mut fresh,
            router: &router,
            handlers: &handlers,
            arena: &mut arena2,
            dirty: &mut dirty2,
        };
        assert!(apply_contained(&mut ctx2, &b1, &sink, &ids).unwrap());
        assert!(apply_contained(&mut ctx2, &canon, &sink, &ids).unwrap());
        assert_eq!(live_tip, ctx2.store.tip());
        assert_eq!(live_s, supply_of(ctx2.store));
    }

    /// D1: Inconsistent mid-unwind still Global-halts once (not per protocol).
    #[test]
    fn inconsistent_mid_unwind_halts_global_once() {
        let rec = Mut {
            filters: vec![],
            panic_at: None,
            seen: AtomicU32::new(0),
        };
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let mut store = seeded();
        let mut arena = DecodeArena::with_capacity(64);
        let mut dirty = DirtyAccumulator::new();
        let mut ctx = ApplyCtx {
            store: &mut store,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        let sink = RecHalt::new();
        let ids = [PROTO, ProtocolId(2)];
        for n in 1..=2 {
            assert!(apply_contained(&mut ctx, &block(n, vec![]), &sink, &ids).unwrap());
        }
        let target_parent = 0;
        super::inject_inconsistent_on_next_unwind();
        let n = Notification::Reverted {
            first: 1,
            last: tip(2),
        };
        let err = handle_notification(&mut ctx, &n, &sink, &ids).unwrap_err();
        assert_eq!(err, crate::IngestError::State(StateError::Inconsistent));
        assert_ne!(ctx.store.tip(), target_parent);
        assert_eq!(sink.hits.load(Ordering::Relaxed), 1);
        assert_eq!(
            *sink.last.lock().unwrap(),
            Some((HaltScope::Global, HaltReason::ReorgTooDeep))
        );
    }

    /// D4: panic after two real writes; unwind restores pre-block cells + tip.
    #[test]
    fn panic_after_two_writes_unwinds_cells_and_tip() {
        let addr = Address::repeat_byte(7);
        let rec = Mut {
            filters: vec![LogFilter {
                address: addr,
                topic0: ISpoke::Supply::SIGNATURE_HASH,
            }],
            panic_at: Some(3),
            seen: AtomicU32::new(0),
        };
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let mut store = seeded();
        let mut arena = DecodeArena::with_capacity(1 << 16);
        let mut dirty = DirtyAccumulator::new();
        let mut ctx = ApplyCtx {
            store: &mut store,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        let sink = RecHalt::new();
        let ids = [PROTO];
        let b1 = block(1, vec![supply_log(addr, 1)]);
        assert!(apply_contained(&mut ctx, &b1, &sink, &ids).unwrap());
        assert_eq!(supply_of(ctx.store), 1);
        rec.seen.store(0, Ordering::Relaxed);
        let b2 = block(
            2,
            vec![
                supply_log(addr, 2),
                supply_log(addr, 3),
                supply_log(addr, 4),
            ],
        );
        assert!(!apply_contained(&mut ctx, &b2, &sink, &ids).unwrap());
        assert_eq!(rec.seen.load(Ordering::Relaxed), 3);
        assert_eq!(ctx.store.tip(), 1, "tip must revert to pre-block");
        assert_eq!(
            supply_of(ctx.store),
            1,
            "logs 1 and 2 writes must be unwound"
        );
        assert_eq!(sink.hits.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn empty_committed_chain_is_none() {
        let rec = Mut {
            filters: vec![],
            panic_at: None,
            seen: AtomicU32::new(0),
        };
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let mut store = seeded();
        let mut arena = DecodeArena::with_capacity(64);
        let mut dirty = DirtyAccumulator::new();
        let mut ctx = ApplyCtx {
            store: &mut store,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        let sink = RecHalt::new();
        let ids = [PROTO];
        let n = Notification::Committed {
            new: OwnedChain {
                blocks: vec![],
                tip: tip(1),
            },
        };
        assert!(handle_notification(&mut ctx, &n, &sink, &ids)
            .unwrap()
            .is_none());
        assert_eq!(ctx.store.tip(), 0);
        assert_eq!(sink.hits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn genesis_revert_is_not_cap_zero() {
        let rec = Mut {
            filters: vec![],
            panic_at: None,
            seen: AtomicU32::new(0),
        };
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let mut store = seeded();
        let mut arena = DecodeArena::with_capacity(64);
        let mut dirty = DirtyAccumulator::new();
        let mut ctx = ApplyCtx {
            store: &mut store,
            router: &router,
            handlers: &handlers,
            arena: &mut arena,
            dirty: &mut dirty,
        };
        let sink = RecHalt::new();
        let ids = [PROTO];
        let floor = ctx.store.floor();
        let tip_n = ctx.store.tip();
        let n = Notification::Reverted {
            first: 0,
            last: tip(0),
        };
        let err = handle_notification(&mut ctx, &n, &sink, &ids).unwrap_err();
        assert_eq!(
            err,
            crate::IngestError::CannotUnwindGenesis { tip: tip_n, floor }
        );
        assert_eq!(sink.hits.load(Ordering::Relaxed), 1);
    }
}
