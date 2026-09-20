//! Exact tier (GUIDE 12 §4, §4c): water-fill one collateral across a pool
//! set on marginal `(fee + impact)`, choose the set greedily on hop gas,
//! and order K collaterals against the progressively displaced state.
//!
//! # Variable
//!
//! The water-fill runs in `ρ = sqrt(marginal out-per-in, after fee)` as a
//! Q96 integer. For a V3 pool `ρ = sqrtPriceX96 · sqrt(1 − f)` (zero-for-
//! one), so tick boundaries are *known points* in `ρ`-space; V2 and Curve
//! are smooth in `ρ`. `g(ρ) = Σ absorbed_i(ρ) − total` is monotone
//! non-increasing in `ρ`.
//!
//! # Method (per GUIDE 12 §4 "exploit the structure")
//!
//! 1. Each V3 leg is walked tick by tick from its current price until it
//!    alone could absorb `total` (exact `SqrtPriceMath` per segment, fee
//!    per step as `computeSwapStep` charges it). Segment edges are the
//!    breakpoints. V2 has one breakpoint (`ρ₀`); Curve has `ρ₀`.
//! 2. Binary search the sorted breakpoints for the interval bracketing
//!    the root — `O(log K)` exact evaluations.
//! 3. Inside the interval every V2/V3 term is `a/ρ − b`, so with no Curve
//!    leg `ρ* = ΣA / (ΣB + total)` in closed form. With a Curve leg the
//!    interval is smooth and monotone; a bracketed Illinois-secant with
//!    bisection fallback finds it.
//! 4. `λ` is loose (1e-6 relative). Allocations are floored, capped, and
//!    the residual goes to the best-`ρ₀` pool (spilling in rank order if
//!    that pool's depth binds). Outputs are then **exact quotes**.
//!
//! Deviation, recorded: the generic fallback is Illinois regula falsi
//! (order ≈ 1.44, bracket-preserving, bisection on stall) rather than
//! `brentq` (≈ 1.6). Brent's inverse-quadratic step needs the three-point
//! rational interpolant, which overflows 512-bit integer arithmetic at
//! Q96 × wei scale; floats are denied in this tree. Cost: ≤ 2 extra
//! evaluations at the 1e-6 tolerance. Both keep the bracket and degrade
//! to bisection, which is the property GUIDE 12 §4 requires.

use alloy_primitives::aliases::I512;
use alloy_primitives::{U256, U512};
use liq_types::AssetId;
use smallvec::SmallVec;
use uniswap_v3_math::{liquidity_math, sqrt_price_math, tick_math};

use crate::solver::{
    curve_rho, mul_div_512, narrow, next_tick_within_word, CurveState, Leg, Pool, PoolBook, PoolId,
    PoolState, RouteError, V3State, PIPS, Q192, Q96,
};

/// `1e18` wei per ETH.
const WEI_PER_ETH: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
/// Relative tolerance denominator for `λ` (GUIDE 12 §4: "1e-6 is ample").
const REL_TOL: U256 = U256::from_limbs([1_000_000, 0, 0, 0]);
/// Most permutations evaluated exhaustively (`K ≤ 4` → 24).
pub const EXHAUSTIVE_K: usize = 4;
/// Tick-walk cap per V3 leg. Beyond it the leg's depth is taken as what
/// was walked (conservative: the pool is under-, never over-used).
const MAX_SEGS: usize = 256;

/// Gas terms in the **output** token so hop gas and impact compare
/// directly (GUIDE 12 §4b "everything reduces to ETH").
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GasTerms {
    /// Exact next base fee (wei / gas), from [`crate::band::next_base_fee`].
    pub base_fee_wei: u128,
    /// Raw output-token units per `1e18` wei (oracle, `min(spot, twa)`).
    pub out_per_eth: U256,
}

impl GasTerms {
    /// `gas · base_fee · out_per_eth / 1e18`, floor.
    pub fn cost_in_out(&self, gas: u64) -> Result<U256, RouteError> {
        let wei = U256::from(gas)
            .checked_mul(U256::from(self.base_fee_wei))
            .ok_or(RouteError::Math)?;
        mul_div_512(wei, self.out_per_eth, WEI_PER_ETH)
    }
}

/// Hard caps on the solve. Iteration caps are refusals, not fallbacks.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SolveBudget {
    /// Most pools in one allocation.
    pub max_pools: u8,
    /// Root-finder evaluations per bracket.
    pub max_iters: u16,
}

impl Default for SolveBudget {
    fn default() -> Self {
        Self {
            max_pools: 6,
            max_iters: 64,
        }
    }
}

/// One pool's share of an exit.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Allocation {
    pub leg: Leg,
    pub amount_in: U256,
    /// Exact quote at `amount_in` against the state the solve saw.
    pub amount_out: U256,
}

/// Result of one collateral → debt exit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitQuote {
    /// Best-`ρ₀` pool first. Sums exactly to `amount_in`.
    pub allocs: SmallVec<[Allocation; 6]>,
    pub amount_in: U256,
    pub amount_out: U256,
    /// Σ `hop_gas` over pools with a non-zero allocation.
    pub hop_gas: u64,
    /// Best marginal at zero size across candidates, Q96 sqrt.
    pub rho0: U256,
}

/// Ordered K-collateral exit (GUIDE 12 §4c).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchQuote {
    /// Indices into the caller's collateral list, execution order.
    pub order: SmallVec<[u8; EXHAUSTIVE_K]>,
    /// One per collateral, in `order`.
    pub quotes: SmallVec<[ExitQuote; EXHAUSTIVE_K]>,
    pub amount_out: U256,
    pub hop_gas: u64,
}

// ───────────────────────────── per-leg models ─────────────────────────────

/// One V3 liquidity range in the swap direction.
#[derive(Copy, Clone, Debug)]
struct Seg {
    rho_hi: U256,
    rho_lo: U256,
    s_start: U256,
    s_end: U256,
    liq: u128,
    /// Gross input absorbed before this segment starts.
    cum: U256,
}

/// Inline on purpose: models live on the stack for one solve; boxing the
/// V3 segment list would put an allocation on the hot path.
#[allow(clippy::large_enum_variant)]
enum Shape<'a> {
    V3 {
        zfo: bool,
        /// `sqrt(1 − f)` in Q96.
        kf: U256,
        fee_pips: u32,
        segs: SmallVec<[Seg; 16]>,
    },
    /// `absorbed(ρ) = a / ρ − b` (`a` Q96-scaled).
    V2 {
        a: U256,
        b: U256,
    },
    Curve {
        st: &'a CurveState,
        i: u8,
        j: u8,
    },
}

struct Model<'a> {
    leg: Leg,
    pool: &'a Pool,
    rho0: U256,
    /// Most this leg will ever be asked to absorb (≤ `total`).
    cap: U256,
    shape: Shape<'a>,
}

