//! Morpho `liquidate` amounts — full close factor, LIF bonus.

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
use crate::math::{
    asset_unit, bonus_ray, liquidation_incentive_factor, mul_div_down, mul_div_up, to_assets_down,
    to_assets_up, to_shares_up, w_div_up, w_mul_down, ORACLE_PRICE_SCALE,
};

pub(crate) fn quote(
    pos: PositionRef<'_>,
    px: &PriceVector,
    cons: &Constraints,
) -> Result<Option<Quote>> {
    let t0 = terms(pos)?;
    let (t, health) = finish(&t0, px)?;
    if health.state != HealthState::Liquidatable {
        return Ok(None);
    }
    let lif = liquidation_incentive_factor(U256::from(t.loan.lltv))?;
    let bonus = bonus_ray(lif)?;
    let curve = BonusCurve::Static { bonus };
    let at_hf = curve
        .bonus_at_hf(health.hf)?
        .ok_or(ProtocolError::Internal)?;
    if at_hf != bonus {
        return Err(ProtocolError::Internal);
    }

    let full_repay = to_assets_up(t.borrow_shares, t.borrow_a, t.borrow_s)?;
    let mut seized = to_assets_down(t.borrow_shares, t.borrow_a, t.borrow_s)?;
    seized = w_mul_down(seized, lif)?;
    seized = mul_div_down(seized, ORACLE_PRICE_SCALE, t.oracle)?;

    let (repay, seize) = if seized > t.collateral {
        let quoted = mul_div_up(t.collateral, t.oracle, ORACLE_PRICE_SCALE)?;
        let shares = to_shares_up(w_div_up(quoted, lif)?, t.borrow_a, t.borrow_s)?;
        let repay = to_assets_up(shares.min(t.borrow_shares), t.borrow_a, t.borrow_s)?;
        (repay, t.collateral)
    } else {
        (full_repay, seized)
    };

    let cap = cons.per_liquidation_notional_cap.raw();
    let repay = if cap == U256::MAX {
        repay
    } else {
        let raw_cap = mul_div(
            cap.checked_mul(WAD_RAY_RATIO).ok_or(FixedError::Overflow)?,
            asset_unit(t.loan_row.decimals)?,
            t.p_loan,
            Rounding::Down,
        )?;
        repay.min(raw_cap)
    };
    if repay.is_zero() {
        return Err(ProtocolError::EmptyQuote);
    }

    let mut repay_options = SmallVec::new();
    repay_options.push(RepayOption {
        asset: t.loan_row.asset,
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
