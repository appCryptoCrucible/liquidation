//! `LiquidationLogic._calculateLiquidationAmounts` (@ `40232a0a`) — the
//! close-factor rule, the dust rules and the collateral conversion, step for
//! step — driving `Protocol::quote`.
//!
//! The chain computes the amounts from the liquidator's `debtToCover`;
//! `max_repay` is what it takes when `debtToCover` is unbounded (or the
//! caller's cap in raw units), which is also exactly the `amountToRestore`
//! the liquidator pays. Passing that amount back as `debtToCover` reproduces
//! the same amounts: the `drawnSharesToCover` cap it induces is
//! `floor(rayMulUp(d, idx) · RAY / idx) >= d`, so the `min` is unchanged.

use alloy_primitives::U256;
use liq_protocol::{
    BonusCurve, Constraints, HealthState, PositionRef, ProtocolError, Quote, RepayOption, Result,
    SeizeOption,
};
use liq_types::fixed::{mul_div, FixedError, Rounding, RAY, WAD_RAY_RATIO};
use liq_types::{AssetId, PriceVector, Ray};
use smallvec::SmallVec;

use crate::health::{finish, price_p8, walk, Account, SlotTerms};
use crate::layout::SpokeMeta;
use crate::math::{
    asset_unit, div_up, from_ray_up, percent_mul_down, percent_mul_up, ray_mul_up, round_ray_up,
    to_assets_down, to_assets_up, to_shares_down, to_value, total_added_assets, value_ray_of, BPS,
    BPS_RAY, BPS_TO_WAD, DUST_VALUE, HF_THRESHOLD_WAD,
};

/// `LiquidationLogic.calculateLiquidationBonus`, bps (`>= 10_000`).
///
/// `min = percentMulDown(max − 1e4, factor) + 1e4`; below `hfForMax` the
/// max; else `min + mulDivDown(max − min, 1e18 − hf, 1e18 − hfForMax)`.
pub(crate) fn liquidation_bonus_bps(
    hf_for_max_wad: U256,
    factor_bps: U256,
    hf_wad: U256,
    max_bonus_bps: U256,
) -> Result<U256> {
    if hf_wad <= hf_for_max_wad {
        return Ok(max_bonus_bps);
    }
    let span = max_bonus_bps
        .checked_sub(BPS)
        .ok_or(FixedError::Underflow)?;
    let min = percent_mul_down(span, factor_bps)?
        .checked_add(BPS)
        .ok_or(FixedError::Overflow)?;
    let rise = mul_div(
        max_bonus_bps
            .checked_sub(min)
            .ok_or(FixedError::Underflow)?,
        HF_THRESHOLD_WAD
            .checked_sub(hf_wad)
            .ok_or(FixedError::Underflow)?,
        HF_THRESHOLD_WAD
            .checked_sub(hf_for_max_wad)
            .ok_or(FixedError::Underflow)?,
        Rounding::Down,
    )?;
    min.checked_add(rise)
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// The spoke's `LiquidationConfig` and one reserve's `maxLiquidationBonus`
/// as the engine's curve. Checked once here: `hf_for_max < 1.0`
/// (`Spoke.updateLiquidationConfig` enforces it; a row that violates it was
/// written wrong → `Internal`).
pub(crate) fn bonus_curve(meta: &SpokeMeta, max_bonus_bps: u32) -> Result<BonusCurve> {
    let hf_for_max_wad = U256::from(meta.hf_for_max_bonus);
    if hf_for_max_wad >= HF_THRESHOLD_WAD {
        return Err(ProtocolError::Internal);
    }
    let max = U256::from(max_bonus_bps);
    let span = max.checked_sub(BPS).ok_or(FixedError::Underflow)?;
    let at_threshold = percent_mul_down(span, U256::from(meta.bonus_factor))?;
    let bps = |b: U256| -> Result<Ray> {
        b.checked_mul(BPS_RAY)
            .map(Ray::from_raw)
            .ok_or(ProtocolError::Fixed(FixedError::Overflow))
    };
    Ok(BonusCurve::HealthLinear {
        bonus_at_threshold: bps(at_threshold)?,
        hf_for_max: Ray::from_raw(
            hf_for_max_wad
                .checked_mul(WAD_RAY_RATIO)
                .ok_or(FixedError::Overflow)?,
        ),
        max_bonus: bps(span)?,
        quantum: Ray::from_raw(BPS_RAY),
    })
}

/// Inputs of `_calculateLiquidationAmounts` for one `(collateral, debt)`
/// pair.
pub(crate) struct AmountsIn<'a> {
    pub coll: &'a SlotTerms<'a>,
    pub debt: &'a SlotTerms<'a>,
    /// `UserAccountData.totalDebtValueRay`.
    pub total_debt_value_ray: U256,
    /// `UserAccountData.healthFactor` (WAD).
    pub hf_wad: U256,
    pub meta: &'a SpokeMeta,
    /// `liquidationBonus` (bps) already evaluated for the pair.
    pub bonus_bps: U256,
    /// The liquidator's `debtToCover`.
    pub debt_to_cover: U256,
}

