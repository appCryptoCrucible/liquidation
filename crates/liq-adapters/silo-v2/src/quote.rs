//! `maxLiquidation` amounts — bonus = `collateralConfig.liquidationFee`.
//! `FullLiquidationRequired` if a cover below the computed repay is offered.

use alloy_primitives::U256;
use liq_protocol::{
    BonusCurve, Constraints, HealthState, PositionRef, ProtocolError, Quote, RepayOption, Result,
    SeizeOption,
};
use liq_types::fixed::{mul_div, FixedError, Rounding, WAD_RAY_RATIO};
use liq_types::PriceVector;
use smallvec::SmallVec;

use crate::health::{finish, terms};
use crate::math::{asset_unit, bonus_ray, full_liquidation_required, max_liquidation};

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
    let repay = if cap == U256::MAX {
        repay
    } else {
        let raw_cap = mul_div(
            cap.checked_mul(WAD_RAY_RATIO).ok_or(FixedError::Overflow)?,
            asset_unit(t.debt_row.decimals)?,
            t.p_debt,
            Rounding::Down,
        )?;
        if full_liquidation_required(repay, raw_cap) {
            repay
        } else {
            repay.min(raw_cap)
        }
    };
    if repay.is_zero() {
        return Err(ProtocolError::EmptyQuote);
    }

    let mut repay_options = SmallVec::new();
    repay_options.push(RepayOption {
        asset: t.debt_row.asset,
        max_repay: repay,
    });
    let mut seize_options = SmallVec::new();
    seize_options.push(SeizeOption {
        asset: t.coll_row.asset,
        max_seize: seize,
        bonus,
        curve,
    });
    Ok(Some(Quote {
        position: pos.id,
        key: *pos.key,
        repay_options,
        seize_options,
    }))
}
