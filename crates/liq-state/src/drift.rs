//! Drift detector (GUIDE 02 §8): stratified sampling, `health()` against an
//! executed `health_probe()` on a published snapshot, and a per-protocol
//! integer EWMA of the mismatch for telemetry.
//!
//! It measures; it does not act. [`DriftDetector::tick`] reports every
//! compared position's mismatch, and re-probes the positions the caller
//! names (a resync's second look). What a mismatch triggers — resync,
//! quarantine, a protocol-wide resync — is the caller's policy
//! (`liq-bot`'s drift thread). Halting a protocol stops the liquidations it
//! gets right along with any it gets wrong, so nothing here halts.
//!
//! Lives here because it needs raw snapshot access.
//!
//! `tick` is called **off** the hot thread (GUIDE 16: drift sampler is
//! background). It never fsyncs and never takes a lock the writer holds.
//!
//! A comparison is only fair on the same inputs the chain uses: local health
//! takes the canonical vector with the position's market overlay laid on
//! ([`DriftTick::patch`] — the protocol's own prices, as the engine prices
//! it), and a position whose local state is known to be pending a chain read
//! (`Blocked { Unread }`), has no debt, or cannot be evaluated or probed is
//! skipped, not scored.

use std::collections::HashMap;

use liq_protocol::{BlockReason, Health, HealthState, ProbeCall, Protocol, Timestamp};
use liq_types::fixed::RAY;
use liq_types::{AssetId, Band, MarketId, PositionId, PriceVector, ProtocolId, Ray};

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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    pub sampled: u32,
    pub compared: u32,
    pub skipped: u32,
    /// `(position, |local − probe| bps)` for every compared position.
    pub results: Vec<(PositionId, u64)>,
}

/// Stratified sampler + per-protocol EWMA.
pub struct DriftDetector {
    /// `(band, take)` — over-sample Hot/Warm by passing a larger `take`.
    strata: [(Band, u32); 4],
    mismatch: HashMap<ProtocolId, Ewma>,
}

/// The protocol's own prices for one market: `(asset, price)` to lay over
/// the canonical vector.
pub type MarketPatch<'a> = dyn Fn(ProtocolId, MarketId) -> Vec<(AssetId, Ray)> + 'a;

/// Inputs for one off-thread [`DriftDetector::tick`]. Bundled so the
/// method stays under clippy's argument cap; every field is read.
pub struct DriftTick<'a> {
    pub snap: &'a StoreSnapshot,
    pub bands: &'a [Band],
    pub timestamp: Timestamp,
    pub px: &'a liq_types::PriceVector,
    /// The protocol's own prices for one market, laid over `px` for its
    /// positions (the engine's per-market overlay). `None`: `px` alone.
    pub patch: Option<&'a MarketPatch<'a>>,
    pub protocol: &'a dyn Protocol,
}

