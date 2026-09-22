//! Aave V3 (3.5+) arithmetic — `WadRayMath`, `PercentageMath`, `MathUtils`,
//! `TokenMath`. Directions: `docs/coverage/aave-v3-rounding.md`.

use alloy_primitives::{uint, U256};
use liq_protocol::{ProtocolError, Result};
use liq_types::fixed::{mul_div, FixedError, Rounding, RAY, WAD, WAD_RAY_RATIO};
use liq_types::Ray;

use crate::layout::Reserve;

pub const SECONDS_PER_YEAR: U256 = uint!(31_536_000_U256);
pub const BPS: U256 = uint!(10_000_U256);
pub const HALF_BPS: U256 = uint!(5_000_U256);
pub const HALF_RAY: U256 = uint!(500_000_000_000_000_000_000_000_000_U256);
pub const HALF_WAD: U256 = uint!(500_000_000_000_000_000_U256);
pub const BPS_RAY: U256 = uint!(100_000_000_000_000_000_000_000_U256);
pub const HF_THRESHOLD_WAD: U256 = WAD;

#[inline]
pub fn ray_mul(a: U256, b: U256) -> Result<U256> {
    Ok(mul_div(a, b, RAY, Rounding::HalfUp)?)
}

#[inline]
pub fn ray_mul_floor(a: U256, b: U256) -> Result<U256> {
    Ok(mul_div(a, b, RAY, Rounding::Down)?)
}

#[inline]
pub fn ray_mul_ceil(a: U256, b: U256) -> Result<U256> {
    Ok(mul_div(a, b, RAY, Rounding::Up)?)
}

#[inline]
pub fn ray_div_floor(a: U256, b: U256) -> Result<U256> {
    Ok(mul_div(a, RAY, b, Rounding::Down)?)
}

#[inline]
pub fn ray_div_ceil(a: U256, b: U256) -> Result<U256> {
    Ok(mul_div(a, RAY, b, Rounding::Up)?)
}

#[inline]
pub fn wad_div(a: U256, b: U256) -> Result<U256> {
    Ok(mul_div(a, WAD, b, Rounding::HalfUp)?)
}

#[inline]
pub fn percent_mul(v: U256, p: U256) -> Result<U256> {
    Ok(mul_div(v, p, BPS, Rounding::HalfUp)?)
}

#[inline]
pub fn percent_mul_floor(v: U256, p: U256) -> Result<U256> {
    Ok(mul_div(v, p, BPS, Rounding::Down)?)
}

#[inline]
pub fn percent_mul_ceil(v: U256, p: U256) -> Result<U256> {
    Ok(mul_div(v, p, BPS, Rounding::Up)?)
}

#[inline]
pub fn percent_div_floor(v: U256, p: U256) -> Result<U256> {
    Ok(mul_div(v, BPS, p, Rounding::Down)?)
}

#[inline]
pub fn percent_div_ceil(v: U256, p: U256) -> Result<U256> {
    Ok(mul_div(v, BPS, p, Rounding::Up)?)
}

#[inline]
pub fn mul_div_ceil(a: U256, b: U256, c: U256) -> Result<U256> {
    Ok(mul_div(a, b, c, Rounding::Up)?)
}

#[inline]
pub fn a_token_balance(scaled: U256, index: U256) -> Result<U256> {
    ray_mul_floor(scaled, index)
}

#[inline]
pub fn v_token_balance(scaled: U256, index: U256) -> Result<U256> {
    ray_mul_ceil(scaled, index)
}

#[inline]
pub fn a_token_mint_scaled(amount: U256, index: U256) -> Result<U256> {
    ray_div_floor(amount, index)
}

#[inline]
pub fn a_token_burn_scaled(amount: U256, index: U256) -> Result<U256> {
    ray_div_ceil(amount, index)
}

#[inline]
pub fn v_token_mint_scaled(amount: U256, index: U256) -> Result<U256> {
    ray_div_ceil(amount, index)
}

#[inline]
pub fn v_token_burn_scaled(amount: U256, index: U256) -> Result<U256> {
    ray_div_floor(amount, index)
}

