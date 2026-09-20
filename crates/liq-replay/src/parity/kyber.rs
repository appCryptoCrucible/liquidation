//! Kyber Elastic `SwapMath.computeSwapStep` (ks-elastic-sc), exact-integer.

use alloy_primitives::{I256, U256};
use uniswap_v3_math::full_math::{mul_div, mul_div_rounding_up};

use super::{quote::KyberStep, ParityError};

const TWO_FEE_UNITS: u64 = 200_000;

fn q96() -> U256 {
    U256::from(1u128 << 96)
}

fn two_fee() -> U256 {
    U256::from(TWO_FEE_UNITS)
}

fn to_i256(y: U256) -> Result<I256, ParityError> {
    I256::try_from(y).map_err(|_| ParityError::Math("kyber toInt256"))
}

fn rev_i256(y: U256) -> Result<I256, ParityError> {
    let v = to_i256(y)?;
    v.checked_neg()
        .ok_or(ParityError::Math("kyber revToInt256"))
}

fn abs_u256(x: I256) -> Result<U256, ParityError> {
    if x >= I256::ZERO {
        U256::try_from(x).map_err(|_| ParityError::Math("kyber abs+"))
    } else {
        let n = x.checked_neg().ok_or(ParityError::Math("kyber abs neg"))?;
        U256::try_from(n).map_err(|_| ParityError::Math("kyber abs-"))
    }
}

fn floor(a: U256, b: U256, d: U256) -> Result<U256, ParityError> {
    mul_div(a, b, d).map_err(|_| ParityError::Math("kyber mulDivFloor"))
}

fn ceil(a: U256, b: U256, d: U256) -> Result<U256, ParityError> {
    mul_div_rounding_up(a, b, d).map_err(|_| ParityError::Math("kyber mulDivCeiling"))
}

fn to_u160(y: U256) -> Result<U256, ParityError> {
    let one: U256 = U256::from(1u64);
    let max = one
        .checked_shl(160)
        .ok_or(ParityError::Math("2^160"))?
        .saturating_sub(U256::ONE);
    if y > max {
        return Err(ParityError::Math("kyber toUint160"));
    }
    Ok(y)
}

pub fn compute_swap_step(
    liquidity: U256,
    current_sqrt: U256,
    target_sqrt: U256,
    fee_in_fee_units: U256,
    specified: I256,
    exact_in: bool,
    is_token0: bool,
) -> Result<KyberStep, ParityError> {
    if current_sqrt == target_sqrt {
        return Ok(KyberStep {
            used: I256::ZERO,
            returned: I256::ZERO,
            delta_l: U256::ZERO,
            next_sqrt: current_sqrt,
        });
    }
    let mut used = calc_reach_amount(
        liquidity,
        current_sqrt,
        target_sqrt,
        fee_in_fee_units,
        exact_in,
        is_token0,
    )?;
    let mut next_sqrt = U256::ZERO;
    if (exact_in && used > specified) || (!exact_in && used <= specified) {
        used = specified;
    } else {
        next_sqrt = target_sqrt;
    }
    let abs_delta = abs_u256(used)?;
    let delta_l;
    if next_sqrt.is_zero() {
        delta_l = estimate_delta_l(
            abs_delta,
            liquidity,
            current_sqrt,
            fee_in_fee_units,
            exact_in,
            is_token0,
        )?;
        next_sqrt = to_u160(calc_final_price(
            abs_delta,
            liquidity,
            delta_l,
            current_sqrt,
            exact_in,
            is_token0,
        )?)?;
    } else {
        delta_l = calc_inc_liq(
            abs_delta,
            liquidity,
            current_sqrt,
            next_sqrt,
            exact_in,
            is_token0,
        )?;
    }
    let mut returned = calc_returned(
        liquidity,
        current_sqrt,
        next_sqrt,
        delta_l,
        exact_in,
        is_token0,
    )?;
    if exact_in && returned == I256::try_from(1i64).map_err(|_| ParityError::Math("1"))? {
        returned = I256::ZERO;
    }
    Ok(KyberStep {
        used,
        returned,
        delta_l,
        next_sqrt,
    })
}

fn calc_reach_amount(
    liquidity: U256,
    current: U256,
    target: U256,
    fee: U256,
    exact_in: bool,
    is_token0: bool,
) -> Result<I256, ParityError> {
    let abs_diff = current.abs_diff(target);
    if exact_in {
        if is_token0 {
            let den = two_fee()
                .checked_mul(target)
                .and_then(|v| v.checked_sub(fee.checked_mul(current)?))
                .ok_or(ParityError::Math("kyber reach den0"))?;
            let num = floor(
                liquidity,
                two_fee()
                    .checked_mul(abs_diff)
                    .ok_or(ParityError::Math("2f d"))?,
                den,
            )?;
            to_i256(floor(num, q96(), current)?)
        } else {
            let den = two_fee()
                .checked_mul(current)
                .and_then(|v| v.checked_sub(fee.checked_mul(target)?))
                .ok_or(ParityError::Math("kyber reach den1"))?;
            let num = floor(
                liquidity,
                two_fee()
                    .checked_mul(abs_diff)
                    .ok_or(ParityError::Math("2f d1"))?,
                den,
            )?;
            to_i256(floor(num, current, q96())?)
        }
    } else if is_token0 {
        let den = two_fee()
            .checked_mul(current)
            .and_then(|v| v.checked_sub(fee.checked_mul(target)?))
            .ok_or(ParityError::Math("kyber reach out0 den"))?;
        let mut num = den
            .checked_sub(
                fee.checked_mul(current)
                    .ok_or(ParityError::Math("fee*cur"))?,
            )
            .ok_or(ParityError::Math("kyber reach out0 num"))?;
        num = floor(
            liquidity
                .checked_shl(96)
                .ok_or(ParityError::Math("L<<96"))?,
            num,
            den,
        )?;
        rev_i256(
            floor(num, abs_diff, current)?
                .checked_div(target)
                .ok_or(ParityError::Math(" /target"))?,
        )
    } else {
        let den = two_fee()
            .checked_mul(target)
            .and_then(|v| v.checked_sub(fee.checked_mul(current)?))
            .ok_or(ParityError::Math("kyber reach out1 den"))?;
        let mut num = den
            .checked_sub(
                fee.checked_mul(target)
                    .ok_or(ParityError::Math("fee*tgt"))?,
            )
            .ok_or(ParityError::Math("kyber reach out1 num"))?;
        num = floor(liquidity, num, den)?;
        rev_i256(floor(num, abs_diff, q96())?)
    }
}

