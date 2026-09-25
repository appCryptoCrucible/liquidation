//! Fluid vault arithmetic — `TickMath.sol`, T1 `liquidate` @ `9496626f`.
//! Directions: `docs/coverage/fluid-rounding.md`.

use alloy_primitives::{uint, Address, U256};
use liq_protocol::{ProtocolError, Result};
use liq_types::fixed::{mul_div, FixedError, Rounding, RAY, WAD};
use liq_types::Ray;

/// Pin `NATIVE_TOKEN`. Not WETH. Unmapped native vaults are UNPRICED.
pub const NATIVE_TOKEN: Address =
    alloy_primitives::address!("0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");
/// Pin dead address for `FluidLiquidateResult` try/catch quote.
pub const DEAD_ADDRESS: Address =
    alloy_primitives::address!("0x000000000000000000000000000000000000dEaD");

pub const MIN_TICK: i32 = -32_767;
pub const MAX_TICK: i32 = 32_767;
/// `1 << 96`.
pub const ZERO_TICK_SCALED_RATIO: U256 = uint!(79228162514264337593543950336_U256);
pub const MIN_RATIOX96: U256 = uint!(37075072_U256);
pub const MAX_RATIOX96: U256 = uint!(169307877264527972847801929085841449095838922544595_U256);
/// Pin `EXCHANGE_PRICES_PRECISION`.
pub const EXCHANGE_PRICES_PRECISION: U256 = uint!(1_000_000_000_000_U256);
/// Pin `debtAmt_ < 10000` reverts.
pub const MIN_LIQUIDATION_AMT: U256 = uint!(10_000_U256);
/// Pin `1e54` = RAY² (oracle invert).
#[inline]
pub fn one_e54() -> Result<U256> {
    RAY.checked_mul(RAY)
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}
/// Pin raw oracle cap `1e45`.
pub const ORACLE_RAW_CAP: U256 =
    uint!(1_000_000_000_000_000_000_000_000_000_000_000_000_000_000_000_U256);
/// Threshold packed 3 decimals (`900` = 90%).
pub const THRESHOLD_SCALE: U256 = uint!(1_000_U256);
/// Penalty applied as `(10000 + penalty) / 10000`.
pub const PENALTY_SCALE: U256 = uint!(10_000_U256);
/// Admin event 1e2 (`100` = 1%) → packed 3-decimal (`/ 10`).
pub const ADMIN_BPS_TO_PACKED: u16 = 10;
pub const X10: u16 = 0x3ff;
pub const X19: u32 = 0x7_ffff;
pub const TICK_STATUS_PERFECT: u8 = 1;
pub const TICK_STATUS_LIQUIDATED: u8 = 2;

const FACTOR00: U256 = uint!(0x100000000000000000000000000000000_U256);
const FACTOR01: U256 = uint!(0xff9dd7de423466c20352b1246ce4856f_U256);
const FACTOR02: U256 = uint!(0xff3bd55f4488ad277531fa1c725a66d0_U256);
const FACTOR03: U256 = uint!(0xfe78410fd6498b73cb96a6917f853259_U256);
const FACTOR04: U256 = uint!(0xfcf2d9987c9be178ad5bfeffaa123273_U256);
const FACTOR05: U256 = uint!(0xf9ef02c4529258b057769680fc6601b3_U256);
const FACTOR06: U256 = uint!(0xf402d288133a85a17784a411f7aba082_U256);
const FACTOR07: U256 = uint!(0xe895615b5beb6386553757b0352bda90_U256);
const FACTOR08: U256 = uint!(0xd34f17a00ffa00a8309940a15930391a_U256);
const FACTOR09: U256 = uint!(0xae6b7961714e20548d88ea5123f9a0ff_U256);
const FACTOR10: U256 = uint!(0x76d6461f27082d74e0feed3b388c0ca1_U256);
const FACTOR11: U256 = uint!(0x372a3bfe0745d8b6b19d985d9a8b85bb_U256);
const FACTOR12: U256 = uint!(0x0be32cbee48979763cf7247dd7bb539d_U256);
const FACTOR13: U256 = uint!(0x8d4f70c9ff4924dac37612d1e2921e_U256);
const FACTOR14: U256 = uint!(0x4e009ae5519380809a02ca7aec77_U256);
const FACTOR15: U256 = uint!(0x17c45e641b6e95dee056ff10_U256);
const U256_MAX: U256 = U256::MAX;
const MASK_32: U256 = uint!(0x100000000_U256);
const E26: U256 = uint!(100_000_000_000_000_000_000_000_000_U256);

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

