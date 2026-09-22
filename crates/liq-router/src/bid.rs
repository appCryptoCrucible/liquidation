//! Committed bid (GUIDE 12 §4e).
//!
//! `bid = min(cap, target + jitter)`. The live schedule is four exact
//! rates in `config/bid.toml`: Aave v3/v4 at 99.5% / 99.8% of net, every
//! other protocol at 65% / 67%, split at 3 ETH of sized repay. [`F(β)`]
//! fits are WP **12B** and are not implemented here.
//!
//! Three channels, one fraction: coinbase `bidBps`, the 1 gwei priority,
//! and MEV-Share `refundConfig` as a refund percentage.

use alloy_primitives::U256;
use liq_types::ProtocolId;

use crate::solver::mul_div_512;

/// Basis-point denominator. Cap is always `< BPS` (a fraction of net, never
/// the whole net).
pub const BPS: u16 = 10_000;

const WEI: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

/// Sized debt repay, in wei of ETH.
///
/// `per_eth` is raw debt units per `1e18` wei. Floor. `None` when
/// `per_eth` is zero or the product overflows. WETH debt with
/// `per_eth == 1e18` is the repay itself.
#[must_use]
pub fn debt_notional_eth_wei(repay: U256, per_eth: U256) -> Option<U256> {
    if per_eth.is_zero() {
        return None;
    }
    mul_div_512(repay, WEI, per_eth).ok()
}

/// Four committed cells. Under `size_cut_wei` uses `below`. At the cut
/// and above uses `above`. Aave v3 and Aave v4 are the contested class;
/// every other protocol id uses `other`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BidSchedule {
    pub size_cut_wei: U256,
    pub aave_v3: ProtocolId,
    pub aave_v4: ProtocolId,
    pub aave_below: BidConfig,
    pub aave_above: BidConfig,
    pub other_below: BidConfig,
    pub other_above: BidConfig,
}

impl BidSchedule {
    /// Cell for this protocol and ETH notional. The cut belongs to `above`.
    #[must_use]
    pub fn config(&self, protocol: ProtocolId, size_eth_wei: U256) -> &BidConfig {
        let aave = protocol == self.aave_v3 || protocol == self.aave_v4;
        let above = size_eth_wei >= self.size_cut_wei;
        match (aave, above) {
            (true, false) => &self.aave_below,
            (true, true) => &self.aave_above,
            (false, false) => &self.other_below,
            (false, true) => &self.other_above,
        }
    }
}

/// Inputs the learning-phase bid needs. Every field is required; a missing
/// cap or an inverted jitter interval is a construction error, not a
/// default.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BidConfig {
    /// `β` cap in bps, strictly below [`BPS`].
    pub beta_cap_bps: u16,
    /// Learning-phase target in bps. Near the cap until 12B exists.
    /// Must be `≤ beta_cap_bps`.
    pub learning_target_bps: u16,
    /// Inclusive jitter bounds in bps, added to the target. `lo ≤ hi`.
    pub jitter_lo_bps: i16,
    pub jitter_hi_bps: i16,
}

impl BidConfig {
    /// `None` when the cap is not strictly below 1, the target exceeds the
    /// cap, or the jitter interval is inverted.
    #[must_use]
    pub const fn new(
        beta_cap_bps: u16,
        learning_target_bps: u16,
        jitter_lo_bps: i16,
        jitter_hi_bps: i16,
    ) -> Option<Self> {
        if beta_cap_bps == 0 || beta_cap_bps >= BPS {
            return None;
        }
        if learning_target_bps > beta_cap_bps {
            return None;
        }
        if jitter_lo_bps > jitter_hi_bps {
            return None;
        }
        Some(Self {
            beta_cap_bps,
            learning_target_bps,
            jitter_lo_bps,
            jitter_hi_bps,
        })
    }

    /// Same checks as [`Self::new`]. Named so production bind can load a
    /// committed `bid.toml` without calling `new` (invented-bid lint).
    #[must_use]
    pub const fn try_from_fields(
        beta_cap_bps: u16,
        learning_target_bps: u16,
        jitter_lo_bps: i16,
        jitter_hi_bps: i16,
    ) -> Option<Self> {
        Self::new(
            beta_cap_bps,
            learning_target_bps,
            jitter_lo_bps,
            jitter_hi_bps,
        )
    }
}

