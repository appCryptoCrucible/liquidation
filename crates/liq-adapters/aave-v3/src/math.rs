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
pub fn ray_div(a: U256, b: U256) -> Result<U256> {
    Ok(mul_div(a, RAY, b, Rounding::HalfUp)?)
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

/// `PercentageMath.percentDiv`, half-up: `(v·10_000 + p/2) / p`.
#[inline]
pub fn percent_div(v: U256, p: U256) -> Result<U256> {
    Ok(mul_div(v, BPS, p, Rounding::HalfUp)?)
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
pub fn supply_assets(
    model: crate::config::BalanceModel,
    scaled: U256,
    index: U256,
) -> Result<U256> {
    match model {
        crate::config::BalanceModel::TokenMath35 => a_token_balance(scaled, index),
        crate::config::BalanceModel::WadRayHalfUp => ray_mul(scaled, index),
    }
}

#[inline]
pub fn debt_assets(model: crate::config::BalanceModel, scaled: U256, index: U256) -> Result<U256> {
    match model {
        crate::config::BalanceModel::TokenMath35 => v_token_balance(scaled, index),
        crate::config::BalanceModel::WadRayHalfUp => ray_mul(scaled, index),
    }
}

#[inline]
pub fn supply_mint_scaled(
    model: crate::config::BalanceModel,
    amount: U256,
    index: U256,
) -> Result<U256> {
    match model {
        crate::config::BalanceModel::TokenMath35 => a_token_mint_scaled(amount, index),
        crate::config::BalanceModel::WadRayHalfUp => ray_div(amount, index),
    }
}

#[inline]
pub fn supply_burn_scaled(
    model: crate::config::BalanceModel,
    amount: U256,
    index: U256,
) -> Result<U256> {
    match model {
        crate::config::BalanceModel::TokenMath35 => a_token_burn_scaled(amount, index),
        crate::config::BalanceModel::WadRayHalfUp => ray_div(amount, index),
    }
}

#[inline]
pub fn debt_mint_scaled(
    model: crate::config::BalanceModel,
    amount: U256,
    index: U256,
) -> Result<U256> {
    match model {
        crate::config::BalanceModel::TokenMath35 => v_token_mint_scaled(amount, index),
        crate::config::BalanceModel::WadRayHalfUp => ray_div(amount, index),
    }
}

#[inline]
pub fn debt_burn_scaled(
    model: crate::config::BalanceModel,
    amount: U256,
    index: U256,
) -> Result<U256> {
    match model {
        crate::config::BalanceModel::TokenMath35 => v_token_burn_scaled(amount, index),
        crate::config::BalanceModel::WadRayHalfUp => ray_div(amount, index),
    }
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
/// Aave V2 `MathUtils.calculateCompoundedInterest` (pool `0x02d84abd…`):
/// the per-second rate first, then `1 + r·n + n(n−1)·r²/2 + n(n−1)(n−2)·r³/6`
/// with `r²`, `r³` by half-up `rayMul`. Its roundings differ from V3's.
pub fn compounded_interest_v2(rate: U256, dt: u64) -> Result<U256> {
    if dt == 0 {
        return Ok(RAY);
    }
    let of = || ProtocolError::Fixed(FixedError::Overflow);
    let exp = U256::from(dt);
    let exp1 = U256::from(dt.saturating_sub(1));
    let exp2 = U256::from(if dt > 2 { dt.saturating_sub(2) } else { 0 });
    let r = rate.checked_div(SECONDS_PER_YEAR).ok_or(of())?;
    let p2 = ray_mul(r, r)?;
    let p3 = ray_mul(p2, r)?;
    let second = exp
        .checked_mul(exp1)
        .and_then(|v| v.checked_mul(p2))
        .ok_or(of())?
        .checked_div(U256::from(2u8))
        .ok_or(of())?;
    let third = exp
        .checked_mul(exp1)
        .and_then(|v| v.checked_mul(exp2))
        .and_then(|v| v.checked_mul(p3))
        .ok_or(of())?
        .checked_div(U256::from(6u8))
        .ok_or(of())?;
    RAY.checked_add(r.checked_mul(exp).ok_or(of())?)
        .and_then(|v| v.checked_add(second))
        .and_then(|v| v.checked_add(third))
        .ok_or(of())
}

/// [`normalized_debt`] by the instance's own compounding.
pub fn normalized_debt_for(
    version: crate::config::AaveVersion,
    r: &Reserve,
    last: u32,
    ts: u64,
) -> Result<U256> {
    if version == crate::config::AaveVersion::V3 {
        return normalized_debt(r, last, ts);
    }
    let stored = U256::from(r.variable_borrow_index);
    let last = u64::from(last);
    if ts < last {
        return Err(ProtocolError::TimestampBeforeUpdate);
    }
    if ts == last {
        return Ok(stored);
    }
    ray_mul(
        compounded_interest_v2(U256::from(r.variable_borrow_rate), ts.wrapping_sub(last))?,
        stored,
    )
}

/// Aave V2 `GenericLogic.calculateUserAccountData` health factor: the
/// liquidation threshold averaged over the collateral (floor), then
/// `totalCollateral.percentMul(avgLT).wadDiv(totalDebt)`, both half-up.
/// `weighted` is `Σ value·LT` over collateral with a non-zero threshold,
/// `collateral` their `Σ value`.
pub fn hf_wad_v2(weighted: U256, collateral: U256, debt: U256) -> Result<U256> {
    if debt.is_zero() {
        return Ok(U256::MAX);
    }
    let avg_lt = if collateral.is_zero() {
        U256::ZERO
    } else {
        weighted
            .checked_div(collateral)
            .ok_or(ProtocolError::Fixed(FixedError::DivisionByZero))?
    };
    wad_div(percent_mul(collateral, avg_lt)?, debt)
}

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

/// Silence unused half constants that document the chain's HALF_* (used via Rounding::HalfUp).
const _: () = {
    let _ = HALF_BPS.as_limbs();
    let _ = HALF_RAY.as_limbs();
    let _ = HALF_WAD.as_limbs();
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod rounding_direction {
    use super::{
        a_token_balance, debt_assets, supply_assets, supply_mint_scaled, v_token_balance, HALF_RAY,
        RAY,
    };
    use crate::config::BalanceModel;

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

    /// Oracle: Spark aToken `balanceOf` is `scaled.rayMul(index)` with
    /// `c = (a * b + HALF_RAY) / RAY` (deployed `WadRayMath` at aToken impl
    /// `0x6175ddec…`). Remainder exactly `HALF_RAY` steps up. TokenMath floors it.
    #[test]
    fn spark_supply_half_up_is_not_the_token_math_floor() {
        let scaled = U256::from(1u8);
        let index = HALF_RAY;
        let half_up = (scaled * index + HALF_RAY) / RAY;
        assert_eq!(half_up, U256::from(1u8));
        assert_eq!(
            supply_assets(BalanceModel::WadRayHalfUp, scaled, index).unwrap(),
            half_up
        );
        assert_eq!(
            supply_assets(BalanceModel::TokenMath35, scaled, index).unwrap(),
            U256::ZERO
        );
        assert_ne!(
            supply_assets(BalanceModel::WadRayHalfUp, scaled, index).unwrap(),
            U256::ZERO
        );
    }

    /// Remainder 1 is below half a ray, so Spark debt stays on the floor.
    /// TokenMath ceils it. A ceil mutation of the Spark path goes red.
    #[test]
    fn spark_debt_half_up_does_not_ceil_a_one_wei_remainder() {
        let scaled = U256::from(1u8);
        let index = U256::from(1u8);
        let half_up = (scaled * index + HALF_RAY) / RAY;
        let ceil = (scaled * index + RAY - U256::from(1u8)) / RAY;
        assert_eq!(half_up, U256::ZERO);
        assert_eq!(ceil, U256::from(1u8));
        assert_eq!(
            debt_assets(BalanceModel::WadRayHalfUp, scaled, index).unwrap(),
            half_up
        );
        assert_ne!(
            debt_assets(BalanceModel::WadRayHalfUp, scaled, index).unwrap(),
            ceil
        );
    }

    /// Oracle: Spark `_mintScaled` uses `amount.rayDiv(index)`, half-up both
    /// ways. `(1 * RAY + index/2) / index` with index = RAY + 1 is 1.
    /// TokenMath's supply mint floors that to 0.
    #[test]
    fn spark_mint_scaled_is_half_up_ray_div() {
        let amount = U256::from(1u8);
        let index = RAY + U256::from(1u8);
        let half_up = (amount * RAY + index / U256::from(2u8)) / index;
        assert_eq!(half_up, U256::from(1u8));
        assert_eq!(
            supply_mint_scaled(BalanceModel::WadRayHalfUp, amount, index).unwrap(),
            half_up
        );
        assert_eq!(
            supply_mint_scaled(BalanceModel::TokenMath35, amount, index).unwrap(),
            U256::ZERO
        );
    }
}

/// Aave V2's own roundings, worked from its deployed Solidity
/// (`MathUtils`, `GenericLogic`, `PercentageMath`, `WadRayMath` of pool
/// `0x02d84abd…`) with exact integers.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod v2_math {
    use super::{compounded_interest_v2, hf_wad_v2, percent_div, percent_mul, RAY};
    use alloy_primitives::U256;

    /// `calculateCompoundedInterest` at 5 % for 3 s and for a day, and at
    /// 30 % for a year; zero time is one RAY.
    #[test]
    fn compounding_matches_v2_math_utils() {
        let r5 = U256::from(5u64) * U256::from(10u64).pow(U256::from(25u8));
        let r30 = U256::from(3u64) * U256::from(10u64).pow(U256::from(26u8));
        assert_eq!(compounded_interest_v2(r5, 0).unwrap(), RAY);
        assert_eq!(
            compounded_interest_v2(r5, 3).unwrap(),
            "1000000004756468805106019786".parse::<U256>().unwrap()
        );
        assert_eq!(
            compounded_interest_v2(r5, 86_400).unwrap(),
            "1000136995684314615598974400".parse::<U256>().unwrap()
        );
        assert_eq!(
            compounded_interest_v2(r30, 31_536_000).unwrap(),
            "1349500611679432534313416000".parse::<U256>().unwrap()
        );
    }

    /// Two collaterals (82.5 % and 70 %) against 1.8 ETH of debt: the
    /// threshold averages to 7_694 (floor), and the health factor is
    /// `percentMul(collateral, 7_694).wadDiv(debt)`, half-up.
    #[test]
    fn health_factor_averages_the_threshold_first() {
        let c1 = U256::from(1_234_567_890_123_456_789u128);
        let c2 = U256::from(987_654_321_098_765_432u128);
        let weighted = c1 * U256::from(8_250u16) + c2 * U256::from(7_000u16);
        let hf = hf_wad_v2(weighted, c1 + c2, U256::from(1_800_000_000_000_000_000u128)).unwrap();
        assert_eq!(hf, U256::from(949_876_538_507_987_654u128));
        assert_eq!(hf_wad_v2(weighted, c1 + c2, U256::ZERO).unwrap(), U256::MAX);
    }

    /// `_calculateAvailableCollateralToLiquidate`: 1,000 USDC of debt at
    /// 0.0004 ETH with a 105 % bonus seizes 0.42 WETH (bonus applied before
    /// the division); half that balance repays 500 USDC (`percentDiv`).
    #[test]
    fn seize_and_debt_needed_round_as_v2_does() {
        let debt_p = U256::from(400_000_000_000_000u64);
        let coll_p = U256::from(1_000_000_000_000_000_000u64);
        let cover = U256::from(1_000_000_000u64);
        let e18 = U256::from(10u64).pow(U256::from(18u8));
        let e6 = U256::from(1_000_000u64);
        let bonus = U256::from(10_500u16);
        let max_coll = percent_mul(debt_p * cover * e18, bonus).unwrap() / (coll_p * e6);
        assert_eq!(max_coll, U256::from(420_000_000_000_000_000u64));
        let half = max_coll / U256::from(2u8);
        let need = percent_div(coll_p * half * e6 / (debt_p * e18), bonus).unwrap();
        assert_eq!(need, U256::from(500_000_000u64));
    }
}
