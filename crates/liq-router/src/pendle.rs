//! Pendle PT → SY market sale: an exact port of
//! `MarketMathCore.swapExactPtForSy` with `LogExpMath` and `PMath`
//! (pendle-core-v2-public @ `7c15b66e`, `PendleMarketV6` — the market every
//! live mainnet PT trades on). `tools/registry/pendle_math.py` is the same
//! port; `tests/pendle_vectors.rs` replays live swaps through this one.
//!
//! Solidity int256 semantics: `/` truncates toward zero; where the market
//! reverts this returns an error. `LogExpMath` runs `unchecked` but never
//! overflows in its domain; overflow here is an error (fail closed).

use alloy_primitives::{uint, I256, U256};

use crate::solver::RouteError;

type R<T> = Result<T, RouteError>;

/// One market as read at a block (`readState(0)`, the YT index), plus the
/// SY's redemption rate into the unwrapped token.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MarketSnapshot {
    pub total_pt: I256,
    pub total_sy: I256,
    pub scalar_root: I256,
    pub expiry: u64,
    pub ln_fee_rate_root: U256,
    pub reserve_fee_percent: U256,
    pub last_ln_implied_rate: U256,
    /// `max(SY.exchangeRate(), YT.pyIndexStored())` — what
    /// `YT.pyIndexCurrent()` returns in the swap.
    pub index: U256,
    /// Block time the sale is quoted for.
    pub quote_ts: u64,
    /// `SY.previewRedeem(into, sy_scale)`: linear (discovery checks it).
    pub out_per_sy_scale: U256,
    pub sy_scale: U256,
}

const ONE_18: I256 = I256::from_raw(uint!(1000000000000000000_U256));
const ONE_20: I256 = I256::from_raw(uint!(100000000000000000000_U256));
const ONE_36: I256 = I256::from_raw(uint!(1000000000000000000000000000000000000_U256));
const LN_36_LOWER: I256 = I256::from_raw(uint!(900000000000000000_U256));
const LN_36_UPPER: I256 = I256::from_raw(uint!(1100000000000000000_U256));
const MAX_EXP: I256 = I256::from_raw(uint!(130000000000000000000_U256));
/// `MIN_NATURAL_EXPONENT` is −41e18: compared through its magnitude.
const MIN_EXP_ABS: I256 = I256::from_raw(uint!(41000000000000000000_U256));
const X0: I256 = I256::from_raw(uint!(128000000000000000000_U256));
const A0: I256 = I256::from_raw(uint!(
    38877084059945950922200000000000000000000000000000000000_U256
));
const X1: I256 = I256::from_raw(uint!(64000000000000000000_U256));
const A1: I256 = I256::from_raw(uint!(6235149080811616882910000000_U256));
/// `(x_n, a_n)` for n = 2..=11 (20 decimals).
const XA: [(I256, I256); 10] = [
    (
        I256::from_raw(uint!(3200000000000000000000_U256)),
        I256::from_raw(uint!(7896296018268069516100000000000000_U256)),
    ),
    (
        I256::from_raw(uint!(1600000000000000000000_U256)),
        I256::from_raw(uint!(888611052050787263676000000_U256)),
    ),
    (
        I256::from_raw(uint!(800000000000000000000_U256)),
        I256::from_raw(uint!(298095798704172827474000_U256)),
    ),
    (
        I256::from_raw(uint!(400000000000000000000_U256)),
        I256::from_raw(uint!(5459815003314423907810_U256)),
    ),
    (
        I256::from_raw(uint!(200000000000000000000_U256)),
        I256::from_raw(uint!(738905609893065022723_U256)),
    ),
    (
        I256::from_raw(uint!(100000000000000000000_U256)),
        I256::from_raw(uint!(271828182845904523536_U256)),
    ),
    (
        I256::from_raw(uint!(50000000000000000000_U256)),
        I256::from_raw(uint!(164872127070012814685_U256)),
    ),
    (
        I256::from_raw(uint!(25000000000000000000_U256)),
        I256::from_raw(uint!(128402541668774148407_U256)),
    ),
    (
        I256::from_raw(uint!(12500000000000000000_U256)),
        I256::from_raw(uint!(113314845306682631683_U256)),
    ),
    (
        I256::from_raw(uint!(6250000000000000000_U256)),
        I256::from_raw(uint!(106449445891785942956_U256)),
    ),
];
/// `exp` applies `x2..x9`; `_ln` applies `x2..x11`.
const EXP_TERMS: usize = 8;
const IMPLIED_RATE_TIME: u64 = 365 * 86_400;
/// `MAX_MARKET_PROPORTION = 1e18 · 96 / 100`.
const MAX_MARKET_PROPORTION: I256 = I256::from_raw(uint!(960000000000000000_U256));

