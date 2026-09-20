//! Time-cross heap (GUIDE 08 §3): positions that cross from accrual alone.
//!
//! `Protocol::time_to_cross` gives the instant at which base rate **plus**
//! any per-position premium brings `hf` to `1.0` with prices fixed; the
//! engine pushes `(t*, id)` and [`TimeCrossHeap::crossed`] pops everything
//! due at the next block's time. A plain `BinaryHeap` — single-threaded,
//! nothing exotic warranted.
//!
//! **Generations, not removal.** Every push supersedes the position's
//! earlier entry by bumping its generation; the old entry stays in the heap
//! and is discarded when it surfaces. An eager rebuild would be O(N) on
//! every rate update, and a reorg that unwinds the state a `t*` was derived
//! from is handled the same way: the position is recomputed and re-pushed,
//! and whatever the unwound block had pushed is stale by generation.
//!
//! Stale entries are bounded: when the heap holds more than twice the
//! tracked universe (plus slack) at least half of it is stale and it is
//! compacted in one `retain` pass — amortised O(1) per push.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use liq_protocol::Timestamp;
use liq_types::PositionId;

/// Sentinel in `at`: no live entry for this position.
const NONE: Timestamp = Timestamp::MAX;
/// Compaction slack on top of `2 · universe`.
const SLACK: usize = 1024;

/// Min-heap of `(crossing time, position, generation)` with lazy deletion.
#[derive(Clone, Debug)]
pub struct TimeCrossHeap {
    heap: BinaryHeap<Reverse<(Timestamp, PositionId, u32)>>,
    gen: Vec<u32>,
    /// Crossing time of the live entry, or [`NONE`].
    at: Vec<Timestamp>,
    stale_pops: u64,
    compactions: u64,
}

impl TimeCrossHeap {
    /// Room for `positions` live entries without growing.
    #[must_use]
    pub fn with_capacity(positions: usize) -> Self {
        Self {
            heap: BinaryHeap::with_capacity(positions.saturating_mul(2).saturating_add(SLACK)),
            gen: Vec::with_capacity(positions),
            at: Vec::with_capacity(positions),
            stale_pops: 0,
            compactions: 0,
        }
    }

    /// Entries held, live and stale.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Stale entries discarded on pop so far (telemetry).
    #[inline]
    #[must_use]
    pub fn stale_pops(&self) -> u64 {
        self.stale_pops
    }

    /// Compaction passes so far (telemetry).
    #[inline]
    #[must_use]
    pub fn compactions(&self) -> u64 {
        self.compactions
    }

    /// Live crossing time of `id`, if scheduled.
    #[inline]
    #[must_use]
    pub fn scheduled(&self, id: PositionId) -> Option<Timestamp> {
        self.at.get(id.0 as usize).copied().filter(|&t| t != NONE)
    }

    fn ensure(&mut self, id: PositionId) {
        let i = id.0 as usize;
        if i >= self.gen.len() {
            let n = i.saturating_add(1);
            self.gen.resize(n, 0);
            self.at.resize(n, NONE);
        }
    }

    /// Bump `id`'s generation: its live entry (if any) becomes stale.
    #[inline]
    fn bump(&mut self, id: PositionId) -> u32 {
        let i = id.0 as usize;
        let g = match self.gen.get_mut(i) {
            Some(g) => {
                *g = g.wrapping_add(1);
                *g
            }
            None => 0,
        };
        if let Some(a) = self.at.get_mut(i) {
            *a = NONE;
        }
        g
    }

    /// Schedule `id` at `at`, superseding any earlier entry. A push at the
    /// already-live time is a no-op (no churn for an unchanged `t*`).
    pub fn push(&mut self, at: Timestamp, id: PositionId) {
        self.ensure(id);
        if self.scheduled(id) == Some(at) {
            return;
        }
        let gen = self.bump(id);
        if let Some(a) = self.at.get_mut(id.0 as usize) {
            *a = at;
        }
        self.heap.push(Reverse((at, id, gen)));
        let bound = self.gen.len().saturating_mul(2).saturating_add(SLACK);
        if self.heap.len() > bound {
            self.compact();
        }
    }

