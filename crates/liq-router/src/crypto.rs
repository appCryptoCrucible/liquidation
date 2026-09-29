//! Curve crypto pools (twocrypto-ng, tricrypto-ng, the original
//! CurveCryptoSwap2): exact integer ports of the deployed swap math.
//!
//! A line-for-line port of `tools/registry/crypto_math.py`, which discovery
//! checks against each admitted pool's own `get_dy` (every ordered coin pair,
//! two sizes). Sources:
//! - `CurveTwocryptoMathOptimized.vy` v2.0.0 (`0x2005…64df`) / v2.1.0
//!   (`0x1fd8…f4a1`) and `CurveTwocryptoOptimized.vy` `_exchange` / `_fee`;
//! - `CurveTricryptoMathOptimized.vy` v2.0.0 (`0xcbff…d6ee`) and
//!   `CurveTricryptoOptimizedWETH.vy` `_exchange` / `_fee`;
//! - `CurveCryptoSwap2ETH.vy` (the original factory pools): `newton_y` only.
//!
//! Vyper semantics: `uint256` arithmetic is checked (an overflow is a revert:
//! [`RouteError::Math`]), `unsafe_*` wraps, `int256` `/` truncates toward
//! zero. `A`/`gamma` ramps are not modelled: a ramping pool is stale.

use alloy_primitives::{I256, U256, U512};
use smallvec::SmallVec;

use crate::solver::{narrow, RouteError, MAX_COINS};

/// `2^192`, for Q96 `ρ`.
const Q192: U256 =
    alloy_primitives::uint!(0x1000000000000000000000000000000000000000000000000_U256);

type R<T> = Result<T, RouteError>;

const A_MULTIPLIER: u64 = 10_000;

/// Which deployed math the pool runs.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CryptoKind {
    /// Original `CurveCryptoSwap2` factory pool: `newton_y`, v2.0.0 bounds.
    TwoV1,
    /// twocrypto-ng with `CurveTwocryptoMathOptimized` v2.0.0.
    TwoV200,
    /// twocrypto-ng with `CurveTwocryptoMathOptimized` v2.1.0.
    TwoV210,
    /// tricrypto-ng with `CurveTricryptoMathOptimized` v2.0.0.
    Tri,
}

/// One crypto pool read at a block (the reseed thread's answer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CryptoRead {
    pub balances: Vec<U256>,
    pub price_scale: Vec<U256>,
    pub d: U256,
    pub ann: U256,
    pub gamma: U256,
    pub mid_fee: U256,
    pub out_fee: U256,
    pub fee_gamma: U256,
    /// `future_A_gamma_time()` is after the read block's timestamp.
    pub ramping: bool,
}

/// A crypto pool's swap state, read off the hot path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CryptoState {
    pub kind: CryptoKind,
    /// Raw `balances(i)`.
    pub balances: SmallVec<[U256; MAX_COINS]>,
    /// `10^(18 − decimals_i)`.
    pub precisions: SmallVec<[U256; MAX_COINS]>,
    /// `price_scale()` (two coins) or `price_scale(0..2)` (three).
    pub price_scale: SmallVec<[U256; MAX_COINS]>,
    /// Stored `D()`.
    pub d: U256,
    /// `A()`: already `A · N^N · A_MULTIPLIER`.
    pub ann: U256,
    pub gamma: U256,
    pub mid_fee: U256,
    pub out_fee: U256,
    pub fee_gamma: U256,
    /// Set by any pool log, and after a simulated swap (the pool's
    /// `tweak_price` moves `D` and `price_scale`, which this model does not
    /// follow); cleared by a read.
    pub stale: bool,
    pub stale_block: u64,
    pub read_block: u64,
}

impl CryptoState {
    fn n(&self) -> R<usize> {
        let n = self.balances.len();
        let want = if self.kind == CryptoKind::Tri { 3 } else { 2 };
        let n_ps = n.checked_sub(1).ok_or(RouteError::BadLeg)?;
        if n != want || self.precisions.len() != n || self.price_scale.len() != n_ps {
            return Err(RouteError::BadLeg);
        }
        Ok(n)
    }

