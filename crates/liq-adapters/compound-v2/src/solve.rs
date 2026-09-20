//! Last-healthy price (bisection on the oracle integer). Accrual crossing
//! is `None`: events do not carry the interest-rate model.

use alloy_primitives::U256;
use liq_protocol::{HealthState, PositionRef, Result, Timestamp};
use liq_types::fixed::FixedError;
use liq_types::price::SourceKind;
use liq_types::{AssetId, Price, PriceVector, Ray};

use crate::health::{finish, price_ray};
use crate::layout::META_SLOT;

fn liquidatable_at(
    pos: PositionRef<'_>,
    px: &PriceVector,
    asset: AssetId,
    p_raw: U256,
) -> Result<bool> {
    Ok(finish(pos, px, Some((asset, p_raw)))?.1.state == HealthState::Liquidatable)
}

pub(crate) fn time_to_cross(_pos: PositionRef<'_>, _px: &PriceVector) -> Result<Option<Timestamp>> {
    Ok(None)
}

fn holds(pos: PositionRef<'_>, asset: AssetId) -> (bool, bool) {
    let mut coll = false;
    let mut debt = false;
    for slot in pos.config.iter() {
        if slot == META_SLOT {
            continue;
        }
        let Some(row) = pos.markets.get(usize::from(slot)) else {
            continue;
        };
        if row.asset != asset {
            continue;
        }
        if pos.supply.get(usize::from(slot)).copied().unwrap_or(0) != 0 {
            coll = true;
        }
        if pos.debt.get(usize::from(slot)).copied().unwrap_or(0) != 0 {
            debt = true;
        }
    }
    (coll, debt)
}

/// Last healthy price: `health` at it is not liquidatable; the adjacent
/// unit toward danger is. Collateral danger is down; debt danger is up.
pub(crate) fn liquidation_price(
    pos: PositionRef<'_>,
    px: &PriceVector,
    asset: AssetId,
) -> Result<Option<Price>> {
    let (t, h) = finish(pos, px, None)?;
    if t.sum_borrow.is_zero() {
        return Ok(None);
    }
    let (is_coll, is_debt) = holds(pos, asset);
    if !is_coll && !is_debt {
        return Ok(None);
    }
    if is_coll && is_debt {
        return Ok(None);
    }
    if t.deprecated_borrow && h.hf >= Ray::ONE {
        return Ok(None);
    }
    let cur = price_ray(px, asset, None)?;
    let tick = px.0.get(usize::from(asset.0));
    let block = tick.map(|p| p.block).unwrap_or(0);
    let ts = tick.map(|p| p.ts).unwrap_or(0);

    let danger_down = is_coll;
    let mut lo = U256::ONE;
    let mut hi = cur
        .checked_mul(U256::from(8u8))
        .ok_or(FixedError::Overflow)?;
    if danger_down {
        if liquidatable_at(pos, px, asset, cur)? {
            hi = cur;
        }
        if !liquidatable_at(pos, px, asset, lo)? {
            return Ok(None);
        }
        // last healthy is the largest p in [lo, hi] that is not liquidatable,
        // with p-1 liquidatable.
        while hi.saturating_sub(lo) > U256::ONE {
            let mid = lo
                .checked_add(hi)
                .ok_or(FixedError::Overflow)?
                .checked_div(U256::from(2u8))
                .ok_or(FixedError::DivisionByZero)?;
            if liquidatable_at(pos, px, asset, mid)? {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let cand = if !liquidatable_at(pos, px, asset, hi)? {
            hi
        } else {
            return Ok(None);
        };
        return Ok(Some(Price {
            asset,
            price: Ray::from_raw(cand),
            source: SourceKind::Canonical,
            block,
            ts,
        }));
    }
    // Debt: danger is a higher price.
    lo = cur;
    if liquidatable_at(pos, px, asset, cur)? {
        return Ok(None);
    }
    if !liquidatable_at(pos, px, asset, hi)? {
        return Ok(None);
    }
    while hi.saturating_sub(lo) > U256::ONE {
        let mid = lo
            .checked_add(hi)
            .ok_or(FixedError::Overflow)?
            .checked_div(U256::from(2u8))
            .ok_or(FixedError::DivisionByZero)?;
        if liquidatable_at(pos, px, asset, mid)? {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let cand = if !liquidatable_at(pos, px, asset, lo)? {
        lo
    } else {
        return Ok(None);
    };
    Ok(Some(Price {
        asset,
        price: Ray::from_raw(cand),
        source: SourceKind::Canonical,
        block,
        ts,
    }))
}
