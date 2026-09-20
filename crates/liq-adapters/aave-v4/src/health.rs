//! `Spoke._processUserAccountData` (@ `40232a0a`, `Spoke.sol:706`), one
//! held slot at a time, plus the liquidation-state classification the
//! contract expresses as reverts in `LiquidationLogic._validateLiquidationCall`
//! and the Hub's `_validate*`.
//!
//! Everything here is stack arithmetic over borrowed slices: no `Vec`, no
//! `SmallVec`, no `Box`, no formatting. That is the structural half of the
//! D57 allocation-free requirement; the conformance harness measures the
//! other half through the caller's `AllocMeter` (`crate::alloc_meter`).
//!
//! Per-step rounding is tabulated in `docs/coverage/aave-v4-rounding.md` §2;
//! the step numbers in comments below refer to that table.

use alloy_primitives::U256;
use liq_protocol::{
    AssetMask, BlockReason, Health, HealthState, MarketFlags, MarketRow, MarketSlot, PositionRef,
    ProtocolError, Result,
};
use liq_types::fixed::{mul_div, FixedError, Rounding, RAY};
use liq_types::{AssetId, MarketId, PriceVector, Ray, Wad};

use crate::layout::{Reserve, ReserveCfg, UserReserve, UNMAPPED_ASSET};
use crate::math::{
    drawn_index_at, hf_wad_to_ray, p8_of, premium_ray, signed, to_assets_down, total_added_assets,
    value_scale, BPS_TO_WAD, VALUE_TO_WAD,
};

/// Collateral side of one slot, when the chain counts it
/// (`collateral && collateralFactor > 0 && suppliedShares > 0`).
#[derive(Copy, Clone, Debug)]
pub(crate) struct Collateral {
    /// The user's snapshotted `collateralFactor` (bps).
    pub cf: U256,
    /// `previewRemoveByShares(suppliedShares)`, asset units (step C3).
    pub assets: U256,
    /// `assets · 10^(18 − decimals)`: Value per unit of 8-decimal price.
    pub per_price: U256,
    /// `toValue(assets)` (step C4), units of Value.
    pub value: U256,
}

/// Debt side of one slot, when `drawnShares > 0`.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Debt {
    /// `(drawnShares · drawnIndex + premiumDebtRay) · 10^(18 − decimals)`:
    /// step D3 (exact, RAY-scaled asset units) times the decimals lift, i.e.
    /// RAY·Value per unit of 8-decimal price.
    pub per_price: U256,
    /// `toValue(debt_ray)` (step D4), RAY·Value.
    pub value_ray: U256,
}

/// One held slot as `_processUserAccountData` sees it.
#[derive(Copy, Clone, Debug)]
pub(crate) struct SlotTerms<'a> {
    pub slot: u16,
    pub row: &'a MarketRow,
    pub reserve: &'a Reserve,
    pub user: &'a UserReserve,
    pub supply_shares: u128,
    pub debt_shares: u128,
    /// `AaveOracle.getReservePrice`, 8 decimals.
    pub p8: U256,
    /// `getAssetDrawnIndex` projected to the view's timestamp (steps I1–I3).
    pub idx: U256,
    /// `Premium.calculatePremiumRay` for this user (step D2); zero when not
    /// borrowing.
    pub premium_ray: U256,
    pub collateral: Option<Collateral>,
    pub debt: Option<Debt>,
}

impl SlotTerms<'_> {
    /// `ReserveFlags.paused` — every action reverts.
    #[inline]
    pub(crate) fn paused(&self) -> bool {
        self.reserve.cfg.flags & ReserveCfg::PAUSED != 0
    }

    /// Hub `SpokeData.active` for this reserve's `(asset, spoke)`.
    #[inline]
    pub(crate) fn spoke_active(&self) -> bool {
        self.reserve.cfg.flags & ReserveCfg::SPOKE_ACTIVE != 0
    }

    /// Hub `SpokeData.halted`: `add/remove/draw/restore` revert.
    #[inline]
    pub(crate) fn spoke_halted(&self) -> bool {
        self.reserve.cfg.flags & ReserveCfg::SPOKE_HALTED != 0
    }

    /// Whether `liquidationCall` accepts this slot as the collateral side
    /// (`_validateLiquidationCall` + `Hub._validateRemove/_validatePayFeeShares`).
    #[inline]
    pub(crate) fn seizable(&self) -> bool {
        self.collateral.is_some() && !self.paused() && self.spoke_active() && !self.spoke_halted()
    }

    /// Whether `liquidationCall` accepts this slot as the debt side
    /// (`_validateLiquidationCall` + `Hub._validateRestore`).
    #[inline]
    pub(crate) fn repayable(&self) -> bool {
        self.debt.is_some() && !self.paused() && self.spoke_active() && !self.spoke_halted()
    }
}

