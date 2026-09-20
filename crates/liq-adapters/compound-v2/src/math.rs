//! Compound V2 arithmetic — `ExponentialNoError.sol` +
//! `Comptroller.liquidateCalculateSeizeTokens` /
//! `getHypotheticalAccountLiquidityInternal` @ `a3214f67`.
//! Directions: `docs/coverage/compound-v2-rounding.md`.

use alloy_primitives::{Address, U256};
use liq_protocol::{ProtocolError, Result};
use liq_types::fixed::{mul_div, FixedError, Rounding, WAD, WAD_RAY_RATIO};
use liq_types::Ray;

/// Pin `closeFactorMinMantissa` (0.05e18) — live close factor must be **strictly greater**.
pub const CLOSE_FACTOR_MIN_MANTISSA: U256 = U256::from_limbs([50_000_000_000_000_000, 0, 0, 0]);
/// Pin `closeFactorMaxMantissa` (0.9e18).
pub const CLOSE_FACTOR_MAX_MANTISSA: U256 = U256::from_limbs([900_000_000_000_000_000, 0, 0, 0]);
/// Pin `collateralFactorMaxMantissa`.
pub const COLLATERAL_FACTOR_MAX_MANTISSA: U256 = CLOSE_FACTOR_MAX_MANTISSA;
/// `ExponentialNoError.expScale`.
pub const EXP_SCALE: U256 = WAD;
/// Pin `isDeprecated`: `reserveFactorMantissa == 1e18`.
pub const DEPRECATED_RESERVE_FACTOR: U256 = WAD;

#[inline]
pub fn mul_div_down(a: U256, b: U256, d: U256) -> Result<U256> {
    Ok(mul_div(a, b, d, Rounding::Down)?)
}

#[inline]
pub fn addr20(a: Address) -> [u8; 20] {
    a.into()
}

#[inline]
pub fn addr_from(b: [u8; 20]) -> Address {
    Address::from(b)
}

/// Pin `_setCloseFactor` bounds: `min < closeFactor <= max`.
#[inline]
pub fn close_factor_in_pin_bounds(mantissa: U256) -> bool {
    mantissa > CLOSE_FACTOR_MIN_MANTISSA && mantissa <= CLOSE_FACTOR_MAX_MANTISSA
}

/// `mul_(Exp a, Exp b)` — `a.mantissa * b.mantissa / expScale` floor.
#[inline]
pub fn mul_exp(a: U256, b: U256) -> Result<U256> {
    mul_div_down(a, b, EXP_SCALE)
}

/// `div_(Exp a, Exp b)` — `a.mantissa * expScale / b.mantissa` floor.
#[inline]
pub fn div_exp(a: U256, b: U256) -> Result<U256> {
    if b.is_zero() {
        return Err(ProtocolError::Fixed(FixedError::DivisionByZero));
    }
    mul_div_down(a, EXP_SCALE, b)
}

/// `mul_ScalarTruncate(Exp a, uint scalar)` — `a.mantissa * scalar / expScale`.
#[inline]
pub fn mul_scalar_truncate(exp_mantissa: U256, scalar: U256) -> Result<U256> {
    mul_div_down(exp_mantissa, scalar, EXP_SCALE)
}