/// `LiquidationAmounts` plus the `amountToRestore` the liquidator pays.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Amounts {
    pub collateral_shares: U256,
    pub collateral_shares_to_liquidator: U256,
    pub drawn_shares: U256,
    pub premium_ray: U256,
    /// `rayMulUp(drawnShares, idx) + fromRayUp(premiumRay)`: raw debt units
    /// transferred from the liquidator.
    pub repay: U256,
}

/// `_calculateDebtToTargetHealthFactor`: RAY-scaled debt units.
fn debt_ray_to_target(p: &AmountsIn<'_>, debt_unit: U256, cf_bps: U256) -> Result<U256> {
    // liquidationPenalty = percentMulUp(bpsToWad(bonus), cf)
    let bonus_wad = p
        .bonus_bps
        .checked_mul(BPS_TO_WAD)
        .ok_or(FixedError::Overflow)?;
    let penalty = percent_mul_up(bonus_wad, cf_bps)?;
    let target = U256::from(p.meta.target_hf);
    let num = debt_unit
        .checked_mul(target.checked_sub(p.hf_wad).ok_or(FixedError::Underflow)?)
        .ok_or(FixedError::Overflow)?;
    let den = target
        .checked_sub(penalty)
        .ok_or(FixedError::Underflow)?
        .checked_mul(
            p.debt
                .p8
                .checked_mul(HF_THRESHOLD_WAD)
                .ok_or(FixedError::Overflow)?,
        )
        .ok_or(FixedError::Overflow)?;
    Ok(mul_div(p.total_debt_value_ray, num, den, Rounding::Up)?)
}

/// `_calculateDebtToLiquidate` → `(drawnSharesToLiquidate,
/// premiumDebtRayToLiquidate)`.
fn debt_to_liquidate(
    p: &AmountsIn<'_>,
    debt_unit: U256,
    cf_bps: U256,
    drawn: U256,
    premium: U256,
) -> Result<(U256, U256)> {
    let to_target = debt_ray_to_target(p, debt_unit, cf_bps)?;
    let mut prem = core::cmp::min(round_ray_up(to_target)?, premium);
    if p.debt_to_cover < from_ray_up(prem)? {
        prem = p
            .debt_to_cover
            .checked_mul(RAY)
            .ok_or(FixedError::Overflow)?;
    }
    let mut drawn_liq = U256::ZERO;
    if prem == premium && prem < to_target {
        let to_target_shares = div_up(to_target.wrapping_sub(prem), p.debt.idx)?;
        let cover = p
            .debt_to_cover
            .checked_sub(from_ray_up(prem)?)
            .ok_or(FixedError::Underflow)?;
        let to_cover_shares = mul_div(cover, RAY, p.debt.idx, Rounding::Down)?;
        drawn_liq = to_target_shares.min(to_cover_shares).min(drawn);
    }
    // debtRayRemaining = (drawn − liq) · idx + premium − prem
    let remaining = drawn
        .wrapping_sub(drawn_liq)
        .checked_mul(p.debt.idx)
        .and_then(|v| v.checked_add(premium))
        .ok_or(FixedError::Overflow)?
        .checked_sub(prem)
        .ok_or(FixedError::Underflow)?;
    let dust_ray = DUST_VALUE.checked_mul(RAY).ok_or(FixedError::Overflow)?;
    let leaves_dust =
        drawn_liq < drawn && to_value(remaining, p.debt.row.decimals, p.debt.p8)? < dust_ray;
    if leaves_dust {
        return Ok((drawn, premium));
    }
    Ok((drawn_liq, prem))
}

/// `_calculateCollateralToLiquidate`: collateral shares for a debt amount.
fn collateral_to_liquidate(
    p: &AmountsIn<'_>,
    coll_unit: U256,
    debt_unit: U256,
    coll_total: U256,
    drawn_liq: U256,
    prem_liq: U256,
) -> Result<U256> {
    let debt_ray = drawn_liq
        .checked_mul(p.debt.idx)
        .and_then(|v| v.checked_add(prem_liq))
        .ok_or(FixedError::Overflow)?;
    let num = p
        .debt
        .p8
        .checked_mul(coll_unit)
        .and_then(|v| v.checked_mul(p.bonus_bps))
        .ok_or(FixedError::Overflow)?;
    let den = debt_unit
        .checked_mul(p.coll.p8)
        .and_then(|v| v.checked_mul(BPS))
        .and_then(|v| v.checked_mul(RAY))
        .ok_or(FixedError::Overflow)?;
    let assets = mul_div(debt_ray, num, den, Rounding::Down)?;
    to_shares_down(
        assets,
        coll_total,
        U256::from(p.coll.reserve.hub.added_shares),
    )
}

