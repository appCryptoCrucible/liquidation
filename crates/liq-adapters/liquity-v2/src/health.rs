//! `getCurrentICR` @ `c8a5a4ee`: entire coll/debt (redistribution + interest)
//! then `LiquityMath._computeCR`. `hf = ICR / MCR` so `1.0` is the seize
//! boundary. `HealthState::Liquidatable` iff `ICR < MCR` and the trove is
//! `active` or `zombie` — hard threshold, never `SoftLiquidating`.

use alloy_primitives::U256;
use liq_protocol::{
    AssetMask, Health, HealthState, MarketFlags, MarketRow, MarketSlot, PositionRef, ProtocolError,
    Result,
};
use liq_types::fixed::FixedError;
use liq_types::{AssetId, PriceVector, Wad};

use crate::layout::{
    BranchRow, TroveCollExtra, TroveDebtExtra, TroveExtra, BOLD_SLOT, COLL_SLOT, UNMAPPED_ASSET,
};
use crate::math::{
    calc_interest, compute_cr, hf_from_icr, interest_period, price_wad_from_ray, redist_gain,
    value_wad,
};

#[derive(Copy, Clone, Debug)]
pub(crate) struct Terms<'a> {
    pub branch: &'a BranchRow,
    pub loan_row: &'a MarketRow,
    pub coll_row: &'a MarketRow,
    pub extra: TroveExtra,
    pub entire_debt: U256,
    pub entire_coll: U256,
    pub p_coll: U256,
    pub p_bold: U256,
    pub p_weth: U256,
}

#[inline]
fn cell(v: &[u128], slot: u16) -> u128 {
    v.get(usize::from(slot)).copied().unwrap_or(0)
}

#[inline]
pub(crate) fn price_ray(px: &PriceVector, asset: AssetId) -> Result<U256> {
    px.0.get(usize::from(asset.0))
        .filter(|p| p.asset == asset)
        .map(|p| p.price.raw())
        .filter(|r| !r.is_zero())
        .ok_or(ProtocolError::MissingPrice(asset))
}

#[inline]
fn extra_trove(pos: PositionRef<'_>) -> Result<TroveExtra> {
    Ok(*pos.extra.view::<TroveExtra>()?)
}

#[inline]
fn extra_debt(pos: PositionRef<'_>) -> Result<TroveDebtExtra> {
    pos.slot_extra
        .get(usize::from(BOLD_SLOT))
        .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
            market: pos.key.market,
            slot: BOLD_SLOT,
        }))?
        .view::<TroveDebtExtra>()
        .copied()
}

#[inline]
fn extra_coll(pos: PositionRef<'_>) -> Result<TroveCollExtra> {
    pos.slot_extra
        .get(usize::from(COLL_SLOT))
        .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
            market: pos.key.market,
            slot: COLL_SLOT,
        }))?
        .view::<TroveCollExtra>()
        .copied()
}

/// Batch-pro-rata recorded debt, or the trove's own recorded debt.
#[inline]
fn recorded_debt(stored: U256, d: &TroveDebtExtra) -> Result<U256> {
    if d.batch_total_shares == 0 {
        return Ok(stored);
    }
    mul_ratio(
        U256::from(d.batch_recorded_debt),
        U256::from(d.batch_debt_shares),
        U256::from(d.batch_total_shares),
    )
}

#[inline]
fn mul_ratio(a: U256, b: U256, d: U256) -> Result<U256> {
    crate::math::mul_div_down(a, b, d)
}

fn accrued_parts(
    recorded: U256,
    extra: &TroveExtra,
    coll_x: &TroveCollExtra,
    ts: u64,
    shutdown: u64,
) -> Result<(U256, U256)> {
    let period = interest_period(u64::from(extra.last_debt_update), shutdown, ts)?;
    let rate = U256::from(extra.annual_interest_rate);
    let fee = U256::from(coll_x.batch_management_fee);
    let w_rate = recorded.checked_mul(rate).ok_or(FixedError::Overflow)?;
    let w_fee = recorded.checked_mul(fee).ok_or(FixedError::Overflow)?;
    Ok((
        calc_interest(w_rate, period)?,
        calc_interest(w_fee, period)?,
    ))
}

