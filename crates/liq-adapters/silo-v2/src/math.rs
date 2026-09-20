//! Silo V2 arithmetic — `SiloMathLib` / `SiloSolvencyLib` / `PartialLiquidationLib`
//! / `Rounding.sol` @ `570a668a98a88a6a2b92697e7b9a3b1c6299dce7`.
//! Directions: `docs/coverage/silo-v2-rounding.md`.

use alloy_primitives::{uint, U256};
use liq_protocol::{ProtocolError, Result};
use liq_types::fixed::{mul_div, FixedError, Rounding, WAD, WAD_RAY_RATIO};
use liq_types::Ray;

pub const PRECISION: U256 = WAD;
pub const DECIMALS_OFFSET_POW: U256 = uint!(1_000_U256);
pub const UNDERESTIMATION: U256 = uint!(2_U256);
pub const FULL_LIQUIDATION_THRESHOLD: U256 = uint!(900_000_000_000_000_000_U256);
pub const BAD_DEBT_WAD: U256 = WAD;
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
pub fn asset_unit(decimals: u8) -> Result<U256> {
    U256::from(10u8)
        .checked_pow(U256::from(decimals))
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// `SiloMathLib._commonConvertTo` then `convertToAssets`.
#[inline]
pub fn convert_to_assets(
    shares: U256,
    total_assets: U256,
    total_shares: U256,
    debt: bool,
    up: bool,
) -> Result<U256> {
    let (ts, ta) = if debt {
        (total_shares, total_assets)
    } else if total_shares.is_zero() {
        (DECIMALS_OFFSET_POW, U256::ONE)
    } else {
        (
            total_shares
                .checked_add(DECIMALS_OFFSET_POW)
                .ok_or(FixedError::Overflow)?,
            total_assets
                .checked_add(U256::ONE)
                .ok_or(FixedError::Overflow)?,
        )
    };
    if ts.is_zero() {
        return Ok(shares);
    }
    if up {
        mul_div_up(shares, ta, ts)
    } else {
        mul_div_down(shares, ta, ts)
    }
}

/// `SiloSolvencyLib.ltvMath` — `Rounding.LTV` is ceil.
#[inline]
pub fn ltv_math(debt_value: U256, coll_value: U256) -> Result<U256> {
    if coll_value.is_zero() {
        return Ok(if debt_value.is_zero() {
            U256::ZERO
        } else {
            U256::MAX
        });
    }
    mul_div_up(debt_value, PRECISION, coll_value)
}

/// `isSolvent`: `ltv <= lt`. Equivalent: `debt_value * 1e18 <= lt * coll_value`.
#[inline]
pub fn is_solvent(debt_value: U256, coll_value: U256, lt: U256) -> Result<bool> {
    if debt_value.is_zero() {
        return Ok(true);
    }
    Ok(ltv_math(debt_value, coll_value)? <= lt)
}

/// Normalised HF so `hf >= 1` iff `isSolvent`. Floor: `lt * coll * 1e9 / debt`.
#[inline]
pub fn hf_ray(debt_value: U256, coll_value: U256, lt: U256) -> Result<Ray> {
    if debt_value.is_zero() {
        return Ok(Ray::from_raw(U256::MAX));
    }
    if coll_value.is_zero() {
        return Ok(Ray::ZERO);
    }
    mul_div_down(
        lt,
        coll_value
            .checked_mul(WAD_RAY_RATIO)
            .ok_or(FixedError::Overflow)?,
        debt_value,
    )
    .map(Ray::from_raw)
}

/// `PartialLiquidationLib.valueToAssetsByRatio` — floor; reverts `UnknownRatio` on 0.
#[inline]
pub fn value_to_assets_by_ratio(
    value: U256,
    total_assets: U256,
    total_value: U256,
) -> Result<U256> {
    if total_value.is_zero() {
        return Err(ProtocolError::Internal);
    }
    mul_div_down(value, total_assets, total_value)
}

/// `PartialLiquidationLib.calculateCollateralToLiquidate`.
#[inline]
pub fn calculate_collateral_to_liquidate(
    debt_value_to_cover: U256,
    sum_of_collateral: U256,
    liquidation_fee: U256,
) -> Result<U256> {
    let fee = mul_div_down(debt_value_to_cover, liquidation_fee, PRECISION)?;
    let to_liq = debt_value_to_cover
        .checked_add(fee)
        .ok_or(FixedError::Overflow)?;
    Ok(if to_liq > sum_of_collateral {
        sum_of_collateral
    } else {
        to_liq
    })
}

/// `PartialLiquidationLib.estimateMaxRepayValue`.
pub fn estimate_max_repay_value(
    total_debt_value: U256,
    total_coll_value: U256,
    ltv_after: U256,
    liquidation_fee: U256,
) -> Result<U256> {
    if total_debt_value.is_zero() {
        return Ok(U256::ZERO);
    }
    if liquidation_fee >= PRECISION {
        return Ok(U256::ZERO);
    }
    if total_debt_value >= total_coll_value || ltv_after.is_zero() {
        return Ok(total_debt_value);
    }
    let lt_cv = ltv_after
        .checked_mul(total_coll_value)
        .ok_or(FixedError::Overflow)?;
    let debt_scaled = total_debt_value
        .checked_mul(PRECISION)
        .ok_or(FixedError::Overflow)?;
    if lt_cv >= debt_scaled {
        return Ok(U256::ZERO);
    }
    let mut repay = debt_scaled
        .checked_sub(lt_cv)
        .ok_or(FixedError::Underflow)?;
    let fee_term = mul_div_down(ltv_after, liquidation_fee, PRECISION)?;
    let divider_r = ltv_after
        .checked_add(fee_term)
        .ok_or(FixedError::Overflow)?;
    if divider_r >= PRECISION {
        return Ok(total_debt_value);
    }
    let den = PRECISION
        .checked_sub(divider_r)
        .ok_or(FixedError::Underflow)?;
    repay = repay.checked_div(den).ok_or(FixedError::DivisionByZero)?;
    if repay > total_debt_value {
        return Ok(total_debt_value);
    }
    let ratio = mul_div_down(repay, PRECISION, total_debt_value)?;
    Ok(if ratio > FULL_LIQUIDATION_THRESHOLD {
        total_debt_value
    } else {
        repay
    })
}

/// `PartialLiquidationLib.maxLiquidation` (preview + underestimation).
pub fn max_liquidation(
    sum_coll_assets: U256,
    sum_coll_value: U256,
    borrower_debt_assets: U256,
    borrower_debt_value: U256,
    target_ltv: U256,
    liquidation_fee: U256,
) -> Result<(U256, U256)> {
    if sum_coll_value.is_zero() {
        return Ok((sum_coll_assets, borrower_debt_assets));
    }
    let repay_value = estimate_max_repay_value(
        borrower_debt_value,
        sum_coll_value,
        target_ltv,
        liquidation_fee,
    )?;
    let coll_value =
        calculate_collateral_to_liquidate(repay_value, sum_coll_value, liquidation_fee)?;
    let mut coll_assets = value_to_assets_by_ratio(coll_value, sum_coll_assets, sum_coll_value)?;
    if coll_assets > UNDERESTIMATION {
        coll_assets = coll_assets
            .checked_sub(UNDERESTIMATION)
            .ok_or(FixedError::Underflow)?;
    } else {
        coll_assets = U256::ZERO;
    }
    let debt_assets =
        value_to_assets_by_ratio(repay_value, borrower_debt_assets, borrower_debt_value)?;
    Ok((coll_assets, debt_assets))
}

/// `liquidationFee` WAD → engine RAY bonus.
#[inline]
pub fn bonus_ray(fee_wad: U256) -> Result<Ray> {
    fee_wad
        .checked_mul(WAD_RAY_RATIO)
        .map(Ray::from_raw)
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

#[inline]
pub fn value_from_price(amount: U256, price_ray: U256, decimals: u8) -> Result<U256> {
    mul_div_down(amount, price_ray, asset_unit(decimals)?)
}

/// Numeraire WAD: `amount * price_ray / (10^dec * 1e9)`.
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

/// `FullLiquidationRequired` when computed repay > max cover (hook require).
#[inline]
pub fn full_liquidation_required(repay: U256, max_cover: U256) -> bool {
    repay > max_cover
}
