//! Balancer V2 weighted pools (two tokens): exact integer ports of the
//! deployed swap math.
//!
//! A line-for-line port of `LogExpMath.sol`, `FixedPoint.sol` and
//! `WeightedMath._calcOutGivenIn` / `_calcInGivenOut` of the audited
//! `balancer-v2-monorepo` the pools were built from, checked against each
//! live pool's own `onSwap` through the Vault (`tests/balancer_vectors.rs`,
//! recorded by `tools/registry/balancer_vectors.py`). Pools:
//! - `WeightedPool` v4 (`20230320-weighted-pool-v4`): `powUp`/`powDown`
//!   take a shortcut when the exponent is exactly 1, 2 or 4;
//! - `WeightedPool2Tokens` (2021, the first Balancer pools): the same math
//!   without those shortcuts, so a 50/50 pool's exponent of exactly 1 still
//!   goes through `LogExpMath.pow` and its error margin.
//!
//! Solidity 0.7 arithmetic wraps; the contracts guard every operation that
//! could with `_require`, so each operation here is checked and an overflow
//! is [`RouteError::Math`] (a revert).

// A line-for-line port of the deployed Balancer arithmetic: where the contracts
// subtract or index, the same guard (a comparison, a fixed array length)
// precedes it here, and every operation that can overflow is checked.
#![allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use alloy_primitives::{Address, B256, I256, U256};
use smallvec::SmallVec;

use crate::solver::{narrow, RouteError, MAX_COINS};

type R<T> = Result<T, RouteError>;

const ONE: u128 = 1_000_000_000_000_000_000;
const MAX_POW_RELATIVE_ERROR: u64 = 10_000;
/// `WeightedMath._MAX_IN_RATIO` and `_MAX_OUT_RATIO`: 30 %.
const MAX_RATIO: u128 = 300_000_000_000_000_000;

fn one() -> U256 {
    U256::from(ONE)
}

// ───────────────────────────── FixedPoint ─────────────────────────────

fn add(a: U256, b: U256) -> R<U256> {
    a.checked_add(b).ok_or(RouteError::Math)
}

fn sub(a: U256, b: U256) -> R<U256> {
    a.checked_sub(b).ok_or(RouteError::Math)
}

fn mul_down(a: U256, b: U256) -> R<U256> {
    a.checked_mul(b).map(|p| p / one()).ok_or(RouteError::Math)
}

fn mul_up(a: U256, b: U256) -> R<U256> {
    let p = a.checked_mul(b).ok_or(RouteError::Math)?;
    if p.is_zero() {
        return Ok(U256::ZERO);
    }
    // ((product - 1) / ONE) + 1
    add((p - U256::ONE) / one(), U256::ONE)
}

fn div_down(a: U256, b: U256) -> R<U256> {
    if b.is_zero() {
        return Err(RouteError::Math);
    }
    if a.is_zero() {
        return Ok(U256::ZERO);
    }
    Ok(a.checked_mul(one()).ok_or(RouteError::Math)? / b)
}

fn div_up(a: U256, b: U256) -> R<U256> {
    if b.is_zero() {
        return Err(RouteError::Math);
    }
    if a.is_zero() {
        return Ok(U256::ZERO);
    }
    let inflated = a.checked_mul(one()).ok_or(RouteError::Math)?;
    add((inflated - U256::ONE) / b, U256::ONE)
}

/// `FixedPoint.complement`: `1 - x`, or 0 past 1.
fn complement(x: U256) -> U256 {
    if x < one() {
        one() - x
    } else {
        U256::ZERO
    }
}

/// `FixedPoint.powUp` (`fast`: the v4 shortcuts for 1, 2 and 4). The weighted
/// swap formulas use only this one: both round the power up.
fn pow_up(x: U256, y: U256, fast: bool) -> R<U256> {
    if fast {
        if y == one() {
            return Ok(x);
        }
        if y == U256::from(2 * ONE) {
            return mul_up(x, x);
        }
        if y == U256::from(4 * ONE) {
            let sq = mul_up(x, x)?;
            return mul_up(sq, sq);
        }
    }
    let raw = log_exp_pow(x, y)?;
    let max_error = add(mul_up(raw, U256::from(MAX_POW_RELATIVE_ERROR))?, U256::ONE)?;
    add(raw, max_error)
}

// ───────────────────────────── LogExpMath ─────────────────────────────

fn i(v: i128) -> I256 {
    I256::try_from(v).unwrap_or(I256::ZERO)
}

fn big(s: &str) -> I256 {
    I256::from_dec_str(s).unwrap_or(I256::ZERO)
}

