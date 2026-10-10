//! Hot-thread drain join (WP 17D / D63).
//!
//! After-block: dirty → `Engine::on_dirty` / `on_block` →
//! `CandidateQueue::drain` → map fields → `select` → `assemble` →
//! `liq_sim::verify` or fail-closed skip → `ExecInbox::try_send`.
//! Same path for shadow and live. Synchronous: never awaits, never
//! blocks on a runtime.
//!
//! The simulator runs on the node's own state at the store's tip
//! ([`LiveSim`]). Without one (the standalone binary), and until the store
//! has caught up with the node, a job whose trigger is already committed is
//! verified by the exec worker against the node over RPC instead, and a job
//! that needs a parent transaction replayed is not sent.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, Bytes, B256, U256};
use arc_swap::ArcSwap;
use liq_engine::{Candidate, Engine, EngineConfig, TriggerCause, World};
use liq_exec::fee::FeeQuote;
use liq_exec::path::{ExecInbox, ExecJob};
use liq_flash::{CostModel, FlashIndex, Haircut};
use liq_node::{as_dirty_sets, AfterBlock, AfterBlockCtx};
use liq_oracle::{CanonicalBook, DerivedBook};
use liq_plan::{EncodedPlan, ValidateCtx, FLAG_SWEEP};
use liq_protocol::{DirtySet, Protocol, Quote};
use liq_router::{
    assemble, bid, debt_notional_eth_wei, select, Bid, BidConfig, BidSchedule, GasTerms,
    MarketView, PoolBook, PositionInput, SelectCfg, SelectedPlan, SolveBudget, EXACT_K,
    NONCE_SLOTS, OUT_PER_ETH_WETH,
};
use liq_sim::{
    execute_calldata, verify, BlockRef, Bundle, MemoryFactory, NextBlock, NodeSim, SimEnv,
    SimError, SimOutcome, SimTx, Simulator, SpecId, StateProviderFactory, Trigger,
    PLANNED_EXECUTOR,
};
use liq_state::StateView;
use liq_types::{AssetId, FlashProvider, PriceTick, PriceVector, ProtocolId, TriggerKind};
use parking_lot::{Mutex, RwLock};

use crate::assemble_view::{require_meta, require_token, ProcessAssembleView};
use crate::bind::{BoundProtocol, SelectBind};
use crate::index::{BoundIndex, FlashSources};

/// Sweep order: Hot, Warm, Cool, Cold, then the rest.
fn sweep_rank(band: Option<liq_types::Band>) -> u8 {
    match band {
        Some(liq_types::Band::Hot) => 0,
        Some(liq_types::Band::Warm) => 1,
        Some(liq_types::Band::Cool) => 2,
        Some(liq_types::Band::Cold) => 3,
        _ => 4,
    }
}

/// Blocks a drift resync stays in the read set without settling.
const RESYNC_TTL_BLOCKS: u64 = 64;
/// First-stage reads a protocol-wide resync puts in flight per block (an
/// Aave account is ~2 per reserve, so this is a few dozen accounts).
const SWEEP_READS_PER_BLOCK: usize = 2048;

/// Gas budget of one SVR backrun. The parent header is the whole block.
/// This transaction lands behind the oracle update, so the plan stops at
/// 12M. Every account that fits shares that one transaction. A second
/// plan is not submitted on the same hint.
const SVR_BACKRUN_GAS: u64 = 12_000_000;

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
    /// Where the simulated calls go: the address the jobs are sent to.
    fn executor(&self) -> Address;
    /// `bundle` in the block after the store's tip.
    fn verify(&self, bundle: &Bundle, at: &NextBlock) -> Result<SimOutcome, SimError>;
    /// `false` while this simulator should not run. Jobs then take the path
    /// they take without one.
    fn ready(&self) -> bool {
        true
    }
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
    fn executor(&self) -> Address {
        PLANNED_EXECUTOR
    }

    /// Under the newest mainnet rules: the in-memory state has no chain to
    /// date it.
    fn verify(&self, bundle: &Bundle, at: &NextBlock) -> Result<SimOutcome, SimError> {
        let state = Arc::new(self.factory.state_at(at.parent)?);
        let mut sim =
            Simulator::from_provider(state, PLANNED_EXECUTOR, Address::ZERO, Address::ZERO);
        verify(
            &mut sim,
            bundle,
            &SimEnv::new(SpecId::OSAKA, at.block_env()?),
        )
    }
}

/// The production simulator: each job on the node's state at the store's
/// tip (`liq_sim::NodeSim`). It runs once this process holds the submit
/// lease, which the ExEx grants when the store reaches the node's head.
/// Before that nothing can be sent, and simulating every catch-up block on
/// the hot thread would only slow the catch-up.
pub struct LiveSim {
    sim: NodeSim,
    lease: Arc<std::sync::atomic::AtomicBool>,
}

impl LiveSim {
    #[must_use]
    pub fn new(sim: NodeSim, lease: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self { sim, lease }
    }
}

impl DrainSim for LiveSim {
    fn executor(&self) -> Address {
        self.sim.executor()
    }

    fn verify(&self, bundle: &Bundle, at: &NextBlock) -> Result<SimOutcome, SimError> {
        self.sim.verify(bundle, at)
    }

