//! Health from `SiloSolvencyLib.isSolvent` / `maxLiquidation` views
//! @ `570a668a`. Fail closed if `getConfig` params were never written.

use alloy_primitives::U256;
use liq_protocol::{
    AssetMask, Health, HealthState, MarketFlags, MarketRow, MarketSlot, PositionRef, ProtocolError,
    Result,
};
use liq_types::{AssetId, PriceVector, Wad};

use crate::layout::{SiloRow, UserExtra, SLOT0, SLOT1};
use crate::math::{convert_to_assets, hf_ray, is_solvent, value_from_price, value_wad};

#[derive(Copy, Clone, Debug)]
pub(crate) struct Terms<'a> {
    pub debt_slot: u16,
    pub coll_slot: u16,
    pub debt_row: &'a MarketRow,
    pub coll_row: &'a MarketRow,
    #[allow(dead_code)]
    pub debt_body: &'a SiloRow,
    #[allow(dead_code)]
    pub coll_body: &'a SiloRow,
    pub debt_shares: U256,
    #[allow(dead_code)]
    pub coll_shares: U256,
    #[allow(dead_code)]
    pub prot_shares: U256,
    pub debt_assets: U256,
    #[allow(dead_code)]
    pub coll_assets: U256,
    #[allow(dead_code)]
    pub prot_assets: U256,
    pub sum_coll_assets: U256,
    pub lt: U256,
    pub fee: U256,
    pub target_ltv: U256,
    pub p_debt: U256,
    pub p_coll: U256,
    pub debt_value: U256,
    pub coll_value: U256,
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

