//! Last healthy price (`twvUSD >= totalDebtUSD`) and accrual crossing.
//! No pool IRM in events — `time_to_cross` does not invent a rate.

use alloy_primitives::U256;
use liq_protocol::{PositionRef, ProtocolError, Result, Timestamp};
use liq_types::fixed::{FixedError, WAD, WAD_RAY_RATIO};
use liq_types::price::SourceKind;
use liq_types::{AssetId, Price, PriceVector, Ray};

use crate::health::{hf_at_price, terms};
use crate::math::{asset_unit, mul_div_down};

pub const HORIZON: u64 = 10 * 365 * 24 * 3600;

pub(crate) fn time_to_cross(pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Timestamp>> {
    let t = terms(pos, px, None)?;
    let h = crate::health::finish(&t, pos)?;
    if t.debt.is_zero() {
        return Ok(None);
    }
    if h.hf < Ray::ONE || t.expired {
        return Ok(Some(pos.timestamp));
    }
    let _ = HORIZON;
    Ok(None)
}

fn mid(lo: U256, hi: U256) -> U256 {
    let span = hi.saturating_sub(lo);
    let half = match span.checked_div(U256::from(2u8)) {
        Some(h) => h,
        None => U256::ZERO,
    };
    lo.saturating_add(half)
}

fn max_price(amount: U256, decimals: u8) -> Result<U256> {
    if amount.is_zero() {
        return Ok(U256::MAX);
    }
    let unit = asset_unit(decimals)?;
    let cap = U256::MAX
        .checked_div(WAD)
        .and_then(|v| v.checked_div(WAD_RAY_RATIO))
        .ok_or(ProtocolError::Fixed(FixedError::DivisionByZero))?;
    mul_div_down(cap, unit, amount)
}

fn smallest_healthy(
    hf_p: impl Fn(U256) -> Result<Ray>,
    lo: U256,
    hi: U256,
) -> Result<Option<U256>> {
    if hi < lo {
        return Ok(None);
    }
    if hf_p(hi)? < Ray::ONE {
        return Ok(None);
    }
    let mut lo = lo;
    let mut hi = hi;
    let mut ans = hi;
    while hi.saturating_sub(lo) > U256::ONE {
        let m = mid(lo, hi);
        if hf_p(m)? >= Ray::ONE {
            ans = m;
            hi = m;
        } else {
            lo = m;
        }
    }
    Ok(Some(ans))
}

fn largest_healthy(hf_p: impl Fn(U256) -> Result<Ray>, lo: U256, hi: U256) -> Result<Option<U256>> {
    if hi < lo {
        return Ok(None);
    }
    if hf_p(lo)? < Ray::ONE {
        return Ok(None);
    }
    let mut lo = lo;
    let mut hi = hi;
    let mut ans = lo;
    while hi.saturating_sub(lo) > U256::ONE {
        let m = mid(lo, hi);
        if hf_p(m)? >= Ray::ONE {
            ans = m;
            lo = m;
        } else {
            hi = m;
        }
    }
    Ok(Some(ans))
}

pub(crate) fn liquidation_price(
    pos: PositionRef<'_>,
    px: &PriceVector,
    asset: AssetId,
) -> Result<Option<Price>> {
    let t = terms(pos, px, None)?;
    if t.debt.is_zero() {
        return Ok(None);
    }
    let mut is_coll = false;
    let mut is_debt = t.debt_row.asset == asset && !t.debt.is_zero();
    let mut coll_amount = U256::ZERO;
    let mut coll_decimals = 0u8;
    for slot in 0u16..u16::from(t.manager.token_count.max(1)) {
        let row = match pos.markets.get(usize::from(slot)) {
            Some(r) => r,
            None => continue,
        };
        if row.asset != asset {
            continue;
        }
        let bal = pos
            .supply
            .get(usize::from(slot))
            .copied()
            .map(U256::from)
            .unwrap_or(U256::ZERO);
        if slot == 0 && t.debt_row.asset == asset {
            is_debt = true;
        }
        if !bal.is_zero() {
            is_coll = true;
            coll_amount = bal;
            coll_decimals = row.decimals;
        }
    }
    if !is_coll && !is_debt {
        return Ok(None);
    }
    // Dual-use (underlying as coll+debt): no unique crossing.
    if is_coll && is_debt {
        return Ok(None);
    }
    let entry =
        px.0.get(usize::from(asset.0))
            .filter(|p| p.asset == asset)
            .ok_or(ProtocolError::MissingPrice(asset))?;

    let hf_p = |p: U256| hf_at_price(pos, px, asset, p);

    let hi = if is_coll {
        max_price(coll_amount, coll_decimals)?
    } else {
        max_price(t.total_debt, t.debt_row.decimals)?
    };
    let raw = if is_coll {
        smallest_healthy(hf_p, U256::ZERO, hi)?
    } else {
        largest_healthy(hf_p, U256::ONE, hi)?
    };
    let Some(raw) = raw.filter(|p| !p.is_zero()) else {
        return Ok(None);
    };
    if hf_p(raw)? < Ray::ONE {
        return Ok(None);
    }
    let danger = if is_coll {
        raw.checked_sub(U256::ONE)
    } else {
        raw.checked_add(U256::ONE)
    };
    if let Some(n) = danger {
        if n <= hi && hf_p(n)? >= Ray::ONE {
            return Ok(None);
        }
    }
    Ok(Some(Price {
        asset,
        price: Ray::from_raw(raw),
        source: SourceKind::Derived {
            deps: smallvec::smallvec![asset],
        },
        block: entry.block,
        ts: entry.ts,
    }))
}
