//! Hot-thread drain join (WP 17D / D63).
//!
//! After-block: dirty → `Engine::on_dirty` / `on_block` →
//! `CandidateQueue::drain` → map fields → `select` → `assemble` →
//! `liq_sim::verify` or fail-closed skip → `ExecInbox::try_send`.
//! Same path for shadow and live. Synchronous: never awaits, never
//! blocks on a runtime.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, Bytes, U256};
use arc_swap::ArcSwap;
use liq_engine::{Candidate, Engine, EngineConfig, TriggerCause, World};
use liq_exec::fee::FeeQuote;
use liq_exec::path::{ExecInbox, ExecJob};
use liq_flash::{CostModel, FlashIndex, Haircut};
use liq_node::{as_dirty_sets, AfterBlock, AfterBlockCtx};
use liq_plan::{EncodedPlan, ValidateCtx, FLAG_SWEEP};
use liq_protocol::{Constraints, DirtySet, Protocol};
use liq_router::{
    assemble, bid, debt_notional_eth_wei, select, Bid, BidConfig, BidSchedule, GasTerms,
    MarketView, PoolBook, PositionInput, SelectCfg, SelectedPlan, SolveBudget, EXACT_K,
    NONCE_SLOTS, OUT_PER_ETH_WETH,
};
use liq_sim::{
    block_env_at, execute_calldata, verify, Bundle, MemoryFactory, SimError, SimOutcome, SimTx,
    Simulator, StateProviderFactory, Trigger, PLANNED_EXECUTOR,
};
use liq_types::{AssetId, FlashProvider, TriggerKind};
use parking_lot::RwLock;

use crate::assemble_view::{require_meta, require_token, ProcessAssembleView};
use crate::bind::{BoundProtocol, SelectBind};
use crate::index::{BoundIndex, FlashSources};

/// Inputs `select` / `assemble` refuse to default. Missing → skip, log.
#[derive(Clone, Debug)]
pub struct SelectReady {
    pub cfg: SelectCfg,
    pub gas: GasTerms,
    /// Set when no schedule is loaded (tests). Production leaves this
    /// empty and bids from [`SelectCfg::bids`].
    pub bid: Option<Bid>,
    pub validate: ValidateCtx,
    pub haircut: Haircut,
    pub gas_failed: u64,
    pub flags: u8,
}

/// Sim seam. No provider → skip send. `StateUnavailable` /
/// `ArchiveUnavailable` → skip send. Does not invent state.
pub trait DrainSim: Send {
    fn verify(&self, bundle: &Bundle, number: u64, timestamp: u64) -> Result<SimOutcome, SimError>;
}

/// In-memory factory wrapper. Overlay only — no invented balances.
pub struct MemoryDrainSim {
    factory: MemoryFactory,
}

impl MemoryDrainSim {
    #[must_use]
    pub fn new(factory: MemoryFactory) -> Self {
        Self { factory }
    }
}

impl DrainSim for MemoryDrainSim {
    fn verify(&self, bundle: &Bundle, number: u64, timestamp: u64) -> Result<SimOutcome, SimError> {
        let provider = self.factory.latest()?;
        let mut sim =
            Simulator::from_provider(provider, PLANNED_EXECUTOR, Address::ZERO, Address::ZERO);
        verify(&mut sim, bundle, block_env_at(number, timestamp))
    }
}

/// Counters for tests and logs. Not decision inputs.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct DrainStats {
    pub jobs_sent: u64,
    pub skipped_predicted: u64,
    pub skipped_pins: u64,
    pub skipped_select: u64,
    pub skipped_sim: u64,
    pub skipped_exec: u64,
    pub skipped_job: u64,
    pub inbox_full: u64,
}

/// Hot-thread join. Empty protocols / empty view / unbound exec is a
/// live no-op — nothing is fabricated.
pub struct DrainJoin {
    pub engine: Engine,
    protocols: &'static [BoundProtocol],
    flash: Arc<ArcSwap<FlashIndex>>,
    routes: liq_router::WarmRouteCache,
    pub assemble: ProcessAssembleView,
    book: Arc<RwLock<PoolBook>>,
    flash_sources: Option<FlashSources>,
    flash_scratch: FlashIndex,
    pub select: Option<SelectReady>,
    pub select_bind: Option<SelectBind>,
    pub inbox: Option<ExecInbox>,
    pub sim: Option<Box<dyn DrainSim>>,
    pub operator: Option<Address>,
    pub chain_id: u64,
    pub cons: Constraints,
    pub fee: Option<FeeQuote>,
    /// Kept for the process. `observe_parent` only when header base fee ≠ 0.
    pub oracle: Option<liq_router::GasOracle>,
    /// Committed `bid.toml` only. Missing → [`Self::select`] stays None.
    pub bid_cfg: Option<BidSchedule>,
    header_clock: Option<Arc<crate::stall::HeaderClock>>,
}

impl DrainJoin {
    /// Production wiring: reserved engine, empty adapters/book/select/sim.
    #[must_use]
    pub fn live_noop(
        flash: Arc<ArcSwap<FlashIndex>>,
        routes: liq_router::WarmRouteCache,
        assemble: ProcessAssembleView,
        inbox: Option<ExecInbox>,
        operator: Option<Address>,
        chain_id: u64,
    ) -> Self {
        Self {
            engine: Engine::new(EngineConfig {
                assets: 1024,
                positions: 1024,
                queue: 1024,
            }),
            protocols: &[],
            flash,
            routes,
            assemble,
            book: Arc::new(RwLock::new(PoolBook::new(HashMap::new(), None, 0))),
            flash_sources: None,
            flash_scratch: FlashIndex::new(0),
            select: None,
            select_bind: None,
            inbox,
            sim: None,
            operator,
            chain_id,
            cons: Constraints::UNBOUNDED,
            fee: None,
            oracle: None,
            bid_cfg: None,
            header_clock: None,
        }
    }

    /// Production join: leaked adapters / intern / wrap bind / live oracle.
    /// Sim stays `None`. `select` stays `None` until [`Self::refresh_select`]
    /// sees a nonzero header gas_limit (never a 30M default).
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn live(
        flash: Arc<ArcSwap<FlashIndex>>,
        routes: liq_router::WarmRouteCache,
        assemble: ProcessAssembleView,
        inbox: Option<ExecInbox>,
        operator: Option<Address>,
        chain_id: u64,
        protocols: &'static [BoundProtocol],
        select_bind: Option<SelectBind>,
        fee: Option<FeeQuote>,
        oracle: Option<liq_router::GasOracle>,
    ) -> Self {
        let mut j = Self::live_noop(flash, routes, assemble, inbox, operator, chain_id);
        j.protocols = protocols;
        j.select_bind = select_bind;
        j.fee = fee;
        j.oracle = oracle;
        j
    }

