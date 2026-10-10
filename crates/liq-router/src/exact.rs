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

use crate::balancer::BalancerState;
use crate::crypto::CryptoState;
use crate::fluid::FluidState;
use crate::solver::{
    curve_rho, mul_div_512, narrow, CurveState, Leg, Pool, PoolBook, PoolId, PoolState, RouteError,
    V3State, PIPS, Q192, Q96,
};
use crate::solver::{ExitSource, Unwrap, Venue};

/// `1e18` wei per ETH.
const WEI_PER_ETH: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
/// D29: profit numeraire is ETH/WETH — `out_per_eth` identity, not a price.
pub const OUT_PER_ETH_WETH: U256 = WEI_PER_ETH;
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
    /// Block base fee (wei / gas). Not mixed with priority, and not
    /// `maxFeePerGas` (that ceiling is only on the builder transaction).
    pub base_fee_wei: u128,
    /// Priority fee (wei / gas), kept separate from [`Self::base_fee_wei`].
    pub priority_fee_wei: u128,
    /// Raw output-token units per `1e18` wei (oracle, `min(spot, twa)`).
    pub out_per_eth: U256,
}

impl GasTerms {
    /// Accounting price: block base fee plus priority. Overflow refuses.
    pub fn accounting_wei_per_gas(&self) -> Result<u128, RouteError> {
        self.base_fee_wei
            .checked_add(self.priority_fee_wei)
            .ok_or(RouteError::Math)
    }

    /// `gas · (base_fee + priority) · out_per_eth / 1e18`, floor.
    pub fn cost_in_out(&self, gas: u64) -> Result<U256, RouteError> {
        let per = self.accounting_wei_per_gas()?;
        let wei = U256::from(gas)
            .checked_mul(U256::from(per))
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
            max_pools: 12,
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

/// The unwrap step ahead of an exit's pools.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct UnwrapUse {
    pub kind: crate::solver::UnwrapKind,
    pub wrapper: AssetId,
    pub into: AssetId,
    /// What the unwrap pays (conservative), in `into` units: the input the
    /// pools share.
    pub amount_out: U256,
}

/// The first swap of an exit through the hub ([`PoolBook::hub_route`]):
/// the collateral, or what it unwraps into, split across these pools into
/// the hub. [`ExitQuote::allocs`] then sell `amount_out` of the hub for the
/// debt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubUse {
    pub hub: AssetId,
    /// Best-`ρ₀` pool first; sums exactly to `amount_in`.
    pub allocs: SmallVec<[Allocation; 6]>,
    /// What these pools sell: the collateral, or the unwrap's output.
    pub amount_in: U256,
    /// Hub units they pay: what [`ExitQuote::allocs`] sell.
    pub amount_out: U256,
}

/// Result of one collateral → debt exit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitQuote {
    /// Best-`ρ₀` pool first. Sums exactly to `amount_in`, with an unwrap
    /// to `unwrap.amount_out`, and through the hub to `hub.amount_out`
    /// (they are then the hub's pools into the debt).
    pub allocs: SmallVec<[Allocation; 6]>,
    /// Collateral in (the wrapper when there is an unwrap).
    pub amount_in: U256,
    pub amount_out: U256,
    /// Σ `hop_gas` over pools with a non-zero allocation.
    pub hop_gas: u64,
    /// Best marginal at zero size across candidates, Q96 sqrt.
    pub rho0: U256,
    /// Unwrap the collateral first (ERC-4626 redeem) when it has no pool.
    pub unwrap: Option<UnwrapUse>,
    /// Sell into the hub first ([`PoolBook::hub_route`]) when that leaves
    /// more debt after gas than the pools between collateral and debt.
    /// Boxed: plans hold quotes inline by the dozen, and most exits are
    /// direct.
    pub hub: Option<Box<HubUse>>,
    /// An exact-output chain through the graph (venue 10): the repay buys
    /// exactly the pull along it, the collateral it leaves closes into WETH
    /// as a direct exit's does. `allocs` is then empty.
    pub chain: Option<Box<ChainUse>>,
}

/// A chain exit's path: its hops (pool and coin indices, in order) and its
/// venue-10 data (hop kinds and params, intermediate tokens).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainUse {
    pub hops: SmallVec<[Leg; 4]>,
    pub data: Vec<u8>,
    /// What the chain buys of the exit's output: all of it alone, its part
    /// when the exit splits with direct pools or other chains (the repay
    /// gives each the pull in proportion).
    pub amount_out: U256,
    /// Further chains the sale is split with, each with its own part; none
    /// shares a pool with this one, another, or the direct pools.
    pub with: Vec<ChainUse>,
    /// Every hop can buy an exact output (V3 and V2). Otherwise (a V4 or
    /// Curve hop) the chain sells an exact input: what buys its part, plus
    /// the tolerance, with the surplus debt swept to WETH.
    pub exact_out: bool,
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
#[derive(Clone)]
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
    /// Curve crypto pool: `ρ(x)` from a forward difference of the exact
    /// output ([`CryptoState::rho`]), inverted like `Curve`.
    Crypto {
        st: &'a CryptoState,
        i: u8,
        j: u8,
    },
    /// Balancer weighted pool: `ρ(x)` from a forward difference of the exact
    /// output ([`BalancerState::rho`]), inverted like `Crypto`.
    Balancer {
        st: &'a BalancerState,
        i: u8,
        j: u8,
    },
    /// Fluid DEX pool: `ρ(x)` from a forward difference of the exact
    /// output ([`FluidState::rho`]), inverted like `Crypto`; its capacity
    /// is the half of the imaginary reserves the pool refuses past.
    Fluid {
        st: &'a FluidState,
        i: u8,
        j: u8,
    },
}

#[derive(Clone)]
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
    // The known window's edge ends the model: its cap is what the read
    // ticks can absorb.
    while let Some((next_tick, initialized, at_edge)) =
        crate::solver::bounded_next_tick_pub(st, tick, zfo)
    {
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
        if target == limit || cum >= total || segs.len() >= MAX_SEGS || at_edge {
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
    let rho0 = segs
        .first()
        .map(|g| g.rho_hi)
        .ok_or(RouteError::InsufficientLiquidity)?;
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
        PoolState::Crypto(s) => build_crypto(leg, pool, s, total),
        PoolState::Balancer(s) => build_balancer(leg, pool, s, total),
        PoolState::Fluid(s) => build_fluid(leg, pool, s, total),
    }
}

/// A Fluid DEX pool: `ρ(x)` from a forward difference of the exact output
/// ([`FluidState::rho`]); the input it absorbs ends where a quote is refused
/// (the imaginary-reserve half, the borrow and withdrawal limits).
fn build_fluid<'a>(
    leg: Leg,
    pool: &'a Pool,
    st: &'a FluidState,
    total: U256,
) -> Result<Model<'a>, RouteError> {
    let rho0 = st.rho(leg.i, leg.j, U256::ZERO)?;
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
        shape: Shape::Fluid {
            st,
            i: leg.i,
            j: leg.j,
        },
    })
}

/// A Balancer weighted pool: `ρ(x)` from a forward difference of the exact
/// output ([`BalancerState::rho`]), inverted like Curve crypto; the input it
/// absorbs ends at its 30 % ratio (a quote past it is refused).
fn build_balancer<'a>(
    leg: Leg,
    pool: &'a Pool,
    st: &'a BalancerState,
    total: U256,
) -> Result<Model<'a>, RouteError> {
    let rho0 = st.rho(leg.i, leg.j, U256::ZERO)?;
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
        shape: Shape::Balancer {
            st,
            i: leg.i,
            j: leg.j,
        },
    })
}