/// `_calculateLiquidationAmounts`. `None` when the chain would revert
/// `MustNotLeaveDust` for this `debtToCover` (the caller's cap is below what
/// the dust rule forces).
pub(crate) fn amounts(p: &AmountsIn<'_>) -> Result<Option<Amounts>> {
    let coll_unit = asset_unit(p.coll.row.decimals)?;
    let debt_unit = asset_unit(p.debt.row.decimals)?;
    let cf_bps = U256::from(p.coll.user.collateral_factor);
    let supplied = U256::from(p.coll.supply_shares);
    let drawn = U256::from(p.debt.debt_shares);
    let premium = p.debt.premium_ray;
    let coll_total = total_added_assets(&p.coll.reserve.hub, p.coll.idx)?;
    let added = U256::from(p.coll.reserve.hub.added_shares);

    let (mut drawn_liq, mut prem_liq) = debt_to_liquidate(p, debt_unit, cf_bps, drawn, premium)?;
    let mut coll_liq =
        collateral_to_liquidate(p, coll_unit, debt_unit, coll_total, drawn_liq, prem_liq)?;

    let mut leaves_coll_dust = false;
    if coll_liq < supplied {
        let remaining = to_assets_down(supplied.wrapping_sub(coll_liq), coll_total, added)?;
        leaves_coll_dust = to_value(remaining, p.coll.row.decimals, p.coll.p8)? < DUST_VALUE;
    }
    if coll_liq > supplied || (leaves_coll_dust && drawn_liq < drawn) {
        coll_liq = supplied;
        let assets_up = to_assets_up(supplied, coll_total, added)?;
        let num = p
            .coll
            .p8
            .checked_mul(debt_unit)
            .and_then(|v| v.checked_mul(BPS))
            .and_then(|v| v.checked_mul(RAY))
            .ok_or(FixedError::Overflow)?;
        let den = p
            .debt
            .p8
            .checked_mul(coll_unit)
            .and_then(|v| v.checked_mul(p.bonus_bps))
            .ok_or(FixedError::Overflow)?;
        let debt_ray = mul_div(assets_up, num, den, Rounding::Up)?;
        if debt_ray <= premium {
            prem_liq = core::cmp::min(round_ray_up(debt_ray)?, premium);
            drawn_liq = U256::ZERO;
        } else {
            prem_liq = premium;
            drawn_liq = div_up(debt_ray.wrapping_sub(prem_liq), p.debt.idx)?;
            if drawn_liq > drawn {
                drawn_liq = drawn;
                coll_liq = collateral_to_liquidate(
                    p, coll_unit, debt_unit, coll_total, drawn_liq, prem_liq,
                )?
                .min(supplied);
            }
        }
    }
    let repay = ray_mul_up(drawn_liq, p.debt.idx)?
        .checked_add(from_ray_up(prem_liq)?)
        .ok_or(FixedError::Overflow)?;
    if p.debt_to_cover < repay {
        return Ok(None);
    }
    // collateralSharesToLiquidator = shares − mulDivUp(shares, fee·(LB−1e4), LB·1e4)
    let fee_num = U256::from(p.coll.user.liquidation_fee)
        .checked_mul(p.bonus_bps.checked_sub(BPS).ok_or(FixedError::Underflow)?)
        .ok_or(FixedError::Overflow)?;
    let fee_den = p.bonus_bps.checked_mul(BPS).ok_or(FixedError::Overflow)?;
    let fee_shares = mul_div(coll_liq, fee_num, fee_den, Rounding::Up)?;
    let to_liquidator = coll_liq
        .checked_sub(fee_shares)
        .ok_or(FixedError::Underflow)?;
    Ok(Some(Amounts {
        collateral_shares: coll_liq,
        collateral_shares_to_liquidator: to_liquidator,
        drawn_shares: drawn_liq,
        premium_ray: prem_liq,
        repay,
    }))
}

/// Inline capacity for the per-slot term list: V4's
/// `MAX_ALLOWED_USER_RESERVES_LIMIT` is 128 but a position past 16 held
/// reserves is not a hot-path concern for `quote()`.
type Terms<'a> = SmallVec<[SlotTerms<'a>; 16]>;

