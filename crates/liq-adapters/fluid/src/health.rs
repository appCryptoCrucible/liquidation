//! Health from T1 `liquidate` tick/threshold @ `9496626f`.
//! Liquidatable iff `top_tick > liquidation_tick` (or absorbed debt remains).
//! `BadDebt` only when debt remains and there is nothing to seize.

use alloy_primitives::U256;
use liq_protocol::{
    AssetMask, BlockReason, Health, HealthState, MarketFlags, MarketRow, MarketSlot, PositionRef,
    ProtocolError, Result,
};
use liq_types::fixed::{FixedError, WAD_RAY_RATIO};
use liq_types::{AssetId, PriceVector, Ray, Wad};

use crate::layout::{VaultExtra, VaultRow, SLOT0};
use crate::math::{
    from_raw, hf_from_ticks, liquidation_tick, oracle_debt_per_col_1e27, raw_debt_per_col,
    TICK_STATUS_PERFECT,
};

#[derive(Copy, Clone, Debug)]
pub(crate) struct Terms<'a> {
    pub body: &'a VaultRow,
    pub extra: &'a VaultExtra,
    pub coll_slot: u16,
    pub debt_slot: u16,
    pub coll_row: &'a MarketRow,
    pub debt_row: &'a MarketRow,
    pub col_tokens: U256,
    pub debt_tokens: U256,
    #[allow(dead_code)]
    pub col_raw: U256,
    pub debt_raw: U256,
    pub p_coll: U256,
    pub p_debt: U256,
    pub oracle_1e27: U256,
    pub raw_debt_per_col: U256,
    pub liq_tick: i32,
    pub top_tick: i32,
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

fn row_at<'a>(pos: PositionRef<'a>, slot: u16) -> Result<&'a MarketRow> {
    pos.markets
        .get(usize::from(slot))
        .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
            market: pos.key.market,
            slot,
        }))
}