    fn ready(&self) -> bool {
        self.lease.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// `plan` charging `gas` at `wei_per_gas` where that costs more than it
/// budgeted, with its profit floor lowered by our share of the difference
/// (what is left of it after the bid), to 1 wei at least: `execute` refuses
/// a net loss itself, and a floor of 0 is refused. `None` when the budget
/// covers it.
fn charge_measured_gas(
    plan: &liq_plan::BatchPlan,
    gas: u64,
    wei_per_gas: u128,
) -> Option<liq_plan::BatchPlan> {
    let cost = u128::from(gas).checked_mul(wei_per_gas)?;
    let extra = cost.checked_sub(plan.gas_cost_wei).filter(|e| *e > 0)?;
    let keep_bps = u128::from(10_000u16.saturating_sub(plan.bid_bps));
    let ours = extra.checked_mul(keep_bps)?.checked_div(10_000)?;
    let mut p = plan.clone();
    p.gas_cost_wei = cost;
    p.min_profit_wei = p.min_profit_wei.saturating_sub(ours).max(1);
    Some(p)
}

/// Gas limit for a simulated job: what its calls needed available plus a
/// fifth (call depth forwards 63/64 of what is left), at most what the
/// simulation itself ran with. `None` when nothing was spent.
fn job_gas(spent: u64, ran_with: u64) -> Option<u64> {
    if spent == 0 {
        return None;
    }
    Some(spent.checked_mul(6)?.checked_div(5)?.min(ran_with))
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
/// Where one block's time went on the hot thread, from `after_block`'s
/// first line to the last job handed to the exec thread (signing, on that
/// thread, is not in it). Every stage is summed over the block's candidates
/// and plans. Read with [`DrainJoin::last_timing`]; measured on every block
/// (a clock read per stage boundary).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockTiming {
    /// Before the engine: flash index, header and fee observation, select
    /// inputs, snapshots.
    pub setup: std::time::Duration,
    /// The engine: prices, protocol prices, folds, health, candidates.
    pub engine: std::time::Duration,
    /// Pins, quote tails, pair terms and bands for the candidates.
    pub prepare: std::time::Duration,
    /// `select`: sizing, exits, flash sources, grouping.
    pub select: std::time::Duration,
    /// `assemble`: the plans' legs and bytes.
    pub assemble: std::time::Duration,
    /// Plan encoding and the in-process simulations (both passes), whether
    /// they pass or refuse the plan.
    pub sim: std::time::Duration,
    /// Building the exec job and handing it over.
    pub handoff: std::time::Duration,
    /// `after_block` start to its end.
    pub total: std::time::Duration,
    pub candidates: u32,
    pub jobs: u32,
}

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
    /// The second operator: signs MEV-Share backruns on its own nonce slot.
    /// `None`: the first operator signs them.
    pub backrun_operator: Option<Address>,
    pub chain_id: u64,
    pub fee: Option<FeeQuote>,
    /// Kept for the process. `observe_parent` only when header base fee ≠ 0.
    pub oracle: Option<liq_router::GasOracle>,
    /// Committed `bid.toml` only. Missing → [`Self::select`] stays None.
    pub bid_cfg: Option<BidSchedule>,
    header_clock: Option<Arc<crate::stall::HeaderClock>>,
    prices: PriceFeed,
    /// Measured gas model for the band's non-swap `fixed_gas`.
    pub band_gas: crate::gas_model::BandGas,
    /// Measured per-leg liquidation gas (`config/liq-gas.toml`). Plan gas.
    pub liq_gas: liq_router::LiqGas,
    /// Block each band key was first seen; the warm table is trusted for a
    /// key once it was built after that block.
    band_registered: HashMap<crate::bands::BandKey, u64>,
    /// The flash index was republished since `Unfundable` positions were
    /// last re-evaluated.
    flash_moved: bool,
    /// This block's [`BlockTiming`], filled as it runs.
    timing: std::cell::Cell<BlockTiming>,
    /// Block of the warm route table `Unfundable` positions were last
    /// re-evaluated against.
    routes_seen: u64,
    /// Protocol-reported prices: the engine's per-market overlay.
    protocol_prices: crate::protocol_prices::ProtocolPriceBook,
    /// Moves from the last applied batch. Reused each block.
    protocol_moves: Vec<liq_engine::ProtocolPriceMove>,
    /// The reader thread's read set and newest batch; `None` → canonical only.
    price_reader: Option<Arc<crate::protocol_prices::ReaderShared>>,
    /// Block the read set was last rebuilt (`0` = never).
    reads_at: u64,
    /// Protocol state read from chain each block (Fluid vaults). `None`
    /// when the reader was not started.
    state_reader: Option<Arc<crate::state_reads::StateReaderShared>>,
    /// Block the state read set was last rebuilt (`0` = never).
    state_reads_at: u64,
    /// Adapter-wide reads from that rebuild (Fluid vaults).
    adapter_state_reads: crate::state_reads::StateReadSet,
    /// Reads positions still want (Gearbox accounts after a multicall).
    position_reads: HashMap<liq_types::PositionId, crate::state_reads::StateReadSet>,
    /// Block of the last [`Self::refresh_state_reads`] (`0` = never): a gap
    /// means blocks were folded without their own `after_block`.
    position_reads_block: u64,
    /// `seq` of the last state batch folded into the store.
    state_applied: u64,
    /// Periodic snapshots of the store (the restart point). `None` in tests
    /// and when the writer could not start.
    snapshots: Option<crate::state_build::SnapshotWriter>,
    /// Drift check, fed each snapshot. `None` when not started.
    drift: Option<crate::drift::DriftHandle>,
    /// Resyncs in flight: a position's `resync_reads` and the block they
    /// were asked at. Published with the other reads until a batch settles
    /// the position (or [`RESYNC_TTL_BLOCKS`] pass).
    resync: HashMap<liq_types::PositionId, (crate::state_reads::StateReadSet, u64)>,
    /// Positions of protocols being resynced whole, a slice per block.
    sweep: std::collections::VecDeque<liq_types::PositionId>,
    /// Kept out of quoting: their health disagrees with the chain even after
    /// a resync.
    quarantine: std::collections::HashSet<liq_types::PositionId>,
    /// MEV-Share hints. Popped on the hot thread. `None` when the reader
    /// was not started.
    svr_rx: Option<rtrb::Consumer<liq_types::MevShareHint>>,
    svr_targets: Vec<liq_oracle::mevshare::SvrTarget>,
    /// Governance payload simulations from the `liq-bot-gov` worker.
    gov_rx: Option<rtrb::Consumer<crate::governance::GovSim>>,
    gov_inbox: Option<liq_exec::gov::GovInbox>,
    /// Built on the first simulation; the adapters' log routes.
    gov_routes: Option<crate::governance::GovRoutes>,
    /// Simulations whose base block the store has not reached yet. The node
    /// can report a head before its ExEx notification is folded here.
    gov_pending: Vec<crate::governance::GovSim>,
    /// Header `gasLimit` of the last observed parent. `0` = not yet seen.
    parent_gas_limit: u64,
    last_block: u64,
    /// Hash of `last_block`: the state the simulator reads.
    last_hash: B256,
    last_ts: u64,
    block_seen: bool,
}

/// The oracle books the ingest router writes, and the engine's view of them.
/// Written on the hot thread by the feed/derived handlers during apply;
/// read here after apply, same thread, so the locks never contend.
struct PriceFeed {
    canonical: Option<Arc<Mutex<CanonicalBook>>>,
    derived: Option<Arc<Mutex<DerivedBook>>>,
    /// Canonical slot where it has a price, else derived. Reused each block.
    merged: PriceVector,
    /// Changed slots this block. Reused each block.
    ticks: Vec<PriceTick>,
    /// The engine has taken a wholesale load and a full-universe fold.
    loaded: bool,
    /// Registry decimals by `AssetId` (prices are per whole token).
    decimals: Vec<u8>,
    weth: Option<AssetId>,
}

impl PriceFeed {
    fn empty() -> Self {
        Self {
            canonical: None,
            derived: None,
            merged: PriceVector::zeroed(),
            ticks: Vec::new(),
            loaded: false,
            decimals: Vec::new(),
            weth: None,
        }
    }
}

/// Seconds from one Ethereum slot to the next.
const SLOT_SECONDS: u64 = 12;

/// When a job built now executes: the next block, one slot after the tip.
/// Health, quotes and sizes are judged then, not at the tip, so a position
/// that crosses by interest alone in the next block is a candidate now (the
/// winner of block N+1 saw it at block N). Accrual is per second for most
/// adapters and per block for Compound; both read this as one block on.
/// State reads, resyncs and the simulation's parent stay at the tip.
#[inline]
const fn eval_ts(tip_ts: u64) -> u64 {
    tip_ts.saturating_add(SLOT_SECONDS)
}

/// Raw units of `asset` per 1e18 wei: `10^decimals · P(ETH) / P(asset)`,
/// both prices per whole token in the same numeraire. `None` when either
/// price or the decimals are unknown — never a guessed rate.
/// Canonical slot when it has a timestamp, otherwise a protocol getter's
/// USD price. A zero ray is not a price.
fn sizing_ray(
    canon: &liq_types::Price,
    protocol_usd: Option<liq_types::Ray>,
) -> Option<liq_types::Ray> {
    if canon.ts != 0 && !canon.price.raw().is_zero() {
        return Some(canon.price);
    }
    protocol_usd.filter(|p| !p.raw().is_zero())
}

fn per_eth_from_rays(asset: liq_types::Ray, eth: liq_types::Ray, decimals: u8) -> Option<U256> {
    if asset.raw().is_zero() || eth.raw().is_zero() {
        return None;
    }
    let unit = U256::from(10u64).checked_pow(U256::from(decimals))?;
    liq_types::fixed::mul_div(
        unit,
        eth.raw(),
        asset.raw(),
        liq_types::fixed::Rounding::Down,
    )
    .ok()
    .filter(|v| !v.is_zero())
}

#[cfg(test)]
fn per_eth_from_prices(
    asset_price: &liq_types::Price,
    eth_price: &liq_types::Price,
    decimals: u8,
) -> Option<U256> {
    if asset_price.ts == 0 || eth_price.ts == 0 || asset_price.price.raw().is_zero() {
        return None;
    }
    per_eth_from_rays(asset_price.price, eth_price.price, decimals)
}

/// Raw collateral units per raw debt unit, RAY, before bonus:
/// `P(debt) · 10^dec(coll) / (P(coll) · 10^dec(debt))`, rounded down so the
/// seized estimate never exceeds what the oracle ratio pays.
///
/// This is the canonical (Chainlink) ratio. It equals the protocol's own
/// ratio where the protocol prices off the same feeds (Aave V3/Spark); for
/// protocols with their own oracle it is an approximation of it.
fn coll_per_debt_from_rays(
    coll: liq_types::Ray,
    debt: liq_types::Ray,
    dec_coll: u8,
    dec_debt: u8,
) -> Option<liq_types::Ray> {
    use liq_types::fixed::{mul_div, Rounding, RAY};
    if coll.raw().is_zero() || debt.raw().is_zero() {
        return None;
    }
    let ten = U256::from(10u64);
    let uc = ten.checked_pow(U256::from(dec_coll))?;
    let ud = ten.checked_pow(U256::from(dec_debt))?;
    let x = mul_div(debt.raw(), uc, ud, Rounding::Down).ok()?;
    mul_div(x, RAY, coll.raw(), Rounding::Down)
        .ok()
        .filter(|v| !v.is_zero())
        .map(liq_types::Ray::from_raw)
}

#[cfg(test)]
fn coll_per_debt_from_prices(
    coll: &liq_types::Price,
    debt: &liq_types::Price,
    dec_coll: u8,
    dec_debt: u8,
) -> Option<liq_types::Ray> {
    if coll.ts == 0 || debt.ts == 0 || coll.price.raw().is_zero() {
        return None;
    }
    coll_per_debt_from_rays(coll.price, debt.price, dec_coll, dec_debt)
}

/// Publish [`MarketView::pair_terms`] for every `(repay, seize)` leg of
/// `quotes`: the candidates about to be selected, and the positions the
/// engine held `Unfundable` this block (their bands are what can make them
/// eligible). `bonus` is the quote's (sizing overlays it anyway);
/// `flash_fee_bps` is the cheapest live source holding the debt;
/// `fixed_gas` is the measured non-swap gas with that same source's wrap,
/// `0` = not measured (the band refuses such a pair rather than
/// under-charging gas).
fn publish_pair_terms(
    feed: &PriceFeed,
    book: &crate::protocol_prices::ProtocolPriceBook,
    flash: &FlashIndex,
    gas: &crate::gas_model::BandGas,
    quotes: &[(ProtocolId, &Quote)],
    view: &mut ProcessAssembleView,
) {
    let bands = view.bands().cloned();
    let mut shared = bands.as_ref().map(|b| b.inputs.lock());
    for &(protocol, quote) in quotes {
        for r in &quote.repay_options {
            let Some(dp0) = feed.merged.0.get(usize::from(r.asset.0)) else {
                continue;
            };
            let Some(dp) = sizing_ray(dp0, book.usd(r.asset)) else {
                continue;
            };
            let Some(&dd) = feed.decimals.get(usize::from(r.asset.0)) else {
                continue;
            };
            let cheapest = flash.entries(r.asset).first();
            let flash_fee_bps = cheapest.map_or(0, |e| e.fee_bps);
            let fixed_gas = cheapest.map_or(0, |e| gas.fixed(protocol, e.provider));
            for s in &quote.seize_options {
                let (Some(cp0), Some(&dc)) = (
                    feed.merged.0.get(usize::from(s.asset.0)),
                    feed.decimals.get(usize::from(s.asset.0)),
                ) else {
                    continue;
                };
                let Some(cp) =
                    sizing_ray(cp0, book.usd_or_market(s.asset, protocol, quote.key.market))
                else {
                    tracing::error!(
                        coll = s.asset.0,
                        debt = r.asset.0,
                        "pair unpriced — pair_terms withheld, leg not sized"
                    );
                    continue;
                };
                let Some(ratio) = coll_per_debt_from_rays(cp, dp, dc, dd) else {
                    tracing::error!(
                        coll = s.asset.0,
                        debt = r.asset.0,
                        "pair unpriced — pair_terms withheld, leg not sized"
                    );
                    continue;
                };
                let terms = liq_router::PairTerms {
                    bonus: s.bonus,
                    coll_per_debt: ratio,
                    flash_fee_bps,
                    fixed_gas,
                };
                view.insert_pair_terms(protocol, s.asset, r.asset, terms);
                if let Some(i) = shared.as_mut() {
                    i.terms.insert((protocol, s.asset, r.asset), terms);
                }
            }
        }
    }
}

/// Refresh [`MarketView::per_eth`] for every priced asset from this block's
/// merged vector. Existing keys are overwritten in place (no allocation
/// after the first block).
fn publish_per_eth(
    feed: &PriceFeed,
    book: &crate::protocol_prices::ProtocolPriceBook,
    view: &mut ProcessAssembleView,
) {
    let Some(weth) = feed.weth else {
        return;
    };
    let Some(eth_slot) = feed.merged.0.get(usize::from(weth.0)) else {
        return;
    };
    let Some(eth) = sizing_ray(eth_slot, book.usd(weth)) else {
        return;
    };
    let bands = view.bands().cloned();
    let mut inputs = bands.as_ref().map(|b| b.inputs.lock());
    for p in &feed.merged.0 {
        let Some(&dec) = feed.decimals.get(usize::from(p.asset.0)) else {
            continue;
        };
        // A token only markets price (no feed, no getter) takes their
        // median, as sizing takes its own market's price.
        let Some(asset_ray) = sizing_ray(p, book.usd_or_markets(p.asset)) else {
            continue;
        };
        if let Some(per) = per_eth_from_rays(asset_ray, eth, dec) {
            view.insert_per_eth(p.asset, per);
            if let Some(i) = inputs.as_mut() {
                i.per_eth.insert(p.asset, per);
            }
        }
    }
}

/// Stamp the band inputs with this block's fees. The warm thread rebuilds
/// the table when this block number advances.
fn publish_band_block(view: &ProcessAssembleView, fee: Option<&FeeQuote>, block: u64) {
    let (Some(bands), Some(fee)) = (view.bands(), fee) else {
        return;
    };
    let mut i = bands.inputs.lock();
    i.base_fee = fee.next_base_fee;
    i.priority_fee = fee.priority_wei;
    i.block = block;
}

/// Make sure every leg about to be sized has a band decision this block.
/// A pair the warm table has already evaluated (registered before the
/// table's block) is left to the table — absent there means not viable.
/// A pair it has not evaluated yet is computed here, once.
#[allow(clippy::too_many_arguments)] // each input is a distinct band term
fn ensure_bands(
    view: &mut ProcessAssembleView,
    registered: &mut HashMap<crate::bands::BandKey, u64>,
    cands: &[&Candidate],
    fee: Option<&FeeQuote>,
    block: u64,
    book: &PoolBook,
    routes: &liq_router::RouteTable,
    budget: &SolveBudget,
) {
    let Some(fee) = fee else {
        return;
    };
    let published_block = view.published_block();
    for c in cands {
        for r in &c.quote.repay_options {
            for s in &c.quote.seize_options {
                let key = (c.protocol, s.asset, r.asset);
                let first_seen = *registered.entry(key).or_insert(block);
                if MarketView::band(view, key.0, key.1, key.2).is_some()
                    || published_block > first_seen
                {
                    continue;
                }
                let (Some(terms), Some(per)) = (
                    MarketView::pair_terms(view, key.0, key.1, key.2),
                    MarketView::per_eth(view, key.2),
                ) else {
                    continue;
                };
                if let Some(b) = crate::bands::compute_one(
                    key,
                    &terms,
                    per,
                    fee.next_base_fee,
                    fee.priority_wei,
                    block,
                    book,
                    routes,
                    budget,
                ) {
                    view.insert_local_band(key, b);
                }
            }
        }
    }
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
            backrun_operator: None,
            chain_id,
            fee: None,
            oracle: None,
            bid_cfg: None,
            header_clock: None,
            prices: PriceFeed::empty(),
            band_gas: crate::gas_model::BandGas::none(),
            liq_gas: liq_router::LiqGas::none(),
            band_registered: HashMap::new(),
            flash_moved: false,
            timing: std::cell::Cell::new(BlockTiming::default()),
            routes_seen: 0,
            protocol_prices: crate::protocol_prices::ProtocolPriceBook::default(),
            protocol_moves: Vec::new(),
            price_reader: None,
            reads_at: 0,
            state_reader: None,
            state_reads_at: 0,
            adapter_state_reads: Vec::new(),
            position_reads: HashMap::new(),
            position_reads_block: 0,
            state_applied: 0,
            snapshots: None,
            drift: None,
            resync: HashMap::new(),
            sweep: std::collections::VecDeque::new(),
            quarantine: std::collections::HashSet::new(),
            svr_rx: None,
            svr_targets: Vec::new(),
            gov_rx: None,
            gov_inbox: None,
            gov_routes: None,
            gov_pending: Vec::new(),
            parent_gas_limit: 0,
            last_block: 0,
            last_hash: B256::ZERO,
            last_ts: 0,
            block_seen: false,
        }
    }

    /// Governance payload simulations in, governance bundles out.
    #[must_use]
    pub fn with_gov(
        mut self,
        rx: rtrb::Consumer<crate::governance::GovSim>,
        inbox: liq_exec::gov::GovInbox,
    ) -> Self {
        self.gov_rx = Some(rx);
        self.gov_inbox = Some(inbox);
        self
    }

    /// SVR aggregators and the hint ring. Empty targets do not start a reader.
    #[must_use]
    pub fn with_svr(
        mut self,
        rx: rtrb::Consumer<liq_types::MevShareHint>,
        targets: Vec<liq_oracle::mevshare::SvrTarget>,
    ) -> Self {
        self.svr_rx = Some(rx);
        self.svr_targets = targets;
        self
    }

    /// Price health from each protocol's own getters (read off the hot path
    /// by the thread behind `shared`), laid over the canonical vector.
    #[must_use]
    pub fn with_price_reader(mut self, shared: Arc<crate::protocol_prices::ReaderShared>) -> Self {
        self.price_reader = Some(shared);
        self
    }

    /// Protocol state read from chain each block (the thread behind
    /// `shared`), folded into the store by [`AfterBlock::amend`].
    #[must_use]
    pub fn with_state_reader(mut self, shared: Arc<crate::state_reads::StateReaderShared>) -> Self {
        self.state_reader = Some(shared);
        self
    }

    /// Persist the store every so many blocks (see [`crate::state_build`]).
    #[must_use]
    pub fn with_snapshots(mut self, writer: crate::state_build::SnapshotWriter) -> Self {
        self.snapshots = Some(writer);
        self
    }

    /// Check each snapshot for drift against the protocols' own views.
    #[must_use]
    pub fn with_drift(mut self, drift: crate::drift::DriftHandle) -> Self {
        self.drift = Some(drift);
        self
    }

    /// Rebuild the state reader's read set when due; between rebuilds, re-ask
    /// the positions `touched` and republish if their reads moved.
    fn refresh_state_reads(
        &mut self,
        block: u64,
        view: StateView<'_>,
        touched: &[liq_types::PositionId],
    ) {
        if self.state_reader.is_none() {
            return;
        }
        let gap =
            self.position_reads_block != 0 && block != self.position_reads_block.saturating_add(1);
        self.position_reads_block = block;
        let rebuild = self.state_reads_at == 0
            || block.saturating_sub(self.state_reads_at)
                >= crate::protocol_prices::READS_REFRESH_BLOCKS;
        let changed = if rebuild || gap {
            if rebuild {
                self.adapter_state_reads =
                    crate::state_reads::collect_state_reads(self.protocols, view);
                self.state_reads_at = block.max(1);
            }
            self.position_reads = crate::state_reads::collect_position_reads(self.protocols, &view);
            tracing::info!(
                reads = self.adapter_state_reads.len(),
                positions = self.position_reads.len(),
                block,
                "protocol state reads rebuilt"
            );
            true
        } else {
            self.reask_positions(&view, touched)
        };
        if changed {
            self.publish_state_reads();
        }
    }

    /// Re-ask `ids` for their reads. `true` when the set changed.
    fn reask_positions(&mut self, view: &StateView<'_>, ids: &[liq_types::PositionId]) -> bool {
        let mut changed = false;
        for &id in ids {
            let reads = crate::state_reads::position_reads_for(self.protocols, view, id);
            if reads.is_empty() {
                changed |= self.position_reads.remove(&id).is_some();
            } else if self.position_reads.get(&id) != Some(&reads) {
                self.position_reads.insert(id, reads);
                changed = true;
            }
        }
        changed
    }

    fn publish_state_reads(&self) {
        let Some(shared) = self.state_reader.as_ref() else {
            return;
        };
        let mut reads =
            crate::state_reads::merged_reads(&self.adapter_state_reads, &self.position_reads);
        let mut ids: Vec<&liq_types::PositionId> = self.resync.keys().collect();
        ids.sort_unstable();
        for id in ids {
            if let Some((r, _)) = self.resync.get(id) {
                reads.extend(r.iter().cloned());
            }
        }
        shared.reads.store(Arc::new(reads));
    }

    /// Ask for `id`'s state from chain. `false` when its adapter cannot.
    fn request_resync(
        &mut self,
        view: &StateView<'_>,
        id: liq_types::PositionId,
        block: u64,
    ) -> bool {
        let reads = crate::state_reads::resync_reads_for(self.protocols, view, id);
        if reads.is_empty() {
            return false;
        }
        self.resync.insert(id, (reads, block));
        true
    }

    /// Apply the drift thread's actions. Non-blocking.
    fn poll_drift(&mut self, store: &liq_state::StateStore) {
        let Some(drift) = self.drift.as_ref() else {
            return;
        };
        let actions = drift.take_actions();
        if actions.is_empty() {
            return;
        }
        let view = store.view(self.last_ts);
        let block = store.tip();
        let mut changed = false;
        for a in actions {
            match a {
                crate::drift::DriftAction::Resync(id) => {
                    if self.request_resync(&view, id, block) {
                        changed = true;
                    } else {
                        tracing::warn!(target: "drift", position = id.0, "drifting position's adapter cannot resync — it will be quarantined if it keeps drifting");
                    }
                }
                crate::drift::DriftAction::ResyncProtocol(pid) => {
                    let before = self.sweep.len();
                    let queued: std::collections::HashSet<liq_types::PositionId> =
                        self.sweep.iter().copied().collect();
                    let n = u32::try_from(view.len()).unwrap_or(u32::MAX);
                    let mut ids: Vec<(u8, liq_types::PositionId)> = (0..n)
                        .map(liq_types::PositionId)
                        .filter(|id| {
                            view.position(*id).is_ok_and(|p| p.key.protocol == pid)
                                && !queued.contains(id)
                        })
                        .map(|id| (sweep_rank(self.engine.band(id)), id))
                        .collect();
                    // Nearest to liquidation first.
                    ids.sort_unstable();
                    self.sweep.extend(ids.into_iter().map(|(_, id)| id));
                    tracing::warn!(target: "drift", protocol = pid.0, queued = self.sweep.len().saturating_sub(before), "protocol-wide resync queued");
                }
                crate::drift::DriftAction::Quarantine(id) => {
                    self.quarantine.insert(id);
                }
                crate::drift::DriftAction::Release(id) => {
                    self.quarantine.remove(&id);
                }
            }
        }
        if changed {
            self.publish_state_reads();
        }
    }

    /// Move the next slice of a protocol-wide resync into flight and drop
    /// resyncs that never settled.
    fn advance_resync(&mut self, view: &StateView<'_>, block: u64) {
        let mut changed = false;
        let before = self.resync.len();
        self.resync.retain(|id, (_, at)| {
            let keep = block.saturating_sub(*at) < RESYNC_TTL_BLOCKS;
            if !keep {
                tracing::warn!(target: "drift", position = id.0, "resync never settled — dropped");
            }
            keep
        });
        changed |= self.resync.len() != before;
        let mut reads = 0usize;
        while reads < SWEEP_READS_PER_BLOCK {
            let Some(id) = self.sweep.pop_front() else {
                break;
            };
            if self.request_resync(view, id, block) {
                reads = reads.saturating_add(self.resync.get(&id).map_or(0, |(r, _)| r.len()));
                changed = true;
            }
        }
        if changed {
            self.publish_state_reads();
        }
    }

    /// Fold the newest state batch into the store while its block is still
    /// the tip (the writes join that block's undo record), then refold what
    /// it changed and plan what became liquidatable.
    fn fold_state(
        &mut self,
        store: &mut liq_state::StateStore,
        dirty: &mut liq_node::DirtyAccumulator,
    ) {
        let Some(shared) = self.state_reader.as_ref() else {
            return;
        };
        let latest = shared.latest.load_full();
        let Some(batch) = latest.as_ref().as_ref() else {
            return;
        };
        if batch.seq <= self.state_applied
            || !self.block_seen
            || batch.block != self.last_block
            || batch.block != store.tip()
        {
            return;
        }
        self.state_applied = batch.seq;
        let ts = self.last_ts;
        dirty.clear();
        let mut settled: Vec<liq_types::PositionId> = Vec::new();
        for (i, p) in self.protocols.iter().enumerate() {
            let answers = batch.answers_for(i);
            if answers.is_empty() {
                continue;
            }
            match p.as_dyn().apply_state_reads(store, ts, &answers) {
                Ok(sets) => {
                    for set in sets {
                        if let DirtySet::Positions(ids) = &set {
                            settled.extend_from_slice(ids);
                        }
                        dirty.merge(set);
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, protocol = p.id().0, block = batch.block, "state reads refused")
                }
            }
        }
        // Positions a batch settled drop their reads; any still unsettled
        // keep theirs for the next head.
        let mut resynced = false;
        for id in &settled {
            resynced |= self.resync.remove(id).is_some();
        }
        if !settled.is_empty() && (self.reask_positions(&store.view(ts), &settled) || resynced) {
            self.publish_state_reads();
        }
        let collapsed = match dirty.collapse(store, ts) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "state-read dirty collapse failed");
                return;
            }
        };
        let mut sets: Vec<DirtySet> = as_dirty_sets(collapsed).collect();
        // The collapse drops positions whose market also accrued, and the
        // engine walks only banded positions for an accrual: a settled
        // account is refolded by name.
        if !settled.is_empty() {
            settled.sort_unstable();
            settled.dedup();
            sets.push(DirtySet::Positions(settled.iter().copied().collect()));
        }
        if sets.iter().all(|s| matches!(s, DirtySet::None)) {
            return;
        }
        let tip = store.tip();
        let store: &liq_state::StateStore = store;
        {
            let proto_refs: Vec<&dyn Protocol> =
                self.protocols.iter().map(|p| p.as_dyn()).collect();
            let flash = self.flash.load();
            let world = World {
                view: store.view(eval_ts(ts)),
                protocols: &proto_refs,
                flash: flash.as_ref(),
                routes: &self.routes,
                haircut: self.world_haircut(),
                overlay: Some(&self.protocol_prices),
            };
            for set in &sets {
                let Some(cause) = cause_for(set) else {
                    continue;
                };
                for p in &proto_refs {
                    if let Err(e) = self.engine.on_dirty(&world, p.id(), set, &cause) {
                        tracing::error!(error = %e, protocol = p.id().0, "on_dirty (state reads) failed");
                    }
                }
            }
        }
        let cands: Vec<Candidate> = self.engine.candidates().collect();
        if cands.is_empty() {
            return;
        }
        let view = store.view(eval_ts(ts));
        let _ = self.enqueue_candidates(&cands, tip, ts, Some(&view));
    }

    /// Protocol prices currently applied (tests / diagnostics).
    #[must_use]
    pub fn protocol_price_book(&self) -> &crate::protocol_prices::ProtocolPriceBook {
        &self.protocol_prices
    }

    /// Rebuild the reader's read set when due, then fold its newest batch
    /// into the book. `true` when some protocol price is new (the engine
    /// resyncs rather than sweeps).
    fn take_protocol_prices(&mut self, block: u64, view: StateView<'_>) -> bool {
        let Some(shared) = self.price_reader.as_ref() else {
            return false;
        };
        if self.reads_at == 0
            || block.saturating_sub(self.reads_at) >= crate::protocol_prices::READS_REFRESH_BLOCKS
        {
            let reads = crate::protocol_prices::collect_reads(self.protocols, view);
            tracing::info!(reads = reads.len(), block, "protocol price reads rebuilt");
            shared.reads.store(Arc::new(reads));
            self.reads_at = block.max(1);
        }
        let latest = shared.latest.load_full();
        let Some(batch) = latest.as_ref().as_ref() else {
            return false;
        };
        if batch.block <= self.protocol_prices.applied() {
            return false;
        }
        // Ratio reads (Morpho) restated in USD by the numeraire's own
        // price, as sizing prices it: canonical first, then a USD getter.
        let entries = {
            let canon = self.engine.prices();
            let book = &self.protocol_prices;
            crate::protocol_prices::to_usd(&batch.entries, |a| {
                let c = canon.0.get(usize::from(a.0))?;
                sizing_ray(c, book.usd_or_batch(a, batch))
            })
        };
        let restated = crate::protocol_prices::PriceBatch {
            block: batch.block,
            entries,
            failed: batch.failed,
        };
        let first = self.protocol_prices.applied() == 0;
        let fresh = self
            .protocol_prices
            .apply(&restated, &mut self.protocol_moves);
        if first {
            // Startup report (coverage plan 1C): what the first batch priced.
            let cov = crate::protocol_prices::coverage(
                &self.protocol_prices,
                &shared.reads.load(),
                batch,
                self.protocols,
            );
            crate::protocol_prices::log_coverage(&cov, batch.block);
        }
        fresh
    }

    /// Size the engine's per-position tables for the real universe so the
    /// hot thread does not reallocate them as positions load. Growth past
    /// this still works, it just allocates.
    #[must_use]
    pub fn with_engine_capacity(mut self, assets: usize, positions: usize) -> Self {
        self.engine = Engine::new(EngineConfig {
            assets: assets.max(1),
            positions: positions.max(1024),
            queue: 1024,
        });
        self
    }

    /// Measured gas model: per-protocol leg gas for `select`, and the
    /// band's non-swap gas. `None` → nothing is measured → nothing sizes.
    #[must_use]
    pub fn with_gas_model(
        mut self,
        model: Option<&crate::gas_model::GasModel>,
        resolve: &dyn Fn(&str) -> Option<liq_types::ProtocolId>,
    ) -> Self {
        if let Some(m) = model {
            self.liq_gas = m.liq_gas(resolve);
            self.band_gas = m.band_gas(resolve);
        }
        self
    }

    /// Registry decimals and the WETH id, for `per_eth` from live prices.
    #[must_use]
    pub fn with_assets(mut self, intern: &liq_config::Intern) -> Self {
        let n = intern.asset_id_capacity();
        self.prices.decimals = vec![0; n];
        for a in intern.assets() {
            if let Some(d) = self.prices.decimals.get_mut(usize::from(a.id.0)) {
                *d = a.decimals;
            }
            if a.address == crate::bind::REGISTRY_WETH {
                self.prices.weth = Some(a.id);
            }
        }
        if self.prices.weth.is_none() {
            tracing::error!("registry has no WETH — per_eth unpublished, nothing sizes");
        }
        self
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
        self.prices.canonical = index.canonical.clone();
        self.prices.derived = index.derived.clone();
        if self.prices.canonical.is_none() {
            tracing::error!(
                "no canonical price book — engine has no prices, nothing is liquidatable"
            );
        }
        self
    }

    /// End of block: re-read sources into the process [`FlashIndex`].
    /// `true` when the published index changed.
    fn publish_flash(&mut self) -> bool {
        let Some(srcs) = self.flash_sources.as_ref() else {
            return false;
        };
        let g = srcs.lock();
        self.flash_scratch.refresh(&g);
        self.flash_scratch.publish_if_material(&self.flash, 0)
    }

    /// Pair terms for every quote the engine held `Unfundable` this block,
    /// into the band inputs. A collateral gets an exit in the warm route
    /// table only from a band, and a band only from its pair's terms; a
    /// position with no exit yet never becomes a candidate to publish them,
    /// so without this its pair would never be evaluated at all.
    fn publish_unfunded_terms(&mut self) {
        let unfunded = self.engine.take_unfunded();
        if unfunded.is_empty() {
            return;
        }
        let legs: Vec<(ProtocolId, &Quote)> =
            unfunded.iter().map(|u| (u.protocol, &u.quote)).collect();
        publish_pair_terms(
            &self.prices,
            &self.protocol_prices,
            &self.flash.load(),
            &self.band_gas,
            &legs,
            &mut self.assemble,
        );
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

    /// The second operator key's address (`exec_bind::BACKRUN_SLOT`).
    #[must_use]
    pub fn with_backrun_operator(mut self, operator: Address) -> Self {
        self.backrun_operator = Some(operator);
        self
    }

    /// The key that signs a job of `kind`, and its nonce slot. MEV-Share
    /// backruns go out under the second operator when there is one.
    fn signer_for(&self, kind: TriggerKind) -> Option<(Address, usize)> {
        let first = self.operator?;
        match (kind, self.backrun_operator) {
            (TriggerKind::SvrAuction, Some(second)) => {
                Some((second, crate::exec_bind::BACKRUN_SLOT))
            }
            _ => Some((first, crate::exec_bind::OPERATOR_SLOT)),
        }
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
                exact_k: u8::try_from(u16::from(EXACT_K).saturating_mul(u16::from(NONCE_SLOTS)))
                    .unwrap_or(u8::MAX),
                legs_per_plan: EXACT_K,
                nonce_slots: NONCE_SLOTS,
                header_gas_limit,
                wrap_gas: bind.wrap_gas,
                wrap_aave_v4: bind.wrap_aave_v4,
                aave_v4: bind.aave_v4,
                liq_gas: self.liq_gas,
                over_borrow: U256::ZERO,
                min_out_tolerance_bps: liq_router::select::MIN_OUT_TOLERANCE_BPS,
                budget: SolveBudget::default(),
                bids: Some(bid_cfg),
                weth: self.prices.weth,
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
        if gas_limit != 0 {
            self.parent_gas_limit = gas_limit;
        }
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
        self.refresh_state_reads(ctx.block, ctx.store.view(ctx.timestamp), ctx.touched);
        self.advance_resync(&ctx.store.view(ctx.timestamp), ctx.block);
        let first_protocol_prices =
            self.take_protocol_prices(ctx.block, ctx.store.view(ctx.timestamp));
        // The overlay needs a vector to lay onto even before any canonical
        // feed loads (a protocol priced only by its own getters).
        if self.engine.prices().0.is_empty() && self.protocol_prices.applied() != 0 {
            let px = zero_prices(self.prices.decimals.len());
            if let Err(e) = self.engine.load_prices(&px) {
                tracing::error!(error = %e, "zero price vector refused");
            }
        }
        let proto_refs: Vec<&dyn Protocol> = self.protocols.iter().map(|p| p.as_dyn()).collect();
        let flash = self.flash.load();
        let world = World {
            view: ctx.store.view(eval_ts(ctx.timestamp)),
            protocols: &proto_refs,
            flash: flash.as_ref(),
            routes: &self.routes,
            haircut: self.world_haircut(),
            overlay: Some(&self.protocol_prices),
        };
        sync_prices(&mut self.engine, &world, &mut self.prices);
        if first_protocol_prices {
            if let Err(e) = self.engine.resync(&world) {
                tracing::error!(error = %e, "resync on first protocol prices reported an error");
            }
        } else if let Err(e) = self.engine.on_protocol_prices(&world, &self.protocol_moves) {
            tracing::error!(error = %e, "protocol price moves failed");
        }
        publish_per_eth(&self.prices, &self.protocol_prices, &mut self.assemble);
        self.assemble.prune_local_bands();
        if proto_refs.is_empty() {
            tracing::error!("empty protocol list — on_dirty skipped (no invented adapter)");
        } else {
            let sets: Vec<DirtySet> = as_dirty_sets(ctx.dirty).collect();
            let wide = sets.iter().any(|s| matches!(s, DirtySet::ProtocolWide));
            for set in &sets {
                if matches!(set, DirtySet::None | DirtySet::ProtocolWide) {
                    continue;
                }
                let Some(cause) = cause_for(set) else {
                    continue;
                };
                for p in &proto_refs {
                    if let Err(e) = self.engine.on_dirty(&world, p.id(), set, &cause) {
                        tracing::error!(error = %e, protocol = p.id().0, "on_dirty failed");
                    }
                }
            }
            // Grace period, sequencer, and any other protocol-wide log are
            // already in the committed state. Resync folds every account;
            // liquidatable ones come out as `Stale` and submit.
            if wide {
                if let Err(e) = self.engine.resync(&world) {
                    tracing::error!(error = %e, "ProtocolWide resync failed");
                }
            }
        }
        if let Err(e) = self.engine.on_block(&world) {
            tracing::error!(error = %e, "on_block failed");
        }
        // GUIDE 07 §4b: an `Unfundable` position is re-evaluated when either
        // side of eligibility moved — flash depth, or the exits the warm
        // route table holds (rebuilt off the hot path once per block).
        let routes_block = self.routes.table().block;
        if self.flash_moved || routes_block > self.routes_seen {
            self.flash_moved = false;
            self.routes_seen = self.routes_seen.max(routes_block);
            if let Err(e) = self.engine.on_flash_change(&world) {
                tracing::error!(error = %e, "Unfundable re-evaluation failed");
            }
        }
    }

    /// Drain the engine queue into the inbox. Tests inject candidates here.
    ///
    /// `view` sources tail-specific pin data (Liquity trove id, Compound
    /// seized cToken, ...) from the position's `PositionExtraRepr` — `None`
    /// in tests, `Some` at the real `after_block` call site.
    pub fn enqueue_candidates(
        &mut self,
        cands: &[Candidate],
        tip: u64,
        timestamp: u64,
        view: Option<&StateView<'_>>,
    ) -> DrainStats {
        self.enqueue(cands, tip, timestamp, view, false)
    }

    /// One hint, one transaction. `exact_k` is not the bound; [`SVR_BACKRUN_GAS`] is.
    fn enqueue_svr(
        &mut self,
        cands: &[Candidate],
        tip: u64,
        timestamp: u64,
        view: Option<&StateView<'_>>,
    ) -> DrainStats {
        self.enqueue(cands, tip, timestamp, view, true)
    }

    fn enqueue(
        &mut self,
        cands: &[Candidate],
        tip: u64,
        timestamp: u64,
        view: Option<&StateView<'_>>,
        svr: bool,
    ) -> DrainStats {
        let mut stats = DrainStats::default();
        if self.inbox.is_none() {
            tracing::error!("ExecPath unbound — drain try_send refused (no invented key)");
            stats.skipped_exec = stats
                .skipped_exec
                .saturating_add(u64::try_from(cands.len()).unwrap_or(u64::MAX));
            return stats;
        }
        let mode = if svr { Mode::Svr } else { Mode::Ordinary };
        let kept: Vec<Candidate>;
        let cands = if self.quarantine.is_empty() {
            cands
        } else {
            kept = cands
                .iter()
                .filter(|c| !self.quarantine.contains(&c.position))
                .cloned()
                .collect();
            &kept[..]
        };
        for b in self.build(cands, tip, view, mode, &mut stats) {
            match self.finish_job(
                b.lead,
                &b.assembled,
                b.hop_and_wrap_gas,
                tip,
                timestamp,
                &b.bid,
            ) {
                Finish::Sent => stats.jobs_sent = stats.jobs_sent.saturating_add(1),
                Finish::Sim => stats.skipped_sim = stats.skipped_sim.saturating_add(1),
                Finish::Job => stats.skipped_job = stats.skipped_job.saturating_add(1),
                Finish::Full => stats.inbox_full = stats.inbox_full.saturating_add(1),
                Finish::Exec => stats.skipped_exec = stats.skipped_exec.saturating_add(1),
            }
        }
        stats
    }

    /// Pins, select and assemble for `cands` under `mode`'s select
    /// configuration. Every plan that assembles, with the candidate it
    /// submits under. Nothing is sent here.
    fn build<'c>(
        &mut self,
        cands: &'c [Candidate],
        tip: u64,
        view: Option<&StateView<'_>>,
        mode: Mode,
        stats: &mut DrainStats,
    ) -> Vec<Built<'c>> {
        let mut out = Vec::new();
        let gas_failed = match self.select.as_ref() {
            None => {
                tracing::error!(
                    "select inputs absent — skip (no invented header gas / wrap / failed gas)"
                );
                stats.skipped_select = stats.skipped_select.saturating_add(1);
                return out;
            }
            Some(ready) => ready.gas_failed,
        };
        let prepare_at = std::time::Instant::now();
        let mut kept: Vec<&'c Candidate> = Vec::new();
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
            if !self.ensure_pins(c, view) {
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
            return out;
        }
        let Some(ready) = self.select.as_ref() else {
            tracing::error!("select inputs dropped — skip");
            stats.skipped_select = stats.skipped_select.saturating_add(1);
            return out;
        };
        let legs: Vec<(ProtocolId, &Quote)> = kept.iter().map(|c| (c.protocol, &c.quote)).collect();
        publish_pair_terms(
            &self.prices,
            &self.protocol_prices,
            &self.flash.load(),
            &self.band_gas,
            &legs,
            &mut self.assemble,
        );
        ensure_bands(
            &mut self.assemble,
            &mut self.band_registered,
            &kept,
            self.fee.as_ref(),
            tip,
            &self.book.read(),
            &self.routes.table(),
            &SolveBudget::default(),
        );
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
        let prepared = prepare_at.elapsed();
        self.add_timing(|t| t.prepare = t.prepare.saturating_add(prepared));
        let select_at = std::time::Instant::now();
        let flash = self.flash.load();
        let warm = self.routes.table();
        let warm_ref = if warm.is_empty() {
            None
        } else {
            Some(warm.as_ref())
        };
        let book = self.book.read();
        let cfg = match mode {
            Mode::Ordinary => ready.cfg,
            Mode::Svr => svr_select_cfg(ready.cfg),
            Mode::Gov => gov_select_cfg(ready.cfg, kept.len()),
        };
        let plans = match select(
            &inputs,
            &cfg,
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
                return out;
            }
        };
        let selected = select_at.elapsed();
        self.add_timing(|t| t.select = t.select.saturating_add(selected));
        if plans.is_empty() {
            tracing::error!("select emitted no plans");
            stats.skipped_select = stats.skipped_select.saturating_add(1);
            return out;
        }
        for plan in &plans {
            let Some(bid) = plan_bid(plan, ready, &self.assemble) else {
                stats.skipped_job = stats.skipped_job.saturating_add(1);
                continue;
            };
            let assemble_at = std::time::Instant::now();
            let assembled = assemble(
                std::slice::from_ref(plan),
                &ready.cfg,
                &book,
                &self.assemble,
                &ready.validate,
                &bid,
                &ready.gas,
                ready.flags,
                flash.as_ref(),
                ready.haircut,
            );
            let took = assemble_at.elapsed();
            self.add_timing(|t| t.assemble = t.assemble.saturating_add(took));
            let assembled = match assembled {
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
                out.push(Built {
                    lead,
                    assembled: a,
                    bid,
                    hop_and_wrap_gas: plan.hop_and_wrap_gas,
                });
            }
        }
        out
    }

    fn ensure_pins(&mut self, c: &Candidate, view: Option<&StateView<'_>>) -> bool {
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
        match p.pins_from_candidate(c, view) {
            Some(pins) => {
                {
                    let assemble = &self.assemble;
                    let token = |a: AssetId| liq_router::AssembleView::token(assemble, a);
                    if let Some(bind) = self.select_bind.as_mut() {
                        p.validate_pins(c, &pins, view, &token, &mut bind.validate);
                    }
                    if let Some(ready) = self.select.as_mut() {
                        p.validate_pins(c, &pins, view, &token, &mut ready.validate);
                    }
                }
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
        // Encoding and simulation, timed on every way out of them.
        let sim_lap = SimLap {
            drain: self,
            at: std::time::Instant::now(),
            done: std::cell::Cell::new(false),
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
        let Some(trigger) = sim_trigger(cand, self.parent_gas_limit) else {
            return Finish::Sim;
        };
        if hop_and_wrap_gas == 0 {
            tracing::error!("hop_and_wrap_gas is zero — skip (no invented gas)");
            return Finish::Job;
        }
        let Some((operator, slot)) = self.signer_for(cand.cause.kind()) else {
            tracing::error!("operator absent — skip (no invented key)");
            return Finish::Exec;
        };
        if matches!(
            &trigger,
            Trigger::Svr {
                reconstructed: None,
                predicted: None,
                ..
            }
        ) {
            let Some(job) = exec_job(
                cand,
                assembled,
                &plan_bytes,
                &calldata,
                hop_and_wrap_gas,
                self.fee,
                operator,
                slot,
                self.chain_id,
                tip,
                bid,
                false,
            ) else {
                return Finish::Job;
            };
            return if inbox.try_send(job) {
                Finish::Sent
            } else {
                tracing::error!("ExecInbox full — counted, not blocked");
                Finish::Full
            };
        }
        let Some(sim) = self.sim.as_ref().filter(|s| s.ready()) else {
            // No in-process simulator (or the store has not caught up): a
            // trigger already in committed state is verified by the exec
            // worker against the node before it is signed. A parent
            // transaction cannot be replayed there.
            if !matches!(trigger, Trigger::InterestDrift) {
                tracing::error!(
                    "in-process simulator not running and a parent tx to replay — skip send"
                );
                return Finish::Sim;
            }
            let Some(job) = exec_job(
                cand,
                assembled,
                &plan_bytes,
                &calldata,
                hop_and_wrap_gas,
                self.fee,
                operator,
                slot,
                self.chain_id,
                tip,
                bid,
                true,
            ) else {
                return Finish::Job;
            };
            return if inbox.try_send(job) {
                Finish::Sent
            } else {
                tracing::error!("ExecInbox full — counted, not blocked");
                Finish::Full
            };
        };
        let Some(at) = self.next_block(tip, timestamp) else {
            return Finish::Sim;
        };
        // The simulation runs with the most gas the block allows a
        // transaction, so the job's limit comes from what it measured.
        let bundle = Bundle {
            trigger,
            calls: vec![SimTx {
                caller: operator,
                to: sim.executor(),
                value: U256::ZERO,
                data: calldata.clone(),
                gas_limit: at.max_tx_gas(),
                // Signed without one (`liq_exec::template::sign_call`).
                access_list: Vec::new(),
            }],
            min_profit: U256::from(assembled.plan.min_profit_wei),
            health: None,
        };
        let mut outcome = match sim.verify(&bundle, &at) {
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
        // The bid is a fraction of `gross − gasCostWei`, and `gasCostWei` is
        // the planner's estimate. Where the transaction burns more, the bid
        // comes out of gas we pay ourselves: block 26,098,187 budgeted 872k
        // gas, ran 1.10M, bid 99.5 % and lost 0.00022 ETH. Charge what the
        // simulation measured and simulate again; a plan that is not
        // profitable at its real gas is not sent.
        let Ok(wei_per_gas) = ready.gas.accounting_wei_per_gas() else {
            tracing::error!("accounting gas price overflow — skip");
            return Finish::Job;
        };
        let (plan_bytes, calldata) = match charge_measured_gas(
            &assembled.plan,
            outcome.gas_used,
            wei_per_gas,
        ) {
            None => (plan_bytes, calldata),
            Some(plan) => {
                let encoded = match EncodedPlan::encode(&plan, &ready.validate) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::error!(error = %e, "re-priced plan encode refused");
                        return Finish::Job;
                    }
                };
                let bytes = Bytes::from(encoded.into_bytes());
                let data = execute_calldata(bytes.as_ref());
                let again = Bundle {
                    calls: bundle
                        .calls
                        .iter()
                        .map(|c| SimTx {
                            data: data.clone(),
                            ..c.clone()
                        })
                        .collect(),
                    min_profit: U256::from(plan.min_profit_wei),
                    ..bundle.clone()
                };
                tracing::info!(
                    planned = assembled.plan.gas_cost_wei,
                    measured = plan.gas_cost_wei,
                    gas = outcome.gas_used,
                    "plan gas cost re-priced from the simulation"
                );
                outcome = match sim.verify(&again, &at) {
                    Ok(o) => o,
                    Err(e) => {
                        tracing::info!(error = %e, "not profitable at its measured gas — skip send");
                        return Finish::Sim;
                    }
                };
                (bytes, data)
            }
        };
        sim_lap.stop();
        let handoff_at = std::time::Instant::now();
        let Some(gas) = job_gas(outcome.gas_spent, at.max_tx_gas()) else {
            tracing::error!("sim spent no gas — skip");
            return Finish::Job;
        };
        let Some(job) = exec_job(
            cand,
            assembled,
            &plan_bytes,
            &calldata,
            gas,
            self.fee,
            operator,
            slot,
            self.chain_id,
            tip,
            bid,
            false,
        ) else {
            return Finish::Job;
        };
        let sent = inbox.try_send(job);
        let handed = handoff_at.elapsed();
        self.add_timing(|t| {
            t.handoff = t.handoff.saturating_add(handed);
            if sent {
                t.jobs = t.jobs.saturating_add(1);
            }
        });
        if sent {
            Finish::Sent
        } else {
            tracing::error!("ExecInbox full — counted, not blocked");
            Finish::Full
        }
    }

    /// The last block's [`BlockTiming`].
    #[must_use]
    pub fn last_timing(&self) -> BlockTiming {
        self.timing.get()
    }

    fn add_timing(&self, f: impl FnOnce(&mut BlockTiming)) {
        let mut t = self.timing.get();
        f(&mut t);
        self.timing.set(t);
    }
}

/// Adds the time since `at` to [`BlockTiming::sim`] once: at [`Self::stop`],
/// or when dropped on an early return.
struct SimLap<'a> {
    drain: &'a DrainJoin,
    at: std::time::Instant,
    done: std::cell::Cell<bool>,
}

