//! Fluid DEX T1 (Instadapp): `swapIn` ported to exact integer arithmetic,
//! together with the two `FluidLiquidity.operate` calls it makes.
//!
//! A line-for-line port of `FluidDexT1`'s `_swapIn` (`CoreHelpers`:
//! `_getPricesAndExchangePrices`, `_getCollateralReserves`,
//! `_getDebtReserves`, `_swapRoutingIn`, `_updateOracle`, the reserve and
//! oracle checks) and of the Liquidity layer's `FluidLiquidityUserModule`
//! (`operate` for a supply/withdraw and a borrow/payback of one token) with
//! `LiquidityCalcs` (exchange prices, withdrawal and borrow limits, the
//! borrow-rate curves). It follows `tools/registry/fluid_math.py`, which is
//! checked against the pool's own `swapIn` estimate
//! (`tools/registry/fluid_vectors.py`); `tests/fluid_vectors.rs` replays the
//! same recording here.
//!
//! Not followed — the pool is then not live: an active range, threshold or
//! center-price shift (their implementation is a separate contract), a pool
//! hook, a paused pool or token, a center price hook that was not read.
//!
//! Solidity 0.8 arithmetic is checked, so every operation here is checked
//! and an overflow is [`RouteError::Math`] (a revert).

// A line-for-line port of the deployed Fluid arithmetic: where the contracts
// subtract or index, the same guard (a comparison, a fixed array length)
// precedes it here, and every operation that can overflow is checked.
#![allow(clippy::arithmetic_side_effects, clippy::indexing_slicing)]

use alloy_primitives::{Address, I256, U256};
use smallvec::SmallVec;

use crate::solver::{narrow, RouteError, MAX_COINS};

type R<T> = Result<T, RouteError>;

const E27: u128 = 1_000_000_000_000_000_000_000_000_000;
const SIX: u128 = 1_000_000;
const EIGHT: u128 = 100_000_000;
const THREE: u128 = 1_000;
const FOUR: u128 = 10_000;
const TWELVE: u128 = 1_000_000_000_000;
const EXCHANGE_PRICES_PRECISION: u128 = TWELVE;
const ORACLE_PRECISION: u128 = 1_000_000_000_000_000_000;
const ORACLE_LIMIT: u128 = 50_000_000_000_000_000;
const MINIMUM_LIQUIDITY_SWAP: u128 = 10_000;
const SECONDS_PER_YEAR: u128 = 365 * 24 * 3600;
const FORCE_STORAGE_WRITE_AFTER_TIME: u64 = 24 * 3600;
const MAX_INPUT_AMOUNT_EXCESS: u128 = 100;
const MAX_TOKEN_AMOUNT_CAP: u128 = (1u128 << 127) - 1;
const RATIO_DEPOSIT_BORROW: u128 = 10_000;
const RATIO_WITHDRAW_PAYBACK: u128 = 2;
const MAX_NEW_AMOUNT_WHEN_RATIO_CHECK: u128 = 1u128 << 80;
const TOTAL_DECAY_CHECKPOINTS: u128 = 1000;
const MIN_DECAY_DURATION_CHECKPOINTS: u128 = 80;
const DECAY_CHECKPOINT_DURATION_SCALEDX10: u64 = 36;
const DEFAULT_COEFFICIENT_SIZE: usize = 56;
const DEFAULT_EXPONENT_SIZE: usize = 8;
const DECAY_COEFFICIENT_SIZE: usize = 18;

/// `n` ones starting at bit `lo`.
#[inline]
fn mask(lo: usize, n: usize) -> U256 {
    ((U256::ONE << n) - U256::ONE) << lo
}

#[inline]
fn u(v: u128) -> U256 {
    U256::from(v)
}

#[inline]
fn bits(v: U256, lo: usize, n: usize) -> U256 {
    (v >> lo) & ((U256::ONE << n) - U256::ONE)
}

/// A bit field that fits 128 bits.
#[inline]
fn field(v: U256, lo: usize, n: usize) -> u128 {
    bits(v, lo, n).to::<u128>()
}

#[inline]
fn flag(v: U256, lo: usize) -> bool {
    (v >> lo) & U256::ONE == U256::ONE
}

fn add(a: U256, b: U256) -> R<U256> {
    a.checked_add(b).ok_or(RouteError::Math)
}

fn sub(a: U256, b: U256) -> R<U256> {
    a.checked_sub(b).ok_or(RouteError::Math)
}

fn mul(a: U256, b: U256) -> R<U256> {
    a.checked_mul(b).ok_or(RouteError::Math)
}

fn div(a: U256, b: U256) -> R<U256> {
    a.checked_div(b).ok_or(RouteError::Math)
}

/// `a * b / c`, rounded down; the product is checked like the contracts'
/// plain `*`.
fn md(a: U256, b: U256, c: U256) -> R<U256> {
    div(mul(a, b)?, c)
}

/// `a * b / c` rounded up (`FixedPointMathLib.mulDivUp`).
fn md_up(a: U256, b: U256, c: U256) -> R<U256> {
    let p = mul(a, b)?;
    if c.is_zero() {
        return Err(RouteError::Math);
    }
    let q = p / c;
    Ok(if (p % c).is_zero() {
        q
    } else {
        add(q, U256::ONE)?
    })
}

fn signed(v: U256) -> R<I256> {
    I256::try_from(v).map_err(|_| RouteError::Math)
}

fn unsigned(v: I256) -> R<U256> {
    U256::try_from(v).map_err(|_| RouteError::Math)
}

fn isqrt(v: U256) -> U256 {
    v.root(2)
}

// ───────────────────────────── BigNumber ─────────────────────────────

/// `BigMathMinified.fromBigNumber` with the default 8-bit exponent.
fn from_big(v: U256) -> U256 {
    (v >> DEFAULT_EXPONENT_SIZE) << (v & U256::from(0xffu64)).to::<usize>()
}

/// `BigMathMinified.toBigNumber`.
fn to_big(normal: U256, coef: usize, exp: usize, round_up: bool) -> R<U256> {
    let last = normal.bit_len().max(coef);
    let exponent = last - coef;
    let mut coefficient = normal >> exponent;
    let mut exponent = exponent;
    if round_up && exponent > 0 {
        coefficient = add(coefficient, U256::ONE)?;
        if coefficient == U256::ONE << coef {
            coefficient = U256::ONE << (coef - 1);
            exponent += 1;
        }
    }
    if exponent >= 1usize << exp {
        return Err(RouteError::Math);
    }
    Ok((coefficient << exp) + U256::from(exponent))
}

fn to_default_big(normal: U256, round_up: bool) -> R<U256> {
    to_big(
        normal,
        DEFAULT_COEFFICIENT_SIZE,
        DEFAULT_EXPONENT_SIZE,
        round_up,
    )
}

// ──────────────────────── Liquidity layer: prices ────────────────────────