impl DriftDetector {
    #[must_use]
    pub fn new(strata: [(Band, u32); 4]) -> Self {
        Self {
            strata,
            mismatch: HashMap::new(),
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

    /// Record one observed `|local − probe|` in bps into the protocol's EWMA.
    pub fn observe(&mut self, protocol: ProtocolId, delta_bps: u64) {
        self.mismatch.entry(protocol).or_default().push(delta_bps);
    }

    /// Off-thread tick: the stratified sample plus `recheck` (positions the
    /// caller wants looked at again), each `health()` vs its executed
    /// `health_probe()`. `exec_probe` is the eth_call; this crate does not
    /// own an RPC client.
    pub fn tick(
        &mut self,
        ctx: DriftTick<'_>,
        recheck: &[PositionId],
        exec_probe: impl Fn(&ProbeCall) -> Result<Ray, DriftError>,
    ) -> Result<TickReport, DriftError> {
        let mut ids = self.sample_ids(ctx.snap, ctx.bands, ctx.protocol.id())?;
        for id in recheck {
            if !ids.contains(id) && (id.0 as usize) < ctx.snap.len() {
                ids.push(*id);
            }
        }
        let mut report = TickReport {
            sampled: u32::try_from(ids.len()).unwrap_or(u32::MAX),
            ..TickReport::default()
        };
        let mut px: PriceVector = ctx.px.clone();
        for id in ids {
            let pos = ctx.snap.position(id, ctx.timestamp)?;
            if pos.key.protocol != ctx.protocol.id() {
                report.skipped = report.skipped.saturating_add(1);
                continue;
            }
            px.clone_from(ctx.px);
            if let Some(patch) = ctx.patch {
                for (asset, price) in patch(pos.key.protocol, pos.key.market) {
                    if let Some(cell) = px.0.get_mut(usize::from(asset.0)) {
                        cell.price = price;
                    }
                }
            }
            // Health the engine could not compute is not drift; neither is a
            // position whose state awaits its chain read, or one with no debt.
            let local = match ctx.protocol.health(pos, &px) {
                Ok(Health {
                    state:
                        HealthState::Blocked {
                            reason: BlockReason::Unread,
                        },
                    ..
                }) => None,
                Ok(h) if h.hf == Health::NO_DEBT_HF => None,
                Ok(h) => Some(h.hf),
                Err(_) => None,
            };
            let Some(local) = local else {
                report.skipped = report.skipped.saturating_add(1);
                continue;
            };
            let Ok(probe) = ctx.protocol.health_probe(pos) else {
                report.skipped = report.skipped.saturating_add(1);
                continue;
            };
            // A probe that fails to execute is I/O, not a mismatch.
            let Ok(remote) = exec_probe(&probe) else {
                report.skipped = report.skipped.saturating_add(1);
                continue;
            };
            let delta = delta_bps(local, remote)?;
            self.observe(ctx.protocol.id(), delta);
            report.results.push((id, delta));
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
    use liq_types::{AssetId, Band, MarketId, PositionKey, ProtocolId, Ray};

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

    /// Property: after a healthy period, a sustained mismatch above a level
    /// eventually lifts the EWMA past it; one below it converges to itself.
    /// Fixed point of a constant series is the constant (the window-scale
    /// accumulator keeps the remainder).
    #[test]
    fn ewma_tracks_sustained_mismatch() {
        const T: u64 = 10;
        const STEPS: u32 = 128;
        for s in [1u64, 5, 11, 16, 100, 1000] {
            let mut e = Ewma::default();
            e.push(s);
            assert_eq!(e.value(), s, "first sample is the value ({s})");
            for _ in 0..STEPS {
                e.push(s);
                assert_eq!(e.value(), s, "fixed point of sustained {s} bps is {s}");
            }
        }
        let strata = [
            (Band::Hot, 2),
            (Band::Warm, 2),
            (Band::Cool, 1),
            (Band::Cold, 1),
        ];
        let mut d = DriftDetector::new(strata);
        let id = ProtocolId(1);
        for _ in 0..STEPS {
            d.observe(id, 0);
        }
        assert_eq!(d.ewma(id).unwrap().value(), 0);
        let mut crossed = false;
        for _ in 0..STEPS {
            d.observe(id, 11);
            crossed |= d.ewma(id).unwrap().value() > T;
        }
        assert!(crossed, "sustained 11 bps after zeros exceeds {T}");
        let mut below = DriftDetector::new(strata);
        let id = ProtocolId(3);
        for _ in 0..STEPS {
            below.observe(id, 0);
        }
        for _ in 0..STEPS {
            below.observe(id, 9);
        }
        assert_eq!(
            below.ewma(id).unwrap().value(),
            9,
            "converged, not pinned at 0"
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
        let d = DriftDetector::new([
            (Band::Hot, 3),
            (Band::Warm, 0),
            (Band::Cool, 0),
            (Band::Cold, 1),
        ]);
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
        let d2 = DriftDetector::new([
            (Band::Hot, 1),
            (Band::Warm, 0),
            (Band::Cool, 0),
            (Band::Cold, 1),
        ]);
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

    /// A protocol whose health is its one asset's price and whose probe
    /// answers 2.0 — enough to see which prices `tick` compares.
    struct PriceIsHealth;

    impl liq_types::LogSubscriber for PriceIsHealth {
        fn subscriptions(&self) -> Vec<liq_types::LogFilter> {
            Vec::new()
        }
    }

    const UNREAD: Address = Address::repeat_byte(9);

    fn two() -> Ray {
        Ray::from_raw(liq_types::fixed::RAY * alloy_primitives::U256::from(2u8))
    }

    fn decode_two(_: &[u8]) -> liq_protocol::Result<Ray> {
        Ok(two())
    }

    impl liq_protocol::Protocol for PriceIsHealth {
        fn id(&self) -> ProtocolId {
            ProtocolId(7)
        }
        fn apply_log(
            &self,
            _: &mut dyn StateWriter,
            _: &liq_protocol::DecodedLog<'_>,
        ) -> liq_protocol::Result<liq_protocol::DirtySet> {
            Ok(liq_protocol::DirtySet::None)
        }
        fn backfill(
            &self,
            _: &mut dyn StateWriter,
            _: &dyn liq_protocol::Archive,
            _: liq_protocol::BlockNum,
        ) -> liq_protocol::Result<()> {
            Ok(())
        }
        fn health(
            &self,
            pos: liq_protocol::PositionRef<'_>,
            px: &liq_types::PriceVector,
        ) -> liq_protocol::Result<liq_protocol::Health> {
            let state = if pos.key.user == UNREAD {
                liq_protocol::HealthState::Blocked {
                    reason: liq_protocol::BlockReason::Unread,
                }
            } else {
                liq_protocol::HealthState::Healthy
            };
            Ok(liq_protocol::Health {
                hf: px.0[0].price,
                debt_value: liq_types::Wad::from_raw(alloy_primitives::U256::from(1u8)),
                collateral_value: liq_types::Wad::from_raw(alloy_primitives::U256::from(1u8)),
                price_sensitivity: liq_protocol::AssetMask::EMPTY,
                state,
            })
        }
        fn liquidation_price(
            &self,
            _: liq_protocol::PositionRef<'_>,
            _: &liq_types::PriceVector,
            _: AssetId,
        ) -> liq_protocol::Result<Option<liq_types::Price>> {
            Ok(None)
        }
        fn time_to_cross(
            &self,
            _: liq_protocol::PositionRef<'_>,
            _: &liq_types::PriceVector,
        ) -> liq_protocol::Result<Option<liq_protocol::Timestamp>> {
            Ok(None)
        }
        fn quote(
            &self,
            _: liq_protocol::PositionRef<'_>,
            _: &liq_types::PriceVector,
        ) -> liq_protocol::Result<Option<liq_protocol::Quote>> {
            Ok(None)
        }
        fn encode(
            &self,
            _: &liq_protocol::Quote,
            _: liq_protocol::LegChoice,
            _: &liq_protocol::FlashRoute,
            _: Address,
        ) -> liq_protocol::Result<liq_protocol::LiquidationPlan> {
            Err(liq_protocol::ProtocolError::Internal)
        }
        fn health_probe(
            &self,
            _: liq_protocol::PositionRef<'_>,
        ) -> liq_protocol::Result<liq_protocol::ProbeCall> {
            Ok(liq_protocol::ProbeCall {
                to: Address::ZERO,
                data: alloy_primitives::Bytes::new(),
                decode: decode_two,
            })
        }
    }

    /// Oracle: health compared to the probe is health at the prices the
    /// protocol uses — the canonical vector with the market's overlay laid
    /// on (2.0, equal to the probe: 0 bps); without it, canonical 1.0 vs 2.0
    /// is 10_000 bps. A position pending its chain read (`Blocked { Unread }`)
    /// is skipped; a `recheck` id outside the sample is compared. Negative:
    /// nothing is reported for a position that was not compared.
    #[test]
    fn tick_reports_mismatch_at_overlay_prices_and_rechecks() {
        let mut st = StateStore::new(StoreConfig {
            base: 1,
            positions: 4,
            markets: 1,
            undo: UndoCapacity {
                ops: 32,
                extras: 4,
                rows: 4,
            },
        });
        let m = MarketId(0);
        st.push_market(m, row()).unwrap();
        for user in [Address::repeat_byte(1), UNREAD, Address::repeat_byte(2)] {
            st.intern(&PositionKey {
                protocol: ProtocolId(7),
                market: m,
                user,
            })
            .unwrap();
        }
        let snap = st.snapshot();
        // Position 2 is Cold, outside the Hot-only sample.
        let bands = [Band::Hot, Band::Hot, Band::Cold];
        let px = liq_types::PriceVector(vec![liq_types::Price {
            asset: AssetId(0),
            price: Ray::ONE,
            source: liq_types::SourceKind::Canonical,
            block: 1,
            ts: 1,
        }]);
        let overlay = |_: ProtocolId, _: MarketId| vec![(AssetId(0), two())];
        let strata = [
            (Band::Hot, 2),
            (Band::Warm, 0),
            (Band::Cool, 0),
            (Band::Cold, 0),
        ];
        let p = PriceIsHealth;
        let exec =
            |c: &liq_protocol::ProbeCall| (c.decode)(&[]).map_err(super::DriftError::Protocol);
        fn tick<'a>(
            snap: &'a crate::snapshot::StoreSnapshot,
            bands: &'a [Band],
            px: &'a liq_types::PriceVector,
            patch: Option<&'a super::MarketPatch<'a>>,
            protocol: &'a PriceIsHealth,
        ) -> super::DriftTick<'a> {
            super::DriftTick {
                snap,
                bands,
                timestamp: 1,
                px,
                patch,
                protocol,
            }
        }

        let mut d = DriftDetector::new(strata);
        let r = d
            .tick(tick(&snap, &bands, &px, Some(&overlay), &p), &[], exec)
            .unwrap();
        assert_eq!((r.compared, r.skipped), (1, 1), "unread position skipped");
        assert_eq!(r.results, vec![(liq_types::PositionId(0), 0)]);

        let r = d
            .tick(tick(&snap, &bands, &px, None, &p), &[], exec)
            .unwrap();
        assert_eq!(r.results, vec![(liq_types::PositionId(0), 10_000)]);

        let r = d
            .tick(
                tick(&snap, &bands, &px, Some(&overlay), &p),
                &[liq_types::PositionId(2)],
                exec,
            )
            .unwrap();
        assert_eq!(
            r.results,
            vec![(liq_types::PositionId(0), 0), (liq_types::PositionId(2), 0)],
            "recheck compared outside the sample"
        );
    }
}
