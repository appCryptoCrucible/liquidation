//! Undo ring (GUIDE 02 §5): the inverse of every mutation of the last
//! [`UNDO_DEPTH`] blocks, pre-reserved at startup so a busy block allocates
//! nothing.
//!
//! The ring holds one [`BlockUndo`] per block in `(floor, tip]`. `floor` is
//! the lowest block the store can be unwound **to**: initially the base block
//! the store was constructed at (a snapshot or the empty store — there is no
//! record to revert *it*), afterwards the block of the most recently evicted
//! record. A reorg below `floor` is [`StateError::ReorgTooDeep`] and unwinds
//! nothing.
//!
//! **Record shape.** [`UndoOp`] is 32 bytes: the two balance inverses carry
//! the previous cell and the previous mask *bit* (a `set_supply`/`set_debt`
//! changes at most that one bit, so the bit is the exact inverse of the whole
//! mask — TESTING §4 mutation #8 is "drop `prev_set`"). The 64-byte
//! `PositionExtraRepr` (per position and per slot) and 256-byte `MarketRow`
//! inverses live in per-block side tables, pushed and popped in the same
//! LIFO order as their marker op, so a block of balance updates does not pay
//! 320 bytes per op.

use liq_protocol::{BlockNum, MarketRow, MarketSlot, PositionExtraRepr};
use liq_types::{MarketId, PositionId};

use crate::error::StateError;

/// Blocks of history the ring holds. A reorg deeper than this is a halt.
pub const UNDO_DEPTH: usize = 128;

/// Inverse of one mutation. Applied newest-first on unwind.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UndoOp {
    /// `set_supply`: restore the cell and the position's mask bit for `slot`.
    Supply {
        pos: PositionId,
        slot: u16,
        /// Whether `slot` was set in the position's `config` before the write.
        prev_set: bool,
        prev: u128,
    },
    /// `set_debt`: as [`UndoOp::Supply`].
    Debt {
        pos: PositionId,
        slot: u16,
        prev_set: bool,
        prev: u128,
    },
    /// `set_extra`: the previous repr is the next pop of [`BlockUndo::extras`].
    Extra { pos: PositionId },
    /// `set_slot_extra`: the previous repr is the next pop of
    /// [`BlockUndo::extras`] (same side table, same LIFO discipline).
    SlotExtra { pos: PositionId, slot: u16 },
    /// `set_market`: the previous row is the next pop of [`BlockUndo::rows`].
    Market { at: MarketSlot },
    /// `intern` assigned a new id: drop the position (it is the newest).
    Created { pos: PositionId },
    /// `push_market` appended a slot; `created` when the market itself was
    /// created by that push and must go too.
    Pushed { market: MarketId, created: bool },
}

const _: () = assert!(core::mem::size_of::<UndoOp>() == 32);

/// Per-block reservation. Exceeding any of these grows the `Vec` (correct,
/// counted in [`UndoRing::overflows`], and shrunk back when the record is
/// reused) — the allocation-free guarantee holds up to these numbers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct UndoCapacity {
    /// [`UndoOp`]s per block.
    pub ops: usize,
    /// `set_extra` inverses per block.
    pub extras: usize,
    /// `set_market` inverses per block.
    pub rows: usize,
}

/// Every inverse of one block.
#[derive(Debug, Default)]
pub struct BlockUndo {
    pub(crate) ops: Vec<UndoOp>,
    pub(crate) extras: Vec<PositionExtraRepr>,
    pub(crate) rows: Vec<MarketRow>,
}

impl BlockUndo {
    fn with_capacity(c: UndoCapacity) -> Self {
        Self {
            ops: Vec::with_capacity(c.ops),
            extras: Vec::with_capacity(c.extras),
            rows: Vec::with_capacity(c.rows),
        }
    }

    /// Empty the record for reuse, keeping its reservation (and giving back
    /// anything a freak block grew beyond it).
    fn clear(&mut self, c: UndoCapacity) {
        self.ops.clear();
        self.extras.clear();
        self.rows.clear();
        if self.ops.capacity() > c.ops {
            self.ops.shrink_to(c.ops);
        }
        if self.extras.capacity() > c.extras {
            self.extras.shrink_to(c.extras);
        }
        if self.rows.capacity() > c.rows {
            self.rows.shrink_to(c.rows);
        }
    }
}