#[inline]
fn mul_shr128(a: U256, b: U256) -> U256 {
    a.wrapping_mul(b).wrapping_shr(128)
}

/// Pin `TickMath.getRatioAtTick`. `ratioX96 = (1.0015^tick) * 2^96`.
pub fn get_ratio_at_tick(tick: i32) -> Result<U256> {
    let abs = tick.unsigned_abs();
    if abs > MAX_TICK.unsigned_abs() {
        return Err(ProtocolError::Internal);
    }
    let mut factor = FACTOR00;
    if abs & 0x1 != 0 {
        factor = FACTOR01;
    }
    if abs & 0x2 != 0 {
        factor = mul_shr128(factor, FACTOR02);
    }
    if abs & 0x4 != 0 {
        factor = mul_shr128(factor, FACTOR03);
    }
    if abs & 0x8 != 0 {
        factor = mul_shr128(factor, FACTOR04);
    }
    if abs & 0x10 != 0 {
        factor = mul_shr128(factor, FACTOR05);
    }
    if abs & 0x20 != 0 {
        factor = mul_shr128(factor, FACTOR06);
    }
    if abs & 0x40 != 0 {
        factor = mul_shr128(factor, FACTOR07);
    }
    if abs & 0x80 != 0 {
        factor = mul_shr128(factor, FACTOR08);
    }
    if abs & 0x100 != 0 {
        factor = mul_shr128(factor, FACTOR09);
    }
    if abs & 0x200 != 0 {
        factor = mul_shr128(factor, FACTOR10);
    }
    if abs & 0x400 != 0 {
        factor = mul_shr128(factor, FACTOR11);
    }
    if abs & 0x800 != 0 {
        factor = mul_shr128(factor, FACTOR12);
    }
    if abs & 0x1000 != 0 {
        factor = mul_shr128(factor, FACTOR13);
    }
    if abs & 0x2000 != 0 {
        factor = mul_shr128(factor, FACTOR14);
    }
    if abs & 0x4000 != 0 {
        factor = mul_shr128(factor, FACTOR15);
    }
    if tick >= 0 {
        if factor.is_zero() {
            return Err(ProtocolError::Fixed(FixedError::DivisionByZero));
        }
        factor = U256_MAX.wrapping_div(factor);
        let precision = u8::from(!factor.wrapping_rem(MASK_32).is_zero());
        Ok(factor
            .wrapping_shr(32)
            .checked_add(U256::from(precision))
            .ok_or(FixedError::Overflow)?)
    } else {
        Ok(factor.wrapping_shr(32))
    }
}

fn ge(a: U256, b: U256) -> bool {
    a >= b
}