#[inline]
fn ik(v: u64) -> I256 {
    I256::from_raw(U256::from(v))
}
#[inline]
fn si(v: U256) -> R<I256> {
    I256::try_from(v).map_err(|_| RouteError::Math)
}
#[inline]
fn us(v: I256) -> R<U256> {
    if v.is_negative() {
        return Err(RouteError::Math);
    }
    Ok(v.into_raw())
}
#[inline]
fn add(a: I256, b: I256) -> R<I256> {
    a.checked_add(b).ok_or(RouteError::Math)
}
#[inline]
fn sub(a: I256, b: I256) -> R<I256> {
    a.checked_sub(b).ok_or(RouteError::Math)
}
#[inline]
fn mul(a: I256, b: I256) -> R<I256> {
    a.checked_mul(b).ok_or(RouteError::Math)
}
/// int256 `/`: truncates toward zero; zero divisor reverts.
#[inline]
fn div(a: I256, b: I256) -> R<I256> {
    a.checked_div(b).ok_or(RouteError::Math)
}
#[inline]
fn neg(a: I256) -> R<I256> {
    a.checked_neg().ok_or(RouteError::Math)
}

/// `LogExpMath.exp` (18 decimals).
pub fn exp(x: I256) -> R<I256> {
    if x > MAX_EXP || (x.is_negative() && neg(x)? > MIN_EXP_ABS) {
        return Err(RouteError::Math);
    }
    if x.is_negative() {
        return div(mul(ONE_18, ONE_18)?, exp(neg(x)?)?);
    }
    let mut x = x;
    let first = if x >= X0 {
        x = sub(x, X0)?;
        A0
    } else if x >= X1 {
        x = sub(x, X1)?;
        A1
    } else {
        I256::ONE
    };
    x = mul(x, ik(100))?;
    let mut product = ONE_20;
    for &(xn, an) in XA.iter().take(EXP_TERMS) {
        if x >= xn {
            x = sub(x, xn)?;
            product = div(mul(product, an)?, ONE_20)?;
        }
    }
    let mut series = ONE_20;
    let mut term = x;
    series = add(series, term)?;
    for k in 2u64..=12 {
        term = div(div(mul(term, x)?, ONE_20)?, ik(k))?;
        series = add(series, term)?;
    }
    div(mul(div(mul(product, series)?, ONE_20)?, first)?, ik(100))
}

fn ln_inner(a: I256) -> R<I256> {
    if a < ONE_18 {
        return neg(ln_inner(div(mul(ONE_18, ONE_18)?, a)?)?);
    }
    let mut a = a;
    let mut sum = I256::ZERO;
    if a >= mul(A0, ONE_18)? {
        a = div(a, A0)?;
        sum = add(sum, X0)?;
    }
    if a >= mul(A1, ONE_18)? {
        a = div(a, A1)?;
        sum = add(sum, X1)?;
    }
    sum = mul(sum, ik(100))?;
    a = mul(a, ik(100))?;
    for &(xn, an) in &XA {
        if a >= an {
            a = div(mul(a, ONE_20)?, an)?;
            sum = add(sum, xn)?;
        }
    }
    let z = div(mul(sub(a, ONE_20)?, ONE_20)?, add(a, ONE_20)?)?;
    let z2 = div(mul(z, z)?, ONE_20)?;
    let mut num = z;
    let mut series = num;
    for k in [3u64, 5, 7, 9, 11] {
        num = div(mul(num, z2)?, ONE_20)?;
        series = add(series, div(num, ik(k))?)?;
    }
    series = mul(series, ik(2))?;
    div(add(sum, series)?, ik(100))
}