/// Sign-safe widen: `x < 2^256 < 2^511`, so the raw bits are the value.
#[inline]
fn i512(x: U256) -> I512 {
    I512::from_raw(U512::from(x))
}

/// `i512(a) − i512(b)`.
#[inline]
fn diff(a: U256, b: U256) -> Result<I512, RouteError> {
    i512(a).checked_sub(i512(b)).ok_or(RouteError::Math)
}

/// `lo + (hi − lo) / 2`, `lo ≤ hi`.
#[inline]
fn midpoint(lo: U256, hi: U256) -> Result<U256, RouteError> {
    hi.checked_sub(lo)
        .and_then(|w| lo.checked_add(w.wrapping_shr(1)))
        .ok_or(RouteError::Math)
}

#[inline]
fn keep(fee_pips: u32) -> Result<U256, RouteError> {
    PIPS.checked_sub(fee_pips)
        .map(U256::from)
        .ok_or(RouteError::Math)
}

/// `sqrt(keep / 1e6)` in Q96.
fn kf_q96(fee_pips: u32) -> Result<U256, RouteError> {
    let sq = U512::from(keep(fee_pips)?)
        .checked_mul(U512::from(Q192))
        .and_then(|v| v.checked_div(U512::from(PIPS)))
        .ok_or(RouteError::Math)?;
    narrow(sq.root(2))
}

#[inline]
fn rho_of_s(s: U256, kf: U256, zfo: bool) -> Result<U256, RouteError> {
    if zfo {
        mul_div_512(s, kf, Q96)
    } else {
        mul_div_512(kf, Q96, s)
    }
}

#[inline]
fn s_of_rho(rho: U256, kf: U256, zfo: bool) -> Result<U256, RouteError> {
    if rho.is_zero() {
        return Err(RouteError::Math);
    }
    if zfo {
        mul_div_512(rho, Q96, kf)
    } else {
        mul_div_512(kf, Q96, rho)
    }
}

/// `computeSwapStep`'s exact-input charge to move `liq` from `from` to
/// `to`: `amountIn` rounded up plus `fee = ceil(amountIn · f / (1e6 − f))`.
fn gross_step(
    from: U256,
    to: U256,
    liq: u128,
    zfo: bool,
    fee_pips: u32,
) -> Result<U256, RouteError> {
    if liq == 0 || from == to {
        return Ok(U256::ZERO);
    }
    let amount = if zfo {
        sqrt_price_math::_get_amount_0_delta(to, from, liq, true)
    } else {
        sqrt_price_math::_get_amount_1_delta(from, to, liq, true)
    }
    .map_err(|_| RouteError::Math)?;
    let k = keep(fee_pips)?;
    let fee = uniswap_v3_math::full_math::mul_div_rounding_up(amount, U256::from(fee_pips), k)
        .map_err(|_| RouteError::Math)?;
    amount.checked_add(fee).ok_or(RouteError::Math)
}

fn build_v3<'a>(
    leg: Leg,
    pool: &'a Pool,
    st: &V3State,
    total: U256,
) -> Result<Model<'a>, RouteError> {
    if st.sqrt_price_x96.is_zero() {
        return Err(RouteError::StalePool);
    }
    let zfo = match (leg.i, leg.j) {
        (0, 1) => true,
        (1, 0) => false,
        _ => return Err(RouteError::BadLeg),
    };
    let kf = kf_q96(st.fee_pips)?;
    let limit = if zfo {
        tick_math::MIN_SQRT_RATIO.checked_add(U256::ONE)
    } else {
        tick_math::MAX_SQRT_RATIO.checked_sub(U256::ONE)
    }
    .ok_or(RouteError::Math)?;
    let mut segs: SmallVec<[Seg; 16]> = SmallVec::new();
    let (mut s, mut tick, mut liq, mut cum) =
        (st.sqrt_price_x96, st.tick, st.liquidity, U256::ZERO);
    loop {
        let (next_tick, initialized) = next_tick_within_word(&st.ticks, tick, st.tick_spacing, zfo);
        let next_tick = next_tick.clamp(tick_math::MIN_TICK, tick_math::MAX_TICK);
        let s_next = tick_math::get_sqrt_ratio_at_tick(next_tick).map_err(|_| RouteError::Math)?;
        let target = if (zfo && s_next < limit) || (!zfo && s_next > limit) {
            limit
        } else {
            s_next
        };
        let gross = gross_step(s, target, liq, zfo, st.fee_pips)?;
        segs.push(Seg {
            rho_hi: rho_of_s(s, kf, zfo)?,
            rho_lo: rho_of_s(target, kf, zfo)?,
            s_start: s,
            s_end: target,
            liq,
            cum,
        });
        cum = cum.checked_add(gross).ok_or(RouteError::Math)?;
        if target == limit || cum >= total || segs.len() >= MAX_SEGS {
            break;
        }
        if initialized {
            let pos = st.ticks.partition_point(|t| t.tick < next_tick);
            let net = st.ticks.get(pos).map_or(0, |t| t.net);
            let net = if zfo {
                net.checked_neg().ok_or(RouteError::Math)?
            } else {
                net
            };
            liq = liquidity_math::add_delta(liq, net).map_err(|_| RouteError::Math)?;
        }
        tick = if zfo {
            next_tick.checked_sub(1).ok_or(RouteError::Math)?
        } else {
            next_tick
        };
        s = target;
    }
    let rho0 = segs.first().map(|g| g.rho_hi).ok_or(RouteError::Math)?;
    Ok(Model {
        leg,
        pool,
        rho0,
        cap: cum.min(total),
        shape: Shape::V3 {
            zfo,
            kf,
            fee_pips: st.fee_pips,
            segs,
        },
    })
}

fn build_v2<'a>(
    leg: Leg,
    pool: &'a Pool,
    rin: U256,
    rout: U256,
    total: U256,
) -> Result<Model<'a>, RouteError> {
    if rin.is_zero() || rout.is_zero() {
        return Err(RouteError::InsufficientLiquidity);
    }
    // a = 2^96 · sqrt(997000 · rin · rout) / 997 ; b = 1000 · rin / 997
    let prod = U512::from(rin)
        .checked_mul(U512::from(rout))
        .and_then(|v| v.checked_mul(U512::from(997_000u64)))
        .ok_or(RouteError::Math)?;
    let root = narrow(prod.root(2))?;
    let a = mul_div_512(root, Q96, U256::from(997u64))?;
    let b = rin
        .checked_mul(U256::from(1000u64))
        .and_then(|v| v.checked_div(U256::from(997u64)))
        .ok_or(RouteError::Math)?;
    let rho0 = pool.rho_at_zero(leg.i, leg.j)?;
    Ok(Model {
        leg,
        pool,
        rho0,
        cap: total,
        shape: Shape::V2 { a, b },
    })
}