/// Pin `TickMath.getTickAtRatio`. Tick is rounded down.
pub fn get_tick_at_ratio(ratio: U256) -> Result<(i32, U256)> {
    if ratio > MAX_RATIOX96 || ratio < MIN_RATIOX96 {
        return Err(ProtocolError::Internal);
    }
    let below = ratio < ZERO_TICK_SCALED_RATIO;
    let mut factor = if below {
        mul_div_down(ZERO_TICK_SCALED_RATIO, E26, ratio)?
    } else {
        mul_div_down(ratio, E26, ZERO_TICK_SCALED_RATIO)?
    };
    let mut tick: u32 = 0;
    // Thresholds from TickMath.sol comments at this pin (floor divisions).
    const T16384: U256 = uint!(4626198540796508716348404308345255985_U256);
    const T8192: U256 = uint!(21508599537851153911767490449162_U256);
    const T4096: U256 = uint!(46377364670549310883002866649_U256);
    const T2048: U256 = uint!(2153540449365864845468344760_U256);
    const T1024: U256 = uint!(464062544207767844008185025_U256);
    const T512: U256 = uint!(215421109505955298802281577_U256);
    const T256: U256 = uint!(146772309890508740607270615_U256);
    const T128: U256 = uint!(121149622323187099817270416_U256);
    const T64: U256 = uint!(110067989135437147685980801_U256);
    const T32: U256 = uint!(104913292358707887270979600_U256);
    const T16: U256 = uint!(102427189924701091191840928_U256);
    const T8: U256 = uint!(101206318935480056907421313_U256);
    const T4: U256 = uint!(100601351350506250000000000_U256);
    const T2: U256 = uint!(100300225000000000000000000_U256);
    const T1: U256 = uint!(100150000000000000000000000_U256);
    for (bit, thr) in [
        (0x4000u32, T16384),
        (0x2000, T8192),
        (0x1000, T4096),
        (0x800, T2048),
        (0x400, T1024),
        (0x200, T512),
        (0x100, T256),
        (0x80, T128),
        (0x40, T64),
        (0x20, T32),
        (0x10, T16),
        (0x8, T8),
        (0x4, T4),
        (0x2, T2),
        (0x1, T1),
    ] {
        if ge(factor, thr) {
            tick |= bit;
            factor = mul_div_down(factor, E26, thr)?;
        }
    }
    let (tick_i, perfect) = if below {
        let t = !tick as i32;
        let perfect = mul_div_down(ratio, factor, T1)?;
        (t, perfect)
    } else {
        let perfect = mul_div_down(ratio, E26, factor)?;
        (
            i32::try_from(tick).map_err(|_| ProtocolError::Internal)?,
            perfect,
        )
    };
    if perfect > ratio {
        return Err(ProtocolError::Internal);
    }
    Ok((tick_i, perfect))
}

/// `getExchangeRateLiquidate` 1e27 from two RAY USD prices (token units).
/// `debt_wei/col_wei * 1e27 = p_coll * 10^debt_dec * 1e27 / (p_debt * 10^coll_dec)`
/// with `p_*` already RAY, so `p_coll * 10^ddec / (p_debt * 10^cdec) * 1e27 / 1e27`
/// = `p_coll * 10^ddec / 10^cdec / p_debt` then `* RAY / 1` wait — see rounding doc R-ORACLE.
pub fn oracle_debt_per_col_1e27(
    p_coll: U256,
    p_debt: U256,
    coll_decimals: u8,
    debt_decimals: u8,
) -> Result<U256> {
    if p_coll.is_zero() || p_debt.is_zero() {
        return Err(ProtocolError::Internal);
    }
    let num = mul_div_down(p_coll, asset_unit(debt_decimals)?, p_debt)?;
    mul_div_down(num, RAY, asset_unit(coll_decimals)?)
}

/// `(p_coll, p_debt = RAY)` such that [`oracle_debt_per_col_1e27`] returns
/// `rate`, or `None` when no integer pair with the debt at 1 RAY does.
/// The debt price is the unit; the collateral price is
/// `rate · 10^coll_decimals / 10^debt_decimals` only when that division and
/// the forward floor both reproduce `rate`. A nearest price is not published.
pub fn t1_prices_from_rate(
    rate: U256,
    coll_decimals: u8,
    debt_decimals: u8,
) -> Option<(U256, U256)> {
    if rate.is_zero() {
        return None;
    }
    let c = asset_unit(coll_decimals).ok()?;
    let d = asset_unit(debt_decimals).ok()?;
    let num = rate.checked_mul(c)?;
    let p_coll = num.checked_div(d)?;
    if p_coll.checked_mul(d) != Some(num) {
        return None;
    }
    let back = oracle_debt_per_col_1e27(p_coll, RAY, coll_decimals, debt_decimals).ok()?;
    if back != rate {
        return None;
    }
    Some((p_coll, RAY))
}

