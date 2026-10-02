//! The drift detector's thread (GUIDE 02 §8): local health against each
//! protocol's own view, on a sample of positions, off the hot path.
//!
//! * The hot thread hands over a job whenever the snapshot writer takes a
//!   store snapshot (every `snapshot_every_blocks`): that snapshot, the
//!   engine's bands and canonical prices at the same block.
//! * This thread reads every protocol's own prices pinned to that block (the
//!   reader's calls, restated in USD as the hot thread restates them) for
//!   the per-market overlay, then runs [`DriftDetector::tick`] per protocol:
//!   each sampled position's `health()` against its `health_probe()` executed
//!   at that block. Protocols without a probe are skipped by the detector.
//! * [`Policy`] turns the mismatches into [`DriftAction`]s for the hot thread.
//!   A mismatch is first a *state* question — the node holds the account's
//!   real state — so the position is resynced from chain
//!   ([`liq_protocol::Protocol::resync_reads`]) and looked at again on the
//!   next tick. Matching then, it was state drift; when that is a large share
//!   of a tick's comparisons the fold itself is drifting and the whole
//!   protocol is resynced. Still mismatching after its resync, it is a math
//!   or price problem a resync cannot fix: the position is quarantined from
//!   quoting (released once it matches) and an alert is logged.
//!
//! Nothing here halts: halting a protocol stops the liquidations it gets
//! right along with the ones it gets wrong, and a liquidation built on wrong
//! state already fails its pre-send simulation.
//!
//! A job arriving while the previous one runs is dropped, not queued.

use std::collections::HashSet;
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TrySendError};

use liq_config::rpc::{ChainRpc, HttpRpc};
use liq_engine::ProtocolPrices;
use liq_protocol::MarketRows;
use liq_state::{DriftDetector, DriftTick, StoreSnapshot};
use liq_types::{Band, MarketId, PositionId, PriceVector, ProtocolId, Ray};

use crate::bind::BoundProtocol;
use crate::protocol_prices::{read_block, to_usd, ProtocolPriceBook, ReadSet};

/// Thresholds and sample sizes. [`DriftConfig::from_env`] reads the
/// operator's overrides.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DriftConfig {
    /// `|local − probe|` HF, in bps of 1.0, above which a position drifts.
    pub max_bps: u64,
    /// Positions probed per band per tick: Hot, Warm, Cool, Cold.
    pub per_band: [u32; 4],
    /// Resync-fixed positions in one tick, at least, before the whole
    /// protocol is resynced …
    pub sweep_min: usize,
    /// … and at least `1 / sweep_share_inv` of that tick's comparisons.
    pub sweep_share_inv: usize,
}

impl Default for DriftConfig {
    fn default() -> Self {
        Self {
            max_bps: 100,
            per_band: [16, 16, 8, 4],
            sweep_min: 3,
            sweep_share_inv: 4,
        }
    }
}

impl DriftConfig {
    /// Defaults, with `LIQ_DRIFT_MAX_BPS` when set and valid.
    #[must_use]
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Some(v) = std::env::var("LIQ_DRIFT_MAX_BPS")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            c.max_bps = v;
        }
        c
    }

    fn detector(&self) -> DriftDetector {
        let [hot, warm, cool, cold] = self.per_band;
        DriftDetector::new([
            (Band::Hot, hot),
            (Band::Warm, warm),
            (Band::Cool, cool),
            (Band::Cold, cold),
        ])
    }
}

/// What the hot thread does about drift.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DriftAction {
    /// Replace this position's state with the chain's.
    Resync(PositionId),
    /// Replace every position of this protocol's state with the chain's.
    ResyncProtocol(ProtocolId),
    /// Keep this position out of quoting.
    Quarantine(PositionId),
    /// It matches again: quote it.
    Release(PositionId),
}

/// Two looks per mismatch: resync, then judge. Pure state; the thread feeds
/// it each tick's results.
#[derive(Debug, Default)]
pub struct Policy {
    /// Resynced, waiting for the second look.
    pending: HashSet<PositionId>,
    quarantined: HashSet<PositionId>,
}

impl Policy {
    /// Positions of `protocol` the next tick must look at again.
    #[must_use]
    pub fn recheck(&self, snap: &StoreSnapshot, protocol: ProtocolId) -> Vec<PositionId> {
        let mut out: Vec<PositionId> = self
            .pending
            .iter()
            .chain(self.quarantined.iter())
            .copied()
            .filter(|id| {
                snap.keys
                    .get(id.0 as usize)
                    .is_some_and(|k| k.key.protocol == protocol)
            })
            .collect();
        out.sort_unstable();
        out
    }

