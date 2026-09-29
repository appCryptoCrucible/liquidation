//! `maxLiquidation` amounts — bonus = `collateralConfig.liquidationFee`.
//! `FullLiquidationRequired` if a cover below the computed repay is offered.

use liq_protocol::SlotRef;
use liq_protocol::{
    BonusCurve, HealthState, PositionRef, ProtocolError, Quote, RepayOption, Result, SeizeOption,
};
use liq_types::PriceVector;
use smallvec::SmallVec;

use alloy_primitives::U256;

use crate::health::{finish, terms, Terms};
use crate::math::{bonus_ray, max_liquidation, mul_div_down, UNDERESTIMATION};

pub(crate) fn quote(pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Quote>> {
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
    // No caller-side notional cap (GUIDE 12 §4b): `repay`/`seize` are the
    // protocol's own ceiling from `max_liquidation` above — except where the
    // collateral silo cannot pay it out.
    let (repay, seize) = within_liquidity(&t, repay, seize)?;

    let mut repay_options = SmallVec::new();
    repay_options.push(RepayOption {
        min_repay: alloy_primitives::U256::ZERO,
        pair_seize: None,
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

/// The Executor liquidates with `receiveSToken = false`, so the hook redeems
/// what it seized: protected collateral first (never lent out), then
/// collateral shares, which redeem only up to the silo's `getLiquidity()`
/// (collateral assets − debt assets). A seize past that reverts. Scale the
/// liquidation down to what can be paid out; the hook honours a smaller
/// `maxDebtToCover` unless it requires the whole debt (below bad debt, a
/// repay of more than 90% is rounded up to the whole debt), in which case
/// there is no quote.
fn within_liquidity(t: &Terms<'_>, repay: U256, seize: U256) -> Result<(U256, U256)> {
    let liquidity = U256::from(t.coll_body.total_collateral_assets)
        .saturating_sub(U256::from(t.coll_body.total_debt_assets));
    // The hook seizes `seize + UNDERESTIMATION`; a basis point of the
    // liquidity covers fees accrued before inclusion.
    let payable = t.prot_assets.saturating_add(mul_div_down(
        liquidity,
        U256::from(9_999u32),
        U256::from(10_000u32),
    )?);
    let needed = seize.saturating_add(UNDERESTIMATION);
    if needed <= payable {
        return Ok((repay, seize));
    }
    let bad_debt = t.debt_value >= t.coll_value;
    if repay >= t.debt_assets && !bad_debt {
        return Err(ProtocolError::EmptyQuote);
    }
    let cap = payable.saturating_sub(UNDERESTIMATION);
    let scaled_repay = mul_div_down(repay, cap, seize)?;
    let scaled_seize = mul_div_down(seize, scaled_repay, repay)?;
    if scaled_repay.is_zero() || scaled_seize.is_zero() {
        return Err(ProtocolError::EmptyQuote);
    }
    Ok((scaled_repay, scaled_seize))
}
