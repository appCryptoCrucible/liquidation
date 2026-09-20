//! Thread-local aggregator simulator (GUIDE 06 §6, §7b).
//!
//! Lives on the fusion thread; mutated through `&mut` only. Median of live
//! CEX mids vs last on-chain answer. [`PendingUpdate`] is pre-warm only —
//! never written into [`crate::canonical::CanonicalBook`].

use crate::cex::CexTick;
use crate::{OracleError, Result};
use alloy_primitives::U256;
use liq_protocol::FeedId;
use liq_types::fixed::{mul_div, Ray, Rounding};
use liq_types::{AssetId, Confidence};
use smallvec::SmallVec;
use std::time::{Duration, Instant};

/// 05B DEFERRED SEAM.
///
/// GUIDE 06 §6: `estimate_eta` is fitted from archive `transmit` history
/// against the simulated deviation curve — **not a constant**. WP 05B is
/// not built (D60). Until the parquet exists this is `now + 12s` (one
/// mainnet slot). **Not a calibrated delay.** The ≥90% within-one-block /
/// 30-day replay acceptance folds into drift/shadow. Do not invent a fit.
pub const ETA_SEAM_SECS: u64 = 12;

/// Predicted confidence when heartbeat is due but deviation is below
/// threshold (basis points of certainty, 0..=10_000; never [`Confidence::CERTAIN`]).
pub const HEARTBEAT_CONFIDENCE_BPS: u16 = 2_500;

/// Pre-warm signal for GUIDE 08. Never a fire trigger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingUpdate {
    pub feed: FeedId,
    pub asset: AssetId,
    pub predicted: Ray,
    pub eta: Instant,
    pub confidence: Confidence,
}

/// Per-feed simulator. `quotes[i]` is the latest mid for `sources[i]`.
pub struct AggregatorSim {
    pub feed: FeedId,
    pub asset: AssetId,
    deviation_threshold_bps: u16,
    heartbeat_secs: u32,
    last_onchain: Option<(Ray, u64)>,
    quotes: SmallVec<[Option<Ray>; 8]>,
}

impl AggregatorSim {
    pub fn new(
        feed: FeedId,
        asset: AssetId,
        deviation_threshold_bps: u16,
        heartbeat_secs: u32,
        n_sources: usize,
    ) -> Result<Self> {
        if n_sources > 8 {
            return Err(OracleError::TooManyCexSources);
        }
        let mut quotes = SmallVec::new();
        for _ in 0..n_sources {
            quotes.push(None);
        }
        Ok(Self {
            feed,
            asset,
            deviation_threshold_bps,
            heartbeat_secs,
            last_onchain: None,
            quotes,
        })
    }

    pub fn set_onchain(&mut self, px: Ray, ts: u64) {
        self.last_onchain = Some((px, ts));
    }

    pub fn on_quote(&mut self, slot: u8, tick: &CexTick) {
        let Some(q) = self.quotes.get_mut(usize::from(slot)) else {
            tracing::error!(slot, "cex quote slot out of range");
            return;
        };
        *q = Some(tick.mid);
    }

    /// Median of currently live CEX mids. `None` if no venue has quoted.
    pub fn median(&self) -> Option<Ray> {
        let mut v: SmallVec<[Ray; 8]> = SmallVec::new();
        for q in &self.quotes {
            if let Some(px) = *q {
                v.push(px);
            }
        }
        if v.is_empty() {
            return None;
        }
        v.sort_unstable();
        let i = v.len().checked_div(2)?;
        v.get(i).copied()
    }

    /// Deviation + heartbeat gate. `now_unix` is receive time, not guessed.
    pub fn on_tick(&mut self, now: Instant, now_unix: u64) -> Option<PendingUpdate> {
        let px = self.median()?;
        let Some((last, last_ts)) = self.last_onchain else {
            tracing::debug!(feed = self.feed.0, "aggsim: no on-chain last; skip");
            return None;
        };
        let dev = match deviation_bps(px, last) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(error = %e, "aggsim deviation");
                return None;
            }
        };
        let hb_due = now_unix.saturating_sub(last_ts) >= u64::from(self.heartbeat_secs);
        if dev < self.deviation_threshold_bps && !hb_due {
            return None;
        }
        let heartbeat_only = dev < self.deviation_threshold_bps;
        let eta = match estimate_eta(now) {
            Some(t) => t,
            None => {
                tracing::error!("aggsim: Instant overflow on ETA seam");
                return None;
            }
        };
        Some(PendingUpdate {
            feed: self.feed,
            asset: self.asset,
            predicted: px,
            eta,
            confidence: predicted_confidence(dev, self.deviation_threshold_bps, heartbeat_only),
        })
    }
}

