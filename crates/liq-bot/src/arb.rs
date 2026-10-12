//! The backrun engine thread (`liq-arb`, the `arb` branch).
//!
//! One thread, `liq-arb-engine`, owns a [`liq_arb::Engine`] — its own copy
//! of the pool book — and nothing else touches it. Inputs arrive by SPSC:
//!
//! * [`ArbTick`] from the hot thread's after-block, last, once the block's
//!   liquidations are queued: the block, the next base fee, the flashable
//!   assets' prices, and the pools those liquidations' exits use (kept out
//!   of anchors for the block). Fixed-size, so the push allocates nothing.
//!   The engine then copies in the pools the block changed
//!   ([`liq_router::PoolBook::sync_from`]) under one short read of the
//!   node's book.
//! * MEV-Share hints that share a swap's logs (the reader's second ring):
//!   each is a victim, folded on the copy and backrun.
//!
//! What it finds goes to the submit worker as an `ExecJob` on its own inbox,
//! signed by the arb key and sent to `ArbExecutor` (`TriggerKind::ArbMevShare`). The thread
//! runs on its own isolated CPU (`cores.toml`: the fourth sim worker's) and
//! parks briefly when idle rather than holding the core in a spin.

use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, B256, U256};
use arc_swap::ArcSwap;
use liq_arb::{Engine, EngineCfg, FlashSource, LogRef, Target};
use liq_flash::FlashIndex;
use liq_router::{PoolBook, PoolId, SolveBudget};
use liq_types::{AssetId, FlashProvider, MevShareHint, PendingPublic};
use parking_lot::RwLock;

/// The `cores.toml` slot.
pub const ARB_THREAD: &str = "liq-arb-engine";
/// Flashable assets a tick carries prices for.
pub const MAX_FLASHABLE: usize = 8;
/// Liquidation exit pools a tick can reserve.
pub const MAX_RESERVED: usize = 16;
/// Idle wait between empty polls.
const IDLE: Duration = Duration::from_micros(100);

/// `config/arb.toml`.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct ArbToml {
    pub enabled: bool,
    pub flashable: Vec<Address>,
    pub base_gas: u64,
    pub max_evals: u16,
    pub bid_bps: u16,
    pub min_net_wei: u128,
    pub flash_swap: bool,
    pub sweep: bool,
}

impl ArbToml {
    /// The committed file, or `None` (logged) when absent or malformed.
    pub fn load(path: &std::path::Path) -> Option<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(r) => r,
            Err(e) => {
                tracing::info!(path = %path.display(), error = %e, "arb config not read — backrun engine off");
                return None;
            }
        };
        match toml::from_str::<Self>(&raw) {
            Ok(t) if t.flashable.len() <= MAX_FLASHABLE => Some(t),
            Ok(t) => {
                tracing::error!(
                    n = t.flashable.len(),
                    cap = MAX_FLASHABLE,
                    "arb config names too many flashable assets — engine off"
                );
                None
            }
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "arb config malformed — engine off");
                None
            }
        }
    }

    /// The engine's fixed terms, the flashable tokens interned. `None`
    /// (logged) when a token is not interned.
    pub fn engine_cfg(
        &self,
        intern: &liq_config::Intern,
        weth: Address,
    ) -> Option<(EngineCfg, Vec<AssetId>)> {
        let mut flashable = Vec::with_capacity(self.flashable.len());
        for a in &self.flashable {
            match intern.asset(*a) {
                Some(id) => flashable.push(id),
                None => {
                    tracing::error!(token = %a, "arb flashable token not interned — engine off");
                    return None;
                }
            }
        }
        let weth_id = intern.asset(weth)?;
        let cfg = EngineCfg {
            weth,
            weth_id,
            flashable: flashable.clone(),
            budget: SolveBudget::default(),
            base_gas: self.base_gas,
            max_evals: self.max_evals,
            bid_bps: self.bid_bps,
            min_net_wei: U256::from(self.min_net_wei),
            min_profit_wei: 1,
            sweep: self.sweep,
            flash_swap: self.flash_swap,
        };
        Some((cfg, flashable))
    }
}