    /// `true` when the pool can be quoted.
    #[must_use]
    pub fn is_live(&self) -> bool {
        !self.stale && !self.d.is_zero() && self.balances.iter().all(|b| !b.is_zero())
    }

    /// Output of `exchange(i, j, dx)` against this state.
    pub fn dy(&self, i: u8, j: u8, dx: U256) -> R<U256> {
        if self.stale {
            return Err(RouteError::StalePool);
        }
        let n = self.n()?;
        let (i, j) = (usize::from(i), usize::from(j));
        if i == j || i >= n || j >= n {
            return Err(RouteError::BadLeg);
        }
        if dx.is_zero() {
            return Ok(U256::ZERO);
        }
        let mut bal = self.balances.clone();
        let bi = bal.get_mut(i).ok_or(RouteError::BadLeg)?;
        *bi = add(*bi, dx)?;
        let e18 = e(18);
        let mut xp: SmallVec<[U256; MAX_COINS]> = SmallVec::new();
        for (k, (&b, &p)) in bal.iter().zip(&self.precisions).enumerate() {
            let v = if k == 0 {
                mul(b, p)?
            } else {
                let ps = *k
                    .checked_sub(1)
                    .and_then(|m| self.price_scale.get(m))
                    .ok_or(RouteError::BadLeg)?;
                udiv_unsafe(mul(mul(b, ps)?, p)?, e18)
            };
            xp.push(v);
        }
        let y0 = match self.kind {
            CryptoKind::TwoV1 => {
                // The pool's own `newton_y` also bounds the result.
                let y = two_newton_y(self.ann, self.gamma, &xp, self.d, j, e(20), false)?;
                let frac = udiv(mul(y, e(18))?, self.d)?;
                if !(frac > sub(e(16), U256::ONE)? && frac < add(e(20), U256::ONE)?) {
                    return Err(RouteError::Math);
                }
                y
            }
            CryptoKind::TwoV200 => two_get_y(self.ann, self.gamma, &xp, self.d, j, false)?,
            CryptoKind::TwoV210 => two_get_y(self.ann, self.gamma, &xp, self.d, j, true)?,
            CryptoKind::Tri => tri_get_y(self.ann, self.gamma, &xp, self.d, j)?,
        };
        let xj = *xp.get(j).ok_or(RouteError::BadLeg)?;
        let mut dy = sub(xj, y0)?;
        *xp.get_mut(j).ok_or(RouteError::BadLeg)? = sub(xj, dy)?;
        dy = sub(dy, U256::ONE)?;
        if j > 0 {
            let ps = *j
                .checked_sub(1)
                .and_then(|m| self.price_scale.get(m))
                .ok_or(RouteError::BadLeg)?;
            dy = udiv(mul(dy, e18)?, ps)?;
        }
        dy = udiv(dy, *self.precisions.get(j).ok_or(RouteError::BadLeg)?)?;
        let f = if self.kind == CryptoKind::Tri {
            tri_fee(&xp, self.mid_fee, self.out_fee, self.fee_gamma)?
        } else {
            two_fee(&xp, self.mid_fee, self.out_fee, self.fee_gamma)?
        };
        let fee = udiv_unsafe(mul(f, dy)?, e(10));
        sub(dy, fee)
    }

    /// `ρ(x)`: `sqrt` of the marginal output per input after `x` of coin
    /// `i`, in Q96 raw units, by a forward difference of the exact output.
    /// Crypto curves have no closed-form derivative in integers. The step is
    /// 1e-5 of coin `i`'s balance (at least 1); at `x = 0` the difference is
    /// taken between `h` and `2h`, because `exchange` subtracts a fixed wei
    /// that `dy(0) = 0` does not.
    pub fn rho(&self, i: u8, j: u8, x: U256) -> R<U256> {
        let bi = *self
            .balances
            .get(usize::from(i))
            .ok_or(RouteError::BadLeg)?;
        let h = udiv_unsafe(bi, U256::from(100_000u64)).max(U256::ONE);
        let a = if x.is_zero() { h } else { x };
        let q0 = self.dy(i, j, a)?;
        let q1 = self.dy(i, j, add(a, h)?)?;
        let dq = q1.saturating_sub(q0);
        let q = U512::from(dq)
            .checked_mul(U512::from(Q192))
            .ok_or(RouteError::Math)?
            .checked_div(U512::from(h))
            .ok_or(RouteError::Math)?;
        narrow(q.root(2))
    }
}