/// Pin: `temp_ = (oracle * supplyExPrice) / borrowExPrice`, cap `1e45`.
pub fn raw_debt_per_col(oracle_1e27: U256, supply_ex: U256, borrow_ex: U256) -> Result<U256> {
    if oracle_1e27.is_zero() || oracle_1e27 > one_e54()? {
        return Err(ProtocolError::Internal);
    }
    if borrow_ex.is_zero() {
        return Err(ProtocolError::Fixed(FixedError::DivisionByZero));
    }
    let mut raw = mul_div_down(oracle_1e27, supply_ex, borrow_ex)?;
    if raw > ORACLE_RAW_CAP {
        raw = ORACLE_RAW_CAP;
    }
    Ok(raw)
}

/// Pin `vaultT1/coreModule/main.sol` @ `9496626f` `liquidate`:
/// `colPerUnitDebt_` is **min collateral per unit of debt in 1e18**.
/// Slip: `(actualCol * 1e18) / actualDebt < colPerUnitDebt_`.
///
/// Internal [`col_per_debt_with_penalty`] (1e27) is a different number.
/// 17A must call this from quote seize/repay — never copy `oracle_1e27`
/// or `colPerDebt` onto the wire. Executor passes the tail through with
/// no conversion.
pub fn col_per_unit_debt_1e18(actual_col: U256, actual_debt: U256) -> Result<U256> {
    if actual_debt.is_zero() {
        return Err(ProtocolError::Fixed(FixedError::DivisionByZero));
    }
    mul_div_down(actual_col, WAD, actual_debt)
}

/// Pin: `colPerDebt = (1e54 / raw) * (10000 + penalty) / 10000` (27 decimals).
pub fn col_per_debt_with_penalty(raw_debt_per_col: U256, penalty: u16) -> Result<U256> {
    if raw_debt_per_col.is_zero() {
        return Err(ProtocolError::Fixed(FixedError::DivisionByZero));
    }
    let col_per = one_e54()?
        .checked_div(raw_debt_per_col)
        .ok_or(FixedError::DivisionByZero)?;
    let num = U256::from(PENALTY_SCALE)
        .checked_add(U256::from(penalty))
        .ok_or(FixedError::Overflow)?;
    mul_div_down(col_per, num, PENALTY_SCALE)
}

/// Pin liquidation-tick ratio: `(raw * ZERO_TICK / 1e27) * threshold / 1000`.
pub fn liquidation_ratio(raw_debt_per_col: U256, threshold_3dec: u16) -> Result<U256> {
    if threshold_3dec == 0 {
        return Err(ProtocolError::Internal);
    }
    let scaled = mul_div_down(raw_debt_per_col, ZERO_TICK_SCALED_RATIO, RAY)?;
    mul_div_down(scaled, U256::from(threshold_3dec), THRESHOLD_SCALE)
}

pub fn liquidation_tick(raw_debt_per_col: U256, threshold_3dec: u16) -> Result<i32> {
    let ratio = liquidation_ratio(raw_debt_per_col, threshold_3dec)?;
    Ok(get_tick_at_ratio(ratio)?.0)
}

/// Token amount → raw: `amt * 1e12 / exPrice` (pin `/` floor).
pub fn to_raw(token_amt: U256, ex_price: U256) -> Result<U256> {
    if ex_price.is_zero() {
        return Err(ProtocolError::Fixed(FixedError::DivisionByZero));
    }
    mul_div_down(token_amt, EXCHANGE_PRICES_PRECISION, ex_price)
}

/// Raw → token: `raw * exPrice / 1e12` (pin liquidate).
pub fn from_raw(raw: U256, ex_price: U256) -> Result<U256> {
    mul_div_down(raw, ex_price, EXCHANGE_PRICES_PRECISION)
}

