//! Two legs per liquidatable account, both on one collateral token:
//!
//! * **Partial** (`repay_options[0]` ↔ `seize_options[0]`) —
//!   `_calcPartialLiquidationPayments`: transfer `A` underlying, seize that
//!   token at the discount. Gearbox then requires the account healthy
//!   (`_fullCollateralCheck`, min HF 1) and its debt ≥ `minDebt`
//!   (`BorrowAmountOutOfLimitsException`), so `A` lives in a window:
//!   `min_repay` restores health, `max_repay` keeps the minimum debt (and
//!   stays within the token balance). No window → no partial leg.
//! * **Full** (`repay_options[1]` ↔ `seize_options[1]`, all-or-nothing) —
//!   `liquidateCreditAccount` with `addCollateral(underlying, X)` +
//!   `withdrawCollateral(token, max)`. `CreditLogic.calcLiquidationPayments`
//!   makes the account keep `totalValue · discount` of value in total
//!   (pool + borrower), valued at Gearbox's oracle; what it already holds in
//!   underlying and in other tokens counts, so
//!   `X = totalValue · discount − underlying − value(other tokens)` plus a
//!   margin for our prices vs Gearbox's (returned by the manager as surplus
//!   underlying). The liquidator keeps `totalValue · (1 − discount)`.
//!   Not offered on bad debt (`_hasBadDebt`): v3.1 then asks the market's
//!   loss policy, which may refuse a public liquidator.
//!
//! Expired-but-healthy accounts quote at the expired discount/fee.

use alloy_primitives::U256;
use liq_protocol::SlotRef;
use liq_protocol::{
    BonusCurve, HealthState, PositionRef, ProtocolError, Quote, RepayOption, Result, SeizeOption,
};
use liq_types::fixed::RAY;
use liq_types::{PriceVector, Ray};
use smallvec::SmallVec;

use crate::health::{terms, Terms};
use crate::layout::UNMAPPED_ASSET;
use crate::math::{
    amount_from_wad, asset_unit, bonus_ray, has_bad_debt, max_partial_amount, mul_div_down,
    seized_from_amount, value_wad, wad_to_underlying, PERCENTAGE_FACTOR,
};

/// Headroom on the partial lower bound (our prices vs Gearbox's), bps.
pub const PARTIAL_MIN_MARGIN_BPS: u64 = 100;

/// Over-add on a full liquidation, as bps of the account's total value:
/// covers our price vector vs Gearbox's own feeds. The manager returns
/// whatever is not owed as underlying, which the plan sweeps.
pub const FULL_MARGIN_BPS: u64 = 50;

#[inline]
fn cell(v: &[u128], slot: u16) -> u128 {
    v.get(usize::from(slot)).copied().unwrap_or(0)
}

pub(crate) fn quote(pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Quote>> {
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
    let mut best: Option<(U256, SeizeOption, U256, U256)> = None;
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
        let lt = U256::from(crate::health::token_lt(
            row,
            slot,
            pos.timestamp,
            t.manager,
        )?);
        let Some((lo, hi)) = partial_window(&t, amount, discount, fee, lt)? else {
            continue;
        };
        let amount = hi;
        let seize = seized_from_amount(amount, t.p_underlying, scale_u, p_t, scale_t, discount)?;
        if seize.is_zero() {
            continue;
        }
        let seize_value = value_wad(seize, p_t, row.decimals)?;
        let cand = (
            lo,
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
                cand.1.bonus > cur.1.bonus || (cand.1.bonus == cur.1.bonus && cand.3 > cur.3)
            }
        };
        if take {
            best = Some(cand);
        }
    }
    // Full path on the largest non-underlying holding.
    let full = full_leg(&t, pos, px, discount)?;

    let mut repay_options = SmallVec::new();
    let mut seize_options = SmallVec::new();
    if let Some((lo, seize, amount, _)) = best {
        // No caller-side notional cap (GUIDE 12 §4b): `amount`/`seize` are
        // the protocol's own ceiling.
        repay_options.push(RepayOption {
            asset: t.debt_row.asset,
            max_repay: amount,
            min_repay: lo,
            pair_seize: Some(0),
            slot: SlotRef::ByAsset,
        });
        seize_options.push(seize);
    }
    if let Some((seize, x)) = full {
        let k = u8::try_from(seize_options.len()).map_err(|_| ProtocolError::Internal)?;
        repay_options.push(RepayOption {
            asset: t.debt_row.asset,
            max_repay: x,
            min_repay: x,
            pair_seize: Some(k),
            slot: SlotRef::ByAsset,
        });
        seize_options.push(seize);
    }
    if repay_options.is_empty() {
        return Ok(None);
    }
    Ok(Some(Quote {
        position: pos.id,
        key: *pos.key,
        repay_options,
        seize_options,
    }))
}

