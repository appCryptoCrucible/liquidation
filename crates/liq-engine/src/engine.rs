//! The engine (GUIDE 08 §4–§6): ticks, blocks and dirty sets in,
//! candidates out.
//!
//! **Two price vectors, one rule.** `px` is canonical — what the chain
//! believes now; every band, threshold and heap entry is derived against
//! it and only a `Canonical`/`Derived` tick or a dirty set mutates them.
//! `shadow` is `px` with this block's **announced** prices (MEV-Share hint,
//! public transmit) laid over it; announced and predicted ticks evaluate
//! against `shadow` and *only emit* — a pending price that never lands
//! leaves no trace in the engine. Announced prices accumulate for the
//! block so a correlated crash is evaluated with every pending update at
//! once (a position under water only once ETH **and** BTC land is found,
//! not missed); the executor's simulation is the arbiter of what lands
//! together. Predicted prices are a transient override, never kept.
//!
//! **Protocol prices.** A protocol's own oracle can price an asset
//! differently from the canonical feed (Aave CAPO, a Morpho market oracle,
//! a Liquity branch feed). [`World::overlay`] carries those per `(protocol,
//! market)`; each fold lays the position's market prices over the vector in
//! place and restores them after, so every adapter reads exactly what its
//! protocol reads. Thresholds on an overlaid asset register in a second
//! index, in protocol-price units, swept by [`Engine::on_protocol_prices`].
//!
//! **What fires on what** ([`TriggerCause`]): announced → `SvrAuction` /
//! `OraclePublic`; predicted → `OraclePredicted` (not fireable); a landed
//! canonical price → `Stale` (liquidatable on canonical state, untaken);
//! `Derived` → `DerivedRate`; the block clock → `InterestDrift` on the
//! first block a position is liquidatable, `Stale` afterwards; dirty sets →
//! the caller's cause (`UserAction`, `ParamChange`, …).
//!
//! **Sweep set per tick** = threshold crossers on the moved asset (the
//! correctness set, minus `Unfundable`) ∪ every `Hot` and `Warm` position ∪
//! `Cool` once ≥ [`CORRELATED_ASSETS`] distinct assets have moved in the
//! block. The batch is sorted by `PositionId` and deduplicated before the
//! fold so the columnar store is walked in address order (02A review:
//! recovers the hardware prefetcher; no `unsafe`).

use std::time::Instant;

use alloy_primitives::U256;
use fixedbitset::FixedBitSet;
use liq_flash::{is_eligible, Eligibility, FlashIndex, Haircut};
use liq_protocol::{
    BlockNum, DirtyRows, DirtySet, FlashRoute, Health, LegChoice, PositionRef, Protocol,
    ProtocolError, Quote, RouteCache,
};
use liq_state::StateView;
use liq_types::fixed::{mul_div, Rounding, RAY};
use liq_types::{
    AssetId, Band, MarketId, MevShareHint, PositionId, Price, PriceTick, PriceVector, ProtocolId,
    Ray, SourceKind, TraceId, Wad,
};

use crate::band::{classify, BandManager};
use crate::candidate::{Candidate, CandidateQueue, Drain, TriggerCause};
use crate::heap::TimeCrossHeap;
use crate::threshold::{Side, ThresholdIndex};
use crate::EngineError;

/// Distinct assets moved in one block at or above which the sweep takes
/// `Cool` as well as `Hot` + `Warm` (GUIDE 08 §2, mitigation 2).
pub const CORRELATED_ASSETS: usize = 3;

/// Threshold safety margin (GUIDE 08 §2, mitigation 1): a threshold is
/// registered 3 % *before* the adapter's exact crossing price — above it on
/// the falling side, below it on the rising side. For a position whose
/// health is proportional to this one price (single collateral, or single
/// debt) that is the HF 1.03 point exactly; with other legs present the HF
/// margin is smaller but the price margin is never less than 3 %. The
/// recompute the crossing triggers is exact, so the margin only trades a
/// spurious recompute for staleness under a multi-asset move.
pub const MARGIN_BPS: u64 = 300;
const BPS: U256 = U256::from_limbs([10_000, 0, 0, 0]);
const BPS_PLUS_MARGIN: U256 = U256::from_limbs([10_300, 0, 0, 0]);

/// Startup sizing (GUIDE 16: everything reserved once).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    /// Global asset ids the price vector spans.
    pub assets: usize,
    /// Position universe.
    pub positions: usize,
    /// Candidate queue bound (RUST-CONVENTIONS §3.4: 1024).
    pub queue: usize,
}

