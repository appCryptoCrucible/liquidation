//! `BonusCurve` — liquidation economics as a function, never a number
//! (GUIDE 01 §4). Adapters **populate** it from live protocol parameters;
//! the engine (GUIDE 12) **evaluates** it. Collapsing it to a scalar in an
//! adapter throws away the Aave V4 edge (GUIDE 04 §4).

use alloy_primitives::U256;
use liq_types::fixed::{mul_div, FixedError, Rounding};
use liq_types::{Ray, RayU128};

use crate::Timestamp;

/// How the bonus evolves as the position deteriorates.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BonusCurve {
    /// Fixed bonus (Aave V3 per reserve, Morpho LIF, Compound discount,
    /// Liquity): fire immediately.
    Static { bonus: Ray },
    /// Aave V4: linear in `hf` between `(1.0, bonus_at_threshold)` and
    /// `(hf_for_max, max_bonus)`, saturating at `max_bonus` below
    /// `hf_for_max`. The optimal firing point is strictly later than
    /// `hf = 1.0`; GUIDE 12 integrates this against competitor arrival.
    ///
    /// Precondition: `hf_for_max < 1.0`. The adapter checks it once when it
    /// reads the spoke's liquidation config — [`BonusCurve::bonus_at_hf`]
    /// cannot, because a `hf_for_max >= 1.0` curve is indistinguishable there
    /// from one already saturated, and it would answer `bonus_at_threshold`
    /// for every `hf` instead of failing.
    ///
    /// `quantum` is the smallest bonus step the protocol's integer
    /// parameters can express — the interpolation floors in **that** unit,
    /// exactly as the chain does. Aave V4 interpolates in bps
    /// (`LiquidationLogic.calculateLiquidationBonus`: `mulDivDown` over
    /// `uint256` bps), so `quantum = 1 bp = 1e23 RAY`; a bonus computed in
    /// RAY and floored there would be up to one bp too generous and the
    /// close factor built on it would overshoot the target. `Ray(1)` means
    /// "continuous". Zero is refused (`DivisionByZero`); a `span` that is
    /// not a whole number of quanta is refused (`Inexact`).
    HealthLinear {
        bonus_at_threshold: Ray,
        hf_for_max: Ray,
        max_bonus: Ray,
        quantum: Ray,
    },
    /// Euler V2: the liquidator's discount is the reciprocal of health,
    /// floored at the vault's `MAX_LIQUIDATION_DISCOUNT`.
    ///
    /// `EVault/Liquidation.sol` (pin `bfb325a6`) prices the seize at a
    /// *discount factor* `df = max(hf, min_df)` and pays the liquidator
    /// `collateral / df`, so the bonus is
    ///
    /// ```text
    /// bonus(hf) = 1 / max(hf, min_df) - 1
    /// ```
    ///
    /// which is zero at `hf = 1`, rises as health falls, and saturates at
    /// `1 / min_df - 1` once `hf <= min_df`. It is a hyperbola, not a line:
    /// [`Self::HealthLinear`] between the same endpoints understates the
    /// bonus everywhere in between, because the true curve is convex.
    ///
    /// This is exactly the health-dependent edge the engine exists to find,
    /// and the reason this module warns against collapsing a curve to a
    /// scalar — quoting Euler as [`Self::Static`] pinned one point and
    /// claimed it held at every health.
    Reciprocal {
        /// `MAX_LIQUIDATION_DISCOUNT` as a discount factor: the smallest
        /// `df` the vault will use, so the largest bonus it will pay.
        /// Must be in `(0, 1]`.
        min_df: Ray,
    },
    /// Dutch auction: the price offered to the liquidator falls with time
    /// since `start`. Evaluation is auction machinery, deferred (D14).
    TimeDescending {
        start: Timestamp,
        curve: AuctionCurve,
    },
}

/// Price-decay shape of a Dutch-auction liquidation. Variants and parameters
/// mirror Sky's `abaci.sol` (`LinearDecrease.tau`,
/// `StairstepExponentialDecrease.{step,cut}`, `ExponentialDecrease.cut`), the
/// mainnet shapes GUIDE 15 Tier 3 would model. Data only until D14 lifts.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AuctionCurve {
    /// Price falls linearly to zero over `tau` seconds.
    LinearDecrease { tau: u64 },
    /// Price multiplied by `cut` (RAY, `< 1`) every `step` seconds.
    StairstepExponentialDecrease { step: u64, cut: RayU128 },
    /// Price multiplied by `cut` (RAY, `< 1`) every second.
    ExponentialDecrease { cut: RayU128 },
}