// `exchangePricesAndConfig` layout (`LiquiditySlotsLink`).
const EP_RATE: usize = 0;
const EP_FEE: usize = 16;
const EP_UTIL: usize = 30;
const EP_THRESH: usize = 44;
const EP_TS: usize = 58;
const EP_SUPPLY_EP: usize = 91;
const EP_BORROW_EP: usize = 155;
const EP_SUPPLY_RATIO: usize = 219;
const EP_BORROW_RATIO: usize = 234;
const EP_USES_CONFIGS2: usize = 249;
const EP_PAUSE: usize = 250;

// user supply / borrow data layout.
const US_AMOUNT: usize = 1;
const US_PREV_WD: usize = 65;
const US_TS: usize = 129;
const US_EXPAND_PCT: usize = 162;
const US_EXPAND_DUR: usize = 176;
const US_BASE_WD: usize = 200;
const US_DECAY_AMT: usize = 218;
const US_DECAY_DUR: usize = 244;
const US_PAUSED: usize = 255;
const UB_PREV_LIMIT: usize = 65;
const UB_BASE_LIMIT: usize = 200;
const UB_MAX_LIMIT: usize = 218;

/// `LiquidityCalcs.calcExchangePrices`.
fn calc_exchange_prices(cfg: U256, ts: u64) -> R<(U256, U256)> {
    let mut supply_ep = bits(cfg, EP_SUPPLY_EP, 64);
    let mut borrow_ep = bits(cfg, EP_BORROW_EP, 64);
    if supply_ep.is_zero() || borrow_ep.is_zero() {
        return Err(RouteError::StalePool);
    }
    let rate = bits(cfg, EP_RATE, 16);
    let secs = u128::from(ts)
        .checked_sub(field(cfg, EP_TS, 33))
        .ok_or(RouteError::Math)?;
    let mut borrow_ratio = field(cfg, EP_BORROW_RATIO, 15);
    if secs == 0 || rate.is_zero() || borrow_ratio == 1 {
        return Ok((supply_ep, borrow_ep));
    }
    borrow_ep = add(
        borrow_ep,
        div(
            mul(mul(borrow_ep, rate)?, u(secs))?,
            u(SECONDS_PER_YEAR * FOUR),
        )?,
    )?;
    let mut t = field(cfg, EP_SUPPLY_RATIO, 15);
    if t == 1 {
        return Ok((supply_ep, borrow_ep));
    }
    let util = field(cfg, EP_UTIL, 14);
    let e27 = u(E27);
    let tt = if t & 1 == 1 {
        t >>= 1;
        let inner = div(mul(e27, u(FOUR))?, u(t))?;
        div(mul(u(util), add(e27, inner)?)?, u(FOUR))?
    } else {
        t >>= 1;
        div(mul(mul(e27, u(util))?, u(FOUR + t))?, u(FOUR * FOUR))?
    };
    let br = if borrow_ratio & 1 == 1 {
        borrow_ratio >>= 1;
        div(mul(u(borrow_ratio), e27)?, u(FOUR + borrow_ratio))?
    } else {
        borrow_ratio >>= 1;
        sub(
            e27,
            div(mul(u(borrow_ratio), e27)?, u(FOUR + borrow_ratio))?,
        )?
    };
    let e54 = mul(e27, e27)?;
    let t = div(mul(mul(u(FOUR), tt)?, br)?, e54)?;
    let t = mul(mul(rate, t)?, u(FOUR - field(cfg, EP_FEE, 14)))?;
    supply_ep = add(
        supply_ep,
        div(
            mul(mul(supply_ep, t)?, u(secs))?,
            u(SECONDS_PER_YEAR * FOUR * FOUR * FOUR),
        )?,
    )?;
    Ok((supply_ep, borrow_ep))
}

fn withdrawal_limit_before(data: U256, user_supply: U256, ts: u64) -> R<U256> {
    let last = from_big(bits(data, US_PREV_WD, 64));
    if last.is_zero() {
        return Ok(U256::ZERO);
    }
    let max_wd = div(
        mul(u(field(data, US_EXPAND_PCT, 14)), user_supply)?,
        u(FOUR),
    )?;
    let elapsed = u128::from(ts)
        .checked_sub(field(data, US_TS, 33))
        .ok_or(RouteError::Math)?;
    let dur = u(field(data, US_EXPAND_DUR, 24));
    let t = div(mul(max_wd, u(elapsed))?, dur)?;
    let cur = if last > t { last - t } else { U256::ZERO };
    let minimum = sub(user_supply, max_wd)?;
    Ok(minimum.max(cur))
}

fn withdrawal_limit_after(data: U256, user_supply: U256, new_limit: U256) -> R<U256> {
    let base = from_big(bits(data, US_BASE_WD, 18));
    if user_supply < base {
        return Ok(U256::ZERO);
    }
    let pct = u(field(data, US_EXPAND_PCT, 14));
    let minimum = sub(user_supply, div(mul(user_supply, pct)?, u(FOUR))?)?;
    Ok(minimum.max(new_limit))
}

fn borrow_limit_before(data: U256, user_borrow: U256, ts: u64) -> R<U256> {
    let pct = u(field(data, US_EXPAND_PCT, 14));
    let max_expansion = div(mul(user_borrow, pct)?, u(FOUR))?;
    let max_expanded = add(user_borrow, max_expansion)?;
    let base = from_big(bits(data, UB_BASE_LIMIT, 18));
    if max_expanded < base {
        return Ok(base);
    }
    let elapsed = u128::from(ts)
        .checked_sub(field(data, US_TS, 33))
        .ok_or(RouteError::Math)?;
    let dur = u(field(data, US_EXPAND_DUR, 24));
    let mut cur = add(
        div(mul(max_expansion, u(elapsed))?, dur)?,
        from_big(bits(data, UB_PREV_LIMIT, 64)),
    )?;
    if cur > max_expanded {
        cur = max_expanded;
    }
    let hard = from_big(bits(data, UB_MAX_LIMIT, 18));
    Ok(if cur > hard { hard } else { cur })
}

fn borrow_limit_after(data: U256, user_borrow: U256, new_limit: U256) -> R<U256> {
    let pct = u(field(data, US_EXPAND_PCT, 14));
    let mut limit = add(user_borrow, div(mul(user_borrow, pct)?, u(FOUR))?)?;
    let base = from_big(bits(data, UB_BASE_LIMIT, 18));
    if limit < base {
        return Ok(base);
    }
    let hard = from_big(bits(data, UB_MAX_LIMIT, 18));
    if limit > hard {
        limit = hard;
    }
    Ok(if new_limit > limit { limit } else { new_limit })
}