/// Prices a protocol's own oracle reports for one of its markets, read off
/// the hot path (GUIDE 06: the protocol's price decides health, not ours).
pub trait ProtocolPrices {
    /// `(asset, price)` for `market` of `protocol`, in the canonical
    /// vector's unit (RAY USD per whole token). Empty when none is known.
    fn patch(&self, protocol: ProtocolId, market: MarketId) -> &[(AssetId, Ray)];
}

/// One protocol-reported price that moved this block.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ProtocolPriceMove {
    pub protocol: ProtocolId,
    pub market: MarketId,
    pub asset: AssetId,
    pub old: Ray,
    pub new: Ray,
}

/// Everything outside the engine that one evaluation reads. Borrowed for
/// the call; the engine keeps nothing of it.
#[derive(Copy, Clone)]
pub struct World<'a> {
    /// Canonical state, evaluated at the *target* block's time.
    pub view: StateView<'a>,
    /// Adapters. Laid out by `ProtocolId` where possible (one indexed
    /// load); any other layout is found by scan.
    pub protocols: &'a [&'a dyn Protocol],
    pub flash: &'a FlashIndex,
    pub routes: &'a dyn RouteCache,
    pub haircut: Haircut,
    /// Protocol-reported prices laid over the vector per position market;
    /// `None` evaluates on the canonical vector alone.
    pub overlay: Option<&'a dyn ProtocolPrices>,
}

impl World<'_> {
    #[inline]
    fn protocol(&self, id: ProtocolId) -> Result<&dyn Protocol, EngineError> {
        if let Some(p) = self
            .protocols
            .get(usize::from(id.0))
            .filter(|p| p.id() == id)
        {
            return Ok(*p);
        }
        self.protocols
            .iter()
            .find(|p| p.id() == id)
            .copied()
            .ok_or(EngineError::UnknownProtocol(id))
    }
}

/// Counters, all monotone. GUIDE 09 reads them; nothing here is on the
/// decision path.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub ticks: u64,
    /// Canonical folds (state mutated).
    pub folds: u64,
    /// Announced / predicted evaluations (emit only).
    pub shadow_folds: u64,
    pub correlated_sweeps: u64,
    pub emitted: u64,
    /// Positions whose fold returned an error (each is also logged).
    pub fold_errors: u64,
}

/// Why a canonical fold emits.
#[derive(Copy, Clone)]
enum Cause<'a> {
    /// The caller's cause (dirty sets).
    Given(&'a TriggerCause),
    /// A canonical price landed: liquidatable on canonical state.
    Landed,
    /// The block clock: `InterestDrift` on first sight, `Stale` after.
    Accrual,
}

/// Mutable state a fold touches. Separate from the price vectors and the
/// batch so one method can hold `&mut` here and `&` there.
struct Tables {
    bands: BandManager,
    index: ThresholdIndex,
    /// Thresholds on overlaid assets, in protocol-price units.
    overlay_index: ThresholdIndex,
    heap: TimeCrossHeap,
    elig: Eligibility,
    queue: CandidateQueue,
    /// First block a position was seen liquidatable on canonical state;
    /// `0` = not liquidatable at the last canonical fold.
    liq_since: Vec<BlockNum>,
    trace_seq: u64,
    stats: Stats,
}

/// The health engine. One per node; driven from the ExEx thread.
pub struct Engine {
    px: PriceVector,
    shadow: PriceVector,
    /// Assets whose `shadow` slot currently differs from `px`.
    overridden: Vec<AssetId>,
    /// Assets moved (announced or landed) in the current block.
    moved: FixedBitSet,
    moved_count: usize,
    t: Tables,
    /// Scratch: ids to fold this call. Sorted + deduplicated before use.
    batch: Vec<PositionId>,
    /// Scratch: `(slot, price, ts)` a fold's overlay replaced, restored
    /// after the fold.
    saved: Vec<(usize, Ray, u64)>,
}

