//! Last healthy price (rational in the 1e36 oracle) and IRM-rate crossing.

use alloy_primitives::U256;
use liq_protocol::{PositionRef, ProtocolError, Result, Timestamp};
use liq_types::fixed::{mul_div, FixedError, Rounding};
use liq_types::price::SourceKind;
use liq_types::{AssetId, Price, PriceVector, Ray};

use crate::health::{finish, terms};
use crate::math::{asset_unit, mul_div_down, HF_THRESHOLD_WAD, ORACLE_PRICE_SCALE};
use liq_types::fixed::WAD;

pub const HORIZON: u64 = 10 * 365 * 24 * 3600;

fn hf_at_ts(pos: PositionRef<'_>, px: &PriceVector, ts: Timestamp) -> Result<U256> {
    let mut at = pos;
    at.timestamp = ts;
    let t = terms(at)?;
    let h = finish(&t, px)?.1;
    if h.hf.raw() == U256::MAX {
        return Ok(U256::MAX);
    }
    mul_div_down(h.hf.raw(), U256::ONE, liq_types::fixed::WAD_RAY_RATIO)
}

pub(crate) fn time_to_cross(pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Timestamp>> {
    let now = pos.timestamp;
    let start = hf_at_ts(pos, px, now)?;
    if start == U256::MAX {
        return Ok(None);
    }
    if start < HF_THRESHOLD_WAD {
        return Ok(Some(now));
    }
    let mut hi = now.checked_add(HORIZON).ok_or(FixedError::Overflow)?;
    if hf_at_ts(pos, px, hi)? >= HF_THRESHOLD_WAD {
        return Ok(None);
    }
    let mut lo = now;
    while hi.wrapping_sub(lo) > 1 {
        let mid = lo.wrapping_add(hi.wrapping_sub(lo) / 2);
        if hf_at_ts(pos, px, mid)? >= HF_THRESHOLD_WAD {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(Some(hi))
}

pub(crate) fn liquidation_price(
    pos: PositionRef<'_>,
    px: &PriceVector,
    asset: AssetId,
) -> Result<Option<Price>> {
    let t0 = terms(pos)?;
    let (t, _) = finish(&t0, px)?;
    let is_coll = t.coll_row.asset == asset;
    let is_loan = t.loan_row.asset == asset;
    if !is_coll && !is_loan {
        return Ok(None);
    }
    if t.borrowed.is_zero() || t.collateral.is_zero() {
        return Ok(None);
    }
    let entry =
        px.0.get(usize::from(asset.0))
            .filter(|p| p.asset == asset)
            .ok_or(ProtocolError::MissingPrice(asset))?;

    let need = mul_div(t.borrowed, WAD, U256::from(t.loan.lltv), Rounding::Up)?;
    let oracle_star = mul_div(need, ORACLE_PRICE_SCALE, t.collateral, Rounding::Up)?;
    if oracle_star.is_zero() {
        return Ok(None);
    }

    // Invert `health::oracle_price`. That reconstruction inserts
    // `10^(loan_decimals - coll_decimals)` into the 1e36 oracle. Dropping
    // it prices the threshold off by that factor, so a 6-decimal loan
    // never crosses.
    let coll_decimals = t.loan.coll_decimals;
    let loan_decimals = t.loan_row.decimals;
    let raw = match loan_decimals.cmp(&coll_decimals) {
        core::cmp::Ordering::Equal => {
            if is_coll {
                mul_div(oracle_star, t.p_loan, ORACLE_PRICE_SCALE, Rounding::Up)?
            } else {
                mul_div(t.p_coll, ORACLE_PRICE_SCALE, oracle_star, Rounding::Down)?
            }
        }
        core::cmp::Ordering::Greater => {
            let lift = asset_unit(loan_decimals.wrapping_sub(coll_decimals))?;
            let scale = ORACLE_PRICE_SCALE
                .checked_mul(lift)
                .ok_or(FixedError::Overflow)?;
            if is_coll {
                mul_div(oracle_star, t.p_loan, scale, Rounding::Up)?
            } else {
                mul_div(t.p_coll, scale, oracle_star, Rounding::Down)?
            }
        }
        core::cmp::Ordering::Less => {
            let lift = asset_unit(coll_decimals.wrapping_sub(loan_decimals))?;
            if is_coll {
                let num = t.p_loan.checked_mul(lift).ok_or(FixedError::Overflow)?;
                mul_div(oracle_star, num, ORACLE_PRICE_SCALE, Rounding::Up)?
            } else {
                let den = oracle_star.checked_mul(lift).ok_or(FixedError::Overflow)?;
                mul_div(t.p_coll, ORACLE_PRICE_SCALE, den, Rounding::Down)?
            }
        }
    };
    if is_coll && raw < U256::from(2u8) {
        return Ok(None);
    }
    if !is_coll && raw.is_zero() {
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