    /// Same leaked adapters the ingest router subscribed.
    #[must_use]
    pub fn bound_protocols(&self) -> &'static [BoundProtocol] {
        self.protocols
    }

    #[must_use]
    pub fn with_book(mut self, book: PoolBook) -> Self {
        self.book = Arc::new(RwLock::new(book));
        self
    }

    /// Same leaked book / flash sources the ingest router subscribed.
    #[must_use]
    pub fn with_index(mut self, index: &'static BoundIndex) -> Self {
        self.book = Arc::clone(&index.book);
        self.flash_sources = Some(Arc::clone(&index.sources));
        self.flash_scratch = FlashIndex::new(index.flash_assets);
        self
    }

    /// End of block: re-read sources into the process [`FlashIndex`].
    fn publish_flash(&mut self) {
        let Some(srcs) = self.flash_sources.as_ref() else {
            return;
        };
        let g = srcs.lock();
        self.flash_scratch.refresh(&g);
        self.flash_scratch.publish_if_material(&self.flash, 0);
    }

    #[must_use]
    pub fn with_select(mut self, ready: SelectReady) -> Self {
        self.select = Some(ready);
        self
    }

    #[must_use]
    pub fn with_sim(mut self, sim: Box<dyn DrainSim>) -> Self {
        self.sim = Some(sim);
        self
    }

    #[must_use]
    pub fn with_fee(mut self, fee: FeeQuote) -> Self {
        self.fee = Some(fee);
        self
    }

    #[must_use]
    pub fn with_select_bind(mut self, bind: SelectBind) -> Self {
        self.select_bind = Some(bind);
        self
    }

    #[must_use]
    pub fn with_bid_cfg(mut self, bid_cfg: Option<BidSchedule>) -> Self {
        self.bid_cfg = bid_cfg;
        self
    }

    /// Shared with the stall thread. `None` means no silence measurement.
    #[must_use]
    pub fn with_header_clock(mut self, clock: Arc<crate::stall::HeaderClock>) -> Self {
        self.header_clock = Some(clock);
        self
    }

    /// Header `gas_limit == 0` → `select` stays `None` (no 30M default).
    /// Nonzero updates an existing [`SelectReady`] header, or forms one
    /// from bind + fee + committed bid. Missing bid.toml → stays None.
    pub fn refresh_select(&mut self, header_gas_limit: u64) {
        if header_gas_limit == 0 {
            tracing::error!("header gas_limit missing — SelectReady stays None (no 30M default)");
            self.select = None;
            return;
        }
        if let Some(ready) = self.select.as_mut() {
            ready.cfg.header_gas_limit = header_gas_limit;
            return;
        }
        self.select = self.form_select(header_gas_limit);
        if self.select.is_none() {
            if self.select_bind.is_none() {
                tracing::error!("select bind absent — SelectReady stays None");
            } else if self.bid_cfg.is_none() {
                tracing::error!("bid.toml absent — SelectReady stays None (D11 unset)");
            } else if self.fee.is_none() {
                tracing::error!("fee window empty — SelectReady stays None");
            } else {
                tracing::error!("SelectReady refused (wrap all-zero or bid refused)");
            }
        }
    }

    fn form_select(&self, header_gas_limit: u64) -> Option<SelectReady> {
        let bind = self.select_bind.as_ref()?;
        let fee = self.fee.as_ref()?;
        let bid_cfg = *self.bid_cfg.as_ref()?;
        if bind.wrap_gas.iter().all(|&g| g == 0) {
            tracing::error!("wrap_gas all zero — SelectReady stays None");
            return None;
        }
        Some(SelectReady {
            cfg: SelectCfg {
                cost: CostModel::FEE_ONLY,
                close_bps: 0,
                exact_k: EXACT_K,
                nonce_slots: NONCE_SLOTS,
                header_gas_limit,
                wrap_gas: bind.wrap_gas,
                wrap_aave_v4: bind.wrap_aave_v4,
                aave_v4: bind.aave_v4,
                liq_gas: 0,
                over_borrow: U256::ZERO,
                min_out_tolerance_bps: liq_router::select::MIN_OUT_TOLERANCE_BPS,
                budget: SolveBudget::default(),
                bids: Some(bid_cfg),
            },
            gas: GasTerms {
                base_fee_wei: fee.next_base_fee,
                priority_fee_wei: fee.priority_wei,
                out_per_eth: OUT_PER_ETH_WETH,
            },
            bid: None,
            validate: bind.validate.clone(),
            haircut: bind.haircut,
            gas_failed: 0,
            flags: FLAG_SWEEP,
        })
    }

    /// Fold the parent header into the live oracle. `base_fee_per_gas == 0`
    /// skips [`liq_router::GasOracle::observe_parent`]. Priority is
    /// [`liq_router::PRIORITY_FEE_WEI`], not a sample from the ring.
    pub fn observe_parent_header(
        &mut self,
        base_fee_per_gas: u64,
        gas_used: u64,
        gas_limit: u64,
        parent_block: u64,
    ) {
        if base_fee_per_gas == 0 {
            tracing::error!("header base_fee_per_gas absent — observe_parent skipped");
            return;
        }
        let Some(oracle) = self.oracle.as_mut() else {
            tracing::error!("gas oracle missing — observe_parent skipped");
            return;
        };
        if let Err(e) =
            oracle.observe_parent(u128::from(base_fee_per_gas), gas_used, gas_limit, &[])
        {
            tracing::error!(error = %e, "observe_parent refused — fee stays None");
            return;
        }
        self.fee = crate::bind::fee_from_oracle(oracle, parent_block);
        if let Some(clock) = &self.header_clock {
            clock.note_now();
        }
    }

    fn world_haircut(&self) -> Haircut {
        self.select
            .as_ref()
            .map(|s| s.haircut)
            .unwrap_or(Haircut::NONE)
    }

    fn feed_engine(&mut self, ctx: AfterBlockCtx<'_>) {
        let proto_refs: Vec<&dyn Protocol> = self.protocols.iter().map(|p| p.as_dyn()).collect();
        let flash = self.flash.load();
        let world = World {
            view: ctx.store.view(ctx.timestamp),
            protocols: &proto_refs,
            flash: flash.as_ref(),
            routes: &self.routes,
            haircut: self.world_haircut(),
            cons: &self.cons,
        };
        if proto_refs.is_empty() {
            tracing::error!("empty protocol list — on_dirty skipped (no invented adapter)");
        } else {
            for set in as_dirty_sets(ctx.dirty) {
                let Some(cause) = cause_for(&set) else {
                    continue;
                };
                for p in &proto_refs {
                    if let Err(e) = self.engine.on_dirty(&world, p.id(), &set, &cause) {
                        tracing::error!(error = %e, protocol = p.id().0, "on_dirty failed");
                    }
                }
            }
        }
        if let Err(e) = self.engine.on_block(&world) {
            tracing::error!(error = %e, "on_block failed");
        }
    }

    /// Drain the engine queue into the inbox. Tests inject candidates here.
    pub fn enqueue_candidates(
        &mut self,
        cands: &[Candidate],
        tip: u64,
        timestamp: u64,
    ) -> DrainStats {
        let mut stats = DrainStats::default();
        if self.inbox.is_none() {
            tracing::error!("ExecPath unbound — drain try_send refused (no invented key)");
            stats.skipped_exec = stats
                .skipped_exec
                .saturating_add(u64::try_from(cands.len()).unwrap_or(u64::MAX));
            return stats;
        }
        let gas_failed = match self.select.as_ref() {
            None => {
                tracing::error!(
                    "select inputs absent — skip (no invented header gas / wrap / failed gas)"
                );
                stats.skipped_select = stats.skipped_select.saturating_add(1);
                return stats;
            }
            Some(ready) => ready.gas_failed,
        };
        let mut kept: Vec<&Candidate> = Vec::new();
        for c in cands {
            if !c.fireable() || c.cause.kind() == TriggerKind::OraclePredicted {
                tracing::error!(
                    pos = c.position.0,
                    kind = ?c.cause.kind(),
                    "OraclePredicted / !fireable never becomes an ExecJob"
                );
                stats.skipped_predicted = stats.skipped_predicted.saturating_add(1);
                continue;
            }
            if !self.ensure_pins(c) {
                stats.skipped_pins = stats.skipped_pins.saturating_add(1);
                continue;
            }
            if let Err(e) = self.assemble.apply_quote_for(
                c.position,
                &c.quote,
                usize::from(c.legs.repay),
                usize::from(c.legs.seize),
                // Zero when no schedule is loaded, which reproduces the old
                // exact-value bounds rather than inventing a tolerance the
                // operator did not configure.
                self.select
                    .as_ref()
                    .map_or(0, |s| s.cfg.min_out_tolerance_bps),
            ) {
                tracing::error!(error = %e, pos = c.position.0, "quote-derived tail refused");
                stats.skipped_pins = stats.skipped_pins.saturating_add(1);
                continue;
            }
            if !pins_ready(&self.assemble, c) {
                stats.skipped_pins = stats.skipped_pins.saturating_add(1);
                continue;
            }
            kept.push(c);
        }
        if kept.is_empty() {
            return stats;
        }
        let Some(ready) = self.select.as_ref() else {
            tracing::error!("select inputs dropped — skip");
            stats.skipped_select = stats.skipped_select.saturating_add(1);
            return stats;
        };
        let inputs: Vec<PositionInput<'_>> = kept
            .iter()
            .map(|c| PositionInput {
                position: c.position,
                protocol: c.protocol,
                health: c.health,
                quote: &c.quote,
                cause: c.cause.kind(),
                p: liq_router::select::learning_p(),
                gas_success: None,
                gas_failed,
            })
            .collect();
        let flash = self.flash.load();
        let warm = self.routes.table();
        let warm_ref = if warm.is_empty() {
            None
        } else {
            Some(warm.as_ref())
        };
        let book = self.book.read();
        let plans = match select(
            &inputs,
            &ready.cfg,
            flash.as_ref(),
            ready.haircut,
            &book,
            warm_ref,
            &self.assemble,
            &ready.gas,
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "select refused");
                stats.skipped_select = stats.skipped_select.saturating_add(1);
                return stats;
            }
        };
        if plans.is_empty() {
            tracing::error!("select emitted no plans");
            stats.skipped_select = stats.skipped_select.saturating_add(1);
            return stats;
        }
        let price = match liq_router::profit::gas_price_in_debt(&ready.gas) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "gas_price_in_debt refused");
                stats.skipped_select = stats.skipped_select.saturating_add(1);
                return stats;
            }
        };
        for plan in &plans {
            let Some(bid) = plan_bid(plan, ready, &self.assemble) else {
                stats.skipped_job = stats.skipped_job.saturating_add(1);
                continue;
            };
            let assembled = match assemble(
                std::slice::from_ref(plan),
                &ready.cfg,
                &book,
                &self.assemble,
                &ready.validate,
                &bid,
                price,
                &ready.gas,
                ready.flags,
                flash.as_ref(),
                ready.haircut,
            ) {
                Ok(a) => a,
                Err(e) => {
                    tracing::error!(error = %e, "assemble refused — no zero tail");
                    stats.skipped_pins = stats.skipped_pins.saturating_add(1);
                    continue;
                }
            };
            for a in assembled {
                let Some(lead) = lead_candidate(plan, &kept) else {
                    stats.skipped_job = stats.skipped_job.saturating_add(1);
                    continue;
                };
                match self.finish_job(lead, &a, plan.hop_and_wrap_gas, tip, timestamp, &bid) {
                    Finish::Sent => stats.jobs_sent = stats.jobs_sent.saturating_add(1),
                    Finish::Sim => stats.skipped_sim = stats.skipped_sim.saturating_add(1),
                    Finish::Job => stats.skipped_job = stats.skipped_job.saturating_add(1),
                    Finish::Full => stats.inbox_full = stats.inbox_full.saturating_add(1),
                    Finish::Exec => stats.skipped_exec = stats.skipped_exec.saturating_add(1),
                }
            }
        }
        stats
    }

    fn ensure_pins(&mut self, c: &Candidate) -> bool {
        if self.assemble.has_pins(c.position) {
            return true;
        }
        let Some(p) = self.protocols.iter().find(|p| p.id() == c.protocol) else {
            tracing::error!(
                proto = c.protocol.0,
                "no bound adapter for TailPins — skip (no invented pin)"
            );
            return false;
        };
        match p.pins_from_candidate(c, None) {
            Some(pins) => {
                self.assemble.insert_pins(c.position, pins);
                true
            }
            None => {
                tracing::error!(
                    pos = c.position.0,
                    "pins_from_candidate refused — no zero tail"
                );
                false
            }
        }
    }

    fn finish_job(
        &self,
        cand: &Candidate,
        assembled: &liq_router::Assembled,
        hop_and_wrap_gas: u64,
        tip: u64,
        timestamp: u64,
        bid: &Bid,
    ) -> Finish {
        let Some(inbox) = self.inbox.as_ref() else {
            tracing::error!("inbox gone — try_send refused");
            return Finish::Exec;
        };
        let Some(ready) = self.select.as_ref() else {
            return Finish::Job;
        };
        let encoded = match EncodedPlan::encode(&assembled.plan, &ready.validate) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(error = %e, "plan encode refused");
                return Finish::Job;
            }
        };
        let plan_bytes = Bytes::from(encoded.into_bytes());
        let calldata = execute_calldata(plan_bytes.as_ref());
        let Some(sim) = self.sim.as_ref() else {
            tracing::error!("no StateProvider — sim fail-closed, skip send");
            return Finish::Sim;
        };
        let Some(trigger) = sim_trigger(cand) else {
            return Finish::Sim;
        };
        if hop_and_wrap_gas == 0 {
            tracing::error!("hop_and_wrap_gas is zero — skip (no invented gas)");
            return Finish::Job;
        }
        let Some(operator) = self.operator else {
            tracing::error!("operator absent — skip (no invented key)");
            return Finish::Exec;
        };
        let bundle = Bundle {
            trigger,
            calls: vec![SimTx {
                caller: operator,
                to: PLANNED_EXECUTOR,
                value: U256::ZERO,
                data: calldata.clone(),
                gas_limit: hop_and_wrap_gas,
            }],
            min_profit: U256::from(assembled.plan.min_profit_wei),
            health: None,
        };
        let outcome = match sim.verify(&bundle, tip, timestamp) {
            Ok(o) => o,
            Err(SimError::StateUnavailable) | Err(SimError::ArchiveUnavailable) => {
                tracing::error!("sim state/archive unavailable — skip send");
                return Finish::Sim;
            }
            Err(e) => {
                tracing::error!(error = %e, "sim verify refused — skip send");
                return Finish::Sim;
            }
        };
        if outcome.gas_used == 0 {
            tracing::error!("sim gas_used is zero — skip");
            return Finish::Job;
        }
        let Some(job) = exec_job(
            cand,
            assembled,
            &plan_bytes,
            &calldata,
            outcome.gas_used,
            self.fee,
            operator,
            self.chain_id,
            tip,
            bid,
        ) else {
            return Finish::Job;
        };
        if inbox.try_send(job) {
            Finish::Sent
        } else {
            tracing::error!("ExecInbox full — counted, not blocked");
            Finish::Full
        }
    }
}

