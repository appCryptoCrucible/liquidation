//! Liquity V2 arithmetic from `Constants.sol` / `LiquityMath.sol` /
//! `LiquityBase._calcInterest` @ `c8a5a4ee`. Directions:
//! `docs/coverage/liquity-v2-rounding.md`.

use alloy_primitives::{uint, U256};
use liq_protocol::{ProtocolError, Result};
use liq_types::fixed::{mul_div, FixedError, Rounding, WAD, WAD_RAY_RATIO};
use liq_types::Ray;

/// `Constants.sol` `DECIMAL_PRECISION`.
pub const DECIMAL_PRECISION: U256 = WAD;
/// `Constants.sol` `ONE_YEAR = 365 days`.
pub const ONE_YEAR: U256 = uint!(31_536_000_U256);
/// `Constants.sol` `ETH_GAS_COMPENSATION = 0.0375 ether`.
pub const ETH_GAS_COMPENSATION: U256 = uint!(37_500_000_000_000_000_U256);
/// `Constants.sol` `COLL_GAS_COMPENSATION_DIVISOR`.
pub const COLL_GAS_COMPENSATION_DIVISOR: U256 = uint!(200_U256);
/// `Constants.sol` `COLL_GAS_COMPENSATION_CAP`.
pub const COLL_GAS_COMPENSATION_CAP: U256 = uint!(2_000_000_000_000_000_000_U256);
/// `Constants.sol` `MIN_BOLD_IN_SP`.
pub const MIN_BOLD_IN_SP: U256 = uint!(1_000_000_000_000_000_000_U256);
/// `Constants.sol` `MIN_DEBT = 2000e18`.
pub const MIN_DEBT: U256 = uint!(2_000_000_000_000_000_000_000_U256);
/// `Constants.sol` `MIN_LIQUIDATION_PENALTY_SP`.
pub const MIN_LIQUIDATION_PENALTY_SP: U256 = uint!(50_000_000_000_000_000_U256);
/// `Constants.sol` `MAX_LIQUIDATION_PENALTY_REDISTRIBUTION`.
pub const MAX_LIQUIDATION_PENALTY_REDISTRIBUTION: U256 = uint!(200_000_000_000_000_000_U256);
/// Pin deploy-script WETH `MCR` (`110 * _1pct`) — config asserts against the
/// branch `AddressesRegistry`, not this constant, at runtime.
pub const PIN_MCR_WETH: U256 = uint!(1_100_000_000_000_000_000_U256);
/// Pin deploy-script SETH `MCR` (`120 * _1pct`).
pub const PIN_MCR_SETH: U256 = uint!(1_200_000_000_000_000_000_U256);
/// Pin deploy-script WETH `CCR` (`150 * _1pct`).
pub const PIN_CCR_WETH: U256 = uint!(1_500_000_000_000_000_000_U256);
/// Pin deploy-script SETH `CCR` (`160 * _1pct`).
pub const PIN_CCR_SETH: U256 = uint!(1_600_000_000_000_000_000_U256);
/// Pin deploy-script `LIQUIDATION_PENALTY_SP` (`5 * _1pct`).
pub const PIN_PENALTY_SP: U256 = uint!(50_000_000_000_000_000_U256);
/// Pin deploy-script WETH `LIQUIDATION_PENALTY_REDISTRIBUTION` (`10 * _1pct`).
pub const PIN_PENALTY_REDIST_WETH: U256 = uint!(100_000_000_000_000_000_U256);
/// Pin deploy-script SETH `LIQUIDATION_PENALTY_REDISTRIBUTION` (`20 * _1pct`).
pub const PIN_PENALTY_REDIST_SETH: U256 = uint!(200_000_000_000_000_000_U256);

#[inline]
pub fn mul_div_down(a: U256, b: U256, d: U256) -> Result<U256> {
    Ok(mul_div(a, b, d, Rounding::Down)?)
}

#[inline]
pub fn mul_div_up(a: U256, b: U256, d: U256) -> Result<U256> {
    Ok(mul_div(a, b, d, Rounding::Up)?)
}

/// `LiquityBase._calcInterest`: `weighted * period / ONE_YEAR / DECIMAL_PRECISION`.
#[inline]
pub fn calc_interest(weighted: U256, period: U256) -> Result<U256> {
    if weighted.is_zero() || period.is_zero() {
        return Ok(U256::ZERO);
    }
    let inner = mul_div_down(weighted, period, ONE_YEAR)?;
    mul_div_down(inner, U256::ONE, DECIMAL_PRECISION)
}