impl Engine {
    #[must_use]
    pub fn new(cfg: EngineConfig) -> Self {
        Self {
            px: PriceVector::zeroed(),
            shadow: PriceVector::zeroed(),
            overridden: Vec::with_capacity(cfg.assets),
            moved: FixedBitSet::with_capacity(cfg.assets),
            moved_count: 0,
            t: Tables {
                bands: BandManager::with_capacity(cfg.positions),
                index: ThresholdIndex::new(cfg.assets, cfg.positions),
                overlay_index: ThresholdIndex::new(cfg.assets, cfg.positions),
                heap: TimeCrossHeap::with_capacity(cfg.positions),
                elig: Eligibility::new(cfg.positions),
                queue: CandidateQueue::with_capacity(cfg.queue),
                liq_since: Vec::with_capacity(cfg.positions),
                trace_seq: 0,
                stats: Stats::default(),
            },
            batch: Vec::with_capacity(cfg.positions.clamp(1024, 1 << 16)),
            saved: Vec::with_capacity(cfg.assets.min(256)),
        }
    }

    // ---- read side -----------------------------------------------------------

    /// Canonical prices as the engine holds them.
    #[inline]
    #[must_use]
    pub fn prices(&self) -> &PriceVector {
        &self.px
    }

    #[inline]
    #[must_use]
    pub fn band(&self, id: PositionId) -> Option<Band> {
        self.t.bands.band(id)
    }

    #[inline]
    #[must_use]
    pub fn bands(&self) -> &BandManager {
        &self.t.bands
    }

    #[inline]
    #[must_use]
    pub fn index(&self) -> &ThresholdIndex {
        &self.t.index
    }

    /// Thresholds registered on overlaid assets (protocol-price units).
    #[inline]
    #[must_use]
    pub fn overlay_index(&self) -> &ThresholdIndex {
        &self.t.overlay_index
    }

    #[inline]
    #[must_use]
    pub fn heap(&self) -> &TimeCrossHeap {
        &self.t.heap
    }

    #[inline]
    #[must_use]
    pub fn stats(&self) -> &Stats {
        &self.t.stats
    }