// ───────────────────────────── helpers ─────────────────────────────

#[inline]
fn e(p: u32) -> U256 {
    U256::from(10u64).pow(U256::from(p))
}

#[inline]
fn add(a: U256, b: U256) -> R<U256> {
    a.checked_add(b).ok_or(RouteError::Math)
}

#[inline]
fn sub(a: U256, b: U256) -> R<U256> {
    a.checked_sub(b).ok_or(RouteError::Math)
}

#[inline]
fn mul(a: U256, b: U256) -> R<U256> {
    a.checked_mul(b).ok_or(RouteError::Math)
}

#[inline]
fn udiv(a: U256, b: U256) -> R<U256> {
    a.checked_div(b).ok_or(RouteError::Math)
}

#[inline]
fn udiv_unsafe(a: U256, b: U256) -> U256 {
    a.checked_div(b).unwrap_or(U256::ZERO)
}

/// `convert(uint256, int256)`.
#[inline]
fn si(v: U256) -> R<I256> {
    I256::try_from(v).map_err(|_| RouteError::Math)
}

/// `convert(int256, uint256)`: negative reverts.
#[inline]
fn us(v: I256) -> R<U256> {
    if v.is_negative() {
        return Err(RouteError::Math);
    }
    Ok(v.into_raw())
}

#[inline]
fn ik(v: u128) -> I256 {
    I256::from_raw(U256::from(v))
}

#[inline]
fn ie(p: u32) -> I256 {
    I256::from_raw(e(p))
}

#[inline]
fn iadd(a: I256, b: I256) -> R<I256> {
    a.checked_add(b).ok_or(RouteError::Math)
}

#[inline]
fn isub(a: I256, b: I256) -> R<I256> {
    a.checked_sub(b).ok_or(RouteError::Math)
}

#[inline]
fn imul(a: I256, b: I256) -> R<I256> {
    a.checked_mul(b).ok_or(RouteError::Math)
}

/// int256 `/`: truncates toward zero, reverts on 0 and `MIN / −1`.
#[inline]
fn idiv(a: I256, b: I256) -> R<I256> {
    a.checked_div(b).ok_or(RouteError::Math)
}

/// `unsafe_div` on int256: 0 for a 0 divisor, wraps `MIN / −1`.
#[inline]
fn idiv_u(a: I256, b: I256) -> I256 {
    if b.is_zero() {
        I256::ZERO
    } else {
        a.wrapping_div(b)
    }
}

#[inline]
fn iabs(a: I256) -> R<I256> {
    a.checked_abs().ok_or(RouteError::Math)
}

#[inline]
fn isq(a: I256) -> R<I256> {
    imul(a, a)
}

/// `isqrt`: floor.
#[inline]
fn isqrt(x: U256) -> U256 {
    x.root(2)
}

const CBRT_T: U256 = alloy_primitives::uint!(115792089237316195423570985008687907853269_U256);

/// `_cbrt`: Vyper's 7-step Newton from a log2 guess, `unsafe_*` throughout.
fn cbrt(x: U256) -> U256 {
    let t18 = CBRT_T.wrapping_mul(e(18));
    let xx = if x >= t18 {
        x
    } else if x >= CBRT_T {
        x.wrapping_mul(e(18))
    } else {
        x.wrapping_mul(e(36))
    };
    let lg = if xx.is_zero() {
        0usize
    } else {
        xx.bit_len().saturating_sub(1)
    };
    let rem = lg % 3;
    let two_pow = U256::from(2u64).wrapping_pow(U256::from(lg / 3));
    let mut a = udiv_unsafe(
        two_pow.wrapping_mul(U256::from(1260u64).wrapping_pow(U256::from(rem))),
        U256::from(1000u64).wrapping_pow(U256::from(rem)),
    );
    for _ in 0..7 {
        a = udiv_unsafe(
            U256::from(2u64)
                .wrapping_mul(a)
                .wrapping_add(udiv_unsafe(xx, a.wrapping_mul(a))),
            U256::from(3u64),
        );
    }
    if x >= t18 {
        a = a.wrapping_mul(e(12));
    } else if x >= CBRT_T {
        a = a.wrapping_mul(e(6));
    }
    a
}

