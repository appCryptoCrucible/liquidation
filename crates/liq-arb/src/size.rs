//! The anchor amount: the maximum of a concave profit, on integers.
//!
//! A backrun's profit in its input amount is concave (every leg's output is
//! concave and increasing in its input, and a water-filled exit keeps that),
//! but not smooth: ticks, Curve's integer Newton and pools joining the
//! water-fill all put kinks in it. Brent's method suits that: a parabola
//! through the three best points when it lands sensibly inside the bracket
//! (superlinear on the smooth stretches), a golden-section step when it
//! does not (always converges). Each evaluation is an exit solve, so the
//! count is what is minimised.
//!
//! Everything is integer (the workspace denies float arithmetic): the
//! bracket is mapped onto `t ∈ [0, 2³²]`, where the parabola's products of
//! position and value stay inside `I256`; any product that would not falls
//! back to the golden step.

use alloy_primitives::{I256, U256};
use smallvec::SmallVec;

/// Bracket resolution: positions are `t ∈ [0, T]`.
const T: i128 = 1 << 32;
/// Stop when the optimum is known to within this many `t` steps: a ten-
/// thousandth of the bracket. Brackets span a factor of at most 16
/// ([`bracket`]), so that is at most 0.16 % of the amount; profit is flat
/// to second order there.
const TOL: i128 = T / 10_000;
/// What the concavity bound may still allow and count as nothing: integer
/// rounding in a chord extended to zero (a pure fee loss is a line through
/// the origin, which extrapolates to ±a few units). 1e3 raw units is 1e-15
/// ETH, a tenth of a cent of USDC: below any floor a backrun is sent at.
const DUST: I256 = I256::from_raw(U256::from_limbs([1_000, 0, 0, 0]));
/// The golden-section fraction `(3 - √5) / 2`, in millionths.
const CGOLD_PPM: i128 = 381_966;
/// What an infeasible amount (a refused quote) is worth: far below any
/// real loss, and small enough that sums of two still fit.
/// `−2²⁵²` in two's complement (the top four bits set).
const INFEASIBLE: I256 = I256::from_raw(U256::from_limbs([0, 0, 0, 0xF000_0000_0000_0000]));

/// The best amount found and what it is worth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Maximum {
    pub x: U256,
    pub value: I256,
    /// Evaluations spent, bracketing included.
    pub evals: u16,
}

/// Maximise `f` near `scale`: [`bracket`] the peak by factors of four from
/// `scale / 64`, then [`brent`] inside the bracket. `None` when no amount
/// tried is worth more than zero, or the budget runs out first.
pub fn maximise<F: FnMut(U256) -> Option<I256>>(
    scale: U256,
    max_evals: u16,
    mut f: F,
) -> Option<Maximum> {
    let (lo, hi, best, used) = bracket(scale, max_evals, &mut f)?;
    let rest = max_evals.saturating_sub(used);
    let m = brent(lo, hi, rest, &mut f);
    let out = match m {
        Some(m) if m.value > best.value => Maximum {
            evals: m.evals.saturating_add(used),
            ..m
        },
        Some(m) => Maximum {
            evals: m.evals.saturating_add(used),
            ..best
        },
        None => Maximum {
            evals: used,
            ..best
        },
    };
    (out.value > I256::ZERO).then_some(out)
}

/// Probes taken while bracketing, ascending by amount.
type Probes = SmallVec<[(U256, I256); 24]>;

fn insert(probes: &mut Probes, x: U256, v: I256) {
    let at = probes
        .iter()
        .position(|&(px, _)| px > x)
        .unwrap_or(probes.len());
    probes.insert(at, (x, v));
}

/// The value at `x` of the line through `(a, fa)` and `(b, fb)`; `None` on
/// overflow.
fn line_at(a: U256, fa: I256, b: U256, fb: I256, x: U256) -> Option<I256> {
    let ia = I256::try_from(a).ok()?;
    let ib = I256::try_from(b).ok()?;
    let ix = I256::try_from(x).ok()?;
    let run = ib.checked_sub(ia)?;
    if run.is_zero() {
        return None;
    }
    let rise = fb.checked_sub(fa)?;
    fa.checked_add(rise.checked_mul(ix.checked_sub(ia)?)?.checked_div(run)?)
}

