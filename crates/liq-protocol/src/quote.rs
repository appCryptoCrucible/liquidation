//! `Quote` — the liquidation economics of one position as **sets** of
//! candidate legs (GUIDE 01 §4). A position with debt in three assets offers
//! three repay legs, only some of which are flash-fundable at depth; a quote
//! that names one asset throws away liquidatable positions (GUIDE 07 §5).

use alloy_primitives::U256;
use liq_types::{AssetId, PositionId, PositionKey, Ray, Wad};
use smallvec::SmallVec;

use crate::bonus::BonusCurve;

/// One repayable debt leg.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RepayOption {
    /// Debt asset (global id).
    pub asset: AssetId,
    /// Maximum repayable **right now** in this asset's raw underlying units,
    /// after the protocol's close-factor rule (V3: `close_factor × debt`;
    /// V4: enough to restore the target HF, raised to clear the reserve when
    /// the remainder would fall below dust), capped by this reserve's debt
    /// and by [`Constraints::per_liquidation_notional_cap`]. Where the rule
    /// depends on the seized reserve (V4: its liquidation threshold and
    /// bonus), it is evaluated against `seize_options[0]`.
    pub max_repay: U256,
}

/// One seizable collateral leg. Bonus is per reserve and e-mode dependent
/// (D26), so it lives here, not on the quote.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SeizeOption {
    /// Collateral asset (global id).
    pub asset: AssetId,
    /// Upper bound on what this leg can yield: the position's seizable
    /// balance of `asset` in raw underlying units. The exit route must be
    /// able to absorb up to this (`RouteCache::has_exit`).
    pub max_seize: U256,
    /// Bonus the protocol pays **at the quoted health**, RAY (`0.05` = 5 %).
    /// Equals `curve.bonus_at_hf(health.hf)` — conformance check 5.
    pub bonus: Ray,
    /// How `bonus` evolves as health deteriorates. Populated by the adapter
    /// from live parameters, evaluated by the engine; never a scalar.
    pub curve: BonusCurve,
}

/// Full economics of liquidating one position at one price vector.
///
/// Both option lists are ordered by **economic preference**, not by storage
/// slot (conformance check 8). The order is a contract, so the engine can
/// take `[0]` without re-ranking: `repay_options` by repayable value
/// (`max_repay × price`) descending; `seize_options` by `bonus` descending,
/// then seizable value descending (D26 — exit cost is the router's term and
/// is subtracted downstream). Ties keep slot order. GUIDE 07 and GUIDE 12
/// search the full `repay × seize` product and name the pair to
/// [`crate::Protocol::encode`] with a [`LegChoice`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quote {
    pub position: PositionId,
    /// Natural key — `encode` needs the borrower and market addresses and
    /// has no store access.
    pub key: PositionKey,
    /// Every debt reserve the protocol accepts repayment in, preferred first.
    pub repay_options: SmallVec<[RepayOption; 4]>,
    /// Every collateral reserve the protocol will release, preferred first.
    pub seize_options: SmallVec<[SeizeOption; 8]>,
}

/// Which `(repay, seize)` pair of a [`Quote`] to encode — indices into the
/// two option sets, validated by the adapter (`LegOutOfRange`).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct LegChoice {
    pub repay: u8,
    pub seize: u8,
}

impl LegChoice {
    /// The adapter's preferred pair: `[0]` of each set.
    pub const PREFERRED: Self = Self { repay: 0, seize: 0 };
}

/// Caller-side limits passed to `Protocol::quote` (GUIDE 12 §1).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Constraints {
    /// Cap on one liquidation's repay value, in the price vector's numeraire
    /// (WAD). The adapter converts it into each repay asset's raw units and
    /// clamps `RepayOption::max_repay`. [`Constraints::UNBOUNDED`] ≙ no cap.
    pub per_liquidation_notional_cap: Wad,
}

impl Constraints {
    /// No caller-side limit; the protocol rule alone bounds `max_repay`.
    pub const UNBOUNDED: Self = Self {
        per_liquidation_notional_cap: Wad::from_raw(U256::MAX),
    };
}
