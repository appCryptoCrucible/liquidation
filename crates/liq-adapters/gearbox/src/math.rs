//! Gearbox V3 arithmetic from `CreditFacadeV3` / `CreditLogic` /
//! `CollateralLogic` / `PriceOracleV3` / `Constants.sol`
//! @ `510fc6541c3767ce825929b4c311826fe81d6fa5`.
//! Directions: `docs/coverage/gearbox-rounding.md`.

use alloy_primitives::aliases::U40;
use alloy_primitives::{uint, Address, U256};
use liq_protocol::{ProtocolError, Result};
use liq_types::fixed::{mul_div, FixedError, Rounding, RAY, WAD, WAD_RAY_RATIO};
use liq_types::Ray;

/// `Constants.sol` `PERCENTAGE_FACTOR = 1e4`.
pub const PERCENTAGE_FACTOR: U256 = uint!(10_000_U256);
/// Pin `MAX_SANE_ENABLED_TOKENS`.
pub const MAX_SANE_ENABLED_TOKENS: u8 = 20;
/// Pin `CreditConfiguratorV3._setLiquidationThreshold`:
/// `timestampRampStart: type(uint40).max`. Alloy `U40::MAX` is `2^40-1`.
pub const STATIC_LT_RAMP_START: u64 = {
    let [limb] = *U40::MAX.as_limbs();
    limb
};

const _: () = {
    assert!(STATIC_LT_RAMP_START > u32::MAX as u64);
};

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