    /// One tick's `(position, mismatch bps)` for `protocol` into actions.
    pub fn judge(
        &mut self,
        cfg: &DriftConfig,
        protocol: ProtocolId,
        block: u64,
        results: &[(PositionId, u64)],
    ) -> Vec<DriftAction> {
        let mut out = Vec::new();
        let mut fixed = 0usize;
        for &(id, bps) in results {
            let drifts = bps > cfg.max_bps;
            if self.pending.remove(&id) {
                if drifts {
                    self.quarantined.insert(id);
                    out.push(DriftAction::Quarantine(id));
                    tracing::error!(
                        target: "drift",
                        protocol = protocol.0,
                        position = id.0,
                        bps,
                        block,
                        "health still disagrees with the chain after a resync — math or price, not state; position quarantined"
                    );
                } else {
                    fixed = fixed.saturating_add(1);
                    tracing::warn!(
                        target: "drift",
                        protocol = protocol.0,
                        position = id.0,
                        block,
                        "resync from chain fixed a drifting position — its logs left the state wrong"
                    );
                }
            } else if self.quarantined.contains(&id) {
                if !drifts {
                    self.quarantined.remove(&id);
                    out.push(DriftAction::Release(id));
                    tracing::info!(target: "drift", protocol = protocol.0, position = id.0, "quarantined position matches again — released");
                }
            } else if drifts {
                self.pending.insert(id);
                out.push(DriftAction::Resync(id));
                tracing::warn!(
                    target: "drift",
                    protocol = protocol.0,
                    position = id.0,
                    bps,
                    block,
                    "health disagrees with the chain — resyncing the position"
                );
            }
        }
        if fixed >= cfg.sweep_min
            && fixed.saturating_mul(cfg.sweep_share_inv) >= results.len()
        {
            out.push(DriftAction::ResyncProtocol(protocol));
            tracing::error!(
                target: "drift",
                protocol = protocol.0,
                fixed,
                compared = results.len(),
                block,
                "many positions needed a resync — this protocol's log fold is drifting; resyncing all of it"
            );
        }
        out
    }
}

/// One block's inputs, captured on the hot thread.
pub struct DriftJob {
    pub snap: StoreSnapshot,
    /// Index = [`PositionId`]; positions the engine has not banded are `Dead`
    /// (never sampled).
    pub bands: Vec<Band>,
    pub px: PriceVector,
    pub block: u64,
    pub timestamp: u64,
}

/// The hot thread's end: jobs out, actions in.
pub struct DriftHandle {
    tx: SyncSender<DriftJob>,
    actions: Receiver<DriftAction>,
}

impl DriftHandle {
    /// Hand over a job; dropped when the previous one is still running.
    pub fn offer(&self, job: DriftJob) {
        match self.tx.try_send(job) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {
                tracing::error!("drift thread is gone — drift is no longer checked");
            }
        }
    }

    /// Actions decided since the last call. Non-blocking.
    pub fn take_actions(&self) -> Vec<DriftAction> {
        self.actions.try_iter().collect()
    }
}

/// Bands for every position of `snap`, from the engine's.
pub fn bands_for(snap: &StoreSnapshot, band: impl Fn(PositionId) -> Option<Band>) -> Vec<Band> {
    let n = u32::try_from(snap.len()).unwrap_or(u32::MAX);
    (0..n)
        .map(|i| band(PositionId(i)).unwrap_or(Band::Dead))
        .collect()
}

/// [`StoreSnapshot`] as the adapters' [`MarketRows`].
struct SnapRows<'a>(&'a StoreSnapshot);

impl MarketRows for SnapRows<'_> {
    fn rows(&self, market: MarketId) -> Option<&[liq_protocol::MarketRow]> {
        let i = *self.0.market_index.get(usize::try_from(market.0).ok()?)?;
        if i == u16::MAX {
            return None;
        }
        self.0.markets.get(usize::from(i)).map(|m| &m.rows[..])
    }
}

/// Start the thread.
pub fn spawn(
    protocols: &'static [BoundProtocol],
    rpc_url: String,
    cfg: DriftConfig,
) -> std::io::Result<DriftHandle> {
    let (tx, rx) = sync_channel::<DriftJob>(1);
    let (act_tx, actions) = channel::<DriftAction>();
    std::thread::Builder::new()
        .name("liq-obs-drift".into())
        .spawn(move || run(protocols, &rpc_url, cfg, &rx, &act_tx))?;
    Ok(DriftHandle { tx, actions })
}