/// Penalty `100` = 1% → RAY `0.01`.
pub fn bonus_ray(penalty: u16) -> Result<Ray> {
    mul_div_down(U256::from(penalty), RAY, PENALTY_SCALE).map(Ray::from_raw)
}

/// `hf >= 1` iff `top_tick <= liquidation_tick`. `ratio(liq)/ratio(top)` RAY.
pub fn hf_from_ticks(top_tick: i32, liq_tick: i32) -> Result<Ray> {
    if !(MIN_TICK..=MAX_TICK).contains(&top_tick) || !(MIN_TICK..=MAX_TICK).contains(&liq_tick) {
        return Err(ProtocolError::Internal);
    }
    let r_top = get_ratio_at_tick(top_tick)?;
    let r_liq = get_ratio_at_tick(liq_tick)?;
    if r_top.is_zero() {
        return Err(ProtocolError::Fixed(FixedError::DivisionByZero));
    }
    mul_div_down(r_liq, RAY, r_top).map(Ray::from_raw)
}

/// Pin single-segment `debtLiquidated_` (perfect tick → threshold).
///
/// `x = ((debt - refRatio*debt/ratio) * 1e27) / (1e27 - colPerDebt*refRatio/2^96)`
pub fn debt_liquidated_to_ref(
    debt_raw: U256,
    ratio: U256,
    ref_ratio: U256,
    col_per_debt_27: U256,
) -> Result<U256> {
    if ratio.is_zero() {
        return Err(ProtocolError::Fixed(FixedError::DivisionByZero));
    }
    let ref_debt = mul_div_down(ref_ratio, debt_raw, ratio)?;
    if debt_raw <= ref_debt {
        return Ok(U256::ZERO);
    }
    let nom = debt_raw
        .checked_sub(ref_debt)
        .ok_or(FixedError::Underflow)?;
    let nom = nom.checked_mul(RAY).ok_or(FixedError::Overflow)?;
    let col_term = mul_div_down(col_per_debt_27, ref_ratio, ZERO_TICK_SCALED_RATIO)?;
    if col_term >= RAY {
        return Err(ProtocolError::Internal);
    }
    let den = RAY.checked_sub(col_term).ok_or(FixedError::Underflow)?;
    Ok(nom.checked_div(den).ok_or(FixedError::DivisionByZero)?)
}

pub fn col_liquidated_from_debt(debt_liq_raw: U256, col_per_debt_27: U256) -> Result<U256> {
    mul_div_down(debt_liq_raw, col_per_debt_27, RAY)
}

/// Tick from raw debt/col: `ratioX96 = debt * 2^96 / col` (floor).
pub fn tick_from_raw(col_raw: U256, debt_raw: U256) -> Result<i32> {
    if col_raw.is_zero() {
        return Err(ProtocolError::Internal);
    }
    let ratio = mul_div_down(debt_raw, ZERO_TICK_SCALED_RATIO, col_raw)?;
    Ok(get_tick_at_ratio(ratio)?.0)
}

pub fn value_wad(amount: U256, price_ray: U256, decimals: u8) -> Result<U256> {
    mul_div_down(amount, price_ray, asset_unit(decimals)?)
}

/// Admin 1e2 threshold → packed 3-decimal (`/ 10`).
pub fn pack_threshold_from_event(v: U256) -> Result<u16> {
    let packed = v
        .checked_div(U256::from(ADMIN_BPS_TO_PACKED))
        .ok_or(FixedError::DivisionByZero)?;
    let n = u16::try_from(packed).map_err(|_| ProtocolError::MalformedLog)?;
    if n > X10 {
        return Err(ProtocolError::MalformedLog);
    }
    Ok(n)
}

pub fn pack_penalty_from_event(v: U256) -> Result<u16> {
    let n = u16::try_from(v).map_err(|_| ProtocolError::MalformedLog)?;
    if n > X10 {
        return Err(ProtocolError::MalformedLog);
    }
    Ok(n)
}

