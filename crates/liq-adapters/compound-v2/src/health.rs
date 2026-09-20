//! `getAccountLiquidityInternal` @ `a3214f67` — liquidatable iff shortfall > 0
//! (or the deprecated-market path). Not Aave HF.

use alloy_primitives::U256;
use liq_protocol::{
    AssetMask, BlockReason, Health, HealthState, MarketFlags, MarketRow, MarketSlot, PositionRef,
    ProtocolError, Result,
};
use liq_types::fixed::{FixedError, WAD};
use liq_types::{AssetId, PriceVector, Wad};

use crate::layout::{BorrowSnap, CTokenRow, ComptrollerMeta, UserExtra, META_SLOT, UNMAPPED_ASSET};
use crate::math::{
    bonus_ray, borrow_balance_stored, ctokens_to_underlying, hf_wad_to_ray, is_deprecated, mul_exp,
    mul_scalar_truncate_add, value_wad,
};

#[derive(Copy, Clone, Debug)]
pub(crate) struct Terms<'a> {
    pub meta: &'a ComptrollerMeta,
    pub sum_borrow: U256,
    pub deprecated_borrow: bool,
}

pub(crate) type PriceOverride = Option<(AssetId, U256)>;

#[inline]
fn cell(v: &[u128], slot: u16) -> u128 {
    v.get(usize::from(slot)).copied().unwrap_or(0)
}

#[inline]
pub(crate) fn price_ray(px: &PriceVector, asset: AssetId, over: PriceOverride) -> Result<U256> {
    if let Some((a, raw)) = over {
        if a == asset {
            return if raw.is_zero() {
                Err(ProtocolError::MissingPrice(asset))
            } else {
                Ok(raw)
            };
        }
    }
    px.0.get(usize::from(asset.0))
        .filter(|p| p.asset == asset)
        .map(|p| p.price.raw())
        .filter(|r| !r.is_zero())
        .ok_or(ProtocolError::MissingPrice(asset))
}

fn entered(user: &UserExtra, slot: u16) -> Result<bool> {
    let bit = 1u128
        .checked_shl(u32::from(slot))
        .ok_or(FixedError::Overflow)?;
    Ok(user.entered_mask & bit != 0)
}

pub(crate) fn terms(pos: PositionRef<'_>) -> Result<(&ComptrollerMeta, &MarketRow)> {
    let row = pos
        .markets
        .get(usize::from(META_SLOT))
        .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
            market: pos.key.market,
            slot: META_SLOT,
        }))?;
    let meta: &ComptrollerMeta = row.body()?;
    if meta.flags & ComptrollerMeta::PARAMS_KNOWN == 0 {
        return Err(ProtocolError::Internal);
    }
    Ok((meta, row))
}

fn borrow_now(pos: PositionRef<'_>, slot: u16, body: &CTokenRow) -> Result<U256> {
    let principal = U256::from(cell(pos.debt, slot));
    if principal.is_zero() {
        return Ok(U256::ZERO);
    }
    let snap = pos
        .slot_extra
        .get(usize::from(slot))
        .ok_or(ProtocolError::Internal)?
        .view::<BorrowSnap>()?;
    if snap.interest_index == 0 {
        return Err(ProtocolError::Internal);
    }
    borrow_balance_stored(
        principal,
        U256::from(body.borrow_index),
        U256::from(snap.interest_index),
    )
}

