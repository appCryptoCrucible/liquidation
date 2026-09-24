//! `maxLiquidation` amounts — bonus = `collateralConfig.liquidationFee`.
//! `FullLiquidationRequired` if a cover below the computed repay is offered.

use liq_protocol::SlotRef;
use liq_protocol::{
    BonusCurve, HealthState, PositionRef, ProtocolError, Quote, RepayOption, Result, SeizeOption,
};
use liq_types::PriceVector;
use smallvec::SmallVec;

use crate::health::{finish, terms};
use crate::math::{bonus_ray, max_liquidation};

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
    // protocol's own ceiling from `max_liquidation` above.

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
