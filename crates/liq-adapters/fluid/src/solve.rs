//! Last healthy price (`top_tick <= liquidation_tick`) and accrual crossing.
//! No rate in events — `time_to_cross` does not invent one.

use alloy_primitives::U256;
use liq_protocol::{HealthState, PositionRef, ProtocolError, Result, Timestamp};
use liq_types::fixed::{FixedError, WAD, WAD_RAY_RATIO};
use liq_types::price::SourceKind;
use liq_types::{AssetId, Price, PriceVector, Ray};

use crate::health::{finish, hf_at_prices, terms};
use crate::math::{asset_unit, mul_div_down};

pub const HORIZON: u64 = 10 * 365 * 24 * 3600;

pub(crate) fn time_to_cross(pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Timestamp>> {
    let t = terms(pos)?;
    let h = finish(&t, px)?.1;
    if t.debt_tokens.is_zero() {
        return Ok(None);
    }
    if h.state == HealthState::Liquidatable || h.hf < Ray::ONE {
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
    let t0 = terms(pos)?;
    let (t, _) = finish(&t0, px)?;
    let is_coll = t.coll_row.asset == asset && t.coll_slot != t.debt_slot;
    let is_debt = t.debt_row.asset == asset && t.coll_slot != t.debt_slot;
    if !is_coll && !is_debt {
        return Ok(None);
    }
    if t.debt_tokens.is_zero() || t.col_tokens.is_zero() {
        return Ok(None);
    }
    let entry =
        px.0.get(usize::from(asset.0))
            .filter(|p| p.asset == asset)
            .ok_or(ProtocolError::MissingPrice(asset))?;

    let hf_p = |p: U256| -> Result<Ray> {
        if p.is_zero() {
            return Ok(Ray::from_raw(U256::ZERO));
        }
        let r = if is_coll {
            hf_at_prices(&t, p, t.p_debt)
        } else {
            hf_at_prices(&t, t.p_coll, p)
        };
        match r {
            Ok(h) => Ok(h),
            Err(ProtocolError::Fixed(_) | ProtocolError::Internal) => {
                // Pin reverts out-of-range oracle/tick. Saturate: extremes on
                // the healthy side of the current price stay healthy.
                let healthy = if is_coll {
                    p >= t.p_coll
                } else {
                    p <= t.p_debt
                };
                Ok(if healthy {
                    Ray::from_raw(U256::MAX)
                } else {
                    Ray::from_raw(U256::ZERO)
                })
            }
            Err(e) => Err(e),
        }
    };

    let hi = if is_coll {
        max_price(t.col_tokens, t.coll_row.decimals)?
    } else {
        max_price(t.debt_tokens, t.debt_row.decimals)?
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