pub(crate) fn terms(pos: PositionRef<'_>) -> Result<Terms<'_>> {
    let loan_row = pos
        .markets
        .get(usize::from(BOLD_SLOT))
        .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
            market: pos.key.market,
            slot: BOLD_SLOT,
        }))?;
    let coll_row = pos
        .markets
        .get(usize::from(COLL_SLOT))
        .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
            market: pos.key.market,
            slot: COLL_SLOT,
        }))?;
    if loan_row.flags.contains(MarketFlags::UNPRICED)
        || coll_row.flags.contains(MarketFlags::UNPRICED)
        || loan_row.asset == UNMAPPED_ASSET
        || coll_row.asset == UNMAPPED_ASSET
    {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    let branch: &BranchRow = loan_row.body()?;
    let extra = extra_trove(pos)?;
    let debt_x = extra_debt(pos)?;
    let coll_x = extra_coll(pos)?;
    let stored_debt = U256::from(cell(pos.debt, BOLD_SLOT));
    let recorded = recorded_debt(stored_debt, &debt_x)?;
    let stake = U256::from(extra.stake);
    let redist_d = redist_gain(
        stake,
        U256::from(branch.l_bold_debt),
        U256::from(debt_x.snapshot_bold),
    )?;
    let redist_c = redist_gain(
        stake,
        U256::from(branch.l_coll),
        U256::from(coll_x.snapshot_coll),
    )?;
    let shutdown = u64::from(branch.shutdown_time);
    let (interest, batch_fee) = accrued_parts(recorded, &extra, &coll_x, pos.timestamp, shutdown)?;
    let entire_debt = recorded
        .checked_add(redist_d)
        .and_then(|v| v.checked_add(interest))
        .and_then(|v| v.checked_add(batch_fee))
        .ok_or(FixedError::Overflow)?;
    let recorded_coll = U256::from(cell(pos.supply, COLL_SLOT));
    let entire_coll = recorded_coll
        .checked_add(redist_c)
        .ok_or(FixedError::Overflow)?;
    Ok(Terms {
        branch,
        loan_row,
        coll_row,
        extra,
        entire_debt,
        entire_coll,
        p_coll: U256::ZERO,
        p_bold: U256::ZERO,
        p_weth: U256::ZERO,
    })
}

pub(crate) fn finish<'a>(
    t: &Terms<'a>,
    px: &PriceVector,
    weth: AssetId,
) -> Result<(Terms<'a>, Health)> {
    let p_coll = price_ray(px, t.coll_row.asset)?;
    let p_bold = price_ray(px, t.loan_row.asset)?;
    let p_weth = price_ray(px, weth)?;
    let price_wad = price_wad_from_ray(p_coll)?;
    let icr = compute_cr(t.entire_coll, t.entire_debt, price_wad)?;
    let mcr = U256::from(t.branch.mcr);
    let hf = hf_from_icr(icr, mcr)?;
    let mut sensitivity = AssetMask::EMPTY;
    if t.entire_coll > U256::ZERO || t.entire_debt > U256::ZERO {
        sensitivity = sensitivity
            .with(BOLD_SLOT)
            .and_then(|s| s.with(COLL_SLOT))
            .ok_or(ProtocolError::Internal)?;
    }
    let debt_value = Wad::from_raw(value_wad(t.entire_debt, p_bold, t.loan_row.decimals)?);
    let collateral_value = Wad::from_raw(value_wad(t.entire_coll, p_coll, t.coll_row.decimals)?);
    let open = extra_is_open(&t.extra) && t.entire_debt > U256::ZERO;
    let state = if !open {
        HealthState::Healthy
    } else if t.loan_row.flags.contains(MarketFlags::PAUSED) {
        HealthState::Blocked {
            reason: liq_protocol::BlockReason::Paused,
        }
    } else if icr < mcr {
        HealthState::Liquidatable
    } else {
        HealthState::Healthy
    };
    let mut t = *t;
    t.p_coll = p_coll;
    t.p_bold = p_bold;
    t.p_weth = p_weth;
    Ok((
        t,
        Health {
            hf,
            debt_value,
            collateral_value,
            price_sensitivity: sensitivity,
            state,
        },
    ))
}

#[inline]
fn extra_is_open(e: &TroveExtra) -> bool {
    e.is_active_or_zombie()
}

pub(crate) fn health(pos: PositionRef<'_>, px: &PriceVector, weth: AssetId) -> Result<Health> {
    let t = terms(pos)?;
    Ok(finish(&t, px, weth)?.1)
}