fn build_curve<'a>(
    leg: Leg,
    pool: &'a Pool,
    st: &'a CurveState,
    total: U256,
) -> Result<Model<'a>, RouteError> {
    let rho0 = curve_rho(st, leg.i, leg.j, U256::ZERO)?;
    // Depth: largest x ≤ total the pool can quote (monotone in x).
    let cap = if pool.quote_exact_in(leg.i, leg.j, total).is_ok() {
        total
    } else {
        let (mut lo, mut hi) = (U256::ZERO, total);
        while hi.checked_sub(lo).ok_or(RouteError::Math)? > rel_tol(total) {
            let mid = midpoint(lo, hi)?;
            if pool.quote_exact_in(leg.i, leg.j, mid).is_ok() {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    };
    Ok(Model {
        leg,
        pool,
        rho0,
        cap,
        shape: Shape::Curve {
            st,
            i: leg.i,
            j: leg.j,
        },
    })
}

fn build_model<'a>(leg: Leg, pool: &'a Pool, total: U256) -> Result<Model<'a>, RouteError> {
    if !pool.is_live() {
        return Err(RouteError::StalePool);
    }
    match &pool.state {
        PoolState::V3(s) => build_v3(leg, pool, s, total),
        PoolState::V2(s) => {
            let (rin, rout) = match (leg.i, leg.j) {
                (0, 1) => (s.reserve0, s.reserve1),
                (1, 0) => (s.reserve1, s.reserve0),
                _ => return Err(RouteError::BadLeg),
            };
            build_v2(leg, pool, rin, rout, total)
        }
        PoolState::Curve(s) => build_curve(leg, pool, s, total),
    }
}

#[inline]
fn rel_tol(x: U256) -> U256 {
    x.checked_div(REL_TOL).unwrap_or(U256::ONE).max(U256::ONE)
}

impl Model<'_> {
    /// Gross input this leg absorbs before its marginal falls to `ρ`.
    fn absorbed(&self, rho: U256, budget: &SolveBudget) -> Result<U256, RouteError> {
        if rho >= self.rho0 {
            return Ok(U256::ZERO);
        }
        let x = match &self.shape {
            Shape::V3 {
                zfo,
                kf,
                fee_pips,
                segs,
            } => {
                let k = segs.partition_point(|g| g.rho_hi > rho);
                let Some(seg) = k.checked_sub(1).and_then(|k| segs.get(k)) else {
                    return Ok(U256::ZERO);
                };
                if rho <= seg.rho_lo {
                    // Exactly on this segment's far edge (a breakpoint):
                    // the next segment's cumulative is the exact answer.
                    // No next segment → depth exhausted.
                    return Ok(segs.get(k).map_or(self.cap, |n| n.cum.min(self.cap)));
                }
                let s = s_of_rho(rho, *kf, *zfo)?;
                let s = if *zfo {
                    s.clamp(seg.s_end, seg.s_start)
                } else {
                    s.clamp(seg.s_start, seg.s_end)
                };
                seg.cum
                    .checked_add(gross_step(seg.s_start, s, seg.liq, *zfo, *fee_pips)?)
                    .ok_or(RouteError::Math)?
            }
            Shape::V2 { a, b } => mul_div_512(*a, U256::ONE, rho)?.saturating_sub(*b),
            Shape::Curve { st, i, j } => {
                // Smooth, monotone: invert ρ(x) on [0, cap].
                let f =
                    |x: U256| -> Result<I512, RouteError> { diff(curve_rho(st, *i, *j, x)?, rho) };
                let f_cap = f(self.cap)?;
                if !f_cap.is_negative() {
                    return Ok(self.cap);
                }
                let f0 = diff(self.rho0, rho)?;
                illinois(
                    f,
                    U256::ZERO,
                    self.cap,
                    f0,
                    f_cap,
                    rel_tol(self.cap),
                    budget.max_iters,
                )?
            }
        };
        Ok(x.min(self.cap))
    }

    /// `(a, b)` of `absorbed(ρ) = a/ρ − b` on the smooth piece containing
    /// `ρ_probe`; `None` for a Curve leg (no closed form).
    fn affine(&self, rho_probe: U256) -> Result<Option<(U256, I512)>, RouteError> {
        if rho_probe >= self.rho0 {
            return Ok(Some((U256::ZERO, I512::ZERO)));
        }
        let cap_term = || Ok(Some((U256::ZERO, diff(U256::ZERO, self.cap)?)));
        match &self.shape {
            Shape::V3 {
                zfo,
                kf,
                fee_pips,
                segs,
                ..
            } => {
                let k = segs.partition_point(|g| g.rho_hi > rho_probe);
                let Some(seg) = k.checked_sub(1).and_then(|k| segs.get(k)) else {
                    return Ok(Some((U256::ZERO, I512::ZERO)));
                };
                if rho_probe <= seg.rho_lo {
                    return cap_term();
                }
                let k = keep(*fee_pips)?;
                let liq = U256::from(seg.liq);
                // a = L · kf · 1e6 / keep
                let a = mul_div_512(
                    liq.checked_mul(*kf).ok_or(RouteError::Math)?,
                    U256::from(PIPS),
                    k,
                )?;
                // b = L·2^96·1e6/(s_start·keep)  (zfo)  |  L·s_start·1e6/(2^96·keep)  (ofz)
                let b_raw = if *zfo {
                    let den = seg.s_start.checked_mul(k).ok_or(RouteError::Math)?;
                    mul_div_512(
                        liq.checked_mul(Q96).ok_or(RouteError::Math)?,
                        U256::from(PIPS),
                        den,
                    )?
                } else {
                    let den = Q96.checked_mul(k).ok_or(RouteError::Math)?;
                    mul_div_512(
                        liq.checked_mul(seg.s_start).ok_or(RouteError::Math)?,
                        U256::from(PIPS),
                        den,
                    )?
                };
                Ok(Some((a, diff(b_raw, seg.cum)?)))
            }
            Shape::V2 { a, b } => Ok(Some((*a, i512(*b)))),
            Shape::Curve { .. } => Ok(None),
        }
    }
}

// ───────────────────────────── root finding ─────────────────────────────

