//! Last healthy price (HF ≥ 1.0) and interest-accumulator crossing.

use alloy_primitives::U256;
use liq_protocol::{PositionRef, ProtocolError, Result, Timestamp};
use liq_types::fixed::FixedError;
use liq_types::price::SourceKind;
use liq_types::{AssetId, Price, PriceVector, Ray};

use crate::health::{finish, terms, PriceOverride};
use crate::layout::DEBT_SLOT;
use crate::math::HF_THRESHOLD_WAD;

pub const HORIZON: u64 = 10 * 365 * 24 * 3600;

fn hf_wad(pos: PositionRef<'_>, px: &PriceVector, over: PriceOverride) -> Result<U256> {
    let t = terms(pos)?;
    let h = finish(&t, pos, px, over)?.1;
    if h.hf.raw() == U256::MAX {
        return Ok(U256::MAX);
    }
    crate::math::mul_div_down(h.hf.raw(), U256::ONE, liq_types::fixed::WAD_RAY_RATIO)
}

fn hf_at_ts(pos: PositionRef<'_>, px: &PriceVector, ts: Timestamp) -> Result<U256> {
    let mut at = pos;
    at.timestamp = ts;
    hf_wad(at, px, None)
}

fn hf_ge_one(h: U256) -> bool {
    h == U256::MAX || h >= HF_THRESHOLD_WAD
}

fn hf_at_price(
    pos: PositionRef<'_>,
    px: &PriceVector,
    asset: AssetId,
    p: U256,
    overflow_healthy: bool,
) -> Result<U256> {
    match hf_wad(pos, px, Some((asset, p))) {
        Ok(h) => Ok(h),
        Err(ProtocolError::Fixed(FixedError::Overflow | FixedError::Underflow)) => {
            Ok(if overflow_healthy {
                U256::MAX
            } else {
                U256::ZERO
            })
        }
        Err(e) => Err(e),
    }
}

pub(crate) fn time_to_cross(pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Timestamp>> {
    let now = pos.timestamp;
    let start = hf_at_ts(pos, px, now)?;
    if start == U256::MAX {
        return Ok(None);
    }
    if start <= HF_THRESHOLD_WAD {
        return Ok(Some(now));
    }
    let mut hi = now.checked_add(HORIZON).ok_or(FixedError::Overflow)?;
    if hf_at_ts(pos, px, hi)? > HF_THRESHOLD_WAD {
        return Ok(None);
    }
    let mut lo = now;
    while hi.wrapping_sub(lo) > 1 {
        let mid = lo.wrapping_add(hi.wrapping_sub(lo).wrapping_shr(1));
        if hf_at_ts(pos, px, mid)? > HF_THRESHOLD_WAD {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(Some(hi))
}

fn holds(pos: PositionRef<'_>, asset: AssetId) -> Option<(bool, bool)> {
    let mut is_coll = false;
    let mut is_debt = false;
    for slot in pos.config.iter() {
        let Some(row) = pos.markets.get(usize::from(slot)) else {
            continue;
        };
        if row.asset != asset {
            continue;
        }
        if slot == DEBT_SLOT {
            is_debt = true;
        } else {
            is_coll = true;
        }
    }
    if is_coll || is_debt {
        Some((is_coll, is_debt))
    } else {
        None
    }
}

fn boundary(
    pos: PositionRef<'_>,
    px: &PriceVector,
    asset: AssetId,
    danger_up: bool,
) -> Result<Option<U256>> {
    let lo = U256::ONE;
    let hi = U256::MAX;
    let overflow_healthy = !danger_up;
    let hf_lo = hf_at_price(pos, px, asset, lo, overflow_healthy)?;
    let hf_hi = hf_at_price(pos, px, asset, hi, overflow_healthy)?;
    if danger_up {
        if hf_ge_one(hf_hi) {
            return Ok(None);
        }
        if !hf_ge_one(hf_lo) {
            return Ok(None);
        }
        let mut l = lo;
        let mut h = hi;
        while h.wrapping_sub(l) > U256::ONE {
            let mid = l.wrapping_add(h.wrapping_sub(l).wrapping_shr(1));
            if hf_ge_one(hf_at_price(pos, px, asset, mid, overflow_healthy)?) {
                l = mid;
            } else {
                h = mid;
            }
        }
        Ok(Some(l))
    } else {
        if hf_ge_one(hf_lo) {
            return Ok(None);
        }
        if !hf_ge_one(hf_hi) {
            return Ok(None);
        }
        let mut l = lo;
        let mut h = hi;
        while h.wrapping_sub(l) > U256::ONE {
            let mid = l.wrapping_add(h.wrapping_sub(l).wrapping_shr(1));
            if hf_ge_one(hf_at_price(pos, px, asset, mid, overflow_healthy)?) {
                h = mid;
            } else {
                l = mid;
            }
        }
        Ok(Some(h))
    }
}

pub(crate) fn liquidation_price(
    pos: PositionRef<'_>,
    px: &PriceVector,
    asset: AssetId,
) -> Result<Option<Price>> {
    let Some((is_coll, is_debt)) = holds(pos, asset) else {
        return Ok(None);
    };
    if is_coll == is_debt {
        return Ok(None);
    }
    let t0 = terms(pos)?;
    let (t, _) = finish(&t0, pos, px, None)?;
    if t.liability_assets.is_zero() {
        return Ok(None);
    }
    let entry =
        px.0.get(usize::from(asset.0))
            .filter(|p| p.asset == asset)
            .ok_or(ProtocolError::MissingPrice(asset))?;
    let Some(raw) = boundary(pos, px, asset, is_debt)? else {
        return Ok(None);
    };
    if raw.is_zero() {
        return Ok(None);
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
