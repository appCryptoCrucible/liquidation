//! Aave V4 arithmetic, one function per contract step, each named after the
//! Solidity it reproduces and rounded the way that line rounds. The
//! direction of every step is tabulated in `docs/coverage/aave-v4-rounding.md`;
//! this module is that table as code. Nothing here allocates.
//!
//! Chain widths: shares and amounts are `uint120`, indices `uint120` RAY,
//! rates `uint96`, offsets `int200`, deficits `uint200`. Everything is
//! widened to `U256` (and `U512` inside `mul_div`) at the arithmetic
//! boundary, exactly as Solidity widens to `uint256`.

use alloy_primitives::{uint, I256, U256};
use liq_protocol::{ProtocolError, Result};
use liq_types::fixed::{mul_div, FixedError, Rounding, RAY, WAD};
use liq_types::Ray;

use crate::layout::HubAsset;

/// `MathUtils.SECONDS_PER_YEAR = 365 days`.
pub const SECONDS_PER_YEAR: U256 = uint!(31_536_000_U256);
/// `SharesMath.VIRTUAL_ASSETS == VIRTUAL_SHARES == 1e6`.
pub const VIRTUAL: U256 = uint!(1_000_000_U256);
/// `PercentageMath.PERCENTAGE_FACTOR`.
pub const BPS: U256 = uint!(10_000_U256);
/// `WAD / PERCENTAGE_FACTOR`: `WadRayMath.bpsToWad` factor.
pub const BPS_TO_WAD: U256 = uint!(100_000_000_000_000_U256);
/// One basis point in RAY: the bonus curve's quantum.
pub const BPS_RAY: U256 = uint!(100_000_000_000_000_000_000_000_U256);
/// `10^(27 - 8)`: the oracle's 8-decimal price as a [`Ray`].
pub const P8_TO_RAY: U256 = uint!(10_000_000_000_000_000_000_U256);
/// `Value` (1e26 = 1 USD, `SpokeUtils.toValue`) to WAD numeraire: `/ 1e8`.
pub const VALUE_TO_WAD: U256 = uint!(100_000_000_U256);
/// `LiquidationLogic.DUST_LIQUIDATION_THRESHOLD = 1000e26` (Value).
pub const DUST_VALUE: U256 = uint!(100_000_000_000_000_000_000_000_000_000_U256);
/// `Spoke.HEALTH_FACTOR_LIQUIDATION_THRESHOLD = 1e18`.
pub const HF_THRESHOLD_WAD: U256 = WAD;
/// `WadRayMath.WAD_DECIMALS`.
pub const WAD_DECIMALS: u8 = 18;

/// The two `u128` halves of a `U256` (little-endian: `lo` first).
#[inline]
#[must_use]
pub fn split(v: U256) -> (u128, u128) {
    let l = v.as_limbs();
    (
        u128::from(l[0]) | (u128::from(l[1]) << 64),
        u128::from(l[2]) | (u128::from(l[3]) << 64),
    )
}

/// Inverse of [`split`].
#[inline]
#[must_use]
pub fn join(lo: u128, hi: u128) -> U256 {
    // Masked before the narrowing cast, so no bit is lost.
    let low = |x: u128| (x & u128::from(u64::MAX)) as u64;
    let high = |x: u128| (x >> 64) as u64;
    U256::from_limbs([low(lo), high(lo), low(hi), high(hi)])
}

/// Stored `int200` halves as an `I256`.
#[inline]
#[must_use]
pub fn signed(lo: u128, hi: u128) -> I256 {
    I256::from_raw(join(lo, hi))
}

/// `ceil(x / RAY)` — `WadRayMath.fromRayUp`.
#[inline]
pub fn from_ray_up(x: U256) -> Result<U256> {
    Ok(mul_div(x, U256::ONE, RAY, Rounding::Up)?)
}

