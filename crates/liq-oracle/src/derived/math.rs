//! Protocol formulas. Rounding is named at every lossy step (GUIDE 00 §2).

use alloy_primitives::U256;
use liq_types::fixed::{mul_div, FixedError, Rounding, RAY, WAD};

/// Aave Capo `PERCENTAGE_FACTOR` (1e4).
pub const PERCENTAGE_FACTOR: U256 = U256::from_limbs([10_000, 0, 0, 0]);
/// Solidity `365 days`.
pub const SECS_PER_YEAR: U256 = U256::from_limbs([31_536_000, 0, 0, 0]);

/// Floor sqrt, OpenZeppelin `Math.sqrt` (Newton, round down).
#[must_use]
pub fn sqrt_down(a: U256) -> U256 {
    if a.is_zero() || a == U256::from(1u64) {
        return a;
    }
    let bits = a.bit_len();
    let half = bits.saturating_add(1).saturating_div(2).min(255);
    let mut x = U256::from(1u64).checked_shl(half).unwrap_or(U256::MAX);
    if x.is_zero() {
        x = U256::from(1u64);
    }
    loop {
        let q = a.checked_div(x).unwrap_or(U256::ZERO);
        let sum = match x.checked_add(q) {
            Some(s) => s,
            None => return x,
        };
        let x1 = sum.checked_div(U256::from(2u64)).unwrap_or(sum);
        if x1 >= x {
            let other = a.checked_div(x).unwrap_or(U256::ZERO);
            return if x < other { x } else { other };
        }
        x = x1;
    }
}

/// LST/LRT / wstETH: `underlying * rate / WAD` (Lido `stEthPerToken`, Solidity `/`).
pub fn mul_wad_down(underlying: U256, rate_wad: U256) -> Result<U256, FixedError> {
    if rate_wad.is_zero() {
        return Err(FixedError::DivisionByZero);
    }
    mul_div(underlying, rate_wad, WAD, Rounding::Down)
}

/// sDAI: `dai * chi / RAY` (`Pot.chi`, Solidity `/`).
pub fn mul_ray_down(underlying: U256, chi: U256) -> Result<U256, FixedError> {
    if chi.is_zero() {
        return Err(FixedError::DivisionByZero);
    }
    mul_div(underlying, chi, RAY, Rounding::Down)
}

/// `shares * pooled / total_shares` (Lido `getPooledEthByShares`).
pub fn pooled_by_shares(
    pooled: U256,
    shares: U256,
    share_amount: U256,
) -> Result<U256, FixedError> {
    if shares.is_zero() {
        return Err(FixedError::DivisionByZero);
    }
    mul_div(share_amount, pooled, shares, Rounding::Down)
}

/// Aave Capo max ratio: `snapshot * (1e4 + growthPct * elapsed / 365 days) / 1e4`.
pub fn capo_max_ratio(
    snapshot_ratio: U256,
    elapsed_secs: U256,
    max_yearly_ratio_growth_percent: U256,
) -> Result<U256, FixedError> {
    let growth = mul_div(
        max_yearly_ratio_growth_percent,
        elapsed_secs,
        SECS_PER_YEAR,
        Rounding::Down,
    )?;
    let factor = PERCENTAGE_FACTOR
        .checked_add(growth)
        .ok_or(FixedError::Overflow)?;
    mul_div(snapshot_ratio, factor, PERCENTAGE_FACTOR, Rounding::Down)
}

/// `min(live, cap)` then `underlying * capped / scale`.
pub fn capped_lst(
    underlying: U256,
    live_ratio: U256,
    snapshot_ratio: U256,
    elapsed_secs: U256,
    max_yearly_ratio_growth_percent: U256,
    scale: U256,
) -> Result<U256, FixedError> {
    if scale.is_zero() {
        return Err(FixedError::DivisionByZero);
    }
    let max_ratio = capo_max_ratio(
        snapshot_ratio,
        elapsed_secs,
        max_yearly_ratio_growth_percent,
    )?;
    let capped = if live_ratio < max_ratio {
        live_ratio
    } else {
        max_ratio
    };
    if capped.is_zero() {
        return Err(FixedError::DivisionByZero);
    }
    mul_div(underlying, capped, scale, Rounding::Down)
}

/// Cross-rate rounding order (protocol adapter, not arbitrary).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CrossOrder {
    /// `(x * y) / 10^d` — Aave `CLSynchronicityPriceAdapterPegToBase`.
    MulThenDivDown,
    /// `(x / 10^d) * y` — different floor; must not be used unless the contract does this.
    DivThenMulDown,
}

pub fn cross_rate(x: U256, y: U256, denom: U256, order: CrossOrder) -> Result<U256, FixedError> {
    if denom.is_zero() {
        return Err(FixedError::DivisionByZero);
    }
    match order {
        CrossOrder::MulThenDivDown => mul_div(x, y, denom, Rounding::Down),
        CrossOrder::DivThenMulDown => {
            let q = mul_div(x, U256::from(1u64), denom, Rounding::Down)?;
            q.checked_mul(y).ok_or(FixedError::Overflow)
        }
    }
}

