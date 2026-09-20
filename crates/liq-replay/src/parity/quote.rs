//! Integer quotes matching Uniswap V2/V3, Curve StableSwap, Kyber Elastic.

use alloy_primitives::{I256, U256};
use uniswap_v3_math::swap_math;

use super::{AmmFamily, ParityError};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct V3Step {
    pub sqrt_next: U256,
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee: U256,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CurveQuote {
    pub amount_out: U256,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct KyberStep {
    pub used: I256,
    pub returned: I256,
    pub delta_l: U256,
    pub next_sqrt: U256,
}

/// `UniswapV2Library.getAmountOut` (`997 / 1000`).
pub fn v2_get_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
) -> Result<U256, ParityError> {
    if amount_in.is_zero() {
        return Err(ParityError::Math("v2 insufficient input"));
    }
    if reserve_in.is_zero() || reserve_out.is_zero() {
        return Err(ParityError::Math("v2 insufficient liquidity"));
    }
    let in_fee = amount_in
        .checked_mul(U256::from(997u64))
        .ok_or(ParityError::Math("v2 overflow"))?;
    let num = in_fee
        .checked_mul(reserve_out)
        .ok_or(ParityError::Math("v2 overflow"))?;
    let den = reserve_in
        .checked_mul(U256::from(1000u64))
        .and_then(|v| v.checked_add(in_fee))
        .ok_or(ParityError::Math("v2 overflow"))?;
    num.checked_div(den).ok_or(ParityError::Math("v2 div"))
}

/// Uniswap V3 `SwapMath.computeSwapStep` via `uniswap_v3_math` (Solidity port).
pub fn v3_compute_swap_step(
    sqrt_current: U256,
    sqrt_target: U256,
    liquidity: u128,
    amount_remaining: I256,
    fee_pips: u32,
) -> Result<V3Step, ParityError> {
    let (sqrt_next, amount_in, amount_out, fee) = swap_math::compute_swap_step(
        sqrt_current,
        sqrt_target,
        liquidity,
        amount_remaining,
        fee_pips,
    )
    .map_err(|_| ParityError::Math("v3 swap step"))?;
    Ok(V3Step {
        sqrt_next,
        amount_in,
        amount_out,
        fee,
    })
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CurveQuoteIn {
    pub balances: [U256; 2],
    pub rates: [U256; 2],
    pub amp: U256,
    pub a_precision: U256,
    pub fee: U256,
    pub i: usize,
    pub j: usize,
    pub dx: U256,
}

/// Curve StableSwap `get_dy` for a 2-coin plain pool (same Newton as UniOracle).
pub fn curve_get_dy(p: CurveQuoteIn) -> Result<U256, ParityError> {
    let CurveQuoteIn {
        balances,
        rates,
        amp,
        a_precision,
        fee,
        i,
        j,
        dx,
    } = p;
    if i == j || i >= 2 || j >= 2 {
        return Err(ParityError::Math("curve bad i/j"));
    }
    let wad = U256::from(1_000_000_000_000_000_000u64);
    let xp0 = balances[0]
        .checked_mul(rates[0])
        .and_then(|v| v.checked_div(wad))
        .ok_or(ParityError::Math("curve xp0"))?;
    let xp1 = balances[1]
        .checked_mul(rates[1])
        .and_then(|v| v.checked_div(wad))
        .ok_or(ParityError::Math("curve xp1"))?;
    let xp = [xp0, xp1];
    let add = dx
        .checked_mul(*rates.get(i).ok_or(ParityError::Math("curve rate"))?)
        .and_then(|v| v.checked_div(wad))
        .ok_or(ParityError::Math("curve dx xp"))?;
    let x = xp
        .get(i)
        .ok_or(ParityError::Math("curve xp i"))?
        .checked_add(add)
        .ok_or(ParityError::Math("curve x"))?;
    let y = get_y(i, j, x, xp, amp, a_precision)?;
    let xj = *xp.get(j).ok_or(ParityError::Math("curve xp j"))?;
    let dy = xj
        .checked_sub(y)
        .and_then(|v| v.checked_sub(U256::from(1u64)))
        .ok_or(ParityError::Math("curve dy"))?;
    let dy_fee = dy
        .checked_mul(fee)
        .and_then(|v| v.checked_div(U256::from(10_000_000_000u64)))
        .ok_or(ParityError::Math("curve fee"))?;
    let after = dy
        .checked_sub(dy_fee)
        .ok_or(ParityError::Math("curve after fee"))?;
    let rj = *rates.get(j).ok_or(ParityError::Math("curve rj"))?;
    after
        .checked_mul(wad)
        .and_then(|v| v.checked_div(rj))
        .ok_or(ParityError::Math("curve to raw"))
}

fn get_d(xp: [U256; 2], amp: U256, a_precision: U256) -> Result<U256, ParityError> {
    let s = xp[0]
        .checked_add(xp[1])
        .ok_or(ParityError::Math("curve S"))?;
    if s.is_zero() {
        return Ok(U256::ZERO);
    }
    let n = U256::from(2u64);
    let ann = amp.checked_mul(n).ok_or(ParityError::Math("curve Ann"))?;
    let mut d = s;
    let n_plus_1 = n.checked_add(U256::ONE).ok_or(ParityError::Math("n+1"))?;
    for _ in 0..255 {
        let mut dp = d;
        for &x in &xp {
            let den = x.checked_mul(n).ok_or(ParityError::Math("curve dP den"))?;
            if den.is_zero() {
                return Err(ParityError::Math("curve zero xp"));
            }
            dp = dp
                .checked_mul(d)
                .and_then(|v| v.checked_div(den))
                .ok_or(ParityError::Math("curve dP"))?;
        }
        let d_prev = d;
        let dp_n = dp.checked_mul(n).ok_or(ParityError::Math("dP n"))?;
        let t1 = ann
            .checked_mul(s)
            .and_then(|v| v.checked_div(a_precision))
            .and_then(|v| v.checked_add(dp_n))
            .and_then(|v| v.checked_mul(d))
            .ok_or(ParityError::Math("curve D num"))?;
        let n1_dp = n_plus_1
            .checked_mul(dp)
            .ok_or(ParityError::Math("(n+1)dP"))?;
        let t2 = ann
            .checked_sub(a_precision)
            .and_then(|v| v.checked_mul(d))
            .and_then(|v| v.checked_div(a_precision))
            .and_then(|v| v.checked_add(n1_dp))
            .ok_or(ParityError::Math("curve D den"))?;
        d = t1.checked_div(t2).ok_or(ParityError::Math("curve D div"))?;
        if d.abs_diff(d_prev) <= U256::ONE {
            return Ok(d);
        }
    }
    Err(ParityError::Math("curve D non-converge"))
}

fn get_y(
    i: usize,
    j: usize,
    x: U256,
    xp: [U256; 2],
    amp: U256,
    a_precision: U256,
) -> Result<U256, ParityError> {
    let n = U256::from(2u64);
    let d = get_d(xp, amp, a_precision)?;
    let ann = amp.checked_mul(n).ok_or(ParityError::Math("y Ann"))?;
    let mut c = d;
    let mut s_ = U256::ZERO;
    for k in 0..2 {
        let xk = if k == i {
            x
        } else if k != j {
            *xp.get(k).ok_or(ParityError::Math("y xp"))?
        } else {
            continue;
        };
        s_ = s_.checked_add(xk).ok_or(ParityError::Math("y S"))?;
        let den = xk.checked_mul(n).ok_or(ParityError::Math("y den"))?;
        if den.is_zero() {
            return Err(ParityError::Math("y zero"));
        }
        c = c
            .checked_mul(d)
            .and_then(|v| v.checked_div(den))
            .ok_or(ParityError::Math("y c"))?;
    }
    let ann_n = ann.checked_mul(n).ok_or(ParityError::Math("Ann n"))?;
    c = c
        .checked_mul(d)
        .and_then(|v| v.checked_mul(a_precision))
        .and_then(|v| v.checked_div(ann_n))
        .ok_or(ParityError::Math("y c2"))?;
    let b = d
        .checked_mul(a_precision)
        .and_then(|v| v.checked_div(ann))
        .and_then(|v| v.checked_add(s_))
        .ok_or(ParityError::Math("y b"))?;
    let mut y = d;
    for _ in 0..255 {
        let y_prev = y;
        let num = y
            .checked_mul(y)
            .and_then(|v| v.checked_add(c))
            .ok_or(ParityError::Math("y num"))?;
        let den = y
            .checked_mul(U256::from(2u64))
            .and_then(|v| v.checked_add(b))
            .and_then(|v| v.checked_sub(d))
            .ok_or(ParityError::Math("y den"))?;
        y = num.checked_div(den).ok_or(ParityError::Math("y div"))?;
        if y.abs_diff(y_prev) <= U256::ONE {
            return Ok(y);
        }
    }
    Err(ParityError::Math("curve Y non-converge"))
}

/// Wei equality is the acceptance rule. Any mismatch is a bug until proven.
pub fn assert_wei_eq(family: AmmFamily, rust: U256, revm: U256) -> Result<(), ParityError> {
    if rust == revm {
        Ok(())
    } else {
        Err(ParityError::WeiDivergence { family, rust, revm })
    }
}