/// One block, as the arb engine needs it. `Copy`: the hot thread's push
/// allocates nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArbTick {
    /// The committed block.
    pub block: u64,
    pub timestamp: u64,
    /// The next block's fees (the drain's quote for this block): its base
    /// fee prices the solve, and the job carries the quote.
    pub fee: liq_exec::fee::FeeQuote,
    /// Raw units per `1e18` wei of each flashable asset, in config order;
    /// zero unpriced.
    pub per_eth: [U256; MAX_FLASHABLE],
    /// Pools this block's queued liquidations exit through.
    pub reserved: [u32; MAX_RESERVED],
    pub n_reserved: u8,
}

/// Planned gas to gas limit: the fork suite measured `execute` at 0.5-1.23x
/// the plan's gas; half again covers every case, and only gas used is paid.
const GAS_LIMIT_PCT: u64 = 150;
/// Blocks after the target a MEV-Share backrun stays valid for: the hinted
/// transaction may land a little later than the next block.
const EXTRA_BLOCKS: u64 = 2;
/// Arb trace ids carry the top bit, so they never meet a liquidation's.
const ARB_TRACE: u64 = 1 << 63;

/// The hot thread's side of the arb engine's input, held by the drain: the
/// tick ring, which assets to price, and the pools this block's sent
/// liquidations exit through.
pub struct ArbFeed {
    pub tick: rtrb::Producer<ArbTick>,
    /// The engine's flashable assets, in config order (the tick's `per_eth`).
    pub flashable: Vec<AssetId>,
    reserved: smallvec::SmallVec<[u32; MAX_RESERVED]>,
    /// Ticks the ring refused (the engine fell a block behind).
    pub dropped: u64,
}

impl ArbFeed {
    #[must_use]
    pub fn new(tick: rtrb::Producer<ArbTick>, flashable: Vec<AssetId>) -> Self {
        Self {
            tick,
            flashable,
            reserved: smallvec::SmallVec::new(),
            dropped: 0,
        }
    }

    /// A new block: no liquidation has claimed a pool yet.
    pub fn begin_block(&mut self) {
        self.reserved.clear();
    }

    /// A liquidation was sent whose exit uses `pools`: keep backruns off
    /// them for the block. Past [`MAX_RESERVED`] the rest are not kept (the
    /// largest exits come first in a plan; the cap is far above a block's
    /// usual few).
    pub fn reserve(&mut self, pools: &[u32]) {
        for &p in pools {
            if self.reserved.len() < MAX_RESERVED && !self.reserved.contains(&p) {
                self.reserved.push(p);
            }
        }
    }

    /// Hand the block to the engine: one non-blocking push, nothing
    /// allocated. `price` answers raw units per `1e18` wei of an asset.
    pub fn end_block(
        &mut self,
        block: u64,
        timestamp: u64,
        fee: liq_exec::fee::FeeQuote,
        price: impl Fn(AssetId) -> Option<U256>,
    ) {
        let mut per_eth = [U256::ZERO; MAX_FLASHABLE];
        for (slot, &a) in per_eth.iter_mut().zip(&self.flashable) {
            *slot = price(a).unwrap_or_default();
        }
        let mut reserved = [0u32; MAX_RESERVED];
        for (slot, &p) in reserved.iter_mut().zip(&self.reserved) {
            *slot = p;
        }
        let tick = ArbTick {
            block,
            timestamp,
            fee,
            per_eth,
            reserved,
            n_reserved: u8::try_from(self.reserved.len()).unwrap_or(u8::MAX),
        };
        if self.tick.push(tick).is_err() {
            self.dropped = self.dropped.saturating_add(1);
            tracing::warn!(
                block,
                dropped = self.dropped,
                "arb tick ring full — engine behind"
            );
        }
    }
}