/// `mul_ScalarTruncateAddUInt`.
#[inline]
pub fn mul_scalar_truncate_add(exp_mantissa: U256, scalar: U256, addend: U256) -> Result<U256> {
    let t = mul_scalar_truncate(exp_mantissa, scalar)?;
    t.checked_add(addend)
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// `borrowBalanceStoredInternal`: `principal * marketIndex / accountIndex`.
#[inline]
pub fn borrow_balance_stored(
    principal: U256,
    market_index: U256,
    account_index: U256,
) -> Result<U256> {
    if principal.is_zero() {
        return Ok(U256::ZERO);
    }
    if account_index.is_zero() {
        return Err(ProtocolError::Internal);
    }
    mul_div_down(principal, market_index, account_index)
}

/// `exchangeRateStoredInternal` when `totalSupply != 0`.
#[inline]
pub fn exchange_rate_stored(
    cash: U256,
    borrows: U256,
    reserves: U256,
    supply: U256,
) -> Result<U256> {
    if supply.is_zero() {
        return Err(ProtocolError::Internal);
    }
    let num = cash
        .checked_add(borrows)
        .ok_or(FixedError::Overflow)?
        .checked_sub(reserves)
        .ok_or(FixedError::Underflow)?;
    mul_div_down(num, EXP_SCALE, supply)
}

/// Pin `isDeprecated`.
#[inline]
pub fn is_deprecated(collateral_factor: U256, borrow_paused: bool, reserve_factor: U256) -> bool {
    collateral_factor.is_zero() && borrow_paused && reserve_factor == DEPRECATED_RESERVE_FACTOR
}

/// Bonus RAY: `liquidationIncentiveMantissa − 1e18`, scaled WAD→RAY.
/// Incentive is the admin file, never a `1.08` constant.
#[inline]
pub fn bonus_ray(incentive_mantissa: U256) -> Result<Ray> {
    if incentive_mantissa < WAD {
        return Err(ProtocolError::Internal);
    }
    let extra = incentive_mantissa
        .checked_sub(WAD)
        .ok_or(FixedError::Underflow)?;
    extra
        .checked_mul(WAD_RAY_RATIO)
        .map(Ray::from_raw)
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
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
pub fn asset_unit(decimals: u8) -> Result<U256> {
    U256::from(10u8)
        .checked_pow(U256::from(decimals))
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// Amount · RAY-price / 10^decimals → WAD numeraire, floor.
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

/// Compound oracle mantissa from PriceVector RAY-per-1.0-token:
/// `price_wad * 10^(18 - decimals)` so `mul_ScalarTruncate` matches
/// `value_wad` for the same raw amount.
#[inline]
pub fn compound_price_mantissa(price_ray: U256, decimals: u8) -> Result<U256> {
    let price_wad = mul_div_down(price_ray, U256::ONE, WAD_RAY_RATIO)?;
    if decimals > 18 {
        return Err(ProtocolError::Internal);
    }
    let exp = 18u32.saturating_sub(u32::from(decimals));
    let scale = U256::from(10u8)
        .checked_pow(U256::from(exp))
        .ok_or(FixedError::Overflow)?;
    price_wad
        .checked_mul(scale)
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// Pin `liquidateCalculateSeizeTokens` (error-free path):
/// `ratio = (incentive * priceBorrowed) / (priceCollateral * exchangeRate)`
/// then `seizeTokens = mul_ScalarTruncate(ratio, actualRepayAmount)`.
/// Prices are Compound Exp mantissas (not a `1.08` constant).
#[inline]
pub fn liquidate_calculate_seize_tokens(
    incentive_mantissa: U256,
    price_borrowed_mantissa: U256,
    price_collateral_mantissa: U256,
    exchange_rate_mantissa: U256,
    actual_repay: U256,
) -> Result<U256> {
    if price_borrowed_mantissa.is_zero() || price_collateral_mantissa.is_zero() {
        return Err(ProtocolError::Internal);
    }
    if exchange_rate_mantissa.is_zero() {
        return Err(ProtocolError::Fixed(FixedError::DivisionByZero));
    }
    let numerator = mul_exp(incentive_mantissa, price_borrowed_mantissa)?;
    let denominator = mul_exp(price_collateral_mantissa, exchange_rate_mantissa)?;
    if denominator.is_zero() {
        return Err(ProtocolError::Fixed(FixedError::DivisionByZero));
    }
    let ratio = div_exp(numerator, denominator)?;
    mul_scalar_truncate(ratio, actual_repay)
}

/// cToken units → underlying via stored exchange rate.
#[inline]
pub fn ctokens_to_underlying(ctokens: U256, exchange_rate_mantissa: U256) -> Result<U256> {
    mul_scalar_truncate(exchange_rate_mantissa, ctokens)
}

#[inline]
pub fn last_update(ts: u64) -> Result<u32> {
    u32::try_from(ts).map_err(|_| ProtocolError::Fixed(FixedError::Overflow))
}

#[inline]
pub fn u128_of(v: U256) -> Result<u128> {
    u128::try_from(v).map_err(|_| ProtocolError::MalformedLog)
}