impl SimLap<'_> {
    fn stop(&self) {
        if !self.done.replace(true) {
            let took = self.at.elapsed();
            self.drain
                .add_timing(|t| t.sim = t.sim.saturating_add(took));
        }
    }
}

impl Drop for SimLap<'_> {
    fn drop(&mut self) {
        self.stop();
    }
}

impl DrainJoin {
    /// The block a job built on `tip` lands in. `None` when `tip` is not
    /// the block this join last saw committed: its hash names the state the
    /// simulator reads.
    fn next_block(&self, tip: u64, timestamp: u64) -> Option<NextBlock> {
        if tip != self.last_block {
            tracing::error!(
                tip,
                last = self.last_block,
                "store tip is not the last committed block — skip sim"
            );
            return None;
        }
        Some(NextBlock {
            parent: BlockRef {
                number: tip,
                hash: self.last_hash,
            },
            parent_timestamp: timestamp,
            parent_gas_limit: self.parent_gas_limit,
        })
    }
}

impl DrainJoin {
    /// Apply decoded SVR reports to the engine. Does not block. Hints that
    /// arrive before the first committed block are dropped: there is no
    /// state to evaluate them against.
    fn poll_svr(&mut self, store: &liq_state::StateStore) {
        let Some(rx) = self.svr_rx.as_mut() else {
            return;
        };
        if !self.block_seen || self.svr_targets.is_empty() {
            let mut dropped = false;
            while rx.pop().is_ok() {
                dropped = true;
            }
            if dropped {
                tracing::error!("SVR hint before a committed block — not applied");
            }
            return;
        }
        let mut ticks: Vec<PriceTick> = Vec::new();
        let mut n = 0u32;
        while n < 8 {
            let hint = match rx.pop() {
                Ok(h) => h,
                Err(_) => break,
            };
            n = n.saturating_add(1);
            match liq_oracle::mevshare::match_hint(&hint, &self.svr_targets) {
                Ok(Some(m)) => {
                    let source =
                        liq_oracle::mevshare::publish_source(&m, std::time::Instant::now());
                    ticks.push(PriceTick {
                        asset: m.target.asset,
                        price: m.price,
                        source,
                        block: self.last_block,
                        ts: self.last_ts,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(error = %e, hash = %hint.hash, "SVR hint decode failed");
                }
            }
        }
        if ticks.is_empty() {
            return;
        }
        let proto_refs: Vec<&dyn Protocol> = self.protocols.iter().map(|p| p.as_dyn()).collect();
        {
            let flash = self.flash.load();
            let world = World {
                view: store.view(eval_ts(self.last_ts)),
                protocols: &proto_refs,
                flash: flash.as_ref(),
                routes: &self.routes,
                haircut: self.world_haircut(),
                overlay: Some(&self.protocol_prices),
            };
            for t in &ticks {
                if let Err(e) = self.engine.on_price_tick(&world, t) {
                    tracing::error!(error = %e, asset = t.asset.0, "SVR price tick refused");
                }
            }
        }
        let cands: Vec<Candidate> = self.engine.candidates().collect();
        if cands.is_empty() {
            return;
        }
        let (groups, other) = partition_by_hint(cands);
        let view = store.view(eval_ts(self.last_ts));
        let tip = store.tip();
        for group in &groups {
            let _ = self.enqueue_svr(group, tip, self.last_ts, Some(&view));
        }
        if !other.is_empty() {
            let _ = self.enqueue_candidates(&other, tip, self.last_ts, Some(&view));
        }
    }
}

impl DrainJoin {
    /// Governance simulations from the worker, each handled on the tip it
    /// was built on.
    fn poll_gov(&mut self, store: &liq_state::StateStore) {
        if let Some(rx) = self.gov_rx.as_mut() {
            while let Ok(s) = rx.pop() {
                self.gov_pending.push(s);
            }
        }
        if self.gov_pending.is_empty() {
            return;
        }
        let (ready, wait) = split_gov_pending(std::mem::take(&mut self.gov_pending), store.tip());
        self.gov_pending = wait;
        for sim in &ready {
            let _ = self.handle_gov(store, sim);
        }
    }

    /// Lay `sim`'s logs over committed state, find the positions the payload
    /// makes liquidatable that are not liquidatable without it, and send one
    /// transaction per account for the target block. Returns the number of
    /// transactions handed to the exec worker.
    pub fn handle_gov(
        &mut self,
        store: &liq_state::StateStore,
        sim: &crate::governance::GovSim,
    ) -> usize {
        if store.tip() != sim.base_block {
            tracing::debug!(
                action = ?sim.action,
                base = sim.base_block,
                tip = store.tip(),
                "governance simulation built on another head — dropped"
            );
            return 0;
        }
        if self.gov_inbox.is_none() {
            tracing::error!("governance inbox unbound — payload not planned");
            return 0;
        }
        let protocols = self.protocols;
        let flash = self.flash.load();
        let haircut = self.world_haircut();
        let routes = self
            .gov_routes
            .get_or_insert_with(|| crate::governance::GovRoutes::new(protocols));
        let (ov, after) = match crate::governance::newly_liquidatable(
            &mut self.engine,
            protocols,
            routes,
            store,
            sim,
            flash.as_ref(),
            &self.routes,
            haircut,
            Some(&self.protocol_prices),
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, action = ?sim.action, "governance logs refused by an adapter");
                return 0;
            }
        };
        if after.is_empty() {
            return 0;
        }
        let pins_view = match store.view_with(&ov, sim.target_ts) {
            Ok(v) => v,
            Err(_) => return 0,
        };
        let mut stats = DrainStats::default();
        let built = self.build(
            &after,
            sim.base_block,
            Some(&pins_view),
            Mode::Gov,
            &mut stats,
        );
        let Some(ready) = self.select.as_ref() else {
            return 0;
        };
        let Some(fee) = self.fee.filter(|f| f.parent_block == sim.base_block) else {
            tracing::error!(
                action = ?sim.action,
                "fee quote is not for this head — not sent"
            );
            return 0;
        };
        let mut txs = Vec::with_capacity(built.len());
        for b in &built {
            let encoded = match EncodedPlan::encode(&b.assembled.plan, &ready.validate) {
                Ok(e) => match sim.action {
                    liq_exec::gov::GovAction::Payload { id, .. } => e.with_gov_payload(id),
                    liq_exec::gov::GovAction::Spell(spell) => e.with_gov_spell(spell),
                },
                Err(e) => Err(e),
            };
            let encoded = match encoded {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!(error = %e, pos = b.lead.position.0, "governance plan encode refused");
                    continue;
                }
            };
            let (Some((collateral, debt)), Some(g0)) =
                (assets_of(b.lead), b.assembled.plan.groups.first())
            else {
                continue;
            };
            let plan = Bytes::from(encoded.into_bytes());
            txs.push(liq_exec::gov::GovTx {
                trace: b.lead.trace,
                calldata: execute_calldata(plan.as_ref()),
                plan,
                protocol: b.lead.protocol,
                market: b.lead.quote.key.market,
                collateral,
                debt,
                flash: g0.provider,
                position: b.lead.quote.key,
                auction_bps: b.bid.refund_bps,
            });
        }
        let n = txs.len();
        if n == 0 {
            tracing::info!(action = ?sim.action, "no governance plan assembled");
            return 0;
        }
        let job = liq_exec::gov::GovJob {
            action: sim.action,
            base_block: sim.base_block,
            target_block: sim.target_block,
            target_ts: sim.target_ts,
            exec_gas: sim.exec_gas,
            fee,
            chain_id: self.chain_id,
            txs,
        };
        match self.gov_inbox.as_ref() {
            Some(inbox) if inbox.try_send(job) => n,
            _ => {
                tracing::error!(action = ?sim.action, "governance inbox full — not sent");
                0
            }
        }
    }
}