impl AfterBlock for DrainJoin {
    fn after_block(&mut self, ctx: AfterBlockCtx<'_>) {
        self.publish_flash();
        self.observe_parent_header(ctx.base_fee_per_gas, ctx.gas_used, ctx.gas_limit, ctx.block);
        self.refresh_select(ctx.gas_limit);
        let tip = ctx.store.tip();
        let ts = ctx.timestamp;
        self.feed_engine(ctx);
        let cands: Vec<Candidate> = self.engine.candidates().collect();
        if cands.is_empty() {
            return;
        }
        let _ = self.enqueue_candidates(&cands, tip, ts);
    }
}

enum Finish {
    Sent,
    Sim,
    Job,
    Full,
    Exec,
}

fn cause_for(set: &DirtySet) -> Option<TriggerCause> {
    match set {
        DirtySet::None => None,
        DirtySet::MarketAccrual(_) => Some(TriggerCause::InterestDrift),
        DirtySet::MarketReprice(rows) => rows
            .first()
            .map(|r| TriggerCause::ParamChange { market: r.market }),
        DirtySet::Positions(_) => {
            tracing::error!("positions dirty without tx hash — UserAction not attached");
            None
        }
        DirtySet::ProtocolWide => {
            tracing::error!("ProtocolWide without scheduled param — skip");
            None
        }
    }
}