fn ck_add(a: I256, b: I256) -> R<I256> {
    a.checked_add(b).ok_or(RouteError::Math)
}

fn ck_sub(a: I256, b: I256) -> R<I256> {
    a.checked_sub(b).ok_or(RouteError::Math)
}

fn ck_mul(a: I256, b: I256) -> R<I256> {
    a.checked_mul(b).ok_or(RouteError::Math)
}

/// Solidity `/` on int256: truncates toward zero; zero divisor reverts.
fn ck_div(a: I256, b: I256) -> R<I256> {
    a.checked_div(b).ok_or(RouteError::Math)
}

/// Solidity `%` on int256: the remainder takes the dividend's sign.
fn ck_rem(a: I256, b: I256) -> R<I256> {
    a.checked_rem(b).ok_or(RouteError::Math)
}

struct Consts {
    one_18: I256,
    one_20: I256,
    one_36: I256,
    /// `x_n` and `a_n` of the exponent decomposition (the `a_n` carry no
    /// decimals, except where the contract's comments say otherwise).
    x: [I256; 12],
    a: [I256; 12],
}

fn consts() -> Consts {
    Consts {
        one_18: i(1_000_000_000_000_000_000),
        one_20: big("100000000000000000000"),
        one_36: big("1000000000000000000000000000000000000"),
        x: [
            big("128000000000000000000"),
            big("64000000000000000000"),
            big("3200000000000000000000"),
            big("1600000000000000000000"),
            big("800000000000000000000"),
            big("400000000000000000000"),
            big("200000000000000000000"),
            big("100000000000000000000"),
            big("50000000000000000000"),
            big("25000000000000000000"),
            big("12500000000000000000"),
            big("6250000000000000000"),
        ],
        a: [
            big("38877084059945950922200000000000000000000000000000000000"),
            big("6235149080811616882910000000"),
            big("7896296018268069516100000000000000"),
            big("888611052050787263676000000"),
            big("298095798704172827474000"),
            big("5459815003314423907810"),
            big("738905609893065022723"),
            big("271828182845904523536"),
            big("164872127070012814685"),
            big("128402541668774148407"),
            big("113314845306682631683"),
            big("106449445891785942956"),
        ],
    }
}

/// `LogExpMath._ln_36`: ln(x) to 36 decimals for x within 0.9..1.1.
fn ln_36(x: I256, c: &Consts) -> R<I256> {
    let x = ck_mul(x, c.one_18)?;
    let z = ck_div(
        ck_mul(ck_sub(x, c.one_36)?, c.one_36)?,
        ck_add(x, c.one_36)?,
    )?;
    let z_squared = ck_div(ck_mul(z, z)?, c.one_36)?;
    let mut num = z;
    let mut series = num;
    for d in [3i128, 5, 7, 9, 11, 13, 15] {
        num = ck_div(ck_mul(num, z_squared)?, c.one_36)?;
        series = ck_add(series, ck_div(num, i(d))?)?;
    }
    ck_mul(series, i(2))
}

/// `LogExpMath._ln`: ln(a) for an 18-decimal `a`.
fn ln(a: I256, c: &Consts) -> R<I256> {
    if a < c.one_18 {
        // -_ln(1e36 / a)
        let inv = ck_div(ck_mul(c.one_18, c.one_18)?, a)?;
        return ln(inv, c)?.checked_neg().ok_or(RouteError::Math);
    }
    let mut a = a;
    let mut sum = I256::ZERO;
    if a >= ck_mul(c.a[0], c.one_18)? {
        a = ck_div(a, c.a[0])?;
        sum = ck_add(sum, c.x[0])?;
    }
    if a >= ck_mul(c.a[1], c.one_18)? {
        a = ck_div(a, c.a[1])?;
        sum = ck_add(sum, c.x[1])?;
    }
    // Everything else is done with 20 decimals.
    sum = ck_mul(sum, i(100))?;
    a = ck_mul(a, i(100))?;
    for n in 2..12 {
        if a >= c.a[n] {
            a = ck_div(ck_mul(a, c.one_20)?, c.a[n])?;
            sum = ck_add(sum, c.x[n])?;
        }
    }
    let z = ck_div(
        ck_mul(ck_sub(a, c.one_20)?, c.one_20)?,
        ck_add(a, c.one_20)?,
    )?;
    let z_squared = ck_div(ck_mul(z, z)?, c.one_20)?;
    let mut num = z;
    let mut series = num;
    for d in [3i128, 5, 7, 9, 11] {
        num = ck_div(ck_mul(num, z_squared)?, c.one_20)?;
        series = ck_add(series, ck_div(num, i(d))?)?;
    }
    series = ck_mul(series, i(2))?;
    ck_div(ck_add(sum, series)?, i(100))
}