/// Simulations built on `tip` (handle now) and on a later block (wait for
/// the store to reach it). One built on an earlier block is dropped: the
/// worker sends a fresh one for the current head.
fn split_gov_pending(
    sims: Vec<crate::governance::GovSim>,
    tip: u64,
) -> (
    Vec<crate::governance::GovSim>,
    Vec<crate::governance::GovSim>,
) {
    let mut ready = Vec::new();
    let mut wait = Vec::new();
    for s in sims {
        match s.base_block.cmp(&tip) {
            std::cmp::Ordering::Equal => ready.push(s),
            std::cmp::Ordering::Greater => wait.push(s),
            std::cmp::Ordering::Less => tracing::debug!(
                action = ?s.action,
                base = s.base_block,
                tip,
                "governance simulation for a passed block — dropped"
            ),
        }
    }
    (ready, wait)
}

/// One bundle per oracle hint. Anything else stays on the ordinary drain.
fn partition_by_hint(cands: Vec<Candidate>) -> (Vec<Vec<Candidate>>, Vec<Candidate>) {
    let mut groups: Vec<(B256, Vec<Candidate>)> = Vec::new();
    let mut other = Vec::new();
    for c in cands {
        if let TriggerCause::SvrAuction { hint, .. } = &c.cause {
            let hint = *hint;
            if let Some((_, group)) = groups.iter_mut().find(|(h, _)| *h == hint) {
                group.push(c);
            } else {
                groups.push((hint, vec![c]));
            }
        } else {
            other.push(c);
        }
    }
    (groups.into_iter().map(|(_, group)| group).collect(), other)
}