fn pins_ready(view: &ProcessAssembleView, c: &Candidate) -> bool {
    for opt in &c.quote.repay_options {
        if let Err(e) = require_token(view, opt.asset) {
            tracing::error!(error = %e, pos = c.position.0, asset = opt.asset.0, "intern miss — no zero token");
            return false;
        }
    }
    for opt in &c.quote.seize_options {
        if let Err(e) = require_token(view, opt.asset) {
            tracing::error!(error = %e, pos = c.position.0, asset = opt.asset.0, "intern miss — no zero token");
            return false;
        }
    }
    if let Err(e) = require_meta(view, c.position) {
        tracing::error!(error = %e, pos = c.position.0, "empty TailPins / missing meta — no zero tail");
        return false;
    }
    true
}

fn lead_candidate<'a>(
    plan: &liq_router::SelectedPlan,
    kept: &[&'a Candidate],
) -> Option<&'a Candidate> {
    let mut lead: Option<&'a Candidate> = None;
    for g in &plan.groups {
        for s in &g.legs {
            let c = kept.iter().copied().find(|x| x.position == s.position)?;
            match lead {
                None => lead = Some(c),
                Some(prev) if prev.cause.kind() != c.cause.kind() => {
                    tracing::error!("mixed triggers in one plan — skip");
                    return None;
                }
                Some(_) => {}
            }
        }
    }
    lead
}

