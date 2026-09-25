//! Morpho Blue arithmetic — `MathLib` / `SharesMathLib` / `ConstantsLib`
//! @ `8e26ca6a`. Directions: `docs/coverage/morpho-blue-rounding.md`.
//! Morpho does **not** use half-up; every step is floor or ceil.

use alloy_primitives::{uint, U256};
use liq_protocol::{ProtocolError, Result};
use liq_types::fixed::{mul_div, FixedError, Rounding, WAD, WAD_RAY_RATIO};
use liq_types::Ray;

use crate::layout::LoanRow;

pub const VIRTUAL_SHARES: U256 = uint!(1_000_000_U256);
pub const VIRTUAL_ASSETS: U256 = uint!(1_U256);
pub const ORACLE_PRICE_SCALE: U256 = uint!(1_000_000_000_000_000_000_000_000_000_000_000_000_U256);
pub const LIQUIDATION_CURSOR: U256 = uint!(300_000_000_000_000_000_U256);
pub const MAX_LIF: U256 = uint!(1_150_000_000_000_000_000_U256);
pub const MAX_FEE: U256 = uint!(250_000_000_000_000_000_U256);
pub const HF_THRESHOLD_WAD: U256 = WAD;

#[inline]
pub fn mul_div_down(a: U256, b: U256, d: U256) -> Result<U256> {
    Ok(mul_div(a, b, d, Rounding::Down)?)
}

#[inline]
pub fn mul_div_up(a: U256, b: U256, d: U256) -> Result<U256> {
    Ok(mul_div(a, b, d, Rounding::Up)?)
}

#[inline]
pub fn w_mul_down(a: U256, b: U256) -> Result<U256> {
    mul_div_down(a, b, WAD)
}

#[inline]
pub fn w_div_down(a: U256, b: U256) -> Result<U256> {
    mul_div_down(a, WAD, b)
}

#[inline]
pub fn w_div_up(a: U256, b: U256) -> Result<U256> {
    mul_div_up(a, WAD, b)
}

#[inline]
pub fn to_shares_down(assets: U256, total_assets: U256, total_shares: U256) -> Result<U256> {
    mul_div_down(
        assets,
        total_shares
            .checked_add(VIRTUAL_SHARES)
            .ok_or(FixedError::Overflow)?,
        total_assets
            .checked_add(VIRTUAL_ASSETS)
            .ok_or(FixedError::Overflow)?,
    )
}

#[inline]
pub fn to_shares_up(assets: U256, total_assets: U256, total_shares: U256) -> Result<U256> {
    mul_div_up(
        assets,
        total_shares
            .checked_add(VIRTUAL_SHARES)
            .ok_or(FixedError::Overflow)?,
        total_assets
            .checked_add(VIRTUAL_ASSETS)
            .ok_or(FixedError::Overflow)?,
    )
}

#[inline]
pub fn to_assets_down(shares: U256, total_assets: U256, total_shares: U256) -> Result<U256> {
    mul_div_down(
        shares,
        total_assets
            .checked_add(VIRTUAL_ASSETS)
            .ok_or(FixedError::Overflow)?,
        total_shares
            .checked_add(VIRTUAL_SHARES)
            .ok_or(FixedError::Overflow)?,
    )
}

#[inline]
pub fn to_assets_up(shares: U256, total_assets: U256, total_shares: U256) -> Result<U256> {
    mul_div_up(
        shares,
        total_assets
            .checked_add(VIRTUAL_ASSETS)
            .ok_or(FixedError::Overflow)?,
        total_shares
            .checked_add(VIRTUAL_SHARES)
            .ok_or(FixedError::Overflow)?,
    )
}