    /// Candidates dropped by the bounded queue so far (GUIDE 08 §6: must be
    /// zero in a calm market).
    #[inline]
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.t.queue.dropped()
    }

    #[inline]
    #[must_use]
    pub fn queued(&self) -> usize {
        self.t.queue.len()
    }

    /// Drain queued candidates, highest `(fireable, est_value)` first.
    #[inline]
    pub fn candidates(&mut self) -> Drain<'_> {
        self.t.queue.drain()
    }

    // ---- prices --------------------------------------------------------------

    /// Load the canonical vector wholesale (startup, or a resync from the
    /// oracle's triple buffer). Slot `i` must hold asset `i` — the layout
    /// every adapter indexes by. Closes the move window.
    pub fn load_prices(&mut self, px: &PriceVector) -> Result<(), EngineError> {
        for (i, p) in px.0.iter().enumerate() {
            if u16::try_from(i).ok() != Some(p.asset.0) {
                return Err(EngineError::PriceLayout {
                    slot: u16::try_from(i).unwrap_or(u16::MAX),
                    found: p.asset,
                });
            }
        }
        self.px.0.clear();
        self.px.0.extend(px.0.iter().map(slim));
        self.shadow.0.clear();
        self.shadow.0.extend(self.px.0.iter().cloned());
        self.overridden.clear();
        self.moved.clear();
        self.moved_count = 0;
        Ok(())
    }

    #[inline]
    fn slot(&self, a: AssetId) -> Result<usize, EngineError> {
        let i = usize::from(a.0);
        if i < self.px.0.len() {
            Ok(i)
        } else {
            Err(EngineError::UnknownAsset(a))
        }
    }

    /// Record a real move of `a` in this block; `true` once the block is a
    /// correlated move.
    #[inline]
    fn note_move(&mut self, i: usize) -> bool {
        if !self.moved.contains(i) {
            self.moved.grow_and_insert(i);
            self.moved_count = self.moved_count.saturating_add(1);
        }
        self.moved_count >= CORRELATED_ASSETS
    }

    /// Fill `batch` with the sweep set for a move of `a` from `old` to `new`.
    fn gather(&mut self, a: AssetId, old: Ray, new: Ray, correlated: bool) {
        let (batch, t) = (&mut self.batch, &mut self.t);
        batch.clear();
        let bands = &t.bands;
        batch.extend(
            t.index
                .crossed(a, old, new)
                .filter(|id| bands.band(*id) != Some(Band::Unfundable)),
        );
        batch.extend_from_slice(bands.members(Band::Hot));
        batch.extend_from_slice(bands.members(Band::Warm));
        if correlated {
            batch.extend_from_slice(bands.members(Band::Cool));
            t.stats.correlated_sweeps = t.stats.correlated_sweeps.saturating_add(1);
        }
    }

    /// GUIDE 08 §4. `Predicted` pre-warms only; announced fires; canonical
    /// and derived commit.
    pub fn on_price_tick(&mut self, w: &World<'_>, tick: &PriceTick) -> Result<(), EngineError> {
        self.t.stats.ticks = self.t.stats.ticks.saturating_add(1);
        let i = self.slot(tick.asset)?;
        let canonical = self
            .px
            .0
            .get(i)
            .map(|p| p.price)
            .ok_or(EngineError::UnknownAsset(tick.asset))?;
        match &tick.source {
            SourceKind::Canonical => self.commit(w, tick, i, canonical, Cause::Landed),
            SourceKind::Derived { .. } => {
                let cause = TriggerCause::DerivedRate { source: tick.asset };
                self.commit(w, tick, i, canonical, Cause::Given(&cause))
            }
            SourceKind::SvrAnnounced { hint, deadline } => {
                let cause = TriggerCause::SvrAuction {
                    hint: hint.hash,
                    deadline: *deadline,
                };
                self.announce(w, tick, i, canonical, &cause, Some(*deadline))
            }
            SourceKind::PendingPublic { tx, .. } => {
                let cause = TriggerCause::OraclePublic { tx: *tx };
                self.announce(w, tick, i, canonical, &cause, None)
            }
            SourceKind::Predicted { confidence, .. } => {
                let cause = TriggerCause::OraclePredicted { conf: *confidence };
                // Predictions are not moves: they never make a block
                // "correlated" on their own.
                let correlated = self.moved_count >= CORRELATED_ASSETS;
                self.gather(tick.asset, canonical, tick.price, correlated);
                let Some(cell) = self.shadow.0.get_mut(i) else {
                    return Err(EngineError::UnknownAsset(tick.asset));
                };
                let saved = std::mem::replace(cell, slim(tick));
                let r = self.run(w, false, Cause::Given(&cause), None);
                if let Some(cell) = self.shadow.0.get_mut(i) {
                    *cell = saved;
                }
                r
            }
        }
    }

    fn commit(
        &mut self,
        w: &World<'_>,
        tick: &PriceTick,
        i: usize,
        old: Ray,
        cause: Cause<'_>,
    ) -> Result<(), EngineError> {
        let correlated = self.note_move(i);
        self.gather(tick.asset, old, tick.price, correlated);
        let p = slim(tick);
        if let Some(cell) = self.shadow.0.get_mut(i) {
            cell.clone_from(&p);
        }
        if let Some(cell) = self.px.0.get_mut(i) {
            *cell = p;
        }
        // A pending price for this asset has landed (or been superseded).
        self.overridden.retain(|&a| a != tick.asset);
        self.run(w, true, cause, None)
    }

    fn announce(
        &mut self,
        w: &World<'_>,
        tick: &PriceTick,
        i: usize,
        canonical: Ray,
        cause: &TriggerCause,
        deadline: Option<Instant>,
    ) -> Result<(), EngineError> {
        let correlated = self.note_move(i);
        // Crossers are measured from the *canonical* price: every threshold
        // between what the chain believes and what is about to land.
        self.gather(tick.asset, canonical, tick.price, correlated);
        if let Some(cell) = self.shadow.0.get_mut(i) {
            *cell = slim(tick);
        }
        if !self.overridden.contains(&tick.asset) {
            self.overridden.push(tick.asset);
        }
        self.run(w, false, Cause::Given(cause), deadline)
    }

    // ---- blocks, dirty sets, flash ------------------------------------------

    /// A new canonical block is in the store. Closes the move window (shadow
    /// falls back to canonical), commits the threshold index, folds every
    /// position the time heap says has crossed plus every `Hot` position
    /// (accrual moves them every block), and emits `InterestDrift` / `Stale`.
    pub fn on_block(&mut self, w: &World<'_>) -> Result<(), EngineError> {
        for a in self.overridden.drain(..) {
            let i = usize::from(a.0);
            if let (Some(dst), Some(src)) = (self.shadow.0.get_mut(i), self.px.0.get(i)) {
                dst.clone_from(src);
            }
        }
        self.moved.clear();
        self.moved_count = 0;
        self.t.index.commit();
        self.t.overlay_index.commit();
        let now = w.view.timestamp();
        self.batch.clear();
        self.batch.extend(self.t.heap.crossed(now));
        self.batch
            .extend_from_slice(self.t.bands.members(Band::Hot));
        self.run(w, true, Cause::Accrual, None)
    }

    /// One collapsed dirty set from `protocol`'s `apply_log`s (GUIDE 03
    /// §4). `cause` is what the caller knows the engine cannot — the
    /// borrower's `UserAction { tx }`, a governance `ParamChange`, … — and
    /// is attached to every candidate this fold emits.
    pub fn on_dirty(
        &mut self,
        w: &World<'_>,
        protocol: ProtocolId,
        dirty: &DirtySet,
        cause: &TriggerCause,
    ) -> Result<(), EngineError> {
        self.batch.clear();
        match dirty {
            DirtySet::None => return Ok(()),
            DirtySet::Positions(ids) => self.batch.extend_from_slice(ids),
            DirtySet::MarketAccrual(rows) => self.gather_rows(w, rows, false)?,
            DirtySet::MarketReprice(rows) => self.gather_rows(w, rows, true)?,
            DirtySet::ProtocolWide => {
                let n = u32::try_from(w.view.len()).unwrap_or(u32::MAX);
                for id in (0..n).map(PositionId) {
                    if w.view.position(id)?.key.protocol == protocol {
                        self.batch.push(id);
                    }
                }
            }
        }
        self.run(w, true, Cause::Given(cause), None)
    }

    /// Positions touching any of `rows`, from the walked bands (`Hot`,
    /// `Warm`, `Cool`) and — for a reprice — every position with a live
    /// threshold on the rows' assets. `Unfundable` is skipped: a rate or
    /// parameter cannot make its debt fundable.
    fn gather_rows(
        &mut self,
        w: &World<'_>,
        rows: &DirtyRows,
        reprice: bool,
    ) -> Result<(), EngineError> {
        let touches = |id: PositionId| -> Result<bool, EngineError> {
            let p = w.view.position(id)?;
            Ok(rows
                .iter()
                .any(|r| r.market == p.key.market && p.config.contains(r.slot)))
        };
        let (batch, t) = (&mut self.batch, &self.t);
        for band in [Band::Hot, Band::Warm, Band::Cool] {
            for &id in t.bands.members(band) {
                if touches(id)? {
                    batch.push(id);
                }
            }
        }
        if reprice {
            for r in rows {
                let asset = w
                    .view
                    .markets(r.market)?
                    .get(usize::from(r.slot))
                    .ok_or(ProtocolError::SlotOutOfRange(*r))?
                    .asset;
                for id in t.index.registered(asset) {
                    if touches(id)? {
                        batch.push(id);
                    }
                }
            }
        }
        Ok(())
    }

    /// Protocol-reported prices moved (one batch per block, after the
    /// block's logs). Folds every position whose protocol-price threshold a
    /// move crossed, plus every `Hot`/`Warm`/`Cool` position in a moved
    /// market. An asset's *first* protocol price has no thresholds behind
    /// it yet: the caller resyncs once instead.
    pub fn on_protocol_prices(
        &mut self,
        w: &World<'_>,
        moves: &[ProtocolPriceMove],
    ) -> Result<(), EngineError> {
        if moves.is_empty() {
            return Ok(());
        }
        self.batch.clear();
        let (batch, t) = (&mut self.batch, &self.t);
        let bands = &t.bands;
        for m in moves {
            batch.extend(
                t.overlay_index
                    .crossed(m.asset, m.old, m.new)
                    .filter(|id| bands.band(*id) != Some(Band::Unfundable)),
            );
        }
        for band in [Band::Hot, Band::Warm, Band::Cool] {
            for &id in bands.members(band) {
                let key = w.view.position(id)?.key;
                if moves
                    .iter()
                    .any(|m| m.protocol == key.protocol && m.market == key.market)
                {
                    batch.push(id);
                }
            }
        }
        self.run(w, true, Cause::Landed, None)
    }

    /// Flash liquidity moved materially: every `Unfundable` position is
    /// re-evaluated and promoted if its debt is fundable now (GUIDE 07 §4b:
    /// marked, never deleted).
    pub fn on_flash_change(&mut self, w: &World<'_>) -> Result<(), EngineError> {
        self.batch.clear();
        self.batch
            .extend_from_slice(self.t.bands.members(Band::Unfundable));
        self.run(w, true, Cause::Landed, None)
    }

    /// Fold the whole universe on canonical state: initial registration
    /// after backfill, or after a reorg deeper than the heap's generations
    /// can express. O(N).
    pub fn resync(&mut self, w: &World<'_>) -> Result<(), EngineError> {
        let n = u32::try_from(w.view.len()).unwrap_or(u32::MAX);
        self.batch.clear();
        self.batch.extend((0..n).map(PositionId));
        self.run(w, true, Cause::Landed, None)
    }

    // ---- the fold --------------------------------------------------------------

    /// Sort + dedup the batch, fold every id, put the buffer back. Continues
    /// past a failing position (logged, counted) and returns the first error
    /// so the caller can halt with the rest of the batch already refreshed.
    fn run(
        &mut self,
        w: &World<'_>,
        canonical: bool,
        cause: Cause<'_>,
        deadline: Option<Instant>,
    ) -> Result<(), EngineError> {
        let mut batch = std::mem::take(&mut self.batch);
        batch.sort_unstable();
        batch.dedup();
        let px = if canonical {
            &mut self.px
        } else {
            &mut self.shadow
        };
        let saved = &mut self.saved;
        let block = w.view.tip();
        let mut first = Ok(());
        for &id in &batch {
            let r = overlay_on(px, w, id, saved).and_then(|()| {
                if canonical {
                    fold(&mut self.t, px, w, id, cause, block, saved)
                } else {
                    shadow_fold(&mut self.t, px, w, id, cause, deadline)
                }
            });
            overlay_off(px, saved);
            if let Err(e) = r {
                self.t.stats.fold_errors = self.t.stats.fold_errors.saturating_add(1);
                tracing::error!(position = id.0, error = %e, "engine fold failed");
                if first.is_ok() {
                    first = Err(e);
                }
            }
        }
        batch.clear();
        self.batch = batch;
        first
    }
}