/// `[lo, hi]` for a partial `A` (underlying transferred): `lo` restores
/// health, `hi = min(balance bound, (debt − minDebt) / (1 − fee))`.
/// Health after (underlying units, TW = twv):
/// `TW − lt·A/disc ≥ D − A·(1 − fee)` ⇒ `A ≥ (D − TW) / ((1 − fee) − lt/disc)`.
/// A quota-capped token loses less twv than `lt·value`, so `lo` is on the
/// safe (high) side. `None` when the slice can never restore health or the
/// window is empty.
fn partial_window(
    t: &Terms<'_>,
    balance_bound: U256,
    discount: U256,
    fee: U256,
    lt: U256,
) -> Result<Option<(U256, U256)>> {
    if balance_bound.is_zero() || discount.is_zero() {
        return Ok(None);
    }
    let pf = PERCENTAGE_FACTOR;
    let keep = pf.checked_sub(fee).ok_or(ProtocolError::Internal)?;
    // k · pf · disc = (pf − fee)·disc − lt·pf
    let pos_part = keep.checked_mul(discount).ok_or(ProtocolError::Internal)?;
    let neg_part = lt.checked_mul(pf).ok_or(ProtocolError::Internal)?;
    let Some(k) = pos_part.checked_sub(neg_part).filter(|k| !k.is_zero()) else {
        return Ok(None);
    };
    let und_dec = t.debt_row.decimals;
    let tw = amount_from_wad(t.twv, t.p_underlying, und_dec)?;
    let gap = t.total_debt.saturating_sub(tw);
    let lo = if gap.is_zero() {
        U256::ZERO
    } else {
        let scale = pf.checked_mul(discount).ok_or(ProtocolError::Internal)?;
        let base = mul_div_down(gap, scale, k)?.saturating_add(U256::ONE);
        mul_div_down(
            base,
            U256::from(10_000u64.saturating_add(PARTIAL_MIN_MARGIN_BPS)),
            U256::from(10_000u64),
        )?
    };
    let min_debt = U256::from(u128::from_be_bytes(t.manager.min_debt));
    let room = t.total_debt.saturating_sub(min_debt);
    let debt_bound = mul_div_down(room, pf, keep)?;
    let hi = balance_bound.min(debt_bound);
    if hi.is_zero() || lo > hi {
        return Ok(None);
    }
    Ok(Some((lo, hi)))
}

/// The full-liquidation leg: `(seize option, X)`, or `None` when the account
/// has bad debt, no non-underlying holding, or `X` is not positive.
fn full_leg(
    t: &Terms<'_>,
    pos: PositionRef<'_>,
    px: &PriceVector,
    discount: U256,
) -> Result<Option<(SeizeOption, U256)>> {
    let und_dec = t.debt_row.decimals;
    let total_u = wad_to_underlying(t.total_value, t.p_underlying, und_dec)?;
    let bad = has_bad_debt(
        total_u,
        U256::from(t.manager.liquidation_discount),
        t.debt,
        t.debt_with_interest.saturating_sub(t.debt),
    )?;
    if bad {
        return Ok(None);
    }
    let mut pick: Option<(u16, U256, U256)> = None; // (slot, balance, value wad)
    for slot in 1u16..u16::from(t.manager.token_count.max(1)) {
        let Some(row) = pos.markets.get(usize::from(slot)) else {
            continue;
        };
        if row.asset == UNMAPPED_ASSET {
            return Err(ProtocolError::OracleSourceMismatch);
        }
        if row.asset == t.debt_row.asset {
            continue;
        }
        let bal = U256::from(cell(pos.supply, slot));
        // `withdrawCollateral(max)` leaves 1 wei; nothing to take below 2.
        if bal <= U256::from(1u8) {
            continue;
        }
        let p_t = crate::health::price_ray(px, row.asset, None)?;
        let v = value_wad(bal, p_t, row.decimals)?;
        if pick.as_ref().is_none_or(|(_, _, pv)| v > *pv) {
            pick = Some((slot, bal, v));
        }
    }
    let Some((slot, bal, v_t_wad)) = pick else {
        return Ok(None);
    };
    let row = pos
        .markets
        .get(usize::from(slot))
        .ok_or(ProtocolError::Internal)?;
    let u0 = U256::from(cell(pos.supply, crate::layout::UNDERLYING_SLOT));
    let v_t = amount_from_wad(v_t_wad, t.p_underlying, und_dec)?;
    // Everything the account keeps besides `token`: its underlying and the
    // other counted holdings (total value is only what Gearbox counts).
    let others = total_u.saturating_sub(v_t).saturating_sub(u0);
    let keep = mul_div_down(total_u, discount, PERCENTAGE_FACTOR)?;
    let margin = mul_div_down(total_u, U256::from(FULL_MARGIN_BPS), U256::from(10_000u64))?;
    let x = keep
        .saturating_sub(u0)
        .saturating_sub(others)
        .checked_add(margin)
        .ok_or(ProtocolError::Internal)?;
    if x.is_zero() || v_t <= x {
        return Ok(None);
    }
    // Bonus such that `x · (1 + bonus)` is worth what is withdrawn.
    let bonus = mul_div_down(v_t.checked_sub(x).ok_or(ProtocolError::Internal)?, RAY, x)?;
    let bonus = Ray::from_raw(bonus);
    Ok(Some((
        SeizeOption {
            asset: row.asset,
            max_seize: bal
                .checked_sub(U256::from(1u8))
                .ok_or(ProtocolError::Internal)?,
            bonus,
            curve: BonusCurve::Static { bonus },
            call_target: alloy_primitives::Address::ZERO,
            slot: SlotRef::ByAsset,
        },
        x,
    )))
}