/// `LogExpMath.exp`: e^x for an 18-decimal `x` in [-41, 130].
fn exp(x: I256, c: &Consts) -> R<I256> {
    if x < i(-41_000_000_000_000_000_000) || x > i(130_000_000_000_000_000_000) {
        return Err(RouteError::Math);
    }
    if x < I256::ZERO {
        // 1e36 / exp(-x)
        let pos = exp(x.checked_neg().ok_or(RouteError::Math)?, c)?;
        return ck_div(ck_mul(c.one_18, c.one_18)?, pos);
    }
    let mut x = x;
    let first_an;
    if x >= c.x[0] {
        x = ck_sub(x, c.x[0])?;
        first_an = c.a[0];
    } else if x >= c.x[1] {
        x = ck_sub(x, c.x[1])?;
        first_an = c.a[1];
    } else {
        first_an = I256::ONE;
    }
    // The rest with 20 decimals.
    x = ck_mul(x, i(100))?;
    let mut product = c.one_20;
    for n in 2..10 {
        if x >= c.x[n] {
            x = ck_sub(x, c.x[n])?;
            product = ck_div(ck_mul(product, c.a[n])?, c.one_20)?;
        }
    }
    // Taylor series: 1 + x + x²/2! + … + x¹²/12!.
    let mut series = ck_add(c.one_20, x)?;
    let mut term = x;
    for n in 2..=12i128 {
        term = ck_div(ck_div(ck_mul(term, x)?, c.one_20)?, i(n))?;
        series = ck_add(series, term)?;
    }
    ck_div(
        ck_mul(ck_div(ck_mul(product, series)?, c.one_20)?, first_an)?,
        i(100),
    )
}

/// `LogExpMath.pow(x, y)`: x^y with 18-decimal base and exponent.
fn log_exp_pow(x: U256, y: U256) -> R<U256> {
    if y.is_zero() {
        return Ok(one());
    }
    if x.is_zero() {
        return Ok(U256::ZERO);
    }
    // x < 2^255
    if x >> 255 != U256::ZERO {
        return Err(RouteError::Math);
    }
    let c = consts();
    let x_int = I256::from_raw(x);
    // y < MILD_EXPONENT_BOUND = 2^254 / 1e20
    let bound = (U256::ONE << 254) / U256::from(10u64).pow(U256::from(20u64));
    if y >= bound {
        return Err(RouteError::Math);
    }
    let y_int = I256::from_raw(y);
    let lower = ck_add(c.one_18, i(-100_000_000_000_000_000))?;
    let upper = ck_add(c.one_18, i(100_000_000_000_000_000))?;
    let logx_times_y = if lower < x_int && x_int < upper {
        let ln36 = ln_36(x_int, &c)?;
        ck_add(
            ck_mul(ck_div(ln36, c.one_18)?, y_int)?,
            ck_div(ck_mul(ck_rem(ln36, c.one_18)?, y_int)?, c.one_18)?,
        )?
    } else {
        ck_mul(ln(x_int, &c)?, y_int)?
    };
    let logx_times_y = ck_div(logx_times_y, c.one_18)?;
    if logx_times_y < i(-41_000_000_000_000_000_000)
        || logx_times_y > i(130_000_000_000_000_000_000)
    {
        return Err(RouteError::Math);
    }
    Ok(exp(logx_times_y, &c)?.into_raw())
}

// ───────────────────────────── WeightedMath ─────────────────────────────

/// `WeightedMath._calcOutGivenIn`, all amounts upscaled to 18 decimals.
fn calc_out_given_in(
    balance_in: U256,
    weight_in: U256,
    balance_out: U256,
    weight_out: U256,
    amount_in: U256,
    fast: bool,
) -> R<U256> {
    if amount_in > mul_down(balance_in, U256::from(MAX_RATIO))? {
        return Err(RouteError::Math); // MAX_IN_RATIO
    }
    let denominator = add(balance_in, amount_in)?;
    let base = div_up(balance_in, denominator)?;
    let exponent = div_down(weight_in, weight_out)?;
    let power = pow_up(base, exponent, fast)?;
    mul_down(balance_out, complement(power))
}