/// Signed `_cbrt` the way both `get_y`s take it: `±cbrt(|v|)`.
fn signed_cbrt(v: I256, zero_positive: bool) -> R<I256> {
    let pos = if zero_positive {
        !v.is_negative()
    } else {
        !v.is_negative() && !v.is_zero()
    };
    if pos {
        si(cbrt(v.into_raw()))
    } else {
        let r = si(cbrt(iabs(v)?.into_raw()))?;
        Ok(r.wrapping_neg())
    }
}

// ───────────────────────────── twocrypto ─────────────────────────────

fn two_lim_mul(gamma: U256, v210: bool) -> U256 {
    let lim = mul(U256::from(100u64), e(18)).unwrap_or(U256::MAX);
    let small = U256::from(2u64).wrapping_mul(e(16));
    if v210 && gamma > small {
        udiv_unsafe(lim.wrapping_mul(small), gamma)
    } else {
        lim
    }
}

/// Shared Newton step of `_newton_y` (both math contracts).
#[allow(clippy::too_many_arguments)]
fn newton_loop(
    ann: U256,
    gamma: U256,
    d: U256,
    mut y: U256,
    k0_i: U256,
    s_i: U256,
    conv: U256,
    n: u64,
) -> R<U256> {
    let e18 = e(18);
    let nn = U256::from(n);
    for _ in 0..255 {
        let y_prev = y;
        let k0 = udiv(mul(mul(k0_i, y)?, nn)?, d)?;
        let s = add(s_i, y)?;
        let mut g1k0 = add(gamma, e18)?;
        g1k0 = if g1k0 > k0 {
            add(sub(g1k0, k0)?, U256::ONE)?
        } else {
            add(sub(k0, g1k0)?, U256::ONE)?
        };
        // 10**18 * D / gamma * _g1k0 / gamma * _g1k0 * A_MULTIPLIER / ANN
        let mut t = udiv(mul(e18, d)?, gamma)?;
        t = udiv(mul(t, g1k0)?, gamma)?;
        let mul1 = udiv(mul(mul(t, g1k0)?, U256::from(A_MULTIPLIER))?, ann)?;
        let mul2 = add(e18, udiv(mul(mul(U256::from(2u64), e18)?, k0)?, g1k0)?)?;
        let mut yfprime = add(add(mul(e18, y)?, mul(s, mul2)?)?, mul1)?;
        let dyfprime = mul(d, mul2)?;
        if yfprime < dyfprime {
            y = udiv_unsafe(y_prev, U256::from(2u64));
            continue;
        }
        yfprime = sub(yfprime, dyfprime)?;
        let fprime = udiv(yfprime, y)?;
        let mut y_minus = udiv(mul1, fprime)?;
        let y_plus = add(
            udiv(add(yfprime, mul(e18, d)?)?, fprime)?,
            udiv(mul(y_minus, e18)?, k0)?,
        )?;
        y_minus = add(y_minus, udiv(mul(e18, s)?, fprime)?)?;
        y = if y_plus < y_minus {
            udiv_unsafe(y_prev, U256::from(2u64))
        } else {
            sub(y_plus, y_minus)?
        };
        let diff = y.abs_diff(y_prev);
        if diff < conv.max(udiv_unsafe(y, e(14))) {
            return Ok(y);
        }
    }
    Err(RouteError::Math)
}

fn two_newton_y(
    ann: U256,
    gamma: U256,
    x: &[U256],
    d: U256,
    i: usize,
    lim_mul: U256,
    v210: bool,
) -> R<U256> {
    let n = 2u64;
    let x_j = *x.get(usize::from(i == 0)).ok_or(RouteError::BadLeg)?;
    let y = udiv(mul(d, d)?, mul(x_j, U256::from(n * n))?)?;
    let k0_i = udiv(mul(mul(e(18), U256::from(n))?, x_j)?, d)?;
    let ok = if v210 {
        k0_i >= udiv_unsafe(e(36), lim_mul) && k0_i <= lim_mul
    } else {
        k0_i > sub(mul(e(16), U256::from(n))?, U256::ONE)?
            && k0_i < add(mul(e(20), U256::from(n))?, U256::ONE)?
    };
    if !ok {
        return Err(RouteError::Math);
    }
    let conv = udiv_unsafe(x_j, e(14))
        .max(udiv_unsafe(d, e(14)))
        .max(U256::from(100u64));
    newton_loop(ann, gamma, d, y, k0_i, x_j, conv, n)
}