/// `calcBorrowRateFromUtilization`, versions 1 and 2 of the rate data.
fn borrow_rate(rate_data: U256, util: u128) -> R<u128> {
    let g = |lo: usize| field(rate_data, lo, 16);
    let line = |y1: u128, y2: u128, x1: u128, x2: u128| -> R<u128> {
        let (y1, y2, x1, x2) = (
            i128::try_from(y1).map_err(|_| RouteError::Math)?,
            i128::try_from(y2).map_err(|_| RouteError::Math)?,
            i128::try_from(x1).map_err(|_| RouteError::Math)?,
            i128::try_from(x2).map_err(|_| RouteError::Math)?,
        );
        let twelve = TWELVE as i128;
        let slope = (y2 - y1)
            .checked_mul(twelve)
            .ok_or(RouteError::Math)?
            .checked_div(x2 - x1)
            .ok_or(RouteError::Math)?;
        let konst = y1 * twelve - slope * x1;
        let util = i128::try_from(util).map_err(|_| RouteError::Math)?;
        let v = slope
            .checked_mul(util)
            .and_then(|s| s.checked_add(konst))
            .ok_or(RouteError::Math)?;
        u128::try_from(v / twelve).map_err(|_| RouteError::Math)
    };
    let version = field(rate_data, 0, 4);
    let r = match version {
        1 => {
            let kink = g(20);
            if util < kink {
                line(g(4), g(36), 0, kink)?
            } else {
                line(g(36), g(52), kink, FOUR)?
            }
        }
        2 => {
            let (k1, k2) = (g(20), g(52));
            if util < k1 {
                line(g(4), g(36), 0, k1)?
            } else if util < k2 {
                line(g(36), g(68), k1, k2)?
            } else {
                line(g(68), g(84), k2, FOUR)?
            }
        }
        _ => return Err(RouteError::StalePool),
    };
    Ok(r.min(0xffff))
}

// ──────────────────────── Liquidity layer: operate ────────────────────────

/// One token's words in the Liquidity layer, for the pool as a user.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct LiqToken {
    /// `_exchangePricesAndConfig[token]`.
    pub ep_cfg: U256,
    /// `_totalAmounts[token]`.
    pub totals: U256,
    /// `_configs2[token]` (maximum utilization in its low 14 bits).
    pub configs2: U256,
    /// `_rateData[token]`.
    pub rate_data: U256,
    /// `_userSupplyData[pool][token]`.
    pub supply: U256,
    /// `_userBorrowData[pool][token]`.
    pub borrow: U256,
    /// The layer's own balance of the token (ETH for the native sentinel).
    pub balance: U256,
}

fn ratio_check(new_amount: U256, existing: U256, deposit_or_borrow: bool) -> R<()> {
    let existing = if deposit_or_borrow {
        mul(u(RATIO_DEPOSIT_BORROW), existing)?
    } else {
        existing / u(RATIO_WITHDRAW_PAYBACK)
    };
    if new_amount > u(MAX_NEW_AMOUNT_WHEN_RATIO_CHECK) && new_amount > existing {
        return Err(RouteError::InsufficientLiquidity);
    }
    Ok(())
}

fn ratio_word(with: U256, free: U256, total: U256) -> R<u128> {
    // (smaller * 1e4 / larger) << 1, bit 0 = the free side is the smaller.
    Ok(if with > free {
        (div(mul(free, u(FOUR))?, with)?.to::<u128>()) << 1
    } else if with < free {
        ((div(mul(with, u(FOUR))?, free)?.to::<u128>()) << 1) | 1
    } else if !total.is_zero() {
        FOUR << 1
    } else {
        0
    })
}

fn abs_diff(a: u128, b: u128) -> u128 {
    a.abs_diff(b)
}