/// The pools a liquidation plan's exits swap through (`ArbFeed::reserve`).
#[must_use]
pub fn exit_pools(plan: &liq_router::SelectedPlan) -> smallvec::SmallVec<[u32; MAX_RESERVED]> {
    let mut out = smallvec::SmallVec::new();
    for g in &plan.groups {
        for l in &g.legs {
            liq_arb::realize::for_each_pool(&l.leg.exit, |p| {
                if !out.contains(&p.0) && out.len() < MAX_RESERVED {
                    out.push(p.0);
                }
            });
        }
    }
    out
}

/// Who signs and where it goes: the arb key's address and its slot in the
/// arb path's signers.
#[derive(Clone, Copy, Debug)]
pub struct ArbSigner {
    pub operator: Address,
    pub slot: usize,
}

/// What the engine thread holds across steps.
pub struct ArbIo {
    pub book: Arc<RwLock<PoolBook>>,
    pub flash: Arc<ArcSwap<FlashIndex>>,
    pub ticks: rtrb::Consumer<ArbTick>,
    pub hints: rtrb::Consumer<MevShareHint>,
    /// Pending public-mempool transactions from Reth's pool (the ExEx's
    /// listener task).
    pub pending: rtrb::Consumer<PendingPublic>,
    pub out: crossbeam_channel::Sender<liq_exec::path::ExecJob>,
}

/// The engine and the buffers its steps reuse.
pub struct ArbThread {
    engine: Engine,
    flashable: Vec<AssetId>,
    per_eth: Vec<U256>,
    sources: Vec<FlashSource>,
    target: u64,
    fee: Option<liq_exec::fee::FeeQuote>,
    signer: ArbSigner,
    bid_bps: u16,
    traces: u64,
    /// Pending public transactions not yet backrun or expired, with the
    /// block each arrived in.
    pending: std::collections::VecDeque<(PendingPublic, u64)>,
    /// Jobs the submit worker's inbox refused (full).
    pub dropped: u64,
}

/// Pending public transactions kept at once; past it the oldest go.
const PENDING_KEEP: usize = 2048;
/// Blocks a pending transaction is kept for: one that has not landed by
/// then is not likely to be the next block's.
const PENDING_BLOCKS: u64 = 2;

impl ArbThread {
    #[must_use]
    pub fn new(
        book: &PoolBook,
        cfg: EngineCfg,
        flashable: Vec<AssetId>,
        n_assets: usize,
        signer: ArbSigner,
    ) -> Self {
        let bid_bps = cfg.bid_bps;
        Self {
            engine: Engine::new(book, cfg),
            flashable,
            per_eth: vec![U256::ZERO; n_assets],
            sources: Vec::with_capacity(2 * MAX_FLASHABLE),
            target: 0,
            fee: None,
            signer,
            bid_bps,
            traces: 0,
            pending: std::collections::VecDeque::with_capacity(PENDING_KEEP),
            dropped: 0,
        }
    }

    /// One pass: the newest tick (older ones are superseded), then every
    /// queued hint. Returns how many inputs it handled.
    pub fn step(&mut self, io: &mut ArbIo) -> usize {
        let mut handled = 0usize;
        let mut latest = None;
        while let Ok(t) = io.ticks.pop() {
            latest = Some(t);
            handled = handled.saturating_add(1);
        }
        if let Some(t) = latest {
            self.on_tick(&t, io);
        }
        while let Ok(h) = io.hints.pop() {
            handled = handled.saturating_add(1);
            self.on_hint(&h, io);
        }
        while let Ok(p) = io.pending.pop() {
            handled = handled.saturating_add(1);
            if self.pending.len() == PENDING_KEEP {
                self.pending.pop_front();
            }
            self.pending.push_back((p, self.target));
        }
        handled
    }

    /// Pending public transactions waiting, oldest first.
    pub fn pending(&self) -> impl Iterator<Item = &(PendingPublic, u64)> {
        self.pending.iter()
    }