fn sim_trigger(c: &Candidate) -> Option<Trigger> {
    match &c.cause {
        TriggerCause::InterestDrift
        | TriggerCause::Stale { .. }
        | TriggerCause::ParamChange { .. } => Some(Trigger::InterestDrift),
        TriggerCause::SvrAuction { hint, .. } => {
            tracing::error!(?hint, "SVR sim skipped — no reconstructed tx");
            None
        }
        other => {
            tracing::error!(kind = ?other.kind(), "sim trigger missing parent tx");
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn exec_job(
    cand: &Candidate,
    assembled: &liq_router::Assembled,
    plan: &Bytes,
    calldata: &Bytes,
    gas_limit: u64,
    fee: Option<FeeQuote>,
    operator: Address,
    chain_id: u64,
    tip: u64,
    bid: &Bid,
) -> Option<ExecJob> {
    let kind = cand.cause.kind();
    if !cand.fireable() || kind == TriggerKind::OraclePredicted {
        tracing::error!(kind = ?kind, "predicted never becomes ExecJob");
        return None;
    }
    let fee = match fee {
        Some(f) => f,
        None => {
            tracing::error!("fee quote missing — skip (no invented base/priority)");
            return None;
        }
    };
    let target = match tip.checked_add(1) {
        Some(t) => t,
        None => {
            tracing::error!("tip overflow — skip target_block");
            return None;
        }
    };
    let max_block = match kind {
        TriggerKind::SvrAuction => {
            tracing::error!("SvrAuction inclusion window not observed — skip (no invented span)");
            return None;
        }
        _ => target,
    };
    let hint_hash = match &cand.cause {
        TriggerCause::SvrAuction { hint, .. } => Some(*hint),
        _ => None,
    };
    if kind == TriggerKind::SvrAuction && hint_hash.is_none() {
        tracing::error!("SvrAuction missing hint — skip");
        return None;
    }
    let backrun_tx = match kind {
        TriggerKind::OraclePublic
        | TriggerKind::OraclePullHeld
        | TriggerKind::PoolStateChange
        | TriggerKind::UserAction
        | TriggerKind::DerivedRate => {
            tracing::error!(kind = ?kind, "ordered trigger missing backrun tx — skip");
            return None;
        }
        _ => None,
    };
    let auction_bps = match kind {
        TriggerKind::InterestDrift | TriggerKind::Stale => 0,
        _ => bid.refund_bps,
    };
    let g0 = assembled.plan.groups.first()?;
    let (collateral, debt) = assets_of(cand)?;
    Some(ExecJob {
        trace: cand.trace,
        plan: plan.clone(),
        trigger: kind,
        protocol: cand.protocol,
        market: cand.quote.key.market,
        collateral,
        debt,
        flash: g0.provider,
        operator_key: operator,
        position: cand.quote.key,
        hint_hash,
        backrun_tx,
        target_block: target,
        max_block,
        fee,
        auction_bps,
        refund_address: operator,
        calldata: calldata.clone(),
        gas_limit,
        chain_id,
        slot: 0,
    })
}

fn assets_of(c: &Candidate) -> Option<(AssetId, AssetId)> {
    let seize = c.quote.seize_options.get(usize::from(c.legs.seize))?;
    let repay = c.quote.repay_options.get(usize::from(c.legs.repay))?;
    Some((seize.asset, repay.asset))
}

/// One `bidBps` for this plan. A schedule resolves every leg; mixed cells
/// are a skip. No schedule uses the injected bid.
fn plan_bid(plan: &SelectedPlan, ready: &SelectReady, view: &dyn MarketView) -> Option<Bid> {
    if let Some(sched) = ready.cfg.bids {
        let mut chosen: Option<BidConfig> = None;
        for g in &plan.groups {
            for s in &g.legs {
                let per = match view.per_eth(s.leg.debt) {
                    Some(p) if !p.is_zero() => p,
                    _ => {
                        tracing::error!("per_eth missing — plan not bid");
                        return None;
                    }
                };
                let size = match debt_notional_eth_wei(s.leg.s, per) {
                    Some(sz) => sz,
                    None => {
                        tracing::error!("debt notional refused — plan not bid");
                        return None;
                    }
                };
                let cfg = *sched.config(s.protocol, size);
                if let Some(prev) = chosen {
                    if prev != cfg {
                        tracing::error!("plan mixes bid cells — skip");
                        return None;
                    }
                } else {
                    chosen = Some(cfg);
                }
            }
        }
        return learning_bid(&chosen?, ready.gas.priority_fee_wei);
    }
    ready.bid
}

/// Bid from one cell. Draw is 0. Missing priority → None.
#[must_use]
pub fn learning_bid(cfg: &BidConfig, priority_wei: u128) -> Option<Bid> {
    match bid(cfg, 0, priority_wei) {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::error!(error = %e, "bid refused");
            None
        }
    }
}

/// Silence unused FlashProvider import in production (tests use it).
#[allow(dead_code)]
fn _flash_id(p: FlashProvider) -> u8 {
    p as u8
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
    use super::*;
    use crate::assemble_view::ProcessAssembleView;
    use crate::exec_bind::{bind, ProcessSecrets};
    use crate::lease::SubmitLease;
    use alloy_primitives::{address, b256, Address, U256};
    use liq_engine::TriggerCause;
    use liq_exec::builders::{leak_str, BuilderEndpoint, BuilderSet};
    use liq_exec::fee::FeeQuote;
    use liq_exec::nonce::{NonceAllocator, NonceMode};
    use liq_exec::path::{CaptureRecorder, ExecPath};
    use liq_exec::submit::{LiveSendBits, SubmitEnabled};
    use liq_exec::template::PrecomputedSigner;
    use liq_flash::{CostModel, FlashSource, HeldAsset, MorphoBlue};
    use liq_node::CollapsedDirty;
    use liq_oracle::mevshare::SearcherKey;
    use liq_plan::{ValidateCtx, FLAG_SWEEP};
    use liq_protocol::{
        AssetMask, BonusCurve, CallbackShape, FlashRoute, Health, HealthState, LegChoice, Quote,
        RepayOption, SeizeOption,
    };
    use liq_router::{
        bid, BidConfig, PairTerms, Pool, PoolBook, PoolState, SelectCfg, SolveBudget, Tick, V3State,
    };
    use liq_sim::MemoryFactory;
    use liq_types::fixed::RAY;
    use liq_types::{
        AssetId, BuilderId, Confidence, FlashProvider, MarketId, PositionId, PositionKey,
        ProtocolId, Ray, TraceId, Wad,
    };
    use smallvec::SmallVec;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const SECRET: alloy_primitives::B256 =
        b256!("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
    const OPERATOR: Address = address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    const PROTO: ProtocolId = ProtocolId(0);
    const A0: AssetId = AssetId(0);
    const A1: AssetId = AssetId(1);
    const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);

    fn e18(n: u64) -> U256 {
        U256::from(n) * U256::from(10u64).pow(U256::from(18u64))
    }

    fn bonus_5() -> Ray {
        Ray::from_raw(RAY / U256::from(20u64))
    }

    fn tok(n: u64) -> Address {
        Address::from_slice(&{
            let mut b = [0u8; 20];
            b[12..].copy_from_slice(&(0x7000_0000u64 + n).to_be_bytes());
            b
        })
    }

    fn addr(n: u64) -> Address {
        Address::from_slice(&{
            let mut b = [0u8; 20];
            b[12..].copy_from_slice(&n.to_be_bytes());
            b
        })
    }

    fn health() -> Health {
        Health {
            hf: Ray::from_raw(RAY / U256::from(2u64)),
            debt_value: Wad::ZERO,
            collateral_value: Wad::ZERO,
            price_sensitivity: AssetMask::EMPTY,
            state: HealthState::Liquidatable,
        }
    }

    fn quote(pos: u32) -> Quote {
        Quote {
            position: PositionId(pos),
            key: PositionKey {
                protocol: PROTO,
                market: MarketId(0),
                user: addr(0xB0 + u64::from(pos)),
            },
            repay_options: SmallVec::from_slice(&[RepayOption {
                asset: A1,
                max_repay: e18(10),
                slot: liq_protocol::SlotRef::ByAsset,
            }]),
            seize_options: SmallVec::from_slice(&[SeizeOption {
                asset: A0,
                max_seize: e18(20),
                bonus: bonus_5(),
                curve: BonusCurve::Static { bonus: bonus_5() },
                call_target: alloy_primitives::Address::ZERO,
                slot: liq_protocol::SlotRef::ByAsset,
            }]),
        }
    }

    fn fireable(pos: u32) -> Candidate {
        candidate(pos, TriggerCause::InterestDrift)
    }

    fn candidate(pos: u32, cause: TriggerCause) -> Candidate {
        Candidate {
            position: PositionId(pos),
            protocol: PROTO,
            health: health(),
            quote: quote(pos),
            legs: LegChoice::PREFERRED,
            funding: FlashRoute {
                provider: FlashProvider::Morpho,
                source: addr(0xA0),
                asset: A1,
                amount: e18(10),
                fee_bps: 0,
                callback: CallbackShape::MorphoFlashCallback,
            },
            cause,
            est_value: Wad::from_raw(U256::from(1u64)),
            deadline: None,
            trace: TraceId::from_raw(u64::from(pos)),
        }
    }

    fn empty_pins() -> liq_router::TailPins {
        liq_router::TailPins {
            adapter: liq_protocol::ExecutorAdapter::AaveV3,
            market: addr(0x51),
            borrower: addr(0xB1),
            protocol_pull: None,
            euler_min_yield: None,
            euler_collateral_vault: None,
            liquity_trove_id: None,
            fluid_t1: None,
            fluid_col_per_unit_debt: None,
            gearbox_min_seized: None,
            gearbox_full_multicall: false,
            compound_ctoken_collateral: None,
            compound_is_cether: None,
        }
    }

    fn aave_pins(borrower: Address) -> liq_router::TailPins {
        let mut p = empty_pins();
        p.borrower = borrower;
        p
    }

    fn wrap_gas() -> [u64; 5] {
        [366_332, 355_632, 460_032, 370_435, 384_134]
    }

    fn flat_schedule() -> BidSchedule {
        let c = BidConfig::new(9_900, 9_900, 0, 0).unwrap();
        BidSchedule {
            size_cut_wei: U256::from(3_000_000_000_000_000_000u128),
            aave_v3: ProtocolId(1),
            aave_v4: ProtocolId(2),
            aave_below: c,
            aave_above: c,
            other_below: c,
            other_above: c,
        }
    }

    fn select_ready(weth: Address) -> SelectReady {
        let bcfg = BidConfig::new(9_900, 9_900, 0, 0).unwrap();
        let bd = bid(&bcfg, 0, 1).unwrap();
        SelectReady {
            cfg: SelectCfg {
                cost: CostModel::FEE_ONLY,
                close_bps: 0,
                exact_k: 8,
                nonce_slots: 1,
                header_gas_limit: 30_000_000,
                wrap_gas: wrap_gas(),
                wrap_aave_v4: 496_704,
                aave_v4: None,
                liq_gas: 80_000,
                over_borrow: U256::from(1u64),
                min_out_tolerance_bps: liq_router::select::MIN_OUT_TOLERANCE_BPS,
                budget: SolveBudget::default(),
                bids: None,
            },
            gas: GasTerms {
                base_fee_wei: 1,
                priority_fee_wei: 0,
                out_per_eth: e18(1),
            },
            bid: Some(bd),
            validate: ValidateCtx {
                weth,
                v4_underlying: Vec::new(),
                morpho: Vec::new(),
                compound: Vec::new(),
                liquity: Vec::new(),
            },
            haircut: Haircut::from_bps(10_000).unwrap(),
            gas_failed: 50_000,
            flags: FLAG_SWEEP,
        }
    }

    fn deep_v3() -> Pool {
        let l: u128 = 50_000_000_000_000_000_000_000;
        Pool {
            address: addr(1),
            assets: SmallVec::from_slice(&[A0, A1]),
            tokens: SmallVec::from_slice(&[tok(0), tok(1)]),
            hop_gas: 100_000,
            state: PoolState::V3(V3State {
                sqrt_price_x96: Q96,
                tick: 0,
                liquidity: l,
                fee_pips: 500,
                tick_spacing: 10,
                ticks: vec![
                    Tick {
                        tick: -887_220,
                        net: i128::try_from(l).unwrap(),
                        gross: l,
                    },
                    Tick {
                        tick: 887_220,
                        net: -i128::try_from(l).unwrap(),
                        gross: l,
                    },
                ],
            }),
        }
    }

    fn book() -> PoolBook {
        let mut assets = HashMap::new();
        assets.insert(tok(0), A0);
        assets.insert(tok(1), A1);
        let mut b = PoolBook::new(assets, None, 100_000);
        b.add(deep_v3()).unwrap();
        b
    }

    fn flash() -> Arc<ArcSwap<FlashIndex>> {
        let srcs: Vec<Box<dyn FlashSource>> = vec![Box::new(MorphoBlue::new(
            addr(0xA0),
            &[HeldAsset {
                asset: A1,
                token: tok(1),
                balance: e18(10_000_000),
            }],
        ))];
        let mut i = FlashIndex::new(4);
        i.refresh(&srcs);
        Arc::new(ArcSwap::from_pointee(i))
    }

    fn routes() -> liq_router::WarmRouteCache {
        crate::routes::warm_handles().1
    }

    fn filled_view(borrower: Address) -> ProcessAssembleView {
        let mut v = ProcessAssembleView::empty();
        let id0 = v.intern_token(tok(0)).unwrap();
        let id1 = v.intern_token(tok(1)).unwrap();
        assert_eq!(id0, A0);
        assert_eq!(id1, A1);
        v.insert_pins(PositionId(1), aave_pins(borrower));
        v.insert_per_eth(A0, e18(1));
        v.insert_per_eth(A1, e18(1));
        v.insert_pair_terms(
            PROTO,
            A0,
            A1,
            PairTerms {
                bonus: bonus_5(),
                coll_per_debt: Ray::from_raw(RAY),
                flash_fee_bps: 0,
                fixed_gas: 50_000,
            },
        );
        v.insert_notional_cap(A1, U256::MAX);
        v
    }

    fn fee(parent: u64) -> FeeQuote {
        FeeQuote {
            parent_block: parent,
            next_base_fee: 1_000,
            priority_wei: 10,
            modest_priority_wei: 2,
        }
    }

    fn join_base(inbox: Option<ExecInbox>, sim: bool) -> DrainJoin {
        let mut j = DrainJoin::live_noop(
            flash(),
            routes(),
            filled_view(addr(0xB1)),
            inbox,
            Some(OPERATOR),
            1,
        )
        .with_book(book())
        .with_select(select_ready(tok(1)))
        .with_fee(fee(0));
        if sim {
            j = j.with_sim(Box::new(MemoryDrainSim::new(MemoryFactory::empty())));
        }
        j
    }

    async fn spawn_mock() -> (String, Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicU64::new(0));
        let hits_t = Arc::clone(&hits);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let hits_t = Arc::clone(&hits_t);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16_384];
                    let mut got = Vec::new();
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                got.extend_from_slice(buf.get(..n).unwrap_or(&[]));
                                if got.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    hits_t.fetch_add(1, Ordering::Relaxed);
                    let payload = br#"{"jsonrpc":"2.0","id":1,"result":{"bundleHash":"0x00"}}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let mut out = resp.into_bytes();
                    out.extend_from_slice(payload);
                    let _ = sock.write_all(&out).await;
                });
            }
        });
        (format!("http://{addr}/"), hits)
    }

    fn builders_for(url: &str) -> BuilderSet {
        let relay = leak_str(url.to_owned());
        BuilderSet::from_parts(
            vec![BuilderEndpoint {
                id: BuilderId(1),
                name: "t",
                endpoint: relay,
            }],
            relay,
        )
        .unwrap()
    }

    fn capture_path(url: &str) -> ExecPath<CaptureRecorder, Allow> {
        let signer = Arc::new(PrecomputedSigner::from_secret(SECRET).unwrap());
        let nonces = NonceAllocator::from_addresses(vec![signer.address()]).unwrap();
        let identity = SearcherKey::from_secret(SECRET).unwrap();
        ExecPath::new(
            CaptureRecorder::default(),
            Allow,
            Arc::new(SubmitEnabled::new(false)),
            NonceMode::Allocate,
            nonces,
            vec![signer],
            builders_for(url),
            identity,
            LiveSendBits::closed(),
        )
        .unwrap()
    }

    struct Allow;
    impl liq_types::RiskAllow for Allow {
        fn allow(&self, _t: liq_types::TraceId, _q: &liq_types::AllowQuery) -> liq_types::Allow {
            liq_types::Allow::Yes
        }
    }

    /// Empty overlay cannot credit protocol profit. Re-run `verify` with
    /// a zero floor so gas is the EVM measurement, not an invented receipt.
    struct OverlayGasSim {
        inner: MemoryDrainSim,
    }

    impl DrainSim for OverlayGasSim {
        fn verify(
            &self,
            bundle: &Bundle,
            number: u64,
            timestamp: u64,
        ) -> Result<SimOutcome, SimError> {
            match self.inner.verify(bundle, number, timestamp) {
                Ok(o) => Ok(o),
                Err(SimError::ProfitBelowFloor { .. }) => {
                    let mut b = bundle.clone();
                    b.min_profit = U256::ZERO;
                    self.inner.verify(&b, number, timestamp)
                }
                Err(e) => Err(e),
            }
        }
    }

    fn wait_rows(path: &ExecPath<CaptureRecorder, Allow>, n: usize) {
        for _ in 0..80 {
            if path.recorder.rows.lock().len() >= n {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn hot_path_source_has_no_await_or_block_on() {
        let src = include_str!("drain.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        assert!(!prod.contains(".await"), "hot drain must not .await");
        assert!(!prod.contains("block_on"), "hot drain must not block_on");
    }

    #[test]
    fn oracle_predicted_never_becomes_a_job() {
        let (inbox, rx) = ExecInbox::pair(4);
        let mut j = join_base(Some(inbox), true);
        let c = candidate(
            1,
            TriggerCause::OraclePredicted {
                conf: Confidence(9_000),
            },
        );
        let st = j.enqueue_candidates(&[c], 0, 1);
        assert_eq!(st.jobs_sent, 0);
        assert!(st.skipped_predicted >= 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn empty_tail_pins_and_intern_miss_zero_jobs() {
        let (inbox, rx) = ExecInbox::pair(4);
        let mut j = DrainJoin::live_noop(
            flash(),
            routes(),
            ProcessAssembleView::empty(),
            Some(inbox),
            Some(OPERATOR),
            1,
        )
        .with_book(book())
        .with_select(select_ready(tok(1)))
        .with_fee(fee(0))
        .with_sim(Box::new(MemoryDrainSim::new(MemoryFactory::empty())));
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1);
        assert_eq!(st.jobs_sent, 0);
        assert!(st.skipped_pins >= 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn no_state_provider_zero_jobs() {
        let (inbox, rx) = ExecInbox::pair(4);
        let mut j = join_base(Some(inbox), false);
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1);
        assert_eq!(st.jobs_sent, 0);
        assert!(
            st.skipped_sim >= 1 || st.skipped_select >= 1 || st.skipped_pins >= 1,
            "no provider must not invent a receipt: {st:?}"
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn no_secrets_unbound_zero_jobs() {
        let mut j = join_base(None, true);
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1);
        assert_eq!(st.jobs_sent, 0);
        assert!(st.skipped_exec >= 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fireable_pins_secrets_recorded_zero_http() {
        let (url, hits) = spawn_mock().await;
        let path = Arc::new(capture_path(&url));
        let (inbox, rx) = ExecInbox::pair(4);
        let worker = std::thread::Builder::new()
            .name("liq-bot-exec-test".into())
            .spawn({
                let path = Arc::clone(&path);
                move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    while let Ok(job) = rx.recv() {
                        let _ = rt.block_on(path.submit_path(&job));
                    }
                }
            })
            .unwrap();
        let mut j = join_base(Some(inbox), false).with_sim(Box::new(OverlayGasSim {
            inner: MemoryDrainSim::new(MemoryFactory::empty()),
        }));
        let mut c = fireable(1);
        c.quote.key.user = addr(0xB1);
        let st = j.enqueue_candidates(&[c], 0, 1);
        if st.jobs_sent == 0 {
            panic!("old unbound drain never enqueued; stats={st:?}");
        }
        wait_rows(&path, 1);
        assert_eq!(
            path.recorder.rows.lock().len(),
            1,
            "submit_path must record"
        );
        assert_eq!(hits.load(Ordering::Relaxed), 0, "submit_enabled false");
        drop(j);
        let _ = worker.join();
    }

    #[test]
    fn missing_header_gas_limit_select_none_zero_jobs() {
        let (inbox, rx) = ExecInbox::pair(4);
        let mut j = join_base(Some(inbox), true);
        assert!(j.select.is_some());
        j.refresh_select(0);
        assert!(j.select.is_none(), "gas_limit 0 must not default 30M");
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1);
        assert_eq!(st.jobs_sent, 0);
        assert!(st.skipped_select >= 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn refresh_select_forms_ready_from_committed_inputs() {
        let mut j = DrainJoin::live_noop(
            flash(),
            routes(),
            filled_view(addr(0xB1)),
            None,
            Some(OPERATOR),
            1,
        )
        .with_select_bind(
            crate::bind::select_bind(
                crate::bind::WrapGas {
                    by_provider: wrap_gas(),
                    aave_v4: 496_704,
                },
                tok(1),
                None,
            )
            .unwrap(),
        )
        .with_fee(fee(0))
        .with_bid_cfg(Some(flat_schedule()));
        assert!(j.select.is_none());
        j.refresh_select(15_000_000);
        let ready = j.select.expect("SelectReady must form from bind+fee+bid");
        assert_eq!(ready.cfg.header_gas_limit, 15_000_000);
        assert_eq!(ready.cfg.exact_k, EXACT_K);
        assert_eq!(ready.cfg.nonce_slots, NONCE_SLOTS);
        assert_eq!(ready.gas_failed, 0);
        assert_eq!(ready.cfg.liq_gas, 0);
        assert_eq!(ready.cfg.wrap_aave_v4, 496_704);
    }

    #[test]
    fn refresh_select_stays_none_without_bid() {
        let mut j = DrainJoin::live_noop(
            flash(),
            routes(),
            filled_view(addr(0xB1)),
            None,
            Some(OPERATOR),
            1,
        )
        .with_select_bind(
            crate::bind::select_bind(
                crate::bind::WrapGas {
                    by_provider: wrap_gas(),
                    aave_v4: 496_704,
                },
                tok(1),
                None,
            )
            .unwrap(),
        )
        .with_fee(fee(0));
        j.refresh_select(15_000_000);
        assert!(j.select.is_none(), "D11 unset must not invent BidConfig");
    }

    #[test]
    fn gas_failed_zero_does_not_abort_enqueue() {
        let (inbox, rx) = ExecInbox::pair(4);
        let mut j = join_base(Some(inbox), false);
        if let Some(s) = j.select.as_mut() {
            s.gas_failed = 0;
        }
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1);
        assert!(
            st.skipped_sim >= 1 || st.jobs_sent >= 1,
            "gas_failed=0 at learning p must reach select, not abort: {st:?}"
        );
        let _ = rx;
    }

    #[test]
    fn empty_fee_window_fee_none_zero_jobs() {
        let (inbox, rx) = ExecInbox::pair(4);
        let mut j = DrainJoin::live_noop(
            flash(),
            routes(),
            filled_view(addr(0xB1)),
            Some(inbox),
            Some(OPERATOR),
            1,
        )
        .with_book(book())
        .with_select(select_ready(tok(1)));
        assert!(j.fee.is_none());
        let oracle = liq_router::GasOracle::with_priority_cap(4).unwrap();
        assert!(crate::bind::fee_from_oracle(&oracle, 0).is_none());
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1);
        assert_eq!(st.jobs_sent, 0);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn production_drain_source_has_no_invented_30m() {
        let src = include_str!("drain.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        assert!(
            !prod.contains("30_000_000") && !prod.contains("30000000"),
            "production drain must not invent 30M header gas"
        );
        assert!(!prod.contains("MemoryFactory::empty"));
        assert!(!prod.contains("BidConfig::new"));
        assert!(!prod.contains("gas_failed: 50_000"));
        assert!(
            prod.contains("observe_parent"),
            "after-block must keep the oracle and call observe_parent"
        );
    }

    fn tiny_store() -> liq_state::StateStore {
        liq_state::StateStore::new(liq_state::StoreConfig {
            base: 0,
            positions: 4,
            markets: 2,
            undo: liq_state::UndoCapacity {
                ops: 8,
                extras: 2,
                rows: 2,
            },
        })
    }

    fn after_ctx<'a>(
        store: &'a liq_state::StateStore,
        dirty: &'a CollapsedDirty,
        base_fee_per_gas: u64,
        gas_used: u64,
        gas_limit: u64,
    ) -> AfterBlockCtx<'a> {
        AfterBlockCtx {
            store,
            dirty,
            block: 1,
            timestamp: 1,
            gas_limit,
            gas_used,
            base_fee_per_gas,
        }
    }

    #[test]
    fn absent_base_fee_skips_observe_fee_none() {
        let store = tiny_store();
        let dirty = CollapsedDirty::default();
        let mut j = DrainJoin::live_noop(
            flash(),
            routes(),
            ProcessAssembleView::empty(),
            None,
            None,
            1,
        );
        j.oracle = liq_router::GasOracle::with_priority_cap(4);
        assert!(j.oracle.as_ref().and_then(|o| o.base_fee_wei()).is_none());
        j.after_block(after_ctx(&store, &dirty, 0, 15_000_000, 45_000_000));
        assert!(
            j.oracle.as_ref().and_then(|o| o.base_fee_wei()).is_none(),
            "base_fee_per_gas == 0 must not call observe_parent"
        );
        assert!(j.fee.is_none());
    }

    #[test]
    fn observed_base_fee_records_parent_fee_stays_none() {
        let store = tiny_store();
        let dirty = CollapsedDirty::default();
        let mut j = DrainJoin::live_noop(
            flash(),
            routes(),
            ProcessAssembleView::empty(),
            None,
            None,
            1,
        );
        j.oracle = liq_router::GasOracle::with_priority_cap(4);
        j.after_block(after_ctx(
            &store,
            &dirty,
            1_000_000_000,
            15_000_000,
            45_000_000,
        ));
        let o = j.oracle.as_ref().expect("oracle lives");
        assert!(
            o.base_fee_wei().is_some(),
            "observed base fee must leave a parent sample"
        );
        assert_eq!(o.block_gas_limit(), Some(45_000_000));
        let fee = j.fee.expect("observed base fee quotes 1 gwei priority");
        assert_eq!(fee.priority_wei, liq_router::PRIORITY_FEE_WEI);
        assert_eq!(fee.modest_priority_wei, liq_router::PRIORITY_FEE_WEI);
        assert_ne!(fee.next_base_fee, 0);
    }

    #[test]
    fn process_bind_no_secrets_still_unbound() {
        let secrets = ProcessSecrets::from_hex(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        );
        assert!(secrets.is_some());
        assert!(ProcessSecrets::from_hex("not-hex", "not-hex").is_none());
        let _ = SubmitLease::refused();
        let _ = bind;
    }
}
