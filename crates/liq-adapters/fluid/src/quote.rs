//! Single-segment perfect-tick quote (top tick → liquidation tick).
//! Partials / tick-walk are fail-closed. T1 `encode` is 10E; T2/T3/T4 stay Unwired.

use alloy_primitives::U256;
use liq_protocol::{
    BonusCurve, Constraints, HealthState, PositionRef, ProtocolError, Quote, RepayOption, Result,
    SeizeOption,
};
use liq_types::fixed::{mul_div, FixedError, Rounding, WAD_RAY_RATIO};
use liq_types::PriceVector;
use smallvec::SmallVec;

use crate::health::{finish, terms};
use crate::layout::VaultExtra;
use crate::math::{
    asset_unit, bonus_ray, col_liquidated_from_debt, col_per_debt_with_penalty,
    debt_liquidated_to_ref, from_raw, get_ratio_at_tick, TICK_STATUS_PERFECT,
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
    if t.extra.flags & VaultExtra::TOP_KNOWN == 0 && t.extra.absorbed_debt_raw == 0 {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    if t.body.liq_penalty > crate::math::X10 {
        return Err(ProtocolError::Internal);
    }
    let bonus = bonus_ray(t.body.liq_penalty)?;
    let curve = BonusCurve::Static { bonus };
    let at_hf = curve
        .bonus_at_hf(health.hf)?
        .ok_or(ProtocolError::Internal)?;
    if at_hf != bonus {
        return Err(ProtocolError::Internal);
    }

    let mut repay_raw = U256::from(t.extra.absorbed_debt_raw);
    let mut seize_raw = U256::from(t.extra.absorbed_col_raw);
    if t.extra.flags & VaultExtra::TOP_KNOWN != 0 && t.top_tick > t.liq_tick {
        if t.extra.tick_status != TICK_STATUS_PERFECT {
            return Err(ProtocolError::OracleSourceMismatch);
        }
        let ratio = get_ratio_at_tick(t.top_tick)?;
        let ref_ratio = get_ratio_at_tick(t.liq_tick)?;
        let col_per = col_per_debt_with_penalty(t.raw_debt_per_col, t.body.liq_penalty)?;
        let d = debt_liquidated_to_ref(t.debt_raw, ratio, ref_ratio, col_per)?;
        let c = col_liquidated_from_debt(d, col_per)?;
        repay_raw = repay_raw.checked_add(d).ok_or(FixedError::Overflow)?;
        seize_raw = seize_raw.checked_add(c).ok_or(FixedError::Overflow)?;
    }
    if repay_raw.is_zero() || seize_raw.is_zero() {
        return Err(ProtocolError::EmptyQuote);
    }
    let mut repay = from_raw(repay_raw, U256::from(t.body.borrow_ex_price))?;
    let mut seize = from_raw(seize_raw, U256::from(t.body.supply_ex_price))?;
    repay = repay.min(t.debt_tokens);
    seize = seize.min(t.col_tokens);
    if repay.is_zero() || seize.is_zero() {
        return Err(ProtocolError::EmptyQuote);
    }

    let cap = cons.per_liquidation_notional_cap.raw();
    if cap != U256::MAX {
        let raw_cap = mul_div(
            cap.checked_mul(WAD_RAY_RATIO).ok_or(FixedError::Overflow)?,
            asset_unit(t.debt_row.decimals)?,
            t.p_debt,
            Rounding::Down,
        )?;
        repay = repay.min(raw_cap);
        if repay.is_zero() {
            return Err(ProtocolError::EmptyQuote);
        }
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