impl BonusCurve {
    /// Bonus the protocol pays if liquidation happens at health `hf`.
    ///
    /// * `Static` → the bonus.
    /// * `HealthLinear` → `bonus_at_threshold` for `hf >= 1.0`, `max_bonus`
    ///   for `hf <= hf_for_max`, linear interpolation between, rounded down.
    ///   Endpoints are exact under any rounding (acceptance: at `hf_for_max`
    ///   equals `max_bonus`; at `1.0` equals `bonus_at_threshold`).
    /// * `TimeDescending` → `None`: not a function of health.
    ///
    /// `Err` only for a malformed curve (`max_bonus < bonus_at_threshold`).
    #[inline]
    pub fn bonus_at_hf(&self, hf: Ray) -> Result<Option<Ray>, FixedError> {
        match *self {
            Self::Static { bonus } => Ok(Some(bonus)),
            Self::HealthLinear {
                bonus_at_threshold,
                hf_for_max,
                max_bonus,
                quantum,
            } => {
                if hf >= Ray::ONE {
                    return Ok(Some(bonus_at_threshold));
                }
                if hf <= hf_for_max {
                    return Ok(Some(max_bonus));
                }
                // hf_for_max < hf < 1.0 here, so both differences are > 0.
                let span = max_bonus.checked_sub(bonus_at_threshold)?;
                let deficit = Ray::ONE.checked_sub(hf)?;
                let width = Ray::ONE.checked_sub(hf_for_max)?;
                // `mul_div` refuses a zero quantum (`DivisionByZero`).
                let span_q = mul_div(span.raw(), U256::ONE, quantum.raw(), Rounding::Down)?;
                if span_q.checked_mul(quantum.raw()) != Some(span.raw()) {
                    return Err(FixedError::Inexact);
                }
                // floor(span_q · deficit / width) in quanta, then back to RAY:
                // the chain's `mulDivDown` over integer bps, step for step.
                let rise_q = mul_div(span_q, deficit.raw(), width.raw(), Rounding::Down)?;
                let rise = rise_q
                    .checked_mul(quantum.raw())
                    .ok_or(FixedError::Overflow)?;
                bonus_at_threshold
                    .checked_add(Ray::from_raw(rise))
                    .map(Some)
            }
            Self::Reciprocal { min_df } => {
                if min_df.raw().is_zero() {
                    return Err(FixedError::DivisionByZero);
                }
                // Above the threshold the vault pays nothing: `df` is capped
                // at 1, so `1/1 - 1 = 0`.
                if hf >= Ray::ONE {
                    return Ok(Some(Ray::ZERO));
                }
                let df = if hf <= min_df { min_df } else { hf };
                // `1/df - 1` in RAY, floored: the bot must never quote a
                // bonus above what the vault will actually pay.
                let inv = mul_div(Ray::ONE.raw(), Ray::ONE.raw(), df.raw(), Rounding::Down)?;
                inv.checked_sub(Ray::ONE.raw())
                    .map(|b| Some(Ray::from_raw(b)))
                    .ok_or(FixedError::Underflow)
            }
            Self::TimeDescending { .. } => Ok(None),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::{AuctionCurve, BonusCurve};
    use alloy_primitives::{uint, U256};
    use liq_types::fixed::{FixedError, RAY};
    use liq_types::{Ray, RayU128};

    /// bps → Ray, computed independently of the code under test.
    fn bps(n: u64) -> Ray {
        Ray::from_raw(RAY.checked_mul(U256::from(n)).unwrap() / U256::from(10_000u64))
    }

    fn v4_like() -> BonusCurve {
        BonusCurve::HealthLinear {
            bonus_at_threshold: bps(100), // 1 %
            hf_for_max: bps(9_500),       // 0.95
            max_bonus: bps(1_000),        // 10 %
            quantum: bps(1),
        }
    }

    /// Oracle: `LiquidationLogic.calculateLiquidationBonus` by hand in bps.
    /// span 900 bp, deficit 0.0123, width 0.05 → 900·0.0123/0.05 = 221.4 →
    /// floor **in bps** = 221 → 3.21 %. A continuous floor would give
    /// 3.214 %, 0.4 bp more than the chain pays. Negative: `quantum` zero
    /// and a span not on the grid are refused.
    #[test]
    fn health_linear_floors_in_the_protocol_quantum() {
        let c = v4_like();
        let hf = Ray::from_raw(RAY - RAY * U256::from(123u64) / U256::from(10_000u64));
        assert_eq!(c.bonus_at_hf(hf).unwrap(), Some(bps(321)));
        let continuous = BonusCurve::HealthLinear {
            bonus_at_threshold: bps(100),
            hf_for_max: bps(9_500),
            max_bonus: bps(1_000),
            quantum: Ray::from_raw(U256::ONE),
        };
        let b = continuous.bonus_at_hf(hf).unwrap().unwrap();
        assert!(b > bps(321) && b < bps(322));
        let zero = BonusCurve::HealthLinear {
            bonus_at_threshold: bps(100),
            hf_for_max: bps(9_500),
            max_bonus: bps(1_000),
            quantum: Ray::ZERO,
        };
        assert_eq!(zero.bonus_at_hf(hf), Err(FixedError::DivisionByZero));
        let off_grid = BonusCurve::HealthLinear {
            bonus_at_threshold: bps(100),
            hf_for_max: bps(9_500),
            max_bonus: Ray::from_raw(bps(1_000).raw() + U256::ONE),
            quantum: bps(1),
        };
        assert_eq!(off_grid.bonus_at_hf(hf), Err(FixedError::Inexact));
    }

    /// GUIDE 01 acceptance: at `hf_for_max` equals `max_bonus`; at `1.0`
    /// equals `bonus_at_threshold`. Oracle: the enum's definition.
    #[test]
    fn health_linear_endpoints_exact() {
        let c = v4_like();
        assert_eq!(c.bonus_at_hf(Ray::ONE).unwrap(), Some(bps(100)));
        assert_eq!(c.bonus_at_hf(bps(9_500)).unwrap(), Some(bps(1_000)));
    }

    /// Oracle: linear interpolation by hand — at the midpoint hf = 0.975 the
    /// bonus is the midpoint 5.5 %; saturation below `hf_for_max`; clamp
    /// above 1.0. Negative: `Static` ignores hf entirely.
    #[test]
    fn health_linear_interior_saturation_and_clamp() {
        let c = v4_like();
        assert_eq!(c.bonus_at_hf(bps(9_750)).unwrap(), Some(bps(550)));
        assert_eq!(c.bonus_at_hf(bps(5_000)).unwrap(), Some(bps(1_000)));
        assert_eq!(c.bonus_at_hf(bps(12_000)).unwrap(), Some(bps(100)));
        let s = BonusCurve::Static { bonus: bps(500) };
        assert_eq!(s.bonus_at_hf(bps(1)).unwrap(), Some(bps(500)));
        assert_eq!(s.bonus_at_hf(bps(20_000)).unwrap(), Some(bps(500)));
    }

    /// Oracle: monotonicity — a lower hf never pays less. Sampled every bp
    /// across the whole interpolated interval.
    #[test]
    fn health_linear_is_monotone_non_increasing_in_hf() {
        let c = v4_like();
        let mut prev = c.bonus_at_hf(bps(9_500)).unwrap().unwrap();
        for hf_bps in 9_501..=10_000u64 {
            let b = c.bonus_at_hf(bps(hf_bps)).unwrap().unwrap();
            assert!(b <= prev, "bonus rose as hf rose at {hf_bps} bps");
            prev = b;
        }
    }

    /// Negative: a malformed curve (`max < threshold`) is an error, not a
    /// wrapped value. Oracle: `checked_sub` semantics.
    #[test]
    fn malformed_curve_errors() {
        let c = BonusCurve::HealthLinear {
            bonus_at_threshold: bps(1_000),
            hf_for_max: bps(9_500),
            max_bonus: bps(100),
            quantum: bps(1),
        };
        assert_eq!(c.bonus_at_hf(bps(9_750)), Err(FixedError::Underflow));
    }

    /// Oracle: RUST-CONVENTIONS §10 — `None` means "not applicable": a
    /// time-driven curve is not a function of hf.
    #[test]
    fn time_descending_is_not_health_driven() {
        let c = BonusCurve::TimeDescending {
            start: 0,
            curve: AuctionCurve::StairstepExponentialDecrease {
                step: 90,
                cut: RayU128::from_raw(uint!(990_000_000_000_000_000_000_000_000_U256).to()),
            },
        };
        assert_eq!(c.bonus_at_hf(bps(9_000)).unwrap(), None);
    }
}