fn run(
    protocols: &'static [BoundProtocol],
    rpc_url: &str,
    cfg: DriftConfig,
    rx: &Receiver<DriftJob>,
    actions: &Sender<DriftAction>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "drift runtime refused — drift is not checked");
            return;
        }
    };
    let rpc = match HttpRpc::connect(rpc_url) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "drift RPC connect failed — drift is not checked");
            return;
        }
    };
    let mut detector = cfg.detector();
    let mut policy = Policy::default();
    while let Ok(job) = rx.recv() {
        let book = protocol_book(protocols, &rt, &rpc, &job);
        let patch = |p, m| book.patch(p, m).to_vec();
        let exec = |probe: &liq_protocol::ProbeCall| -> Result<Ray, liq_state::DriftError> {
            let ret = rt
                .block_on(rpc.call_at(probe.to, probe.data.clone(), job.block))
                .map_err(|_| liq_state::DriftError::Probe)?;
            (probe.decode)(&ret).map_err(liq_state::DriftError::Protocol)
        };
        for p in protocols {
            let tick = DriftTick {
                snap: &job.snap,
                bands: &job.bands,
                timestamp: job.timestamp,
                px: &job.px,
                patch: Some(&patch),
                protocol: p.as_dyn(),
            };
            let recheck = policy.recheck(&job.snap, p.id());
            match detector.tick(tick, &recheck, exec) {
                Ok(r) => {
                    if r.compared > 0 {
                        let ewma = detector.ewma(p.id());
                        tracing::info!(
                            protocol = p.id().0,
                            block = job.block,
                            compared = r.compared,
                            skipped = r.skipped,
                            ewma_bps = ewma.map(|e| e.value()),
                            "drift tick"
                        );
                    }
                    for a in policy.judge(&cfg, p.id(), job.block, &r.results) {
                        if actions.send(a).is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, protocol = p.id().0, block = job.block, "drift tick failed");
                }
            }
        }
    }
}

/// Every protocol's own prices at `job.block`, restated in USD by the
/// canonical vector, as the engine's overlay.
fn protocol_book(
    protocols: &[BoundProtocol],
    rt: &tokio::runtime::Runtime,
    rpc: &HttpRpc,
    job: &DriftJob,
) -> ProtocolPriceBook {
    let rows = SnapRows(&job.snap);
    let mut reads: ReadSet = Vec::new();
    for (i, p) in protocols.iter().enumerate() {
        for r in p.as_dyn().price_reads(&rows) {
            reads.push((i, r));
        }
    }
    let batch = rt.block_on(read_block(protocols, rpc, &reads, job.block));
    let entries = to_usd(&batch.entries, |a| {
        job.px
            .0
            .get(usize::from(a.0))
            .map(|c| c.price)
            .filter(|p| !p.raw().is_zero())
    });
    let restated = crate::protocol_prices::PriceBatch {
        block: batch.block,
        entries,
        failed: batch.failed,
    };
    let mut book = ProtocolPriceBook::default();
    let mut moves = Vec::new();
    book.apply(&restated, &mut moves);
    book
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::{DriftAction, DriftConfig, Policy};
    use liq_types::{PositionId, ProtocolId};

    const P: ProtocolId = ProtocolId(4);

    fn id(i: u32) -> PositionId {
        PositionId(i)
    }

    /// Policy oracle, the two-look rule: a mismatch is resynced; a match on
    /// the second look is fixed state (no further action); a mismatch on
    /// the second look is quarantined; a quarantined position that matches is
    /// released. Negative: a match never triggers anything, and no action is
    /// a halt.
    #[test]
    fn mismatch_is_resynced_then_judged() {
        let cfg = DriftConfig::default();
        let mut p = Policy::default();
        let first = p.judge(&cfg, P, 1, &[(id(1), 0), (id(2), 500), (id(3), 500)]);
        assert_eq!(
            first,
            vec![DriftAction::Resync(id(2)), DriftAction::Resync(id(3))]
        );
        let second = p.judge(&cfg, P, 2, &[(id(2), 0), (id(3), 500)]);
        assert_eq!(second, vec![DriftAction::Quarantine(id(3))]);
        assert_eq!(p.judge(&cfg, P, 3, &[(id(3), 900)]), vec![]);
        assert_eq!(
            p.judge(&cfg, P, 4, &[(id(3), 0)]),
            vec![DriftAction::Release(id(3))]
        );
        assert_eq!(p.judge(&cfg, P, 5, &[(id(1), 0), (id(3), 0)]), vec![]);
    }

    /// Oracle: enough second-look fixes in one tick — at least `sweep_min`
    /// and a `1/sweep_share_inv` share of the comparisons — say the fold
    /// drifts, and the protocol is resynced. Negative: the same fixes
    /// spread over a larger tick (under the share) do not.
    #[test]
    fn many_fixes_resync_the_protocol() {
        let cfg = DriftConfig::default();
        let bad: Vec<_> = (0..3).map(|i| (id(i), 500)).collect();
        let mut p = Policy::default();
        p.judge(&cfg, P, 1, &bad);
        let fixed: Vec<_> = (0..3).map(|i| (id(i), 0)).collect();
        let mut tick = fixed.clone();
        tick.extend((10..19).map(|i| (id(i), 0)));
        assert_eq!(
            p.judge(&cfg, P, 2, &tick),
            vec![DriftAction::ResyncProtocol(P)],
            "3 of 12 is a quarter"
        );

        let mut q = Policy::default();
        q.judge(&cfg, P, 1, &bad);
        let mut wide = fixed;
        wide.extend((10..30).map(|i| (id(i), 0)));
        assert_eq!(q.judge(&cfg, P, 2, &wide), vec![], "3 of 23 is under a quarter");
    }
}