impl LiqToken {
    /// `FluidLiquidityUserModule.operate` as the DEX calls it: supply (+) /
    /// withdraw (−) and borrow (+) / payback (−) of this token at `ts`, with
    /// `pulled` tokens transferred in.
    fn operate(&self, supply_amount: I256, borrow_amount: I256, ts: u64, pulled: U256) -> R<Self> {
        if supply_amount.is_zero() && borrow_amount.is_zero() {
            return Err(RouteError::InsufficientLiquidity);
        }
        let cfg = self.ep_cfg;
        if (cfg >> EP_PAUSE) > U256::ZERO {
            return Err(RouteError::StalePool);
        }
        let (sep, bep) = calc_exchange_prices(cfg, ts)?;
        let mut totals = self.totals;
        let mut s_raw = from_big(bits(totals, 0, 64));
        let mut s_free = from_big(bits(totals, 64, 64));
        let mut b_raw = from_big(bits(totals, 128, 64));
        let mut b_free = from_big(totals >> 192);
        let mut supply_data = self.supply;
        let mut borrow_data = self.borrow;
        let balance = add(self.balance, pulled)?;
        let prec = u(EXCHANGE_PRICES_PRECISION);

        if !supply_amount.is_zero() {
            let before = totals;
            let data = supply_data;
            if data.is_zero() || flag(data, US_PAUSED) {
                return Err(RouteError::StalePool);
            }
            let deposit = !supply_amount.is_negative();
            let mag = supply_amount.unsigned_abs();
            let mut user_supply = from_big(bits(data, US_AMOUNT, 64));
            let mut decay = from_big(bits(data, US_DECAY_AMT, 26));
            let mut decay_cps = field(data, US_DECAY_DUR, 10);
            if !decay.is_zero() {
                let decayed = (ts * 10 / DECAY_CHECKPOINT_DURATION_SCALEDX10)
                    .checked_sub(
                        field(data, US_TS, 33) as u64 * 10 / DECAY_CHECKPOINT_DURATION_SCALEDX10,
                    )
                    .ok_or(RouteError::Math)?;
                if u128::from(decayed) < decay_cps {
                    decay = sub(
                        decay,
                        div(mul(decay, u(u128::from(decayed)))?, u(decay_cps))?,
                    )?;
                    decay_cps -= u128::from(decayed);
                } else {
                    decay = U256::ZERO;
                    decay_cps = 0;
                }
            }
            let mut wd_before = withdrawal_limit_before(data, user_supply, ts)?;
            let (mut new_raw, mut new_free) = (U256::ZERO, U256::ZERO);
            if !(data & U256::ONE).is_zero() {
                if deposit {
                    new_raw = md(mag, prec, sep)?;
                    user_supply = add(user_supply, new_raw)?;
                } else {
                    new_raw = md_up(mag, prec, sep)?;
                    if new_raw > user_supply {
                        return Err(RouteError::InsufficientLiquidity);
                    }
                    user_supply -= new_raw;
                }
            } else {
                new_free = mag;
                if deposit {
                    user_supply = add(user_supply, new_free)?;
                } else {
                    if new_free > user_supply {
                        return Err(RouteError::InsufficientLiquidity);
                    }
                    user_supply -= new_free;
                }
            }
            let mut check_decay_expansion = false;
            if !deposit {
                if user_supply < wd_before {
                    return Err(RouteError::InsufficientLiquidity);
                }
                if !decay.is_zero() {
                    let wd_amount = add(new_raw, new_free)?;
                    if wd_amount > decay {
                        wd_before = if wd_before > decay {
                            wd_before - decay
                        } else {
                            U256::ZERO
                        };
                        decay = U256::ZERO;
                    } else {
                        wd_before = if wd_before > wd_amount {
                            wd_before - wd_amount
                        } else {
                            U256::ZERO
                        };
                        decay -= wd_amount;
                    }
                    check_decay_expansion = true;
                }
            }
            let wd_after = withdrawal_limit_after(data, user_supply, wd_before)?;
            if wd_after.is_zero() {
                decay = U256::ZERO;
            } else if wd_before != wd_after {
                if deposit {
                    if wd_before.is_zero() {
                        wd_before = from_big(bits(data, US_BASE_WD, 18));
                    }
                    if wd_after > wd_before {
                        let new_decay = wd_after - wd_before;
                        let total = add(decay, new_decay)?;
                        decay_cps = div(
                            add(
                                mul(u(decay_cps), decay)?,
                                mul(u(TOTAL_DECAY_CHECKPOINTS), new_decay)?,
                            )?,
                            total,
                        )?
                        .to::<u128>();
                        if decay_cps < MIN_DECAY_DURATION_CHECKPOINTS {
                            decay_cps = MIN_DECAY_DURATION_CHECKPOINTS;
                        }
                        decay = total;
                    } else {
                        decay = U256::ZERO;
                        decay_cps = 0;
                    }
                } else if check_decay_expansion {
                    let not_pushed = if wd_after > wd_before {
                        wd_after - wd_before
                    } else {
                        U256::ZERO
                    };
                    decay = add(decay, not_pushed)?;
                }
            }
            if decay < U256::from(10u64) {
                decay = U256::ZERO;
                decay_cps = 0;
            } else {
                decay = to_big(decay, DECAY_COEFFICIENT_SIZE, DEFAULT_EXPONENT_SIZE, false)?;
                if decay_cps > TOTAL_DECAY_CHECKPOINTS {
                    decay_cps = TOTAL_DECAY_CHECKPOINTS;
                } else if decay_cps == 0 {
                    decay_cps = 1;
                }
            }
            let user_big = to_default_big(user_supply, false)?;
            if bits(data, US_AMOUNT, 64) == user_big {
                return Err(RouteError::InsufficientLiquidity);
            }
            let wd_after_big = to_default_big(wd_after, false)?;
            // Preserved: mode (bit 0), expansion / base-limit fields
            // (162..217), the pause flag and the one above it (254..255).
            let preserved = (data & U256::ONE)
                | (data & (((U256::ONE << 56) - U256::ONE) << 162))
                | (data & (U256::from(3u64) << 254));
            supply_data = preserved
                | (user_big << US_AMOUNT)
                | (wd_after_big << US_PREV_WD)
                | (U256::from(ts) << US_TS)
                | (decay << US_DECAY_AMT)
                | (U256::from(decay_cps) << US_DECAY_DUR);
            if new_free.is_zero() {
                if deposit {
                    ratio_check(new_raw, s_raw, true)?;
                    s_raw = add(s_raw, new_raw)?;
                } else {
                    ratio_check(new_raw, s_raw, false)?;
                    s_raw = if s_raw > new_raw {
                        s_raw - new_raw
                    } else {
                        U256::ZERO
                    };
                }
                totals = (totals & !mask(0, 64)) | to_default_big(s_raw, false)?;
            } else {
                if deposit {
                    ratio_check(new_free, s_free, true)?;
                    s_free = add(s_free, new_free)?;
                } else {
                    ratio_check(new_free, s_free, false)?;
                    s_free = if s_free > new_free {
                        s_free - new_free
                    } else {
                        U256::ZERO
                    };
                }
                if s_free > u(MAX_TOKEN_AMOUNT_CAP) {
                    return Err(RouteError::Math);
                }
                totals = (totals & !mask(64, 64)) | (to_default_big(s_free, false)? << 64);
            }
            if before == totals {
                return Err(RouteError::InsufficientLiquidity);
            }
        }
        if !borrow_amount.is_zero() {
            let before = totals;
            let data = borrow_data;
            if data.is_zero() || flag(data, US_PAUSED) {
                return Err(RouteError::StalePool);
            }
            let borrowing = !borrow_amount.is_negative();
            let mag = borrow_amount.unsigned_abs();
            let mut user_borrow = from_big(bits(data, US_AMOUNT, 64));
            let mut new_limit = borrow_limit_before(data, user_borrow, ts)?;
            let (mut new_raw, mut new_free) = (U256::ZERO, U256::ZERO);
            if !(data & U256::ONE).is_zero() {
                if borrowing {
                    new_raw = md_up(mag, prec, bep)?;
                    user_borrow = add(user_borrow, new_raw)?;
                } else {
                    new_raw = md(mag, prec, bep)?;
                    if new_raw > user_borrow {
                        return Err(RouteError::InsufficientLiquidity);
                    }
                    user_borrow -= new_raw;
                }
            } else {
                new_free = mag;
                if borrowing {
                    user_borrow = add(user_borrow, new_free)?;
                } else {
                    if new_free > user_borrow {
                        return Err(RouteError::InsufficientLiquidity);
                    }
                    user_borrow -= new_free;
                }
            }
            if borrowing && user_borrow > new_limit {
                return Err(RouteError::InsufficientLiquidity);
            }
            new_limit = borrow_limit_after(data, user_borrow, new_limit)?;
            let user_big = to_default_big(user_borrow, true)?;
            if bits(data, US_AMOUNT, 64) == user_big {
                return Err(RouteError::InsufficientLiquidity);
            }
            let limit_big = to_default_big(new_limit, false)?;
            // Preserved: mode (bit 0) and everything from bit 162 up.
            let preserved = (data & U256::ONE) | ((data >> 162) << 162);
            borrow_data = preserved
                | (user_big << US_AMOUNT)
                | (limit_big << UB_PREV_LIMIT)
                | (U256::from(ts) << US_TS);
            if new_free.is_zero() {
                if borrowing {
                    ratio_check(new_raw, b_raw, true)?;
                    b_raw = add(b_raw, new_raw)?;
                } else {
                    ratio_check(new_raw, b_raw, false)?;
                    b_raw = if b_raw > new_raw {
                        b_raw - new_raw
                    } else {
                        U256::ZERO
                    };
                }
                totals = (totals & !mask(128, 64)) | (to_default_big(b_raw, true)? << 128);
            } else {
                if borrowing {
                    ratio_check(new_free, b_free, true)?;
                    b_free = add(b_free, new_free)?;
                } else {
                    ratio_check(new_free, b_free, false)?;
                    b_free = if b_free > new_free {
                        b_free - new_free
                    } else {
                        U256::ZERO
                    };
                }
                if b_free > u(MAX_TOKEN_AMOUNT_CAP) {
                    return Err(RouteError::Math);
                }
                totals = (totals & ((U256::ONE << 192) - U256::ONE))
                    | (to_default_big(b_free, true)? << 192);
            }
            if before == totals {
                return Err(RouteError::InsufficientLiquidity);
            }
        }
        // Exchange prices, utilization and the ratios.
        let s_with = md(s_raw, sep, prec)?;
        if s_with > u(MAX_TOKEN_AMOUNT_CAP) && supply_amount.is_positive() {
            return Err(RouteError::Math);
        }
        let total_supply = add(s_free, s_with)?;
        let s_ratio = ratio_word(s_with, s_free, total_supply)?;
        let b_with = md(b_raw, bep, prec)?;
        if b_with > u(MAX_TOKEN_AMOUNT_CAP) && borrow_amount.is_positive() {
            return Err(RouteError::Math);
        }
        let total_borrow = add(b_free, b_with)?;
        let b_ratio = ratio_word(b_with, b_free, total_borrow)?;
        let mut util = 0u128;
        if !total_supply.is_zero() {
            util = div(mul(total_borrow, u(FOUR))?, total_supply)?
                .try_into()
                .map_err(|_| RouteError::Math)?;
            if borrow_amount.is_positive() {
                let max_util = if flag(cfg, EP_USES_CONFIGS2) {
                    field(self.configs2, 0, 14)
                } else {
                    FOUR
                };
                if util > max_util {
                    return Err(RouteError::InsufficientLiquidity);
                }
            }
        }
        let mut new_cfg = cfg;
        let mut write =
            u128::from(ts) > field(cfg, EP_TS, 33) + u128::from(FORCE_STORAGE_WRITE_AFTER_TIME);
        if !write {
            let thr = field(cfg, EP_THRESH, 14);
            write = abs_diff(util, field(cfg, EP_UTIL, 14)) > thr;
            if !write {
                let last = field(cfg, EP_SUPPLY_RATIO, 15);
                write = if last & 1 == s_ratio & 1 {
                    abs_diff(s_ratio >> 1, last >> 1) > thr
                } else {
                    true
                };
                if !write {
                    let last = field(cfg, EP_BORROW_RATIO, 15);
                    write = if last & 1 == b_ratio & 1 {
                        abs_diff(b_ratio >> 1, last >> 1) > thr
                    } else {
                        true
                    };
                }
            }
        }
        if write {
            let rate = borrow_rate(self.rate_data, util)?;
            if sep > (U256::ONE << 64) - U256::ONE
                || bep > (U256::ONE << 64) - U256::ONE
                || util > 0x3fff
            {
                return Err(RouteError::Math);
            }
            // Keep: fee (16..30), threshold (44..58), uses-configs2 / pause
            // (249..) bits; replace the rest.
            let keep_mask = (((U256::ONE << 14) - U256::ONE) << EP_FEE)
                | (((U256::ONE << 14) - U256::ONE) << EP_THRESH)
                | (((U256::ONE << 7) - U256::ONE) << EP_USES_CONFIGS2);
            new_cfg = (cfg & keep_mask)
                | U256::from(rate)
                | (U256::from(util) << EP_UTIL)
                | (U256::from(ts) << EP_TS)
                | (sep << EP_SUPPLY_EP)
                | (bep << EP_BORROW_EP)
                | (U256::from(s_ratio) << EP_SUPPLY_RATIO)
                | (U256::from(b_ratio) << EP_BORROW_RATIO);
        }
        // The layer must hold what it pays out.
        let mut out = U256::ZERO;
        if supply_amount.is_negative() {
            out = add(out, supply_amount.unsigned_abs())?;
        }
        if borrow_amount.is_positive() {
            out = add(out, borrow_amount.unsigned_abs())?;
        }
        if out > balance {
            return Err(RouteError::InsufficientLiquidity);
        }
        Ok(Self {
            ep_cfg: new_cfg,
            totals,
            supply: supply_data,
            borrow: borrow_data,
            balance: balance - out,
            ..*self
        })
    }
}

