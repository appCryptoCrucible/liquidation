//! [`DirtyAccumulator`]: collapse Reprice ⊃ Accrual ⊃ Positions per market
//! via [`liq_protocol::DirtySet::rank`] (GUIDE 03 §4). Buffers reused.

use std::collections::HashSet;

use liq_protocol::{DirtyPositions, DirtyRows, DirtySet};
use liq_state::StateStore;
use liq_types::{MarketId, PositionId};

use crate::{IngestError, Result, Timestamp};

/// Reused output of one block's collapse.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CollapsedDirty {
    pub protocol_wide: bool,
    pub reprices: DirtyRows,
    pub accruals: DirtyRows,
    pub positions: DirtyPositions,
}

impl CollapsedDirty {
    pub fn clear(&mut self) {
        self.protocol_wide = false;
        self.reprices.clear();
        self.accruals.clear();
        self.positions.clear();
    }
}

/// Per-block dirty-set fold. `clear` between blocks; never reallocate after
/// the high-water mark.
pub struct DirtyAccumulator {
    protocol_wide: bool,
    markets: Vec<MarketId>,
    ranks: Vec<u8>,
    rows: Vec<DirtyRows>,
    positions: DirtyPositions,
    /// Dedup for [`Self::merge`] of `Positions`. Linear `contains` on the
    /// `SmallVec` is O(P²) in unique positions per block; a reserved set is
    /// O(P) and does not allocate after the high-water mark. Bound: P unique
    /// positions dirtied in the block (one Aave log ≤ 8; a busy block is
    /// hundreds, not unbounded).
    seen: HashSet<PositionId>,
    out: CollapsedDirty,
}

impl DirtyAccumulator {
    #[must_use]
    pub fn with_capacity(markets: usize, positions: usize) -> Self {
        Self {
            protocol_wide: false,
            markets: Vec::with_capacity(markets),
            ranks: Vec::with_capacity(markets),
            rows: Vec::with_capacity(markets),
            positions: DirtyPositions::with_capacity(positions),
            seen: HashSet::with_capacity(positions),
            out: CollapsedDirty::default(),
        }
    }

    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(32, 64)
    }

    /// Drop contents, keep allocations. `rows` keeps its per-market
    /// [`DirtyRows`] slots — clearing the outer `Vec` would drop them and
    /// free any that had spilled past `SmallVec`'s inline capacity, so the
    /// next block would re-allocate. `markets.len()` is the live prefix.
    pub fn clear(&mut self) {
        self.protocol_wide = false;
        self.markets.clear();
        self.ranks.clear();
        for r in &mut self.rows {
            r.clear();
        }
        self.positions.clear();
        self.seen.clear();
        self.out.clear();
    }

    /// Merge one adapter's report. Wider rank subsumes narrower for the same
    /// market; never the reverse.
    pub fn merge(&mut self, set: DirtySet) {
        match set {
            DirtySet::None => {}
            DirtySet::ProtocolWide => {
                self.protocol_wide = true;
            }
            DirtySet::MarketReprice(rows) => {
                self.merge_rows(DirtySet::MarketReprice(DirtyRows::new()).rank(), rows)
            }
            DirtySet::MarketAccrual(rows) => {
                self.merge_rows(DirtySet::MarketAccrual(DirtyRows::new()).rank(), rows)
            }
            DirtySet::Positions(ps) => {
                if self.protocol_wide {
                    return;
                }
                for p in ps {
                    if self.seen.insert(p) {
                        self.positions.push(p);
                    }
                }
            }
        }
    }

    fn merge_rows(&mut self, rank: u8, rows: DirtyRows) {
        if self.protocol_wide {
            return;
        }
        for slot in rows {
            let i = self.find_or_insert(slot.market);
            let Some(existing) = self.ranks.get_mut(i) else {
                continue;
            };
            if rank > *existing {
                *existing = rank;
            }
            if let Some(set) = self.rows.get_mut(i) {
                if !set.contains(&slot) {
                    set.push(slot);
                }
            }
        }
    }

    fn find_or_insert(&mut self, market: MarketId) -> usize {
        if let Some(i) = self.markets.iter().position(|&m| m == market) {
            return i;
        }
        let i = self.markets.len();
        self.markets.push(market);
        self.ranks.push(0);
        if self.rows.len() <= i {
            self.rows.push(DirtyRows::new());
        }
        i
    }

    /// Collapse using [`DirtySet::rank`]. Positions whose market is already
    /// accruing or repricing are dropped. Accrual on a market that also has
    /// reprice is dropped. ProtocolWide suppresses everything narrower.
    pub fn collapse(
        &mut self,
        store: &StateStore,
        timestamp: Timestamp,
    ) -> Result<&CollapsedDirty> {
        self.out.clear();
        if self.protocol_wide {
            self.out.protocol_wide = true;
            return Ok(&self.out);
        }
        let view = store.view(timestamp);
        for (i, &rank) in self.ranks.iter().enumerate() {
            let Some(rows) = self.rows.get(i) else {
                continue;
            };
            if rank == DirtySet::MarketReprice(DirtyRows::new()).rank() {
                for r in rows {
                    self.out.reprices.push(*r);
                }
            } else if rank == DirtySet::MarketAccrual(DirtyRows::new()).rank() {
                for r in rows {
                    self.out.accruals.push(*r);
                }
            }
        }
        for &pos in &self.positions {
            let pref = view
                .position(pos)
                .map_err(|_| IngestError::UnknownPosition(pos))?;
            let market = pref.key.market;
            let covered = self
                .markets
                .iter()
                .position(|&m| m == market)
                .is_some_and(|i| {
                    self.ranks
                        .get(i)
                        .copied()
                        .is_some_and(|r| r >= DirtySet::MarketAccrual(DirtyRows::new()).rank())
                });
            if !covered {
                self.out.positions.push(pos);
            }
        }
        Ok(&self.out)
    }

    /// The last [`Self::collapse`] output. [`crate::apply_block`] collapses at
    /// the end of every block, so the engine (GUIDE 03 §4 `flush_to_engine`)
    /// reads the result here instead of paying for a second collapse.
    #[inline]
    #[must_use]
    pub fn collapsed(&self) -> &CollapsedDirty {
        &self.out
    }
}