fn build_crypto<'a>(
    leg: Leg,
    pool: &'a Pool,
    st: &'a CryptoState,
    total: U256,
) -> Result<Model<'a>, RouteError> {
    let rho0 = st.rho(leg.i, leg.j, U256::ZERO)?;
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
        shape: Shape::Crypto {
            st,
            i: leg.i,
            j: leg.j,
        },
    })
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
            Shape::Balancer { st, i, j } => {
                let f = |x: U256| -> Result<I512, RouteError> { diff(st.rho(*i, *j, x)?, rho) };
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
            Shape::Fluid { st, i, j } => {
                let f = |x: U256| -> Result<I512, RouteError> { diff(st.rho(*i, *j, x)?, rho) };
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
            Shape::Crypto { st, i, j } => {
                // Same inversion, ρ from the exact output.
                let f = |x: U256| -> Result<I512, RouteError> { diff(st.rho(*i, *j, x)?, rho) };
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
            Shape::Curve { .. }
            | Shape::Crypto { .. }
            | Shape::Balancer { .. }
            | Shape::Fluid { .. } => Ok(None),
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
    water_fill_level(models, total, budget).map(|(xs, _)| xs)
}

/// [`water_fill`], with the common marginal `ρ*` the fill stops at (`None`
/// for one pool, which takes everything).
fn water_fill_level(
    models: &[Model<'_>],
    total: U256,
    budget: &SolveBudget,
) -> Result<(SmallVec<[U256; 6]>, Option<U256>), RouteError> {
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
        return Ok((allocs, None));
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
            Shape::V2 { .. }
            | Shape::Curve { .. }
            | Shape::Crypto { .. }
            | Shape::Balancer { .. }
            | Shape::Fluid { .. } => bps.push(m.rho0),
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
    Ok((allocs, Some(rho_star)))
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

/// A pool that can only sell an exact input (Curve, Fluid DEX): the
/// assembler overbuys through it and a closer sells the surplus.
fn exact_in_only(pool: &Pool) -> bool {
    matches!(
        pool.venue(),
        Venue::CurveStable | Venue::CurveCrypto | Venue::Fluid
    )
}

/// What a leg through `m` costs in gas, its share of `closer_gas` included.
fn model_gas(m: &Model<'_>, closer_gas: u64) -> u64 {
    if exact_in_only(m.pool) {
        m.pool.hop_gas.saturating_add(closer_gas)
    } else {
        m.pool.hop_gas
    }
}

/// Exact quotes for an allocation vector over `models`. `closer_gas` is
/// charged once when any allocated pool sells only an exact input: such a
/// leg overbuys what it owes, and a closer sells the surplus back.
fn quote_allocs(
    models: &[Model<'_>],
    xs: &[U256],
    closer_gas: u64,
) -> Result<(SmallVec<[Allocation; 6]>, U256, u64), RouteError> {
    let mut allocs = SmallVec::with_capacity(xs.len());
    let mut out = U256::ZERO;
    let mut gas = 0u64;
    let mut closer = 0u64;
    for (m, &x) in models.iter().zip(xs) {
        let amount_out = m.pool.quote_exact_in(m.leg.i, m.leg.j, x)?;
        if !x.is_zero() {
            gas = gas.checked_add(m.pool.hop_gas).ok_or(RouteError::Math)?;
            if exact_in_only(m.pool) {
                closer = closer_gas;
            }
        }
        out = out.checked_add(amount_out).ok_or(RouteError::Math)?;
        allocs.push(Allocation {
            leg: m.leg,
            amount_in: x,
            amount_out,
        });
    }
    let gas = gas.checked_add(closer).ok_or(RouteError::Math)?;
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
    solve_on_with(pools, legs, total, gas, budget, 0)
}

/// [`solve_on`] where a pool that sells only an exact input (Curve) also
/// costs `closer_gas`: in an exit it overbuys what is owed, and a closer
/// sells the surplus back into WETH.
///
/// The set grows from the best marginal price, and that pool may be the
/// wrong one alone: dearer in gas than its price gains, or too shallow for
/// the size. At block 26,098,187, 0.8 WETH → WBTC through Curve tricrypto
/// was 0.00002 WETH better than V3 0.3 % and about 150k gas dearer with its
/// closer, and V3 0.3 % alone beat the lower-fee V3 pools ahead of it in
/// price. So each pool alone, and a set grown within each class of pools
/// no dearer in gas than a given one, compete too: the exit that leaves the
/// most after gas is kept (ties to the set grown on price).
fn solve_on_with(
    pools: &[Pool],
    legs: &[Leg],
    total: U256,
    gas: &GasTerms,
    budget: &SolveBudget,
    closer_gas: u64,
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
            Err(e @ (RouteError::StalePool | RouteError::InsufficientLiquidity)) => {
                tracing::debug!(pool = ?pool.address, venue = ?pool.venue(), error = %e, "leg excluded from solve");
            }
            Err(e) => return Err(e),
        }
    }
    if models.is_empty() {
        return Err(RouteError::InsufficientLiquidity);
    }
    models.sort_by_key(|m| std::cmp::Reverse(m.rho0));
    let rho0 = models.first().map_or(U256::ZERO, |m| m.rho0);
    let net = |q: &ExitQuote| -> Result<U256, RouteError> {
        Ok(q.amount_out.saturating_sub(gas.cost_in_out(q.hop_gas)?))
    };
    let mut best = grow(&models, total, gas, budget, closer_gas, rho0);
    let mut consider = |q: Result<ExitQuote, RouteError>| -> Result<(), RouteError> {
        let Ok(q) = q else { return Ok(()) };
        let replace = match &best {
            Ok(b) => net(&q)? > net(b)?,
            Err(_) => true,
        };
        if replace {
            best = Ok(q);
        }
        Ok(())
    };
    if models.len() > 1 {
        for m in &models {
            consider(grow(
                std::slice::from_ref(m),
                total,
                gas,
                budget,
                closer_gas,
                rho0,
            ))?;
        }
    }
    // Every pool water-filled, then the ones that do not pay their hop gas
    // dropped: no prefix of the `ρ₀` order is assumed to be the right set.
    if models.len() > 1 && budget.max_pools > 1 {
        consider(prune(&models, total, gas, budget, closer_gas, rho0))?;
    }
    let mut levels: SmallVec<[u64; 8]> = models.iter().map(|m| model_gas(m, closer_gas)).collect();
    levels.sort_unstable();
    levels.dedup();
    // The dearest class is every pool: the set already grown on price.
    levels.pop();
    for level in levels {
        let class: SmallVec<[Model<'_>; 8]> = models
            .iter()
            .filter(|m| model_gas(m, closer_gas) <= level)
            .cloned()
            .collect();
        if class.len() > 1 {
            consider(grow(&class, total, gas, budget, closer_gas, rho0))?;
        }
    }
    best
}

/// The greedy pool-set growth of [`solve_on_with`] over `models` (best `ρ₀`
/// first), reporting `rho0` as the exit's best marginal.
fn grow(
    models: &[Model<'_>],
    total: U256,
    gas: &GasTerms,
    budget: &SolveBudget,
    closer_gas: u64,
    rho0: U256,
) -> Result<ExitQuote, RouteError> {
    let max_pools = usize::from(budget.max_pools).clamp(1, models.len());
    let mut best: Option<ExitQuote> = None;
    for k in 1..=max_pools {
        let set = models.get(..k).ok_or(RouteError::Math)?;
        let xs = match water_fill(set, total, budget) {
            Ok(xs) => xs,
            Err(RouteError::InsufficientLiquidity) => continue,
            Err(e) => return Err(e),
        };
        let (allocs, out, hop_gas) = match quote_allocs(set, &xs, closer_gas) {
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
            unwrap: None,
            hub: None,
            chain: None,
        });
    }
    best.ok_or(RouteError::InsufficientLiquidity)
}

/// The pool set chosen on what each pool adds instead of on `ρ₀` order.
/// `total` is water-filled across every model; an allocated pool's
/// contribution is its output beyond what the other pools would pay for its
/// input at the fill's common marginal (`ρ*²` out per in). Every pool whose
/// contribution does not pay its hop gas, or that took nothing, is dropped
/// and the rest refilled, until all pay; past `budget.max_pools` the least
/// contributing go too. The estimate is first order and errs low: the
/// others' marginal falls as they take a dropped pool's input, so a pool is
/// worth at least its contribution.
///
/// The greedy growth ([`grow`]) takes pools in `ρ₀` order up to
/// `max_pools`: shallow pools with a good spot price fill the set before
/// deep ones. At block 25,791,740 three pools that together took 7,200 of
/// 3.2M USDT held three of the six places, and Pancake WETH/USDT 0.05 %
/// (731k USDT of depth, where the winner sold) and Uniswap V2 USDT/WETH had
/// none.
fn prune(
    models: &[Model<'_>],
    total: U256,
    gas: &GasTerms,
    budget: &SolveBudget,
    closer_gas: u64,
    rho0: U256,
) -> Result<ExitQuote, RouteError> {
    let mut set: Vec<Model<'_>> = models.to_vec();
    let mut last: Option<ExitQuote> = None;
    loop {
        let (xs, level) = match water_fill_level(&set, total, budget) {
            Ok(r) => r,
            // Dropping left too little depth: the set before stands.
            Err(RouteError::InsufficientLiquidity) => {
                return last.ok_or(RouteError::InsufficientLiquidity)
            }
            Err(e) => return Err(e),
        };
        let (allocs, out, hop_gas) = quote_allocs(&set, &xs, closer_gas)?;
        last = Some(ExitQuote {
            allocs,
            amount_in: total,
            amount_out: out,
            hop_gas,
            rho0,
            unwrap: None,
            hub: None,
            chain: None,
        });
        let Some(rho) = level else {
            return last.ok_or(RouteError::InsufficientLiquidity);
        };
        // (contribution − gas, index) of every allocated pool.
        let mut scored: Vec<(I512, usize)> = Vec::with_capacity(set.len());
        let mut keep: Vec<bool> = vec![false; set.len()];
        for (k, (m, &x)) in set.iter().zip(&xs).enumerate() {
            if x.is_zero() {
                continue;
            }
            let got = m.pool.quote_exact_in(m.leg.i, m.leg.j, x)?;
            let at_margin = mul_div_512(mul_div_512(x, rho, Q96)?, rho, Q96)?;
            let cost = gas.cost_in_out(model_gas(m, closer_gas))?;
            let net = diff(got, at_margin)?
                .checked_sub(i512(cost))
                .ok_or(RouteError::Math)?;
            scored.push((net, k));
        }
        scored.sort_by_key(|s| std::cmp::Reverse(s.0));
        let cap = usize::from(budget.max_pools);
        let mut kept = 0usize;
        for &(net, k) in &scored {
            if !net.is_negative() && kept < cap {
                if let Some(f) = keep.get_mut(k) {
                    *f = true;
                }
                kept = kept.saturating_add(1);
            }
        }
        // Always keep the best pool, whatever its gas: the sale must go
        // somewhere.
        if kept == 0 {
            if let Some(&(_, k)) = scored.first() {
                if let Some(f) = keep.get_mut(k) {
                    *f = true;
                }
                kept = 1;
            }
        }
        if kept == set.len() {
            return last.ok_or(RouteError::InsufficientLiquidity);
        }
        set = set
            .into_iter()
            .zip(keep)
            .filter_map(|(m, k)| k.then_some(m))
            .collect();
    }
}

/// [`solve_on`] through `unwrap` when set: the unwrap's output is what the
/// pools sell (nothing to sell when it is already `debt`). The unwrap is
/// quoted on the book it came from (a Curve LP reads its pool there).
#[allow(clippy::too_many_arguments)] // each input is a distinct solve term
fn solve_via(
    pools: &[Pool],
    legs: &[Leg],
    unwrap: Option<(&Unwrap, &PoolBook)>,
    debt: AssetId,
    total: U256,
    gas: &GasTerms,
    budget: &SolveBudget,
    closer_gas: u64,
) -> Result<ExitQuote, RouteError> {
    let Some((u, book)) = unwrap else {
        return solve_on_with(pools, legs, total, gas, budget, closer_gas);
    };
    let inner = u.convert(total, book)?;
    if inner.is_zero() {
        return Err(RouteError::InsufficientLiquidity);
    }
    let mut q = if u.into == debt {
        ExitQuote {
            allocs: SmallVec::new(),
            amount_in: inner,
            amount_out: inner,
            hop_gas: 0,
            rho0: U256::ZERO,
            unwrap: None,
            hub: None,
            chain: None,
        }
    } else {
        solve_on_with(pools, legs, inner, gas, budget, closer_gas)?
    };
    q.rho0 = if u.into == debt {
        u.rho(book)?
    } else {
        u.scale_rho(q.rho0, book)?
    };
    q.amount_in = total;
    q.hop_gas = q.hop_gas.checked_add(u.gas).ok_or(RouteError::Math)?;
    q.unwrap = Some(UnwrapUse {
        kind: u.kind,
        wrapper: u.wrapper,
        into: u.into,
        amount_out: inner,
    });
    Ok(q)
}

/// The exit through the hub: what is sold (the collateral, or what it
/// unwraps into) water-filled into the hub, then all the hub that pays
/// water-filled into the debt. Each step chooses its pool set on its own
/// output, the first in hub units (wei: the hub is WETH).
#[allow(clippy::too_many_arguments)] // each input is a distinct solve term
fn solve_hub(
    pools: &[Pool],
    into_hub: &[Leg],
    out_of_hub: &[Leg],
    hub: AssetId,
    unwrap: Option<(&Unwrap, &PoolBook)>,
    total: U256,
    gas: &GasTerms,
    budget: &SolveBudget,
    last: &SolveBudget,
    closer_gas: u64,
) -> Result<ExitQuote, RouteError> {
    solve_hub_priced(
        pools,
        into_hub,
        out_of_hub,
        hub,
        unwrap,
        total,
        gas,
        budget,
        last,
        closer_gas,
        OUT_PER_ETH_WETH,
    )
}

/// [`solve_hub`] through any hub token, its units per `1e18` wei being
/// `hub_per_eth` (the first leg's gas is charged in them).
#[allow(clippy::too_many_arguments)]
fn solve_hub_priced(
    pools: &[Pool],
    into_hub: &[Leg],
    out_of_hub: &[Leg],
    hub: AssetId,
    unwrap: Option<(&Unwrap, &PoolBook)>,
    total: U256,
    gas: &GasTerms,
    budget: &SolveBudget,
    last: &SolveBudget,
    closer_gas: u64,
    hub_per_eth: U256,
) -> Result<ExitQuote, RouteError> {
    if total.is_zero() {
        return Err(RouteError::BadLeg);
    }
    let sold = match unwrap {
        Some((u, book)) => u.convert(total, book)?,
        None => total,
    };
    if sold.is_zero() {
        return Err(RouteError::InsufficientLiquidity);
    }
    let in_hub = GasTerms {
        out_per_eth: hub_per_eth,
        ..*gas
    };
    let first = solve_on(pools, into_hub, sold, &in_hub, budget)?;
    if first.amount_out.is_zero() {
        return Err(RouteError::InsufficientLiquidity);
    }
    let second = solve_on_with(pools, out_of_hub, first.amount_out, gas, last, closer_gas)?;
    let mut rho0 = mul_div_512(first.rho0, second.rho0, Q96)?;
    let mut hop_gas = first
        .hop_gas
        .checked_add(second.hop_gas)
        .ok_or(RouteError::Math)?;
    let mut used = None;
    if let Some((u, book)) = unwrap {
        rho0 = u.scale_rho(rho0, book)?;
        hop_gas = hop_gas.checked_add(u.gas).ok_or(RouteError::Math)?;
        used = Some(UnwrapUse {
            kind: u.kind,
            wrapper: u.wrapper,
            into: u.into,
            amount_out: sold,
        });
    }
    Ok(ExitQuote {
        allocs: second.allocs,
        amount_in: total,
        amount_out: second.amount_out,
        hop_gas,
        rho0,
        unwrap: used,
        hub: Some(Box::new(HubUse {
            hub,
            allocs: first.allocs,
            amount_in: sold,
            amount_out: first.amount_out,
        })),
        chain: None,
    })
}

/// Of a direct exit and one through the hub, the one that leaves more debt
/// after its hop gas. A direct exit buys only what is owed and closes the
/// collateral it leaves into WETH through one more pool; an exit through
/// the hub sold all of it into WETH already. So the direct exit is charged
/// that hop too, at the hop gas of the hub exit's first pool. When one
/// fails, the other stands.
fn better(
    direct: Result<ExitQuote, RouteError>,
    via_hub: Result<ExitQuote, RouteError>,
    pools: &[Pool],
    gas: &GasTerms,
) -> Result<ExitQuote, RouteError> {
    match (direct, via_hub) {
        (Ok(d), Ok(h)) => {
            let closer = h
                .hub
                .as_ref()
                .and_then(|u| u.allocs.first())
                .and_then(|a| pools.get(usize::try_from(a.leg.pool.0).ok()?))
                .map_or(0, |p| p.hop_gas);
            let direct_net = d
                .amount_out
                .saturating_sub(gas.cost_in_out(d.hop_gas.saturating_add(closer))?);
            let hub_net = h.amount_out.saturating_sub(gas.cost_in_out(h.hop_gas)?);
            Ok(if hub_net > direct_net { h } else { d })
        }
        (Ok(q), Err(_)) | (Err(_), Ok(q)) => Ok(q),
        (Err(d), Err(h)) => Err(match d {
            RouteError::InsufficientLiquidity | RouteError::StalePool => h,
            e => e,
        }),
    }
}

/// [`solve_on`] over the book's live pools for one pair, unwrapping the
/// collateral first when it has no pool of its own ([`PoolBook::exit_source`]),
/// or through the hub ([`PoolBook::hub_route`]) when that leaves more debt
/// after gas.
pub fn solve_pair(
    book: &PoolBook,
    asset_in: AssetId,
    asset_out: AssetId,
    total: U256,
    gas: &GasTerms,
    budget: &SolveBudget,
) -> Result<ExitQuote, RouteError> {
    // Seized in the debt asset itself (a WETH/WETH loop): nothing to swap.
    // The whole seize repays, at no hop gas and a marginal of one.
    if asset_in == asset_out {
        return Ok(ExitQuote {
            allocs: SmallVec::new(),
            amount_in: total,
            amount_out: total,
            hop_gas: 0,
            rho0: crate::solver::Q96,
            unwrap: None,
            hub: None,
            chain: None,
        });
    }
    let closer = exact_in_closer_gas(book, asset_out);
    let direct = match book.exit_source(asset_in, asset_out) {
        ExitSource::Direct(legs) => solve_on_with(book.pools(), legs, total, gas, budget, closer),
        ExitSource::Unwrap(u, legs) => solve_via(
            book.pools(),
            legs,
            Some((u, book)),
            asset_out,
            total,
            gas,
            budget,
            closer,
        ),
    };
    let best = match book.hub_route(asset_in, asset_out) {
        None => direct,
        Some(route) => {
            let via_hub = solve_hub(
                book.pools(),
                route.into_hub,
                route.out_of_hub,
                route.hub,
                route.unwrap.map(|u| (u, book)),
                total,
                gas,
                budget,
                budget,
                closer,
            );
            better(direct, via_hub, book.pools(), gas)
        }
    };
    let best = graph_hubs(book, asset_in, asset_out, total, gas, budget, closer, best);
    graph_chain(book, asset_in, asset_out, total, gas, best)
}

/// A sized leg's exit, refined once (`profit::evaluate`): split across the
/// direct pools and pool-disjoint chains ([`split_routes`]), then, worth
/// [`FLOW_MIN_ETH`] or more, routed as a flow across the whole graph
/// ([`flow_split`]). Neither runs inside [`solve_pair`]: sizing quotes an
/// exit many times, and these are the costly part of a large one.
pub fn refine_exit(
    book: &PoolBook,
    asset_in: AssetId,
    asset_out: AssetId,
    gas: &GasTerms,
    budget: &SolveBudget,
    exit: ExitQuote,
) -> Result<ExitQuote, RouteError> {
    let total = exit.amount_in;
    let closer = exact_in_closer_gas(book, asset_out);
    let best = split_routes(book, asset_out, gas, budget, closer, Ok(exit));
    flow_split(book, asset_in, asset_out, total, gas, best)
}

/// Sales whose best exit is worth at least this many ETH are also routed as
/// a flow ([`flow_split`]).
pub const FLOW_MIN_ETH: u64 = 5;
/// Slices a flow divides a sale into: 2 % each, as fine as the direct
/// pools' own split needs to be matched.
const FLOW_SLICES: u64 = 50;
/// Chain legs a flow may spread over: once that many distinct paths have
/// taken slices, the rest go along them.
const FLOW_PATHS: usize = 32;
/// Every how many slices the graph is searched for new paths. The search
/// is the flow's cost (about a thousand quotes a slice); between searches
/// the slices go to the paths already found, each re-quoted on the pools
/// as the earlier slices left them, so no slice is priced on stale state —
/// only a path that first pays once the others have moved waits for the
/// next search, by when it was already close. Each search keeps a route
/// per hop count as a candidate, not only the winner.
const FLOW_SEARCH_EVERY: u64 = 4;
/// Trials the balancing pass may run after the slices: each shifts a part
/// of a slice from the path paying least at the margin to the one paying
/// most, kept when the sale pays more for it; a trial that does not pay
/// halves the part, down to [`FLOW_BALANCE_FINEST`]th of a slice.
const FLOW_BALANCE_TRIALS: usize = 64;
/// The smallest part of a slice the balancing pass moves.
const FLOW_BALANCE_FINEST: u64 = 64;

/// One path a flow sends slices along.
struct FlowPath {
    hops: SmallVec<[Leg; 4]>,
    data: Vec<u8>,
    exact_out: bool,
    route_gas: u64,
    sold: U256,
    out: U256,
    /// The direct pools, water-filled as one route (no `hops`): a slice is
    /// quoted and applied across them by [`solve_on_with`].
    direct: bool,
}

/// A large sale routed as a flow across the whole graph (plan 4E). Run
/// once on a leg's sized exit (`profit::evaluate`), not inside every quote
/// the sizing makes: when that exit is worth at least [`FLOW_MIN_ETH`], the
/// sale is cut into [`FLOW_SLICES`] slices, and each goes along the path
/// that pays most for it on a working copy of the book, which then takes
/// that slice's swaps: a later slice sees the pools the earlier ones moved,
/// so the flow spreads across parallel pools on every hop and paths may
/// share a pool. A path is any chain of one to four hops through every
/// curated venue; slices on the same path merge into one chain leg, its
/// output the sum of its slices'. Paths are ranked by output alone (a
/// path's gas is paid once, however many slices take it); the flow is kept
/// when, after the gas of every leg, it nets more than `best`.
pub fn flow_split(
    book: &PoolBook,
    asset_in: AssetId,
    asset_out: AssetId,
    total: U256,
    gas: &GasTerms,
    best: Result<ExitQuote, RouteError>,
) -> Result<ExitQuote, RouteError> {
    let Ok(b) = &best else {
        return flow_kept(best, "a route could not be quoted");
    };
    let big = gas
        .out_per_eth
        .checked_mul(U256::from(FLOW_MIN_ETH))
        .is_some_and(|min| !min.is_zero() && b.amount_out >= min);
    if book.graph().is_none() || !big {
        return flow_kept(best, "a route could not be quoted");
    }
    let (start, sold, wrapper) = match &b.unwrap {
        Some(u) => (u.into, u.amount_out, book.unwrap_of(u.wrapper)),
        None => (asset_in, total, None),
    };
    let slice = sold
        .checked_div(U256::from(FLOW_SLICES))
        .unwrap_or_default();
    if slice.is_zero() {
        return flow_kept(best, "sale too small to slice");
    }
    let gross = GasTerms {
        base_fee_wei: 0,
        priority_fee_wei: 0,
        ..*gas
    };
    // The direct pools as one route, water-filled: a single-hop path is
    // one of them, so the path search starts at two hops when they exist.
    let legs = book.legs(start, asset_out);
    let direct = legs
        .iter()
        .any(|l| book.get(l.pool).is_some_and(Pool::is_live));
    let budget = SolveBudget::default();
    let min_hops = if direct { 2 } else { 1 };
    let mut paths: Vec<FlowPath> = Vec::new();
    if direct {
        paths.push(FlowPath {
            hops: SmallVec::new(),
            data: Vec::new(),
            exact_out: true,
            route_gas: 0,
            sold: U256::ZERO,
            out: U256::ZERO,
            direct: true,
        });
    }
    let mut work = book.clone();
    let mut left = sold;
    let mut quotes = 0u32;
    let mut slice_no = 0u64;
    while !left.is_zero() {
        let search_now = slice_no.is_multiple_of(FLOW_SEARCH_EVERY);
        slice_no = slice_no.saturating_add(1);
        // The last slice takes the remainder.
        let s = if left < slice.saturating_mul(U256::from(2u64)) {
            left
        } else {
            slice
        };
        // The known paths, re-quoted on the pools as they stand.
        let mut pick: Option<(usize, U256)> = None;
        for (k, p) in paths.iter().enumerate() {
            if p.direct {
                if let Ok(q) = solve_on_with(work.pools(), legs, s, &gross, &budget, 0) {
                    if pick.is_none_or(|(_, o)| q.amount_out > o) {
                        pick = Some((k, q.amount_out));
                    }
                }
                continue;
            }
            let mut x = s;
            let mut ok = true;
            for l in &p.hops {
                match work
                    .get(l.pool)
                    .and_then(|q| q.quote_exact_in(l.i, l.j, x).ok())
                {
                    Some(o) => x = o,
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok && pick.is_none_or(|(_, o)| x > o) {
                pick = Some((k, x));
            }
        }
        // And, every [`FLOW_SEARCH_EVERY`]th slice, the paths paying most
        // for this slice on those pools, found now: new ones join as
        // candidates (re-quoted on every later slice) while the plan has
        // room for another chain leg.
        let chains = paths
            .iter()
            .filter(|p| !p.direct && !p.sold.is_zero())
            .count();
        if search_now && chains < FLOW_PATHS {
            let (fresh, q) = crate::graph::best_slice_chains(&work, start, asset_out, s, min_hops);
            quotes = quotes.saturating_add(q);
            for (route, hops, data, exact_out) in fresh {
                let k = match paths.iter().position(|p| p.data == data) {
                    Some(k) => k,
                    None => {
                        paths.push(FlowPath {
                            hops,
                            data,
                            exact_out,
                            route_gas: route.hop_gas,
                            sold: U256::ZERO,
                            out: U256::ZERO,
                            direct: false,
                        });
                        paths.len().saturating_sub(1)
                    }
                };
                if pick.is_none_or(|(_, o)| route.amount_out > o) {
                    pick = Some((k, route.amount_out));
                }
            }
        }
        let Some((k, _)) = pick else {
            return flow_kept(best, "no path takes a slice: every pool full to its window");
        };
        let Some(p) = paths.get_mut(k) else {
            return flow_kept(best, "path index");
        };
        let mut x = s;
        if p.direct {
            // Applied across the direct pools as the water-fill divides it.
            let Ok(q) = solve_on_with(work.pools(), legs, s, &gross, &budget, 0) else {
                return flow_kept(best, "the direct pools refused the slice when applied");
            };
            x = U256::ZERO;
            for a in q.allocs.iter().filter(|a| !a.amount_in.is_zero()) {
                let Some(pool) = work.get_mut(a.leg.pool) else {
                    return flow_kept(best, "pool missing from the working copy");
                };
                let Ok(o) = pool.apply_exact_in(a.leg.i, a.leg.j, a.amount_in) else {
                    return flow_kept(best, "a direct pool refused its share when applied");
                };
                x = x.saturating_add(o);
            }
        } else {
            for l in &p.hops {
                let Some(pool) = work.get_mut(l.pool) else {
                    return flow_kept(best, "pool missing from the working copy");
                };
                let Ok(o) = pool.apply_exact_in(l.i, l.j, x) else {
                    return flow_kept(best, "a quoted hop refused the slice when applied");
                };
                x = o;
            }
        }
        p.sold = p.sold.saturating_add(s);
        p.out = p.out.saturating_add(x);
        left = left.saturating_sub(s);
    }
    // The slices were accounted as they interleaved; the plan runs each
    // path whole, in order. Each path's output is what that order pays,
    // and the pass that follows judges every move by it.
    let Some((outs, run)) = flow_run(book, legs, &paths, &gross, &budget) else {
        return flow_kept(best, "the paths could not be run in plan order");
    };
    for (p, o) in paths.iter_mut().zip(&outs) {
        p.out = *o;
    }
    work = run;
    let mut total: U256 = outs.iter().fold(U256::ZERO, |a, o| a.saturating_add(*o));
    // Balancing: the greedy slices leave the paths' marginal rates unequal
    // by up to a slice's slippage. Part of a slice moves from the path
    // paying least for one more to the path paying most, as long as the
    // sale as a whole pays more for it; the forward marginals that choose
    // the pair ignore that the giver's last part paid more than its next
    // would, so a move that does not pay halves the part and tries again.
    let finest = slice
        .checked_div(U256::from(FLOW_BALANCE_FINEST))
        .unwrap_or(U256::ONE)
        .max(U256::ONE);
    let mut delta = slice
        .checked_div(U256::from(4u64))
        .unwrap_or(U256::ONE)
        .max(U256::ONE);
    let mut moves = 0usize;
    for _ in 0..FLOW_BALANCE_TRIALS {
        if delta < finest {
            break;
        }
        let mut marg: Vec<Option<U256>> = Vec::with_capacity(paths.len());
        for p in &paths {
            let m = if p.direct {
                solve_on_with(work.pools(), legs, delta, &gross, &budget, 0)
                    .ok()
                    .map(|q| q.amount_out)
            } else {
                let mut x = delta;
                let mut ok = true;
                for l in &p.hops {
                    match work
                        .get(l.pool)
                        .and_then(|q| q.quote_exact_in(l.i, l.j, x).ok())
                    {
                        Some(o) => x = o,
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                ok.then_some(x)
            };
            marg.push(m);
        }
        let hi = marg
            .iter()
            .enumerate()
            .filter_map(|(k, m)| m.map(|m| (k, m)))
            .max_by_key(|&(_, m)| m);
        let lo = marg
            .iter()
            .enumerate()
            .filter_map(|(k, m)| m.map(|m| (k, m)))
            .filter(|&(k, _)| paths.get(k).is_some_and(|p| p.sold >= delta))
            .min_by_key(|&(_, m)| m);
        let (Some((hi, m_hi)), Some((lo, m_lo))) = (hi, lo) else {
            break;
        };
        if hi == lo || m_hi <= m_lo {
            break;
        }
        let mut trial: Vec<FlowPath> = paths
            .iter()
            .map(|p| FlowPath {
                hops: p.hops.clone(),
                data: p.data.clone(),
                exact_out: p.exact_out,
                route_gas: p.route_gas,
                sold: p.sold,
                out: p.out,
                direct: p.direct,
            })
            .collect();
        if let Some(p) = trial.get_mut(lo) {
            p.sold = p.sold.saturating_sub(delta);
        }
        if let Some(p) = trial.get_mut(hi) {
            p.sold = p.sold.saturating_add(delta);
        }
        let Some((outs, run)) = flow_run(book, legs, &trial, &gross, &budget) else {
            break;
        };
        let t: U256 = outs.iter().fold(U256::ZERO, |a, o| a.saturating_add(*o));
        if t <= total {
            delta = delta.checked_div(U256::from(2u64)).unwrap_or(U256::ZERO);
            continue;
        }
        for (p, o) in trial.iter_mut().zip(&outs) {
            p.out = *o;
        }
        paths = trial;
        work = run;
        total = t;
        moves = moves.saturating_add(1);
    }
    if tracing::enabled!(tracing::Level::DEBUG) {
        for p in &paths {
            let hops: Vec<String> = p
                .hops
                .iter()
                .map(|l| {
                    book.get(l.pool)
                        .map_or_else(|| format!("{}", l.pool.0), |q| format!("{:?}", q.address))
                })
                .collect();
            tracing::debug!(
                direct = p.direct,
                hops = ?hops,
                sold = %p.sold,
                out = %p.out,
                "flow path"
            );
        }
    }
    paths.retain(|p| !p.sold.is_zero());
    if paths.len() < 2 {
        return flow_kept(best, "every slice went to one route");
    }
    let mut chains: Vec<ChainUse> = Vec::with_capacity(paths.len());
    let mut allocs = SmallVec::new();
    let mut amount_out = U256::ZERO;
    let mut hop_gas = 0u64;
    let mut rho0 = Q96;
    for p in &paths {
        let x = p.out;
        if p.direct {
            // The water-fill of what the direct pools sold, on the book as
            // it stands: the repay's shares.
            let Ok(q) = solve_on_with(book.pools(), legs, p.sold, gas, &budget, 0) else {
                return flow_kept(
                    best,
                    "the direct pools could not be re-solved at their share",
                );
            };
            allocs = q.allocs;
            amount_out = amount_out.saturating_add(x);
            hop_gas = hop_gas.saturating_add(q.hop_gas);
            rho0 = rho0.max(q.rho0);
            continue;
        }
        let mut rho = Q96;
        for l in &p.hops {
            let Some(r) = book.get(l.pool).and_then(|q| q.rho_at_zero(l.i, l.j).ok()) else {
                return flow_kept(best, "a route could not be quoted");
            };
            let Ok(m) = mul_div_512(rho, r, Q96) else {
                return flow_kept(best, "a route could not be quoted");
            };
            rho = m;
        }
        if let Some(u) = wrapper {
            let Ok(scaled) = u.scale_rho(rho, book) else {
                return flow_kept(best, "a route could not be quoted");
            };
            rho = scaled;
        }
        rho0 = rho0.max(rho);
        amount_out = amount_out.saturating_add(x);
        hop_gas = hop_gas.saturating_add(p.route_gas);
        chains.push(ChainUse {
            hops: p.hops.clone(),
            data: p.data.clone(),
            amount_out: x,
            with: Vec::new(),
            exact_out: p.exact_out,
        });
    }
    // The unwrap and the leftover's way into WETH, as for any chain exit.
    if let Some(u) = wrapper {
        hop_gas = hop_gas.saturating_add(u.gas);
    }
    if let Some(weth) = book.hub().filter(|&w| w != start) {
        let direct_close = book
            .legs(start, weth)
            .iter()
            .any(|l| book.get(l.pool).is_some_and(Pool::is_live));
        if !direct_close {
            let weth_gas = GasTerms {
                out_per_eth: OUT_PER_ETH_WETH,
                ..*gas
            };
            match crate::graph::best_chain(book, start, weth, sold, &weth_gas, 1) {
                Some(c) => hop_gas = hop_gas.saturating_add(c.0.hop_gas),
                None => return best,
            }
        }
    }
    if chains.is_empty() {
        return flow_kept(best, "every slice went to the direct pools");
    }
    chains.sort_by_key(|c| core::cmp::Reverse(c.amount_out));
    let mut lead = chains.remove(0);
    lead.with = chains;
    let flow = ExitQuote {
        allocs,
        amount_in: b.amount_in,
        amount_out,
        hop_gas,
        rho0,
        unwrap: b.unwrap,
        hub: None,
        chain: Some(Box::new(lead)),
    };
    let net = |q: &ExitQuote| -> Result<U256, RouteError> {
        Ok(q.amount_out.saturating_sub(gas.cost_in_out(q.hop_gas)?))
    };
    let (f, b_net) = (net(&flow)?, net(b)?);
    tracing::info!(
        paths = flow.chain.as_ref().map_or(0, |c| c.with.len().saturating_add(1)),
        quotes,
        moves,
        flow_out = %flow.amount_out,
        flow_net = %f,
        best_out = %b.amount_out,
        best_net = %b_net,
        kept = f > b_net,
        "flow"
    );
    if f > b_net {
        Ok(flow)
    } else {
        best
    }
}

/// The paths run whole and in order on a fresh copy of the book, as the
/// plan runs them: each path's output, and the copy they leave. `None`
/// when a path cannot take its amount.
fn flow_run(
    book: &PoolBook,
    legs: &[Leg],
    paths: &[FlowPath],
    gross: &GasTerms,
    budget: &SolveBudget,
) -> Option<(Vec<U256>, PoolBook)> {
    let mut work = book.clone();
    let mut outs = Vec::with_capacity(paths.len());
    for p in paths {
        if p.sold.is_zero() {
            outs.push(U256::ZERO);
            continue;
        }
        let mut x = p.sold;
        if p.direct {
            let q = solve_on_with(work.pools(), legs, p.sold, gross, budget, 0).ok()?;
            x = U256::ZERO;
            for a in q.allocs.iter().filter(|a| !a.amount_in.is_zero()) {
                let o = work
                    .get_mut(a.leg.pool)?
                    .apply_exact_in(a.leg.i, a.leg.j, a.amount_in)
                    .ok()?;
                x = x.saturating_add(o);
            }
        } else {
            for l in &p.hops {
                x = work.get_mut(l.pool)?.apply_exact_in(l.i, l.j, x).ok()?;
            }
        }
        outs.push(x);
    }
    Some((outs, work))
}

/// `best`, logging why the flow was not taken.
fn flow_kept(
    best: Result<ExitQuote, RouteError>,
    why: &'static str,
) -> Result<ExitQuote, RouteError> {
    tracing::info!(why, "flow not taken");
    best
}

/// Slices [`split_routes`] divides a sale into: each goes to the route that
/// pays most for it next, so the split is the best on a grid of 1 %.
const SPLIT_SLICES: u32 = 100;
/// Chains, besides the direct pools, a sale is split across.
const SPLIT_CHAINS: usize = 3;

/// The least input along `hops` whose output is at least `want`: doubling
/// from one unit of the input (its units are not the output's), then
/// bisection on the exact path quote. `None` when the path cannot deliver
/// `want`: a hop refuses once the path has answered, before the output is
/// reached. A hop that refuses a size below its smallest swap (a Fluid pool
/// takes no less than 1e6 of its 12-decimal units) is no verdict: doubling
/// goes on until the path answers.
pub(crate) fn path_in_for(book: &PoolBook, hops: &[Leg], want: U256) -> Option<U256> {
    if want.is_zero() {
        return Some(U256::ZERO);
    }
    let mut hi = U256::ONE;
    let mut answered = false;
    loop {
        match path_out(book, hops, hi) {
            Some(o) if o >= want => break,
            // A zero output is no answer either: a hop rounded the dust away.
            Some(o) => {
                answered |= !o.is_zero();
                hi = hi.checked_mul(U256::from(2u64))?;
            }
            None if answered => return None,
            None => hi = hi.checked_mul(U256::from(2u64))?,
        }
    }
    let mut lo = U256::ZERO;
    while hi.saturating_sub(lo) > U256::ONE {
        let mid = lo.saturating_add(hi.saturating_sub(lo).checked_div(U256::from(2u64))?);
        if path_out(book, hops, mid).is_some_and(|o| o >= want) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Some(hi)
}

/// Output of `amount` sold along `hops`, each hop quoted exactly.
pub(crate) fn path_out(book: &PoolBook, hops: &[Leg], amount: U256) -> Option<U256> {
    let mut x = amount;
    for l in hops {
        x = book.get(l.pool)?.quote_exact_in(l.i, l.j, x).ok()?;
    }
    Some(x)
}

/// One route a sale can be split across: the direct pools, or a chain.
enum SplitRoute {
    Direct,
    Chain {
        route_gas: u64,
        hops: SmallVec<[Leg; 4]>,
        data: Vec<u8>,
        exact_out: bool,
    },
}

/// A sale split across the direct pools and up to [`SPLIT_CHAINS`] chains
/// (plan 4E). The exit `best` found sells through direct pools or along one
/// chain, from the collateral or what it unwraps into; the search's other
/// chains of two to four hops from that token are added as long as none
/// shares a pool with the routes before it (direct pools hold the start
/// token and the debt, which a chain's hops never both do). Disjoint, each
/// route's output depends on what it alone sells, and is concave in it, so
/// handing each of [`SPLIT_SLICES`] slices to the route that pays most for
/// it next is the best split on that grid. It runs only when another
/// route's first slice pays more than `best`'s route its last, and is kept
/// when it nets more after the gas of every route it uses.
fn split_routes(
    book: &PoolBook,
    asset_out: AssetId,
    gas: &GasTerms,
    budget: &SolveBudget,
    closer: u64,
    best: Result<ExitQuote, RouteError>,
) -> Result<ExitQuote, RouteError> {
    let Ok(b) = &best else {
        return best;
    };
    if book.graph().is_none() || b.hub.is_some() {
        return best;
    }
    let primary_chain = b.chain.as_ref().filter(|c| c.with.is_empty());
    if b.chain.is_some() && (primary_chain.is_none() || !b.allocs.is_empty()) {
        return best;
    }
    // The token sold and how much of it.
    let first_asset = |l: &Leg| {
        book.get(l.pool)
            .and_then(|p| p.assets.get(usize::from(l.i)).copied())
    };
    let (start, sold) = match &b.unwrap {
        Some(u) => (u.into, u.amount_out),
        None => {
            let leg = b
                .allocs
                .first()
                .map(|a| a.leg)
                .or_else(|| primary_chain.and_then(|c| c.hops.first().copied()));
            match leg.and_then(|l| first_asset(&l)) {
                Some(a) => (a, b.amount_in),
                None => return best,
            }
        }
    };
    let slice = sold
        .checked_div(U256::from(SPLIT_SLICES))
        .unwrap_or_default();
    if slice.is_zero() {
        return best;
    }
    // The routes: the direct pools, then pool-disjoint chains.
    let legs = book.legs(start, asset_out);
    let mut routes: SmallVec<[SplitRoute; 4]> = SmallVec::new();
    let mut used: Vec<PoolId> = legs.iter().map(|l| l.pool).collect();
    if legs
        .iter()
        .any(|l| book.get(l.pool).is_some_and(Pool::is_live))
    {
        routes.push(SplitRoute::Direct);
    }
    for (route, hops, data, exact_out) in
        crate::graph::best_chains(book, start, asset_out, sold, gas, 2)
    {
        if routes.len() > SPLIT_CHAINS {
            break;
        }
        if hops.iter().any(|l| used.contains(&l.pool)) {
            continue;
        }
        used.extend(hops.iter().map(|l| l.pool));
        routes.push(SplitRoute::Chain {
            route_gas: route.hop_gas,
            hops,
            data,
            exact_out,
        });
    }
    if routes.len() < 2 {
        return best;
    }
    let direct = |x: U256| -> Option<ExitQuote> {
        if x.is_zero() {
            return None;
        }
        solve_on_with(book.pools(), legs, x, gas, budget, closer).ok()
    };
    let out = |r: &SplitRoute, x: U256| -> U256 {
        if x.is_zero() {
            return U256::ZERO;
        }
        match r {
            SplitRoute::Direct => direct(x).map_or(U256::ZERO, |q| q.amount_out),
            SplitRoute::Chain { hops, .. } => path_out(book, hops, x).unwrap_or(U256::ZERO),
        }
    };
    // Which route `best` sells through; does any other pay more for a first
    // slice than it pays for its last?
    let own = routes.iter().position(|r| match (r, primary_chain) {
        (SplitRoute::Direct, None) => true,
        (SplitRoute::Chain { data, .. }, Some(c)) => *data == c.data,
        _ => false,
    });
    let Some(own) = own else {
        return best;
    };
    let last = routes.get(own).map_or(U256::ZERO, |r| {
        out(r, sold).saturating_sub(out(r, sold.saturating_sub(slice)))
    });
    let first: SmallVec<[U256; 4]> = routes.iter().map(|r| out(r, slice)).collect();
    if !first.iter().enumerate().any(|(k, f)| k != own && *f > last) {
        return best;
    }
    // Greedy: each slice to the route paying most for it next.
    let n = routes.len();
    let mut alloc: SmallVec<[U256; 4]> = SmallVec::from_elem(U256::ZERO, n);
    let mut now: SmallVec<[U256; 4]> = SmallVec::from_elem(U256::ZERO, n);
    let mut next: SmallVec<[U256; 4]> = first.clone();
    let mut left = sold;
    while !left.is_zero() {
        let s = slice.min(left);
        let Some(k) = (0..n).max_by_key(|&k| {
            next.get(k)
                .zip(now.get(k))
                .map_or(U256::ZERO, |(a, b)| a.saturating_sub(*b))
        }) else {
            return best;
        };
        let (Some(a), Some(c), Some(r)) = (alloc.get_mut(k), now.get_mut(k), routes.get(k)) else {
            return best;
        };
        *a = a.saturating_add(s);
        *c = out(r, *a);
        left = left.saturating_sub(s);
        let peek = out(r, a.saturating_add(slice.min(left).max(U256::ONE)));
        if let Some(x) = next.get_mut(k) {
            *x = peek;
        }
    }
    // The split, quoted at its final allocation.
    let mut allocs = SmallVec::new();
    let mut amount_out = U256::ZERO;
    let mut hop_gas = 0u64;
    let mut chains: Vec<ChainUse> = Vec::new();
    let mut rho0 = b.rho0;
    let wrapper = b.unwrap.as_ref().and_then(|u| book.unwrap_of(u.wrapper));
    for (r, a) in routes.iter().zip(&alloc) {
        if a.is_zero() {
            continue;
        }
        match r {
            SplitRoute::Direct => {
                let Some(d) = direct(*a) else {
                    return best;
                };
                amount_out = amount_out.saturating_add(d.amount_out);
                hop_gas = hop_gas.saturating_add(d.hop_gas);
                allocs = d.allocs;
            }
            SplitRoute::Chain {
                route_gas,
                hops,
                data,
                exact_out,
            } => {
                let Some(o) = path_out(book, hops, *a) else {
                    return best;
                };
                let mut rho = Q96;
                for l in hops {
                    let Some(x) = book.get(l.pool).and_then(|p| p.rho_at_zero(l.i, l.j).ok())
                    else {
                        return best;
                    };
                    let Ok(m) = mul_div_512(rho, x, Q96) else {
                        return best;
                    };
                    rho = m;
                }
                // Through an unwrap, the marginal is in the wrapper, at its rate.
                if let Some(u) = wrapper {
                    let Ok(scaled) = u.scale_rho(rho, book) else {
                        return best;
                    };
                    rho = scaled;
                }
                rho0 = rho0.max(rho);
                amount_out = amount_out.saturating_add(o);
                hop_gas = hop_gas.saturating_add(*route_gas);
                chains.push(ChainUse {
                    hops: hops.clone(),
                    data: data.clone(),
                    amount_out: o,
                    with: Vec::new(),
                    exact_out: *exact_out,
                });
            }
        }
    }
    if chains.is_empty() {
        return best;
    }
    // The leftover's way into WETH and the unwrap, as `best` carried them.
    let carried = b.hop_gas.saturating_sub(match primary_chain {
        Some(_) => b.hop_gas.min(own_route_gas(&routes, own)),
        None => b.hop_gas.min(direct(sold).map_or(0, |d| d.hop_gas)),
    });
    hop_gas = hop_gas.saturating_add(carried);
    // The chain buying the most leads; the rest ride with it.
    chains.sort_by_key(|c| core::cmp::Reverse(c.amount_out));
    let mut lead = chains.remove(0);
    lead.with = chains;
    let split = ExitQuote {
        allocs,
        amount_in: b.amount_in,
        amount_out,
        hop_gas,
        rho0,
        unwrap: b.unwrap,
        hub: None,
        chain: Some(Box::new(lead)),
    };
    let net = |q: &ExitQuote| -> Result<U256, RouteError> {
        Ok(q.amount_out.saturating_sub(gas.cost_in_out(q.hop_gas)?))
    };
    if net(&split)? > net(b)? {
        Ok(split)
    } else {
        best
    }
}

/// The hop gas of `routes[k]` when it is a chain.
fn own_route_gas(routes: &[SplitRoute], k: usize) -> u64 {
    match routes.get(k) {
        Some(SplitRoute::Chain { route_gas, .. }) => *route_gas,
        _ => 0,
    }
}

/// The best of `best` and the best exact-output chain of two to four hops
/// through the book's graph ([`crate::graph::GraphRoutes::chain_graph`],
/// venue 10), from the collateral or, when it has a live unwrap, from what
/// it unwraps into (the unwrap runs first, exact input, as an unwrap exit's
/// does). A chain sells all it needs along one path and leaves the rest to
/// close into WETH, as a direct exit does, so it is weighed as one
/// ([`better`] charges it that closer). The leftover closes through a pool
/// into WETH or, without one, through an exact-input chain to WETH, whose
/// hops the exit is charged.
fn graph_chain(
    book: &PoolBook,
    asset_in: AssetId,
    asset_out: AssetId,
    total: U256,
    gas: &GasTerms,
    best: Result<ExitQuote, RouteError>,
) -> Result<ExitQuote, RouteError> {
    let (Some(_), Some(weth)) = (book.graph(), book.hub()) else {
        return best;
    };
    let mut candidates: SmallVec<[(AssetId, U256, Option<&Unwrap>); 2]> = SmallVec::new();
    candidates.push((asset_in, total, None));
    if let Some(u) = book
        .unwrap_of(asset_in)
        .filter(|u| u.is_live() && u.into != asset_out)
    {
        if let Ok(sold) = u.convert(total, book) {
            candidates.push((u.into, sold, Some(u)));
        }
    }
    let mut best = best;
    for (start, amount, unwrap) in candidates {
        let Some((chain, closes_in_gas)) =
            chain_exit(book, start, asset_out, amount, total, unwrap, weth, gas)
        else {
            continue;
        };
        best = match best {
            // A hub exit closes nothing; the chain closes its leftover,
            // charged by `better` unless its gas already carries the close.
            Ok(b) if b.hub.is_some() && !closes_in_gas => {
                better(Ok(chain), Ok(b), book.pools(), gas)
            }
            Ok(b) => {
                let b_net = b.amount_out.saturating_sub(gas.cost_in_out(b.hop_gas)?);
                let c_net = chain
                    .amount_out
                    .saturating_sub(gas.cost_in_out(chain.hop_gas)?);
                Ok(if c_net > b_net { chain } else { b })
            }
            Err(_) => Ok(chain),
        };
    }
    best
}

/// One chain exit from `start` (the collateral, or what `unwrap` makes of
/// `total` of it: `amount`), and whether its gas carries its leftover's way
/// into WETH (a chain, rather than a pool [`better`] charges).
#[allow(clippy::too_many_arguments)]
fn chain_exit(
    book: &PoolBook,
    start: AssetId,
    asset_out: AssetId,
    amount: U256,
    total: U256,
    unwrap: Option<&Unwrap>,
    weth: AssetId,
    gas: &GasTerms,
) -> Option<(ExitQuote, bool)> {
    use crate::graph::best_chain;
    // The leftover's way into WETH: a pool (charged as any direct exit's
    // closer, by `better`), or a chain whose hops this exit carries.
    let direct_close = start == weth
        || book
            .legs(start, weth)
            .iter()
            .any(|l| book.get(l.pool).is_some_and(Pool::is_live));
    let close_gas = if direct_close {
        0
    } else {
        let weth_gas = GasTerms {
            out_per_eth: OUT_PER_ETH_WETH,
            ..*gas
        };
        best_chain(book, start, weth, amount, &weth_gas, 1)?
            .0
            .hop_gas
    };
    let (route, hops, data, exact_out) = best_chain(book, start, asset_out, amount, gas, 2)?;
    // The path's marginal at zero size: the product of its hops'.
    let mut rho0 = Q96;
    for l in &hops {
        let r = book.get(l.pool)?.rho_at_zero(l.i, l.j).ok()?;
        rho0 = mul_div_512(rho0, r, Q96).ok()?;
    }
    let mut hop_gas = route.hop_gas.saturating_add(close_gas);
    let mut used = None;
    if let Some(u) = unwrap {
        rho0 = u.scale_rho(rho0, book).ok()?;
        hop_gas = hop_gas.saturating_add(u.gas);
        used = Some(UnwrapUse {
            kind: u.kind,
            wrapper: u.wrapper,
            into: u.into,
            amount_out: amount,
        });
    }
    let quote = ExitQuote {
        allocs: SmallVec::new(),
        amount_in: total,
        amount_out: route.amount_out,
        hop_gas,
        rho0,
        unwrap: used,
        hub: None,
        chain: Some(Box::new(ChainUse {
            hops,
            data,
            amount_out: route.amount_out,
            with: Vec::new(),
            exact_out,
        })),
    };
    Some((quote, !direct_close))
}

/// The best of `best` and the two-hop exits through the intermediate
/// tokens the book's graph proposes ([`crate::graph::GraphRoutes::hubs`]),
/// other than the hub itself (already tried). Such an exit sells all the
/// collateral into `x`, buys the debt with `x`, and closes the `x` left
/// into WETH through one more pool, whose hop gas it carries. A token with
/// no live pool against WETH is not tried: neither its gas nor its
/// leftover could be paid in WETH.
#[allow(clippy::too_many_arguments)]
fn graph_hubs(
    book: &PoolBook,
    asset_in: AssetId,
    asset_out: AssetId,
    total: U256,
    gas: &GasTerms,
    budget: &SolveBudget,
    closer: u64,
    best: Result<ExitQuote, RouteError>,
) -> Result<ExitQuote, RouteError> {
    let (Some(g), Some(weth)) = (book.graph(), book.hub()) else {
        return best;
    };
    let mut best = best;
    for x in g.hubs(book, asset_in, asset_out, crate::graph::GRAPH_HUBS) {
        if x == weth {
            continue;
        }
        let (into, out) = (book.legs(asset_in, x), book.legs(x, asset_out));
        if into.is_empty() || out.is_empty() {
            continue;
        }
        // `x` per 1e18 wei, and the closer's gas: its best live pool
        // against WETH at zero size.
        let Some((rho, close_gas)) = book
            .legs(weth, x)
            .iter()
            .filter_map(|l| {
                let p = book.get(l.pool).filter(|p| p.is_live())?;
                Some((p.rho_at_zero(l.i, l.j).ok()?, p.hop_gas))
            })
            .max_by_key(|(r, _)| *r)
        else {
            continue;
        };
        let Ok(per_eth) = mul_div_512(WEI_PER_ETH, rho, Q96).and_then(|v| mul_div_512(v, rho, Q96))
        else {
            continue;
        };
        if per_eth.is_zero() {
            continue;
        }
        let Ok(mut q) = solve_hub_priced(
            book.pools(),
            into,
            out,
            x,
            None,
            total,
            gas,
            budget,
            budget,
            closer,
            per_eth,
        ) else {
            continue;
        };
        q.hop_gas = q.hop_gas.saturating_add(close_gas);
        best = match best {
            // Two hub exits: the one that leaves more after its gas.
            Ok(b) if b.hub.is_some() => {
                let b_net = b.amount_out.saturating_sub(gas.cost_in_out(b.hop_gas)?);
                let q_net = q.amount_out.saturating_sub(gas.cost_in_out(q.hop_gas)?);
                Ok(if q_net > b_net { q } else { b })
            }
            other => better(other, Ok(q), book.pools(), gas),
        };
    }
    best
}

/// [`solve_pair`] restricted to exits whose last swap, into the debt, is
/// one Uniswap V3 pool: the pool a flash swap can lend the debt from
/// ([`liq_types::FlashProvider::UniV3Swap`]). The collateral's way into the
/// hub, when the exit goes through it, is unrestricted. Each candidate pool
/// is weighed alone on output after gas ([`solve_on_with`]).
/// `InsufficientLiquidity` when no such pool absorbs the size.
pub fn solve_pair_single_lender(
    book: &PoolBook,
    asset_in: AssetId,
    asset_out: AssetId,
    total: U256,
    gas: &GasTerms,
    budget: &SolveBudget,
) -> Result<ExitQuote, RouteError> {
    let one = SolveBudget {
        max_pools: 1,
        ..*budget
    };
    let v3 = |legs: &[Leg]| -> SmallVec<[Leg; 8]> {
        legs.iter()
            .filter(|l| book.get(l.pool).is_some_and(Pool::is_v3_contract))
            .copied()
            .collect()
    };
    let direct = match book.exit_source(asset_in, asset_out) {
        ExitSource::Direct(legs) => solve_on_with(book.pools(), &v3(legs), total, gas, &one, 0),
        ExitSource::Unwrap(u, legs) => solve_via(
            book.pools(),
            &v3(legs),
            Some((u, book)),
            asset_out,
            total,
            gas,
            &one,
            0,
        ),
    };
    let Some(route) = book.hub_route(asset_in, asset_out) else {
        return direct;
    };
    let via_hub = solve_hub(
        book.pools(),
        route.into_hub,
        &v3(route.out_of_hub),
        route.hub,
        route.unwrap.map(|u| (u, book)),
        total,
        gas,
        budget,
        &one,
        0,
    );
    better(direct, via_hub, book.pools(), gas)
}

/// The one Uniswap V3 pool `exit` buys all of its debt from, when that is
/// its shape: the pool a flash swap lends from. `None` for a split, a pool
/// of another venue, or an unwrap straight into the debt.
#[must_use]
pub fn lender_of(book: &PoolBook, exit: &ExitQuote) -> Option<PoolId> {
    // A flash swap's lender is paid in WETH or the collateral: an exit
    // through another hub token is not one it can fund.
    if exit.hub.as_ref().is_some_and(|h| Some(h.hub) != book.hub()) {
        return None;
    }
    // A chain, alone or split with direct pools, is not one pool's loan.
    if exit.chain.is_some() {
        return None;
    }
    let mut used = exit.allocs.iter().filter(|a| !a.amount_in.is_zero());
    let a = used.next()?;
    if used.next().is_some() {
        return None;
    }
    let p = book.get(a.leg.pool)?;
    p.is_v3_contract().then_some(a.leg.pool)
}

/// Gas of the closer an exit into `debt` runs when a pool that sells only an
/// exact input (Curve) buys its share: the surplus debt goes back into WETH
/// through the cheapest live pool between them. Zero when the debt is WETH
/// (the surplus is profit) or the book has no hub.
fn exact_in_closer_gas(book: &PoolBook, debt: AssetId) -> u64 {
    let Some(hub) = book.hub().filter(|&h| h != debt) else {
        return 0;
    };
    book.legs(debt, hub)
        .iter()
        .filter_map(|l| book.get(l.pool))
        .filter(|p| p.is_live())
        .map(|p| p.hop_gas)
        .min()
        .unwrap_or(0)
}

/// Best zero-size marginal (Q96 sqrt) over the exits [`solve_pair`] chooses
/// between: the live pools of `(coll, debt)` (or an unwrap's), and the
/// hub's. `None`: neither has a live pool.
#[must_use]
pub fn exit_rho0(book: &PoolBook, coll: AssetId, debt: AssetId) -> Option<U256> {
    let best = |legs: &[Leg]| {
        legs.iter()
            .filter_map(|l| {
                let p = book.get(l.pool)?;
                p.is_live().then(|| p.rho_at_zero(l.i, l.j).ok()).flatten()
            })
            .max()
    };
    let direct = match book.exit_source(coll, debt) {
        ExitSource::Direct(legs) => best(legs),
        ExitSource::Unwrap(u, legs) => match best(legs) {
            Some(r) => u.scale_rho(r, book).ok(),
            None if u.into == debt => u.rho(book).ok(),
            None => None,
        },
    };
    let hub = book.hub_route(coll, debt).and_then(|r| {
        let rho = mul_div_512(best(r.into_hub)?, best(r.out_of_hub)?, Q96).ok()?;
        match r.unwrap {
            Some(u) => u.scale_rho(rho, book).ok(),
            None => Some(rho),
        }
    });
    direct.max(hub)
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
    let closer = exact_in_closer_gas(book, debt);
    // Scratch copy of every pool any collateral may touch; legs remapped.
    let mut base: Vec<Pool> = Vec::new();
    let mut ids: SmallVec<[PoolId; 16]> = SmallVec::new();
    let mut legs_per: SmallVec<[SmallVec<[Leg; 8]>; EXHAUSTIVE_K]> = SmallVec::new();
    let mut unwrap_per: SmallVec<[Option<Unwrap>; EXHAUSTIVE_K]> = SmallVec::new();
    for &(coll, _) in colls {
        let mut remapped = SmallVec::new();
        unwrap_per.push(match book.exit_source(coll, debt) {
            ExitSource::Unwrap(u, _) => Some(u.clone()),
            ExitSource::Direct(_) => None,
        });
        for leg in book.exit_legs(coll, debt) {
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
            let uw = unwrap_per.get(ci_us).ok_or(RouteError::BadLeg)?.as_ref();
            let q = solve_via(
                scratch,
                legs,
                uw.map(|u| (u, book)),
                debt,
                amount,
                gas,
                budget,
                closer,
            )?;
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
            let pool_rho = book
                .exit_legs(coll, debt)
                .iter()
                .filter_map(|l| book.get(l.pool).and_then(|p| p.rho_at_zero(l.i, l.j).ok()))
                .max();
            let rho0 = match (unwrap_per.get(ci).and_then(Option::as_ref), pool_rho) {
                (None, Some(r)) => r,
                (Some(u), Some(r)) => u.scale_rho(r, book)?,
                (Some(u), None) if u.into == debt => u.rho(book)?,
                _ => return Err(RouteError::InsufficientLiquidity),
            };
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
        priority_fee_wei: 0,
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
            priority_fee_wei: 0,
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

    /// Uniswap V2 `getAmountOut`: `x·997·r_out / (r_in·1000 + x·997)`.
    fn v2_out(x: U256, r_in: U256, r_out: U256) -> U256 {
        let x997 = x * U256::from(997u64);
        x997 * r_out / (r_in * U256::from(1000u64) + x997)
    }

    /// A V2 pair holding `a` (reserve `ra`) and `b` (reserve `rb`).
    fn v2_pair(n: u64, a: AssetId, b: AssetId, ra: U256, rb: U256) -> Pool {
        let mut p = v2(n, ra, rb);
        p.assets = smallvec::SmallVec::from_slice(&[a, b]);
        p.tokens = smallvec::SmallVec::from_slice(&[tok(u64::from(a.0)), tok(u64::from(b.0))]);
        p
    }

    /// A0 collateral, A1 debt, A2 the hub (WETH). The A0/A1 pair is thin
    /// (10 : 100); A0/A2 (10,000 : 1,000) and A2/A1 (1,000 : 100,000) are
    /// deep at the same price, 10 A1 per A0. Five A0 straight into A1 pay
    /// about 33.3; through A2, about 49.7. Oracle: Uniswap V2 `getAmountOut`
    /// on each pair. With no hub the book takes the thin pair, as it took
    /// LINK/USDT's at block 26,106,490.
    #[test]
    fn an_exit_through_the_hub_wins_when_the_direct_pool_is_thin() {
        let pools = vec![
            v2_pair(1, A0, A1, e18(10), e18(100)),
            v2_pair(2, A0, A2, e18(10_000), e18(1_000)),
            v2_pair(3, A2, A1, e18(1_000), e18(100_000)),
        ];
        let x = e18(5);
        let direct = v2_out(x, e18(10), e18(100));
        let weth = v2_out(x, e18(10_000), e18(1_000));
        let through = v2_out(weth, e18(1_000), e18(100_000));
        assert!(through > direct + e18(10), "{through} vs {direct}");

        let mut bk = book(pools);
        let q = solve_pair(&bk, A0, A1, x, &FREE, &B).unwrap();
        assert!(q.hub.is_none(), "no hub: direct only");
        assert_eq!(q.amount_out, direct);

        bk.set_hub(A2);
        let q = solve_pair(&bk, A0, A1, x, &FREE, &B).unwrap();
        let hub = q.hub.as_ref().unwrap(); // through the hub
        assert_eq!(hub.hub, A2);
        assert_eq!((hub.amount_in, hub.amount_out), (x, weth));
        assert_eq!(hub.allocs.iter().map(|a| a.amount_in).sum::<U256>(), x);
        assert_eq!(q.allocs.iter().map(|a| a.amount_in).sum::<U256>(), weth);
        assert_eq!((q.amount_in, q.amount_out), (x, through));
        assert_eq!(q.hop_gas, 2 * HOP_GAS);
    }

    /// The depths swapped: A0/A1 deep, the hub's pairs thin. The direct
    /// pair pays more, so the exit stays direct.
    #[test]
    fn the_direct_pool_wins_when_it_pays_more() {
        let mut bk = book(vec![
            v2_pair(1, A0, A1, e18(10_000), e18(100_000)),
            v2_pair(2, A0, A2, e18(10), e18(1)),
            v2_pair(3, A2, A1, e18(1), e18(100)),
        ]);
        bk.set_hub(A2);
        let x = e18(5);
        let q = solve_pair(&bk, A0, A1, x, &FREE, &B).unwrap();
        assert!(q.hub.is_none());
        assert_eq!(q.amount_out, v2_out(x, e18(10_000), e18(100_000)));
    }

    /// No pool between A0 and A1 at all. Direct, there is no exit; through
    /// A2 there is.
    #[test]
    fn a_collateral_with_no_pool_into_its_debt_exits_through_the_hub() {
        let mut bk = book(vec![
            v2_pair(2, A0, A2, e18(10_000), e18(1_000)),
            v2_pair(3, A2, A1, e18(1_000), e18(100_000)),
        ]);
        let x = e18(5);
        assert_eq!(
            solve_pair(&bk, A0, A1, x, &FREE, &B),
            Err(RouteError::InsufficientLiquidity)
        );
        assert_eq!(exit_rho0(&bk, A0, A1), None);
        bk.set_hub(A2);
        let q = solve_pair(&bk, A0, A1, x, &FREE, &B).unwrap();
        let weth = v2_out(x, e18(10_000), e18(1_000));
        assert_eq!(q.amount_out, v2_out(weth, e18(1_000), e18(100_000)));
        assert!(exit_rho0(&bk, A0, A1).is_some());
        // The hub's own pairs stay direct: nothing routes WETH through WETH.
        assert!(solve_pair(&bk, A0, A2, x, &FREE, &B).unwrap().hub.is_none());
        assert!(solve_pair(&bk, A2, A1, x, &FREE, &B).unwrap().hub.is_none());
    }

    /// A direct exit buys only what is owed and closes the rest of the
    /// collateral into WETH through one more pool; the exit through the hub
    /// sold it all into WETH already. Both run two swaps. Here the hub pays
    /// `d` more A1 and a swap costs about `2d` of gas: charged only its own
    /// hop, the direct exit would win (`out − 1 hop > out + d − 2 hops`);
    /// charged the closer it really runs, it loses.
    #[test]
    fn the_direct_exit_is_charged_its_closer_hop() {
        let pools = vec![
            v2_pair(1, A0, A1, e18(1_000), e18(10_000)),
            v2_pair(2, A0, A2, e18(100_000), e18(10_000)),
            v2_pair(3, A2, A1, e18(10_000), e18(1_000_000)),
        ];
        let x = e18(5);
        let direct = solve_pair(&book(pools.clone()), A0, A1, x, &FREE, &B).unwrap();
        let mut bk = book(pools);
        bk.set_hub(A2);
        let via = solve_pair(&bk, A0, A1, x, &FREE, &B).unwrap();
        assert!(via.hub.is_some());
        let d = via.amount_out - direct.amount_out;
        let gas = GasTerms {
            base_fee_wei: u128::try_from(d * U256::from(2u64) / U256::from(HOP_GAS)).unwrap(),
            priority_fee_wei: 0,
            out_per_eth: e18(1),
        };
        let hop = gas.cost_in_out(HOP_GAS).unwrap();
        assert!(hop > d, "a swap's gas outweighs the hub's edge");
        assert!(
            direct.amount_out - hop > via.amount_out - gas.cost_in_out(2 * HOP_GAS).unwrap(),
            "charged one hop, the direct exit would look better"
        );
        let q = solve_pair(&bk, A0, A1, x, &gas, &B).unwrap();
        assert!(q.hub.is_some(), "charged its closer too, it does not");
    }

    /// Two A0/A1 pairs: X at 1.01 A1 per A0 but 300k gas a hop, Y at 1.00
    /// and 100k. Ten A0 through X pay `d` more A1 (Uniswap V2
    /// `getAmountOut` on each), and 200k gas costs `2d`: X's edge is worth
    /// less than its gas, so the exit takes Y alone. With gas free, X.
    #[test]
    fn a_cheaper_pool_wins_when_the_dearer_ones_edge_is_worth_less_than_its_gas() {
        let mut x = v2_pair(1, A0, A1, e18(10_000), e18(10_100));
        x.hop_gas = 300_000;
        let mut y = v2_pair(2, A0, A1, e18(10_000), e18(10_000));
        y.hop_gas = 100_000;
        let bk = book(vec![x, y]);
        let size = e18(10);
        let (out_x, out_y) = (
            v2_out(size, e18(10_000), e18(10_100)),
            v2_out(size, e18(10_000), e18(10_000)),
        );
        let d = out_x - out_y;
        let gas = GasTerms {
            base_fee_wei: u128::try_from(d * U256::from(2u64) / U256::from(200_000u64)).unwrap(),
            priority_fee_wei: 0,
            out_per_eth: e18(1),
        };
        let extra = gas.cost_in_out(200_000).unwrap();
        assert!(extra > d, "the dearer pool's gas outweighs its edge");
        let used = |q: &ExitQuote| -> Vec<PoolId> {
            q.allocs
                .iter()
                .filter(|a| !a.amount_in.is_zero())
                .map(|a| a.leg.pool)
                .collect()
        };
        let q = solve_pair(&bk, A0, A1, size, &gas, &B).unwrap();
        assert_eq!(used(&q), vec![PoolId(1)], "Y alone");
        assert_eq!((q.amount_out, q.hop_gas), (out_y, 100_000));
        let q = solve_pair(&bk, A0, A1, size, &FREE, &B).unwrap();
        assert!(used(&q).contains(&PoolId(0)), "free gas: X's price");
    }

    /// A thin pair at a slightly better price (10 : 10.1) and a deep one
    /// (10,000 : 10,000). The set grown on price starts at the thin pair
    /// and, with gas free, splits five A0 across both. The split gains `g`
    /// over the deep pair alone; when a hop costs more than `g`, the deep
    /// pair alone leaves more, and it is not first on price, so only
    /// trying each pool alone finds it. Oracle for its output: Uniswap V2
    /// `getAmountOut`.
    #[test]
    fn a_deeper_pool_alone_beats_a_split_whose_gain_is_worth_less_than_a_hop() {
        let bk = book(vec![
            v2_pair(1, A0, A1, e18(10), e18(10) + e18(1) / U256::from(10u64)),
            v2_pair(2, A0, A1, e18(10_000), e18(10_000)),
        ]);
        let size = e18(5);
        let deep = v2_out(size, e18(10_000), e18(10_000));
        let split = solve_pair(&bk, A0, A1, size, &FREE, &B).unwrap();
        assert_eq!(
            split
                .allocs
                .iter()
                .filter(|a| !a.amount_in.is_zero())
                .count(),
            2
        );
        let g = split.amount_out - deep;
        assert!(!g.is_zero(), "the split gains something");
        let gas = GasTerms {
            base_fee_wei: u128::try_from(g * U256::from(2u64) / U256::from(HOP_GAS)).unwrap(),
            priority_fee_wei: 0,
            out_per_eth: e18(1),
        };
        assert!(
            gas.cost_in_out(HOP_GAS).unwrap() > g,
            "a hop costs more than the split gains"
        );
        let q = solve_pair(&bk, A0, A1, size, &gas, &B).unwrap();
        let used: Vec<PoolId> = q
            .allocs
            .iter()
            .filter(|a| !a.amount_in.is_zero())
            .map(|a| a.leg.pool)
            .collect();
        assert_eq!(used, vec![PoolId(1)], "the deep pair alone");
        assert_eq!((q.amount_out, q.hop_gas), (deep, HOP_GAS));
    }

    /// A Curve pool sells only an exact input, so an exit through it buys
    /// a little more than owed and a closer sells the surplus back into
    /// WETH: one more hop, charged to the Curve leg. Curve A0/A1 (4 bp)
    /// pays `d` more than the V2 pair (30 bp) for ten A0, with the same hop
    /// gas; a hop costs `2d`. Into A1, which is not WETH, the closer makes
    /// Curve the dearer exit; with no hub (no closer), Curve's price wins.
    #[test]
    fn a_curve_exit_pays_for_the_closer_that_sells_its_surplus() {
        let pools = vec![
            crate::fixtures::curve(1, &[e18(1_000_000), e18(1_000_000)], 10_000, 4_000_000),
            v2_pair(2, A0, A1, e18(1_000_000), e18(1_000_000)),
            v2_pair(3, A1, A2, e18(1_000_000), e18(1_000_000)),
        ];
        let size = e18(10);
        let plain = book(pools.clone());
        let curve_out = plain.pools()[0].quote_exact_in(0, 1, size).unwrap();
        let v2 = v2_out(size, e18(1_000_000), e18(1_000_000));
        assert!(curve_out > v2, "Curve's 4 bp beats the pair's 30 bp");
        let d = curve_out - v2;
        let gas = GasTerms {
            base_fee_wei: u128::try_from(d * U256::from(2u64) / U256::from(HOP_GAS)).unwrap(),
            priority_fee_wei: 0,
            out_per_eth: e18(1),
        };
        let q = solve_pair(&plain, A0, A1, size, &gas, &B).unwrap();
        assert_eq!(q.amount_out, curve_out, "no hub, no closer: Curve");
        assert_eq!(q.hop_gas, HOP_GAS);
        let mut hubbed = book(pools);
        hubbed.set_hub(A2);
        let q = solve_pair(&hubbed, A0, A1, size, &gas, &B).unwrap();
        assert_eq!(q.amount_out, v2, "the closer's hop outweighs Curve's edge");
        assert_eq!(q.hop_gas, HOP_GAS);
        // Into the hub itself the surplus is profit: no closer.
        assert_eq!(exact_in_closer_gas(&hubbed, A2), 0);
        assert_eq!(exact_in_closer_gas(&hubbed, A1), HOP_GAS);
    }

    /// Six shallow pools with a better spot price than two deep ones fill
    /// every place the `ρ₀`-ordered growth has (`max_pools` 6) and cannot
    /// take the sale between them, so that growth alone finds one deep pool
    /// at best. The pruned fill takes both deep pools and the shallow ones
    /// that add output. Oracle: each pool's own exact quote, for the
    /// allocation's output and for one deep pool taking everything.
    #[test]
    fn shallow_pools_with_the_best_spot_price_do_not_crowd_out_deep_ones() {
        let shallow = |n| v3(n, 100, 1, sqrt_at(10), &[(-50, 50, L / 1_000)]);
        let deep = |n| v2(n, e18(1_000_000), e18(1_000_000));
        let bk = book(vec![
            shallow(1),
            shallow(2),
            shallow(3),
            shallow(4),
            shallow(5),
            shallow(6),
            deep(7),
            deep(8),
        ]);
        let x = e18(20_000);
        let q = solve_pair(&bk, A0, A1, x, &FREE, &B).unwrap();
        let used: Vec<&Allocation> = q.allocs.iter().filter(|a| !a.amount_in.is_zero()).collect();
        assert!(used.len() <= usize::from(B.max_pools));
        let deep_used = used
            .iter()
            .filter(|a| bk.get(a.leg.pool).unwrap().venue() == Venue::UniV2)
            .count();
        assert_eq!(deep_used, 2, "both deep pools take a share");
        let summed = used.iter().fold(U256::ZERO, |acc, a| {
            acc + bk
                .get(a.leg.pool)
                .unwrap()
                .quote_exact_in(a.leg.i, a.leg.j, a.amount_in)
                .unwrap()
        });
        assert_eq!(q.amount_out, summed, "the output is the pools' own quotes");
        assert_eq!(used.iter().fold(U256::ZERO, |acc, a| acc + a.amount_in), x);
        let one_deep = deep(7).quote_exact_in(0, 1, x).unwrap();
        assert!(
            q.amount_out > one_deep,
            "{} vs one deep pool {one_deep}",
            q.amount_out
        );
    }

    /// The single-lender solve. Two equal V3 pools A0/A1 split the exit by
    /// price and have no lender; restricted to one lender, the exit takes
    /// one of them whole (`lender_of` names it) for a little more impact.
    /// A V2-only book has no lender. Through the hub, the lender is the
    /// one V3 pool WETH → debt; the way into WETH may be any venue.
    #[test]
    fn a_single_lender_exit_is_one_v3_pool_into_the_debt() {
        let p = |n| v3(n, 3000, 60, SQRT_ONE, &[(-6000, 6000, L)]);
        let bk = book(vec![p(1), p(2)]);
        let x = e18(20);
        let split = solve_pair(&bk, A0, A1, x, &FREE, &B).unwrap();
        assert_eq!(
            split
                .allocs
                .iter()
                .filter(|a| !a.amount_in.is_zero())
                .count(),
            2,
            "equal pools split by price"
        );
        assert_eq!(lender_of(&bk, &split), None);
        let one = solve_pair_single_lender(&bk, A0, A1, x, &FREE, &B).unwrap();
        let lender = lender_of(&bk, &one).unwrap();
        assert_eq!(one.amount_in, x);
        assert_eq!(
            one.amount_out,
            bk.get(lender).unwrap().quote_exact_in(0, 1, x).unwrap()
        );
        assert!(
            one.amount_out < split.amount_out,
            "whole on one pool: more impact"
        );

        let v2only = book(vec![v2(1, e18(1_000), e18(1_000))]);
        assert_eq!(
            solve_pair_single_lender(&v2only, A0, A1, x, &FREE, &B),
            Err(RouteError::InsufficientLiquidity)
        );
        let direct = solve_pair(&v2only, A0, A1, x, &FREE, &B).unwrap();
        assert_eq!(lender_of(&v2only, &direct), None, "a V2 pair cannot lend");

        let mut into_debt = p(2);
        into_debt.assets = smallvec::SmallVec::from_slice(&[A2, A1]);
        into_debt.tokens = smallvec::SmallVec::from_slice(&[tok(2), tok(1)]);
        let mut hub = book(vec![
            v2_pair(1, A0, A2, e18(10_000), e18(10_000)),
            into_debt,
        ]);
        hub.set_hub(A2);
        let q = solve_pair_single_lender(&hub, A0, A1, e18(5), &FREE, &B).unwrap();
        assert!(q.hub.is_some(), "through WETH");
        assert_eq!(
            lender_of(&hub, &q),
            Some(PoolId(1)),
            "the WETH/debt pool lends"
        );
    }

    /// The warm tier's zero-size marginal is the better exit's: the direct
    /// pair's, or the hub's two pairs' product (`ρ` is a square root, so
    /// the product of the two `ρ` over Q96).
    #[test]
    fn exit_rho0_is_the_better_of_the_direct_and_hub_marginals() {
        let mut bk = book(vec![
            v2_pair(1, A0, A1, e18(10), e18(90)),
            v2_pair(2, A0, A2, e18(10_000), e18(1_000)),
            v2_pair(3, A2, A1, e18(1_000), e18(100_000)),
        ]);
        let rho = |bk: &PoolBook, i: u32| bk.get(PoolId(i)).unwrap().rho_at_zero(0, 1).unwrap();
        let direct = rho(&bk, 0);
        let through = mul_div_512(rho(&bk, 1), rho(&bk, 2), Q96).unwrap();
        assert!(through > direct, "9 per A0 direct, 9.97·… through the hub");
        assert_eq!(exit_rho0(&bk, A0, A1), Some(direct));
        bk.set_hub(A2);
        assert_eq!(exit_rho0(&bk, A0, A1), Some(through));
    }

    /// EIP-1559-style gas conversion: `gas · fee · out_per_eth / 1e18`.
    #[test]
    fn gas_terms_cost() {
        let g = GasTerms {
            base_fee_wei: 20_000_000_000,
            priority_fee_wei: 0,
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