/// `MarketId = 4000 + vaultId` (vault 1 → 4001). Fail closed outside 4001..=4199.
pub fn market_from_vault_id(vault_id: u32) -> Result<liq_types::MarketId> {
    if vault_id == 0 {
        return Err(ProtocolError::MalformedLog);
    }
    let id = crate::layout::CATALOG_MARKET
        .0
        .checked_add(vault_id)
        .ok_or(FixedError::Overflow)?;
    if !(crate::layout::FIRST_VAULT_MARKET.0..=crate::layout::LAST_VAULT_MARKET.0).contains(&id) {
        return Err(ProtocolError::Internal);
    }
    Ok(liq_types::MarketId(id))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects
)]
mod tick_tests {
    use super::*;

    #[test]
    fn tick_zero_is_two_pow_96() {
        assert_eq!(get_ratio_at_tick(0).unwrap(), ZERO_TICK_SCALED_RATIO);
    }

    #[test]
    fn tick_one_matches_pin_comment() {
        // TickMath.sol comment: (1.0015^1) * 2^96 = 79347004758035734099934266261
        // Assembly rounds up when `mod(factor, 2^32) != 0` → +1 vs the comment.
        assert_eq!(
            get_ratio_at_tick(1).unwrap(),
            uint!(79347004758035734099934266262_U256)
        );
    }

    #[test]
    fn get_tick_at_ratio_inverts_tick_zero() {
        let (t, _) = get_tick_at_ratio(ZERO_TICK_SCALED_RATIO).unwrap();
        assert_eq!(t, 0);
    }

    #[test]
    fn oracle_eth_usdc_example_scale() {
        // 2000 USDC/ETH: 2000e27, 1e27, 18, 6 → 2000e15 (FluidOracle targetDecimals 15).
        let r = oracle_debt_per_col_1e27(
            uint!(2000_000_000_000_000_000_000_000_000_000_U256),
            RAY,
            18,
            6,
        )
        .unwrap();
        assert_eq!(r, uint!(2_000_000_000_000_000_000_U256));
    }

    #[test]
    fn t1_rate_inverts_the_documented_eth_usdc_example() {
        // Same pin as `oracle_eth_usdc_example_scale`: rate 2000e15, 18/6
        // collateral/debt. The pair is the comment's inputs, not a value
        // copied out of `t1_prices_from_rate`.
        let (p_coll, p_debt) =
            t1_prices_from_rate(uint!(2_000_000_000_000_000_000_U256), 18, 6).unwrap();
        assert_eq!(p_coll, uint!(2000_000_000_000_000_000_000_000_000_000_U256));
        assert_eq!(p_debt, RAY);
        let back = oracle_debt_per_col_1e27(p_coll, p_debt, 18, 6).unwrap();
        assert_eq!(back, uint!(2_000_000_000_000_000_000_U256));
    }

    #[test]
    fn t1_rate_that_does_not_divide_is_not_published() {
        // 1 debt-per-col at 1e27 with 18/6 does not survive the forward floor.
        assert!(t1_prices_from_rate(U256::from(1u64), 18, 6).is_none());
        assert!(t1_prices_from_rate(U256::ZERO, 18, 6).is_none());
    }

    #[test]
    fn col_per_unit_debt_wire_is_1e18_not_1e27() {
        // Pin slip: (actualCol * 1e18) / actualDebt. 1:1 tokens → 1e18.
        let one = WAD;
        let wire = col_per_unit_debt_1e18(one, one).unwrap();
        assert_eq!(wire, WAD);
        assert!(wire < RAY);
        // A 1e27 tail fails the pin inequality on this quote (ExcessSlippage).
        assert!(wire < RAY);
        assert_eq!(col_per_unit_debt_1e18(U256::ZERO, one).unwrap(), U256::ZERO);
        assert!(col_per_unit_debt_1e18(one, U256::ZERO).is_err());
    }
}