/// Lay the position's protocol prices over `px`, remembering what they
/// replaced in `saved`. A slot never canonically priced gets the view's
/// timestamp so readers that copy `ts` see a live price.
fn overlay_on(
    px: &mut PriceVector,
    w: &World<'_>,
    id: PositionId,
    saved: &mut Vec<(usize, Ray, u64)>,
) -> Result<(), EngineError> {
    saved.clear();
    let Some(ov) = w.overlay else {
        return Ok(());
    };
    let key = w.view.position(id)?.key;
    let patch = ov.patch(key.protocol, key.market);
    if patch.is_empty() {
        return Ok(());
    }
    let now = w.view.timestamp();
    for &(asset, price) in patch {
        let i = usize::from(asset.0);
        let cell = px.0.get_mut(i).ok_or(EngineError::UnknownAsset(asset))?;
        saved.push((i, cell.price, cell.ts));
        cell.price = price;
        if cell.ts == 0 {
            cell.ts = now;
        }
    }
    Ok(())
}

/// Undo [`overlay_on`], newest first.
fn overlay_off(px: &mut PriceVector, saved: &mut Vec<(usize, Ray, u64)>) {
    for &(i, price, ts) in saved.iter().rev() {
        if let Some(cell) = px.0.get_mut(i) {
            cell.price = price;
            cell.ts = ts;
        }
    }
    saved.clear();
}