fn two_get_y(ann_u: U256, gamma_u: U256, x: &[U256], d_u: U256, i: usize, v210: bool) -> R<U256> {
    let n = 2u64;
    let max_gamma = if v210 {
        mul(U256::from(199u64), e(15))?
    } else {
        mul(U256::from(2u64), e(15))?
    };
    // N^N · A_MULTIPLIER / 10 and · 1000, N = 2.
    let min_a = U256::from(4_000u64);
    let max_a = U256::from(40_000_000u64);
    if ann_u < min_a || ann_u > max_a || gamma_u < e(10) || gamma_u > max_gamma {
        return Err(RouteError::Math);
    }
    if d_u < e(17) || d_u > mul(e(15), e(18))? {
        return Err(RouteError::Math);
    }
    let lim_mul = two_lim_mul(gamma_u, v210);
    let lim_s = si(lim_mul)?;
    let (ann, gamma, d) = (si(ann_u)?, si(gamma_u)?, si(d_u)?);
    let x_j_u = *x.get(usize::from(i == 0)).ok_or(RouteError::BadLeg)?;
    let x_j = si(x_j_u)?;
    let gamma2 = gamma.wrapping_mul(gamma);
    let _y = idiv(isq(d)?, imul(x_j, ik(u128::from(n * n)))?)?;
    let k0_i = idiv_u(imul(imul(ie(18), ik(u128::from(n)))?, x_j)?, d);
    let ok = if v210 {
        k0_i >= idiv_u(ie(36), lim_s) && k0_i <= lim_s
    } else {
        // 10**16 · N − 1 < K0_i < 10**20 · N + 1, N = 2.
        k0_i > ik(19_999_999_999_999_999) && k0_i < ik(200_000_000_000_000_000_001)
    };
    if !ok {
        return Err(RouteError::Math);
    }
    let ann_gamma2 = imul(ann, gamma2)?;
    let mut a = ie(32);
    let four_e8 = ik(400_000_000);
    let mut b = isub(
        isub(
            idiv(idiv(imul(d, ann_gamma2)?, four_e8)?, x_j)?,
            ie(32).wrapping_mul(ik(3)),
        )?,
        ik(2).wrapping_mul(gamma).wrapping_mul(ie(14)),
    )?;
    let mut c = isub(
        iadd(
            iadd(
                iadd(
                    ie(32).wrapping_mul(ik(3)),
                    ik(4).wrapping_mul(gamma).wrapping_mul(ie(14)),
                )?,
                idiv_u(gamma2, ie(4)),
            )?,
            idiv_u(
                imul(idiv_u(ik(4).wrapping_mul(ann_gamma2), four_e8), x_j)?,
                d,
            ),
        )?,
        idiv_u(ik(4).wrapping_mul(ann_gamma2), four_e8),
    )?;
    let mut dd = idiv_u(isq(ie(18).wrapping_add(gamma))?, ie(4)).wrapping_neg();
    let delta0 = isub(idiv(imul(imul(ik(3), a)?, c)?, b)?, b)?;
    let delta1 = isub(
        iadd(imul(ik(3), delta0)?, b)?,
        idiv(imul(idiv(imul(ik(27), isq(a)?)?, b)?, dd)?, b)?,
    )?;
    let threshold = iabs(delta0)?.min(iabs(delta1)?).min(a);
    let mut divider = ik(1);
    for (lim, dv) in [
        (48u32, 30u32),
        (46, 28),
        (44, 26),
        (42, 24),
        (40, 22),
        (38, 20),
        (36, 18),
        (34, 16),
        (32, 14),
        (30, 12),
        (28, 10),
        (26, 8),
        (24, 6),
        (20, 2),
    ] {
        if threshold > ie(lim) {
            divider = ie(dv);
            break;
        }
    }
    a = idiv_u(a, divider);
    b = idiv_u(b, divider);
    c = idiv_u(c, divider);
    dd = idiv_u(dd, divider);
    let delta0 = isub(idiv_u(ik(3).wrapping_mul(a).wrapping_mul(c), b), b)?;
    let delta1 = isub(
        iadd(imul(ik(3), delta0)?, b)?,
        idiv_u(idiv_u(ik(27).wrapping_mul(isq(a)?), b).wrapping_mul(dd), b),
    )?;
    let sqrt_arg = iadd(
        isq(delta1)?,
        idiv_u(imul(ik(4), isq(delta0)?)?, b).wrapping_mul(delta0),
    )?;
    if sqrt_arg <= I256::ZERO {
        return two_newton_y(ann_u, gamma_u, x, d_u, i, lim_mul, v210);
    }
    let sqrt_val = si(isqrt(sqrt_arg.into_raw()))?;
    let b_cbrt = signed_cbrt(b, false)?;
    let second_cbrt = if delta1 > I256::ZERO {
        si(cbrt(udiv_unsafe(
            delta1.wrapping_add(sqrt_val).into_raw(),
            U256::from(2u64),
        )))?
    } else {
        let v = us(sqrt_val.wrapping_sub(delta1))?;
        si(cbrt(udiv_unsafe(v, U256::from(2u64))))?.wrapping_neg()
    };
    let c1 = idiv_u(
        idiv_u(isq(b_cbrt)?, ie(18)).wrapping_mul(second_cbrt),
        ie(18),
    );
    let root = idiv(
        isub(
            isub(ie(18).wrapping_mul(c1), ie(18).wrapping_mul(b))?,
            imul(idiv(ie(18).wrapping_mul(b), c1)?, delta0)?,
        )?,
        ik(3).wrapping_mul(a),
    )?;
    let y0 = idiv_u(
        idiv_u(idiv_u(isq(d)?, x_j).wrapping_mul(root), ik(4)),
        ie(18),
    );
    let y = us(y0)?;
    us(root)?;
    let frac = udiv_unsafe(mul(y, e(18))?, d_u);
    let ok = if v210 {
        frac >= udiv_unsafe(udiv_unsafe(e(36), U256::from(n)), lim_mul)
            && frac <= udiv_unsafe(lim_mul, U256::from(n))
    } else {
        frac >= sub(e(16), U256::ONE)? && frac < add(e(20), U256::ONE)?
    };
    if !ok {
        return Err(RouteError::Math);
    }
    Ok(y)
}

