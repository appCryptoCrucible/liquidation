//! WP 09A fills [`EngineJoin`]. This crate must not import liq-engine.

use crate::types::DecodedLiquidation;

/// Second-opinion seam: the engine join and miss alarm live in 09A.
pub trait EngineJoin {
    fn observe(&self, ev: &DecodedLiquidation);
}

/// Default: persist only. 09A replaces this.
#[derive(Copy, Clone, Debug, Default)]
pub struct NoEngineJoin;

impl EngineJoin for NoEngineJoin {
    fn observe(&self, _: &DecodedLiquidation) {}
}
