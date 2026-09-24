//! `Quote` — the liquidation economics of one position as **sets** of
//! candidate legs (GUIDE 01 §4). A position with debt in three assets offers
//! three repay legs, only some of which are flash-fundable at depth; a quote
//! that names one asset throws away liquidatable positions (GUIDE 07 §5).

use alloy_primitives::{Address, U256};
use liq_types::{AssetId, PositionId, PositionKey, Ray};
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
    /// the remainder would fall below dust), capped by this reserve's debt.
    /// No caller-side notional cap: sizing below this ceiling is the
    /// viability band's job (GUIDE 12 §4b — `liq_router::band`), not the
    /// adapter's; a stored per-call cap here would be exactly the derived,
    /// wrongly-keyed, cross-asset-meaningless threshold that section retired.
    /// Where the rule depends on the seized reserve (V4: its liquidation
    /// threshold and bonus), it is evaluated against `seize_options[0]`.
    pub max_repay: U256,
    /// Smallest repay the protocol accepts for this leg. `0` for every
    /// repay-what-you-like leg; equal to `max_repay` for an all-or-nothing
    /// leg (Gearbox full liquidation: the account is closed as a whole, so
    /// sizing below it would revert, not shrink). The engine skips the leg
    /// rather than size under it.
    pub min_repay: U256,
    /// `Some(k)`: this repay leg is only valid with `seize_options[k]` (its
    /// amounts and bonus were computed together — Gearbox partial vs full
    /// on the same token). `None`: pairs with every seize option.
    pub pair_seize: Option<u8>,
    /// Which of the protocol's own reserves/markets this leg names, when
    /// `asset` alone does not identify it. See [`SlotRef`].
    pub slot: SlotRef,
}

impl RepayOption {
    /// Whether this repay leg may be combined with `seize_options[seize]`.
    #[inline]
    #[must_use]
    pub fn pairs_with(&self, seize: u8) -> bool {
        self.pair_seize.is_none_or(|k| k == seize)
    }
}

/// The protocol-native identifier of the reserve, market or token contract an
/// option refers to.
///
/// `AssetId` is a GLOBAL token id: one `AssetId` for WETH across every
/// protocol. That is the right key for prices and routing, and the wrong one
/// for naming a leg to the chain, because the mapping back is not injective:
///
/// * Compound V2 lists cWBTC **and** cWBTC2 against the same WBTC underlying.
///   Resolving WBTC by `.find()` always returned the first — the deprecated
///   cWBTC — so a plan repaid against a market where the borrower had no debt
///   (P6).
/// * Aave V4 names reserves by `reserveId`, which the adapter knows (it is
///   `store slot − 1`) and dropped at this boundary, leaving nothing in
///   production able to build the V4 leg tail at all (V1).
///
/// Carrying the adapter's own identifier alongside the global one closes both.
/// It is opaque to the engine and the router: only the adapter that produced
/// the quote interprets it, in `encode`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum SlotRef {
    /// `asset` identifies the leg on its own. Every adapter with one reserve
    /// per token.
    #[default]
    ByAsset,
    /// The row's slot in the adapter's own market store. Aave V4 derives
    /// `reserveId = slot - 1` from it; Compound V2 uses it to pick between
    /// two cTokens over one underlying.
    Slot(u16),
    /// A contract address the liquidation call names directly.
    Contract(Address),
}

impl SlotRef {
    /// The store slot, when this reference carries one.
    #[must_use]
    #[inline]
    pub const fn slot(self) -> Option<u16> {
        match self {
            Self::Slot(s) => Some(s),
            _ => None,
        }
    }

    /// The named contract, when this reference carries one.
    #[must_use]
    #[inline]
    pub const fn contract(self) -> Option<Address> {
        match self {
            Self::Contract(a) => Some(a),
            _ => None,
        }
    }
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
    /// Contract the liquidation call names when it is not `asset`.
    /// Zero except Euler V2, where it is the collateral vault. The plan's
    /// collateral asset stays the underlying the swaps sell.
    pub call_target: Address,
    /// Which of the protocol's own reserves/markets this leg names, when
    /// `asset` alone does not identify it. See [`SlotRef`].
    pub slot: SlotRef,
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