/// Price of `asset` from the vector, as the 8-decimal integer the Spoke
/// reads. `MissingPrice` when absent or zero (the oracle's `InvalidPrice`
/// revert: a zero price is no price).
#[inline]
pub(crate) fn price_p8(px: &PriceVector, asset: AssetId) -> Result<U256> {
    px.0.get(usize::from(asset.0))
        .filter(|p| p.asset == asset)
        .and_then(|p| p8_of(p.price))
        .ok_or(ProtocolError::MissingPrice(asset))
}

#[inline]
fn cell(v: &[u128], slot: u16) -> u128 {
    v.get(usize::from(slot)).copied().unwrap_or(0)
}

/// Visit every slot `_processUserAccountData` would visit, in slot order,
/// with the terms it computes. `price_of` supplies the 8-decimal price per
/// asset (the vector, or the vector with one asset overridden).
///
/// Errors: `SlotOutOfRange` (a set bit past the market's rows),
/// `BodyLayout`/`ExtraLayout` (a row or extra that is not this adapter's),
/// `OracleSourceMismatch` (the slot is `UNPRICED` or its underlying is not
/// in the registry), `MissingPrice`, `TimestampBeforeUpdate`, and `Fixed`
/// for an overflow the chain would also revert on.
pub(crate) fn walk<'a, F, P>(pos: &PositionRef<'a>, mut price_of: P, mut f: F) -> Result<()>
where
    F: FnMut(&SlotTerms<'a>) -> Result<()>,
    P: FnMut(AssetId) -> Result<U256>,
{
    for slot in pos.config.iter() {
        let row = pos
            .markets
            .get(usize::from(slot))
            .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
                market: pos.key.market,
                slot,
            }))?;
        let supply_shares = cell(pos.supply, slot);
        let debt_shares = cell(pos.debt, slot);
        // A slot never written has the zero extra (StateWriter contract).
        let user: &UserReserve = match pos.slot_extra.get(usize::from(slot)) {
            Some(e) => e.view()?,
            None => &UserReserve::ZERO,
        };
        let reserve: &Reserve = row.body()?;

        let borrowing = debt_shares > 0;
        let counted = user.flags & UserReserve::USING_AS_COLLATERAL != 0
            && user.collateral_factor > 0
            && supply_shares > 0;
        if !borrowing && !counted {
            // Not a `PositionStatus` bit the chain iterates with a
            // contribution; it reads no price for it.
            continue;
        }
        if row.flags.contains(MarketFlags::UNPRICED) || row.asset == UNMAPPED_ASSET {
            return Err(ProtocolError::OracleSourceMismatch);
        }
        let p8 = price_of(row.asset)?;
        let scale = value_scale(row.decimals)?;
        let idx = drawn_index_at(&reserve.hub, row.last_update, pos.timestamp)?;

        let collateral = if counted {
            // C1–C3: totalAddedAssets at the projected index, floor share
            // conversion; C4: exact value.
            let total = total_added_assets(&reserve.hub, idx)?;
            let assets = to_assets_down(
                U256::from(supply_shares),
                total,
                U256::from(reserve.hub.added_shares),
            )?;
            let per_price = assets.checked_mul(scale).ok_or(FixedError::Overflow)?;
            let value = per_price.checked_mul(p8).ok_or(FixedError::Overflow)?;
            Some(Collateral {
                cf: U256::from(user.collateral_factor),
                assets,
                per_price,
                value,
            })
        } else {
            None
        };

        let (premium, debt) = if borrowing {
            // D1–D2: premium with full precision (reverts negative); D3
            // exact; D4 exact.
            let premium = premium_ray(
                user.premium_shares,
                signed(user.premium_offset_lo, user.premium_offset_hi),
                idx,
            )?;
            let debt_ray = U256::from(debt_shares)
                .checked_mul(idx)
                .and_then(|d| d.checked_add(premium))
                .ok_or(FixedError::Overflow)?;
            let per_price = debt_ray.checked_mul(scale).ok_or(FixedError::Overflow)?;
            let value_ray = per_price.checked_mul(p8).ok_or(FixedError::Overflow)?;
            (
                premium,
                Some(Debt {
                    per_price,
                    value_ray,
                }),
            )
        } else {
            (U256::ZERO, None)
        };

        f(&SlotTerms {
            slot,
            row,
            reserve,
            user,
            supply_shares,
            debt_shares,
            p8,
            idx,
            premium_ray: premium,
            collateral,
            debt,
        })?;
    }
    Ok(())
}