fn two_fee(xp: &[U256], mid_fee: U256, out_fee: U256, fee_gamma: U256) -> R<U256> {
    let (x0, x1) = (
        *xp.first().ok_or(RouteError::BadLeg)?,
        *xp.get(1).ok_or(RouteError::BadLeg)?,
    );
    let e18 = e(18);
    let s = add(x0, x1)?;
    let k = udiv(mul(udiv(mul(mul(e18, U256::from(4u64))?, x0)?, s)?, x1)?, s)?;
    let f = udiv(mul(fee_gamma, e18)?, sub(add(fee_gamma, e18)?, k)?)?;
    Ok(udiv_unsafe(
        add(mul(mid_fee, f)?, mul(out_fee, sub(e18, f)?)?)?,
        e18,
    ))
}

// ───────────────────────────── tricrypto ─────────────────────────────

fn tri_newton_y(ann: U256, gamma: U256, x: &[U256], d: U256, i: usize) -> R<U256> {
    let n = 3u64;
    let e18 = e(18);
    let lo = sub(e(16), U256::ONE)?;
    let hi = add(e(20), U256::ONE)?;
    for (k, &xk) in x.iter().enumerate() {
        if k != i {
            let frac = udiv(mul(xk, e18)?, d)?;
            if !(frac > lo && frac < hi) {
                return Err(RouteError::Math);
            }
        }
    }
    let mut y = udiv_unsafe(d, U256::from(n));
    let mut k0_i = e18;
    let mut s_i = U256::ZERO;
    let mut xs: SmallVec<[U256; MAX_COINS]> = x.iter().copied().collect();
    *xs.get_mut(i).ok_or(RouteError::BadLeg)? = U256::ZERO;
    xs.sort_unstable_by(|a, b| b.cmp(a));
    let x_hi = *xs.first().ok_or(RouteError::BadLeg)?;
    let conv = udiv_unsafe(x_hi, e(14))
        .max(udiv_unsafe(d, e(14)))
        .max(U256::from(100u64));
    for jj in 2..=3usize {
        let xv = *3usize
            .checked_sub(jj)
            .and_then(|m| xs.get(m))
            .ok_or(RouteError::BadLeg)?;
        y = udiv(mul(y, d)?, mul(xv, U256::from(n))?)?;
        s_i = add(s_i, xv)?;
    }
    for jj in 0..2usize {
        let xv = *xs.get(jj).ok_or(RouteError::BadLeg)?;
        k0_i = udiv(mul(mul(k0_i, xv)?, U256::from(n))?, d)?;
    }
    let y = newton_loop(ann, gamma, d, y, k0_i, s_i, conv, n)?;
    let frac = udiv(mul(y, e18)?, d)?;
    if !(frac > lo && frac < hi) {
        return Err(RouteError::Math);
    }
    Ok(y)
}