/// `LiquityMath._computeCR`: `coll * price / debt`, or `type(uint256).max` at zero debt.
#[inline]
pub fn compute_cr(coll: U256, debt: U256, price_wad: U256) -> Result<U256> {
    if debt.is_zero() {
        return Ok(U256::MAX);
    }
    mul_div_down(coll, price_wad, debt)
}

/// `TroveManager._getInterestPeriod`.
#[inline]
pub fn interest_period(last_update: u64, shutdown_time: u64, now: u64) -> Result<U256> {
    if shutdown_time == 0 {
        let dt = now
            .checked_sub(last_update)
            .ok_or(ProtocolError::TimestampBeforeUpdate)?;
        return Ok(U256::from(dt));
    }
    if last_update < shutdown_time {
        let dt = shutdown_time
            .checked_sub(last_update)
            .ok_or(ProtocolError::TimestampBeforeUpdate)?;
        return Ok(U256::from(dt));
    }
    Ok(U256::ZERO)
}

/// Redistribution gain: `stake * (L - snapshot) / DECIMAL_PRECISION` (floor).
#[inline]
pub fn redist_gain(stake: U256, l_acc: U256, snapshot: U256) -> Result<U256> {
    let delta = l_acc.checked_sub(snapshot).ok_or(FixedError::Underflow)?;
    if stake.is_zero() || delta.is_zero() {
        return Ok(U256::ZERO);
    }
    mul_div_down(stake, delta, DECIMAL_PRECISION)
}

/// `TroveManager._getCollGasCompensation`: `min(_coll / 200, 2 ether)`.
#[inline]
pub fn coll_gas_compensation(coll_sp_portion: U256) -> Result<U256> {
    let raw = coll_sp_portion
        .checked_div(COLL_GAS_COMPENSATION_DIVISOR)
        .ok_or(FixedError::DivisionByZero)?;
    Ok(core::cmp::min(raw, COLL_GAS_COMPENSATION_CAP))
}

/// Offsettable BOLD in the Stability Pool: `total - min(MIN_BOLD_IN_SP, total)`.
#[inline]
pub fn bold_in_sp_for_offsets(total_bold_deposits: U256) -> Result<U256> {
    let leave = core::cmp::min(MIN_BOLD_IN_SP, total_bold_deposits);
    total_bold_deposits
        .checked_sub(leave)
        .ok_or(ProtocolError::Fixed(FixedError::Underflow))
}

/// Collateral portion offset against the SP, then its gas-comp slice.
///
/// `collSPPortion = entireColl * debtToOffset / entireDebt` (floor);
/// `debtToOffset = min(entireDebt, boldInSPForOffsets)`.
#[inline]
pub fn coll_gas_from_offset(
    entire_coll: U256,
    entire_debt: U256,
    total_bold_deposits: U256,
) -> Result<U256> {
    if entire_debt.is_zero() {
        return Ok(U256::ZERO);
    }
    let available = bold_in_sp_for_offsets(total_bold_deposits)?;
    let debt_to_offset = core::cmp::min(entire_debt, available);
    if debt_to_offset.is_zero() {
        return Ok(U256::ZERO);
    }
    let coll_sp = mul_div_down(entire_coll, debt_to_offset, entire_debt)?;
    coll_gas_compensation(coll_sp)
}

#[inline]
pub fn price_wad_from_ray(price_ray: U256) -> Result<U256> {
    mul_div_down(price_ray, U256::ONE, WAD_RAY_RATIO)
}

#[inline]
pub fn hf_from_icr(icr_wad: U256, mcr_wad: U256) -> Result<Ray> {
    if icr_wad == U256::MAX {
        return Ok(Ray::from_raw(U256::MAX));
    }
    if mcr_wad.is_zero() {
        return Err(ProtocolError::Fixed(FixedError::DivisionByZero));
    }
    mul_div_down(icr_wad, liq_types::fixed::RAY, mcr_wad).map(Ray::from_raw)
}

#[inline]
pub fn asset_unit(decimals: u8) -> Result<U256> {
    U256::from(10u8)
        .checked_pow(U256::from(decimals))
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

#[inline]
pub fn wad_to_ray_price(price_wad: U256) -> Result<U256> {
    price_wad
        .checked_mul(WAD_RAY_RATIO)
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}