/// An upper bound on `f` over every amount, from the probes, by concavity:
/// a chord between two probes, extended past them, lies above `f`. Each gap
/// between probes is bounded by the chord next to it extended into it, the
/// stretch below the smallest probe by the first chord extended down to 0,
/// and the stretch above the largest by the last chord extended up (when it
/// falls; an infeasible probe ends the range). `None` when some stretch has
/// no bound (or a product overflows): nothing is proven.
fn upper_bound(probes: &Probes) -> Option<I256> {
    let feasible: SmallVec<[(U256, I256); 24]> = probes
        .iter()
        .copied()
        .filter(|&(_, v)| v > INFEASIBLE)
        .collect();
    let n = feasible.len();
    if n < 2 {
        return None;
    }
    let ends_infeasible = probes.last().is_some_and(|&(_, v)| v <= INFEASIBLE);
    let mut ub = feasible.iter().map(|&(_, v)| v).max()?;
    let p = |k: usize| feasible.get(k).copied();
    // Below the smallest probe: the first chord extended to 0.
    let (x0, f0) = p(0)?;
    let (x1, f1) = p(1)?;
    ub = ub.max(line_at(x0, f0, x1, f1, U256::ZERO)?);
    // Each gap [x_k, x_{k+1}]: the tighter of the chord on its left
    // extended right and the chord on its right extended left.
    for k in 0..n.saturating_sub(1) {
        let (a, fa) = p(k)?;
        let (b, fb) = p(k.saturating_add(1))?;
        let from_left = (k >= 1)
            .then(|| k.checked_sub(1).and_then(p))
            .flatten()
            .and_then(|(z, fz)| line_at(z, fz, a, fa, b))
            .map(|e| e.max(fa));
        let from_right = p(k.saturating_add(2))
            .and_then(|(c, fc)| line_at(b, fb, c, fc, a))
            .map(|e| e.max(fb));
        let gap = match (from_left, from_right) {
            (Some(l), Some(r)) => l.min(r),
            (Some(l), None) => l,
            (None, Some(r)) => r,
            (None, None) => return None,
        };
        ub = ub.max(gap);
    }
    // Above the largest feasible probe.
    let (y, fy) = p(n.checked_sub(1)?)?;
    let (w, fw) = p(n.checked_sub(2)?)?;
    if fy > fw {
        // Still rising: unbounded, unless an infeasible probe ends the range.
        if !ends_infeasible {
            return None;
        }
        let (end, _) = *probes.last()?;
        ub = ub.max(line_at(w, fw, y, fy, end)?);
    }
    Some(ub)
}

/// Probe `scale / 64 · 4^k` upward until the value falls, then downward
/// from the smallest probe if that was the best, until the peak is
/// bracketed. Stops as soon as the probes prove by concavity that nothing
/// is worth more than zero ([`upper_bound`]): an unprofitable victim costs a
/// few evaluations, not the whole budget. Returns the bracket around the
/// best probe, that probe, and the evaluations used.
fn bracket<F: FnMut(U256) -> Option<I256>>(
    scale: U256,
    max_evals: u16,
    f: &mut F,
) -> Option<(U256, U256, Maximum, u16)> {
    let four = U256::from(4u8);
    let start = scale
        .checked_div(U256::from(64u8))
        .unwrap_or_default()
        .max(U256::from(1u8));
    let mut evals = 0u16;
    let mut probes: Probes = SmallVec::new();
    let nothing_pays = |probes: &Probes| upper_bound(probes).is_some_and(|ub| ub <= DUST);
    let best_of = |probes: &Probes| probes.iter().copied().max_by_key(|&(_, v)| v);
    // Upward until the value falls below the best.
    let mut x = start;
    while evals < max_evals {
        let v = f(x).unwrap_or(INFEASIBLE);
        evals = evals.saturating_add(1);
        insert(&mut probes, x, v);
        if nothing_pays(&probes) {
            return None;
        }
        let (bx, _) = best_of(&probes)?;
        if bx < x {
            break;
        }
        let Some(next) = x.checked_mul(four) else {
            break;
        };
        x = next;
    }
    // The smallest probe is the best: the peak may lie lower still.
    let mut x = start;
    while evals < max_evals && best_of(&probes).is_some_and(|(bx, _)| bx == x) {
        x = x.checked_div(four).unwrap_or_default();
        if x.is_zero() {
            break;
        }
        let v = f(x).unwrap_or(INFEASIBLE);
        evals = evals.saturating_add(1);
        insert(&mut probes, x, v);
        if nothing_pays(&probes) {
            return None;
        }
    }
    let (bx, bv) = best_of(&probes)?;
    if bv <= INFEASIBLE {
        return None;
    }
    let k = probes.iter().position(|&(px, _)| px == bx)?;
    let lo = k
        .checked_sub(1)
        .and_then(|j| probes.get(j))
        .map_or_else(|| bx.checked_div(four).unwrap_or_default(), |&(px, _)| px);
    let hi = probes
        .get(k.saturating_add(1))
        .map_or_else(|| bx.saturating_mul(four), |&(px, _)| px);
    Some((
        lo,
        hi,
        Maximum {
            x: bx,
            value: bv,
            evals: 0,
        },
        evals,
    ))
}

