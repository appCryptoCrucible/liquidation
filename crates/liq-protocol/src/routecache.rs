//! `RouteCache` — leg 3 of the flash cycle, as a one-method interface (D46).
//! `liq-flash` (07B) consumes it through `DepthOnlyRouteCache`, a stated
//! conservative interim that under-admits; `liq-router` (12A-1) implements
//! the real one from pool state and replaces the stub.

use alloy_primitives::U256;
use liq_types::AssetId;

/// Can seized collateral be swapped back into the debt asset at acceptable
/// slippage? Eligibility is two-sided (GUIDE 07 §5): flash-fundable debt with
/// no collateral exit is as untargetable as unfundable debt.
pub trait RouteCache: Send + Sync {
    /// `true` when `amount` raw units of `coll` have an exit route at the
    /// router's configured slippage bound. Hot path: an `arc-swap` load plus
    /// a table lookup, no allocation.
    fn has_exit(&self, coll: AssetId, amount: U256) -> bool;
}
