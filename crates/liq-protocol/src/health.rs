//! Normalised health (GUIDE 01 §3). `hf == 1.0` is the liquidation boundary
//! for **every** protocol; this normalisation is what lets the band manager,
//! threshold index and risk limits be written once.

use liq_types::{Ray, Wad};

use crate::mask::AssetMask;
use crate::Timestamp;

/// Position health at one price vector and one chain time.
///
/// `Copy` with no heap field: a `health()` that returns this cannot allocate
/// through its return value (the implementation is separately asserted
/// allocation-free by the conformance harness under the sanctioned counting
/// allocator, RUST-CONVENTIONS §6).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Health {
    /// Health factor normalised so `1.0` is the boundary for every protocol.
    /// With zero debt the protocol convention is `hf = U256::MAX`
    /// (Aave `type(uint256).max`); see [`Health::NO_DEBT_HF`].
    pub hf: Ray,
    /// Total debt in the price vector's numeraire, WAD-scaled, **including**
    /// any per-position accrual the protocol charges (V4 risk premium).
    pub debt_value: Wad,
    /// Total **unweighted** collateral value (what is seizable), same units.
    /// The liquidation-threshold weighting is inside `hf`, not here — the
    /// chain's own convention (`getUserAccountData.totalCollateralBase`).
    pub collateral_value: Wad,
    /// Slots whose price moves `hf`. The threshold index registers the
    /// position on exactly these assets.
    pub price_sensitivity: AssetMask,
    /// What the protocol will let a liquidator do right now.
    pub state: HealthState,
}

impl Health {
    /// `hf` reported for a position with no debt — mirrors Aave's
    /// `type(uint256).max` so a zero-debt position never looks liquidatable
    /// and never divides by zero.
    pub const NO_DEBT_HF: Ray = Ray::from_raw(alloy_primitives::U256::MAX);
}

/// Liquidation state, normalised across protocols.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HealthState {
    /// `hf >= 1.0` (or no debt).
    Healthy,
    /// `hf < 1.0` but the protocol refuses liquidation right now.
    Blocked { reason: BlockReason },
    /// `hf < 1.0` and a liquidation call would succeed.
    Liquidatable,
    /// A Dutch-auction style liquidation is running (Sky Clipper, Ajna).
    AuctionOpen { started: Timestamp },
    /// Collateral no longer covers debt; `deficit` in the numeraire.
    BadDebt { deficit: Wad },
    /// Curve LLAMMA and similar: the position is rebalanced continuously
    /// across price bands and is **never** "liquidatable" in the
    /// seize-collateral-take-bonus sense. **Decline, don't adapt** (GUIDE 15
    /// §5): an adapter that reports this state must never return a `Quote`
    /// (conformance check 10), and the protocol is recorded in `DECLINED.md`
    /// rather than built.
    SoftLiquidating,
}

/// Why a position below the boundary cannot be liquidated right now.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlockReason {
    /// Reserve or protocol paused.
    Paused,
    /// Reserve frozen in a way that blocks liquidation for this protocol.
    Frozen,
    /// Post-unpause grace period (Aave V3.1+ `liquidationGracePeriodUntil`).
    GracePeriod,
    /// Debt below the protocol's dust floor; liquidation would revert.
    Dust,
}