    /// Drop `id`'s scheduled crossing (no debt, unfundable, untracked).
    pub fn invalidate(&mut self, id: PositionId) {
        self.ensure(id);
        if self.scheduled(id).is_some() {
            self.bump(id);
        }
    }

    /// Positions whose live crossing time is `<= now`, earliest first,
    /// popped as they are yielded. Borrows the heap; allocates nothing.
    #[inline]
    pub fn crossed(&mut self, now: Timestamp) -> Crossed<'_> {
        Crossed { heap: self, now }
    }

    fn compact(&mut self) {
        let gen = &self.gen;
        self.heap
            .retain(|Reverse((_, id, g))| gen.get(id.0 as usize).copied() == Some(*g));
        self.compactions = self.compactions.saturating_add(1);
    }
}

/// See [`TimeCrossHeap::crossed`].
#[derive(Debug)]
pub struct Crossed<'a> {
    heap: &'a mut TimeCrossHeap,
    now: Timestamp,
}

impl Iterator for Crossed<'_> {
    type Item = PositionId;

    fn next(&mut self) -> Option<PositionId> {
        loop {
            let &Reverse((t, id, g)) = self.heap.heap.peek()?;
            if t > self.now {
                return None;
            }
            self.heap.heap.pop();
            let i = id.0 as usize;
            if self.heap.gen.get(i).copied() == Some(g) {
                if let Some(a) = self.heap.at.get_mut(i) {
                    *a = NONE;
                }
                return Some(id);
            }
            self.heap.stale_pops = self.heap.stale_pops.saturating_add(1);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{TimeCrossHeap, SLACK};
    use liq_types::PositionId;

    /// Oracle: the heap contract — a superseded entry never fires, a
    /// dropped entry never fires, the live one fires exactly once at the
    /// first `now >= t*`, earliest first. Negative: nothing fires early.
    #[test]
    fn supersede_invalidate_and_fire_once() {
        let mut h = TimeCrossHeap::with_capacity(8);
        h.push(100, PositionId(1));
        h.push(50, PositionId(2));
        h.push(70, PositionId(1)); // supersedes 100
        h.push(60, PositionId(3));
        h.invalidate(PositionId(3));
        assert_eq!(h.scheduled(PositionId(1)), Some(70));
        assert_eq!(h.scheduled(PositionId(3)), None);
        assert_eq!(h.crossed(49).count(), 0, "nothing due yet");
        let due: Vec<u32> = h.crossed(69).map(|p| p.0).collect();
        assert_eq!(due, vec![2]);
        let due: Vec<u32> = h.crossed(1_000).map(|p| p.0).collect();
        assert_eq!(due, vec![1], "the stale 100 and the dropped 60 never fire");
        assert_eq!(h.crossed(u64::MAX).count(), 0, "fired once");
        assert_eq!(h.stale_pops(), 2, "60 (dropped) and 100 (superseded)");
        assert_eq!(h.scheduled(PositionId(1)), None);
        // Same-time re-push is a no-op; a different time supersedes.
        h.push(5, PositionId(4));
        let n = h.len();
        h.push(5, PositionId(4));
        assert_eq!(h.len(), n);
        h.push(6, PositionId(4));
        assert_eq!(h.len(), n + 1);
        assert_eq!(h.crossed(5).count(), 0);
        assert_eq!(h.crossed(6).map(|p| p.0).collect::<Vec<_>>(), vec![4]);
    }

    /// Oracle: the stated bound — the heap never exceeds `2·universe +
    /// SLACK` entries however often the same positions are re-pushed, and
    /// every compaction keeps exactly the live set.
    #[test]
    fn churn_is_bounded_by_compaction() {
        let mut h = TimeCrossHeap::with_capacity(4);
        let universe = 4u32;
        for round in 0..2_000u64 {
            for i in 0..universe {
                h.push(round + u64::from(i), PositionId(i));
            }
            assert!(h.len() <= 2 * universe as usize + SLACK, "round {round}");
        }
        assert!(h.compactions() > 0);
        let due: Vec<u32> = h.crossed(u64::MAX).map(|p| p.0).collect();
        assert_eq!(due.len(), universe as usize, "one live entry per position");
        let mut sorted = due.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1, 2, 3]);
        assert!(h.is_empty() || h.crossed(u64::MAX).count() == 0);
    }
}
