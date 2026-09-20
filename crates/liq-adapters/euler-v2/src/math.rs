//! Euler V2 arithmetic — `Liquidation.sol` / `Cache.sol` / `Owed.sol` /
//! `Constants.sol` / `RPow.sol` @ `bfb325a6`. Directions:
//! `docs/coverage/euler-v2-rounding.md`.

use alloy_primitives::{uint, Address, U256};
use liq_protocol::{ProtocolError, Result};
use liq_types::fixed::{mul_div, FixedError, Rounding, RAY, WAD, WAD_RAY_RATIO};
use liq_types::Ray;

use crate::layout::VaultRow;

pub const CONFIG_SCALE: u16 = 10_000;
pub const INTERNAL_DEBT_PRECISION_SHIFT: usize = 31;
pub const INITIAL_INTEREST_ACCUMULATOR: U256 = RAY;
pub const DEFAULT_INTEREST_FEE: u16 = 1_000;
pub const HF_THRESHOLD_WAD: U256 = WAD;
/// `1 << INTERNAL_DEBT_PRECISION_SHIFT`.
pub const DEBT_SCALE: U256 = uint!(2_147_483_648_U256);

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
pub fn u256_from_limbs(lo: u128, hi: u128) -> U256 {
    U256::from_limbs([
        lo as u64,
        lo.wrapping_shr(64) as u64,
        hi as u64,
        hi.wrapping_shr(64) as u64,
    ])
}

#[inline]
pub fn u256_to_limbs(v: U256) -> Result<(u128, u128)> {
    let [l0, l1, l2, l3] = *v.as_limbs();
    let lo = u128::from(l1).wrapping_shl(64) | u128::from(l0);
    let hi = u128::from(l3).wrapping_shl(64) | u128::from(l2);
    Ok((lo, hi))
}

#[inline]
pub fn vault_acc(row: &VaultRow) -> U256 {
    u256_from_limbs(row.interest_accumulator_lo, row.interest_accumulator_hi)
}

#[inline]
pub fn set_vault_acc(row: &mut VaultRow, acc: U256) -> Result<()> {
    let (lo, hi) = u256_to_limbs(acc)?;
    row.interest_accumulator_lo = lo;
    row.interest_accumulator_hi = hi;
    Ok(())
}

/// `OwedLib.toAssetsUpUint`: ceil(owed / 2^31).
#[inline]
pub fn to_assets_up(owed_exact: U256) -> Result<U256> {
    if owed_exact.is_zero() {
        return Ok(U256::ZERO);
    }
    let add = DEBT_SCALE
        .checked_sub(U256::ONE)
        .ok_or(FixedError::Underflow)?;
    let n = owed_exact.checked_add(add).ok_or(FixedError::Overflow)?;
    Ok(n.wrapping_shr(INTERNAL_DEBT_PRECISION_SHIFT))
}