fn viewed(row: &MarketRow) -> Result<&VaultRow> {
    let b: &VaultRow = row.body()?;
    if b.flags & VaultRow::VIEWED == 0 {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    if row.flags.contains(MarketFlags::UNPRICED) || b.flags & VaultRow::PRICED == 0 {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    if b.flags & VaultRow::EX_KNOWN == 0 || b.supply_ex_price == 0 || b.borrow_ex_price == 0 {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    if !b.is_t1_token_pair() {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    Ok(b)
}

pub(crate) fn terms(pos: PositionRef<'_>) -> Result<Terms<'_>> {
    let extra: &VaultExtra = pos.extra.view()?;
    let coll_slot = SLOT0;
    let coll_row = row_at(pos, coll_slot)?;
    let body = viewed(coll_row)?;
    let debt_slot = body.debt_slot();
    let debt_row = row_at(pos, debt_slot)?;
    let _ = viewed(debt_row)?;
    // Columns persist pin raw. Tokens = raw * ex / 1e12. Do not invert
    // stored amounts through the current ex (that is the post-interest drift).
    let col_raw = U256::from(cell(pos.supply, coll_slot));
    let debt_raw = U256::from(cell(pos.debt, debt_slot));
    let col_tokens = from_raw(col_raw, U256::from(body.supply_ex_price))?;
    let debt_tokens = from_raw(debt_raw, U256::from(body.borrow_ex_price))?;
    Ok(Terms {
        body,
        extra,
        coll_slot,
        debt_slot,
        coll_row,
        debt_row,
        col_tokens,
        debt_tokens,
        col_raw,
        debt_raw,
        p_coll: U256::ZERO,
        p_debt: U256::ZERO,
        oracle_1e27: U256::ZERO,
        raw_debt_per_col: U256::ZERO,
        liq_tick: 0,
        top_tick: extra.top_tick,
    })
}

fn value_wad(amount: U256, price_ray: U256, decimals: u8) -> Result<U256> {
    let unit = crate::math::asset_unit(decimals)?;
    crate::math::mul_div_down(
        amount,
        price_ray,
        unit.checked_mul(WAD_RAY_RATIO)
            .ok_or(FixedError::Overflow)?,
    )
}

pub(crate) fn finish<'a>(t: &Terms<'a>, px: &PriceVector) -> Result<(Terms<'a>, Health)> {
    finish_at(t, px, None)
}

pub(crate) fn finish_at<'a>(
    t: &Terms<'a>,
    px: &PriceVector,
    over: Option<(AssetId, U256)>,
) -> Result<(Terms<'a>, Health)> {
    let p_coll = override_px(px, t.coll_row.asset, over)?;
    let p_debt = override_px(px, t.debt_row.asset, over)?;
    let oracle_1e27 =
        oracle_debt_per_col_1e27(p_coll, p_debt, t.coll_row.decimals, t.debt_row.decimals)?;
    let raw_dpc = raw_debt_per_col(
        oracle_1e27,
        U256::from(t.body.supply_ex_price),
        U256::from(t.body.borrow_ex_price),
    )?;
    let liq_tick = liquidation_tick(raw_dpc, t.body.liq_threshold)?;
    let mut t = *t;
    t.p_coll = p_coll;
    t.p_debt = p_debt;
    t.oracle_1e27 = oracle_1e27;
    t.raw_debt_per_col = raw_dpc;
    t.liq_tick = liq_tick;

    let coll_wad = Wad::from_raw(value_wad(t.col_tokens, p_coll, t.coll_row.decimals)?);
    let debt_wad = Wad::from_raw(value_wad(t.debt_tokens, p_debt, t.debt_row.decimals)?);
    let absorbed_col = from_raw(
        U256::from(t.extra.absorbed_col_raw),
        U256::from(t.body.supply_ex_price),
    )?;
    let absorbed_debt = from_raw(
        U256::from(t.extra.absorbed_debt_raw),
        U256::from(t.body.borrow_ex_price),
    )?;
    let seizable = t
        .col_tokens
        .checked_add(absorbed_col)
        .ok_or(FixedError::Overflow)?;
    let debt_all = t
        .debt_tokens
        .checked_add(absorbed_debt)
        .ok_or(FixedError::Overflow)?;

    let mut sensitivity = AssetMask::EMPTY;
    if t.col_tokens > U256::ZERO || t.debt_tokens > U256::ZERO {
        sensitivity = sensitivity
            .with(t.coll_slot)
            .and_then(|s| s.with(t.debt_slot))
            .ok_or(ProtocolError::Internal)?;
    }

    if debt_all.is_zero() {
        return Ok((
            t,
            Health {
                hf: Health::NO_DEBT_HF,
                debt_value: Wad::from_raw(U256::ZERO),
                collateral_value: coll_wad,
                price_sensitivity: sensitivity,
                state: HealthState::Healthy,
            },
        ));
    }
    if seizable.is_zero() {
        return Ok((
            t,
            Health {
                hf: Ray::from_raw(U256::ZERO),
                debt_value: debt_wad,
                collateral_value: Wad::from_raw(U256::ZERO),
                price_sensitivity: sensitivity,
                state: HealthState::BadDebt { deficit: debt_wad },
            },
        ));
    }

    let has_absorb = t.extra.absorbed_debt_raw > 0 && t.extra.absorbed_col_raw > 0;
    let tick_liq = if t.extra.flags & VaultExtra::TOP_KNOWN == 0 {
        if has_absorb {
            false
        } else {
            return Err(ProtocolError::OracleSourceMismatch);
        }
    } else {
        if t.extra.tick_status != TICK_STATUS_PERFECT && t.extra.tick_status != 2 {
            return Err(ProtocolError::Internal);
        }
        t.top_tick > liq_tick
    };

    let hf = if t.extra.flags & VaultExtra::TOP_KNOWN != 0 {
        hf_from_ticks(t.top_tick, liq_tick)?
    } else {
        Ray::from_raw(U256::ZERO)
    };

    let state = if tick_liq || has_absorb {
        if t.coll_row.flags.contains(MarketFlags::PAUSED)
            || t.debt_row.flags.contains(MarketFlags::PAUSED)
        {
            HealthState::Blocked {
                reason: BlockReason::Paused,
            }
        } else {
            HealthState::Liquidatable
        }
    } else {
        HealthState::Healthy
    };

    Ok((
        t,
        Health {
            hf,
            debt_value: debt_wad,
            collateral_value: coll_wad,
            price_sensitivity: sensitivity,
            state,
        },
    ))
}

fn override_px(px: &PriceVector, asset: AssetId, over: Option<(AssetId, U256)>) -> Result<U256> {
    if let Some((a, p)) = over {
        if a == asset {
            if p.is_zero() {
                return Err(ProtocolError::MissingPrice(asset));
            }
            return Ok(p);
        }
    }
    price_ray(px, asset)
}

pub(crate) fn health(pos: PositionRef<'_>, px: &PriceVector) -> Result<Health> {
    let t = terms(pos)?;
    Ok(finish(&t, px)?.1)
}

pub(crate) fn hf_at_prices(t: &Terms<'_>, p_coll: U256, p_debt: U256) -> Result<Ray> {
    let oracle_1e27 =
        oracle_debt_per_col_1e27(p_coll, p_debt, t.coll_row.decimals, t.debt_row.decimals)?;
    let raw_dpc = raw_debt_per_col(
        oracle_1e27,
        U256::from(t.body.supply_ex_price),
        U256::from(t.body.borrow_ex_price),
    )?;
    let liq_tick = liquidation_tick(raw_dpc, t.body.liq_threshold)?;
    if t.extra.flags & VaultExtra::TOP_KNOWN == 0 {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    hf_from_ticks(t.top_tick, liq_tick)
}