/// `MathLib.wTaylorCompounded`: `x*n + (xn)^2/(2W) + …/(3W)` all floor.
#[inline]
pub fn w_taylor_compounded(rate: U256, elapsed: U256) -> Result<U256> {
    let first = rate.checked_mul(elapsed).ok_or(FixedError::Overflow)?;
    let two_wad = WAD
        .checked_mul(U256::from(2u8))
        .ok_or(FixedError::Overflow)?;
    let three_wad = WAD
        .checked_mul(U256::from(3u8))
        .ok_or(FixedError::Overflow)?;
    let second = mul_div_down(first, first, two_wad)?;
    let third = mul_div_down(second, first, three_wad)?;
    first
        .checked_add(second)
        .and_then(|v| v.checked_add(third))
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

#[inline]
pub fn asset_unit(decimals: u8) -> Result<U256> {
    U256::from(10u8)
        .checked_pow(U256::from(decimals))
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// Accrue `LoanRow` totals to `ts` with the last IRM rate (constant between
/// `AccrueInterest` logs — Morpho calls `IIrm.borrowRate` once per accrue).
#[inline]
pub fn accrued(row: &LoanRow, last_update: u32, ts: u64) -> Result<(U256, U256, U256, U256)> {
    let last = u64::from(last_update);
    if ts < last {
        return Err(ProtocolError::TimestampBeforeUpdate);
    }
    let mut supply_a = U256::from(row.total_supply_assets);
    let mut supply_s = U256::from(row.total_supply_shares);
    let mut borrow_a = U256::from(row.total_borrow_assets);
    let borrow_s = U256::from(row.total_borrow_shares);
    let elapsed = ts.wrapping_sub(last);
    if elapsed == 0 || row.last_borrow_rate == 0 {
        return Ok((supply_a, supply_s, borrow_a, borrow_s));
    }
    let interest = w_mul_down(
        borrow_a,
        w_taylor_compounded(U256::from(row.last_borrow_rate), U256::from(elapsed))?,
    )?;
    borrow_a = borrow_a.checked_add(interest).ok_or(FixedError::Overflow)?;
    supply_a = supply_a.checked_add(interest).ok_or(FixedError::Overflow)?;
    if row.fee != 0 && !interest.is_zero() {
        let fee_amount = w_mul_down(interest, U256::from(row.fee))?;
        let fee_shares = to_shares_down(
            fee_amount,
            supply_a
                .checked_sub(fee_amount)
                .ok_or(FixedError::Underflow)?,
            supply_s,
        )?;
        supply_s = supply_s
            .checked_add(fee_shares)
            .ok_or(FixedError::Overflow)?;
    }
    Ok((supply_a, supply_s, borrow_a, borrow_s))
}

/// `liquidationIncentiveFactor = min(MAX_LIF, WAD.wDivDown(WAD - cursor.wMulDown(WAD - lltv)))`.
#[inline]
pub fn liquidation_incentive_factor(lltv: U256) -> Result<U256> {
    let inner = w_mul_down(
        LIQUIDATION_CURSOR,
        WAD.checked_sub(lltv).ok_or(FixedError::Underflow)?,
    )?;
    let den = WAD.checked_sub(inner).ok_or(FixedError::Underflow)?;
    let lif = w_div_down(WAD, den)?;
    Ok(if lif > MAX_LIF { MAX_LIF } else { lif })
}

/// LIF − 1 as engine RAY bonus.
#[inline]
pub fn bonus_ray(lif: U256) -> Result<Ray> {
    lif.checked_sub(WAD)
        .and_then(|b| b.checked_mul(WAD_RAY_RATIO))
        .map(Ray::from_raw)
        .ok_or(ProtocolError::Fixed(FixedError::Underflow))
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
pub fn value_wad(amount: U256, price_ray: U256, decimals: u8) -> Result<U256> {
    mul_div_down(
        amount,
        price_ray,
        asset_unit(decimals)?
            .checked_mul(WAD_RAY_RATIO)
            .ok_or(FixedError::Overflow)?,
    )
}

/// Silence MAX_FEE (owner cap; recorded from source, not a health step).
const _: () = {
    let _ = MAX_FEE.as_limbs();
};

#[cfg(test)]
mod lif_pin {
    use super::{liquidation_incentive_factor, MAX_LIF};
    use alloy_primitives::{uint, U256};

    #[test]
    fn incentive_matches_the_documented_floor_and_caps() {
        // lltv = 0.86e18. cursor.wMulDown(WAD - lltv) = 0.042e18.
        // WAD.wDivDown(WAD - that) = floor(1e36 / 0.958e18).
        let lif = liquidation_incentive_factor(uint!(860_000_000_000_000_000_U256));
        assert_eq!(lif.ok(), Some(uint!(1_043_841_336_116_910_229_U256)));
        // lltv = 0. Uncapped floor(1e36 / 0.7e18) is above 1.15e18, so the
        // function returns the cap and not that larger integer.
        let capped = liquidation_incentive_factor(U256::ZERO);
        assert_eq!(capped.ok(), Some(MAX_LIF));
        assert_ne!(capped.ok(), Some(uint!(1_428_571_428_571_428_571_U256)));
        assert!(liquidation_incentive_factor(uint!(1_000_000_000_000_000_001_U256)).is_err());
    }
}
