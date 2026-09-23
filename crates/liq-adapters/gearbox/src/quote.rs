//! Partial repay-and-seize quote from `_calcPartialLiquidationPayments`.
//! Full close needs Gearbox adapter MultiCall fills — fail closed (no invented
//! swap profits). Expired-but-healthy still quotes when a seizable token exists.

use alloy_primitives::U256;
use liq_protocol::SlotRef;
use liq_protocol::{
    BonusCurve, Constraints, HealthState, PositionRef, ProtocolError, Quote, RepayOption, Result,
    SeizeOption,
};
use liq_types::fixed::{mul_div, FixedError, Rounding, WAD_RAY_RATIO};
use liq_types::PriceVector;
use smallvec::SmallVec;

use crate::health::terms;
use crate::layout::UNMAPPED_ASSET;
use crate::math::{asset_unit, bonus_ray, max_partial_amount, seized_from_amount, value_wad};

#[inline]
fn cell(v: &[u128], slot: u16) -> u128 {
    v.get(usize::from(slot)).copied().unwrap_or(0)
}

pub(crate) fn quote(
    pos: PositionRef<'_>,
    px: &PriceVector,
    cons: &Constraints,
) -> Result<Option<Quote>> {
    let t = terms(pos, px, None)?;
    let health = crate::health::finish(&t, pos)?;
    if health.state != HealthState::Liquidatable {
        return Ok(None);
    }
    let expired = t.expired && !t.unhealthy;
    let discount = U256::from(if expired {
        t.manager.liquidation_discount_expired
    } else {
        t.manager.liquidation_discount
    });
    let fee = U256::from(if expired {
        t.manager.fee_liquidation_expired
    } else {
        t.manager.fee_liquidation
    });
    let bonus = bonus_ray(discount)?;
    let curve = BonusCurve::Static { bonus };
    let at_hf = curve
        .bonus_at_hf(health.hf)?
        .ok_or(ProtocolError::Internal)?;
    if at_hf != bonus {
        return Err(ProtocolError::Internal);
    }

    let scale_u = asset_unit(t.debt_row.decimals)?;
    let mut best: Option<(SeizeOption, U256, U256)> = None;
    for slot in 1u16..u16::from(t.manager.token_count.max(1)) {
        let row = match pos.markets.get(usize::from(slot)) {
            Some(r) => r,
            None => continue,
        };
        if row.asset == UNMAPPED_ASSET {
            return Err(ProtocolError::OracleSourceMismatch);
        }
        if row.asset == t.debt_row.asset {
            continue;
        }
        let bal = U256::from(cell(pos.supply, slot));
        if bal.is_zero() {
            continue;
        }
        let p_t = crate::health::price_ray(px, row.asset, None)?;
        let scale_t = asset_unit(row.decimals)?;
        let amount = max_partial_amount(
            bal,
            t.total_debt,
            t.p_underlying,
            scale_u,
            p_t,
            scale_t,
            discount,
            fee,
        )?;
        if amount.is_zero() {
            continue;
        }
        let seize = seized_from_amount(amount, t.p_underlying, scale_u, p_t, scale_t, discount)?;
        if seize.is_zero() {
            continue;
        }
        let seize_value = value_wad(seize, p_t, row.decimals)?;
        let cand = (
            SeizeOption {
                asset: row.asset,
                max_seize: seize,
                bonus,
                curve,
                call_target: alloy_primitives::Address::ZERO,
                slot: SlotRef::ByAsset,
            },
            amount,
            seize_value,
        );
        let take = match best.as_ref() {
            None => true,
            Some(cur) => {
                cand.0.bonus > cur.0.bonus || (cand.0.bonus == cur.0.bonus && cand.2 > cur.2)
            }
        };
        if take {
            best = Some(cand);
        }
    }
    let Some((seize, mut amount, _)) = best else {
        // Full path: no seizable non-underlying token, or adapter MultiCall
        // unpriced. Do not invent swap fills.
        return Ok(None);
    };

    let cap = cons.per_liquidation_notional_cap.raw();
    if cap != U256::MAX {
        let raw_cap = mul_div(
            cap.checked_mul(WAD_RAY_RATIO).ok_or(FixedError::Overflow)?,
            scale_u,
            t.p_underlying,
            Rounding::Down,
        )?;
        if amount > raw_cap {
            amount = raw_cap;
        }
        if amount.is_zero() {
            return Ok(None);
        }
    }

    let mut repay_options = SmallVec::new();
    repay_options.push(RepayOption {
        asset: t.debt_row.asset,
        max_repay: amount,
        slot: SlotRef::ByAsset,
    });
    let mut seize_options = SmallVec::new();
    seize_options.push(seize);
    Ok(Some(Quote {
        position: pos.id,
        key: *pos.key,
        repay_options,
        seize_options,
    }))
}