/// `Protocol::quote` body.
pub(crate) fn quote(
    pos: PositionRef<'_>,
    px: &PriceVector,
    cons: &Constraints,
) -> Result<Option<Quote>> {
    let mut acc = Account::new(pos.key.market);
    let mut terms: Terms<'_> = SmallVec::new();
    walk(
        &pos,
        |a| price_p8(px, a),
        |t| {
            acc.add(t)?;
            terms.push(*t);
            Ok(())
        },
    )?;
    let health = finish(&acc)?;
    if health.state != HealthState::Liquidatable {
        return Ok(None);
    }
    let hf_wad = acc.hf_wad()?;
    let meta: &SpokeMeta = pos
        .markets
        .first()
        .ok_or(ProtocolError::UnknownMarket(pos.key.market))?
        .body()?;
    let price_ray = |asset: AssetId| -> Result<Ray> {
        px.0.get(usize::from(asset.0))
            .filter(|p| p.asset == asset)
            .map(|p| p.price)
            .ok_or(ProtocolError::MissingPrice(asset))
    };

    // Seize options: every seizable collateral slot, bonus from the curve
    // at the current health, cross-checked against the chain's own bps.
    // (option, value for ordering, slot, chain bps for the amounts).
    let mut seize: SmallVec<[(SeizeOption, U256, u16, U256); 8]> = SmallVec::new();
    for t in terms.iter().filter(|t| t.seizable()) {
        let Some(c) = t.collateral else {
            continue;
        };
        let curve = bonus_curve(meta, t.user.max_liquidation_bonus)?;
        let bonus = curve
            .bonus_at_hf(health.hf)?
            .ok_or(ProtocolError::Internal)?;
        let chain_bps = liquidation_bonus_bps(
            U256::from(meta.hf_for_max_bonus),
            U256::from(meta.bonus_factor),
            hf_wad,
            U256::from(t.user.max_liquidation_bonus),
        )?;
        if chain_bps
            .checked_sub(BPS)
            .and_then(|b| b.checked_mul(BPS_RAY))
            != Some(bonus.raw())
        {
            return Err(ProtocolError::Internal);
        }
        let value = value_ray_of(c.assets, price_ray(t.row.asset)?, t.row.decimals)?;
        seize.push((
            SeizeOption {
                asset: t.row.asset,
                max_seize: c.assets,
                bonus,
                curve,
                call_target: alloy_primitives::Address::ZERO,
            },
            value,
            t.slot,
            chain_bps,
        ));
    }
    // bonus desc, value desc, slot asc — stable.
    seize.sort_by(|a, b| b.0.bonus.cmp(&a.0.bonus).then(b.1.cmp(&a.1)));
    let Some(&(_, _, best_slot, bonus_bps)) = seize.first() else {
        return Err(ProtocolError::EmptyQuote);
    };
    let coll = terms
        .iter()
        .find(|t| t.slot == best_slot)
        .ok_or(ProtocolError::Internal)?;

    // Repay options: every repayable debt slot, amounts against seize[0].
    let mut repay: SmallVec<[(RepayOption, U256); 4]> = SmallVec::new();
    for t in terms.iter().filter(|t| t.repayable()) {
        let price = price_ray(t.row.asset)?;
        let cap = cons.per_liquidation_notional_cap.raw();
        let debt_to_cover = if cap == U256::MAX {
            U256::MAX
        } else {
            // cap (WAD numeraire) → raw units: cap · 1e9 · 10^d / price_ray.
            mul_div(
                cap.checked_mul(WAD_RAY_RATIO).ok_or(FixedError::Overflow)?,
                asset_unit(t.row.decimals)?,
                price.raw(),
                Rounding::Down,
            )?
        };
        if debt_to_cover.is_zero() {
            continue;
        }
        let Some(a) = amounts(&AmountsIn {
            coll,
            debt: t,
            total_debt_value_ray: acc.debt_value_ray,
            hf_wad,
            meta,
            bonus_bps,
            debt_to_cover,
        })?
        else {
            continue;
        };
        let value = value_ray_of(a.repay, price, t.row.decimals)?;
        repay.push((
            RepayOption {
                asset: t.row.asset,
                max_repay: a.repay,
            },
            value,
        ));
    }
    repay.sort_by_key(|a| core::cmp::Reverse(a.1));
    if repay.is_empty() {
        return Err(ProtocolError::EmptyQuote);
    }
    Ok(Some(Quote {
        position: pos.id,
        key: *pos.key,
        repay_options: repay.into_iter().map(|(o, _)| o).collect(),
        seize_options: seize.into_iter().map(|(o, _, _, _)| o).collect(),
    }))
}