// ───────────────────────────── the pool ─────────────────────────────

/// One Fluid DEX T1 pool read at a block (the reseed thread's answer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FluidRead {
    /// `dexVariables` (storage slot 0) and `dexVariables2` (slot 1).
    pub dex_vars: U256,
    pub dex_vars2: U256,
    /// The center price hook's `centerPrice()`, when the pool uses one.
    pub center_ext: Option<U256>,
    pub tokens: [LiqToken; 2],
    /// Timestamp the next block executes at (the swap's `block.timestamp`).
    pub exec_ts: u64,
    /// The pool's constants, read once: `constantsView2`'s four precisions
    /// and `constantsView`'s deployer contract (the center price hook is
    /// its CREATE at the hook id).
    pub constants: Option<([U256; 4], Address)>,
}

/// A Fluid DEX T1 pool's swap state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FluidState {
    /// `token0NumeratorPrecision`, `token0DenominatorPrecision`, and the
    /// token1 pair (`constantsView2`).
    pub prec: [U256; 4],
    /// The pool's two tokens in pool order; the native sentinel stays as the
    /// pool names it.
    pub tokens: SmallVec<[Address; MAX_COINS]>,
    pub native: [bool; 2],
    /// The pool's deployer contract (zero until read).
    pub deployer: Address,
    pub dex_vars: U256,
    pub dex_vars2: U256,
    pub center_ext: Option<U256>,
    pub liq: [LiqToken; 2],
    /// `block.timestamp` the quotes are computed at.
    pub exec_ts: u64,
    /// Set by a Liquidity `LogOperate` on the pool's tokens or a pool log;
    /// cleared by a read.
    pub stale: bool,
    pub stale_block: u64,
    pub read_block: u64,
}

struct Prices {
    center: U256,
    upper: U256,
    lower: U256,
    geometric_mean: U256,
}

impl FluidState {
    /// `true` when the pool can be quoted: read, not stale, not paused or
    /// shifting or hooked, both layer positions defined.
    #[must_use]
    pub fn is_live(&self) -> bool {
        let dv2 = self.dex_vars2;
        !self.stale
            && self.tokens.len() == 2
            && !flag(dv2, 255)
            && (dv2 & U256::from(3u64)) != U256::ZERO
            && !flag(dv2, 26)
            && !flag(dv2, 248)
            && !flag(dv2, 67)
            && field(dv2, 142, 30) == 0
            && (field(dv2, 112, 30) == 0 || self.center_ext.is_some())
            && (!self.liq[0].supply.is_zero() || !self.liq[0].borrow.is_zero())
            && (!self.liq[1].supply.is_zero() || !self.liq[1].borrow.is_zero())
    }