impl Default for DirtyAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience: emit collapsed sets as the [`DirtySet`] variants the engine
/// consumes (GUIDE 03 §4 `flush_to_engine`).
pub fn as_dirty_sets(c: &CollapsedDirty) -> impl Iterator<Item = DirtySet> {
    let wide = c.protocol_wide.then_some(DirtySet::ProtocolWide);
    let reprice = (!c.reprices.is_empty()).then(|| DirtySet::MarketReprice(c.reprices.clone()));
    let accrual = (!c.accruals.is_empty()).then(|| DirtySet::MarketAccrual(c.accruals.clone()));
    let pos = (!c.positions.is_empty()).then(|| DirtySet::Positions(c.positions.clone()));
    wide.into_iter().chain(reprice).chain(accrual).chain(pos)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::DirtyAccumulator;
    use alloy_primitives::Address;
    use liq_protocol::{DirtyPositions, DirtyRows, DirtySet, MarketRow, MarketSlot, StateWriter};
    use liq_state::{StateStore, StoreConfig, UndoCapacity};
    use liq_types::{AssetId, MarketId, PositionId, PositionKey, ProtocolId};

    fn row() -> MarketRow {
        MarketRow::blank(AssetId(0), 18)
    }

    fn store_with_pos(market: MarketId) -> (StateStore, PositionId) {
        let mut st = StateStore::new(StoreConfig {
            base: 0,
            positions: 8,
            markets: 4,
            undo: UndoCapacity {
                ops: 64,
                extras: 8,
                rows: 8,
            },
        });
        st.begin_block(1).unwrap();
        st.push_market(market, row()).unwrap();
        let id = st
            .intern(&PositionKey {
                protocol: ProtocolId(0),
                market,
                user: Address::repeat_byte(1),
            })
            .unwrap();
        (st, id)
    }

    /// Oracle: GUIDE 03 §4 rank order. Negative: a Positions report does not
    /// survive next to Accrual on the same market; Accrual does not survive
    /// next to Reprice.
    #[test]
    fn collapse_reprice_subsumes_accrual_subsumes_positions() {
        let m = MarketId(1);
        let slot = MarketSlot { market: m, slot: 0 };
        let (st, pid) = store_with_pos(m);
        let mut acc = DirtyAccumulator::new();
        acc.merge(DirtySet::Positions(DirtyPositions::from_slice(&[pid])));
        acc.merge(DirtySet::MarketAccrual(DirtyRows::from_slice(&[slot])));
        acc.merge(DirtySet::MarketReprice(DirtyRows::from_slice(&[slot])));
        let out = acc.collapse(&st, 1).unwrap();
        assert!(!out.protocol_wide);
        assert_eq!(out.reprices.as_slice(), &[slot]);
        assert!(out.accruals.is_empty(), "reprice subsumes accrual");
        assert!(out.positions.is_empty(), "reprice subsumes positions");
        assert!(
            DirtySet::MarketReprice(DirtyRows::new()).rank()
                > DirtySet::MarketAccrual(DirtyRows::new()).rank()
        );
    }

    /// Oracle: GUIDE 03 §4 — Positions alone stay Positions. Negative: they
    /// are not widened to Accrual or ProtocolWide.
    #[test]
    fn positions_alone_are_not_widened() {
        let m = MarketId(2);
        let (st, pid) = store_with_pos(m);
        let mut acc = DirtyAccumulator::new();
        acc.merge(DirtySet::Positions(DirtyPositions::from_slice(&[pid])));
        let out = acc.collapse(&st, 1).unwrap();
        assert_eq!(out.positions.as_slice(), &[pid]);
        assert!(out.accruals.is_empty());
        assert!(out.reprices.is_empty());
        assert!(!out.protocol_wide);
    }

    /// Oracle: GUIDE 03 §4b — "`DirtyAccumulator` reuses its buffers,
    /// `clear()` between blocks, never reallocate". `DirtyRows` is a
    /// `SmallVec<[MarketSlot; 16]>`, so a market with more than 16 dirty
    /// slots spills to the heap; that spilled buffer must survive `clear()`.
    /// Negative: a `clear()` that dropped the per-market row sets would leave
    /// capacity at the inline 16 and re-allocate on the next block.
    #[test]
    fn clear_keeps_spilled_row_capacity() {
        let m = MarketId(4);
        let slots: Vec<MarketSlot> = (0..20u16)
            .map(|slot| MarketSlot { market: m, slot })
            .collect();
        let mut acc = DirtyAccumulator::new();
        acc.merge(DirtySet::MarketAccrual(DirtyRows::from_slice(&slots)));
        let spilled = acc.rows.first().unwrap().capacity();
        assert!(spilled >= 20, "row set should have spilled past inline 16");
        acc.clear();
        assert_eq!(acc.markets.len(), 0, "live prefix is empty after clear");
        assert_eq!(
            acc.rows.first().map(smallvec::SmallVec::capacity),
            Some(spilled),
            "clear() must keep the spilled row buffer"
        );
        acc.merge(DirtySet::MarketAccrual(DirtyRows::from_slice(&slots)));
        assert_eq!(
            acc.rows.first().unwrap().capacity(),
            spilled,
            "the next block must reuse the buffer, not re-allocate"
        );
    }

    /// Oracle: ProtocolWide rank 4 swallows everything. Negative: leftover
    /// Positions after ProtocolWide would be a collapse bug.
    #[test]
    fn protocol_wide_suppresses_narrower() {
        let m = MarketId(3);
        let (st, pid) = store_with_pos(m);
        let mut acc = DirtyAccumulator::new();
        acc.merge(DirtySet::Positions(DirtyPositions::from_slice(&[pid])));
        acc.merge(DirtySet::ProtocolWide);
        let out = acc.collapse(&st, 1).unwrap();
        assert!(out.protocol_wide);
        assert!(out.positions.is_empty());
        assert!(out.accruals.is_empty());
        assert!(out.reprices.is_empty());
    }
}