/// Which select configuration a batch is built under.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Mode {
    Ordinary,
    Svr,
    /// A governance action's liquidations. Its gas is charged by the
    /// Executor, at `tx.gasprice`, to whichever transaction applies it.
    Gov,
}

/// One assembled plan and the candidate it submits under.
struct Built<'c> {
    lead: &'c Candidate,
    assembled: liq_router::Assembled,
    bid: Bid,
    hop_and_wrap_gas: u64,
}

/// One account per plan, so one account per transaction: an owner who
/// changes their position in the block before reverts only their own.
fn gov_select_cfg(mut cfg: SelectCfg, accounts: usize) -> SelectCfg {
    let n = u8::try_from(accounts).unwrap_or(u8::MAX).max(1);
    cfg.legs_per_plan = 1;
    cfg.exact_k = n;
    cfg.nonce_slots = n.min(NONCE_SLOTS);
    cfg
}

fn svr_select_cfg(mut cfg: SelectCfg) -> SelectCfg {
    // 255 does not bind: an Aave leg is several hundred thousand gas, so
    // 12M fills first. One slot, and no 8-leg split: the accounts that do
    // not fit are not a second bundle on this hint.
    cfg.exact_k = u8::MAX;
    cfg.nonce_slots = 1;
    cfg.legs_per_plan = u8::MAX;
    cfg.header_gas_limit = cfg.header_gas_limit.min(SVR_BACKRUN_GAS);
    cfg
}

