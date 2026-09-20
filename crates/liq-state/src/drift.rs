//! Drift detector scaffold (GUIDE 02 §8).
//!
//! Lives here because it needs raw snapshot access. Calibration (thresholds,
//! sample rate) is WP 05C; this WP wires stratified sampling, per-protocol
//! integer EWMA, `health_probe()` issuance against a published snapshot, and
//! [`HaltReason::DriftMismatch`] into [`HaltSink`].
//!
//! `tick` is called **off** the hot thread (GUIDE 16: drift sampler is
//! background). It never fsyncs and never takes a lock the writer holds.

use std::collections::HashMap;

use liq_protocol::{ProbeCall, Protocol, Timestamp};
use liq_types::fixed::RAY;
use liq_types::{Band, HaltReason, HaltScope, HaltSink, PositionId, ProtocolId, Ray};

use crate::error::StateError;
use crate::snapshot::StoreSnapshot;

/// Integer EWMA window. 05C may replace this; the scaffold needs a defined
/// smoothing so a single spike does not equal a halt.
const EWMA_W: u64 = 16;

/// One protocol's mismatch series, in basis points of HF (relative to RAY).
///
/// `acc` is held at window scale (`value() = acc / EWMA_W`) so a sustained
/// sample of `s` has fixed point `s`. Dividing every step (`(v·15+s)/16`)
/// discards the remainder and pins at 0 for any sustained mismatch under
/// `EWMA_W` bps — the GUIDE 04 / 04C operating regime.
#[derive(Copy, Clone, Debug, Default)]
pub struct Ewma {
    acc: u64,
    count: u64,
}

impl Ewma {
    fn push(&mut self, sample: u64) {
        if self.count == 0 {
            self.acc = sample.saturating_mul(EWMA_W);
            self.count = 1;
            return;
        }
        let decayed = self.acc.checked_div(EWMA_W).unwrap_or(0);
        self.acc = self.acc.saturating_sub(decayed).saturating_add(sample);
        self.count = self.count.saturating_add(1);
    }

    #[must_use]
    pub fn value(&self) -> u64 {
        self.acc.checked_div(EWMA_W).unwrap_or(0)
    }

    /// Observations in this series. 05C reads this for sample-confidence
    /// (whether the EWMA has enough points to trust a halt).
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }
}

/// Failure of one drift tick. Probe I/O errors surface here; they are not
/// converted into a halt (a transport failure is not a health mismatch).
#[derive(Debug, thiserror::Error)]
pub enum DriftError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Protocol(#[from] liq_protocol::ProtocolError),
    #[error("band slice length {bands} != snapshot positions {positions}")]
    BandLen { bands: usize, positions: usize },
    #[error("probe execution failed")]
    Probe,
    #[error("health-factor bps conversion overflowed")]
    BpsOverflow,
}

/// What one [`DriftDetector::tick`] did. Telemetry for 09A; 05C reads the
/// EWMA, not this.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    pub sampled: u32,
    pub compared: u32,
    pub skipped: u32,
}

/// Stratified sampler + per-protocol EWMA. `max_mismatch_bps` is a
/// placeholder 05C will calibrate; passing it in is the mechanism.
pub struct DriftDetector {
    /// `(band, take)` — over-sample Hot/Warm by passing a larger `take`.
    strata: [(Band, u32); 4],
    mismatch: HashMap<ProtocolId, Ewma>,
    max_mismatch_bps: u32,
}

/// Inputs for one off-thread [`DriftDetector::tick`]. Bundled so the
/// method stays under clippy's argument cap; every field is read.
pub struct DriftTick<'a> {
    pub snap: &'a StoreSnapshot,
    pub bands: &'a [Band],
    pub timestamp: Timestamp,
    pub px: &'a liq_types::PriceVector,
    pub protocol: &'a dyn Protocol,
    pub sink: &'a dyn HaltSink,
}

impl DriftDetector {
    #[must_use]
    pub fn new(strata: [(Band, u32); 4], max_mismatch_bps: u32) -> Self {
        Self {
            strata,
            mismatch: HashMap::new(),
            max_mismatch_bps,
        }
    }