fn estimate_delta_l(
    abs_delta: U256,
    _liquidity: U256,
    current: U256,
    fee: U256,
    exact_in: bool,
    is_token0: bool,
) -> Result<U256, ParityError> {
    if !exact_in {
        return Err(ParityError::Math(
            "kyber exact-out quadratic not used in 05E fixtures",
        ));
    }
    if is_token0 {
        let den = two_fee()
            .checked_shl(96)
            .ok_or(ParityError::Math("2fee<<96"))?;
        floor(
            current,
            abs_delta
                .checked_mul(fee)
                .ok_or(ParityError::Math("abs*fee"))?,
            den,
        )
    } else {
        let den = two_fee()
            .checked_mul(current)
            .ok_or(ParityError::Math("2fee*P"))?;
        floor(
            q96(),
            abs_delta
                .checked_mul(fee)
                .ok_or(ParityError::Math("abs*fee1"))?,
            den,
        )
    }
}

fn calc_inc_liq(
    abs_delta: U256,
    liquidity: U256,
    current: U256,
    next: U256,
    exact_in: bool,
    is_token0: bool,
) -> Result<U256, ParityError> {
    if is_token0 {
        let tmp1 = floor(liquidity, q96(), current)?;
        let tmp2 = if exact_in {
            tmp1.checked_add(abs_delta)
        } else {
            tmp1.checked_sub(abs_delta)
        }
        .ok_or(ParityError::Math("kyber tmp2-0"))?;
        let tmp3 = floor(next, tmp2, q96())?;
        Ok(if tmp3 > liquidity {
            tmp3.checked_sub(liquidity)
                .ok_or(ParityError::Math("dL0"))?
        } else {
            U256::ZERO
        })
    } else {
        let tmp1 = floor(liquidity, current, q96())?;
        let tmp2 = if exact_in {
            tmp1.checked_add(abs_delta)
        } else {
            tmp1.checked_sub(abs_delta)
        }
        .ok_or(ParityError::Math("kyber tmp2-1"))?;
        let tmp3 = floor(tmp2, q96(), next)?;
        Ok(if tmp3 > liquidity {
            tmp3.checked_sub(liquidity)
                .ok_or(ParityError::Math("dL1"))?
        } else {
            U256::ZERO
        })
    }
}

fn calc_final_price(
    abs_delta: U256,
    liquidity: U256,
    delta_l: U256,
    current: U256,
    exact_in: bool,
    is_token0: bool,
) -> Result<U256, ParityError> {
    if is_token0 {
        let tmp = floor(abs_delta, current, q96())?;
        if exact_in {
            ceil(
                liquidity
                    .checked_add(delta_l)
                    .ok_or(ParityError::Math("L+dL"))?,
                current,
                liquidity
                    .checked_add(tmp)
                    .ok_or(ParityError::Math("L+tmp"))?,
            )
        } else {
            floor(
                liquidity
                    .checked_add(delta_l)
                    .ok_or(ParityError::Math("L+dL"))?,
                current,
                liquidity
                    .checked_sub(tmp)
                    .ok_or(ParityError::Math("L-tmp"))?,
            )
        }
    } else {
        let tmp = floor(abs_delta, q96(), current)?;
        if exact_in {
            floor(
                liquidity
                    .checked_add(tmp)
                    .ok_or(ParityError::Math("L+tmp1"))?,
                current,
                liquidity
                    .checked_add(delta_l)
                    .ok_or(ParityError::Math("L+dL1"))?,
            )
        } else {
            ceil(
                liquidity
                    .checked_sub(tmp)
                    .ok_or(ParityError::Math("L-tmp1"))?,
                current,
                liquidity
                    .checked_add(delta_l)
                    .ok_or(ParityError::Math("L+dL1b"))?,
            )
        }
    }
}

fn calc_returned(
    liquidity: U256,
    current: U256,
    next: U256,
    delta_l: U256,
    exact_in: bool,
    is_token0: bool,
) -> Result<I256, ParityError> {
    if is_token0 {
        if exact_in {
            let a = to_i256(ceil(delta_l, next, q96())?)?;
            let b = rev_i256(floor(liquidity, current.abs_diff(next), q96())?)?;
            a.checked_add(b).ok_or(ParityError::Math("ret0 in"))
        } else {
            let a = to_i256(ceil(delta_l, next, q96())?)?;
            let b = to_i256(ceil(liquidity, next.abs_diff(current), q96())?)?;
            a.checked_add(b).ok_or(ParityError::Math("ret0 out"))
        }
    } else {
        let a = to_i256(ceil(
            liquidity
                .checked_add(delta_l)
                .ok_or(ParityError::Math("L+dL ret"))?,
            q96(),
            next,
        )?)?;
        let b = rev_i256(floor(liquidity, q96(), current)?)?;
        a.checked_add(b).ok_or(ParityError::Math("ret1"))
    }
}
