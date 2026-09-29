//! Quote: the vault's own liquidation, one option per token.
//!
//! Repay options are the debt tokens the liquidation can be paid in (one on
//! T1/T2, either of two on T3/T4); seize options the tokens the collateral
//! can be taken in (one on T1/T3, either of two on T2/T4). Each amount is
//! what the vault/DEX said the full liquidation is in that one token; any
//! repay pairs with any seize. `SlotRef::Slot` names the store slot, from
//! which the tail's one-token choice is derived.

use alloy_primitives::U256;
use liq_protocol::{
    BonusCurve, HealthState, PositionRef, ProtocolError, Quote, RepayOption, Result, SeizeOption,
    SlotRef,
};
use liq_types::fixed::{mul_div, Rounding, RAY};
use liq_types::{PriceVector, Ray};
use smallvec::SmallVec;

use crate::health::{cell, health, terms, value};
use crate::layout::UNMAPPED_ASSET;

pub(crate) fn quote(pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Quote>> {
    let h = health(pos, px)?;
    if h.state != HealthState::Liquidatable {
        return Ok(None);
    }
    let t = terms(&pos)?;
    let n_col = u16::from(t.body.n_col);
    let n_all = n_col.saturating_add(u16::from(t.body.n_debt));

    let mut repay: Vec<(RepayOption, U256)> = Vec::new();
    for s in n_col..n_all {
        let amt = cell(pos.debt, s);
        let row = pos
            .markets
            .get(usize::from(s))
            .ok_or(ProtocolError::Internal)?;
        if amt == 0 || row.asset == UNMAPPED_ASSET {
            continue;
        }
        let v = value(row, amt, px).ok_or(ProtocolError::MissingPrice(row.asset))?;
        repay.push((
            RepayOption {
                asset: row.asset,
                max_repay: U256::from(amt),
                min_repay: U256::ZERO,
                pair_seize: None,
                slot: SlotRef::Slot(s),
            },
            v,
        ));
    }
    if repay.is_empty() {
        return Ok(None);
    }
    // Repayable value descending; ties keep slot order.
    repay.sort_by_key(|r| core::cmp::Reverse(r.1));
    let pay_value = repay.first().map_or(U256::ZERO, |r| r.1);

    let mut seize: Vec<(SeizeOption, U256)> = Vec::new();
    for s in 0..n_col {
        let amt = cell(pos.supply, s);
        let row = pos
            .markets
            .get(usize::from(s))
            .ok_or(ProtocolError::Internal)?;
        if amt == 0 || row.asset == UNMAPPED_ASSET {
            continue;
        }
        let v = value(row, amt, px).ok_or(ProtocolError::MissingPrice(row.asset))?;
        // What the vault pays over the preferred repay, at these prices.
        // Below zero is a liquidation that loses: bonus 0, left to the
        // profit check.
        let bonus = if pay_value.is_zero() || v <= pay_value {
            Ray::ZERO
        } else {
            let gain = v.checked_sub(pay_value).ok_or(ProtocolError::Internal)?;
            Ray::from_raw(mul_div(gain, RAY, pay_value, Rounding::Down)?)
        };
        seize.push((
            SeizeOption {
                asset: row.asset,
                max_seize: U256::from(amt),
                bonus,
                curve: BonusCurve::Static { bonus },
                call_target: alloy_primitives::Address::ZERO,
                slot: SlotRef::Slot(s),
            },
            v,
        ));
    }
    if seize.is_empty() {
        return Ok(None);
    }
    // (bonus, value) descending; ties keep slot order.
    seize.sort_by_key(|s| core::cmp::Reverse((s.0.bonus, s.1)));

    Ok(Some(Quote {
        position: pos.id,
        key: *pos.key,
        repay_options: repay.into_iter().map(|(o, _)| o).collect::<SmallVec<_>>(),
        seize_options: seize.into_iter().map(|(o, _)| o).collect::<SmallVec<_>>(),
    }))
}