pub(crate) fn finish<'a>(
    pos: PositionRef<'a>,
    px: &PriceVector,
    over: PriceOverride,
) -> Result<(Terms<'a>, Health)> {
    let (meta, meta_row) = terms(pos)?;
    let user: &UserExtra = pos.extra.view()?;
    if pos.timestamp < u64::from(meta_row.last_update) {
        return Err(ProtocolError::TimestampBeforeUpdate);
    }
    let mut sum_coll = U256::ZERO;
    let mut sum_borrow = U256::ZERO;
    let mut unweighted = U256::ZERO;
    let mut sensitivity = AssetMask::EMPTY;
    let mut deprecated_borrow = false;

    for slot in pos.config.iter() {
        if slot == META_SLOT {
            continue;
        }
        if !entered(user, slot)? {
            continue;
        }
        let row = pos
            .markets
            .get(usize::from(slot))
            .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
                market: pos.key.market,
                slot,
            }))?;
        let body: &CTokenRow = row.body()?;
        if body.flags & CTokenRow::LISTED == 0 {
            continue;
        }
        let ctokens = U256::from(cell(pos.supply, slot));
        let borrow = borrow_now(pos, slot, body)?;
        if ctokens.is_zero() && borrow.is_zero() {
            continue;
        }
        if row.flags.contains(MarketFlags::UNPRICED)
            || row.asset == UNMAPPED_ASSET
            || body.flags & CTokenRow::PRICED == 0
        {
            return Err(ProtocolError::OracleSourceMismatch);
        }
        if !ctokens.is_zero() && body.flags & CTokenRow::EXRATE_KNOWN == 0 {
            return Err(ProtocolError::Internal);
        }
        let p = price_ray(px, row.asset, over)?;
        if !ctokens.is_zero() {
            let underlying =
                ctokens_to_underlying(ctokens, U256::from(body.exchange_rate_mantissa))?;
            let raw = value_wad(underlying, p, row.decimals)?;
            unweighted = unweighted.checked_add(raw).ok_or(FixedError::Overflow)?;
            let tokens_to_denom = mul_exp(
                mul_exp(
                    U256::from(body.collateral_factor_mantissa),
                    U256::from(body.exchange_rate_mantissa),
                )?,
                crate::math::compound_price_mantissa(p, row.decimals)?,
            )?;
            sum_coll = mul_scalar_truncate_add(tokens_to_denom, ctokens, sum_coll)?;
            sensitivity = sensitivity.with(slot).ok_or(ProtocolError::Internal)?;
        }
        if !borrow.is_zero() {
            let oracle = crate::math::compound_price_mantissa(p, row.decimals)?;
            sum_borrow = mul_scalar_truncate_add(oracle, borrow, sum_borrow)?;
            sensitivity = sensitivity.with(slot).ok_or(ProtocolError::Internal)?;
            if is_deprecated(
                U256::from(body.collateral_factor_mantissa),
                body.flags & CTokenRow::BORROW_PAUSED != 0,
                U256::from(body.reserve_factor_mantissa),
            ) {
                deprecated_borrow = true;
            }
        }
    }

    let hf = if sum_borrow.is_zero() {
        Health::NO_DEBT_HF
    } else {
        hf_wad_to_ray(crate::math::mul_div_down(sum_coll, WAD, sum_borrow)?)?
    };
    let shortfall = sum_borrow > sum_coll;
    let debt_value = Wad::from_raw(sum_borrow);
    let collateral_value = Wad::from_raw(unweighted);
    let seize_paused = meta.flags & ComptrollerMeta::SEIZE_PAUSED != 0;

    let state = if sum_borrow.is_zero() {
        HealthState::Healthy
    } else if unweighted.is_zero() {
        HealthState::BadDebt {
            deficit: debt_value,
        }
    } else if seize_paused {
        HealthState::Blocked {
            reason: BlockReason::Paused,
        }
    } else if shortfall || deprecated_borrow {
        HealthState::Liquidatable
    } else {
        HealthState::Healthy
    };

    let _ = bonus_ray(U256::from(meta.liquidation_incentive_mantissa))?;
    Ok((
        Terms {
            meta,
            sum_borrow,
            deprecated_borrow,
        },
        Health {
            hf,
            debt_value,
            collateral_value,
            price_sensitivity: sensitivity,
            state,
        },
    ))
}

pub(crate) fn health(pos: PositionRef<'_>, px: &PriceVector) -> Result<Health> {
    Ok(finish(pos, px, None)?.1)
}