    /// The first half of `_getPricesAndExchangePrices` for a pool with no
    /// active shift.
    fn prices(&self, ts: u64) -> R<Prices> {
        let (dv1, dv2) = (self.dex_vars, self.dex_vars2);
        let mut center = if field(dv2, 112, 30) == 0 {
            from_big(bits(dv1, 81, 40))
        } else {
            self.center_ext.ok_or(RouteError::StalePool)?
        };
        let last_stored = from_big(bits(dv1, 41, 40));
        let upper_pct = u(field(dv2, 27, 20));
        let lower_pct = u(field(dv2, 47, 20));
        let six = u(SIX);
        let mut upper = md(center, six, sub(six, upper_pct)?)?;
        let mut lower = md(center, sub(six, lower_pct)?, six)?;
        let mut changed = false;
        if field(dv2, 68, 20) > 0 {
            let up_thr = u(field(dv2, 68, 10));
            let lo_thr = u(field(dv2, 78, 10));
            let shifting_time = field(dv2, 88, 24);
            let elapsed = u128::from(ts)
                .checked_sub(field(dv1, 121, 33))
                .ok_or(RouteError::Math)?;
            let three = u(THREE);
            if last_stored > add(center, md(sub(upper, center)?, sub(three, up_thr)?, three)?)? {
                center = if elapsed < shifting_time {
                    add(
                        center,
                        md(sub(upper, center)?, u(elapsed), u(shifting_time))?,
                    )?
                } else {
                    upper
                };
                changed = true;
            } else if last_stored
                < sub(center, md(sub(center, lower)?, sub(three, lo_thr)?, three)?)?
            {
                center = if elapsed < shifting_time {
                    sub(
                        center,
                        md(sub(center, lower)?, u(elapsed), u(shifting_time))?,
                    )?
                } else {
                    lower
                };
                changed = true;
            }
        }
        let max = from_big(bits(dv2, 172, 28));
        if center > max {
            center = max;
            changed = true;
        } else {
            let min = from_big(bits(dv2, 200, 28));
            if center < min {
                center = min;
                changed = true;
            }
        }
        if changed {
            upper = md(center, six, sub(six, upper_pct)?)?;
            lower = md(center, sub(six, lower_pct)?, six)?;
        }
        let geometric_mean = if upper < u(10u128.pow(38)) {
            isqrt(mul(upper, lower)?)
        } else {
            let e18 = u(10u128.pow(18));
            mul(isqrt(mul(upper / e18, lower / e18)?), e18)?
        };
        Ok(Prices {
            center,
            upper,
            lower,
            geometric_mean,
        })
    }

    /// Real and imaginary collateral reserves (`_getCollateralReserves`).
    fn col_reserves(&self, p: &Prices, sep: [U256; 2]) -> R<[U256; 4]> {
        let supply = |k: usize| -> R<U256> {
            let data = self.liq[k].supply;
            let mut amt = from_big(bits(data, US_AMOUNT, 64));
            if !(data & U256::ONE).is_zero() {
                amt = md(amt, sep[k], u(EXCHANGE_PRICES_PRECISION))?;
            }
            md(amt, self.prec[2 * k], self.prec[2 * k + 1])
        };
        let (s0, s1) = (supply(0)?, supply(1)?);
        let e27 = u(E27);
        let (i0, i1) = if p.geometric_mean < e27 {
            outside_range(p.geometric_mean, p.upper, s0, s1)?
        } else {
            let e54 = mul(e27, e27)?;
            let (b, a) = outside_range(div(e54, p.geometric_mean)?, div(e54, p.lower)?, s1, s0)?;
            (a, b)
        };
        Ok([s0, s1, add(i0, s0)?, add(i1, s1)?])
    }

    /// Debt and its reserves (`_getDebtReserves`): `[d0, d1, r0, r1, i0, i1]`.
    fn debt_reserves(&self, p: &Prices, bep: [U256; 2]) -> R<[U256; 6]> {
        let debt = |k: usize| -> R<U256> {
            let data = self.liq[k].borrow;
            let mut amt = from_big(bits(data, US_AMOUNT, 64));
            if !(data & U256::ONE).is_zero() {
                amt = md(amt, bep[k], u(EXCHANGE_PRICES_PRECISION))?;
            }
            md(amt, self.prec[2 * k], self.prec[2 * k + 1])
        };
        let (d0, d1) = (debt(0)?, debt(1)?);
        let e27 = u(E27);
        let (r0, r1, i0, i1);
        if p.geometric_mean < e27 {
            let (rx, ry, irx, iry) = debt_reserves_calc(p.geometric_mean, p.lower, d0, d1)?;
            (r0, r1, i0, i1) = (rx, ry, irx, iry);
        } else {
            let e54 = mul(e27, e27)?;
            let (rx, ry, irx, iry) =
                debt_reserves_calc(div(e54, p.geometric_mean)?, div(e54, p.upper)?, d1, d0)?;
            (r1, r0, i1, i0) = (rx, ry, irx, iry);
        }
        Ok([d0, d1, r0, r1, i0, i1])
    }