/// `|px − last| · 10_000 / last`, floor. `last == 0` fails.
pub fn deviation_bps(px: Ray, last: Ray) -> Result<u16> {
    if last.raw().is_zero() {
        return Err(OracleError::NonPositiveAnswer);
    }
    let diff = if px >= last {
        px.checked_sub(last)
    } else {
        last.checked_sub(px)
    }
    .map_err(|_| OracleError::NonPositiveAnswer)?;
    let bps = mul_div(
        diff.raw(),
        U256::from(10_000u16),
        last.raw(),
        Rounding::Down,
    )
    .map_err(|_| OracleError::NonPositiveAnswer)?;
    match u16::try_from(bps) {
        Ok(v) => Ok(v),
        Err(_) => Ok(u16::MAX),
    }
}

/// Scale: 0..=10_000 bps of certainty (WP 00C / 06A-2). Predicted never 10_000.
/// At `dev == threshold`: 5000. As `dev → ∞`: 9999. Heartbeat-only: 2500.
pub fn predicted_confidence(dev_bps: u16, threshold_bps: u16, heartbeat_only: bool) -> Confidence {
    if heartbeat_only {
        return Confidence(HEARTBEAT_CONFIDENCE_BPS);
    }
    let d = u32::from(dev_bps);
    let t = u32::from(threshold_bps.max(1));
    let num = d.saturating_mul(10_000);
    let den = d.saturating_add(t);
    let c = num.checked_div(den).unwrap_or(0);
    let c = c.min(9_999);
    match u16::try_from(c) {
        Ok(v) => Confidence(v),
        Err(_) => Confidence(9_999),
    }
}

/// 05B seam — see [`ETA_SEAM_SECS`]. `None` on Instant overflow (logged by caller).
#[must_use]
pub fn estimate_eta(now: Instant) -> Option<Instant> {
    now.checked_add(Duration::from_secs(ETA_SEAM_SECS))
}

#[cfg(test)]
mod tests {
    use super::{
        deviation_bps, estimate_eta, predicted_confidence, AggregatorSim, ETA_SEAM_SECS,
        HEARTBEAT_CONFIDENCE_BPS,
    };
    use crate::cex::{decimal_to_ray, CexTick, CexVenue};
    use liq_protocol::FeedId;
    use liq_types::{AssetId, Confidence};
    use std::time::{Duration, Instant};

    fn tick(mid: &str) -> CexTick {
        let px = decimal_to_ray(mid).unwrap();
        CexTick {
            venue: CexVenue::Binance,
            symbol: "ETHUSDT".into(),
            bid: px,
            ask: px,
            mid: px,
        }
    }

    /// Oracle: median of three CEX mids, upper-middle for even (matches 06D).
    #[test]
    fn median_three_venues() {
        let mut s = AggregatorSim::new(FeedId(0), AssetId(0), 50, 3600, 3).unwrap();
        s.on_quote(0, &tick("100"));
        s.on_quote(1, &tick("102"));
        s.on_quote(2, &tick("101"));
        assert_eq!(s.median().unwrap(), decimal_to_ray("101").unwrap());
    }

    /// Oracle: deviation 50 bps on 0.5% move.
    #[test]
    fn deviation_fifty_bps() {
        let last = decimal_to_ray("100").unwrap();
        let px = decimal_to_ray("100.5").unwrap();
        assert_eq!(deviation_bps(px, last).unwrap(), 50);
        assert_eq!(deviation_bps(last, last).unwrap(), 0);
    }

    /// Oracle: fires on threshold; Predicted is not CERTAIN; no emit below.
    #[test]
    fn pending_only_when_dev_or_heartbeat() {
        let mut s = AggregatorSim::new(FeedId(1), AssetId(2), 50, 3600, 1).unwrap();
        let now = Instant::now();
        s.set_onchain(decimal_to_ray("100").unwrap(), 1_000);
        s.on_quote(0, &tick("100.4"));
        assert!(s.on_tick(now, 1_000).is_none());
        s.on_quote(0, &tick("100.5"));
        let p = s.on_tick(now, 1_000).unwrap();
        assert_eq!(p.predicted, decimal_to_ray("100.5").unwrap());
        assert_ne!(p.confidence, Confidence::CERTAIN);
        assert_eq!(p.confidence, predicted_confidence(50, 50, false));
        s.on_quote(0, &tick("100.1"));
        let hb = s.on_tick(now, 1_000 + 3600).unwrap();
        assert_eq!(hb.confidence, Confidence(HEARTBEAT_CONFIDENCE_BPS));
        assert_eq!(
            estimate_eta(now)
                .unwrap()
                .checked_duration_since(now)
                .unwrap(),
            Duration::from_secs(ETA_SEAM_SECS)
        );
    }

    /// Oracle: no last on-chain → no fabricated deviation.
    #[test]
    fn no_onchain_no_pending() {
        let mut s = AggregatorSim::new(FeedId(0), AssetId(0), 50, 3600, 1).unwrap();
        s.on_quote(0, &tick("100"));
        assert!(s.on_tick(Instant::now(), 1).is_none());
    }
}