/// `tick` with any MEV-Share hint payload (`call_data`, `logs`) dropped:
/// the vectors hold what health needs — the price, its source class, hash
/// and deadline — and never a heap payload (06A-1: hint logs stay off the
/// `PriceVector`). Storing it is then allocation-free.
fn slim(p: &Price) -> Price {
    let source = match &p.source {
        SourceKind::SvrAnnounced { hint, deadline } => SourceKind::SvrAnnounced {
            hint: MevShareHint {
                hash: hint.hash,
                to: hint.to,
                function_selector: hint.function_selector,
                call_data: None,
                logs: None,
            },
            deadline: *deadline,
        },
        other => other.clone(),
    };
    Price {
        asset: p.asset,
        price: p.price,
        source,
        block: p.block,
        ts: p.ts,
    }
}

#[inline]
fn has_debt(h: &Health) -> bool {
    h.hf != Health::NO_DEBT_HF && !h.debt_value.raw().is_zero()
}

/// Health, and the quote when below the boundary.
#[inline]
fn evaluate(
    p: &dyn Protocol,
    pos: PositionRef<'_>,
    px: &PriceVector,
) -> Result<(Health, Option<Quote>), EngineError> {
    let h = p.health(pos, px)?;
    let q = if h.hf < Ray::ONE {
        p.quote(pos, px)?
    } else {
        None
    };
    Ok((h, q))
}