/// `lo + (hi − lo) · t / T`.
fn at(lo: U256, w: U256, t: i128) -> Option<U256> {
    let t = U256::from(u64::try_from(t).ok()?);
    let step = w
        .checked_mul(t)?
        .checked_div(U256::from(u64::try_from(T).ok()?))?;
    lo.checked_add(step)
}

fn i(v: i128) -> I256 {
    I256::try_from(v).unwrap_or(I256::ZERO)
}

/// The parabola step of Brent's method, `None` when any product would not
/// fit (the caller then takes a golden step). Minimises `g` (= −profit).
#[allow(clippy::too_many_arguments)]
fn parabola(x: i128, w: i128, v: i128, gx: I256, gw: I256, gv: I256) -> Option<(I256, I256)> {
    let r = i(x.checked_sub(w)?).checked_mul(gx.checked_sub(gv)?)?;
    let q = i(x.checked_sub(v)?).checked_mul(gx.checked_sub(gw)?)?;
    let p = i(x.checked_sub(v)?)
        .checked_mul(q)?
        .checked_sub(i(x.checked_sub(w)?).checked_mul(r)?)?;
    let q = q.checked_sub(r)?.checked_mul(I256::try_from(2i8).ok()?)?;
    Some((p, q))
}

/// Brent's method on `[lo, hi]`, at most `max_evals` evaluations.
#[allow(clippy::arithmetic_side_effects)] // t-space arithmetic is bounded by T = 2^32 in i128
pub fn brent<F: FnMut(U256) -> Option<I256>>(
    lo: U256,
    hi: U256,
    max_evals: u16,
    f: &mut F,
) -> Option<Maximum> {
    if hi <= lo || max_evals == 0 {
        return None;
    }
    let w_span = hi - lo;
    let mut evals = 0u16;
    let mut g = |t: i128, evals: &mut u16| -> I256 {
        *evals = evals.saturating_add(1);
        match at(lo, w_span, t).and_then(&mut *f) {
            Some(v) => -v,
            None => -INFEASIBLE,
        }
    };
    let (mut a, mut b) = (0i128, T);
    let mut x = CGOLD_PPM * T / 1_000_000;
    let (mut w, mut v) = (x, x);
    let mut gx = g(x, &mut evals);
    let (mut gw, mut gv) = (gx, gx);
    let (mut d, mut e) = (0i128, 0i128);
    while evals < max_evals {
        let m = (a + b) / 2;
        if (x - m).abs() <= 2 * TOL - (b - a) / 2 {
            break;
        }
        let golden_e = if x >= m { a - x } else { b - x };
        let mut took_parabola = false;
        if e.abs() > TOL {
            if let Some((mut p, mut q)) = parabola(x, w, v, gx, gw, gv) {
                if q > I256::ZERO {
                    p = -p;
                }
                q = q.abs();
                let etemp = e;
                let half_qe = q.checked_mul(i(etemp)).map(|z| z.abs());
                let lo_ok = q.checked_mul(i(a - x)).is_some_and(|z| p > z);
                let hi_ok = q.checked_mul(i(b - x)).is_some_and(|z| p < z);
                let small = half_qe.is_some_and(|h| {
                    p.abs()
                        .checked_mul(I256::try_from(2i8).unwrap_or(I256::ONE))
                        .is_some_and(|pp| pp < h)
                });
                if small && lo_ok && hi_ok && !q.is_zero() {
                    if let Ok(step) = i128::try_from(p / q) {
                        e = d;
                        d = step;
                        let u = x + d;
                        if u - a < 2 * TOL || b - u < 2 * TOL {
                            d = if m > x { TOL } else { -TOL };
                        }
                        took_parabola = true;
                    }
                }
            }
        }
        if !took_parabola {
            e = golden_e;
            d = CGOLD_PPM * e / 1_000_000;
        }
        let u = if d.abs() >= TOL {
            x + d
        } else if d >= 0 {
            x + TOL
        } else {
            x - TOL
        };
        let u = u.clamp(a, b);
        let gu = g(u, &mut evals);
        if gu <= gx {
            if u >= x {
                a = x;
            } else {
                b = x;
            }
            v = w;
            gv = gw;
            w = x;
            gw = gx;
            x = u;
            gx = gu;
        } else {
            if u < x {
                a = u;
            } else {
                b = u;
            }
            if gu <= gw || w == x {
                v = w;
                gv = gw;
                w = u;
                gw = gu;
            } else if gu <= gv || v == x || v == w {
                v = u;
                gv = gu;
            }
        }
    }
    Some(Maximum {
        x: at(lo, w_span, x)?,
        value: -gx,
        evals,
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    /// A two-pool V2 cycle on integers: what the contracts compute, so its
    /// argmax is a real backrun's, with kinks from flooring.
    fn v2_cycle(x: U256, r: [u128; 4]) -> Option<I256> {
        let out = |amt: U256, rin: u128, rout: u128| -> U256 {
            let a = amt * U256::from(997u32);
            a * U256::from(rout) / (U256::from(rin) * U256::from(1000u32) + a)
        };
        let mid = out(x, r[0], r[1]);
        let back = out(mid, r[2], r[3]);
        Some(I256::try_from(back).ok()? - I256::try_from(x).ok()?)
    }

    /// Oracle: exhaustive scan of every amount at a step fine enough that
    /// the true peak lies between two scanned points.
    fn scan(lo: u128, hi: u128, step: u128, r: [u128; 4]) -> (u128, I256) {
        let mut best = (lo, I256::MIN);
        let mut x = lo;
        while x <= hi {
            let v = v2_cycle(U256::from(x), r).unwrap();
            if v > best.1 {
                best = (x, v);
            }
            x += step;
        }
        best
    }

    #[test]
    fn finds_the_v2_cycle_peak_the_scan_finds() {
        // Pool B pays 2 % more for the token than pool A charges.
        let r = [
            1_000_000_000_000_000_000_000u128,
            1_000_000_000_000_000_000_000,
            1_000_000_000_000_000_000_000,
            1_020_000_000_000_000_000_000,
        ];
        let (sx, sv) = scan(
            1_000_000_000_000_000,
            20_000_000_000_000_000_000,
            1_000_000_000_000_000,
            r,
        );
        let m = maximise(U256::from(sx), 40, |x| v2_cycle(x, r)).unwrap();
        // Within the scan's step of the peak, and worth at least as much as
        // the scan's best point to within that step's second-order loss.
        let got = u128::try_from(m.x).unwrap();
        assert!(got.abs_diff(sx) <= sx / 500, "x {got} vs scan {sx}");
        assert!(
            m.value >= sv - I256::try_from(1_000_000_000_000u64).unwrap(),
            "value {} vs scan {sv}",
            m.value
        );
        assert!(m.evals <= 14, "evals {}", m.evals);
    }

    #[test]
    fn scale_far_from_the_peak_still_brackets_it() {
        let r = [
            1_000_000_000_000_000_000_000u128,
            1_000_000_000_000_000_000_000,
            1_000_000_000_000_000_000_000,
            1_020_000_000_000_000_000_000,
        ];
        let (sx, _) = scan(
            1_000_000_000_000_000,
            20_000_000_000_000_000_000,
            1_000_000_000_000_000,
            r,
        );
        for scale in [
            U256::from(sx) / U256::from(1000u32),
            U256::from(sx) * U256::from(50u32),
        ] {
            let m = maximise(scale, 60, |x| v2_cycle(x, r)).unwrap();
            let got = u128::try_from(m.x).unwrap();
            assert!(
                got.abs_diff(sx) <= sx / 500,
                "scale {scale}: x {got} vs scan {sx}"
            );
        }
    }

    #[test]
    fn no_profitable_amount_is_none() {
        // Same price both pools: every round trip loses the two fees.
        let r = [1_000_000_000_000_000_000_000u128; 4];
        assert!(
            maximise(U256::from(1_000_000_000_000_000_000u128), 40, |x| v2_cycle(
                x, r
            ))
            .is_none()
        );
    }

    /// Nothing pays: the probes prove it by concavity within a few
    /// evaluations, instead of doubling to the budget. Every real objective
    /// carries the backrun's gas, so it starts a gas cost below zero (a
    /// fee-only loss with no gas would have zero as its supremum, at zero
    /// size, which no finite probe set can prove away). Level pools (every
    /// size loses the fees) at a 0.001 ETH gas cost, and a 0.1 % edge that
    /// a gas cost larger than its best gross swallows.
    #[test]
    fn unprofitable_stops_early() {
        let level = [1_000_000_000_000_000_000_000u128; 4];
        let small_gas = I256::try_from(1_000_000_000_000_000u128).unwrap();
        let mut n = 0u32;
        let r = maximise(U256::from(1_000_000_000_000_000_000u128), 40, |x| {
            n += 1;
            v2_cycle(x, level).map(|v| v - small_gas)
        });
        assert!(r.is_none());
        assert!(n <= 4, "level pools: {n} evaluations");

        // 0.7 % gap less 0.6 % fees: a little gross at best, under 1e17 of gas.
        let thin = [
            1_000_000_000_000_000_000_000u128,
            1_000_000_000_000_000_000_000,
            1_000_000_000_000_000_000_000,
            1_007_000_000_000_000_000_000,
        ];
        let gas = I256::try_from(100_000_000_000_000_000u128).unwrap();
        let best_gross = (1..2000u128)
            .map(|k| v2_cycle(U256::from(k) * U256::from(1_000_000_000_000_000u128), thin).unwrap())
            .max()
            .unwrap();
        assert!(
            best_gross > I256::ZERO && best_gross < gas,
            "fixture: gross {best_gross} must be positive and under the gas"
        );
        let mut n = 0u32;
        let r = maximise(U256::from(1_000_000_000_000_000_000u128), 40, |x| {
            n += 1;
            v2_cycle(x, thin).map(|v| v - gas)
        });
        assert!(r.is_none());
        assert!(n <= 12, "gas-swallowed edge: {n} evaluations");
    }

    #[test]
    fn refused_amounts_do_not_break_the_search() {
        // Quotes above 2e18 refuse (a pool's tick window, say): below the
        // free peak, so the best amount sits at the cap.
        let r = [
            1_000_000_000_000_000_000_000u128,
            1_000_000_000_000_000_000_000,
            1_000_000_000_000_000_000_000,
            1_020_000_000_000_000_000_000,
        ];
        let cap = U256::from(2_000_000_000_000_000_000u128);
        let m = maximise(U256::from(1_000_000_000_000_000_000u128), 40, |x| {
            if x > cap {
                None
            } else {
                v2_cycle(x, r)
            }
        })
        .unwrap();
        assert!(m.x <= cap);
        let (sx, _) = scan(
            1_000_000_000_000_000,
            2_000_000_000_000_000_000,
            1_000_000_000_000_000,
            r,
        );
        let got = u128::try_from(m.x).unwrap();
        assert!(got.abs_diff(sx) <= sx / 200, "x {got} vs capped scan {sx}");
    }
}