/// Fixed-size ring of [`BlockUndo`] records plus the block bookkeeping
/// (`tip`, `floor`) the store exposes.
pub struct UndoRing {
    blocks: Box<[BlockUndo; UNDO_DEPTH]>,
    cap: UndoCapacity,
    /// Slot of the record for `tip` (meaningful when `tip > floor`).
    head: usize,
    tip: BlockNum,
    floor: BlockNum,
    overflows: u64,
}

impl UndoRing {
    pub(crate) fn new(base: BlockNum, cap: UndoCapacity) -> Self {
        Self {
            blocks: Box::new(core::array::from_fn(|_| BlockUndo::with_capacity(cap))),
            cap,
            head: 0,
            tip: base,
            floor: base,
            overflows: 0,
        }
    }

    #[inline]
    pub(crate) fn tip(&self) -> BlockNum {
        self.tip
    }

    #[inline]
    pub(crate) fn floor(&self) -> BlockNum {
        self.floor
    }

    #[inline]
    pub(crate) fn overflows(&self) -> u64 {
        self.overflows
    }

    /// Records held: blocks that can be unwound right now.
    #[inline]
    fn depth(&self) -> u64 {
        self.tip.saturating_sub(self.floor)
    }

    /// Open the record for `block`, which must be `tip + 1`. Evicts the oldest
    /// record when the ring is full (raising `floor`).
    pub(crate) fn begin(&mut self, block: BlockNum) -> Result<(), StateError> {
        let gap = StateError::BlockGap {
            tip: self.tip,
            got: block,
        };
        if self.tip.checked_add(1) != Some(block) {
            return Err(gap);
        }
        self.head = self.head.checked_add(1).map_or(0, wrap);
        self.tip = block;
        let k = UNDO_DEPTH as u64;
        if self.depth() > k {
            self.floor = self.tip.saturating_sub(k);
        }
        if let Some(rec) = self.blocks.get_mut(self.head) {
            rec.clear(self.cap);
        }
        Ok(())
    }

    /// The open record — `None` while no block is open (the loading phase
    /// before the first `begin`, or right after a full unwind), in which case
    /// mutations are unjournaled and become part of the floor state.
    #[inline]
    fn current(&mut self) -> Option<&mut BlockUndo> {
        if self.depth() == 0 {
            return None;
        }
        self.blocks.get_mut(self.head)
    }

    #[inline]
    pub(crate) fn push(&mut self, op: UndoOp) {
        let Some(rec) = self.current() else {
            return;
        };
        let full = rec.ops.len() == rec.ops.capacity();
        rec.ops.push(op);
        if full {
            self.overflows = self.overflows.saturating_add(1);
        }
    }

    #[inline]
    pub(crate) fn push_extra(&mut self, pos: PositionId, prev: PositionExtraRepr) {
        let Some(rec) = self.current() else {
            return;
        };
        let full = rec.ops.len() == rec.ops.capacity() || rec.extras.len() == rec.extras.capacity();
        rec.ops.push(UndoOp::Extra { pos });
        rec.extras.push(prev);
        if full {
            self.overflows = self.overflows.saturating_add(1);
        }
    }

    #[inline]
    pub(crate) fn push_slot_extra(&mut self, pos: PositionId, slot: u16, prev: PositionExtraRepr) {
        let Some(rec) = self.current() else {
            return;
        };
        let full = rec.ops.len() == rec.ops.capacity() || rec.extras.len() == rec.extras.capacity();
        rec.ops.push(UndoOp::SlotExtra { pos, slot });
        rec.extras.push(prev);
        if full {
            self.overflows = self.overflows.saturating_add(1);
        }
    }

    #[inline]
    pub(crate) fn push_market(&mut self, at: MarketSlot, prev: MarketRow) {
        let Some(rec) = self.current() else {
            return;
        };
        let full = rec.ops.len() == rec.ops.capacity() || rec.rows.len() == rec.rows.capacity();
        rec.ops.push(UndoOp::Market { at });
        rec.rows.push(prev);
        if full {
            self.overflows = self.overflows.saturating_add(1);
        }
    }

    /// Ring slot holding `block`'s record; `None` outside `(floor, tip]`.
    pub(crate) fn slot_of(&self, block: BlockNum) -> Option<usize> {
        if block <= self.floor || block > self.tip {
            return None;
        }
        let back = usize::try_from(self.tip.checked_sub(block)?).ok()?;
        Some(wrap(
            self.head.checked_add(UNDO_DEPTH)?.checked_sub(wrap(back))?,
        ))
    }

