//! `maxLiquidation` amounts — bonus = `collateralConfig.liquidationFee`.
//! `FullLiquidationRequired` if a cover below the computed repay is offered.

use alloy_primitives::U256;
use liq_protocol::SlotRef;
use liq_protocol::{
    BonusCurve, Constraints, HealthState, PositionRef, ProtocolError, Quote, RepayOption, Result,
    SeizeOption,
};
use liq_types::fixed::{mul_div, FixedError, Rounding, WAD_RAY_RATIO};
use liq_types::PriceVector;
use smallvec::SmallVec;

use crate::health::{finish, terms};
use crate::math::{asset_unit, bonus_ray, max_liquidation};

pub(crate) fn quote(
    pos: PositionRef<'_>,
    px: &PriceVector,
    cons: &Constraints,
) -> Result<Option<Quote>> {
    let t0 = terms(pos)?;
    let (t, health) = finish(&t0, px)?;
    // BadDebt is zero collateral only (`NoCollateralToLiquidate`). LTV ≥ 1e18
    // with coll remaining is Liquidatable and quotes `maxLiquidation`.
    if health.state != HealthState::Liquidatable {
        return Ok(None);
    }
    if t.lt.is_zero() {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    let bonus = bonus_ray(t.fee)?;
    let curve = BonusCurve::Static { bonus };
    let at_hf = curve
        .bonus_at_hf(health.hf)?
        .ok_or(ProtocolError::Internal)?;
    if at_hf != bonus {
        return Err(ProtocolError::Internal);
    }

    let (seize, repay) = max_liquidation(
        t.sum_coll_assets,
        t.coll_value,
        t.debt_assets,
        t.debt_value,
        t.target_ltv,
        t.fee,
    )?;
    if repay.is_zero() || seize.is_zero() {
        return Err(ProtocolError::EmptyQuote);
    }

    let cap = cons.per_liquidation_notional_cap.raw();
    let mut seize = seize;
    let repay = if cap == U256::MAX {
        repay
    } else {
        let raw_cap = mul_div(
            cap.checked_mul(WAD_RAY_RATIO).ok_or(FixedError::Overflow)?,
            asset_unit(t.debt_row.decimals)?,
            t.p_debt,
            Rounding::Down,
        )?;
        // T9. This used to branch on `full_liquidation_required(repay,
        // raw_cap)` (`repay > raw_cap`): that branch returned `repay`
        // unclamped, and the other arm was `repay.min(raw_cap)`, which is
        // also `repay` whenever `repay <= raw_cap`. Both arms returned
        // `repay` — the operator's notional cap never bound on any Silo leg.
        // `repay.min(raw_cap)` alone is the whole fix.
        let capped = repay.min(raw_cap);
        if capped < repay && !repay.is_zero() {
            // Silo's collateral-to-liquidate scales linearly with the repay
            // value (`max_liquidation`'s `calculate_collateral_to_liquidate`
            // is applied to `repay_value`), so a capped repay must carry a
            // proportionally smaller `max_seize` — otherwise the router sizes
            // an exit for collateral that will not actually be seized at the
            // reduced repay.
            seize = mul_div(seize, capped, repay, Rounding::Down)?;
        }
        capped
    };
    if repay.is_zero() || seize.is_zero() {
        return Err(ProtocolError::EmptyQuote);
    }

    let mut repay_options = SmallVec::new();
    repay_options.push(RepayOption {
        asset: t.debt_row.asset,
        max_repay: repay,
        slot: SlotRef::ByAsset,
    });
    let mut seize_options = SmallVec::new();
    seize_options.push(SeizeOption {
        asset: t.coll_row.asset,
        max_seize: seize,
        bonus,
        curve,
        call_target: alloy_primitives::Address::ZERO,
        slot: SlotRef::ByAsset,
    });
    Ok(Some(Quote {
        position: pos.id,
        key: *pos.key,
        repay_options,
        seize_options,
    }))
}