#[inline]
pub fn asset_unit(decimals: u8) -> Result<U256> {
    U256::from(10u8)
        .checked_pow(U256::from(decimals))
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// `CreditLogic.calcAccruedInterest`: `(amount * indexNow) / indexLast - amount`.
#[inline]
pub fn calc_accrued_interest(amount: U256, index_last: U256, index_now: U256) -> Result<U256> {
    if amount.is_zero() {
        return Ok(U256::ZERO);
    }
    if index_last.is_zero() {
        return Err(ProtocolError::Fixed(FixedError::DivisionByZero));
    }
    let grown = mul_div_down(amount, index_now, index_last)?;
    grown
        .checked_sub(amount)
        .ok_or(ProtocolError::Fixed(FixedError::Underflow))
}

/// `CreditLogic.calcTotalDebt`.
#[inline]
pub fn calc_total_debt(debt: U256, accrued_interest: U256, accrued_fees: U256) -> Result<U256> {
    debt.checked_add(accrued_interest)
        .and_then(|v| v.checked_add(accrued_fees))
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// Accrued interest fee: `interest * feeInterest / PERCENTAGE_FACTOR` (floor).
#[inline]
pub fn interest_fee(interest: U256, fee_interest: U256) -> Result<U256> {
    mul_div_down(interest, fee_interest, PERCENTAGE_FACTOR)
}

/// `isUnhealthy = twvUSD < totalDebtUSD`. Normalised HF is `twv * RAY / debt`.
#[inline]
pub fn hf_ray(twv: U256, total_debt: U256) -> Result<Ray> {
    if total_debt.is_zero() {
        return Ok(Ray::from_raw(U256::MAX));
    }
    mul_div_down(twv, RAY, total_debt).map(Ray::from_raw)
}

#[inline]
pub fn is_unhealthy(twv: U256, total_debt: U256) -> bool {
    twv < total_debt
}

/// `_isExpired`: expirable && expirationDate != 0 && now >= expirationDate.
#[inline]
pub fn is_expired(expirable: bool, expiration_date: u64, now: u64) -> bool {
    expirable && expiration_date != 0 && now >= expiration_date
}

/// Pin `_hasBadDebt`: `totalValue * liquidationDiscount < (debt + accruedInterest) * PERCENTAGE_FACTOR`.
/// Uses the **non-expired** discount from `fees()`, even on expired accounts.
#[inline]
pub fn has_bad_debt(
    total_value: U256,
    liquidation_discount: U256,
    debt: U256,
    accrued_interest: U256,
) -> Result<bool> {
    let left = total_value
        .checked_mul(liquidation_discount)
        .ok_or(FixedError::Overflow)?;
    let di = debt
        .checked_add(accrued_interest)
        .ok_or(FixedError::Overflow)?;
    let right = di
        .checked_mul(PERCENTAGE_FACTOR)
        .ok_or(FixedError::Overflow)?;
    Ok(left < right)
}

/// `CreditLogic.getLiquidationThreshold` (linear ramp, floor via integer mix).
/// Pin static LT writes [`STATIC_LT_RAMP_START`] (`type(uint40).max`); the first
/// branch is the pin (`now <= timestampRampStart` → `ltInitial`). Not `u32::MAX`.
#[inline]
pub fn get_liquidation_threshold(
    lt_initial: u16,
    lt_final: u16,
    ramp_start: u64,
    ramp_duration: u32,
    now: u64,
) -> Result<u16> {
    if now <= ramp_start {
        return Ok(lt_initial);
    }
    let end = ramp_start
        .checked_add(u64::from(ramp_duration))
        .ok_or(FixedError::Overflow)?;
    if now >= end {
        return Ok(lt_final);
    }
    let left = U256::from(lt_initial)
        .checked_mul(U256::from(
            end.checked_sub(now).ok_or(FixedError::Underflow)?,
        ))
        .ok_or(FixedError::Overflow)?;
    let right = U256::from(lt_final)
        .checked_mul(U256::from(
            now.checked_sub(ramp_start).ok_or(FixedError::Underflow)?,
        ))
        .ok_or(FixedError::Overflow)?;
    let num = left.checked_add(right).ok_or(FixedError::Overflow)?;
    let den = U256::from(end.checked_sub(ramp_start).ok_or(FixedError::Underflow)?);
    let v = mul_div_down(num, U256::ONE, den)?;
    u16::try_from(v).map_err(|_| ProtocolError::Internal)
}

/// `PriceOracleV3.convert`: `amount * priceFrom * scaleTo / (priceTo * scaleFrom)` (floor).
/// `price_*` are RAY USD-per-token; scale is `10 ** decimals`. The 8-dec oracle
/// scale cancels (see rounding table C1).
#[inline]
pub fn convert(
    amount: U256,
    price_from: U256,
    scale_from: U256,
    price_to: U256,
    scale_to: U256,
) -> Result<U256> {
    let b = price_from
        .checked_mul(scale_to)
        .ok_or(FixedError::Overflow)?;
    let d = price_to
        .checked_mul(scale_from)
        .ok_or(FixedError::Overflow)?;
    mul_div_down(amount, b, d)
}

/// Pin `_calcPartialLiquidationPayments` seizedAmount:
/// `convert(amount, underlying, token) * PERCENTAGE_FACTOR / liquidationDiscount`.
#[inline]
pub fn seized_from_amount(
    amount: U256,
    price_underlying: U256,
    scale_underlying: U256,
    price_token: U256,
    scale_token: U256,
    discount: U256,
) -> Result<U256> {
    let converted = convert(
        amount,
        price_underlying,
        scale_underlying,
        price_token,
        scale_token,
    )?;
    mul_div_down(converted, PERCENTAGE_FACTOR, discount)
}

/// Pin `_calcPartialLiquidationPayments` feeAmount:
/// `amount * feeLiquidation / PERCENTAGE_FACTOR`.
#[inline]
pub fn fee_from_amount(amount: U256, fee_liquidation: U256) -> Result<U256> {
    mul_div_down(amount, fee_liquidation, PERCENTAGE_FACTOR)
}

/// Pin `_calcPartialLiquidationPayments` repaidAmount: `amount - feeAmount`
/// (unchecked on chain because configurator keeps fee < 100%).
#[inline]
pub fn repaid_from_amount(amount: U256, fee_liquidation: U256) -> Result<U256> {
    let fee = fee_from_amount(amount, fee_liquidation)?;
    amount
        .checked_sub(fee)
        .ok_or(ProtocolError::Fixed(FixedError::Underflow))
}

/// Liquidator bonus in RAY: `PERCENTAGE_FACTOR / discount - 1`.
/// `liquidationDiscount = PERCENTAGE_FACTOR - premium` (not a protocol constant).
#[inline]
pub fn bonus_ray(discount: U256) -> Result<Ray> {
    if discount.is_zero() || discount > PERCENTAGE_FACTOR {
        return Err(ProtocolError::Internal);
    }
    let premium = PERCENTAGE_FACTOR
        .checked_sub(discount)
        .ok_or(FixedError::Underflow)?;
    mul_div_down(premium, RAY, discount).map(Ray::from_raw)
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

/// Inverse of [`value_wad`] (floor): underlying amount from WAD value.
#[inline]
pub fn amount_from_wad(value: U256, price_ray: U256, decimals: u8) -> Result<U256> {
    if price_ray.is_zero() {
        return Err(ProtocolError::Fixed(FixedError::DivisionByZero));
    }
    mul_div_down(
        value,
        asset_unit(decimals)?
            .checked_mul(WAD_RAY_RATIO)
            .ok_or(FixedError::Overflow)?,
        price_ray,
    )
}

/// LT-weighted value, floor: `value * lt / PERCENTAGE_FACTOR`.
#[inline]
pub fn weighted_value(value: U256, lt: U256) -> Result<U256> {
    mul_div_down(value, lt, PERCENTAGE_FACTOR)
}

/// `CollateralLogic.calcOneTokenCollateral` weighted side: `min(value*lt/PF, quota_usd)`.
#[inline]
pub fn token_twv(value: U256, lt: U256, quota_usd: U256) -> Result<U256> {
    let w = weighted_value(value, lt)?;
    Ok(if w < quota_usd { w } else { quota_usd })
}

/// Convert WAD numeraire to underlying raw units (for `_hasBadDebt` totalValue).
#[inline]
pub fn wad_to_underlying(total_value_wad: U256, price_ray: U256, decimals: u8) -> Result<U256> {
    amount_from_wad(total_value_wad, price_ray, decimals)
}

/// Largest `amount` (underlying transferred by the liquidator) such that
/// seized ≤ `token_balance` and repaidAmount ≤ `total_debt`.
#[allow(clippy::too_many_arguments)]
pub fn max_partial_amount(
    token_balance: U256,
    total_debt: U256,
    price_underlying: U256,
    scale_underlying: U256,
    price_token: U256,
    scale_token: U256,
    discount: U256,
    fee_liquidation: U256,
) -> Result<U256> {
    if token_balance.is_zero() || total_debt.is_zero() {
        return Ok(U256::ZERO);
    }
    let mut lo = U256::ZERO;
    let mut hi = U256::MAX;
    let mut ans = U256::ZERO;
    for _ in 0..256 {
        if hi < lo {
            break;
        }
        let span = match hi.checked_sub(lo) {
            Some(s) => s,
            None => break,
        };
        let half = match span.checked_div(U256::from(2u8)) {
            Some(h) => h,
            None => break,
        };
        let mid = match lo.checked_add(half) {
            Some(m) => m,
            None => break,
        };
        let ok = match (
            seized_from_amount(
                mid,
                price_underlying,
                scale_underlying,
                price_token,
                scale_token,
                discount,
            ),
            repaid_from_amount(mid, fee_liquidation),
        ) {
            (Ok(seized), Ok(repaid)) => seized <= token_balance && repaid <= total_debt,
            _ => false,
        };
        if ok {
            ans = mid;
            lo = match mid.checked_add(U256::ONE) {
                Some(n) => n,
                None => break,
            };
        } else if mid.is_zero() {
            break;
        } else {
            hi = match mid.checked_sub(U256::ONE) {
                Some(n) => n,
                None => break,
            };
        }
    }
    Ok(ans)
}

/// `CreditLogic.calcLiquidationPayments` remainingFunds (no transfer-fee override).
/// `amountWithFee` / `amountMinusFee` are identity at this pin.
pub fn remaining_funds_bound(
    total_debt: U256,
    debt_with_interest: U256,
    total_value: U256,
    liquidation_discount: U256,
    fee_liquidation: U256,
) -> Result<(U256, U256, U256, U256)> {
    let mut amount_to_pool = total_debt;
    let fee_on_value = mul_div_down(total_value, fee_liquidation, PERCENTAGE_FACTOR)?;
    amount_to_pool = amount_to_pool
        .checked_add(fee_on_value)
        .ok_or(FixedError::Overflow)?;
    let total_funds = mul_div_down(total_value, liquidation_discount, PERCENTAGE_FACTOR)?;
    let mut remaining = U256::ZERO;
    let mut profit = U256::ZERO;
    let mut loss = U256::ZERO;
    if total_funds > amount_to_pool {
        remaining = total_funds
            .checked_sub(amount_to_pool)
            .ok_or(FixedError::Underflow)?;
    } else {
        amount_to_pool = total_funds;
    }
    if amount_to_pool >= debt_with_interest {
        profit = amount_to_pool
            .checked_sub(debt_with_interest)
            .ok_or(FixedError::Underflow)?;
    } else {
        loss = debt_with_interest
            .checked_sub(amount_to_pool)
            .ok_or(FixedError::Underflow)?;
    }
    let _ = WAD;
    Ok((amount_to_pool, remaining, profit, loss))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
mod pin_math {
    use super::*;
    use alloy_primitives::aliases::U40;
    use alloy_primitives::uint;

    /// Pin `_calcPartialLiquidationPayments` with identical RAY prices.
    /// amount=100e6 (6 dec), token 18 dec, discount=9500, fee=150.
    #[test]
    fn partial_payments_match_pin_floors() {
        let amount = uint!(100_000_000_U256);
        let p = RAY;
        let s_u = uint!(1_000_000_U256);
        let s_t = uint!(1_000_000_000_000_000_000_U256);
        let disc = uint!(9_500_U256);
        let fee = uint!(150_U256);
        let seized = seized_from_amount(amount, p, s_u, p, s_t, disc).unwrap();
        // convert = 100e18; seized = 100e18 * 10000 / 9500.
        assert_eq!(seized, uint!(105_263_157_894_736_842_105_U256));
        assert_eq!(fee_from_amount(amount, fee).unwrap(), uint!(1_500_000_U256));
        assert_eq!(
            repaid_from_amount(amount, fee).unwrap(),
            uint!(98_500_000_U256)
        );
        // Bonus is per-manager discount, not 1.05 / 1.08.
        let b = bonus_ray(disc).unwrap();
        assert_eq!(b.raw(), mul_div_down(uint!(500_U256), RAY, disc).unwrap());
    }

    #[test]
    fn unhealthy_is_strict_twv_lt_debt() {
        assert!(!is_unhealthy(U256::from(10u8), U256::from(10u8)));
        assert!(is_unhealthy(U256::from(9u8), U256::from(10u8)));
        let hf = hf_ray(U256::from(10u8), U256::from(10u8)).unwrap();
        assert_eq!(hf, Ray::ONE);
        let hf_u = hf_ray(U256::from(9u8), U256::from(10u8)).unwrap();
        assert!(hf_u < Ray::ONE);
    }

    #[test]
    fn expired_even_when_healthy() {
        assert!(is_expired(true, 1, 1));
        assert!(is_expired(true, 1, 2));
        assert!(!is_expired(true, 2, 1));
        assert!(!is_expired(false, 1, 2));
        assert!(!is_expired(true, 0, 1_000));
    }

    #[test]
    fn convert_is_one_floor_not_two() {
        let amount = uint!(3_U256);
        let pf = uint!(7_U256);
        let st = uint!(11_U256);
        let pt = uint!(5_U256);
        let sf = uint!(2_U256);
        let one = convert(amount, pf, sf, pt, st).unwrap();
        let two = mul_div_down(mul_div_down(amount, pf, pt).unwrap(), st, sf).unwrap();
        assert_ne!(one, two, "fixture must witness association");
        assert_eq!(one, uint!(23_U256));
        assert_eq!(two, uint!(22_U256));
    }

    #[test]
    fn static_lt_uint40_max_is_no_ramp() {
        let [limb] = *U40::MAX.as_limbs();
        assert_eq!(STATIC_LT_RAMP_START, limb);
        assert!(STATIC_LT_RAMP_START > u64::from(u32::MAX));
        let now_after_u32 = u64::from(u32::MAX).saturating_add(1);
        assert_eq!(
            get_liquidation_threshold(8_500, 7_000, STATIC_LT_RAMP_START, 0, 1_700_000_000)
                .unwrap(),
            8_500
        );
        assert_eq!(
            get_liquidation_threshold(8_500, 7_000, STATIC_LT_RAMP_START, 0, now_after_u32)
                .unwrap(),
            8_500
        );
        assert_eq!(
            get_liquidation_threshold(8_500, 7_000, STATIC_LT_RAMP_START, 0, STATIC_LT_RAMP_START)
                .unwrap(),
            8_500
        );
    }

    #[test]
    fn remaining_funds_identity_fee_override() {
        let (to_pool, rem, profit, loss) = remaining_funds_bound(
            uint!(100_U256),
            uint!(90_U256),
            uint!(200_U256),
            uint!(9_500_U256),
            uint!(150_U256),
        )
        .unwrap();
        // totalFunds = 200*9500/10000 = 190; amountToPool = 100 + 200*150/10000 = 103
        // remaining = 87; profit = 103-90 = 13
        assert_eq!(to_pool, uint!(103_U256));
        assert_eq!(rem, uint!(87_U256));
        assert_eq!(profit, uint!(13_U256));
        assert_eq!(loss, U256::ZERO);
    }
}