    fn on_tick(&mut self, t: &ArbTick, io: &ArbIo) {
        for p in &mut self.per_eth {
            *p = U256::ZERO;
        }
        for (a, px) in self.flashable.iter().zip(t.per_eth) {
            if let Some(slot) = self.per_eth.get_mut(usize::from(a.0)) {
                *slot = px;
            }
        }
        self.sources.clear();
        let index = io.flash.load();
        for &a in &self.flashable {
            for e in index.entries(a) {
                if e.fee_bps == 0
                    && matches!(e.provider, FlashProvider::UniV4 | FlashProvider::Morpho)
                {
                    self.sources.push(FlashSource {
                        asset: a,
                        provider: e.provider,
                        source: e.source,
                        available: e.available,
                    });
                }
            }
        }
        let target = Target {
            number: t.block.saturating_add(1),
            timestamp: t.timestamp.saturating_add(12),
            base_fee_wei: t.fee.next_base_fee,
        };
        let copied = {
            let book = io.book.read();
            self.engine
                .on_block(&book, target, &self.per_eth, &self.sources)
        };
        let n = usize::from(t.n_reserved).min(MAX_RESERVED);
        let mut reserved = [PoolId(0); MAX_RESERVED];
        for (r, &id) in reserved.iter_mut().zip(t.reserved.get(..n).unwrap_or(&[])) {
            *r = PoolId(id);
        }
        self.engine.reserve(reserved.get(..n).unwrap_or(&[]));
        self.target = target.number;
        self.fee = Some(t.fee);
        let keep_from = target.number.saturating_sub(PENDING_BLOCKS);
        self.pending.retain(|(_, seen)| *seen >= keep_from);
        tracing::debug!(
            block = t.block,
            copied,
            sources = self.sources.len(),
            "arb: block"
        );
    }

    fn on_hint(&mut self, h: &MevShareHint, io: &ArbIo) {
        if self.target == 0 {
            return;
        }
        let Some(logs) = h.logs.as_deref() else {
            return;
        };
        let mut refs: smallvec::SmallVec<[LogRef<'_>; 16]> = smallvec::SmallVec::new();
        for l in logs {
            refs.push(LogRef {
                address: l.address,
                topics: l.data.topics(),
                data: &l.data.data,
            });
        }
        match self.engine.on_victim(&refs) {
            Ok(Ok(o)) => {
                let Some(job) = self.job(&o, Some(h.hash), liq_types::TriggerKind::ArbMevShare)
                else {
                    return;
                };
                tracing::info!(hint = %h.hash, net_wei = %o.backrun.net_wei, evals = o.backrun.evals, "arb: backrun");
                if io.out.try_send(job).is_err() {
                    self.dropped = self.dropped.saturating_add(1);
                    tracing::warn!(
                        dropped = self.dropped,
                        "arb: submit inbox full — job dropped"
                    );
                }
            }
            Ok(Err(why)) => tracing::trace!(hint = %h.hash, ?why, "arb: no backrun"),
            Err(e) => tracing::warn!(hint = %h.hash, error = %e, "arb: solve refused"),
        }
    }

    /// The submit job for `o`: `ArbExecutor.execute(plan)` from the arb key,
    /// for the target block and [`EXTRA_BLOCKS`] after, scoped for the risk
    /// gate as the arb protocol on the anchor pool's market.
    fn job(
        &mut self,
        o: &liq_arb::Opportunity,
        hint: Option<B256>,
        trigger: liq_types::TriggerKind,
    ) -> Option<liq_exec::path::ExecJob> {
        use liq_types::{MarketId, PositionKey, TraceId, ARB_PROTOCOL};
        let fee = self.fee?;
        self.traces = self.traces.wrapping_add(1);
        let market = MarketId(o.backrun.anchor.pool.0);
        let plan = Bytes::from(o.plan.as_bytes().to_vec());
        Some(liq_exec::path::ExecJob {
            trace: TraceId::from_raw(ARB_TRACE | self.traces),
            calldata: liq_sim::execute_calldata(&plan),
            plan,
            trigger,
            protocol: ARB_PROTOCOL,
            market,
            collateral: o.backrun.start,
            debt: o.other,
            flash: o.flash,
            operator_key: self.signer.operator,
            position: PositionKey {
                protocol: ARB_PROTOCOL,
                market,
                user: Address::ZERO,
            },
            hint_hash: hint,
            backrun_tx: None,
            parents: Vec::new(),
            target_block: self.target,
            max_block: self.target.saturating_add(EXTRA_BLOCKS),
            fee,
            auction_bps: self.bid_bps,
            gas_limit: o.backrun.gas.saturating_mul(GAS_LIMIT_PCT) / 100,
            chain_id: 1,
            slot: self.signer.slot,
            rpc_verify: false,
        })
    }

    /// Run until `stop` is set: step, and park briefly when idle.
    pub fn run(mut self, mut io: ArbIo, stop: &std::sync::atomic::AtomicBool) {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            if self.step(&mut io) == 0 {
                std::thread::park_timeout(IDLE);
            }
        }
    }
}