fn ln_36(x: I256) -> R<I256> {
    let x = mul(x, ONE_18)?;
    let z = div(mul(sub(x, ONE_36)?, ONE_36)?, add(x, ONE_36)?)?;
    let z2 = div(mul(z, z)?, ONE_36)?;
    let mut num = z;
    let mut series = num;
    for k in [3u64, 5, 7, 9, 11, 13, 15] {
        num = div(mul(num, z2)?, ONE_36)?;
        series = add(series, div(num, ik(k))?)?;
    }
    mul(series, ik(2))
}

/// `LogExpMath.ln` (18 decimals).
pub fn ln(a: I256) -> R<I256> {
    if a <= I256::ZERO {
        return Err(RouteError::Math);
    }
    if LN_36_LOWER < a && a < LN_36_UPPER {
        return div(ln_36(a)?, ONE_18);
    }
    ln_inner(a)
}

/// `PMath.divDown(int256, int256)`.
fn div_down(a: I256, b: I256) -> R<I256> {
    div(mul(a, ONE_18)?, b)
}

/// `PMath.subNoNeg`.
fn sub_no_neg(a: I256, b: I256) -> R<I256> {
    if a < b {
        return Err(RouteError::Math);
    }
    sub(a, b)
}

/// `PYIndexLib` on signed amounts: sign kept, magnitude floored (or ceiled).
fn sy_to_asset(index: I256, sy: I256) -> R<I256> {
    let m = div(
        mul(sy.checked_abs().ok_or(RouteError::Math)?, index)?,
        ONE_18,
    )?;
    if sy.is_negative() {
        neg(m)
    } else {
        Ok(m)
    }
}
fn asset_to_sy(index: I256, asset: I256, up: bool) -> R<I256> {
    let a = asset.checked_abs().ok_or(RouteError::Math)?;
    let num = mul(a, ONE_18)?;
    let m = if up {
        div(sub(add(num, index)?, I256::ONE)?, index)?
    } else {
        div(num, index)?
    };
    if asset.is_negative() {
        neg(m)
    } else {
        Ok(m)
    }
}

fn log_proportion(p: I256) -> R<I256> {
    if p == ONE_18 {
        return Err(RouteError::Math);
    }
    ln(div_down(p, sub(ONE_18, p)?)?)
}

fn exchange_rate(
    total_pt: I256,
    total_asset: I256,
    rate_scalar: I256,
    rate_anchor: I256,
    net_pt: I256,
) -> R<I256> {
    let num = sub_no_neg(total_pt, net_pt)?;
    let p = div_down(num, add(total_pt, total_asset)?)?;
    if p > MAX_MARKET_PROPORTION {
        return Err(RouteError::InsufficientLiquidity);
    }
    let r = add(div_down(log_proportion(p)?, rate_scalar)?, rate_anchor)?;
    if r < ONE_18 {
        return Err(RouteError::InsufficientLiquidity);
    }
    Ok(r)
}

fn rate_from_implied(ln_rate: U256, tte: u64) -> R<I256> {
    let rt = ln_rate
        .checked_mul(U256::from(tte))
        .and_then(|v| v.checked_div(U256::from(IMPLIED_RATE_TIME)))
        .ok_or(RouteError::Math)?;
    exp(si(rt)?)
}