/// Side and registered price for an exact crossing price `lp` given the
/// current price `cur`: [`MARGIN_BPS`] before the crossing, clamped so the
/// threshold never sits on the safe side of the current price (a position
/// already inside its margin is `Hot` and swept every tick regardless; the
/// clamp keeps it inside the next tick's range too). At `lp == cur` the
/// side is undecidable from prices alone and both are registered.
fn margin(lp: Ray, cur: Ray) -> Result<(Option<Side>, Ray), EngineError> {
    if lp < cur {
        let up = mul_div(lp.raw(), BPS_PLUS_MARGIN, BPS, Rounding::Up)?;
        Ok((Some(Side::Falling), Ray::from_raw(up.min(cur.raw()))))
    } else if lp > cur {
        let down = mul_div(lp.raw(), BPS, BPS_PLUS_MARGIN, Rounding::Down)?;
        Ok((Some(Side::Rising), Ray::from_raw(down.max(cur.raw()))))
    } else {
        Ok((None, cur))
    }
}

/// Canonical fold of one position: exact health at `px`, quote +
/// eligibility when below the boundary, then band, thresholds and heap
/// re-derived (GUIDE 08 §2 mitigation 3: every recompute re-registers).
#[allow(clippy::too_many_arguments)] // each input is a distinct fold term
fn fold(
    t: &mut Tables,
    px: &PriceVector,
    w: &World<'_>,
    id: PositionId,
    cause: Cause<'_>,
    block: BlockNum,
    overlaid: &[(usize, Ray, u64)],
) -> Result<(), EngineError> {
    t.stats.folds = t.stats.folds.saturating_add(1);
    let pos = w.view.position(id)?;
    let p = w.protocol(pos.key.protocol)?;
    let (h, q) = evaluate(p, pos, px)?;
    let debt = has_debt(&h);
    let mut band = classify(h.hf, debt);
    let i = id.0 as usize;
    if i >= t.liq_since.len() {
        t.liq_since.resize(i.saturating_add(1), 0);
    }
    let since = t.liq_since.get(i).copied().unwrap_or(0);
    let mut liquidatable = false;
    if let Some(q) = q {
        match t.elig.evaluate(&q, w.flash, w.routes, w.haircut) {
            Some((legs, route)) => {
                liquidatable = true;
                let since = if since == 0 { block } else { since };
                let cause = match cause {
                    Cause::Given(c) => c.clone(),
                    Cause::Landed => TriggerCause::Stale {
                        liquidatable_since: since,
                    },
                    Cause::Accrual if since == block => TriggerCause::InterestDrift,
                    Cause::Accrual => TriggerCause::Stale {
                        liquidatable_since: since,
                    },
                };
                emit(t, id, pos.key.protocol, h, q, legs, route, cause, None)?;
            }
            None => band = Band::Unfundable,
        }
    }
    if let Some(s) = t.liq_since.get_mut(i) {
        *s = if liquidatable {
            if since == 0 {
                block
            } else {
                since
            }
        } else {
            0
        };
    }
    // Thresholds: one per price-sensitive slot, at the margin. A slot whose
    // price came from the protocol overlay registers in the overlay index,
    // in the protocol's units — canonical ticks never compare against it.
    t.index.begin(id);
    t.overlay_index.begin(id);
    if debt {
        for slot in h.price_sensitivity.iter() {
            let asset = pos
                .markets
                .get(usize::from(slot))
                .ok_or(ProtocolError::SlotOutOfRange(liq_protocol::MarketSlot {
                    market: pos.key.market,
                    slot,
                }))?
                .asset;
            let cur =
                px.0.get(usize::from(asset.0))
                    .ok_or(EngineError::UnknownAsset(asset))?
                    .price;
            if let Some(lp) = p.liquidation_price(pos, px, asset)? {
                let (side, at) = margin(lp.price, cur)?;
                let on_overlay = overlaid.iter().any(|(i, ..)| *i == usize::from(asset.0));
                let index = if on_overlay {
                    &mut t.overlay_index
                } else {
                    &mut t.index
                };
                match side {
                    Some(side) => index.register(id, asset, side, at)?,
                    None => {
                        index.register(id, asset, Side::Falling, at)?;
                        index.register(id, asset, Side::Rising, at)?;
                    }
                }
            }
        }
    }
    // Heap: only a healthy position has a future crossing; a liquidatable
    // one is `Hot` and folded every block anyway.
    if debt && h.hf >= Ray::ONE {
        match p.time_to_cross(pos, px)? {
            Some(at) => t.heap.push(at, id),
            None => t.heap.invalidate(id),
        }
    } else {
        t.heap.invalidate(id);
    }
    t.bands.set(id, band);
    Ok(())
}

