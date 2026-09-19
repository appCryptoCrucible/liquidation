//! `DirtySet` — what one `apply_log` invalidated (GUIDE 01 §6).
//!
//! Precision here buys two orders of magnitude: an adapter that returns
//! `MarketReprice` (or `ProtocolWide`) for a routine rate update starves every
//! other protocol's recompute budget. The conformance harness (check 7)
//! enforces minimality against the event-coverage audit's classification.
//!
//! Granularity is the **row** ([`MarketSlot`]): one row is one balance column
//! (GUIDE 02 §2), so a hub-level accrual on Aave V4 reports every row that
//! shares the `hub_ref` — several rows across spokes — rather than widening to
//! `ProtocolWide`. Inline capacities are sized so no real log spills to the
//! heap: 16 rows covers a hub asset referenced by every spoke; 8 positions
//! covers any single-log position set (a transfer touches two).

use liq_types::PositionId;
use smallvec::SmallVec;

use crate::market::MarketSlot;

/// Rows dirtied by one log. Inline capacity 16; see module docs.
pub type DirtyRows = SmallVec<[MarketSlot; 16]>;
/// Positions dirtied by one log. Inline capacity 8; see module docs.
pub type DirtyPositions = SmallVec<[PositionId; 8]>;

/// Ordered from narrowest to widest; `DirtyAccumulator` (GUIDE 03 §4)
/// collapses `MarketReprice ⊃ MarketAccrual ⊃ Positions` per market.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirtySet {
    /// The log changed nothing the engine reads.
    None,
    /// These positions' balances or per-position extra changed. Recompute
    /// them; nothing else moved.
    Positions(DirtyPositions),
    /// Index/rate moved on these rows. Re-**project** every position with the
    /// slot set; do not re-fold.
    MarketAccrual(DirtyRows),
    /// Risk parameters changed on these rows. Full recompute of every position
    /// with the slot set **and** threshold re-derivation.
    MarketReprice(DirtyRows),
    /// Everything in this protocol must be recomputed. Reserved for an event
    /// whose scope genuinely is the protocol and not a row set — a pool-level
    /// pause, a comptroller-level close-factor change; never for accrual.
    /// The Aave V4 audit (`docs/coverage/aave-v4.md`) maps **no** event here:
    /// a reserve pause/freeze is `MarketReprice` on the affected rows, and a
    /// proxy/authority change is a `HaltSink` halt, not a `DirtySet`.
    ProtocolWide,
}

impl DirtySet {
    /// Breadth rank: `None < Positions < MarketAccrual < MarketReprice <
    /// ProtocolWide`. Used by the conformance harness to assert an adapter
    /// reported no wider than the event class warrants.
    #[inline]
    #[must_use]
    pub const fn rank(&self) -> u8 {
        match self {
            Self::None => 0,
            Self::Positions(_) => 1,
            Self::MarketAccrual(_) => 2,
            Self::MarketReprice(_) => 3,
            Self::ProtocolWide => 4,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{DirtyPositions, DirtyRows, DirtySet};
    use crate::market::MarketSlot;
    use liq_types::{MarketId, PositionId};

    /// Oracle: GUIDE 01 §6 / GUIDE 03 §4 — the breadth order the
    /// `DirtyAccumulator` collapses along (`MarketReprice ⊃ MarketAccrual ⊃
    /// Positions`) and the harness's check-7 minimality comparison. Negative:
    /// accrual must not rank at or above a reprice. Constructing the market
    /// variants also pins their payload to row (`MarketSlot`) granularity —
    /// a hub accrual fans out across spokes without `ProtocolWide`.
    #[test]
    fn rank_orders_narrowest_to_widest() {
        let rows: DirtyRows = DirtyRows::from_slice(&[
            MarketSlot {
                market: MarketId(1),
                slot: 0,
            },
            MarketSlot {
                market: MarketId(2),
                slot: 7,
            },
        ]);
        let ranks = [
            DirtySet::None.rank(),
            DirtySet::Positions(DirtyPositions::from_slice(&[PositionId(0)])).rank(),
            DirtySet::MarketAccrual(rows.clone()).rank(),
            DirtySet::MarketReprice(rows).rank(),
            DirtySet::ProtocolWide.rank(),
        ];
        assert_eq!(ranks, [0, 1, 2, 3, 4], "strictly increasing breadth");
    }
}