#[inline]
pub fn assets_to_owed(assets: U256) -> Result<U256> {
    assets
        .checked_shl(INTERNAL_DEBT_PRECISION_SHIFT)
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

/// `owed.mulDiv(vaultAcc, userAcc)` — floor.
#[inline]
pub fn current_owed(owed: U256, vault_acc: U256, user_acc: U256) -> Result<U256> {
    if owed.is_zero() {
        return Ok(U256::ZERO);
    }
    if user_acc.is_zero() {
        return Err(ProtocolError::Internal);
    }
    mul_div_down(owed, vault_acc, user_acc)
}

/// `RPow.rpow` — EVM wrapping mul, overflow flag, half-up via `scalar >> 1`.
#[inline]
pub fn rpow(mut x: U256, mut n: U256, scalar: U256) -> Result<(U256, bool)> {
    if scalar.is_zero() {
        return Err(ProtocolError::Fixed(FixedError::DivisionByZero));
    }
    if x.is_zero() {
        return Ok((if n.is_zero() { scalar } else { U256::ZERO }, false));
    }
    let mut z = if (n & U256::ONE) == U256::ONE {
        x
    } else {
        scalar
    };
    let half = scalar.wrapping_shr(1);
    n = n.wrapping_shr(1);
    while !n.is_zero() {
        if x.wrapping_shr(128) != U256::ZERO {
            return Ok((z, true));
        }
        let xx = x.wrapping_mul(x);
        let (xx_round, carry) = xx.overflowing_add(half);
        if carry {
            return Ok((z, true));
        }
        x = xx_round
            .checked_div(scalar)
            .ok_or(FixedError::DivisionByZero)?;
        if (n & U256::ONE) == U256::ONE {
            let zx = z.wrapping_mul(x);
            if !x.is_zero() {
                let q = zx.checked_div(x).ok_or(FixedError::DivisionByZero)?;
                if q != z {
                    return Ok((z, true));
                }
            }
            let (zx_round, carry) = zx.overflowing_add(half);
            if carry {
                return Ok((z, true));
            }
            z = zx_round
                .checked_div(scalar)
                .ok_or(FixedError::DivisionByZero)?;
        }
        n = n.wrapping_shr(1);
    }
    Ok((z, false))
}

/// `Cache.initVaultCache` interest update. Returns (newAcc, newTotalBorrowsExact).
#[inline]
pub fn accrue_accumulator(
    acc: U256,
    total_borrows_owed: U256,
    rate: U256,
    delta_t: U256,
) -> Result<(U256, U256)> {
    if delta_t.is_zero() {
        return Ok((acc, total_borrows_owed));
    }
    let base = rate.checked_add(RAY).ok_or(FixedError::Overflow)?;
    let (multiplier, overflow) = rpow(base, delta_t, RAY)?;
    let mut new_acc = acc;
    if !overflow {
        let intermediate = new_acc.wrapping_mul(multiplier);
        if !multiplier.is_zero() {
            if let Some(q) = intermediate.checked_div(multiplier) {
                if new_acc == q {
                    if let Some(a) = intermediate.checked_div(RAY) {
                        new_acc = a;
                    }
                }
            }
        }
    }
    let mut new_borrows = total_borrows_owed;
    let intermediate = new_borrows.wrapping_mul(new_acc);
    if !new_acc.is_zero() && !acc.is_zero() {
        if let Some(q) = intermediate.checked_div(new_acc) {
            if new_borrows == q {
                if let Some(b) = intermediate.checked_div(acc) {
                    new_borrows = b;
                }
            }
        }
    }
    Ok((new_acc, new_borrows))
}

/// `minDiscountFactor = 1e18 − 1e18 × maxLiquidationDiscount / CONFIG_SCALE`.
#[inline]
pub fn min_discount_factor(max_liquidation_discount: u16) -> Result<U256> {
    if max_liquidation_discount == CONFIG_SCALE {
        return Err(ProtocolError::Internal);
    }
    let cut = mul_div_down(
        WAD,
        U256::from(max_liquidation_discount),
        U256::from(CONFIG_SCALE),
    )?;
    WAD.checked_sub(cut)
        .ok_or(ProtocolError::Fixed(FixedError::Underflow))
}

/// `discountFactor = collAdj * 1e18 / liability`, floored at `minDiscountFactor`.
#[inline]
pub fn discount_factor(coll_adj: U256, liability: U256, min_df: U256) -> Result<U256> {
    if liability.is_zero() {
        return Err(ProtocolError::Internal);
    }
    let df = mul_div_down(coll_adj, WAD, liability)?;
    Ok(if df < min_df { min_df } else { df })
}

/// Liquidation bonus RAY: `1/discountFactor − 1`.
#[inline]
pub fn bonus_ray(discount_factor_wad: U256) -> Result<Ray> {
    if discount_factor_wad.is_zero() {
        return Err(ProtocolError::Fixed(FixedError::DivisionByZero));
    }
    let b = mul_div_down(
        RAY,
        WAD.checked_sub(discount_factor_wad)
            .ok_or(FixedError::Underflow)?,
        discount_factor_wad,
    )?;
    Ok(Ray::from_raw(b))
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

/// Amount · RAY-price / 10^decimals → WAD numeraire, floor (`getQuote` mid).
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

/// `getCollateralValue`: quote then `* ltv / CONFIG_SCALE` floor.
#[inline]
pub fn collateral_adj_value(shares: U256, price_ray: U256, decimals: u8, ltv: u16) -> Result<U256> {
    if ltv == 0 {
        return Ok(U256::ZERO);
    }
    let quoted = value_wad(shares, price_ray, decimals)?;
    mul_div_down(quoted, U256::from(ltv), U256::from(CONFIG_SCALE))
}

/// LTV ramp (`LTVConfigLib.getLTV` liquidation=true) at `ts`.
#[inline]
pub fn current_liquidation_ltv(
    liquidation_ltv: u16,
    initial_liquidation_ltv: u16,
    target_timestamp: u64,
    ramp_duration: u32,
    ts: u64,
) -> u16 {
    if ts >= target_timestamp || liquidation_ltv >= initial_liquidation_ltv {
        return liquidation_ltv;
    }
    if ramp_duration == 0 {
        return liquidation_ltv;
    }
    let time_remaining = match target_timestamp.checked_sub(ts) {
        Some(t) => t,
        None => return liquidation_ltv,
    };
    let span = match initial_liquidation_ltv.checked_sub(liquidation_ltv) {
        Some(s) => u64::from(s),
        None => return liquidation_ltv,
    };
    let add = match span.checked_mul(time_remaining) {
        Some(p) => match p.checked_div(u64::from(ramp_duration)) {
            Some(a) => a,
            None => return liquidation_ltv,
        },
        None => return liquidation_ltv,
    };
    match u16::try_from(add) {
        Ok(a) => liquidation_ltv.saturating_add(a),
        Err(_) => liquidation_ltv,
    }
}

/// `calculateMaxLiquidation` repay (assets) and yield (collateral shares).
#[inline]
pub fn max_liquidation(
    liability_assets: U256,
    liability_value: U256,
    coll_adj: U256,
    collateral_balance: U256,
    collateral_value: U256,
    min_df: U256,
) -> Result<(U256, U256)> {
    if coll_adj > liability_value || liability_value.is_zero() {
        return Ok((U256::ZERO, U256::ZERO));
    }
    let df = discount_factor(coll_adj, liability_value, min_df)?;
    if collateral_value.is_zero() {
        return Ok((U256::ZERO, collateral_balance));
    }
    let mut max_repay_value = liability_value;
    let mut max_yield_value = mul_div_down(max_repay_value, WAD, df)?;
    if collateral_value < max_yield_value {
        max_repay_value = mul_div_down(collateral_value, df, WAD)?;
        max_yield_value = collateral_value;
    }
    let repay = mul_div_down(max_repay_value, liability_assets, liability_value)?;
    let yield_bal = mul_div_down(max_yield_value, collateral_balance, collateral_value)?;
    Ok((repay, yield_bal))
}

#[inline]
pub fn last_update(ts: u64) -> Result<u32> {
    u32::try_from(ts).map_err(|_| ProtocolError::Fixed(FixedError::Overflow))
}