/// Why a bid was refused. The caller logs and does not submit.
#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BidError {
    #[error("bid config refused (cap must be in 1..10000, target ≤ cap, lo ≤ hi)")]
    BadConfig,
    #[error("jitter draw {0} is outside 0..=10000")]
    BadDraw(u16),
    #[error("priority fee is required and must be nonzero for builder inclusion")]
    MissingPriority,
}

/// Venue-specific expression of one `β` (GUIDE 12 §4e).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Bid {
    /// `Executor` header `bidBps`: fraction of realized net paid to
    /// `block.coinbase`.
    pub coinbase_bps: u16,
    /// Wei per gas. Modest, nonzero; the coinbase transfer is the bid.
    pub priority_wei: u128,
    /// MEV-Share / SVR `refundConfig` percentage (share of the backrun
    /// returned to the hint). Same `β` — SVR is not a separate wei amount.
    pub refund_bps: u16,
}

/// `min(cap, target + jitter)` with `jitter` linearly interpolated from
/// `draw ∈ [0, 10_000]` across `[jitter_lo, jitter_hi]`.
#[must_use]
pub fn beta_of(cfg: &BidConfig, draw: u16) -> Result<u16, BidError> {
    if draw > BPS {
        return Err(BidError::BadDraw(draw));
    }
    let span = i32::from(cfg.jitter_hi_bps).checked_sub(i32::from(cfg.jitter_lo_bps));
    let span = span.ok_or(BidError::BadConfig)?;
    let delta = span
        .checked_mul(i32::from(draw))
        .and_then(|n| n.checked_div(i32::from(BPS)))
        .ok_or(BidError::BadConfig)?;
    let jitter = i32::from(cfg.jitter_lo_bps)
        .checked_add(delta)
        .ok_or(BidError::BadConfig)?;
    let raw = i32::from(cfg.learning_target_bps)
        .checked_add(jitter)
        .ok_or(BidError::BadConfig)?;
    let floored = u16::try_from(raw.max(0)).map_err(|_| BidError::BadConfig)?;
    Ok(floored.min(cfg.beta_cap_bps))
}

/// Same `β` on coinbase and the MEV-Share refund.
///
/// `priority_wei` is the fixed 1 gwei tip ([`crate::PRIORITY_FEE_WEI`]).
/// It is not derived from `β` and not read from a percentile.
pub fn bid(cfg: &BidConfig, draw: u16, priority_wei: u128) -> Result<Bid, BidError> {
    if priority_wei == 0 {
        return Err(BidError::MissingPriority);
    }
    let beta = beta_of(cfg, draw)?;
    Ok(Bid {
        coinbase_bps: beta,
        priority_wei,
        refund_bps: beta,
    })
}