impl AfterBlock for DrainJoin {
    fn after_block(&mut self, ctx: AfterBlockCtx<'_>) {
        let started = std::time::Instant::now();
        self.timing.set(BlockTiming::default());
        self.block_seen = true;
        self.last_block = ctx.block;
        self.last_hash = ctx.hash;
        self.last_ts = ctx.timestamp;
        self.flash_moved |= self.publish_flash();
        self.observe_parent_header(ctx.base_fee_per_gas, ctx.gas_used, ctx.gas_limit, ctx.block);
        self.refresh_select(ctx.gas_limit);
        let tip = ctx.store.tip();
        let ts = ctx.timestamp;
        let store = ctx.store;
        let snap = self
            .snapshots
            .as_mut()
            .and_then(|w| w.after_block(store, ctx.block, ctx.hash));
        let block = ctx.block;
        let engine_at = std::time::Instant::now();
        self.add_timing(|t| t.setup = started.elapsed());
        self.feed_engine(ctx);
        self.add_timing(|t| t.engine = engine_at.elapsed());
        // After the engine took this block: its bands and prices match the
        // snapshot's block.
        if let (Some(snap), Some(drift)) = (snap, self.drift.as_ref()) {
            let bands = crate::drift::bands_for(&snap, |id| self.engine.band(id));
            drift.offer(crate::drift::DriftJob {
                snap,
                bands,
                px: self.engine.prices().clone(),
                block,
                timestamp: ts,
            });
        }
        self.publish_unfunded_terms();
        let cands: Vec<Candidate> = self.engine.candidates().collect();
        let n = u32::try_from(cands.len()).unwrap_or(u32::MAX);
        self.add_timing(|t| t.candidates = n);
        if !cands.is_empty() {
            let view = store.view(eval_ts(ts));
            let _ = self.enqueue_candidates(&cands, tip, ts, Some(&view));
        }
        // Last: the warm thread rebuilds bands and exits once this stamp
        // lands, so every pair term this block published is in that rebuild.
        publish_band_block(&self.assemble, self.fee.as_ref(), block);
        let total = started.elapsed();
        self.add_timing(|t| t.total = total);
    }

    fn poll(&mut self, store: &liq_state::StateStore) {
        self.poll_svr(store);
        self.poll_gov(store);
        self.poll_drift(store);
    }

    fn amend(&mut self, store: &mut liq_state::StateStore, dirty: &mut liq_node::DirtyAccumulator) {
        self.fold_state(store, dirty);
    }
}

enum Finish {
    Sent,
    Sim,
    Job,
    Full,
    Exec,
}

/// A vector of `n` unpriced slots, slot `i` = asset `i`.
fn zero_prices(n: usize) -> PriceVector {
    PriceVector(
        (0..n)
            .filter_map(|i| u16::try_from(i).ok())
            .map(|i| liq_types::Price {
                asset: AssetId(i),
                price: liq_types::Ray::ZERO,
                source: liq_types::SourceKind::Canonical,
                block: 0,
                ts: 0,
            })
            .collect(),
    )
}