/// Aave UniV2-style fair LP: `2 * sqrt(r0*r1) * sqrt(p0*p1) / supply`, all floor.
pub fn lp_fair(r0: U256, r1: U256, p0: U256, p1: U256, supply: U256) -> Result<U256, FixedError> {
    if supply.is_zero() {
        return Err(FixedError::DivisionByZero);
    }
    let k = r0.checked_mul(r1).ok_or(FixedError::Overflow)?;
    let px = p0.checked_mul(p1).ok_or(FixedError::Overflow)?;
    let sqrt_k = sqrt_down(k);
    let sqrt_p = sqrt_down(px);
    let twice = sqrt_k
        .checked_mul(U256::from(2u64))
        .ok_or(FixedError::Overflow)?;
    mul_div(twice, sqrt_p, supply, Rounding::Down)
}

#[cfg(test)]
mod tests {
    use super::{
        capo_max_ratio, capped_lst, cross_rate, lp_fair, mul_ray_down, mul_wad_down,
        pooled_by_shares, sqrt_down, CrossOrder, PERCENTAGE_FACTOR, SECS_PER_YEAR,
    };
    use alloy_primitives::U256;
    use liq_types::fixed::{RAY, WAD};

    fn u(n: u128) -> U256 {
        U256::from(n)
    }

    /// Oracle: Lido `getPooledEthByShares(1e18)` = `1e18 * pooled / shares` floor.
    #[test]
    fn lst_formula_steth_per_token() {
        let pooled = u(1_150_000) * u(1_000_000_000_000_000_000);
        let shares = u(1_000_000) * u(1_000_000_000_000_000_000);
        let rate = pooled_by_shares(pooled, shares, WAD).unwrap();
        assert_eq!(rate, u(1_150_000_000_000_000_000));
        let steth = RAY;
        let wst = mul_wad_down(steth, rate).unwrap();
        assert_eq!(wst, u(1_150_000_000_000_000_000_000_000_000));
    }

    /// Oracle: Maker `rmul` identity `dai * chi / RAY`.
    #[test]
    fn yield_formula_sdai_chi() {
        let dai = RAY;
        let chi = u(1_234_000_000_000_000_000_000_000_000);
        assert_eq!(mul_ray_down(dai, chi).unwrap(), chi);
        let dai2 = u(2) * RAY;
        assert_eq!(mul_ray_down(dai2, chi).unwrap(), u(2) * chi);
    }

    /// Oracle: Solidity `/` discards remainder (tie-break = floor).
    #[test]
    fn lst_rounding_floor_remainder() {
        let under = u(1_000_000_000_000_000_000_000_000_001);
        let rate = WAD + U256::from(1u64);
        let got = mul_wad_down(under, rate).unwrap();
        let exact_floor = (under * rate) / WAD;
        assert_eq!(got, exact_floor);
        assert!(got < under + under / WAD + U256::from(2u64));
    }

    /// Oracle: Aave Capo `snapshot * (1e4 + g * t / 365 days) / 1e4`.
    #[test]
    fn cap_formula_year_of_growth() {
        let snap = WAD;
        let g = u(1_000); // 10% = 1000 / 10000
        let max = capo_max_ratio(snap, SECS_PER_YEAR, g).unwrap();
        assert_eq!(max, WAD * u(11_000) / PERCENTAGE_FACTOR);
        let under = RAY;
        let live = WAD * u(2);
        let capped = capped_lst(under, live, snap, SECS_PER_YEAR, g, WAD).unwrap();
        assert_eq!(capped, RAY * u(11_000) / PERCENTAGE_FACTOR);
        let live_ok = WAD * u(10_500) / PERCENTAGE_FACTOR;
        let uncapped = capped_lst(under, live_ok, snap, SECS_PER_YEAR, g, WAD).unwrap();
        assert_eq!(uncapped, RAY * u(10_500) / PERCENTAGE_FACTOR);
    }

    /// Oracle: Capo inner `g * elapsed / 365 days` floors before applying to snapshot.
    #[test]
    fn cap_rounding_elapsed_not_year() {
        let snap = WAD;
        let g = u(1_000);
        let elapsed = u(1);
        let max = capo_max_ratio(snap, elapsed, g).unwrap();
        assert_eq!(max, snap, "1s of 10%/year floors to zero growth");
    }

    /// Oracle: `(x*y)/100` vs `(x/100)*y` — 150,150,2dec.
    #[test]
    fn cross_rounding_order() {
        let x = u(150);
        let y = u(150);
        let d = u(100);
        assert_eq!(
            cross_rate(x, y, d, CrossOrder::MulThenDivDown).unwrap(),
            u(225)
        );
        assert_eq!(
            cross_rate(x, y, d, CrossOrder::DivThenMulDown).unwrap(),
            u(150)
        );
    }

    /// Oracle: `2*sqrt(k)*sqrt(p0*p1)/s` with non-square k floors.
    #[test]
    fn lp_formula_and_sqrt_floor() {
        assert_eq!(sqrt_down(u(0)), u(0));
        assert_eq!(sqrt_down(u(1)), u(1));
        assert_eq!(sqrt_down(u(4)), u(2));
        assert_eq!(sqrt_down(u(10)), u(3));
        let r0 = u(100);
        let r1 = u(400);
        let p0 = RAY;
        let p1 = RAY;
        let supply = u(200);
        let fv = lp_fair(r0, r1, p0, p1, supply).unwrap();
        let expect = u(2) * u(200) * RAY / u(200);
        assert_eq!(fv, expect);
        let fv2 = lp_fair(u(10), u(10), RAY, RAY, u(3)).unwrap();
        let sk = sqrt_down(u(100));
        let sp = sqrt_down(RAY * RAY);
        assert_eq!(fv2, u(2) * sk * sp / u(3));
    }
}