    #[must_use]
    pub fn ewma(&self, id: ProtocolId) -> Option<Ewma> {
        self.mismatch.get(&id).copied()
    }

    /// Stratified ids for `protocol` from `bands` (index = [`PositionId`]).
    /// Evenly spaced inside each stratum; no RNG (reproducible; 05C may
    /// replace).
    pub fn sample_ids(
        &self,
        snap: &StoreSnapshot,
        bands: &[Band],
        protocol: ProtocolId,
    ) -> Result<Vec<PositionId>, DriftError> {
        if bands.len() != snap.len() {
            return Err(DriftError::BandLen {
                bands: bands.len(),
                positions: snap.len(),
            });
        }
        let mut buckets: [Vec<PositionId>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        for (i, key) in snap.keys.iter().enumerate() {
            if key.key.protocol != protocol {
                continue;
            }
            let band = *bands.get(i).ok_or(StateError::Inconsistent)?;
            let id = u32::try_from(i)
                .map(PositionId)
                .map_err(|_| StateError::Inconsistent)?;
            for (slot, (want, _)) in self.strata.iter().enumerate() {
                if *want == band {
                    if let Some(b) = buckets.get_mut(slot) {
                        b.push(id);
                    }
                    break;
                }
            }
        }
        let mut out = Vec::new();
        for (slot, (_, take)) in self.strata.iter().enumerate() {
            let ids = buckets.get(slot).map_or(&[][..], Vec::as_slice);
            out.extend(take_even(ids, *take as usize));
        }
        Ok(out)
    }

    /// Record one observed `|local − probe|` in bps. Emits
    /// [`HaltReason::DriftMismatch`] on [`HaltScope::Protocol`] when the
    /// EWMA exceeds `max_mismatch_bps`.
    pub fn observe(&mut self, protocol: ProtocolId, delta_bps: u64, sink: &dyn HaltSink) {
        let ewma = self.mismatch.entry(protocol).or_default();
        ewma.push(delta_bps);
        if ewma.value() > u64::from(self.max_mismatch_bps) {
            sink.halt(HaltScope::Protocol(protocol), HaltReason::DriftMismatch);
        }
    }

    /// Off-thread tick: sample, `health()` vs executed `health_probe()`,
    /// EWMA, maybe halt. `exec_probe` is the eth_call (node/17A); this crate
    /// does not own an RPC client.
    pub fn tick(
        &mut self,
        ctx: DriftTick<'_>,
        exec_probe: impl Fn(&ProbeCall) -> Result<Ray, DriftError>,
    ) -> Result<TickReport, DriftError> {
        let ids = self.sample_ids(ctx.snap, ctx.bands, ctx.protocol.id())?;
        let mut report = TickReport {
            sampled: u32::try_from(ids.len()).unwrap_or(u32::MAX),
            compared: 0,
            skipped: 0,
        };
        for id in ids {
            let pos = ctx.snap.position(id, ctx.timestamp)?;
            if pos.key.protocol != ctx.protocol.id() {
                report.skipped = report.skipped.saturating_add(1);
                continue;
            }
            let local = match ctx.protocol.health(pos, ctx.px) {
                Ok(h) => h.hf,
                Err(liq_protocol::ProtocolError::MissingPrice(_)) => {
                    report.skipped = report.skipped.saturating_add(1);
                    continue;
                }
                Err(e) => return Err(DriftError::Protocol(e)),
            };
            let probe = match ctx.protocol.health_probe(pos) {
                Ok(c) => c,
                Err(liq_protocol::ProtocolError::ProbeUnavailable) => {
                    report.skipped = report.skipped.saturating_add(1);
                    continue;
                }
                Err(e) => return Err(DriftError::Protocol(e)),
            };
            let remote = exec_probe(&probe)?;
            let delta = delta_bps(local, remote)?;
            self.observe(ctx.protocol.id(), delta, ctx.sink);
            report.compared = report.compared.saturating_add(1);
        }
        Ok(report)
    }
}

fn take_even(ids: &[PositionId], k: usize) -> Vec<PositionId> {
    if k == 0 || ids.is_empty() {
        return Vec::new();
    }
    let n = ids.len();
    let take = k.min(n);
    let mut out = Vec::with_capacity(take);
    for i in 0..take {
        let idx = i.saturating_mul(n).checked_div(take).unwrap_or(0);
        if let Some(&id) = ids.get(idx) {
            out.push(id);
        }
    }
    out
}

/// `|local − remote|` as bps of 1.0 RAY. Oracle: definition
/// `floor(|a−b| · 10_000 / 10^27)`.
pub fn delta_bps(local: Ray, remote: Ray) -> Result<u64, DriftError> {
    let a = local.raw();
    let b = remote.raw();
    let diff = if a > b {
        a.saturating_sub(b)
    } else {
        b.saturating_sub(a)
    };
    let num = diff
        .checked_mul(alloy_primitives::U256::from(10_000u64))
        .ok_or(DriftError::BpsOverflow)?;
    let bps = num.checked_div(RAY).ok_or(DriftError::BpsOverflow)?;
    u64::try_from(bps).map_err(|_| DriftError::BpsOverflow)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::{delta_bps, DriftDetector, Ewma};
    use crate::store::{StateStore, StoreConfig};
    use crate::undo::UndoCapacity;
    use alloy_primitives::Address;
    use liq_protocol::{MarketRow, StateWriter};
    use liq_types::{
        AssetId, Band, HaltReason, HaltScope, HaltSink, MarketId, PositionKey, ProtocolId, Ray,
    };
    use std::sync::Mutex;

    struct Rec(Mutex<Vec<(HaltScope, HaltReason)>>);

    impl HaltSink for Rec {
        fn halt(&self, scope: HaltScope, reason: HaltReason) {
            self.0.lock().unwrap().push((scope, reason));
        }
    }

    fn row() -> MarketRow {
        MarketRow::blank(AssetId(0), 18)
    }

    /// Oracle: definition of floor-bps against RAY. Negative: equal HFs → 0.
    #[test]
    fn delta_bps_is_abs_diff_over_ray() {
        let one = Ray::ONE;
        assert_eq!(delta_bps(one, one).unwrap(), 0);
        // 1 bp of 1.0 = RAY / 10_000.
        let plus = Ray::from_raw(
            liq_types::fixed::RAY
                .checked_add(
                    liq_types::fixed::RAY
                        .checked_div(alloy_primitives::U256::from(10_000u64))
                        .unwrap(),
                )
                .unwrap(),
        );
        assert_eq!(delta_bps(one, plus).unwrap(), 1);
    }

    /// Property: after a healthy period, a sustained mismatch above the
    /// threshold eventually halts; a sustained mismatch below it does not.
    /// Convergence table: fixed point of a constant series is the constant
    /// (definition of the scaled accumulator), not `sustained - 15`.
    /// Boundary: EWMA exactly on `max_mismatch_bps` must not halt (`>`, not `>=`).
    #[test]
    fn ewma_sustained_above_threshold_halts_below_does_not() {
        const T: u32 = 10;
        const STEPS: u32 = 128;
        let strata = [
            (Band::Hot, 2),
            (Band::Warm, 2),
            (Band::Cool, 1),
            (Band::Cold, 1),
        ];

        for s in [1u64, 5, 11, 16, 100, 1000] {
            let mut e = Ewma::default();
            e.push(s);
            assert_eq!(e.value(), s, "first sample is the value ({s})");
            for _ in 0..STEPS {
                e.push(s);
                assert_eq!(
                    e.value(),
                    s,
                    "fixed point of sustained {s} bps is {s}, not {s}-15"
                );
            }
        }

        let mut above = DriftDetector::new(strata, T);
        let rec_above = Rec(Mutex::new(Vec::new()));
        let id_above = ProtocolId(1);
        for _ in 0..STEPS {
            above.observe(id_above, 0, &rec_above);
        }
        assert!(
            rec_above.0.lock().unwrap().is_empty(),
            "healthy period must not halt"
        );
        assert_eq!(above.ewma(id_above).unwrap().value(), 0);

        let mut halted = false;
        for _ in 0..STEPS {
            above.observe(id_above, 11, &rec_above);
            if !rec_above.0.lock().unwrap().is_empty() {
                halted = true;
                break;
            }
        }
        assert!(
            halted,
            "sustained 11 bps after healthy zeros must eventually exceed T={T}"
        );
        assert!(above.ewma(id_above).unwrap().value() > u64::from(T));
        let hits = rec_above.0.lock().unwrap();
        assert_eq!(
            hits[0],
            (HaltScope::Protocol(id_above), HaltReason::DriftMismatch)
        );
        drop(hits);

        let mut below = DriftDetector::new(strata, T);
        let rec_below = Rec(Mutex::new(Vec::new()));
        let id_below = ProtocolId(3);
        for _ in 0..STEPS {
            below.observe(id_below, 0, &rec_below);
        }
        for _ in 0..STEPS {
            below.observe(id_below, 9, &rec_below);
        }
        assert!(
            rec_below.0.lock().unwrap().is_empty(),
            "sustained 9 bps < T={T} must not halt"
        );
        assert_eq!(
            below.ewma(id_below).unwrap().value(),
            9,
            "converged to the sustained sample, not pinned at 0"
        );

        let edge = ProtocolId(2);
        let rec_edge = Rec(Mutex::new(Vec::new()));
        let mut on_threshold = DriftDetector::new(strata, T);
        on_threshold.observe(edge, 10, &rec_edge);
        assert_eq!(on_threshold.ewma(edge).unwrap().value(), 10);
        assert!(
            rec_edge.0.lock().unwrap().is_empty(),
            "EWMA == max_mismatch_bps must not halt"
        );
    }

    /// Oracle: the band slice we pass. Hot is over-sampled vs Cold.
    #[test]
    fn stratified_sample_overweights_hot() {
        let mut st = StateStore::new(StoreConfig {
            base: 1,
            positions: 16,
            markets: 1,
            undo: UndoCapacity {
                ops: 32,
                extras: 4,
                rows: 4,
            },
        });
        let m = MarketId(0);
        st.push_market(m, row()).unwrap();
        for u in 0..8u8 {
            st.intern(&PositionKey {
                protocol: ProtocolId(7),
                market: m,
                user: Address::repeat_byte(u),
            })
            .unwrap();
        }
        let snap = st.snapshot();
        let bands = [
            Band::Hot,
            Band::Hot,
            Band::Hot,
            Band::Hot,
            Band::Cold,
            Band::Cold,
            Band::Cold,
            Band::Cold,
        ];
        let d = DriftDetector::new(
            [
                (Band::Hot, 3),
                (Band::Warm, 0),
                (Band::Cool, 0),
                (Band::Cold, 1),
            ],
            100,
        );
        let ids = d.sample_ids(&snap, &bands, ProtocolId(7)).unwrap();
        assert_eq!(ids.len(), 4, "3 hot + 1 cold");
        let mut hot = 0;
        let mut cold = 0;
        for id in ids {
            match bands[id.0 as usize] {
                Band::Hot => hot += 1,
                Band::Cold => cold += 1,
                _ => panic!("sampled a band we did not ask for"),
            }
        }
        assert_eq!(hot, 3);
        assert_eq!(cold, 1);
        let d2 = DriftDetector::new(
            [
                (Band::Hot, 1),
                (Band::Warm, 0),
                (Band::Cool, 0),
                (Band::Cold, 1),
            ],
            100,
        );
        assert!(d2.sample_ids(&snap, &bands[..3], ProtocolId(7)).is_err());
    }

    /// First sample equals itself (no decay of a fabricated zero).
    #[test]
    fn ewma_first_sample_is_the_value() {
        let mut e = Ewma::default();
        e.push(40);
        assert_eq!(e.value(), 40);
        assert_eq!(e.count(), 1);
    }
}
