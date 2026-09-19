//! `BonusCurve` — liquidation economics as a function, never a number
//! (GUIDE 01 §4). Adapters **populate** it from live protocol parameters;
//! the engine (GUIDE 12) **evaluates** it. Collapsing it to a scalar in an
//! adapter throws away the Aave V4 edge (GUIDE 04 §4).

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
    HealthLinear {
        bonus_at_threshold: Ray,
        hf_for_max: Ray,
        max_bonus: Ray,
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
                let rise = mul_div(span.raw(), deficit.raw(), width.raw(), Rounding::Down)?;
                bonus_at_threshold
                    .checked_add(Ray::from_raw(rise))
                    .map(Some)
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
        }
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