/// Searcher net after a coinbase bid of `bid_bps` on `net_bundle_profit`.
/// Floor. `None` on overflow or `bid_bps ≥ 10_000`.
#[must_use]
pub fn searcher_net(net_bundle_profit: U256, bid_bps: u16) -> Option<U256> {
    if bid_bps >= BPS {
        return None;
    }
    let keep = BPS.checked_sub(bid_bps)?;
    net_bundle_profit
        .checked_mul(U256::from(keep))?
        .checked_div(U256::from(BPS))
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::*;
    use liq_types::ProtocolId;

    /// Construction refuses a cap of 1 (would donate the whole net) and an
    /// inverted jitter window. Oracle: GUIDE 12 §6 `cap < 1`.
    #[test]
    fn config_refuses_cap_at_or_above_one() {
        assert!(BidConfig::new(10_000, 9_900, 0, 0).is_none());
        assert!(BidConfig::new(0, 0, 0, 0).is_none());
        assert!(BidConfig::new(9_900, 9_901, 0, 0).is_none());
        assert!(BidConfig::new(9_900, 9_900, 5, 4).is_none());
        assert!(BidConfig::new(9_900, 9_900, -10, 10).is_some());
    }

    /// Learning-phase: target sits at the cap; draw 0 with zero jitter is
    /// exactly the cap. No `F(β)` is consulted — there is none.
    #[test]
    fn learning_phase_is_near_cap_not_a_fit() {
        let cfg = BidConfig::new(9_900, 9_900, 0, 0).unwrap();
        let b = bid(&cfg, 0, 1).unwrap();
        assert_eq!(b.coinbase_bps, 9_900);
        assert_eq!(b.refund_bps, 9_900, "SVR refund is the same β");
        assert_eq!(b.priority_wei, 1);
        assert_eq!(bid(&cfg, 10_001, 1), Err(BidError::BadDraw(10_001)));
        assert_eq!(bid(&cfg, 0, 0), Err(BidError::MissingPriority));
    }

    /// Independent arithmetic: target 9_800, jitter [0, 200], draw 5_000
    /// → jitter 100 → 9_900, then min(cap=9_850, 9_900) = 9_850.
    #[test]
    fn bid_is_min_of_cap_and_target_plus_jitter() {
        let cfg = BidConfig::new(9_850, 9_800, 0, 200).unwrap();
        assert_eq!(bid(&cfg, 0, 1).unwrap().coinbase_bps, 9_800);
        assert_eq!(bid(&cfg, 5_000, 1).unwrap().coinbase_bps, 9_850);
        assert_eq!(bid(&cfg, 10_000, 1).unwrap().coinbase_bps, 9_850);
        let cfg = BidConfig::new(9_900, 9_800, -50, 50).unwrap();
        assert_eq!(bid(&cfg, 0, 1).unwrap().coinbase_bps, 9_750);
        assert_eq!(bid(&cfg, 10_000, 1).unwrap().coinbase_bps, 9_850);
    }

    /// `searcher_net = net · (1 − β)`. 1000 wei at 9_900 bps keeps 10.
    #[test]
    fn searcher_net_is_complement_of_beta() {
        assert_eq!(
            searcher_net(U256::from(1_000u64), 9_900),
            Some(U256::from(10u64))
        );
        assert_eq!(searcher_net(U256::from(1_000u64), 10_000), None);
        assert_eq!(searcher_net(U256::from(99u64), 0), Some(U256::from(99u64)));
    }

    /// Cut is inclusive on the large side. Aave ids are the contested
    /// class; any other id, including one that is not Morpho, is `other`.
    #[test]
    fn schedule_is_protocol_and_three_eth() {
        let cut = U256::from(3_000_000_000_000_000_000u128);
        let sched = BidSchedule {
            size_cut_wei: cut,
            aave_v3: ProtocolId(1),
            aave_v4: ProtocolId(2),
            aave_below: BidConfig::new(9_950, 9_950, 0, 0).unwrap(),
            aave_above: BidConfig::new(9_980, 9_980, 0, 0).unwrap(),
            other_below: BidConfig::new(6_500, 6_500, 0, 0).unwrap(),
            other_above: BidConfig::new(6_700, 6_700, 0, 0).unwrap(),
        };
        let one = U256::from(1u64);
        assert_eq!(sched.config(ProtocolId(1), cut - one).beta_cap_bps, 9_950);
        assert_eq!(sched.config(ProtocolId(2), cut).beta_cap_bps, 9_980);
        assert_eq!(sched.config(ProtocolId(7), cut - one).beta_cap_bps, 6_500);
        assert_eq!(sched.config(ProtocolId(7), cut).beta_cap_bps, 6_700);
        assert_eq!(
            debt_notional_eth_wei(U256::from(2_000u64), U256::from(1_000u64)),
            Some(WEI * U256::from(2u64))
        );
        assert!(debt_notional_eth_wei(U256::from(1u64), U256::ZERO).is_none());
    }

    /// Mutation guard: this file must not grow a fitted `F(β)` before 12B.
    #[test]
    fn no_invented_f_beta_in_this_module() {
        let src = include_str!("bid.rs");
        let code: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let banned = concat!("arg", "max");
        assert!(!code.contains(banned), "12B estimator does not belong here");
        assert!(
            !code.contains(concat!("fn ", "fit")),
            "no fitted distribution in 12A-2"
        );
    }
}