#[inline]
pub fn linear_interest(rate: U256, dt: u64) -> Result<U256> {
    let growth = mul_div(rate, U256::from(dt), SECONDS_PER_YEAR, Rounding::Down)?;
    RAY.checked_add(growth)
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// `MathUtils.calculateCompoundedInterest` binomial, `rayMul` half-up.
#[inline]
pub fn compounded_interest(rate: U256, dt: u64) -> Result<U256> {
    if dt == 0 {
        return Ok(RAY);
    }
    let x = mul_div(rate, U256::from(dt), SECONDS_PER_YEAR, Rounding::Down)?;
    let x6 = x
        .checked_div(U256::from(6u8))
        .ok_or(FixedError::DivisionByZero)?;
    let inner = x
        .checked_div(U256::from(2u8))
        .and_then(|h| h.checked_add(ray_mul(x, x6).ok()?))
        .ok_or(FixedError::Overflow)?;
    RAY.checked_add(x)
        .and_then(|v| v.checked_add(ray_mul(x, inner).ok()?))
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

#[inline]
pub fn normalized_income(r: &Reserve, last: u32, ts: u64) -> Result<U256> {
    let stored = U256::from(r.liquidity_index);
    let last = u64::from(last);
    if ts < last {
        return Err(ProtocolError::TimestampBeforeUpdate);
    }
    if ts == last {
        return Ok(stored);
    }
    ray_mul(
        linear_interest(U256::from(r.liquidity_rate), ts.wrapping_sub(last))?,
        stored,
    )
}

#[inline]
pub fn normalized_debt(r: &Reserve, last: u32, ts: u64) -> Result<U256> {
    let stored = U256::from(r.variable_borrow_index);
    let last = u64::from(last);
    if ts < last {
        return Err(ProtocolError::TimestampBeforeUpdate);
    }
    if ts == last {
        return Ok(stored);
    }
    ray_mul(
        compounded_interest(U256::from(r.variable_borrow_rate), ts.wrapping_sub(last))?,
        stored,
    )
}

#[inline]
pub fn asset_unit(decimals: u8) -> Result<U256> {
    U256::from(10u8)
        .checked_pow(U256::from(decimals))
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// Oracle integer → RAY. `scale = 10^(27 - oracle_decimals)`.
#[inline]
pub fn p_of(price: Ray, scale: U256) -> Option<U256> {
    let p = price.raw().wrapping_div(scale);
    (!p.is_zero()).then_some(p)
}

#[inline]
pub fn hf_wad_to_ray(hf_wad: U256) -> Result<Ray> {
    if hf_wad == U256::MAX {
        return Ok(Ray::from_raw(U256::MAX));
    }
    hf_wad
        .checked_mul(WAD_RAY_RATIO)
        .map(Ray::from_raw)
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

#[inline]
pub fn value_ray_of(amount: U256, price: Ray, decimals: u8) -> Result<U256> {
    Ok(mul_div(
        amount,
        price.raw(),
        asset_unit(decimals)?,
        Rounding::Down,
    )?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod rounding_direction {
    use super::{a_token_balance, v_token_balance, RAY};

    use alloy_primitives::U256;

    /// `scaled * index` not divisible by RAY. Floor and ceil differ by 1.
    /// An index of RAY makes them identical, so a ceil↔floor flip stays green.
    #[test]
    fn supply_floors_and_debt_ceils_off_the_ray_identity() {
        let scaled = U256::from(1u8);
        let index = RAY + U256::from(1u8);
        let floor = scaled * index / RAY;
        let ceil = (scaled * index + RAY - U256::from(1u8)) / RAY;
        assert_eq!(floor, U256::from(1u8));
        assert_eq!(ceil, U256::from(2u8));
        assert_eq!(a_token_balance(scaled, index).unwrap(), floor);
        assert_eq!(v_token_balance(scaled, index).unwrap(), ceil);
    }
}

/// Silence unused half constants that document the chain's HALF_* (used via Rounding::HalfUp).
const _: () = {
    let _ = HALF_BPS.as_limbs();
    let _ = HALF_RAY.as_limbs();
    let _ = HALF_WAD.as_limbs();
};