fn tri_get_y(ann_u: U256, gamma_u: U256, x: &[U256], d_u: U256, i: usize) -> R<U256> {
    let n = 3u64;
    // N^N · A_MULTIPLIER / 100 and · 1000, N = 3.
    let min_a = U256::from(2_700u64);
    let max_a = U256::from(270_000_000u64);
    if ann_u < min_a || ann_u > max_a || gamma_u < e(10) || gamma_u > mul(U256::from(5u64), e(16))?
    {
        return Err(RouteError::Math);
    }
    if d_u < e(17) || d_u > mul(e(15), e(18))? {
        return Err(RouteError::Math);
    }
    let lo = sub(e(16), U256::ONE)?;
    let hi = add(e(20), U256::ONE)?;
    for (k, &xk) in x.iter().enumerate() {
        if k != i {
            let frac = udiv(mul(xk, e(18))?, d_u)?;
            if !(frac > lo && frac < hi) {
                return Err(RouteError::Math);
            }
        }
    }
    let (j, k) = match i {
        0 => (1usize, 2usize),
        1 => (0, 2),
        2 => (0, 1),
        _ => return Err(RouteError::BadLeg),
    };
    let (ann, gamma, d) = (si(ann_u)?, si(gamma_u)?, si(d_u)?);
    let x_j = si(*x.get(j).ok_or(RouteError::BadLeg)?)?;
    let x_k = si(*x.get(k).ok_or(RouteError::BadLeg)?)?;
    let gamma2 = gamma.wrapping_mul(gamma);
    let mut a = idiv(ie(36), ik(27))?;
    let e36_9 = idiv(ie(36), ik(9))?;
    let am = ik(u128::from(A_MULTIPLIER));
    let mut b = isub(
        e36_9.wrapping_add(idiv_u(
            ik(2).wrapping_mul(ie(18)).wrapping_mul(gamma),
            ik(27),
        )),
        idiv_u(
            idiv_u(
                idiv_u(
                    imul(idiv_u(d.wrapping_mul(d), x_j).wrapping_mul(gamma2), ann)?,
                    ik(729),
                ),
                am,
            ),
            x_k,
        ),
    )?;
    let mut c = iadd(
        e36_9.wrapping_add(idiv_u(
            gamma.wrapping_mul(gamma.wrapping_add(ik(4).wrapping_mul(ie(18)))),
            ik(27),
        )),
        idiv_u(
            idiv_u(
                idiv_u(imul(gamma2, x_j.wrapping_add(x_k).wrapping_sub(d))?, d).wrapping_mul(ann),
                ik(27),
            ),
            am,
        ),
    )?;
    let mut dd = idiv_u(isq(ie(18).wrapping_add(gamma))?, ik(27));
    let d0 = iabs(isub(idiv(imul(ik(3).wrapping_mul(a), c)?, b)?, b)?)?;
    let mut divider = ik(1);
    for (lim, dv) in [
        (48u32, 30u32),
        (44, 26),
        (40, 22),
        (36, 18),
        (32, 14),
        (28, 10),
        (24, 6),
        (20, 2),
    ] {
        if d0 > ie(lim) {
            divider = ie(dv);
            break;
        }
    }
    if iabs(a)? > iabs(b)? {
        let ap = iabs(idiv_u(a, b))?;
        a = idiv_u(a.wrapping_mul(ap), divider);
        b = idiv_u(imul(b, ap)?, divider);
        c = idiv_u(imul(c, ap)?, divider);
        dd = idiv_u(imul(dd, ap)?, divider);
    } else {
        let ap = iabs(idiv_u(b, a))?;
        a = idiv_u(idiv(a, ap)?, divider);
        b = idiv_u(idiv_u(b, ap), divider);
        c = idiv_u(idiv_u(c, ap), divider);
        dd = idiv_u(idiv_u(dd, ap), divider);
    }
    let three_ac = imul(ik(3).wrapping_mul(a), c)?;
    let delta0 = isub(idiv_u(three_ac, b), b)?;
    let delta1 = isub(
        isub(idiv_u(imul(ik(3), three_ac)?, b), ik(2).wrapping_mul(b))?,
        idiv_u(imul(idiv_u(imul(ik(27), isq(a)?)?, b), dd)?, b),
    )?;
    let sqrt_arg = iadd(
        isq(delta1)?,
        imul(idiv_u(imul(ik(4), isq(delta0)?)?, b), delta0)?,
    )?;
    if sqrt_arg <= I256::ZERO {
        return tri_newton_y(ann_u, gamma_u, x, d_u, i);
    }
    let sqrt_val = si(isqrt(sqrt_arg.into_raw()))?;
    let b_cbrt = signed_cbrt(b, true)?;
    let second_cbrt = if delta1 > I256::ZERO {
        si(cbrt(udiv_unsafe(
            us(iadd(delta1, sqrt_val)?)?,
            U256::from(2u64),
        )))?
    } else {
        let v = us(isub(delta1, sqrt_val)?
            .checked_neg()
            .ok_or(RouteError::Math)?)?;
        si(cbrt(udiv_unsafe(v, U256::from(2u64))))?.wrapping_neg()
    };
    let c1 = idiv_u(
        imul(idiv_u(imul(b_cbrt, b_cbrt)?, ie(18)), second_cbrt)?,
        ie(18),
    );
    let root_k0 = idiv_u(isub(iadd(b, idiv(imul(b, delta0)?, c1)?)?, c1)?, ik(3));
    let root = idiv_u(
        imul(
            idiv_u(imul(idiv_u(idiv_u(imul(d, d)?, ik(27)), x_k), d)?, x_j),
            root_k0,
        )?,
        a,
    );
    let y = us(root)?;
    us(idiv_u(imul(ie(18), root_k0)?, a))?;
    let frac = udiv_unsafe(mul(y, e(18))?, d_u);
    if !(frac >= lo && frac < hi) {
        return Err(RouteError::Math);
    }
    let _ = n;
    Ok(y)
}

fn tri_fee(xp: &[U256], mid_fee: U256, out_fee: U256, fee_gamma: U256) -> R<U256> {
    let e18 = e(18);
    let three = U256::from(3u64);
    let (x0, x1, x2) = (
        *xp.first().ok_or(RouteError::BadLeg)?,
        *xp.get(1).ok_or(RouteError::BadLeg)?,
        *xp.get(2).ok_or(RouteError::BadLeg)?,
    );
    let s = add(add(x0, x1)?, x2)?;
    let mut k = udiv(mul(mul(e18, three)?, x0)?, s)?;
    k = udiv_unsafe(mul(mul(k, three)?, x1)?, s);
    k = udiv_unsafe(mul(mul(k, three)?, x2)?, s);
    if !fee_gamma.is_zero() {
        k = udiv(mul(fee_gamma, e18)?, sub(add(fee_gamma, e18)?, k)?)?;
    }
    Ok(udiv_unsafe(
        add(mul(mid_fee, k)?, mul(out_fee, sub(e18, k)?)?)?,
        e18,
    ))
}