/// `WeightedMath._calcInGivenOut`, all amounts upscaled to 18 decimals.
fn calc_in_given_out(
    balance_in: U256,
    weight_in: U256,
    balance_out: U256,
    weight_out: U256,
    amount_out: U256,
    fast: bool,
) -> R<U256> {
    if amount_out > mul_down(balance_out, U256::from(MAX_RATIO))? {
        return Err(RouteError::Math); // MAX_OUT_RATIO
    }
    let base = div_up(balance_out, sub(balance_out, amount_out)?)?;
    let exponent = div_up(weight_out, weight_in)?;
    let power = pow_up(base, exponent, fast)?;
    let ratio = sub(power, one())?;
    mul_up(balance_in, ratio)
}

// ───────────────────────────── the pool ─────────────────────────────

/// One Balancer V2 weighted pool read at a block (the reseed thread's
/// answer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BalancerRead {
    /// `Vault.getPoolTokens` balances, in the pool's (ascending) token order.
    pub balances: Vec<U256>,
    /// `getNormalizedWeights()`.
    pub weights: Vec<U256>,
    /// `getSwapFeePercentage()`.
    pub swap_fee: U256,
    /// `getPausedState().paused`.
    pub paused: bool,
}

/// A Balancer V2 weighted pool's swap state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BalancerState {
    /// The Vault's pool id: the leg's data.
    pub pool_id: B256,
    /// The pool's tokens, in pool order (to match the Vault's Swap logs).
    pub tokens: SmallVec<[Address; MAX_COINS]>,
    /// Vault balances, raw token units, in the pool's token order.
    pub balances: SmallVec<[U256; MAX_COINS]>,
    /// Normalized weights, 18 decimals.
    pub weights: SmallVec<[U256; MAX_COINS]>,
    /// `10^(18 − decimals)` of each token.
    pub scaling: SmallVec<[U256; MAX_COINS]>,
    /// Swap fee, 18 decimals.
    pub swap_fee: U256,
    /// A v4 pool: `powUp`/`powDown` shortcut an exponent of 1, 2 or 4.
    pub fast_pow: bool,
    /// Set by any Vault log on the pool, or while the pool is paused;
    /// cleared by a read.
    pub stale: bool,
    pub stale_block: u64,
    pub read_block: u64,
}

impl BalancerState {
    fn pair(&self, i: u8, j: u8) -> R<(usize, usize)> {
        let (i, j) = (usize::from(i), usize::from(j));
        if i == j || i >= self.balances.len() || j >= self.balances.len() {
            return Err(RouteError::BadLeg);
        }
        if self.weights.len() != self.balances.len() || self.scaling.len() != self.balances.len() {
            return Err(RouteError::BadLeg);
        }
        Ok((i, j))
    }

    /// `true` when the pool can be quoted.
    #[must_use]
    pub fn is_live(&self) -> bool {
        !self.stale
            && self.balances.len() == 2
            && self.balances.iter().all(|b| !b.is_zero())
            && self.weights.len() == 2
            && self.scaling.len() == 2
    }

    /// Output of `swap(GIVEN_IN)` of `dx` of coin `i` into coin `j` through
    /// the Vault: `BasePool.onSwap` — the fee off the raw amount (rounding
    /// the fee up), everything upscaled, the weighted formula, the output
    /// downscaled down.
    pub fn dy(&self, i: u8, j: u8, dx: U256) -> R<U256> {
        if self.stale {
            return Err(RouteError::StalePool);
        }
        let (i, j) = self.pair(i, j)?;
        if dx.is_zero() {
            return Ok(U256::ZERO);
        }
        let (si, sj) = (self.scaling[i], self.scaling[j]);
        let (bi, bj) = (
            self.balances[i].checked_mul(si).ok_or(RouteError::Math)?,
            self.balances[j].checked_mul(sj).ok_or(RouteError::Math)?,
        );
        let fee = mul_up(dx, self.swap_fee)?;
        let amount = sub(dx, fee)?.checked_mul(si).ok_or(RouteError::Math)?;
        let out = calc_out_given_in(
            bi,
            self.weights[i],
            bj,
            self.weights[j],
            amount,
            self.fast_pow,
        )?;
        Ok(out / sj)
    }