    /// `_swapIn` through both `LIQUIDITY.operate` calls: the output of
    /// `amount_in` of coin `zfo ? 0 : 1`, and the state after.
    pub fn swap(&self, swap0to1: bool, amount_in: U256) -> R<(U256, Self)> {
        if self.stale {
            return Err(RouteError::StalePool);
        }
        let ts = self.exec_ts;
        let (dv1, dv2) = (self.dex_vars, self.dex_vars2);
        if flag(dv2, 255) || flag(dv1, 0) || (dv2 & U256::from(3u64)).is_zero() {
            return Err(RouteError::StalePool);
        }
        if flag(dv2, 26) || flag(dv2, 248) || flag(dv2, 67) || field(dv2, 142, 30) != 0 {
            return Err(RouteError::StalePool);
        }
        let (n_in, d_in, n_out, d_out) = if swap0to1 {
            (self.prec[0], self.prec[1], self.prec[2], self.prec[3])
        } else {
            (self.prec[2], self.prec[3], self.prec[0], self.prec[1])
        };
        let adj = md(amount_in, n_in, d_in)?;
        if adj < u(SIX)
            || adj >= U256::ONE << 96
            || amount_in < U256::from(100u64)
            || amount_in >= U256::ONE << 128
        {
            return Err(RouteError::InsufficientLiquidity);
        }
        let p = self.prices(ts)?;
        let (sep0, bep0) = calc_exchange_prices(self.liq[0].ep_cfg, ts)?;
        let (sep1, bep1) = calc_exchange_prices(self.liq[1].ep_cfg, ts)?;
        let smart_col = flag(dv2, 0);
        let smart_debt = flag(dv2, 1);
        let fee_raw = field(dv2, 2, 17);
        let revenue_cut = EIGHT - field(dv2, 19, 7) * fee_raw;
        let fee = SIX - fee_raw;

        // [in_real, out_real, in_imag, out_imag]
        let mut cs = [U256::ZERO; 4];
        // [in_debt, out_debt, in_real, out_real, in_imag, out_imag]
        let mut ds = [U256::ZERO; 6];
        if smart_col {
            let c = self.col_reserves(&p, [sep0, sep1])?;
            cs = if swap0to1 {
                [c[0], c[1], c[2], c[3]]
            } else {
                [c[1], c[0], c[3], c[2]]
            };
        }
        if smart_debt {
            let d = self.debt_reserves(&p, [bep0, bep1])?;
            ds = if swap0to1 {
                [d[0], d[1], d[2], d[3], d[4], d[5]]
            } else {
                [d[1], d[0], d[3], d[2], d[5], d[4]]
            };
        }
        if adj > add(cs[2], ds[4])? / U256::from(2u64) {
            return Err(RouteError::InsufficientLiquidity);
        }
        let mut routing = I256::ZERO;
        if smart_col && smart_debt {
            routing = swap_routing_in(adj, cs[3], cs[2], ds[5], ds[4])?;
        }
        let adj_s = signed(adj)?;
        let (mut t_col, mut t_debt);
        if adj_s > routing && routing.is_positive() {
            t_col = unsigned(routing)?;
            t_debt = adj - t_col;
        } else if (smart_col && !smart_debt) || routing >= adj_s {
            t_col = adj;
            t_debt = U256::ZERO;
        } else if (!smart_col && smart_debt) || !routing.is_positive() {
            t_col = U256::ZERO;
            t_debt = adj;
        } else {
            return Err(RouteError::InsufficientLiquidity);
        }
        let (mut o_col, mut o_debt) = (U256::ZERO, U256::ZERO);
        let center = p.center;
        let verify = |real_in: U256, real_out: U256, add_in: U256, take: U256| -> R<()> {
            let e27 = u(E27);
            if swap0to1 {
                let r0 = add(real_in, add_in)?;
                let r1 = sub(real_out, take)?;
                if r1 < md(r0, center, mul(e27, u(MINIMUM_LIQUIDITY_SWAP))?)? {
                    return Err(RouteError::InsufficientLiquidity);
                }
            } else {
                let r0 = sub(real_out, take)?;
                let r1 = add(real_in, add_in)?;
                if r0 < md(r1, e27, mul(center, u(MINIMUM_LIQUIDITY_SWAP))?)? {
                    return Err(RouteError::InsufficientLiquidity);
                }
            }
            Ok(())
        };
        if !t_col.is_zero() {
            o_col = amount_out(md(t_col, u(fee), u(SIX))?, cs[2], cs[3])?;
            verify(cs[0], cs[1], t_col, o_col)?;
        }
        if !t_debt.is_zero() {
            o_debt = amount_out(md(t_debt, u(fee), u(SIX))?, ds[4], ds[5])?;
            verify(ds[2], ds[3], t_debt, o_debt)?;
        }
        t_col = md(t_col, u(revenue_cut), u(EIGHT))?;
        t_debt = md(t_debt, u(revenue_cut), u(EIGHT))?;
        let e27 = u(E27);
        let price = if t_col > t_debt {
            if swap0to1 {
                md(sub(cs[3], o_col)?, e27, add(cs[2], t_col)?)?
            } else {
                md(add(cs[2], t_col)?, e27, sub(cs[3], o_col)?)?
            }
        } else if swap0to1 {
            md(sub(ds[5], o_debt)?, e27, add(ds[4], t_debt)?)?
        } else {
            md(add(ds[4], t_debt)?, e27, sub(ds[5], o_debt)?)?
        };
        t_col = md(t_col, d_in, n_in)?;
        t_debt = md(t_debt, d_in, n_in)?;
        o_col = md(o_col, d_out, n_out)?;
        o_debt = md(o_debt, d_out, n_out)?;
        let out = add(o_col, o_debt)?;

        // The two `LIQUIDITY.operate` calls.
        let (i_in, i_out) = if swap0to1 { (0usize, 1usize) } else { (1, 0) };
        let credited = add(t_col, t_debt)?;
        if amount_in < credited
            || amount_in > md(credited, u(FOUR + MAX_INPUT_AMOUNT_EXCESS), u(FOUR))?
        {
            return Err(RouteError::InsufficientLiquidity);
        }
        let mut liq = self.liq;
        liq[i_in] = liq[i_in].operate(signed(t_col)?, -signed(t_debt)?, ts, amount_in)?;
        liq[i_out] = liq[i_out].operate(-signed(o_col)?, signed(o_debt)?, ts, U256::ZERO)?;
        let limit = if swap0to1 {
            field(dv2, 238, 10)
        } else {
            field(dv2, 228, 10)
        };
        if limit < THREE {
            let util = field(liq[i_out].ep_cfg, EP_UTIL, 14);
            if util > limit * 10 {
                return Err(RouteError::InsufficientLiquidity);
            }
        }
        // `_updateOracle`: the parts that gate the swap and the next quote.
        let time_diff = u128::from(ts)
            .checked_sub(field(dv1, 121, 33))
            .ok_or(RouteError::Math)?;
        let new_dv1 = if time_diff == 0 {
            let old_center = from_big(bits(dv1, 81, 40));
            let eight = u(EIGHT);
            if center < md(eight - U256::ONE, old_center, eight)?
                || center > md(eight + U256::ONE, old_center, eight)?
            {
                return Err(RouteError::InsufficientLiquidity);
            }
            let older = from_big(bits(dv1, 1, 40));
            price_diff_check(older, price)?;
            (dv1 & !mask(41, 40)) | (to_big(price, 32, 8, false)? << 41)
        } else {
            let last = from_big(bits(dv1, 41, 40));
            price_diff_check(last, price)?;
            if flag(dv1, 195) {
                // Oracle active: bits 1..194 are rewritten. The oracle's slot
                // pointer and mapping (176.., 179..) advance on chain in ways
                // no quote reads; the pool's own LogOperate re-reads it.
                (dv1 & !mask(1, 194))
                    | (bits(dv1, 41, 40) << 1)
                    | (to_big(price, 32, 8, false)? << 41)
                    | (to_big(center, 32, 8, false)? << 81)
                    | (U256::from(ts) << 121)
                    | (U256::from(time_diff.min((1u128 << 22) - 1)) << 154)
                    | (bits(dv1, 176, 3) << 176)
                    | (bits(dv1, 179, 16) << 179)
            } else {
                (dv1 & !mask(1, 153))
                    | (bits(dv1, 41, 40) << 1)
                    | (to_big(price, 32, 8, false)? << 41)
                    | (to_big(center, 32, 8, false)? << 81)
                    | (U256::from(ts) << 121)
            }
        };
        let mut after = self.clone();
        after.liq = liq;
        after.dex_vars = new_dv1;
        Ok((out, after))
    }

    /// Output of selling `dx` of coin `i` for coin `j`.
    pub fn dy(&self, i: u8, j: u8, dx: U256) -> R<U256> {
        let zfo = zero_for_one(i, j)?;
        if dx.is_zero() {
            return Ok(U256::ZERO);
        }
        self.swap(zfo, dx).map(|r| r.0)
    }

    /// The output, **mutating** the state the way both layers do.
    pub fn apply(&mut self, i: u8, j: u8, dx: U256) -> R<U256> {
        let zfo = zero_for_one(i, j)?;
        let (out, next) = self.swap(zfo, dx)?;
        *self = next;
        Ok(out)
    }

