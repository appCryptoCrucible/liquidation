//! Health from facade `_revertIfNotLiquidatable`:
//! `isUnhealthy = twvUSD < totalDebtUSD`; expired accounts are liquidatable
//! even if healthy. Pin `CreditFacadeV3` @ `510fc654`.

use alloy_primitives::U256;
use liq_protocol::{
    AssetMask, BlockReason, Health, HealthState, MarketFlags, MarketRow, MarketSlot, PositionRef,
    ProtocolError, Result,
};
use liq_types::fixed::FixedError;
use liq_types::{AssetId, PriceVector, Wad};

use crate::layout::{
    AccountExtra, ManagerRow, QuotaExtra, TokenRow, UNDERLYING_SLOT, UNMAPPED_ASSET,
};
use crate::math::{
    calc_accrued_interest, calc_total_debt, get_liquidation_threshold, hf_ray, interest_fee,
    is_expired, is_unhealthy, token_twv, value_wad,
};

#[derive(Copy, Clone, Debug)]
pub(crate) struct Terms<'a> {
    pub manager: &'a ManagerRow,
    pub debt_row: &'a MarketRow,
    pub debt: U256,
    /// `debt + accruedInterest` (no fees) — `_hasBadDebt`'s right side.
    pub debt_with_interest: U256,
    pub total_debt: U256,
    pub twv: U256,
    pub total_value: U256,
    pub p_underlying: U256,
    pub expired: bool,
    pub unhealthy: bool,
}

#[inline]
fn cell(v: &[u128], slot: u16) -> u128 {
    v.get(usize::from(slot)).copied().unwrap_or(0)
}

#[inline]
pub(crate) fn price_ray(
    px: &PriceVector,
    asset: AssetId,
    over: Option<(AssetId, U256)>,
) -> Result<U256> {
    if let Some((a, raw)) = over {
        if a == asset {
            return if raw.is_zero() {
                Err(ProtocolError::MissingPrice(asset))
            } else {
                Ok(raw)
            };
        }
    }
    if asset == UNMAPPED_ASSET {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    px.0.get(usize::from(asset.0))
        .filter(|p| p.asset == asset)
        .map(|p| p.price.raw())
        .filter(|r| !r.is_zero())
        .ok_or(ProtocolError::MissingPrice(asset))
}

fn extra(pos: PositionRef<'_>) -> Result<&AccountExtra> {
    pos.extra.view()
}

fn extra_quoted(pos: PositionRef<'_>, slot: u16) -> Result<bool> {
    let q: &QuotaExtra = pos
        .slot_extra
        .get(usize::from(slot))
        .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
            market: pos.key.market,
            slot,
        }))?
        .view()?;
    Ok(q.flags & QuotaExtra::QUOTED != 0)
}

fn manager_row(pos: PositionRef<'_>) -> Result<(&MarketRow, &ManagerRow)> {
    let row =
        pos.markets
            .get(usize::from(UNDERLYING_SLOT))
            .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
                market: pos.key.market,
                slot: UNDERLYING_SLOT,
            }))?;
    let m: &ManagerRow = row.body()?;
    if m.flags & ManagerRow::FEES == 0 {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    if row.flags.contains(MarketFlags::UNPRICED) || m.flags & ManagerRow::PRICED == 0 {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    Ok((row, m))
}

fn quota_usd(pos: PositionRef<'_>, slot: u16, p_underlying: U256, und_dec: u8) -> Result<U256> {
    if slot == UNDERLYING_SLOT {
        return Ok(U256::MAX);
    }
    let q: &QuotaExtra = pos
        .slot_extra
        .get(usize::from(slot))
        .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
            market: pos.key.market,
            slot,
        }))?
        .view()?;
    if q.flags & QuotaExtra::QUOTED == 0 {
        return Ok(U256::ZERO);
    }
    if q.quota == 0 {
        return Ok(U256::ZERO);
    }
    value_wad(U256::from(q.quota), p_underlying, und_dec)
}