    /// Input of coin `i` that `swap(GIVEN_OUT)` of `dy` of coin `j` costs:
    /// the formula on the upscaled output, the input downscaled **up**, the
    /// fee added after (rounding up).
    pub fn dx(&self, i: u8, j: u8, dy: U256) -> R<U256> {
        if self.stale {
            return Err(RouteError::StalePool);
        }
        let (i, j) = self.pair(i, j)?;
        let (si, sj) = (self.scaling[i], self.scaling[j]);
        let (bi, bj) = (
            self.balances[i].checked_mul(si).ok_or(RouteError::Math)?,
            self.balances[j].checked_mul(sj).ok_or(RouteError::Math)?,
        );
        let amount = dy.checked_mul(sj).ok_or(RouteError::Math)?;
        let raw = calc_in_given_out(
            bi,
            self.weights[i],
            bj,
            self.weights[j],
            amount,
            self.fast_pow,
        )?;
        // `_downscaleUp`: divUp by the factor (plain integers here).
        let down = if raw.is_zero() {
            U256::ZERO
        } else {
            (raw - U256::ONE) / si + U256::ONE
        };
        div_up(down, complement(self.swap_fee))
    }

    /// The input of coin `i` the pool accepts: 30 % of its balance after the
    /// fee (`MAX_IN_RATIO`), as raw units of coin `i`.
    pub fn capacity_in(&self, i: u8) -> Option<U256> {
        let i = usize::from(i);
        let bal = self.balances.get(i)?;
        let cap = bal.checked_mul(U256::from(MAX_RATIO))? / one();
        // The ratio is on the amount after fee.
        cap.checked_mul(one())?
            .checked_div(complement(self.swap_fee))
    }

    /// Output of the swap, **mutating** the balances as the Vault does: the
    /// input joins coin `i`, the output leaves coin `j`.
    pub fn apply(&mut self, i: u8, j: u8, dx: U256) -> R<U256> {
        let out = self.dy(i, j, dx)?;
        let (iu, ju) = self.pair(i, j)?;
        self.balances[iu] = add(self.balances[iu], dx)?;
        self.balances[ju] = sub(self.balances[ju], out)?;
        Ok(out)
    }

    /// `ρ(x)`: `sqrt` of the marginal output per input after `x` of coin `i`,
    /// Q96 raw units, by a forward difference of the exact output (the
    /// formula has no integer derivative). The step is 1e-5 of the input
    /// balance, at least 1; at `x = 0` the difference is between `h` and `2h`.
    pub fn rho(&self, i: u8, j: u8, x: U256) -> R<U256> {
        let bi = *self
            .balances
            .get(usize::from(i))
            .ok_or(RouteError::BadLeg)?;
        let h = (bi / U256::from(100_000u64)).max(U256::ONE);
        let a = if x.is_zero() { h } else { x };
        let q0 = self.dy(i, j, a)?;
        let q1 = self.dy(i, j, add(a, h)?)?;
        let dq = q1.saturating_sub(q0);
        let q192 = U256::ONE << 192;
        let q = alloy_primitives::U512::from(dq)
            .checked_mul(alloy_primitives::U512::from(q192))
            .ok_or(RouteError::Math)?
            .checked_div(alloy_primitives::U512::from(h))
            .ok_or(RouteError::Math)?;
        narrow(q.root(2))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    /// Oracle: closed-form identities that hold to the contract's stated
    /// error (1e-14 relative): x^1 = x, x^2 = x·x, x^0.5 = sqrt, and
    /// e^(ln x) = x, at bases on both sides of the 0.9..1.1 `ln_36` branch.
    #[test]
    fn pow_matches_closed_forms() {
        let e18 = |v: u128| U256::from(v) * U256::from(10u64).pow(U256::from(18u64));
        let near = |got: U256, want: U256, rel: u64| {
            let diff = got.abs_diff(want);
            assert!(
                diff <= want / U256::from(rel) + U256::from(2u64),
                "{got} vs {want}"
            );
        };
        for base in [
            e18(1) / U256::from(2u64),
            e18(1) * U256::from(95u64) / U256::from(100u64),
            e18(1) * U256::from(101u64) / U256::from(100u64),
            e18(3),
            e18(1000),
        ] {
            near(
                log_exp_pow(base, e18(1)).unwrap(),
                base,
                100_000_000_000_000,
            );
            let sq = mul_down(base, base).unwrap();
            near(log_exp_pow(base, e18(2)).unwrap(), sq, 100_000_000_000_000);
            // x^0.5 squared is x.
            let half = log_exp_pow(base, e18(1) / U256::from(2u64)).unwrap();
            near(mul_down(half, half).unwrap(), base, 10_000_000_000_000);
        }
        assert_eq!(log_exp_pow(e18(7), U256::ZERO).unwrap(), e18(1));
        assert_eq!(log_exp_pow(U256::ZERO, e18(7)).unwrap(), U256::ZERO);
        // Past the natural exponent bound: 2^200 has ln·y over 130.
        assert!(log_exp_pow(e18(1) << 100usize, e18(4)).is_err());
    }
}