/// What [`start`] hands the rest of startup.
pub struct ArbStarted {
    /// For the drain (`DrainJoin::with_arb`).
    pub feed: ArbFeed,
    /// For the exec worker (`spawn_exec_worker`, its `arb` input).
    pub exec: crate::exec_worker::ArbExec,
    /// For the MEV-Share reader (`spawn_hint_reader`, its `arb` input).
    pub hints: rtrb::Producer<MevShareHint>,
    /// For the ExEx's pool listener (`Started::arb_pending`).
    pub pending: rtrb::Producer<PendingPublic>,
    pub thread: std::thread::JoinHandle<()>,
    pub stop: Arc<std::sync::atomic::AtomicBool>,
}

/// Hints the arb ring holds: about 95 arrive per block, and the engine
/// rejects a non-paying one in a few exit solves.
const HINT_RING: usize = 1024;
/// Ticks the ring holds: more than a few behind and the engine is not
/// keeping up anyway (it takes the newest).
const TICK_RING: usize = 8;
/// Jobs waiting for the exec worker.
const JOB_INBOX: usize = 256;

/// Start the backrun engine when `config/arb.toml` enables it, the
/// `ArbExecutor` is configured and its key is present; `None` (logged)
/// otherwise, and the liquidator runs as it did. Binds the arb submit path
/// (`ArbExecutor`, the arb key, the shared builders and identity, its own
/// shadow log), and spawns `liq-arb-engine` on its `cores.toml` CPU — run
/// unpinned only when `allow_unpinned`.
#[allow(clippy::too_many_arguments)]
pub fn start(
    config_dir: &std::path::Path,
    cores_path: &std::path::Path,
    allow_unpinned: bool,
    intern: &liq_config::Intern,
    weth: Address,
    arb_executor: Option<Address>,
    shadow: &std::path::Path,
    shared: &crate::shared::Shared,
    book: Arc<RwLock<PoolBook>>,
) -> Option<ArbStarted> {
    let cfg = ArbToml::load(&config_dir.join("arb.toml"))?;
    if !cfg.enabled {
        tracing::info!("arb.toml: enabled = false — backrun engine off");
        return None;
    }
    let Some(executor) = arb_executor else {
        tracing::error!("venues.arb_executor unset — backrun engine off");
        return None;
    };
    let liq = crate::exec_bind::ProcessSecrets::from_env()?;
    let secrets = crate::exec_bind::ProcessSecrets::arb_from_env(&liq)?;
    let path = crate::exec_bind::bind_from_config(
        &config_dir.join("builders.toml"),
        &shadow.join("arb"),
        shared,
        Some(secrets),
    )?
    .with_executor(executor);
    let operator = path.signers.first()?.address();
    let (engine_cfg, flashable) = cfg.engine_cfg(intern, weth)?;
    let n_assets = intern.asset_id_capacity();
    let (tick_tx, tick_rx) = rtrb::RingBuffer::<ArbTick>::new(TICK_RING);
    let (hint_tx, hint_rx) = rtrb::RingBuffer::<MevShareHint>::new(HINT_RING);
    let (pending_tx, pending_rx) = rtrb::RingBuffer::<PendingPublic>::new(liq_types::RING_CAP);
    let (job_tx, job_rx) = crossbeam_channel::bounded(JOB_INBOX);
    let core = crate::threads::CoreMap::load(cores_path)
        .and_then(|m| m.slot(ARB_THREAD).map(|s| s.core))
        .ok();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let io = ArbIo {
        book: Arc::clone(&book),
        flash: Arc::clone(&shared.flash),
        ticks: tick_rx,
        hints: hint_rx,
        pending: pending_rx,
        out: job_tx,
    };
    let signer = ArbSigner { operator, slot: 0 };
    let thread_flashable = flashable.clone();
    let thread_stop = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name(ARB_THREAD.into())
        .spawn(move || {
            let pinned = core.map(|c| crate::threads::pin_to_core(usize::from(c)));
            match pinned {
                Some(Ok(())) => {}
                other if allow_unpinned => {
                    tracing::warn!(?other, "arb engine unpinned (allowed)");
                }
                other => {
                    tracing::error!(?other, "arb engine not pinned to its core — not started");
                    return;
                }
            }
            // The copy is taken here, off the startup thread; the first
            // tick brings it up to date.
            let snapshot = io.book.read().clone();
            let engine = ArbThread::new(&snapshot, engine_cfg, thread_flashable, n_assets, signer);
            drop(snapshot);
            tracing::info!(%executor, %operator, "arb engine running");
            engine.run(io, &thread_stop);
        })
        .ok()?;
    Some(ArbStarted {
        feed: ArbFeed::new(tick_tx, flashable),
        exec: crate::exec_worker::ArbExec {
            rx: job_rx,
            path: Arc::new(path),
        },
        hints: hint_tx,
        pending: pending_tx,
        thread,
        stop,
    })
}