    /// The input of coin `i` the pool absorbs: half the imaginary reserves
    /// of the side taking the input (`swap in limiting amounts`), as raw
    /// units of coin `i`.
    pub fn capacity_in(&self, i: u8) -> Option<U256> {
        let zfo = i == 0;
        let ts = self.exec_ts;
        let p = self.prices(ts).ok()?;
        let (sep0, bep0) = calc_exchange_prices(self.liq[0].ep_cfg, ts).ok()?;
        let (sep1, bep1) = calc_exchange_prices(self.liq[1].ep_cfg, ts).ok()?;
        let mut imag = U256::ZERO;
        if flag(self.dex_vars2, 0) {
            let c = self.col_reserves(&p, [sep0, sep1]).ok()?;
            imag = add(imag, if zfo { c[2] } else { c[3] }).ok()?;
        }
        if flag(self.dex_vars2, 1) {
            let d = self.debt_reserves(&p, [bep0, bep1]).ok()?;
            imag = add(imag, if zfo { d[4] } else { d[5] }).ok()?;
        }
        let (n, dn) = if zfo {
            (self.prec[0], self.prec[1])
        } else {
            (self.prec[2], self.prec[3])
        };
        // adjusted amount ≤ imag / 2 → raw amount = adjusted · den / num.
        md(imag / U256::from(2u64), dn, n).ok()
    }

    /// `ρ(0)` by a forward difference of the exact output (the pool has no
    /// integer derivative); the step is the pool's one-millionth input side
    /// capacity, at least the smallest swap the pool accepts.
    pub fn rho(&self, i: u8, j: u8, x: U256) -> R<U256> {
        let cap = self
            .capacity_in(i)
            .ok_or(RouteError::InsufficientLiquidity)?;
        let (n, dn) = if i == 0 {
            (self.prec[0], self.prec[1])
        } else {
            (self.prec[2], self.prec[3])
        };
        // the smallest input the pool accepts: adjusted ≥ 1e6
        let floor = md_up(u(SIX), dn, n)?.max(U256::from(100u64));
        let h = (cap / U256::from(1_000_000u64)).max(
            floor
                .checked_mul(U256::from(10u64))
                .ok_or(RouteError::Math)?,
        );
        let a = if x.is_zero() { h } else { x };
        let q0 = self.dy(i, j, a)?;
        let q1 = self.dy(i, j, add(a, h)?)?;
        let dq = q1.saturating_sub(q0);
        let q = alloy_primitives::U512::from(dq)
            .checked_mul(alloy_primitives::U512::from(U256::ONE << 192))
            .ok_or(RouteError::Math)?
            .checked_div(alloy_primitives::U512::from(h))
            .ok_or(RouteError::Math)?;
        narrow(q.root(2))
    }
}

#[inline]
fn zero_for_one(i: u8, j: u8) -> R<bool> {
    match (i, j) {
        (0, 1) => Ok(true),
        (1, 0) => Ok(false),
        _ => Err(RouteError::BadLeg),
    }
}

fn amount_out(amount_in: U256, i_in: U256, i_out: U256) -> R<U256> {
    md(amount_in, i_out, add(i_in, amount_in)?)
}

/// `_calculateReservesOutsideRange`.
fn outside_range(gp: U256, pa: U256, rx: U256, ry: U256) -> R<(U256, U256)> {
    let e27 = u(E27);
    let p1 = sub(pa, gp)?;
    let p2 = div(
        add(mul(gp, rx)?, mul(ry, e27)?)?,
        mul(U256::from(2u64), p1)?,
    )?;
    let prod = mul(rx, ry)?;
    let p3 = if prod < u(10u128.pow(25)) * u(10u128.pow(25)) {
        div(mul(prod, e27)?, p1)?
    } else {
        mul(div(prod, p1)?, e27)?
    };
    let xa = add(p2, isqrt(add(p3, mul(p2, p2)?)?))?;
    let yb = md(xa, gp, e27)?;
    Ok((xa, yb))
}

/// `_calculateDebtReserves`: `(rx, ry, irx, iry)` from the debt of both
/// tokens.
fn debt_reserves_calc(gp: U256, pb: U256, dx: U256, dy: U256) -> R<(U256, U256, U256, U256)> {
    let e27 = u(E27);
    let p1 = signed(mul(dx, gp)?)?
        .checked_sub(signed(mul(dy, e27)?)?)
        .ok_or(RouteError::Math)?
        / signed(mul(U256::from(2u64), e27)?)?;
    let prod = mul(dx, dy)?;
    let p2 = if prod < u(10u128.pow(25)) * u(10u128.pow(25)) {
        div(mul(prod, pb)?, e27)?
    } else {
        mul(div(prod, e27)?, pb)?
    };
    let p1sq = signed(mul(unsigned_abs(p1), unsigned_abs(p1))?)?;
    let root = signed(isqrt(unsigned(
        signed(p2)?.checked_add(p1sq).ok_or(RouteError::Math)?,
    )?))?;
    let ry_s = p1.checked_add(root).ok_or(RouteError::Math)?;
    if ry_s.is_negative() {
        return Err(RouteError::Math);
    }
    let ry = unsigned(ry_s)?;
    let iry_s = signed(mul(ry, e27)?)?
        .checked_sub(signed(mul(dx, pb)?)?)
        .ok_or(RouteError::Math)?;
    if iry_s < signed(u(SIX))? {
        return Err(RouteError::InsufficientLiquidity);
    }
    let iry_raw = unsigned(iry_s)?;
    let iry = if ry < u(10u128.pow(25)) {
        div(mul(mul(ry, ry)?, e27)?, iry_raw)?
    } else {
        div(mul(ry, ry)?, iry_raw / e27)?
    };
    let irx = sub(div(mul(iry, dx)?, ry)?, dx)?;
    let rx = div(mul(irx, dy)?, add(iry, dy)?)?;
    Ok((rx, ry, irx, iry))
}

fn unsigned_abs(v: I256) -> U256 {
    v.unsigned_abs()
}

/// `_swapRoutingIn`: the share of the input the collateral side takes.
fn swap_routing_in(t: U256, x: U256, y: U256, x2: U256, y2: U256) -> R<I256> {
    let e18 = u(10u128.pow(18));
    let xy = isqrt(mul(mul(x, y)?, e18)?);
    let x2y2 = isqrt(mul(mul(x2, y2)?, e18)?);
    let num = signed(mul(y2, xy)?)?
        .checked_add(signed(mul(t, xy)?)?)
        .and_then(|v| v.checked_sub(signed(mul(y, x2y2).ok()?).ok()?))
        .ok_or(RouteError::Math)?;
    let den = signed(add(xy, x2y2)?)?;
    num.checked_div(den).ok_or(RouteError::Math)
}

/// `_priceDiffCheck`.
fn price_diff_check(old: U256, new: U256) -> R<()> {
    if new.is_zero() {
        return Err(RouteError::Math);
    }
    let p = u(ORACLE_PRECISION);
    let ratio = md(old, p, new)?;
    let diff = signed(p)? - signed(ratio)?;
    if diff > signed(u(ORACLE_LIMIT))? || diff < -signed(u(ORACLE_LIMIT))? {
        return Err(RouteError::InsufficientLiquidity);
    }
    Ok(())
}