pub(crate) fn token_lt(row: &MarketRow, slot: u16, now: u64, mgr: &ManagerRow) -> Result<u16> {
    if slot == UNDERLYING_SLOT {
        return Ok(mgr.lt_underlying);
    }
    let t: &TokenRow = row.body()?;
    if t.flags & TokenRow::LISTED == 0 {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    get_liquidation_threshold(t.lt_initial, t.lt_final, t.ramp_start, t.ramp_duration, now)
}

pub(crate) fn terms<'a>(
    pos: PositionRef<'a>,
    px: &PriceVector,
    over: Option<(AssetId, U256)>,
) -> Result<Terms<'a>> {
    let extra = extra(pos)?;
    let (debt_row, manager) = manager_row(pos)?;
    let debt = U256::from(cell(pos.debt, UNDERLYING_SLOT));
    let index_last = U256::from(extra.cumulative_index_last_update);
    // No invented IRM: if the account never stored an index, treat now == last
    // (zero accrued). Drift vs `baseInterestIndex()` is a probe gap.
    let index_now = if index_last.is_zero() {
        U256::ZERO
    } else {
        index_last
    };
    let accrued_interest = if debt.is_zero() || index_last.is_zero() {
        U256::ZERO
    } else {
        calc_accrued_interest(debt, index_last, index_now)?
    };
    let base_fee = interest_fee(accrued_interest, U256::from(manager.fee_interest))?;
    let quota_interest = U256::from(extra.cumulative_quota_interest);
    let quota_fee = interest_fee(quota_interest, U256::from(manager.fee_interest))?;
    let accrued_interest = accrued_interest
        .checked_add(quota_interest)
        .ok_or(FixedError::Overflow)?;
    let debt_with_interest = debt
        .checked_add(accrued_interest)
        .ok_or(FixedError::Overflow)?;
    let accrued_fees = U256::from(extra.quota_fees)
        .checked_add(base_fee)
        .and_then(|v| v.checked_add(quota_fee))
        .ok_or(FixedError::Overflow)?;
    let total_debt = calc_total_debt(debt, accrued_interest, accrued_fees)?;
    let p_underlying = price_ray(px, debt_row.asset, over)?;
    let total_debt_wad = value_wad(total_debt, p_underlying, debt_row.decimals)?;

    let mut twv = U256::ZERO;
    let mut total_value = U256::ZERO;
    let enabled = extra.enabled_tokens_mask;
    for slot in 0u16..u16::from(manager.token_count.max(1)) {
        let row = pos
            .markets
            .get(usize::from(slot))
            .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
                market: pos.key.market,
                slot,
            }))?;
        let bit = 1u64
            .checked_shl(u32::from(slot))
            .ok_or(FixedError::Overflow)?;
        let quoted = slot == UNDERLYING_SLOT || extra_quoted(pos, slot)?;
        let count_token = slot == UNDERLYING_SLOT || (enabled & bit != 0 && quoted);
        if !count_token {
            continue;
        }
        if row.asset == UNMAPPED_ASSET {
            return Err(ProtocolError::OracleSourceMismatch);
        }
        let bal = U256::from(cell(pos.supply, slot));
        if bal.is_zero() && slot != UNDERLYING_SLOT {
            continue;
        }
        let p = price_ray(px, row.asset, over)?;
        let value = value_wad(bal, p, row.decimals)?;
        let lt = U256::from(token_lt(row, slot, pos.timestamp, manager)?);
        let q_usd = quota_usd(pos, slot, p_underlying, debt_row.decimals)?;
        let w = token_twv(value, lt, q_usd)?;
        total_value = total_value.checked_add(value).ok_or(FixedError::Overflow)?;
        twv = twv.checked_add(w).ok_or(FixedError::Overflow)?;
    }
    let expired = is_expired(
        manager.expirable != 0,
        u64::from(manager.expiration_date),
        pos.timestamp,
    );
    let unhealthy = is_unhealthy(twv, total_debt_wad);
    Ok(Terms {
        manager,
        debt_row,
        debt,
        debt_with_interest,
        total_debt,
        twv,
        total_value,
        p_underlying,
        expired,
        unhealthy,
    })
}

pub(crate) fn finish(t: &Terms<'_>, pos: PositionRef<'_>) -> Result<Health> {
    let hf = hf_ray(t.twv, {
        let p = t.p_underlying;
        value_wad(t.total_debt, p, t.debt_row.decimals)?
    })?;
    // Recompute debt wad (same as terms) for the Health struct.
    let debt_wad = Wad::from_raw(value_wad(
        t.total_debt,
        t.p_underlying,
        t.debt_row.decimals,
    )?);
    let coll_wad = Wad::from_raw(t.total_value);
    let mut sensitivity = AssetMask::EMPTY;
    let extra = extra(pos)?;
    for slot in 0u16..u16::from(t.manager.token_count.max(1)) {
        let bit = 1u64
            .checked_shl(u32::from(slot))
            .ok_or(FixedError::Overflow)?;
        let quoted = slot == UNDERLYING_SLOT || extra_quoted(pos, slot)?;
        if slot == UNDERLYING_SLOT
            || cell(pos.supply, slot) > 0
            || (extra.enabled_tokens_mask & bit != 0 && quoted)
        {
            if let Some(s) = sensitivity.with(slot) {
                sensitivity = s;
            }
        }
    }
    let paused = pos
        .markets
        .get(usize::from(UNDERLYING_SLOT))
        .is_some_and(|r| r.flags.contains(MarketFlags::PAUSED));
    let state = if t.debt.is_zero() {
        HealthState::Healthy
    } else if paused {
        HealthState::Blocked {
            reason: BlockReason::Paused,
        }
    } else if t.unhealthy || t.expired {
        HealthState::Liquidatable
    } else {
        HealthState::Healthy
    };
    Ok(Health {
        hf,
        debt_value: debt_wad,
        collateral_value: coll_wad,
        price_sensitivity: sensitivity,
        state,
    })
}

pub(crate) fn health(pos: PositionRef<'_>, px: &PriceVector) -> Result<Health> {
    let t = terms(pos, px, None)?;
    finish(&t, pos)
}

pub(crate) fn hf_at_price(
    pos: PositionRef<'_>,
    px: &PriceVector,
    asset: AssetId,
    price: U256,
) -> Result<liq_types::Ray> {
    let t = terms(pos, px, Some((asset, price)))?;
    hf_ray(
        t.twv,
        value_wad(t.total_debt, t.p_underlying, t.debt_row.decimals)?,
    )
}
