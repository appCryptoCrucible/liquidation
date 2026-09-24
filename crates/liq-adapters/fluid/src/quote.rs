//! Single-segment perfect-tick quote (top tick → liquidation tick).
//! Partials / tick-walk are fail-closed. T1 `encode` is 10E; T2/T3/T4 stay Unwired.

use alloy_primitives::U256;
use liq_protocol::SlotRef;
use liq_protocol::{
    BonusCurve, HealthState, LegChoice, PositionRef, ProtocolError, Quote, RepayOption, Result,
    SeizeOption,
};
use liq_types::fixed::FixedError;
use liq_types::PriceVector;
use smallvec::SmallVec;

use crate::health::{finish, terms};
use crate::layout::VaultExtra;
use crate::math::{
    bonus_ray, col_liquidated_from_debt, col_per_debt_with_penalty, col_per_unit_debt_1e18,
    debt_liquidated_to_ref, from_raw, get_ratio_at_tick, TICK_STATUS_PERFECT,
};

/// Wire `colPerUnitDebt_` from the quoted seize/repay pair.
/// Pin 1e18 slip — not FluidOracle 1e27, not internal `colPerDebt`.
pub fn col_per_unit_debt_1e18_from_quote(q: &Quote, legs: LegChoice) -> Result<U256> {
    let repay = q
        .repay_options
        .get(usize::from(legs.repay))
        .ok_or(ProtocolError::LegOutOfRange)?;
    let seize = q
        .seize_options
        .get(usize::from(legs.seize))
        .ok_or(ProtocolError::LegOutOfRange)?;
    col_per_unit_debt_1e18(seize.max_seize, repay.max_repay)
}

pub(crate) fn quote(pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Quote>> {
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

    // No caller-side notional cap (GUIDE 12 §4b): `repay`/`seize` are the
    // protocol's own ceiling — Fluid liquidates proportionally, and nothing
    // downstream of this adapter needs a second, adapter-shaped copy of the
    // viability band's sizing.

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