    /// Move `slot`'s record out (leaving an empty, capacity-less one) so the
    /// store can apply it without holding a borrow on the ring.
    #[inline]
    pub(crate) fn take(&mut self, slot: usize) -> Option<BlockUndo> {
        self.blocks.get_mut(slot).map(core::mem::take)
    }

    /// Return a record taken with [`Self::take`], cleared for reuse.
    #[inline]
    pub(crate) fn put(&mut self, slot: usize, mut rec: BlockUndo) {
        rec.clear(self.cap);
        if let Some(r) = self.blocks.get_mut(slot) {
            *r = rec;
        }
    }

    /// Set `tip = target` after the records in `(target, tip]` were applied.
    pub(crate) fn rewind(&mut self, target: BlockNum) -> Result<(), StateError> {
        let back = usize::try_from(
            self.tip
                .checked_sub(target)
                .ok_or(StateError::Inconsistent)?,
        )
        .map_err(|_| StateError::Inconsistent)?;
        self.head = wrap(
            self.head
                .checked_add(UNDO_DEPTH)
                .and_then(|h| h.checked_sub(wrap(back)))
                .ok_or(StateError::Inconsistent)?,
        );
        self.tip = target;
        Ok(())
    }
}

/// `i mod UNDO_DEPTH`. `UNDO_DEPTH` is a nonzero constant, so `checked_rem`
/// never yields `None`.
#[inline]
const fn wrap(i: usize) -> usize {
    match i.checked_rem(UNDO_DEPTH) {
        Some(r) => r,
        None => 0,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{UndoCapacity, UndoRing, UNDO_DEPTH};
    use crate::error::StateError;

    const CAP: UndoCapacity = UndoCapacity {
        ops: 8,
        extras: 2,
        rows: 2,
    };

    /// Oracle: GUIDE 02 §5 — the ring covers exactly the last `K` blocks; the
    /// floor rises by one per block once full; a gap is refused. Negative:
    /// `begin(tip + 2)` fails and changes nothing.
    #[test]
    fn floor_tracks_eviction_and_gaps_are_refused() {
        let mut r = UndoRing::new(1_000, CAP);
        assert_eq!((r.tip(), r.floor()), (1_000, 1_000));
        assert_eq!(r.slot_of(1_000), None, "the base block has no record");
        for b in 1_001..=1_000 + UNDO_DEPTH as u64 {
            r.begin(b).unwrap();
            assert_eq!(r.floor(), 1_000, "not full yet: floor is the base");
        }
        for b in 1_001..=1_000 + UNDO_DEPTH as u64 {
            assert!(r.slot_of(b).is_some());
        }
        r.begin(1_001 + UNDO_DEPTH as u64).unwrap();
        assert_eq!(r.floor(), 1_001, "one eviction, floor + 1");
        assert_eq!(r.slot_of(1_001), None, "evicted");
        assert!(r.slot_of(1_002).is_some());
        assert_eq!(
            r.begin(r.tip() + 2),
            Err(StateError::BlockGap {
                tip: 1_001 + UNDO_DEPTH as u64,
                got: 1_003 + UNDO_DEPTH as u64,
            })
        );
        assert_eq!(
            r.tip(),
            1_001 + UNDO_DEPTH as u64,
            "refused begin is a no-op"
        );
    }

    /// Oracle: definition of a ring — `slot_of` is injective over the held
    /// window, and `rewind` puts `head` where a subsequent `begin` reuses the
    /// next slot without colliding with a surviving record.
    #[test]
    fn slots_are_distinct_and_rewind_realigns_head() {
        let mut r = UndoRing::new(0, CAP);
        for b in 1..=(UNDO_DEPTH as u64 + 5) {
            r.begin(b).unwrap();
        }
        let mut seen = std::collections::HashSet::new();
        for b in (r.floor() + 1)..=r.tip() {
            assert!(seen.insert(r.slot_of(b).unwrap()));
        }
        assert_eq!(seen.len(), UNDO_DEPTH);
        let keep = r.tip() - 3;
        let keep_slot = r.slot_of(keep).unwrap();
        r.rewind(keep).unwrap();
        assert_eq!(r.tip(), keep);
        assert_eq!(
            r.slot_of(keep),
            Some(keep_slot),
            "surviving record keeps its slot"
        );
        r.begin(keep + 1).unwrap();
        assert_ne!(r.slot_of(keep + 1), Some(keep_slot));
    }
}