/// Bracketed Illinois regula falsi with bisection fallback on `f` sign-
/// changing over `[a, b]`. Stops when the bracket is within `xtol` or `f`
/// hits zero. Iteration cap is a refusal.
fn illinois(
    mut f: impl FnMut(U256) -> Result<I512, RouteError>,
    mut a: U256,
    mut b: U256,
    mut fa: I512,
    mut fb: I512,
    xtol: U256,
    max_iters: u16,
) -> Result<U256, RouteError> {
    if fa.is_zero() {
        return Ok(a);
    }
    if fb.is_zero() {
        return Ok(b);
    }
    if fa.is_negative() == fb.is_negative() {
        return Err(RouteError::Math);
    }
    let two = I512::from_limbs([2, 0, 0, 0, 0, 0, 0, 0]);
    let mut side: i8 = 0;
    let mut stall: u8 = 0;
    for _ in 0..max_iters {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let width = hi.checked_sub(lo).ok_or(RouteError::Math)?;
        if width <= xtol {
            return Ok(if fa.abs() <= fb.abs() { a } else { b });
        }
        // Secant point; bisect when the step degenerates or has stalled
        // twice (the bisection fallback GUIDE 12 §4 requires).
        let mid = midpoint(lo, hi)?;
        let secant = diff(b, a)?
            .checked_mul(fb)
            .and_then(|n| n.checked_div(fb.checked_sub(fa)?))
            .and_then(|q| i512(b).checked_sub(q))
            .and_then(|c| U512::try_from(c).ok())
            .and_then(|c| narrow(c).ok());
        let c = match secant {
            Some(c) if stall < 2 && c > lo && c < hi => c,
            _ => {
                stall = 0;
                mid
            }
        };
        let fc = f(c)?;
        if fc.is_zero() {
            return Ok(c);
        }
        let same_as_b = fc.is_negative() == fb.is_negative();
        let kept = if same_as_b { a } else { b };
        let shrink_ok = c.abs_diff(kept) <= width.wrapping_shr(1);
        stall = if shrink_ok {
            0
        } else {
            stall.saturating_add(1)
        };
        if same_as_b {
            b = c;
            fb = fc;
            if side == -1 {
                fa = fa.checked_div(two).ok_or(RouteError::Math)?;
            }
            side = -1;
        } else {
            a = c;
            fa = fc;
            if side == 1 {
                fb = fb.checked_div(two).ok_or(RouteError::Math)?;
            }
            side = 1;
        }
    }
    Err(RouteError::Math)
}

// ───────────────────────────── water-fill ─────────────────────────────