/// Evaluation at a pending or predicted price: emits, mutates nothing.
fn shadow_fold(
    t: &mut Tables,
    px: &PriceVector,
    w: &World<'_>,
    id: PositionId,
    cause: Cause<'_>,
    deadline: Option<Instant>,
) -> Result<(), EngineError> {
    t.stats.shadow_folds = t.stats.shadow_folds.saturating_add(1);
    let Cause::Given(cause) = cause else {
        return Ok(());
    };
    let pos = w.view.position(id)?;
    let p = w.protocol(pos.key.protocol)?;
    let (h, q) = evaluate(p, pos, px)?;
    if let Some(q) = q {
        if let Some((legs, route)) = is_eligible(&q, w.flash, w.routes, w.haircut) {
            emit(
                t,
                id,
                pos.key.protocol,
                h,
                q,
                legs,
                route,
                cause.clone(),
                deadline,
            )?;
        }
    }
    Ok(())
}

/// Build and queue one candidate. `est_value` = `min(debt, collateral) ·
/// bonus` of the chosen seize leg, in the numeraire (WAD): the bonus the
/// pair would pay on its smaller leg — an ordering key, documented as such
/// on [`Candidate::est_value`].
#[allow(clippy::too_many_arguments)]
fn emit(
    t: &mut Tables,
    id: PositionId,
    protocol: ProtocolId,
    health: Health,
    quote: Quote,
    legs: LegChoice,
    funding: FlashRoute,
    cause: TriggerCause,
    deadline: Option<Instant>,
) -> Result<(), EngineError> {
    let bonus = quote
        .seize_options
        .get(usize::from(legs.seize))
        .map(|s| s.bonus.raw())
        .ok_or(ProtocolError::Internal)?;
    let notional = health.debt_value.raw().min(health.collateral_value.raw());
    let est_value = Wad::from_raw(mul_div(notional, bonus, RAY, Rounding::Down)?);
    let trace = TraceId::from_raw(t.trace_seq);
    t.trace_seq = t.trace_seq.wrapping_add(1);
    let deadline = deadline.or(match cause {
        TriggerCause::SvrAuction { deadline, .. } => Some(deadline),
        _ => None,
    });
    t.queue.push(Candidate {
        position: id,
        protocol,
        health,
        quote,
        legs,
        funding,
        cause,
        est_value,
        deadline,
        trace,
    });
    t.stats.emitted = t.stats.emitted.saturating_add(1);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{margin, MARGIN_BPS};
    use crate::threshold::Side;
    use alloy_primitives::U256;
    use liq_types::Ray;

    fn r(v: u64) -> Ray {
        Ray::from_raw(U256::from(v))
    }

    /// Oracle: GUIDE 08 §2 — the registered threshold is 3 % before the
    /// exact crossing on the danger side, never past the current price,
    /// and a crossing at the current price registers both sides.
    #[test]
    fn margin_is_three_percent_toward_danger_and_clamped() {
        assert_eq!(MARGIN_BPS, 300);
        // Collateral: crossing at 1000, price 2000 → 1030 falling.
        assert_eq!(
            margin(r(1_000), r(2_000)).unwrap(),
            (Some(Side::Falling), r(1_030))
        );
        // Debt: crossing at 2060, price 1000 → 2000 rising.
        assert_eq!(
            margin(r(2_060), r(1_000)).unwrap(),
            (Some(Side::Rising), r(2_000))
        );
        // Inside the margin already: clamped to the current price.
        assert_eq!(
            margin(r(1_000), r(1_010)).unwrap(),
            (Some(Side::Falling), r(1_010))
        );
        assert_eq!(
            margin(r(1_020), r(1_010)).unwrap(),
            (Some(Side::Rising), r(1_010))
        );
        // At the boundary.
        assert_eq!(margin(r(5), r(5)).unwrap(), (None, r(5)));
    }
}