/// Victims an ordering search permutes; 4! = 24 orders at most.
pub const MAX_ORDERED: usize = 4;

/// The most profitable order of a group of pending transactions that share
/// pools, and the backrun behind it, simulated.
#[derive(Clone, Debug)]
pub struct BestOrder {
    /// Indices into the victims given, in bundle order (ours goes last).
    pub order: smallvec::SmallVec<[usize; MAX_ORDERED]>,
    pub opportunity: liq_arb::Opportunity,
    /// The simulation of that order then our plan: realized WETH, gas.
    pub sim: liq_sim::SimOutcome,
}

/// Step `p` to its next permutation in lexicographic order; `false` once it
/// has wrapped back to ascending (every order seen).
pub fn next_permutation(p: &mut [usize]) -> bool {
    let n = p.len();
    let Some(i) = (1..n)
        .rev()
        .find(|&i| p.get(i.saturating_sub(1)) < p.get(i))
    else {
        p.reverse();
        return false;
    };
    let pivot = i.saturating_sub(1);
    let Some(j) = (i..n).rev().find(|&j| p.get(j) > p.get(pivot)) else {
        return false;
    };
    p.swap(pivot, j);
    if let Some(tail) = p.get_mut(i..) {
        tail.reverse();
    }
    true
}

/// Every order of `victims` (at most [`MAX_ORDERED`]): run them in that
/// order on the simulator's state and take the logs they emit, solve the
/// backrun on those ([`liq_arb::Engine::on_victim`]), then simulate the
/// victims in that order followed by our plan, priced. The order whose
/// simulated backrun keeps the most WETH wins. Orders where a victim
/// reverts are skipped (a user's transaction must not fail in our bundle);
/// so are orders whose plan the simulator refuses. `None` when no order
/// leaves a backrun.
pub fn best_ordering<P: liq_sim::DatabaseRef<Error = liq_sim::SimError>>(
    engine: &mut Engine,
    sim: &mut liq_sim::Simulator<P>,
    env: &liq_sim::SimEnv,
    victims: &[liq_sim::SimTx],
    ours: (Address, Address, u64),
) -> Result<Option<BestOrder>, liq_sim::SimError> {
    use liq_sim::{execute_calldata, verify_ordered, victim_logs, Ordered, SimTx, VictimRun};
    let (operator, executor, gas_limit) = ours;
    let k = victims.len();
    if k == 0 || k > MAX_ORDERED {
        return Ok(None);
    }
    let mut order: smallvec::SmallVec<[usize; MAX_ORDERED]> = (0..k).collect();
    let mut best: Option<BestOrder> = None;
    loop {
        let ordered: Vec<SimTx> = order
            .iter()
            .filter_map(|&i| victims.get(i).cloned())
            .collect();
        if let VictimRun::Logs(per_victim) = victim_logs(sim, &ordered, env)? {
            let refs: smallvec::SmallVec<[LogRef<'_>; 32]> = per_victim
                .iter()
                .flatten()
                .map(|l| LogRef {
                    address: l.address,
                    topics: l.data.topics(),
                    data: &l.data.data,
                })
                .collect();
            if let Ok(Ok(o)) = engine.on_victim(&refs) {
                let call = SimTx {
                    caller: operator,
                    to: executor,
                    value: U256::ZERO,
                    data: execute_calldata(o.plan.as_bytes()),
                    gas_limit,
                    access_list: Vec::new(),
                };
                match verify_ordered(sim, &ordered, &[call], env) {
                    Ok(Ordered::Ran(s))
                        if best
                            .as_ref()
                            .is_none_or(|b| s.net_profit_wei > b.sim.net_profit_wei) =>
                    {
                        best = Some(BestOrder {
                            order: order.clone(),
                            opportunity: o,
                            sim: s,
                        });
                    }
                    Ok(_) => {}
                    Err(liq_sim::SimError::Revert { .. }) => {
                        tracing::warn!(
                            ?order,
                            "arb: plan reverted in simulation — model and chain disagree"
                        );
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        if !next_permutation(&mut order) {
            break;
        }
    }
    Ok(best)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn fee() -> liq_exec::fee::FeeQuote {
        liq_exec::fee::FeeQuote {
            parent_block: 10,
            next_base_fee: 7,
            priority_wei: 1,
            modest_priority_wei: 1,
        }
    }

    /// The feed's tick: the block, its fee, each flashable asset's price in
    /// config order, and the pools sent liquidations claimed — once each,
    /// and only this block's.
    #[test]
    fn feed_ticks_carry_the_block_and_its_claimed_pools() {
        let (prod, mut cons) = rtrb::RingBuffer::<ArbTick>::new(4);
        let mut feed = ArbFeed::new(prod, vec![AssetId(3), AssetId(1)]);
        let price = |a: AssetId| Some(U256::from(u64::from(a.0) * 100));
        feed.begin_block();
        feed.reserve(&[5, 9]);
        feed.reserve(&[9, 2]);
        feed.end_block(10, 120, fee(), price);
        let t = cons.pop().unwrap();
        assert_eq!((t.block, t.timestamp, t.fee), (10, 120, fee()));
        assert_eq!(&t.per_eth[..2], &[U256::from(300u64), U256::from(100u64)]);
        assert!(t.per_eth[2..].iter().all(|p| p.is_zero()));
        assert_eq!(&t.reserved[..usize::from(t.n_reserved)], &[5, 9, 2]);
        feed.begin_block();
        feed.end_block(11, 132, fee(), price);
        assert_eq!(
            cons.pop().unwrap().n_reserved,
            0,
            "a new block starts with nothing claimed"
        );
    }

    /// Oracle: every order of 0..4 exactly once — 24 distinct orders — and
    /// the slice back at ascending afterwards.
    #[test]
    fn permutations_cover_every_order_once() {
        let mut p = [0usize, 1, 2, 3];
        let mut seen = std::collections::HashSet::new();
        loop {
            assert!(seen.insert(p), "repeat {p:?}");
            if !next_permutation(&mut p) {
                break;
            }
        }
        assert_eq!(seen.len(), 24);
        assert_eq!(p, [0, 1, 2, 3]);
        let mut one = [0usize];
        assert!(!next_permutation(&mut one));
    }
}