/// `Σ absorbed_i(ρ) − total`.
fn g(
    models: &[Model<'_>],
    rho: U256,
    total: U256,
    budget: &SolveBudget,
) -> Result<I512, RouteError> {
    let mut acc = I512::ZERO;
    for m in models {
        acc = acc
            .checked_add(i512(m.absorbed(rho, budget)?))
            .ok_or(RouteError::Math)?;
    }
    acc.checked_sub(i512(total)).ok_or(RouteError::Math)
}

/// Water-fill `total` across `models` (sorted by `ρ₀` desc). Returns the
/// per-model input allocations, summing exactly to `total`.
fn water_fill(
    models: &[Model<'_>],
    total: U256,
    budget: &SolveBudget,
) -> Result<SmallVec<[U256; 6]>, RouteError> {
    let n = models.len();
    let cap_sum = models
        .iter()
        .try_fold(U256::ZERO, |acc, m| acc.checked_add(m.cap))
        .ok_or(RouteError::Math)?;
    if cap_sum < total {
        return Err(RouteError::InsufficientLiquidity);
    }
    let mut allocs: SmallVec<[U256; 6]> = SmallVec::with_capacity(n);
    if n == 1 {
        allocs.push(total);
        return Ok(allocs);
    }
    // Breakpoints, descending.
    let mut bps: SmallVec<[U256; 64]> = SmallVec::new();
    for m in models {
        match &m.shape {
            Shape::V3 { segs, .. } => {
                bps.extend(segs.iter().map(|s| s.rho_hi));
                if let Some(last) = segs.last() {
                    bps.push(last.rho_lo);
                }
            }
            Shape::V2 { .. } | Shape::Curve { .. } => bps.push(m.rho0),
        }
    }
    bps.sort_unstable_by(|x, y| y.cmp(x));
    bps.dedup();
    // Smallest k with g(bps[k]) ≥ 0 (g is non-decreasing along k).
    let (mut lo_k, mut hi_k) = (0usize, bps.len());
    while lo_k < hi_k {
        let mid = lo_k
            .checked_add(hi_k.checked_sub(lo_k).ok_or(RouteError::Math)? / 2)
            .ok_or(RouteError::Math)?;
        let v = g(
            models,
            *bps.get(mid).ok_or(RouteError::Math)?,
            total,
            budget,
        )?;
        if v.is_negative() {
            lo_k = mid.checked_add(1).ok_or(RouteError::Math)?;
        } else {
            hi_k = mid;
        }
    }
    let rho_star = if let Some(&rho_k) = bps.get(lo_k) {
        let g_k = g(models, rho_k, total, budget)?;
        if g_k.is_zero() {
            rho_k // root exactly on a breakpoint
        } else {
            let rho_hi = *bps
                .get(lo_k.checked_sub(1).ok_or(RouteError::Math)?)
                .ok_or(RouteError::Math)?;
            solve_interval(models, rho_k, rho_hi, g_k, total, budget)?
        }
    } else {
        // Root below every breakpoint: only V2 (unbounded) legs still move.
        let lo = U256::ONE;
        let g_lo = g(models, lo, total, budget)?;
        let hi = *bps.last().ok_or(RouteError::Math)?;
        solve_interval(models, lo, hi, g_lo, total, budget)?
    };
    // Allocations at ρ*, floor, capped, exact sum via residual to best.
    let mut remaining = total;
    for m in models {
        let x = m.absorbed(rho_star, budget)?.min(remaining);
        allocs.push(x);
        remaining = remaining.checked_sub(x).ok_or(RouteError::Math)?;
    }
    for (x, m) in allocs.iter_mut().zip(models) {
        if remaining.is_zero() {
            break;
        }
        let room = m.cap.checked_sub(*x).ok_or(RouteError::Math)?;
        let add = room.min(remaining);
        *x = x.checked_add(add).ok_or(RouteError::Math)?;
        remaining = remaining.checked_sub(add).ok_or(RouteError::Math)?;
    }
    if !remaining.is_zero() {
        return Err(RouteError::InsufficientLiquidity);
    }
    Ok(allocs)
}

/// Root of `g` on `[rho_lo, rho_hi]` with `g(rho_lo) > 0 > g(rho_hi)`:
/// closed form when every leg is affine here, bracketed secant otherwise.
fn solve_interval(
    models: &[Model<'_>],
    rho_lo: U256,
    rho_hi: U256,
    g_lo: I512,
    total: U256,
    budget: &SolveBudget,
) -> Result<U256, RouteError> {
    let probe = midpoint(rho_lo, rho_hi)?;
    let mut a_sum = U512::ZERO;
    let mut b_sum = I512::ZERO;
    let mut affine = true;
    for m in models {
        match m.affine(probe)? {
            Some((a, b)) => {
                a_sum = a_sum.checked_add(U512::from(a)).ok_or(RouteError::Math)?;
                b_sum = b_sum.checked_add(b).ok_or(RouteError::Math)?;
            }
            None => {
                affine = false;
                break;
            }
        }
    }
    if affine {
        // A/ρ − B − T = 0 → ρ* = A / (B + T)
        let den = b_sum.checked_add(i512(total)).ok_or(RouteError::Math)?;
        if !den.is_positive() {
            return Err(RouteError::Math);
        }
        let den = U512::try_from(den).map_err(|_| RouteError::Math)?;
        let q = a_sum.checked_div(den).ok_or(RouteError::Math)?;
        let rho = narrow(q)?;
        return Ok(rho.clamp(rho_lo, rho_hi));
    }
    let g_hi = g(models, rho_hi, total, budget)?;
    illinois(
        |rho| g(models, rho, total, budget),
        rho_lo,
        rho_hi,
        g_lo,
        g_hi,
        rel_tol(rho_hi),
        budget.max_iters,
    )
}

/// Exact quotes for an allocation vector over `models`.
fn quote_allocs(
    models: &[Model<'_>],
    xs: &[U256],
) -> Result<(SmallVec<[Allocation; 6]>, U256, u64), RouteError> {
    let mut allocs = SmallVec::with_capacity(xs.len());
    let mut out = U256::ZERO;
    let mut gas = 0u64;
    for (m, &x) in models.iter().zip(xs) {
        let amount_out = m.pool.quote_exact_in(m.leg.i, m.leg.j, x)?;
        if !x.is_zero() {
            gas = gas.checked_add(m.pool.hop_gas).ok_or(RouteError::Math)?;
        }
        out = out.checked_add(amount_out).ok_or(RouteError::Math)?;
        allocs.push(Allocation {
            leg: m.leg,
            amount_in: x,
            amount_out,
        });
    }
    Ok((allocs, out, gas))
}

/// Water-fill `total` of `legs` (all `asset_in → asset_out` over `pools`,
/// `Leg::pool` indexing `pools`), greedy on the pool set: pools enter in
/// `ρ₀` order and the set stops growing when the next pool's hop gas (in
/// output units) exceeds the output it adds (GUIDE 12 §4).
pub fn solve_on(
    pools: &[Pool],
    legs: &[Leg],
    total: U256,
    gas: &GasTerms,
    budget: &SolveBudget,
) -> Result<ExitQuote, RouteError> {
    if total.is_zero() {
        return Err(RouteError::BadLeg);
    }
    let mut models: SmallVec<[Model<'_>; 8]> = SmallVec::new();
    for &leg in legs {
        let pool = pools
            .get(usize::try_from(leg.pool.0).map_err(|_| RouteError::BadLeg)?)
            .ok_or(RouteError::BadLeg)?;
        match build_model(leg, pool, total) {
            Ok(m) => models.push(m),
            Err(RouteError::StalePool | RouteError::InsufficientLiquidity) => {
                tracing::debug!(pool = ?pool.address, "leg excluded from solve");
            }
            Err(e) => return Err(e),
        }
    }
    if models.is_empty() {
        return Err(RouteError::InsufficientLiquidity);
    }
    models.sort_by_key(|m| std::cmp::Reverse(m.rho0));
    let rho0 = models.first().map_or(U256::ZERO, |m| m.rho0);
    let max_pools = usize::from(budget.max_pools).clamp(1, models.len());
    let mut best: Option<ExitQuote> = None;
    for k in 1..=max_pools {
        let set = models.get(..k).ok_or(RouteError::Math)?;
        let xs = match water_fill(set, total, budget) {
            Ok(xs) => xs,
            Err(RouteError::InsufficientLiquidity) => continue,
            Err(e) => return Err(e),
        };
        let (allocs, out, hop_gas) = match quote_allocs(set, &xs) {
            Ok(q) => q,
            Err(RouteError::InsufficientLiquidity) => continue,
            Err(e) => return Err(e),
        };
        if let Some(prev) = &best {
            let gain = out.saturating_sub(prev.amount_out);
            let added_gas = hop_gas.saturating_sub(prev.hop_gas);
            if gain <= gas.cost_in_out(added_gas)? {
                break;
            }
        }
        best = Some(ExitQuote {
            allocs,
            amount_in: total,
            amount_out: out,
            hop_gas,
            rho0,
        });
    }
    best.ok_or(RouteError::InsufficientLiquidity)
}

/// [`solve_on`] over the book's live pools for one pair.
pub fn solve_pair(
    book: &PoolBook,
    asset_in: AssetId,
    asset_out: AssetId,
    total: U256,
    gas: &GasTerms,
    budget: &SolveBudget,
) -> Result<ExitQuote, RouteError> {
    solve_on(
        book.pools(),
        book.legs(asset_in, asset_out),
        total,
        gas,
        budget,
    )
}

// ───────────────────────────── K collaterals ─────────────────────────────

/// K collaterals into one debt asset, legs executed sequentially against
/// the displaced state (GUIDE 12 §4c). Exhaustive over orderings for
/// `K ≤ 4`; largest-notional-first beyond. Best = most debt out.
pub fn solve_batch(
    book: &PoolBook,
    colls: &[(AssetId, U256)],
    debt: AssetId,
    gas: &GasTerms,
    budget: &SolveBudget,
) -> Result<BatchQuote, RouteError> {
    if colls.is_empty() || colls.len() > usize::from(u8::MAX) {
        return Err(RouteError::BadLeg);
    }
    // Scratch copy of every pool any collateral may touch; legs remapped.
    let mut base: Vec<Pool> = Vec::new();
    let mut ids: SmallVec<[PoolId; 16]> = SmallVec::new();
    let mut legs_per: SmallVec<[SmallVec<[Leg; 8]>; EXHAUSTIVE_K]> = SmallVec::new();
    for &(coll, _) in colls {
        let mut remapped = SmallVec::new();
        for leg in book.legs(coll, debt) {
            let idx = match ids.iter().position(|&p| p == leg.pool) {
                Some(i) => i,
                None => {
                    ids.push(leg.pool);
                    base.push(book.get(leg.pool).ok_or(RouteError::BadLeg)?.clone());
                    ids.len().checked_sub(1).ok_or(RouteError::Math)?
                }
            };
            remapped.push(Leg {
                pool: PoolId(u32::try_from(idx).map_err(|_| RouteError::Math)?),
                i: leg.i,
                j: leg.j,
            });
        }
        legs_per.push(remapped);
    }
    let k = colls.len();
    let run = |order: &[u8], scratch: &mut Vec<Pool>| -> Result<BatchQuote, RouteError> {
        scratch.clone_from(&base);
        let mut quotes: SmallVec<[ExitQuote; EXHAUSTIVE_K]> = SmallVec::new();
        let (mut out, mut hop_gas) = (U256::ZERO, 0u64);
        for &ci in order {
            let ci_us = usize::from(ci);
            let &(_, amount) = colls.get(ci_us).ok_or(RouteError::BadLeg)?;
            let legs = legs_per.get(ci_us).ok_or(RouteError::BadLeg)?;
            let q = solve_on(scratch, legs, amount, gas, budget)?;
            for a in &q.allocs {
                if a.amount_in.is_zero() {
                    continue;
                }
                let p = scratch
                    .get_mut(usize::try_from(a.leg.pool.0).map_err(|_| RouteError::Math)?)
                    .ok_or(RouteError::BadLeg)?;
                p.apply_exact_in(a.leg.i, a.leg.j, a.amount_in)?;
            }
            out = out.checked_add(q.amount_out).ok_or(RouteError::Math)?;
            hop_gas = hop_gas.checked_add(q.hop_gas).ok_or(RouteError::Math)?;
            quotes.push(q);
        }
        Ok(BatchQuote {
            order: order.iter().copied().collect(),
            quotes,
            amount_out: out,
            hop_gas,
        })
    };
    let mut scratch: Vec<Pool> = Vec::with_capacity(base.len());
    if k > EXHAUSTIVE_K {
        // Largest notional first: amount · ρ₀² in debt units.
        let mut order: Vec<(U256, u8)> = Vec::with_capacity(k);
        for (ci, &(coll, amount)) in colls.iter().enumerate() {
            let rho0 = book
                .legs(coll, debt)
                .iter()
                .filter_map(|l| book.get(l.pool).and_then(|p| p.rho_at_zero(l.i, l.j).ok()))
                .max()
                .ok_or(RouteError::InsufficientLiquidity)?;
            let notional = mul_div_512(mul_div_512(amount, rho0, Q96)?, rho0, Q96)?;
            order.push((notional, u8::try_from(ci).map_err(|_| RouteError::BadLeg)?));
        }
        order.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
        let idx: Vec<u8> = order.into_iter().map(|(_, i)| i).collect();
        return run(&idx, &mut scratch);
    }
    let mut perm: SmallVec<[u8; EXHAUSTIVE_K]> = (0..k)
        .map(|i| u8::try_from(i).map_err(|_| RouteError::Math))
        .collect::<Result<_, _>>()?;
    let mut best: Option<BatchQuote> = None;
    // Heap's algorithm, iterative.
    let mut c: SmallVec<[usize; EXHAUSTIVE_K]> = SmallVec::from_elem(0, k);
    let mut consider = |perm: &[u8], best: &mut Option<BatchQuote>| -> Result<(), RouteError> {
        match run(perm, &mut scratch) {
            Ok(q) => {
                if best.as_ref().is_none_or(|b| q.amount_out > b.amount_out) {
                    *best = Some(q);
                }
                Ok(())
            }
            Err(RouteError::InsufficientLiquidity) => Ok(()),
            Err(e) => Err(e),
        }
    };
    consider(&perm, &mut best)?;
    let mut i = 1usize;
    while i < k {
        let ci = c.get_mut(i).ok_or(RouteError::Math)?;
        if *ci < i {
            let swap_with = if i.is_multiple_of(2) { 0 } else { *ci };
            perm.swap(i, swap_with);
            *ci = ci.checked_add(1).ok_or(RouteError::Math)?;
            i = 1;
            consider(&perm, &mut best)?;
        } else {
            *ci = 0;
            i = i.checked_add(1).ok_or(RouteError::Math)?;
        }
    }
    best.ok_or(RouteError::InsufficientLiquidity)
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use std::collections::HashMap;

    use alloy_primitives::U256;
    use uniswap_v3_math::{full_math, sqrt_price_math};

    use super::*;
    use crate::fixtures::*;
    use crate::solver::PoolState;

    const FREE: GasTerms = GasTerms {
        base_fee_wei: 0,
        out_per_eth: U256::ZERO,
    };
    const B: SolveBudget = SolveBudget {
        max_pools: 6,
        max_iters: 64,
    };
    const L: u128 = 1_000_000_000_000_000_000_000; // 1000e18

    fn book(pools: Vec<Pool>) -> PoolBook {
        let mut assets = HashMap::new();
        assets.insert(tok(0), A0);
        assets.insert(tok(1), A1);
        assets.insert(tok(2), A2);
        let mut b = PoolBook::new(assets, None, HOP_GAS);
        for p in pools {
            b.add(p).unwrap();
        }
        b
    }

    fn sum_in(q: &ExitQuote) -> U256 {
        q.allocs.iter().fold(U256::ZERO, |a, x| a + x.amount_in)
    }

    fn used(q: &ExitQuote) -> usize {
        q.allocs.iter().filter(|a| !a.amount_in.is_zero()).count()
    }

    /// Gross input to move `liq` from price 1 down to `tick`, as the
    /// contract charges it (independent of the solver's model code).
    fn gross_to(tick: i32, liq: u128, fee: u32) -> U256 {
        let a = sqrt_price_math::_get_amount_0_delta(sqrt_at(tick), SQRT_ONE, liq, true).unwrap();
        a + full_math::mul_div_rounding_up(a, U256::from(fee), U256::from(1_000_000 - fee)).unwrap()
    }

    fn sqrt_after(p: &Pool, x: U256) -> U256 {
        let mut q = p.clone();
        q.apply_exact_in(0, 1, x).unwrap();
        let PoolState::V3(s) = q.state else {
            unreachable!()
        };
        s.sqrt_price_x96
    }

    /// Acceptance (GUIDE 12): root exactly at a tick boundary. Two pools
    /// with identical liquidity above tick −600; A gains liquidity below
    /// it. `total = 2·X` where `X` moves one pool exactly to −600, so the
    /// optimum puts **both** at the boundary — `g` has a kink there and
    /// its root is the kink. Variant (i): both pools carry the tick → the
    /// breakpoint search lands on `g == 0` and allocations are exact.
    /// Variant (ii): B has no tick at −600 → the interval solve converges
    /// to within the `λ` tolerance and marginals still equalise.
    #[test]
    fn root_at_tick_boundary() {
        let x = gross_to(-600, L, 3000);
        let total = x * U256::from(2u64);
        let a = v3(1, 3000, 60, SQRT_ONE, &[(-6000, 6000, L), (-6000, -600, L)]);
        // (i)
        let bk = book(vec![
            a.clone(),
            v3(2, 3000, 60, SQRT_ONE, &[(-6000, 6000, L), (-6000, -600, L)]),
        ]);
        let q = solve_pair(&bk, A0, A1, total, &FREE, &B).unwrap();
        assert_eq!(sum_in(&q), total);
        assert_eq!(q.allocs[0].amount_in, x);
        assert_eq!(q.allocs[1].amount_in, x);
        for p in bk.pools() {
            assert_eq!(sqrt_after(p, x), sqrt_at(-600));
        }
        // (ii)
        let b = v3(2, 3000, 60, SQRT_ONE, &[(-6000, 6000, L)]);
        let bk = book(vec![a.clone(), b.clone()]);
        let q = solve_pair(&bk, A0, A1, total, &FREE, &B).unwrap();
        assert_eq!(sum_in(&q), total);
        let tol = x / U256::from(1_000_000u64);
        for al in &q.allocs {
            assert!(al.amount_in.abs_diff(x) <= tol, "{} vs {x}", al.amount_in);
        }
        let sa = sqrt_after(&a, q.allocs[0].amount_in);
        let sb = sqrt_after(&b, q.allocs[1].amount_in);
        let target = sqrt_at(-600);
        let stol = target / U256::from(1_000_000u64);
        assert!(
            sa.abs_diff(target) <= stol && sb.abs_diff(target) <= stol,
            "{sa} {sb} {target}"
        );
    }

    /// Acceptance: beats proportional and equal splitting on a fixed
    /// fixture; and against a brute-force grid oracle on two pools the
    /// result is within the `λ` tolerance of the grid optimum.
    #[test]
    fn beats_proportional_and_equal_and_matches_grid() {
        let deep = v2(1, e18(1_000), e18(1_000));
        let shallow = v2(2, e18(100), e18(100));
        let v3p = v3(3, 500, 10, SQRT_ONE, &[(-6000, 6000, 300 * L / 1000)]);
        let total = e18(50);
        let bk = book(vec![deep.clone(), shallow.clone(), v3p.clone()]);
        let ours = solve_pair(&bk, A0, A1, total, &FREE, &B).unwrap();
        assert_eq!(sum_in(&ours), total);
        let pools = [&deep, &shallow, &v3p];
        let out_of = |xs: [U256; 3]| -> U256 {
            xs.iter()
                .zip(pools)
                .map(|(&x, p)| p.quote_exact_in(0, 1, x).unwrap())
                .sum()
        };
        let third = total / U256::from(3u64);
        let equal = out_of([third, third, total - third - third]);
        // proportional to depth: reserve0 for V2, L·2^96/√P = L for V3 at price 1.
        let w = [e18(1_000), e18(100), U256::from(300 * L / 1000)];
        let ws: U256 = w.iter().sum();
        let p0 = total * w[0] / ws;
        let p1 = total * w[1] / ws;
        let prop = out_of([p0, p1, total - p0 - p1]);
        assert!(
            ours.amount_out > equal && ours.amount_out > prop,
            "{} {equal} {prop}",
            ours.amount_out
        );

        let bk2 = book(vec![deep.clone(), shallow.clone()]);
        let ours2 = solve_pair(&bk2, A0, A1, total, &FREE, &B).unwrap();
        let mut best = U256::ZERO;
        let n = 2_000u64;
        for k in 0..=n {
            let xa = total * U256::from(k) / U256::from(n);
            let o = deep.quote_exact_in(0, 1, xa).unwrap()
                + shallow.quote_exact_in(0, 1, total - xa).unwrap();
            best = best.max(o);
        }
        assert!(
            ours2.amount_out + best / U256::from(1_000_000u64) >= best,
            "{} vs grid {best}",
            ours2.amount_out
        );
    }

    /// Acceptance: allocations sum exactly to the input on awkward sizes;
    /// the best-`ρ₀` pool leads the vector (residual target).
    #[test]
    fn allocations_sum_exactly() {
        let bk = book(vec![
            v2(1, e18(1_000), e18(1_000)),
            v3(2, 500, 10, SQRT_ONE, &[(-6000, 6000, L)]),
            v2(3, e18(333), e18(331)),
        ]);
        for t in [
            U256::from(1u64),
            U256::from(7_919u64),
            e18(1) + U256::from(1u64),
            e18(97) + U256::from(123_456_789u64),
        ] {
            let q = solve_pair(&bk, A0, A1, t, &FREE, &B).unwrap();
            assert_eq!(sum_in(&q), t);
            assert!(q.allocs.windows(2).all(|w| {
                let ra = bk.get(w[0].leg.pool).unwrap().rho_at_zero(0, 1).unwrap();
                let rb = bk.get(w[1].leg.pool).unwrap().rho_at_zero(0, 1).unwrap();
                ra >= rb
            }));
        }
    }

    /// Acceptance: pool-set selection stops when hop gas exceeds the
    /// output it adds. Small exit → one pool; large exit → several.
    #[test]
    fn greedy_stops_on_hop_gas() {
        let bk = book(vec![
            v2(1, e18(1_000_000), e18(1_000_000)),
            v2(2, e18(10_000), e18(10_000)),
        ]);
        // 30 gwei, output token is 18-dec ETH-equivalent: one hop = 0.003 out.
        let gas = GasTerms {
            base_fee_wei: 30_000_000_000,
            out_per_eth: e18(1),
        };
        let small = solve_pair(&bk, A0, A1, e18(1), &gas, &B).unwrap();
        assert_eq!(used(&small), 1, "{small:?}");
        assert_eq!(small.hop_gas, HOP_GAS);
        let large = solve_pair(&bk, A0, A1, e18(100_000), &gas, &B).unwrap();
        assert_eq!(used(&large), 2, "{large:?}");
        assert_eq!(large.hop_gas, 2 * HOP_GAS);
        // With free gas the small exit also splits (any gain is worth it).
        let free = solve_pair(&bk, A0, A1, e18(1), &FREE, &B).unwrap();
        assert_eq!(used(&free), 2);
    }

    /// Curve leg forces the smooth (bracketed-secant) path. Sum exact and
    /// better than an equal split. `A = 10` so that 10 % one-sided
    /// impact (~1 %) exceeds the V2 fee (0.3 %) and the split is real —
    /// at `A = 200` the same trade is ~0.09 % all-in and Curve correctly
    /// takes everything.
    #[test]
    fn curve_plus_v2_water_fill() {
        let c = curve(1, &[e18(2_000_000), e18(2_000_000)], 10 * 100, 4_000_000);
        let d = v2(2, e18(500_000), e18(500_000));
        let bk = book(vec![c.clone(), d.clone()]);
        let total = e18(200_000);
        let q = solve_pair(&bk, A0, A1, total, &FREE, &B).unwrap();
        assert_eq!(sum_in(&q), total);
        assert_eq!(used(&q), 2);
        let half = total / U256::from(2u64);
        let equal = c.quote_exact_in(0, 1, half).unwrap() + d.quote_exact_in(0, 1, half).unwrap();
        assert!(q.amount_out > equal, "{} vs {equal}", q.amount_out);
        // Curve should take the lion's share: 1 bp-ish fee, 4× the depth.
        let curve_alloc = q
            .allocs
            .iter()
            .find(|a| a.leg.pool == PoolId(0))
            .unwrap()
            .amount_in;
        assert!(curve_alloc > total * U256::from(3u64) / U256::from(4u64));
    }

    /// Depth binds: a single shallow pool refuses; adding the deep pool
    /// (which the greedy must not skip) solves.
    #[test]
    fn insufficient_depth_is_a_refusal() {
        let bk = book(vec![v3(1, 3000, 60, SQRT_ONE, &[(-600, 600, L / 1000)])]);
        assert_eq!(
            solve_pair(&bk, A0, A1, e18(500), &FREE, &B),
            Err(RouteError::InsufficientLiquidity)
        );
        assert_eq!(
            solve_pair(&bk, A0, A1, U256::ZERO, &FREE, &B),
            Err(RouteError::BadLeg)
        );
        assert_eq!(
            solve_pair(&bk, A0, A2, e18(1), &FREE, &B),
            Err(RouteError::InsufficientLiquidity)
        );
    }

    /// GUIDE 12 §4c: sequential legs see the displaced state. Oracle: V3
    /// fees do not enter liquidity, so two sequential swaps equal one swap
    /// of the sum to per-step rounding; the batch total must match.
    #[test]
    fn batch_displaced_state_is_path_consistent() {
        let p = v3(1, 3000, 60, SQRT_ONE, &[(-6000, 6000, L), (-6000, -600, L)]);
        let bk = book(vec![p.clone()]);
        let (x1, x2) = (e18(15), e18(30));
        let one = p.quote_exact_in(0, 1, x1 + x2).unwrap();
        let b = solve_batch(&bk, &[(A0, x1), (A0, x2)], A1, &FREE, &B).unwrap();
        assert_eq!(b.quotes.len(), 2);
        assert!(
            b.amount_out.abs_diff(one) <= U256::from(4u64),
            "{} vs {one}",
            b.amount_out
        );
        assert_eq!(b.hop_gas, 2 * HOP_GAS);
    }

    /// Independent pools → every ordering yields the same total, and it
    /// equals the sum of the single solves. `K > 4` takes the heuristic
    /// path and still sums exactly.
    #[test]
    fn batch_orderings_and_heuristic_path() {
        let mut p2 = v2(2, e18(1_000), e18(1_000));
        p2.assets = smallvec::SmallVec::from_slice(&[A2, A1]);
        p2.tokens = smallvec::SmallVec::from_slice(&[tok(2), tok(1)]);
        let p1 = v2(1, e18(1_000), e18(1_000));
        let bk = book(vec![p1.clone(), p2.clone()]);
        let s1 = solve_pair(&bk, A0, A1, e18(10), &FREE, &B).unwrap();
        let s2 = solve_pair(&bk, A2, A1, e18(20), &FREE, &B).unwrap();
        let b = solve_batch(&bk, &[(A0, e18(10)), (A2, e18(20))], A1, &FREE, &B).unwrap();
        assert_eq!(b.amount_out, s1.amount_out + s2.amount_out);
        assert_eq!(b.order.len(), 2);
        let five: Vec<(liq_types::AssetId, U256)> = (0..5)
            .map(|k| (if k % 2 == 0 { A0 } else { A2 }, e18(1 + k)))
            .collect();
        let b5 = solve_batch(&bk, &five, A1, &FREE, &B).unwrap();
        assert_eq!(b5.order.len(), 5);
        assert_eq!(b5.order[0], 4, "largest notional first");
        let total_in: U256 = b5.quotes.iter().map(sum_in).sum();
        assert_eq!(total_in, five.iter().map(|x| x.1).sum::<U256>());
    }

    /// EIP-1559-style gas conversion: `gas · fee · out_per_eth / 1e18`.
    #[test]
    fn gas_terms_cost() {
        let g = GasTerms {
            base_fee_wei: 20_000_000_000,
            out_per_eth: U256::from(3_000_000_000u64), // 3000 USDC (6 dec) per ETH
        };
        // 100k gas · 20 gwei = 0.002 ETH = 6 USDC = 6_000_000 raw
        assert_eq!(g.cost_in_out(100_000).unwrap(), U256::from(6_000_000u64));
    }

    /// Six pools (3 V3 with several ticks, 3 V2) solve without refusal,
    /// sum exactly and use every pool at a large size. Wall time is
    /// measured by `benches/solve.rs`; here only a debug-build regression
    /// guard so a pathological iteration count cannot land silently.
    #[test]
    fn six_pool_solve() {
        let bk = six_pool_book();
        let total = e18(3_000);
        let t0 = std::time::Instant::now();
        let q = solve_pair(&bk, A0, A1, total, &FREE, &B).unwrap();
        let dt = t0.elapsed();
        assert_eq!(sum_in(&q), total);
        assert_eq!(used(&q), 6, "{q:?}");
        assert!(
            dt < std::time::Duration::from_millis(50),
            "debug solve took {dt:?}"
        );
    }

    fn six_pool_book() -> PoolBook {
        book(vec![
            v3(
                1,
                3000,
                60,
                SQRT_ONE,
                &[(-6000, 6000, L), (-1200, -600, L), (-3000, -1800, 2 * L)],
            ),
            v3(
                2,
                500,
                10,
                SQRT_ONE,
                &[(-2000, 2000, L / 2), (-500, 500, L), (-100, 100, 2 * L)],
            ),
            v3(
                3,
                10_000,
                200,
                SQRT_ONE,
                &[(-20_000, 20_000, 3 * L), (-4000, 0, L)],
            ),
            v2(4, e18(2_000), e18(2_000)),
            v2(5, e18(700), e18(690)),
            v2(6, e18(5_000), e18(5_050)),
        ])
    }

    /// GUIDE 12 acceptance: no HTTP client and no thread spawning in this
    /// crate — asserted on the manifest and the sources, not assumed.
    #[test]
    fn no_http_no_threads_in_router() {
        let manifest = include_str!("../Cargo.toml");
        for banned in [
            concat!("req", "west"),
            "hyper",
            concat!("alloy-", "provider"),
            concat!("alloy-", "transport"),
            "tokio",
            "ureq",
            "jsonrpsee",
            concat!("ray", "on"),
        ] {
            assert!(
                !manifest.contains(banned),
                "{banned} in liq-router manifest"
            );
        }
        let srcs = [
            include_str!("lib.rs"),
            include_str!("solver.rs"),
            include_str!("exact.rs"),
            include_str!("warm.rs"),
            include_str!("band.rs"),
            include_str!("cache.rs"),
        ];
        // Patterns are assembled so this test's own source never contains
        // them verbatim.
        let banned = [
            concat!("thread::", "spawn"),
            concat!("thread::", "scope"),
            concat!("ray", "on"),
            concat!("req", "west"),
            concat!("Prov", "ider"),
        ];
        for s in srcs {
            let code_only: String = s
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            for b in banned {
                assert!(!code_only.contains(b), "{b} found in router source");
            }
        }
    }
}
