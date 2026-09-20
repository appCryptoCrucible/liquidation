//! WP 08B trigger fan-out (GUIDE 08 §5). Does not edit 08A's engine modules.
//!
//! * [`on_derived_tick`] — 06A-2 `SourceKind::Derived` ticks go through
//!   08A's existing `Engine::on_price_tick` (already emits
//!   `TriggerCause::DerivedRate`; this is the owned call site).
//! * [`attach_crossing`] / [`crossing_set`] — precompute the position set
//!   that 08A's [`ThresholdIndex`] would fold when parameters move, then
//!   [`fire_param_change`] at the **execution** block only.
//! * [`StaleConfig`] — N-block horizon before a `TriggerCause::Stale` is
//!   forwarded. 08A emits `Stale` on first canonical sighting; this gate
//!   is the 08B rule ("untaken after N blocks"). Carry-forward: 08A has
//!   no `liq_since` / N hook — gating is on drain.

use liq_protocol::DirtySet;
use liq_types::{
    AssetId, PositionId, PriceTick, Ray, ScheduledParamChange, SourceKind, TriggerKind,
};

use crate::candidate::{Candidate, TriggerCause};
use crate::engine::{Engine, World};
use crate::threshold::ThresholdIndex;
use crate::EngineError;

/// Blocks a position must remain liquidatable and untaken before a
/// [`TriggerCause::Stale`] is forwarded.
///
/// Seam: construct with [`StaleConfig::new`] (wiring / `LIQ_STALE_AFTER_BLOCKS`
/// at 17A). Zero is invalid — that would collapse onto 08A's first-sight
/// emission and hide the N-block rule.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct StaleConfig {
    pub after_blocks: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TriggerError {
    #[error("derived fan-out given a non-derived SourceKind")]
    NotDerived,
    #[error("stale_after_blocks must be > 0 (set LIQ_STALE_AFTER_BLOCKS / StaleConfig::new)")]
    ZeroStaleHorizon,
    #[error("param-change fire at block {tip}, schedule is {scheduled}")]
    NotExecutionBlock { tip: u64, scheduled: u64 },
    #[error(transparent)]
    Engine(#[from] EngineError),
}

impl StaleConfig {
    pub fn new(after_blocks: u64) -> core::result::Result<Self, TriggerError> {
        if after_blocks == 0 {
            return Err(TriggerError::ZeroStaleHorizon);
        }
        Ok(Self { after_blocks })
    }

    /// `LIQ_STALE_AFTER_BLOCKS`. Missing or unparsable is an error (no default).
    pub fn from_env() -> core::result::Result<Self, TriggerError> {
        let s =
            std::env::var("LIQ_STALE_AFTER_BLOCKS").map_err(|_| TriggerError::ZeroStaleHorizon)?;
        let n = s
            .parse::<u64>()
            .map_err(|_| TriggerError::ZeroStaleHorizon)?;
        Self::new(n)
    }

    /// `tip - liquidatable_since >= N`.
    #[inline]
    #[must_use]
    pub fn ripe(self, tip: u64, liquidatable_since: u64) -> bool {
        tip.saturating_sub(liquidatable_since) >= self.after_blocks
    }
}

/// Hot path: derived tick → 08A `on_price_tick`. Fail closed if the source
/// is not [`SourceKind::Derived`].
#[inline]
pub fn on_derived_tick(
    engine: &mut Engine,
    world: &World<'_>,
    tick: &PriceTick,
) -> core::result::Result<(), TriggerError> {
    match tick.source {
        SourceKind::Derived { .. } => Ok(engine.on_price_tick(world, tick)?),
        _ => Err(TriggerError::NotDerived),
    }
}

/// Positions whose live threshold on `asset` lies in `[old, new]` (inclusive
/// on the crossed side). Sorted + deduped for a stable `DirtySet`.
#[must_use]
pub fn crossing_set(index: &ThresholdIndex, asset: AssetId, old: Ray, new: Ray) -> Vec<PositionId> {
    let mut v: Vec<PositionId> = index.crossed(asset, old, new).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Every live registration on `asset` — a market-wide reprice when the new
/// liquidation price is not yet known as a RAY (LT/LTV cut).
#[must_use]
pub fn registered_set(index: &ThresholdIndex, asset: AssetId) -> Vec<PositionId> {
    let mut v: Vec<PositionId> = index.registered(asset).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Fill [`ScheduledParamChange::crossing`] from the current index.
pub fn attach_crossing(
    mut ev: ScheduledParamChange,
    index: &ThresholdIndex,
    asset: AssetId,
    old: Ray,
    new: Ray,
) -> ScheduledParamChange {
    ev.crossing = crossing_set(index, asset, old, new);
    ev
}

/// Fire [`TriggerCause::ParamChange`] through 08A `on_dirty` **only** at
/// `execution_block`. Earlier/later tips are an error (never silently run
/// at the announcement block).
pub fn fire_param_change(
    engine: &mut Engine,
    world: &World<'_>,
    ev: &ScheduledParamChange,
) -> core::result::Result<(), TriggerError> {
    let tip = world.view.tip();
    if tip != ev.execution_block {
        return Err(TriggerError::NotExecutionBlock {
            tip,
            scheduled: ev.execution_block,
        });
    }
    let cause = TriggerCause::ParamChange { market: ev.market };
    debug_assert_eq!(cause.kind(), TriggerKind::ParamChange);
    let dirty = DirtySet::Positions(ev.crossing.iter().copied().collect());
    Ok(engine.on_dirty(world, ev.protocol, &dirty, &cause)?)
}

/// Drain engine candidates, forwarding `Stale` only after N blocks. Other
/// causes pass. Dropped early-Stale rows are re-emitted by 08A on the next
/// `on_block` / canonical fold.
pub fn take_ripe(engine: &mut Engine, tip: u64, stale: StaleConfig) -> Vec<Candidate> {
    engine
        .candidates()
        .filter(|c| match c.cause {
            TriggerCause::Stale { liquidatable_since } => stale.ripe(tip, liquidatable_since),
            _ => true,
        })
        .collect()
}

/// `TriggerCause::kind` is the 08A mapping — never re-enumerated here.
#[inline]
#[must_use]
pub fn kind(cause: &TriggerCause) -> TriggerKind {
    cause.kind()
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
    use super::*;
    use alloy_primitives::U256;
    use liq_types::{AssetId, MarketId, PositionId, Ray, TriggerKind};

    fn r(v: u64) -> Ray {
        Ray::from_raw(U256::from(v))
    }

    #[test]
    fn stale_n_refuses_zero_and_gates_horizon() {
        assert!(StaleConfig::new(0).is_err());
        let n = StaleConfig::new(3).unwrap();
        assert!(!n.ripe(10, 10));
        assert!(!n.ripe(12, 10));
        assert!(n.ripe(13, 10));
        assert_eq!(n.after_blocks, 3);
    }

    #[test]
    fn kind_delegates_to_08a_trigger_cause() {
        assert_eq!(
            kind(&TriggerCause::DerivedRate { source: AssetId(1) }),
            TriggerKind::DerivedRate
        );
        assert_eq!(
            kind(&TriggerCause::ParamChange {
                market: MarketId(7)
            }),
            TriggerKind::ParamChange
        );
        assert_eq!(
            kind(&TriggerCause::Stale {
                liquidatable_since: 9
            }),
            TriggerKind::Stale
        );
    }

    #[test]
    fn crossing_matches_index_and_dedups() {
        let a = AssetId(0);
        let mut idx = ThresholdIndex::new(1, 8);
        idx.begin(PositionId(1));
        idx.register(PositionId(1), a, crate::Side::Falling, r(100))
            .unwrap();
        idx.begin(PositionId(2));
        idx.register(PositionId(2), a, crate::Side::Falling, r(50))
            .unwrap();
        idx.begin(PositionId(3));
        idx.register(PositionId(3), a, crate::Side::Rising, r(200))
            .unwrap();
        let got = crossing_set(&idx, a, r(120), r(40));
        assert_eq!(got, vec![PositionId(1), PositionId(2)]);
        let all = registered_set(&idx, a);
        assert_eq!(all, vec![PositionId(1), PositionId(2), PositionId(3)]);
    }
}