fn viewed(row: &MarketRow) -> Result<&SiloRow> {
    let b: &SiloRow = row.body()?;
    if b.flags & SiloRow::VIEWED == 0 {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    if row.flags.contains(MarketFlags::UNPRICED) || b.flags & SiloRow::PRICED == 0 {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    Ok(b)
}

pub(crate) fn terms(pos: PositionRef<'_>) -> Result<Terms<'_>> {
    let extra: &UserExtra = pos.extra.view()?;
    let d0 = cell(pos.debt, SLOT0);
    let d1 = cell(pos.debt, SLOT1);
    let debt_slot = match (d0 > 0, d1 > 0) {
        (false, false) => {
            return empty_terms(pos);
        }
        (true, false) => SLOT0,
        (false, true) => SLOT1,
        (true, true) => return Err(ProtocolError::Internal),
    };
    let coll_slot = if extra.collateral_slot == UserExtra::UNSET {
        if debt_slot == SLOT0 {
            SLOT1
        } else {
            SLOT0
        }
    } else {
        u16::from(extra.collateral_slot)
    };
    if coll_slot != SLOT0 && coll_slot != SLOT1 {
        return Err(ProtocolError::Internal);
    }
    fill_terms(pos, extra, debt_slot, coll_slot)
}

fn empty_terms(pos: PositionRef<'_>) -> Result<Terms<'_>> {
    let extra: &UserExtra = pos.extra.view()?;
    let coll_slot = if extra.collateral_slot == UserExtra::UNSET {
        SLOT0
    } else {
        u16::from(extra.collateral_slot)
    };
    let debt_slot = if coll_slot == SLOT0 { SLOT1 } else { SLOT0 };
    fill_terms(pos, extra, debt_slot, coll_slot)
}

fn fill_terms<'a>(
    pos: PositionRef<'a>,
    extra: &UserExtra,
    debt_slot: u16,
    coll_slot: u16,
) -> Result<Terms<'a>> {
    let debt_row = row_at(pos, debt_slot)?;
    let coll_row = row_at(pos, coll_slot)?;
    let debt_body = viewed(debt_row)?;
    let coll_body = viewed(coll_row)?;
    let debt_shares = U256::from(cell(pos.debt, debt_slot));
    let coll_shares = U256::from(cell(pos.supply, coll_slot));
    let prot_shares = U256::from(extra.protected(coll_slot));
    let debt_assets = if debt_shares.is_zero() {
        U256::ZERO
    } else {
        convert_to_assets(
            debt_shares,
            U256::from(debt_body.total_debt_assets),
            U256::from(debt_body.total_debt_shares),
            true,
            true,
        )?
    };
    let coll_assets = convert_to_assets(
        coll_shares,
        U256::from(coll_body.total_collateral_assets),
        U256::from(coll_body.total_collateral_shares),
        false,
        false,
    )?;
    let prot_assets = convert_to_assets(
        prot_shares,
        U256::from(coll_body.total_protected_assets),
        U256::from(coll_body.total_protected_shares),
        false,
        false,
    )?;
    let sum_coll_assets = coll_assets
        .checked_add(prot_assets)
        .ok_or(liq_types::fixed::FixedError::Overflow)?;
    Ok(Terms {
        debt_slot,
        coll_slot,
        debt_row,
        coll_row,
        debt_body,
        coll_body,
        debt_shares,
        coll_shares,
        prot_shares,
        debt_assets,
        coll_assets,
        prot_assets,
        sum_coll_assets,
        lt: U256::from(coll_body.lt),
        fee: U256::from(coll_body.liquidation_fee),
        target_ltv: U256::from(coll_body.liquidation_target_ltv),
        p_debt: U256::ZERO,
        p_coll: U256::ZERO,
        debt_value: U256::ZERO,
        coll_value: U256::ZERO,
    })
}

pub(crate) fn finish<'a>(t: &Terms<'a>, px: &PriceVector) -> Result<(Terms<'a>, Health)> {
    let p_debt = price_ray(px, t.debt_row.asset)?;
    let p_coll = price_ray(px, t.coll_row.asset)?;
    let debt_value = if t.debt_assets.is_zero() {
        U256::ZERO
    } else {
        value_from_price(t.debt_assets, p_debt, t.debt_row.decimals)?
    };
    let coll_value = if t.sum_coll_assets.is_zero() {
        U256::ZERO
    } else {
        value_from_price(t.sum_coll_assets, p_coll, t.coll_row.decimals)?
    };
    let mut t = *t;
    t.p_debt = p_debt;
    t.p_coll = p_coll;
    t.debt_value = debt_value;
    t.coll_value = coll_value;

    let hf = hf_ray(debt_value, coll_value, t.lt)?;
    let solvent = is_solvent(debt_value, coll_value, t.lt)?;
    let mut sensitivity = AssetMask::EMPTY;
    if t.sum_coll_assets > U256::ZERO || t.debt_shares > U256::ZERO {
        sensitivity = sensitivity
            .with(t.debt_slot)
            .and_then(|s| s.with(t.coll_slot))
            .ok_or(ProtocolError::Internal)?;
    }
    let debt_wad = Wad::from_raw(value_wad(t.debt_assets, p_debt, t.debt_row.decimals)?);
    let coll_wad = Wad::from_raw(value_wad(t.sum_coll_assets, p_coll, t.coll_row.decimals)?);
    // Pin `_BAD_DEBT = 1e18` only changes cover size in `liquidationPreview`
    // (any cover). Hook `NoCollateralToLiquidate` is zero collateral with
    // debt. Remaining coll at LTV ≥ 1e18 is still `maxLiquidation`.
    let state = if t.debt_shares.is_zero() || solvent {
        HealthState::Healthy
    } else if t.sum_coll_assets.is_zero() {
        HealthState::BadDebt { deficit: debt_wad }
    } else if t.coll_row.flags.contains(MarketFlags::PAUSED)
        || t.debt_row.flags.contains(MarketFlags::PAUSED)
    {
        HealthState::Blocked {
            reason: liq_protocol::BlockReason::Paused,
        }
    } else {
        HealthState::Liquidatable
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

pub(crate) fn health(pos: PositionRef<'_>, px: &PriceVector) -> Result<Health> {
    let t = terms(pos)?;
    Ok(finish(&t, px)?.1)
}

pub(crate) fn hf_at_prices(t: &Terms<'_>, p_coll: U256, p_debt: U256) -> Result<liq_types::Ray> {
    let debt_value = if t.debt_assets.is_zero() {
        U256::ZERO
    } else {
        value_from_price(t.debt_assets, p_debt, t.debt_row.decimals)?
    };
    let coll_value = if t.sum_coll_assets.is_zero() {
        U256::ZERO
    } else {
        value_from_price(t.sum_coll_assets, p_coll, t.coll_row.decimals)?
    };
    hf_ray(debt_value, coll_value, t.lt)
}