/// The account-level sums `_processUserAccountData` accumulates, plus the
/// liquidation gates.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Account {
    /// The position's market, for error addressing.
    pub market: MarketId,
    /// `avgCollateralFactor` before normalisation: `Σ cf · value`
    /// (bps · Value).
    pub weighted: U256,
    /// `totalCollateralValue` (Value).
    pub collateral_value: U256,
    /// `totalDebtValueRay` (RAY · Value).
    pub debt_value_ray: U256,
    /// `activeCollateralCount`.
    pub collateral_count: u32,
    /// `borrowCount`.
    pub borrow_count: u32,
    /// Some collateral slot passes `seizable()`.
    pub any_seizable: bool,
    /// Some debt slot passes `repayable()`.
    pub any_repayable: bool,
    /// Every borrowed reserve's hub spoke is active: the post-liquidation
    /// `refreshPremium`/`reportDeficit` fan-out over all debt reserves
    /// requires it (`Hub.sol:367`, `:882`).
    pub all_debt_spokes_active: bool,
    /// Slots whose price enters `hf`.
    pub sensitivity: liq_protocol::AssetMask,
}

impl Account {
    /// Empty sums for a position of `market`.
    #[inline]
    pub(crate) const fn new(market: MarketId) -> Self {
        Self {
            market,
            weighted: U256::ZERO,
            collateral_value: U256::ZERO,
            debt_value_ray: U256::ZERO,
            collateral_count: 0,
            borrow_count: 0,
            any_seizable: false,
            any_repayable: false,
            all_debt_spokes_active: true,
            sensitivity: AssetMask::EMPTY,
        }
    }

    /// Fold one slot in (the loop body of `_processUserAccountData`).
    #[inline]
    pub(crate) fn add(&mut self, t: &SlotTerms<'_>) -> Result<()> {
        if let Some(c) = t.collateral {
            self.collateral_value = self
                .collateral_value
                .checked_add(c.value)
                .ok_or(FixedError::Overflow)?;
            let w = c.cf.checked_mul(c.value).ok_or(FixedError::Overflow)?;
            self.weighted = self.weighted.checked_add(w).ok_or(FixedError::Overflow)?;
            self.collateral_count = self.collateral_count.saturating_add(1);
            self.any_seizable |= t.seizable();
        }
        if let Some(d) = t.debt {
            self.debt_value_ray = self
                .debt_value_ray
                .checked_add(d.value_ray)
                .ok_or(FixedError::Overflow)?;
            self.borrow_count = self.borrow_count.saturating_add(1);
            self.any_repayable |= t.repayable();
            self.all_debt_spokes_active &= t.spoke_active();
        }
        self.sensitivity = self
            .sensitivity
            .with(t.slot)
            .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
                market: self.market,
                slot: t.slot,
            }))?;
        Ok(())
    }

    /// Step H1: `healthFactor = mulDiv(avgCollateralFactor.bpsToWad(), RAY,
    /// totalDebtValueRay, Floor)`, or `type(uint256).max` with no debt.
    #[inline]
    pub(crate) fn hf_wad(&self) -> Result<U256> {
        if self.debt_value_ray.is_zero() {
            return Ok(U256::MAX);
        }
        let wad = self
            .weighted
            .checked_mul(BPS_TO_WAD)
            .ok_or(FixedError::Overflow)?;
        Ok(mul_div(wad, RAY, self.debt_value_ray, Rounding::Down)?)
    }

    /// `totalDebtValue = totalDebtValueRay.fromRayUp()` (step H2), Value.
    #[inline]
    pub(crate) fn debt_value(&self) -> Result<U256> {
        Ok(mul_div(self.debt_value_ray, U256::ONE, RAY, Rounding::Up)?)
    }
}

/// Value (1e26 = 1 USD) to the engine's WAD numeraire, floored. The 8 low
/// digits are below any price feed's resolution; `hf` is never derived from
/// this.
#[inline]
fn value_to_wad(v: U256) -> Wad {
    Wad::from_raw(v.wrapping_div(VALUE_TO_WAD))
}

/// `Protocol::health` body.
pub(crate) fn health(pos: PositionRef<'_>, px: &PriceVector) -> Result<Health> {
    let mut acc = Account::new(pos.key.market);
    walk(&pos, |asset| price_p8(px, asset), |t| acc.add(t))?;
    finish(&acc)
}

/// Steps H1–H2 and the state classification over folded sums.
pub(crate) fn finish(acc: &Account) -> Result<Health> {
    let hf = hf_wad_to_ray(acc.hf_wad()?)?;
    let debt_value = value_to_wad(acc.debt_value()?);
    let collateral_value = value_to_wad(acc.collateral_value);
    let state = if hf >= Ray::ONE {
        HealthState::Healthy
    } else if acc.collateral_value.is_zero() {
        // Nothing counted as collateral: `_validateLiquidationCall` reverts
        // `ReserveNotSupplied`/`ReserveNotEnabledAsCollateral` on every
        // pair. Only a deficit report can clear this debt.
        HealthState::BadDebt {
            deficit: debt_value,
        }
    } else if acc.any_seizable && acc.any_repayable && acc.all_debt_spokes_active {
        HealthState::Liquidatable
    } else {
        HealthState::Blocked {
            reason: BlockReason::Paused,
        }
    };
    Ok(Health {
        hf,
        debt_value,
        collateral_value,
        price_sensitivity: acc.sensitivity,
        state,
    })
}