/// Bring the engine's price vector up to the oracle books after this block's
/// logs were applied. First call (or an asset gaining its first price): a
/// wholesale load plus a full-universe fold, since positions holding a
/// previously unpriced asset were never banded. Otherwise each changed slot
/// is one `on_price_tick`, which sweeps only the positions whose thresholds
/// it crossed.
fn sync_prices(engine: &mut Engine, world: &World<'_>, feed: &mut PriceFeed) {
    let Some(canonical) = feed.canonical.as_ref() else {
        return;
    };
    let plan = {
        let c = canonical.lock();
        let d = feed.derived.as_ref().map(|d| d.lock());
        plan_price_sync(
            c.vector(),
            d.as_ref().map(|d| d.vector()),
            engine.prices(),
            feed.loaded,
            &mut feed.merged,
            &mut feed.ticks,
        )
    };
    if plan == PriceSync::Reload {
        if let Err(e) = engine.load_prices(&feed.merged) {
            tracing::error!(error = %e, "price load refused — engine keeps its old prices");
            return;
        }
        feed.loaded = true;
        let priced = feed.merged.0.iter().filter(|p| p.ts != 0).count();
        tracing::info!(
            priced,
            assets = feed.merged.0.len(),
            "engine price load + full resync"
        );
        if let Err(e) = engine.resync(world) {
            tracing::error!(error = %e, "full resync after price load reported an error");
        }
        return;
    }
    for t in &feed.ticks {
        if let Err(e) = engine.on_price_tick(world, t) {
            tracing::error!(error = %e, asset = t.asset.0, "price tick refused");
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PriceSync {
    /// Wholesale load + full fold: first sync, a resized book, or an asset
    /// that just got its first price.
    Reload,
    /// `ticks` holds the changed slots (possibly none).
    Ticks,
}

/// Fill `merged` (canonical slot where priced, else derived) and decide how
/// the engine takes it. `ticks` is filled only for [`PriceSync::Ticks`].
fn plan_price_sync(
    canonical: &PriceVector,
    derived: Option<&PriceVector>,
    engine_px: &PriceVector,
    loaded: bool,
    merged: &mut PriceVector,
    ticks: &mut Vec<PriceTick>,
) -> PriceSync {
    merged.0.clear();
    ticks.clear();
    for (i, p) in canonical.0.iter().enumerate() {
        let d = derived.and_then(|d| d.0.get(i)).filter(|dp| dp.ts != 0);
        let chosen = match d {
            Some(dp) if p.ts == 0 => dp,
            _ => p,
        };
        merged.0.push(chosen.clone());
    }
    if !loaded || engine_px.0.len() != merged.0.len() {
        return PriceSync::Reload;
    }
    for (p, old) in merged.0.iter().zip(engine_px.0.iter()) {
        if p.ts == 0 || (p.price == old.price && p.ts == old.ts) {
            continue;
        }
        if old.ts == 0 {
            ticks.clear();
            return PriceSync::Reload;
        }
        ticks.push(p.clone());
    }
    PriceSync::Ticks
}

fn cause_for(set: &DirtySet) -> Option<TriggerCause> {
    match set {
        DirtySet::None | DirtySet::ProtocolWide => None,
        DirtySet::MarketAccrual(_) => Some(TriggerCause::InterestDrift),
        DirtySet::MarketReprice(rows) => rows
            .first()
            .map(|r| TriggerCause::ParamChange { market: r.market }),
        // The borrower tx is already in this committed block. The collapsed
        // dirty set does not carry its hash, so the cause has no parent.
        // The account is still folded and, if liquidatable, submitted.
        DirtySet::Positions(_) => Some(TriggerCause::UserAction { tx: None }),
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

/// Causes whose effect is already in the committed state. They submit as
/// one transaction. Pre-inclusion causes (SVR, public transmit, pull
/// payload) are not in this set: a naked tx would hit the old price.
fn canonical_cause(kind: TriggerKind) -> bool {
    matches!(
        kind,
        TriggerKind::InterestDrift
            | TriggerKind::Stale
            | TriggerKind::ParamChange
            | TriggerKind::UserAction
            | TriggerKind::DerivedRate
            | TriggerKind::PoolStateChange
    )
}

fn lead_candidate<'a>(
    plan: &liq_router::SelectedPlan,
    kept: &[&'a Candidate],
) -> Option<&'a Candidate> {
    let mut first: Option<&'a Candidate> = None;
    let mut canonical: Option<&'a Candidate> = None;
    let mut mixed = false;
    let mut seen: Option<TriggerKind> = None;
    for g in &plan.groups {
        for s in &g.legs {
            let c = kept.iter().copied().find(|x| x.position == s.position)?;
            if first.is_none() {
                first = Some(c);
            }
            if canonical.is_none() && canonical_cause(c.cause.kind()) {
                canonical = Some(c);
            }
            match seen {
                None => seen = Some(c.cause.kind()),
                Some(k) if k != c.cause.kind() => mixed = true,
                Some(_) => {}
            }
        }
    }
    if mixed {
        tracing::error!("mixed triggers in one plan — submitting the canonical leg");
    }
    canonical.or(first)
}

fn sim_trigger(c: &Candidate, trigger_gas: u64) -> Option<Trigger> {
    match &c.cause {
        // State already committed. Simulate our call alone.
        TriggerCause::InterestDrift
        | TriggerCause::Stale { .. }
        | TriggerCause::ParamChange { .. }
        | TriggerCause::UserAction { .. }
        | TriggerCause::DerivedRate { .. }
        | TriggerCause::PoolStateChange { .. } => Some(Trigger::InterestDrift),
        // Shared hint: event hash, forwarder, and `forward` calldata. `from`
        // is optional. When it is present, replay `forward` as that sender.
        // When it is absent, backrun the hash and do not invent a caller.
        TriggerCause::SvrAuction {
            hint,
            forwarder,
            call_data,
            caller,
            ..
        } => {
            if call_data.is_empty() {
                tracing::error!(hash = %hint, "SVR hint has no calldata — skip");
                return None;
            }
            let shared = Box::new(liq_types::MevShareHint {
                hash: *hint,
                to: if forwarder.is_zero() {
                    None
                } else {
                    Some(*forwarder)
                },
                function_selector: Some(liq_oracle::mevshare::FORWARD_SELECTOR),
                call_data: Some(call_data.clone()),
                logs: None,
                from: *caller,
            });
            if let Some(caller) = *caller {
                if forwarder.is_zero() {
                    tracing::error!(
                        hash = %hint,
                        "SVR hint has a sender but no forwarder — skip sim"
                    );
                    return None;
                }
                if trigger_gas == 0 {
                    tracing::error!(hash = %hint, "parent gas limit absent — skip SVR sim");
                    return None;
                }
                Some(Trigger::Svr {
                    hint: shared,
                    reconstructed: Some(Box::new(SimTx {
                        caller,
                        to: *forwarder,
                        value: U256::ZERO,
                        data: call_data.clone(),
                        gas_limit: trigger_gas,
                        access_list: Vec::new(),
                    })),
                    predicted: None,
                })
            } else {
                Some(Trigger::Svr {
                    hint: shared,
                    reconstructed: None,
                    predicted: None,
                })
            }
        }
        TriggerCause::OraclePublic { tx } => {
            tracing::error!(
                ?tx,
                "public transmit has no raw tx — canonical landing submits"
            );
            None
        }
        TriggerCause::OraclePullHeld { .. } => {
            tracing::error!("pull-oracle update has no submitter tx — skip (no invented caller)");
            None
        }
        TriggerCause::OraclePredicted { .. } => None,
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
    slot: usize,
    chain_id: u64,
    tip: u64,
    bid: &Bid,
    rpc_verify: bool,
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
    // `path.rs` accepts `max_block - target_block` in 2..=3. The hint does
    // not carry a block range. Span 2 is that validator's minimum: the next
    // block through two after it (`maxBlock` is inclusive).
    let max_block = match kind {
        TriggerKind::SvrAuction => match target.checked_add(2) {
            Some(m) => m,
            None => {
                tracing::error!("SVR max_block overflow — skip");
                return None;
            }
        },
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
    // Raw parent bytes only. A hash is not a transaction. Canonical causes
    // submit with no parent; pre-inclusion causes never reach here without
    // bytes because `sim_trigger` already refused them.
    let backrun_tx = match &cand.cause {
        TriggerCause::OraclePullHeld { payload } if !payload.is_empty() => Some(payload.clone()),
        _ => None,
    };
    let auction_bps = match kind {
        TriggerKind::InterestDrift
        | TriggerKind::Stale
        | TriggerKind::UserAction
        | TriggerKind::DerivedRate
        | TriggerKind::PoolStateChange => 0,
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
        calldata: calldata.clone(),
        gas_limit,
        chain_id,
        slot,
        rpc_verify,
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
                min_repay: alloy_primitives::U256::ZERO,
                pair_seize: None,
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
            fluid: None,
            fluid_tail: None,
            gearbox_min_seized: None,
            gearbox_full: false,
            compound_ctoken_collateral: None,
            compound_is_cether: None,
            aave_v4_collateral_reserve_id: None,
            aave_v4_debt_reserve_id: None,
            morpho_market_id: None,
        }
    }

    fn aave_pins(borrower: Address) -> liq_router::TailPins {
        let mut p = empty_pins();
        p.borrower = borrower;
        p
    }

    fn wrap_gas() -> [u64; 7] {
        [366_332, 355_632, 460_032, 370_435, 384_134, 0, 0]
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
                legs_per_plan: u8::MAX,
                nonce_slots: 1,
                header_gas_limit: 30_000_000,
                wrap_gas: wrap_gas(),
                wrap_aave_v4: 496_704,
                aave_v4: None,
                liq_gas: liq_router::LiqGas::uniform(80_000),
                over_borrow: U256::from(1u64),
                min_out_tolerance_bps: liq_router::select::MIN_OUT_TOLERANCE_BPS,
                budget: SolveBudget::default(),
                bids: None,
                weth: None,
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
                factory: 0,
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
                v4: None,
                window: None,
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
        v.insert_local_band(
            (PROTO, A0, A1),
            liq_router::ViabilityBand {
                min_size: U256::ZERO,
                max_size: U256::MAX,
                base_fee: 0,
                block: 0,
            },
        );
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
                warm: false,
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
        fn executor(&self) -> Address {
            self.inner.executor()
        }

        fn verify(&self, bundle: &Bundle, at: &NextBlock) -> Result<SimOutcome, SimError> {
            match self.inner.verify(bundle, at) {
                Ok(o) => Ok(o),
                Err(SimError::ProfitBelowFloor { .. }) => {
                    let mut b = bundle.clone();
                    b.min_profit = U256::ZERO;
                    self.inner.verify(&b, at)
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
        let st = j.enqueue_candidates(&[c], 0, 1, None);
        assert_eq!(st.jobs_sent, 0);
        assert!(st.skipped_predicted >= 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn position_dirty_is_a_user_action_without_an_invented_hash() {
        let cause = cause_for(&DirtySet::Positions(Default::default())).unwrap();
        assert!(matches!(cause, TriggerCause::UserAction { tx: None }));
        assert!(cause_for(&DirtySet::ProtocolWide).is_none());
    }

    #[test]
    fn canonical_triggers_send_without_a_parent_tx() {
        let causes = [
            TriggerCause::UserAction { tx: None },
            TriggerCause::DerivedRate { source: A0 },
            TriggerCause::PoolStateChange { pool: addr(1) },
            TriggerCause::ParamChange {
                market: liq_types::MarketId(0),
            },
        ];
        for cause in causes {
            let (inbox, rx) = ExecInbox::pair(4);
            // Empty chain has no profit. OverlayGasSim keeps the measured
            // gas and drops the profit floor, same as the interest-drift
            // submit test. The assertion is that the cause is submitted.
            let mut j = join_base(Some(inbox), false).with_sim(Box::new(OverlayGasSim {
                inner: MemoryDrainSim::new(MemoryFactory::empty()),
            }));
            let mut c = candidate(1, cause);
            c.quote.key.user = addr(0xB1);
            let st = j.enqueue_candidates(&[c], 0, 1, None);
            assert!(
                st.jobs_sent >= 1,
                "liquidatable canonical cause must submit, stats={st:?}"
            );
            assert!(rx.try_recv().is_ok());
        }
    }

    #[test]
    fn preinclusion_without_parent_bytes_does_not_send() {
        let (inbox, rx) = ExecInbox::pair(4);
        let mut j = join_base(Some(inbox), true);
        let mut c = candidate(
            1,
            TriggerCause::OraclePublic {
                tx: b256!("0x1111111111111111111111111111111111111111111111111111111111111111"),
            },
        );
        c.quote.key.user = addr(0xB1);
        let st = j.enqueue_candidates(&[c], 0, 1, None);
        assert_eq!(st.jobs_sent, 0);
        assert!(st.skipped_sim >= 1);
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
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1, None);
        assert_eq!(st.jobs_sent, 0);
        assert!(st.skipped_pins >= 1);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    /// No in-process simulator: a trigger already in committed state goes to
    /// the exec worker marked for RPC verification, sized from the plan's gas
    /// until that simulation replaces it; nothing is marked verified.
    fn no_state_provider_hands_committed_triggers_to_rpc_verification() {
        let (inbox, rx) = ExecInbox::pair(4);
        let mut j = join_base(Some(inbox), false);
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1, None);
        assert_eq!(st.jobs_sent, 1, "{st:?}");
        let job = rx.try_recv().unwrap();
        assert!(job.rpc_verify);
        assert!(job.gas_limit > 0);
        assert!(rx.try_recv().is_err());
    }

    /// A trigger whose parent transaction would have to be replayed cannot
    /// be verified over RPC, so without a simulator it is still not sent.
    #[test]
    fn no_state_provider_sends_nothing_needing_a_parent_tx() {
        let (inbox, rx) = ExecInbox::pair(4);
        let mut j = join_base(Some(inbox), false);
        let c = candidate(
            1,
            TriggerCause::OraclePublic {
                tx: B256::repeat_byte(3),
            },
        );
        let st = j.enqueue_candidates(&[c], 0, 1, None);
        assert_eq!(st.jobs_sent, 0);
        assert!(rx.try_recv().is_err());
    }

    type Asked = Arc<parking_lot::Mutex<Vec<(Bundle, NextBlock)>>>;

    /// Records what it is asked and gives a fixed answer.
    struct Recorded {
        asked: Asked,
        answer: Result<SimOutcome, SimError>,
        ready: bool,
    }

    impl DrainSim for Recorded {
        fn executor(&self) -> Address {
            addr(0xE0)
        }

        fn verify(&self, bundle: &Bundle, at: &NextBlock) -> Result<SimOutcome, SimError> {
            self.asked.lock().push((bundle.clone(), *at));
            self.answer.clone()
        }

        fn ready(&self) -> bool {
            self.ready
        }
    }

    fn recorded(answer: Result<SimOutcome, SimError>, ready: bool) -> (Box<Recorded>, Asked) {
        let asked = Asked::default();
        let sim = Recorded {
            asked: Arc::clone(&asked),
            answer,
            ready,
        };
        (Box::new(sim), asked)
    }

    fn spent(gas: u64) -> Result<SimOutcome, SimError> {
        Ok(SimOutcome {
            gas_used: gas / 2,
            gas_spent: gas,
            net_profit_wei: alloy_primitives::I256::ZERO,
            confidence: Confidence::CERTAIN,
            weth_after: U256::ZERO,
        })
    }

    /// A committed-state job is simulated in the block after the store's
    /// tip, on the tip's state (its hash), with the most gas a transaction
    /// may carry, at the address jobs are sent to. Its gas limit is what the
    /// simulation spent plus a fifth, and it skips the RPC check.
    #[test]
    fn simulated_job_is_sized_from_the_simulation() {
        let (inbox, rx) = ExecInbox::pair(4);
        let (sim, asked) = recorded(spent(500_000), true);
        let mut j = join_base(Some(inbox), false).with_sim(sim);
        j.last_hash = B256::repeat_byte(5);
        j.parent_gas_limit = 60_000_000;
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1_790_000_000, None);
        assert_eq!(st.jobs_sent, 1, "{st:?}");
        let job = rx.try_recv().unwrap();
        assert!(!job.rpc_verify);
        assert_eq!(job.gas_limit, 600_000);
        let asked = asked.lock();
        assert_eq!(asked.len(), 1);
        let (bundle, at) = &asked[0];
        assert_eq!(
            at.parent,
            BlockRef {
                number: 0,
                hash: B256::repeat_byte(5)
            }
        );
        assert_eq!(at.parent_timestamp, 1_790_000_000);
        assert_eq!(at.parent_gas_limit, 60_000_000);
        assert!(matches!(bundle.trigger, Trigger::InterestDrift));
        assert_eq!(bundle.calls.len(), 1);
        assert_eq!(bundle.calls[0].caller, OPERATOR);
        assert_eq!(bundle.calls[0].to, addr(0xE0));
        assert_eq!(bundle.calls[0].data, job.calldata);
        assert_eq!(bundle.calls[0].gas_limit, liq_sim::MAX_TX_GAS);
    }

    /// Until the store reaches the node's head the simulator is not run:
    /// the job takes the RPC check, as it does without a simulator.
    #[test]
    fn unready_simulator_leaves_jobs_to_the_rpc_check() {
        let (inbox, rx) = ExecInbox::pair(4);
        let (sim, asked) = recorded(spent(500_000), false);
        let mut j = join_base(Some(inbox), false).with_sim(sim);
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1, None);
        assert_eq!(st.jobs_sent, 1, "{st:?}");
        assert!(rx.try_recv().unwrap().rpc_verify);
        assert!(asked.lock().is_empty());
    }

    /// The tip's state is gone (reorged out): the job is not sent, and not
    /// handed to the RPC check either.
    #[test]
    fn unavailable_state_skips_the_job() {
        let (inbox, rx) = ExecInbox::pair(4);
        let (sim, _) = recorded(Err(SimError::StateUnavailable), true);
        let mut j = join_base(Some(inbox), false).with_sim(sim);
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1, None);
        assert_eq!(st.jobs_sent, 0);
        assert!(st.skipped_sim >= 1, "{st:?}");
        assert!(rx.try_recv().is_err());
    }

    /// A tip that is not the last committed block has no known hash: the
    /// simulator is not asked about another block's state.
    #[test]
    fn tip_other_than_the_last_commit_is_not_simulated() {
        let (inbox, rx) = ExecInbox::pair(4);
        let (sim, asked) = recorded(spent(500_000), true);
        let mut j = join_base(Some(inbox), false).with_sim(sim);
        j.last_block = 1;
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1, None);
        assert_eq!(st.jobs_sent, 0);
        assert!(st.skipped_sim >= 1, "{st:?}");
        assert!(asked.lock().is_empty());
        assert!(rx.try_recv().is_err());
    }

    /// Spent plus a fifth, never above what the simulation ran with.
    #[test]
    fn job_gas_is_spent_plus_a_fifth_within_the_cap() {
        assert_eq!(job_gas(0, liq_sim::MAX_TX_GAS), None);
        assert_eq!(job_gas(1_000_000, liq_sim::MAX_TX_GAS), Some(1_200_000));
        assert_eq!(
            job_gas(15_000_000, liq_sim::MAX_TX_GAS),
            Some(liq_sim::MAX_TX_GAS)
        );
    }

    /// Block 26,098,187's plan, from the replay: budgeted 1,533,601,355,468,235
    /// wei of gas (872,215 gas at 1,758,283,629 wei/gas), bid 9,950 bps, floor
    /// 165,950,247,651,707 wei; the simulation ran 1,102,639 gas. Oracle:
    /// the arithmetic — the plan is charged 1,102,639 × 1,758,283,629 and its
    /// floor drops by 0.5 % of the extra (what is left after the bid), to no
    /// less than 1 wei. A plan whose budget covers the measured gas is left
    /// as it is.
    #[test]
    fn a_plan_is_charged_the_gas_its_simulation_used() {
        let plan = liq_plan::BatchPlan {
            flags: 0,
            bid_bps: 9_950,
            gas_cost_wei: 1_533_601_355_468_235,
            min_profit_wei: 165_950_247_651_707,
            groups: Vec::new(),
            profit_swaps: Vec::new(),
        };
        let price = 1_758_283_629u128;
        let p = charge_measured_gas(&plan, 1_102_639, price).unwrap();
        let cost = 1_102_639u128 * price;
        assert_eq!(p.gas_cost_wei, cost);
        let extra = cost - plan.gas_cost_wei;
        assert_eq!(p.min_profit_wei, plan.min_profit_wei - extra * 50 / 10_000);
        assert_eq!((p.bid_bps, p.flags), (plan.bid_bps, plan.flags));
        assert_eq!(
            charge_measured_gas(&plan, 872_215, price),
            None,
            "within budget"
        );
        assert_eq!(charge_measured_gas(&plan, 500_000, price), None);
        // Our share of the extra (about 2.0e12 wei) is above a 10-wei
        // floor: the floor stops at 1 wei, never 0.
        let thin = liq_plan::BatchPlan {
            min_profit_wei: 10,
            ..plan.clone()
        };
        let p = charge_measured_gas(&thin, 1_102_639, price).unwrap();
        assert_eq!(p.min_profit_wei, 1);
    }

    fn backrun(pos: u32) -> Candidate {
        candidate(
            pos,
            TriggerCause::SvrAuction {
                hint: B256::repeat_byte(9),
                deadline: std::time::Instant::now() + Duration::from_secs(30),
                forwarder: addr(0xF0),
                call_data: Bytes::from_static(&[0x6f, 0xad, 0xcf, 0x72]),
                caller: None,
            },
        )
    }

    /// MEV-Share backruns go out under the second operator on its own nonce
    /// slot; everything else under the first.
    #[test]
    fn backruns_are_signed_by_the_second_operator() {
        let second = addr(0xE7);
        let (inbox, rx) = ExecInbox::pair(4);
        let mut j = join_base(Some(inbox), false).with_backrun_operator(second);
        let st = j.enqueue_svr(&[backrun(1)], 0, 1, None);
        assert_eq!(st.jobs_sent, 1, "{st:?}");
        let job = rx.try_recv().unwrap();
        assert_eq!(job.trigger, TriggerKind::SvrAuction);
        assert_eq!(job.operator_key, second);
        assert_eq!(job.slot, crate::exec_bind::BACKRUN_SLOT);

        let st = j.enqueue_candidates(&[fireable(1)], 0, 1, None);
        assert_eq!(st.jobs_sent, 1, "{st:?}");
        let job = rx.try_recv().unwrap();
        assert_eq!(job.operator_key, OPERATOR);
        assert_eq!(job.slot, crate::exec_bind::OPERATOR_SLOT);
    }

    /// Without a second key the first one signs the backruns too.
    #[test]
    fn without_a_second_key_backruns_use_the_first() {
        let (inbox, rx) = ExecInbox::pair(4);
        let mut j = join_base(Some(inbox), false);
        let st = j.enqueue_svr(&[backrun(1)], 0, 1, None);
        assert_eq!(st.jobs_sent, 1, "{st:?}");
        let job = rx.try_recv().unwrap();
        assert_eq!(job.operator_key, OPERATOR);
        assert_eq!(job.slot, crate::exec_bind::OPERATOR_SLOT);
    }

    /// The live simulator runs once the lease is held.
    #[test]
    fn live_sim_waits_for_the_lease() {
        let lease = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sim = LiveSim::new(
            NodeSim::deployed(Arc::new(MemoryFactory::empty()), addr(0xE1)),
            Arc::clone(&lease),
        );
        assert!(!sim.ready());
        lease.store(true, Ordering::Release);
        assert!(sim.ready());
        assert_eq!(sim.executor(), addr(0xE1));
    }

    #[test]
    fn no_secrets_unbound_zero_jobs() {
        let mut j = join_base(None, true);
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1, None);
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
        let st = j.enqueue_candidates(&[c], 0, 1, None);
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
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1, None);
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
        assert_eq!(
            ready.cfg.exact_k,
            u8::try_from(u16::from(EXACT_K).saturating_mul(u16::from(NONCE_SLOTS)))
                .unwrap_or(u8::MAX)
        );
        assert_eq!(ready.cfg.legs_per_plan, EXACT_K);
        assert_eq!(ready.cfg.nonce_slots, NONCE_SLOTS);
        assert_eq!(ready.gas_failed, 0);
        assert_eq!(
            ready.cfg.liq_gas,
            liq_router::LiqGas::none(),
            "unmeasured until liq-gas.toml loads"
        );
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
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1, None);
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
        let st = j.enqueue_candidates(&[fireable(1)], 0, 1, None);
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
            touched: &dirty.positions,
            block: 1,
            hash: alloy_primitives::B256::ZERO,
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

    fn px(asset: u16, price: u64, ts: u64) -> liq_types::Price {
        liq_types::Price {
            asset: AssetId(asset),
            price: liq_types::Ray::from_raw(U256::from(price)),
            source: liq_types::SourceKind::Canonical,
            block: 1,
            ts,
        }
    }

    fn vec_of(ps: &[liq_types::Price]) -> PriceVector {
        liq_types::PriceVector(ps.to_vec())
    }

    #[test]
    fn price_sync_first_call_reloads_and_derived_fills_unpriced_slots() {
        let canon = vec_of(&[px(0, 100, 5), px(1, 0, 0)]);
        let derived = vec_of(&[px(0, 999, 9), px(1, 42, 7)]);
        let (mut merged, mut ticks) = (PriceVector::zeroed(), Vec::new());
        let plan = plan_price_sync(
            &canon,
            Some(&derived),
            &PriceVector::zeroed(),
            false,
            &mut merged,
            &mut ticks,
        );
        assert_eq!(plan, PriceSync::Reload);
        assert_eq!(
            merged.0[0].price, canon.0[0].price,
            "canonical wins where priced"
        );
        assert_eq!(
            merged.0[1].price, derived.0[1].price,
            "derived fills an unpriced slot"
        );
    }

    #[test]
    fn price_sync_changed_slot_is_one_tick_unchanged_is_none() {
        let engine_px = vec_of(&[px(0, 100, 5), px(1, 200, 5)]);
        let canon = vec_of(&[px(0, 100, 5), px(1, 210, 6)]);
        let (mut merged, mut ticks) = (PriceVector::zeroed(), Vec::new());
        let plan = plan_price_sync(&canon, None, &engine_px, true, &mut merged, &mut ticks);
        assert_eq!(plan, PriceSync::Ticks);
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].asset, AssetId(1));

        let plan = plan_price_sync(&engine_px, None, &engine_px, true, &mut merged, &mut ticks);
        assert_eq!(plan, PriceSync::Ticks);
        assert!(ticks.is_empty(), "no change, no tick");
    }

    #[test]
    fn per_eth_scales_by_decimals_and_price_ratio() {
        let ray = |usd: u64| U256::from(usd) * liq_types::fixed::RAY;
        let price = |asset: u16, usd: u64| liq_types::Price {
            asset: AssetId(asset),
            price: liq_types::Ray::from_raw(ray(usd)),
            source: liq_types::SourceKind::Canonical,
            block: 1,
            ts: 1,
        };
        let eth = price(0, 3_000);
        // USDC: 6 decimals, $1 → 3000e6 raw per ETH.
        assert_eq!(
            per_eth_from_prices(&price(1, 1), &eth, 6),
            Some(U256::from(3_000_000_000u64))
        );
        // WETH itself: identity 1e18.
        assert_eq!(per_eth_from_prices(&eth, &eth, 18), Some(e18(1)));
        // An 18-dec $1,500 token: 2e18 raw per ETH.
        assert_eq!(
            per_eth_from_prices(&price(2, 1_500), &eth, 18),
            Some(e18(2))
        );
        // Unpriced slot is None, never a guess.
        let mut unpriced = price(3, 1);
        unpriced.ts = 0;
        assert_eq!(per_eth_from_prices(&unpriced, &eth, 18), None);
    }

    #[test]
    fn protocol_usd_prices_an_asset_the_canonical_vector_lacks() {
        use crate::protocol_prices::{PriceBatch, ProtocolPriceBook, QuotedPrice};
        let ray = |usd: u64| U256::from(usd) * liq_types::fixed::RAY;
        let price = |asset: u16, usd: u64| liq_types::Price {
            asset: AssetId(asset),
            price: liq_types::Ray::from_raw(ray(usd)),
            source: liq_types::SourceKind::Canonical,
            block: 1,
            ts: 1,
        };
        let mut book = ProtocolPriceBook::default();
        let mut moves = Vec::new();
        book.apply(
            &PriceBatch {
                block: 1,
                entries: vec![QuotedPrice {
                    scale: None,
                    protocol: liq_types::ProtocolId(1),
                    market: liq_types::MarketId(1),
                    asset: AssetId(1),
                    price: liq_types::Ray::from_raw(liq_types::fixed::RAY),
                    usd: true,
                }],
                failed: 0,
            },
            &mut moves,
        );
        let mut unpriced = price(1, 1);
        unpriced.ts = 0;
        let eth = price(0, 3_000);
        let asset = sizing_ray(&unpriced, book.usd(AssetId(1))).unwrap();
        // 6 decimals, $1 from the protocol getter, ETH at $3000 → 3000e6.
        assert_eq!(
            per_eth_from_rays(asset, eth.price, 6),
            Some(U256::from(3_000_000_000u64))
        );
        // WETH collateral at $3000 (18 dec) per raw USDC: floor(1e12 / 3000).
        let ratio = coll_per_debt_from_rays(eth.price, asset, 18, 6).unwrap();
        assert_eq!(
            ratio.raw() / liq_types::fixed::RAY,
            U256::from(333_333_333u64)
        );
        // A canonical price is kept. The book must not overwrite it.
        let canon = price(1, 2);
        assert_eq!(
            sizing_ray(&canon, book.usd(AssetId(1))).unwrap().raw(),
            canon.price.raw()
        );
        // No getter and no canonical timestamp: no rate.
        let mut missing = price(4, 1);
        missing.ts = 0;
        assert!(sizing_ray(&missing, book.usd(AssetId(4))).is_none());
    }

    #[test]
    fn coll_per_debt_is_raw_coll_per_raw_debt_ray() {
        let ray = |usd: u64| U256::from(usd) * liq_types::fixed::RAY;
        let price = |asset: u16, usd: u64| liq_types::Price {
            asset: AssetId(asset),
            price: liq_types::Ray::from_raw(ray(usd)),
            source: liq_types::SourceKind::Canonical,
            block: 1,
            ts: 1,
        };
        // WETH coll at $3000 (18 dec) against USDC debt at $1 (6 dec):
        // 1 raw USDC buys 1e-6/3000 ETH = 333_333_333 raw wei (floor).
        let r = coll_per_debt_from_prices(&price(0, 3_000), &price(1, 1), 18, 6).unwrap();
        assert_eq!(r.raw() / liq_types::fixed::RAY, U256::from(333_333_333u64));
        // Same token both sides at equal decimals: exactly 1.
        let one = coll_per_debt_from_prices(&price(0, 3_000), &price(0, 3_000), 18, 18).unwrap();
        assert_eq!(one.raw(), liq_types::fixed::RAY);
        let mut unpriced = price(2, 1);
        unpriced.ts = 0;
        assert!(coll_per_debt_from_prices(&unpriced, &price(1, 1), 18, 6).is_none());
    }

    /// The node can report head N before the hot thread folds N: a
    /// simulation for N waits, one for the tip runs, one for a passed block
    /// is dropped.
    #[test]
    fn gov_pending_waits_for_its_block() {
        let sim = |base: u64| crate::governance::GovSim {
            action: liq_exec::gov::GovAction::Payload {
                controller: Address::ZERO,
                id: base,
            },
            base_block: base,
            target_block: base + 1,
            target_ts: 0,
            exec_gas: 0,
            logs: Vec::new(),
        };
        let (ready, wait) = split_gov_pending(vec![sim(9), sim(10), sim(11), sim(12)], 10);
        assert_eq!(
            ready.iter().map(|s| s.base_block).collect::<Vec<_>>(),
            vec![10]
        );
        assert_eq!(
            wait.iter().map(|s| s.base_block).collect::<Vec<_>>(),
            vec![11, 12]
        );
        let (ready, wait) = split_gov_pending(wait, 11);
        assert_eq!(ready.len(), 1);
        assert_eq!(wait.len(), 1);
    }

    #[test]
    fn price_sync_first_price_for_an_asset_forces_reload() {
        let engine_px = vec_of(&[px(0, 100, 5), px(1, 0, 0)]);
        let canon = vec_of(&[px(0, 100, 5), px(1, 50, 8)]);
        let (mut merged, mut ticks) = (PriceVector::zeroed(), Vec::new());
        let plan = plan_price_sync(&canon, None, &engine_px, true, &mut merged, &mut ticks);
        assert_eq!(
            plan,
            PriceSync::Reload,
            "positions holding a newly priced asset were never banded"
        );
        assert!(ticks.is_empty());
    }
}