/// `swapExactPtForSy(pt_in)` quoted at `s.quote_ts`: the SY paid to the
/// seller. Errors where the market reverts (expired, proportion above 96 %,
/// rate below one, zero LP fee, …).
pub fn sell_pt(s: &MarketSnapshot, pt_in: U256) -> R<U256> {
    if s.expiry <= s.quote_ts {
        return Err(RouteError::StalePool);
    }
    let index = si(s.index)?;
    let net_pt = neg(si(pt_in)?)?;
    if s.total_pt <= net_pt {
        return Err(RouteError::InsufficientLiquidity);
    }
    let tte = s.expiry.checked_sub(s.quote_ts).ok_or(RouteError::Math)?;
    let rate_scalar = div(mul(s.scalar_root, ik(IMPLIED_RATE_TIME))?, ik(tte))?;
    if rate_scalar <= I256::ZERO {
        return Err(RouteError::Math);
    }
    let total_asset = sy_to_asset(index, s.total_sy)?;
    if s.total_pt.is_zero() || total_asset.is_zero() {
        return Err(RouteError::InsufficientLiquidity);
    }
    // _getRateAnchor
    let new_rate = rate_from_implied(s.last_ln_implied_rate, tte)?;
    if new_rate < ONE_18 {
        return Err(RouteError::Math);
    }
    let prop = div_down(s.total_pt, add(s.total_pt, total_asset)?)?;
    let anchor = sub(new_rate, div_down(log_proportion(prop)?, rate_scalar)?)?;
    let fee_rate = rate_from_implied(s.ln_fee_rate_root, tte)?;
    // calcTrade (netPtToAccount < 0)
    let pre = exchange_rate(s.total_pt, total_asset, rate_scalar, anchor, net_pt)?;
    let pre_asset = neg(div_down(net_pt, pre)?)?;
    let fee = neg(div(mul(pre_asset, sub(ONE_18, fee_rate)?)?, fee_rate)?)?;
    let reserve = div(mul(fee, si(s.reserve_fee_percent)?)?, ik(100))?;
    let net_asset = sub(pre_asset, fee)?;
    let net_sy = asset_to_sy(index, net_asset, net_asset.is_negative())?;
    let sy_fee = asset_to_sy(index, fee, false)?;
    let sy_reserve = asset_to_sy(index, reserve, false)?;
    // _setNewMarketStateTrade
    let new_pt = sub_no_neg(s.total_pt, net_pt)?;
    let new_sy = sub_no_neg(s.total_sy, add(net_sy, sy_reserve)?)?;
    let r = exchange_rate(
        new_pt,
        sy_to_asset(index, new_sy)?,
        rate_scalar,
        anchor,
        I256::ZERO,
    )?;
    let ln_implied = us(ln(r)?)?
        .checked_mul(U256::from(IMPLIED_RATE_TIME))
        .and_then(|v| v.checked_div(U256::from(tte)))
        .ok_or(RouteError::Math)?;
    if ln_implied.is_zero() {
        return Err(RouteError::Math);
    }
    // PendleMarketV6.swapExactPtForSy: MarketZeroNetLPFee
    if sy_to_asset(index, sub(sy_fee, sy_reserve)?)?.is_zero() {
        return Err(RouteError::InsufficientLiquidity);
    }
    us(sy_fee)?;
    us(sy_reserve)?;
    us(net_sy)
}

/// Sell `pt_in` and redeem the SY: what the unwrap pays in the SY's output
/// token. The SY step is linear; it keeps the same haircut as any linear
/// unwrap (one part per million and one wei).
pub fn sell_pt_for_out(s: &MarketSnapshot, pt_in: U256) -> R<U256> {
    let sy = sell_pt(s, pt_in)?;
    if s.sy_scale.is_zero() {
        return Err(RouteError::StalePool);
    }
    let raw = crate::solver::mul_div_512(sy, s.out_per_sy_scale, s.sy_scale)?;
    let haircut = raw
        .checked_div(U256::from(1_000_000u64))
        .unwrap_or_default();
    Ok(raw.saturating_sub(haircut).saturating_sub(U256::ONE))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    fn i(v: i128) -> I256 {
        I256::try_from(v).unwrap()
    }

    /// Balancer's `LogExpMath` reference points: `exp(1) = e`, `ln(e) ≈ 1`,
    /// `exp(ln(x)) ≈ x`, and the `ln_36` band near one.
    #[test]
    fn log_exp_reference_points() {
        let e = exp(ONE_18).unwrap();
        assert_eq!(e, i(2_718_281_828_459_045_235));
        let one = ln(e).unwrap();
        assert!((one - ONE_18).checked_abs().unwrap() <= i(10));
        for x in [
            i(5 * 10i128.pow(17)),
            i(1_050_000_000_000_000_000),
            i(42 * 10i128.pow(18)),
        ] {
            let back = exp(ln(x).unwrap()).unwrap();
            assert!((back - x).checked_abs().unwrap() * i(1_000_000_000_000) <= x);
        }
        assert!(ln(I256::ZERO).is_err());
        assert!(exp(i(131 * 10i128.pow(18))).is_err());
    }
}