/// `ceil(x / RAY) * RAY` — `WadRayMath.roundRayUp`. Overflow when the
/// re-scaled value does not fit (the chain reverts there too).
#[inline]
pub fn round_ray_up(x: U256) -> Result<U256> {
    from_ray_up(x)?
        .checked_mul(RAY)
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// `a + b` for `uint256 + int256` — `MathUtils.add`: underflow is the
/// chain's revert, here `Fixed(Underflow)`.
#[inline]
pub fn add_signed(a: U256, b: I256) -> Result<U256> {
    let (neg, mag) = b.into_sign_and_abs();
    let r = if neg.is_negative() {
        a.checked_sub(mag).ok_or(FixedError::Underflow)
    } else {
        a.checked_add(mag).ok_or(FixedError::Overflow)
    };
    Ok(r?)
}

/// `10^(18 - decimals)` — the `uncheckedExp` in `SpokeUtils.toValue`.
/// `decimals > 18` is rejected by `Spoke.addReserve`; here it is
/// `Fixed(Overflow)` (the scale does not exist).
#[inline]
pub fn value_scale(decimals: u8) -> Result<U256> {
    let exp = WAD_DECIMALS
        .checked_sub(decimals)
        .ok_or(FixedError::Overflow)?;
    U256::from(10u8)
        .checked_pow(U256::from(exp))
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// `10^decimals` — the asset unit.
#[inline]
pub fn asset_unit(decimals: u8) -> Result<U256> {
    U256::from(10u8)
        .checked_pow(U256::from(decimals))
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// `SpokeUtils.toValue`: `amount * price * 10^(18 - decimals)`, exact
/// (checked multiplication; the chain reverts on overflow).
#[inline]
pub fn to_value(amount: U256, decimals: u8, p8: U256) -> Result<U256> {
    amount
        .checked_mul(p8)
        .and_then(|v| v.checked_mul(value_scale(decimals).ok()?))
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// The oracle's `uint256` price from a registry [`Ray`]: the 8-decimal
/// aggregator answer the Spoke reads (`AaveOracle.getReservePrice`,
/// `ORACLE_DECIMALS = 8`). `liq-oracle` scales answers by `10^19`, so this is
/// exact for every real price; a probe price off the grid floors to the
/// answer the chain would see. Zero is the chain's `InvalidPrice` revert.
#[inline]
pub fn p8_of(price: Ray) -> Option<U256> {
    let p = price.raw().wrapping_div(P8_TO_RAY);
    (!p.is_zero()).then_some(p)
}

/// `MathUtils.calculateLinearInterest`: `RAY + floor(rate * dt / YEAR)`.
#[inline]
pub fn linear_interest(rate: U256, dt: u64) -> Result<U256> {
    let growth = mul_div(rate, U256::from(dt), SECONDS_PER_YEAR, Rounding::Down)?;
    RAY.checked_add(growth)
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// `AssetLogic.getDrawnIndex` at chain time `ts`: the stored index when no
/// time has passed or nothing is drawn; else `rayMulUp(index,
/// linearInterest)`. `ts < last_update` is the chain's revert in
/// `calculateLinearInterest` → `TimestampBeforeUpdate`.
#[inline]
pub fn drawn_index_at(a: &HubAsset, last_update: u32, ts: u64) -> Result<U256> {
    let stored = U256::from(a.drawn_index);
    let last = u64::from(last_update);
    if ts < last {
        return Err(ProtocolError::TimestampBeforeUpdate);
    }
    if ts == last || (a.drawn_shares == 0 && a.premium_shares == 0) {
        return Ok(stored);
    }
    let li = linear_interest(U256::from(a.drawn_rate), ts.wrapping_sub(last))?;
    Ok(mul_div(stored, li, RAY, Rounding::Up)?)
}

/// `Premium.calculatePremiumRay`: `premiumShares * drawnIndex -
/// premiumOffsetRay`, which the chain casts to `uint256` (reverting when
/// negative → `Fixed(Underflow)`).
#[inline]
pub fn premium_ray(shares: u128, offset: I256, idx: U256) -> Result<U256> {
    let prod = U256::from(shares)
        .checked_mul(idx)
        .ok_or(FixedError::Overflow)?;
    let prod = I256::try_from(prod).map_err(|_| FixedError::Overflow)?;
    let p = prod.checked_sub(offset).ok_or(FixedError::Overflow)?;
    if p.is_negative() {
        return Err(ProtocolError::Fixed(FixedError::Underflow));
    }
    Ok(p.into_raw())
}

/// `AssetLogic._calculateAggregatedOwedRay`: `drawnShares * idx +
/// premiumRay + deficitRay`, exact.
#[inline]
pub fn aggregated_owed_ray(a: &HubAsset, idx: U256) -> Result<U256> {
    let drawn = U256::from(a.drawn_shares)
        .checked_mul(idx)
        .ok_or(FixedError::Overflow)?;
    let premium = premium_ray(
        a.premium_shares,
        signed(a.premium_offset_lo, a.premium_offset_hi),
        idx,
    )?;
    drawn
        .checked_add(premium)
        .and_then(|v| v.checked_add(join(a.deficit_ray_lo, a.deficit_ray_hi)))
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// `AssetLogic.getUnrealizedFees` at index `idx`: zero when the index has
/// not moved or the fee is zero; else `percentMulDown(fromRayUp(owed(idx))
/// - fromRayUp(owed(stored)), liquidityFee)`.
#[inline]
pub fn unrealized_fees(a: &HubAsset, idx: U256) -> Result<U256> {
    if idx == U256::from(a.drawn_index) || a.liquidity_fee == 0 {
        return Ok(U256::ZERO);
    }
    let after = from_ray_up(aggregated_owed_ray(a, idx)?)?;
    let before = from_ray_up(aggregated_owed_ray(a, U256::from(a.drawn_index))?)?;
    let grown = after.checked_sub(before).ok_or(FixedError::Underflow)?;
    Ok(mul_div(
        grown,
        U256::from(a.liquidity_fee),
        BPS,
        Rounding::Down,
    )?)
}

/// `AssetLogic.totalAddedAssets` at index `idx`: `liquidity + swept +
/// fromRayUp(owed) - realizedFees - unrealizedFees`, exact.
#[inline]
pub fn total_added_assets(a: &HubAsset, idx: U256) -> Result<U256> {
    let owed = from_ray_up(aggregated_owed_ray(a, idx)?)?;
    U256::from(a.liquidity)
        .checked_add(U256::from(a.swept))
        .and_then(|v| v.checked_add(owed))
        .ok_or(FixedError::Overflow)?
        .checked_sub(U256::from(a.realized_fees))
        .and_then(|v| v.checked_sub(unrealized_fees(a, idx).ok()?))
        .ok_or(ProtocolError::Fixed(FixedError::Underflow))
}

/// `SharesMath.toAssetsDown`: `floor(shares * (T + 1e6) / (S + 1e6))` —
/// `Hub.previewRemoveByShares`.
#[inline]
pub fn to_assets_down(shares: U256, total_assets: U256, total_shares: U256) -> Result<U256> {
    Ok(mul_div(
        shares,
        total_assets
            .checked_add(VIRTUAL)
            .ok_or(FixedError::Overflow)?,
        total_shares
            .checked_add(VIRTUAL)
            .ok_or(FixedError::Overflow)?,
        Rounding::Down,
    )?)
}

/// `SharesMath.toAssetsUp` — `Hub.previewAddByShares`.
#[inline]
pub fn to_assets_up(shares: U256, total_assets: U256, total_shares: U256) -> Result<U256> {
    Ok(mul_div(
        shares,
        total_assets
            .checked_add(VIRTUAL)
            .ok_or(FixedError::Overflow)?,
        total_shares
            .checked_add(VIRTUAL)
            .ok_or(FixedError::Overflow)?,
        Rounding::Up,
    )?)
}

/// `SharesMath.toSharesDown`: `floor(assets * (S + 1e6) / (T + 1e6))` —
/// `Hub.previewAddByAssets`.
#[inline]
pub fn to_shares_down(assets: U256, total_assets: U256, total_shares: U256) -> Result<U256> {
    Ok(mul_div(
        assets,
        total_shares
            .checked_add(VIRTUAL)
            .ok_or(FixedError::Overflow)?,
        total_assets
            .checked_add(VIRTUAL)
            .ok_or(FixedError::Overflow)?,
        Rounding::Down,
    )?)
}

/// `WadRayMath.rayMulUp`: `ceil(a * b / RAY)`.
#[inline]
pub fn ray_mul_up(a: U256, b: U256) -> Result<U256> {
    Ok(mul_div(a, b, RAY, Rounding::Up)?)
}

/// `MathUtils.divUp`: `ceil(a / b)`.
#[inline]
pub fn div_up(a: U256, b: U256) -> Result<U256> {
    Ok(mul_div(a, U256::ONE, b, Rounding::Up)?)
}

/// `PercentageMath.percentMulUp`: `ceil(v * p / 1e4)`.
#[inline]
pub fn percent_mul_up(v: U256, p: U256) -> Result<U256> {
    Ok(mul_div(v, p, BPS, Rounding::Up)?)
}

/// `PercentageMath.percentMulDown`: `floor(v * p / 1e4)`.
#[inline]
pub fn percent_mul_down(v: U256, p: U256) -> Result<U256> {
    Ok(mul_div(v, p, BPS, Rounding::Down)?)
}

/// Chain health factor (WAD, `1e18` = boundary) as the normalised [`Ray`]:
/// `hf_wad * 1e9`, lossless. `type(uint256).max` (no debt) maps to
/// `Health::NO_DEBT_HF`, which is the same sentinel.
#[inline]
pub fn hf_wad_to_ray(hf_wad: U256) -> Result<Ray> {
    if hf_wad == U256::MAX {
        return Ok(Ray::from_raw(U256::MAX));
    }
    hf_wad
        .checked_mul(liq_types::fixed::WAD_RAY_RATIO)
        .map(Ray::from_raw)
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// Numeraire value (`RAY`-scaled, as the conformance harness computes it:
/// `amount * price_ray / 10^decimals`, floored) of a raw amount. Used only
/// for the preference ordering of quote options (`Quote` docs, check 8).
#[inline]
pub fn value_ray_of(amount: U256, price: Ray, decimals: u8) -> Result<U256> {
    Ok(mul_div(
        amount,
        price.raw(),
        asset_unit(decimals)?,
        Rounding::Down,
    )?)
}
