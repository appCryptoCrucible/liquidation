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
//!   node's book. With `top_of_block` on in `arb.toml` (off since
//!   2026-10-14: it blocks the hints for seconds a block for little value)
//!   it also solves what the block left open for the top of the next one
//!   (`TriggerKind::ArbTop`: ours alone).
//! * MEV-Share hints that share a swap's logs (the reader's second ring):
//!   each is a victim, folded on the copy and backrun
//!   (`TriggerKind::ArbMevShare`: `{hash}` then ours).
//! * Pending public-mempool transactions (the ExEx's pool listener): those
//!   that share a pool are searched in every subset and order, simulated
//!   (`TriggerKind::ArbPublic`: they in the chosen order, then ours).
//!
//! Hints come first: they are where most backruns are won. Public-mempool
//! work is cut into units (one transaction run alone, or one subset of a
//! group in every order, [`GroupSearch`]) and each step serves every
//! waiting hint before the next unit.
//!
//! Every public and top-of-block backrun is simulated on the node's state
//! before it is signed and must gain what its plan claims
//! ([`crate::arb_check`]); a pool whose model a simulation contradicts on
//! the exact state is quarantined. The coinbase bid comes from the
//! hot-reloaded `[bids]` table of `config/arb.toml`, per intake and per
//! anchor pool ([`BidToml`]).
//!
//! What it finds goes to the submit worker as an `ExecJob` on its own inbox,
//! signed by the arb key (one nonce per target block: a block's backruns
//! are alternatives) and sent to `ArbExecutor`. The thread runs on its own
//! isolated CPU (`cores.toml`) and parks briefly when idle rather than
//! holding the core in a spin.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_primitives::{Address, Bytes, B256, I256, U256};
use arc_swap::ArcSwap;
use liq_arb::{Engine, EngineCfg, FlashSource, LogRef, Opportunity, Target};
use liq_flash::FlashIndex;
use liq_oracle::mevshare::ArbHint;
use liq_protocol::DecodedLog;
use liq_router::{PoolBook, PoolId, SolveBudget};
use liq_sim::{
    block_env_at, executor_anchors, mainnet_spec, verify_ordered_logs, victim_logs, views_after,
    BlockRef, Ordered, SimEnv, SimError, SimTx, Simulator, StateProviderFactory, VictimRun,
};
use liq_types::{AssetId, FlashProvider, MevShareHint, PendingPublic, TriggerKind};
use parking_lot::RwLock;

use crate::arb_check::{chain_view, claim_holds, first_disagreement, view_reads, Disagreement};

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
    pub min_net_wei: u128,
    pub flash_swap: bool,
    pub sweep: bool,
    /// Solve what each block left open, for the top of the next one. Off:
    /// it shares the engine's one thread with the hints and blocks them
    /// while it runs (1-7 s a block on the dev box) for little value.
    pub top_of_block: bool,
    pub bids: BidToml,
}

/// One intake's coinbase bids, basis points of realized net.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct CategoryBid {
    /// Anchored on a pool other backrunners take (every pool not listed
    /// uncontested).
    pub contested_bps: u16,
    /// Anchored on a pool listed in [`BidToml::uncontested`].
    pub uncontested_bps: u16,
}

/// `[bids]` of `config/arb.toml`: the bid per intake, contested or not by
/// the anchor pool. Re-read when the file changes ([`spawn_bid_reload`])
/// and published to the engine thread through an `ArcSwap`; the engine
/// takes a new table at its next block.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct BidToml {
    pub mevshare: CategoryBid,
    pub public: CategoryBid,
    pub top: CategoryBid,
    /// Anchor pool contracts no other backrunner has been seen taking. A
    /// V4, Balancer or Fluid pool shares its singleton's address with every
    /// other pool there and cannot be listed.
    #[serde(default)]
    pub uncontested: Vec<Address>,
}

/// Why a `[bids]` table was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BidError {
    #[error("a bid above 10 000 bps")]
    Range,
    /// The MEV-Share relay takes a refund percentage: whole percents only.
    #[error("a MEV-Share bid that is not a whole percent")]
    NotPercent,
    #[error("a singleton (V4 PoolManager, Balancer Vault, Fluid) listed uncontested")]
    Singleton,
}

impl BidToml {
    /// Refuse a table the submit path would refuse a job from, or that
    /// lists a singleton.
    pub fn validate(&self) -> Result<(), BidError> {
        for c in [self.mevshare, self.public, self.top] {
            if c.contested_bps > 10_000 || c.uncontested_bps > 10_000 {
                return Err(BidError::Range);
            }
        }
        if !self.mevshare.contested_bps.is_multiple_of(100)
            || !self.mevshare.uncontested_bps.is_multiple_of(100)
        {
            return Err(BidError::NotPercent);
        }
        use liq_router::solver::{BALANCER_VAULT, FLUID_LIQUIDITY, V4_POOL_MANAGER};
        if self
            .uncontested
            .iter()
            .any(|a| *a == V4_POOL_MANAGER || *a == BALANCER_VAULT || *a == FLUID_LIQUIDITY)
        {
            return Err(BidError::Singleton);
        }
        Ok(())
    }

    /// The bids for `intake`.
    #[must_use]
    pub fn of(&self, intake: Intake) -> CategoryBid {
        match intake {
            Intake::MevShare => self.mevshare,
            Intake::Public => self.public,
            Intake::Top => self.top,
        }
    }
}

impl ArbToml {
    /// The committed file, or `None` (logged) when absent or malformed.
    pub fn load(path: &Path) -> Option<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(r) => r,
            Err(e) => {
                tracing::info!(path = %path.display(), error = %e, "arb config not read — backrun engine off");
                return None;
            }
        };
        match Self::parse(&raw) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "arb config refused — engine off");
                None
            }
        }
    }

    /// Parse and check a file's text.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let t = toml::from_str::<Self>(raw).map_err(|e| e.to_string())?;
        if t.flashable.len() > MAX_FLASHABLE {
            return Err(format!(
                "{} flashable assets, at most {MAX_FLASHABLE}",
                t.flashable.len()
            ));
        }
        t.bids.validate().map_err(|e| e.to_string())?;
        Ok(t)
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
            // Every call passes the bid table's; this is never read.
            bid_bps: self.bids.mevshare.contested_bps,
            min_net_wei: U256::from(self.min_net_wei),
            min_profit_wei: 1,
            sweep: self.sweep,
            flash_swap: self.flash_swap,
            top_of_block: self.top_of_block,
        };
        Some((cfg, flashable))
    }
}

/// How often the bid reloader looks at `arb.toml`.
const BID_POLL: Duration = Duration::from_secs(2);

/// Watch `path` and publish its `[bids]` to `bids` whenever the file
/// changes and parses; a table that does not parse or validate is logged
/// and the one in force stays. Runs on its own unpinned thread until
/// `stop`.
pub fn spawn_bid_reload(
    path: PathBuf,
    bids: Arc<ArcSwap<BidToml>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("liq-arb-bids".into())
        .spawn(move || {
            let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
            let mut seen = modified(&path);
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(BID_POLL);
                let now = modified(&path);
                if now == seen {
                    continue;
                }
                seen = now;
                let parsed = std::fs::read_to_string(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|raw| ArbToml::parse(&raw));
                match parsed {
                    Ok(t) if *bids.load_full() == t.bids => {}
                    Ok(t) => {
                        tracing::info!(bids = ?t.bids, "arb bids reloaded");
                        bids.store(Arc::new(t.bids));
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "arb.toml changed but refused — bids unchanged");
                    }
                }
            }
        })
}

/// One block, as the arb engine needs it. `Copy`: the hot thread's push
/// allocates nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArbTick {
    /// The committed block.
    pub block: u64,
    pub timestamp: u64,
    /// The committed block's hash: names the state a public-mempool backrun
    /// simulates on (the parent of the target block).
    pub parent_hash: B256,
    /// The next block's fees (the drain's quote for this block): its base
    /// fee prices the solve, and the job carries the quote.
    pub fee: liq_exec::fee::FeeQuote,
    /// Raw units per `1e18` wei of each flashable asset, in config order;
    /// zero unpriced.
    pub per_eth: [U256; MAX_FLASHABLE],
    /// Pools this block's queued liquidations exit through.
    pub reserved: [u32; MAX_RESERVED],
    pub n_reserved: u8,
    /// When the drain began on the block: a top-of-block backrun's
    /// trigger-in.
    pub born: Instant,
}

/// Planned gas to gas limit: the fork suite measured `execute` at 0.5-1.23x
/// the plan's gas; half again covers every case, and only gas used is paid.
const GAS_LIMIT_PCT: u64 = 150;
/// A simulated backrun's gas limit: what the simulation needed available,
/// with a fifth again for state that moves before it lands.
const SIM_GAS_PCT: u64 = 120;
/// Blocks after the target a MEV-Share or public backrun stays valid for:
/// the transaction it follows may land a little later than the next block.
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
    /// `parent_hash` is the committed block's hash — the state a public
    /// backrun simulates on; `born`, when the drain began on it.
    #[allow(clippy::too_many_arguments)]
    pub fn end_block(
        &mut self,
        block: u64,
        timestamp: u64,
        parent_hash: B256,
        fee: liq_exec::fee::FeeQuote,
        born: Instant,
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
            parent_hash,
            fee,
            per_eth,
            reserved,
            n_reserved: u8::try_from(self.reserved.len()).unwrap_or(u8::MAX),
            born,
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

/// Where a backrun's trigger came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intake {
    MevShare,
    Public,
    Top,
}

impl Intake {
    const ALL: [Self; 3] = [Self::MevShare, Self::Public, Self::Top];

    fn trigger(self) -> TriggerKind {
        match self {
            Self::MevShare => TriggerKind::ArbMevShare,
            Self::Public => TriggerKind::ArbPublic,
            Self::Top => TriggerKind::ArbTop,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::MevShare => 0,
            Self::Public => 1,
            Self::Top => 2,
        }
    }
}

/// Latency buckets: quarter powers of two of a microsecond, to 2^32 µs.
const LAT_BUCKETS: usize = 128;

/// Trigger-in to job-out on the engine thread, per intake, in fixed
/// log-spaced buckets (a quarter power of two wide), so recording allocates
/// nothing. Reported every [`LAT_REPORT_BLOCKS`] blocks; each job's exact
/// figure is also logged (`liq::arb_latency`), and the submit path logs its
/// venue ack against the same trigger-in.
#[derive(Clone, Debug)]
pub struct Latency {
    buckets: [[u32; LAT_BUCKETS]; 3],
    n: [u64; 3],
    max_us: [u64; 3],
}

impl Default for Latency {
    fn default() -> Self {
        Self {
            buckets: [[0; LAT_BUCKETS]; 3],
            n: [0; 3],
            max_us: [0; 3],
        }
    }
}

/// The bucket of `us`: `4·⌊log2 us⌋` plus the next two bits.
fn lat_bucket(us: u64) -> usize {
    if us < 4 {
        return usize::try_from(us).unwrap_or(0);
    }
    let lg = 63u32.saturating_sub(us.leading_zeros());
    let frac = (us >> lg.saturating_sub(2)) & 3;
    let b = u64::from(lg)
        .saturating_sub(1)
        .saturating_mul(4)
        .saturating_add(frac);
    usize::try_from(b)
        .unwrap_or(LAT_BUCKETS)
        .min(LAT_BUCKETS - 1)
}

/// The smallest value in bucket `b` (the inverse of [`lat_bucket`]).
fn lat_floor(b: usize) -> u64 {
    let b = u64::try_from(b).unwrap_or(u64::MAX);
    if b < 4 {
        return b;
    }
    let lg = (b / 4).saturating_add(1);
    let frac = b % 4;
    frac.saturating_add(4) << lg.saturating_sub(2)
}

impl Latency {
    pub fn record(&mut self, intake: Intake, d: Duration) {
        let k = intake.index();
        let us = u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
        if let (Some(row), Some(n), Some(m)) = (
            self.buckets.get_mut(k),
            self.n.get_mut(k),
            self.max_us.get_mut(k),
        ) {
            if let Some(c) = row.get_mut(lat_bucket(us)) {
                *c = c.saturating_add(1);
            }
            *n = n.saturating_add(1);
            *m = (*m).max(us);
        }
    }

    /// Jobs recorded for `intake`.
    #[must_use]
    pub fn count(&self, intake: Intake) -> u64 {
        self.n.get(intake.index()).copied().unwrap_or(0)
    }

    /// The quantile at `permille`/1000's bucket floor for `intake`, µs
    /// (within a quarter power of two below the true value); `None` before
    /// any job.
    #[must_use]
    pub fn quantile_us(&self, intake: Intake, permille: u64) -> Option<u64> {
        let k = intake.index();
        let n = *self.n.get(k)?;
        if n == 0 {
            return None;
        }
        let rank = n.saturating_mul(permille).div_ceil(1000).clamp(1, n);
        let mut seen = 0u64;
        for (b, &c) in self.buckets.get(k)?.iter().enumerate() {
            seen = seen.saturating_add(u64::from(c));
            if seen >= rank {
                return Some(lat_floor(b));
            }
        }
        self.max_us.get(k).copied()
    }

    fn report(&self) {
        for i in Intake::ALL {
            if self.count(i) == 0 {
                continue;
            }
            tracing::info!(
                intake = ?i,
                jobs = self.count(i),
                p50_us = self.quantile_us(i, 500),
                p99_us = self.quantile_us(i, 990),
                max_us = self.max_us.get(i.index()).copied(),
                "arb: trigger-in to job-out"
            );
        }
    }
}

/// Blocks between latency reports (about an hour).
pub const LAT_REPORT_BLOCKS: u64 = 300;

/// The arb thread's in-process simulator: the node's state factory, the
/// `ArbExecutor` the backrun is sent to (deployed, or the compiled code
/// placed beside its modules when it is not). `None` when this process has
/// no node state (the standalone binary): public-mempool and top-of-block
/// backruns are not simulated, so none are sent — a bundle that was never
/// simulated is not one to risk the block's nonce on.
pub struct ArbSim {
    factory: Arc<dyn StateProviderFactory>,
    executor: Address,
    /// `None`: the deployed `ArbExecutor` (the chain's code). `Some`: the
    /// compiled system placed at [`Self::executor`] for simulation.
    code: Option<liq_sim::ArbExecutorCode>,
}

impl ArbSim {
    /// A simulator over `factory`'s states for `executor`; `code` places
    /// the compiled `ArbExecutor` system there when it is not deployed.
    #[must_use]
    pub fn new(
        factory: Arc<dyn StateProviderFactory>,
        executor: Address,
        code: Option<liq_sim::ArbExecutorCode>,
    ) -> Self {
        Self {
            factory,
            executor,
            code,
        }
    }

    /// A fresh simulator on `parent`'s state with the `ArbExecutor` placed,
    /// its `WETH`/`PROFIT_SINK` read back from the placed code (the
    /// authority — a deployed contract that does not answer as an
    /// `ArbExecutor` is refused, not guessed at). The state a backrun for
    /// the next block runs on: the committed block's.
    fn build(
        &self,
        parent: BlockRef,
        env: &SimEnv,
    ) -> Result<Simulator<liq_sim::BlockState>, SimError> {
        let state = Arc::new(self.factory.state_at(parent)?);
        let placed = self
            .code
            .as_ref()
            .map(|c| c.placements(self.executor).to_vec())
            .unwrap_or_default();
        let mut sim =
            Simulator::with_executor(state, self.executor, &placed, Address::ZERO, Address::ZERO)?;
        let (weth, sink) = executor_anchors(&mut sim.db, env, self.executor)?;
        sim.weth = weth;
        sim.profit_account = sink;
        Ok(sim)
    }
}

/// A pending public-mempool transaction as the simulator runs it: the
/// sender, callee, value, calldata, gas and access list it was validated
/// with. Creations never reach here (the pool listener drops them).
fn pending_to_sim_tx(p: &PendingPublic) -> SimTx {
    SimTx {
        caller: p.sender,
        to: p.to.unwrap_or(Address::ZERO),
        value: p.value,
        data: p.input.clone(),
        gas_limit: p.gas_limit,
        access_list: p.access_list.iter().map(|(a, k)| (*a, k.clone())).collect(),
    }
}

/// Holds simulations of our plans to their claims, and quarantines a pool
/// on a confirmed model error ([`crate::arb_check`]).
pub struct Checker {
    /// Our call's logs from the last simulation (reused).
    logs: Vec<alloy_primitives::Log>,
    /// Where quarantines are appended as evidence (`shadow/arb/quarantine.jsonl`);
    /// read back at startup so a quarantine outlives a restart until the
    /// line is removed.
    file: Option<PathBuf>,
    /// The committed block and the target's timestamp (for decoding).
    block: u64,
    timestamp: u64,
    /// Plans a simulation refused.
    pub refused: u64,
    /// Pools quarantined.
    pub quarantined: u64,
}

impl Checker {
    #[must_use]
    pub fn new(file: Option<PathBuf>) -> Self {
        Self {
            logs: Vec::with_capacity(64),
            file,
            block: 0,
            timestamp: 0,
            refused: 0,
            quarantined: 0,
        }
    }

    /// Simulate `victims` then our call for `o` on `sim`, and hold the
    /// result to the claim. `Some(outcome)` when it holds (send); `None`
    /// when a victim reverts, our call reverts, or the gain differs — the
    /// last audited hop by hop, quarantining a pool only on a confirmed
    /// model error.
    #[allow(clippy::too_many_arguments)]
    fn run<P: liq_sim::DatabaseRef<Error = SimError>>(
        &mut self,
        engine: &mut Engine,
        sim: &mut Simulator<P>,
        env: &SimEnv,
        victims: &[SimTx],
        o: &Opportunity,
        ours: (Address, Address, u64),
    ) -> Result<Option<liq_sim::SimOutcome>, SimError> {
        let call = our_call(o, ours);
        self.logs.clear();
        let s = match verify_ordered_logs(sim, victims, &[call], env, &mut self.logs) {
            Ok(Ordered::Ran(s)) => s,
            Ok(Ordered::VictimReverted(_)) => return Ok(None),
            Err(SimError::Revert { reason }) => {
                self.refused = self.refused.saturating_add(1);
                tracing::warn!(
                    anchor = o.backrun.anchor.pool.0,
                    kept_wei = %o.kept_wei,
                    reason = %reason,
                    "arb: plan reverted in simulation — model and chain disagree"
                );
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        if claim_holds(o, s.net_profit_wei) {
            return Ok(Some(s));
        }
        self.refused = self.refused.saturating_add(1);
        self.audit(engine, sim, env, victims, o, s.net_profit_wei)?;
        Ok(None)
    }

    fn audit<P: liq_sim::DatabaseRef<Error = SimError>>(
        &mut self,
        engine: &mut Engine,
        sim: &mut Simulator<P>,
        env: &SimEnv,
        victims: &[SimTx],
        o: &Opportunity,
        gained: I256,
    ) -> Result<(), SimError> {
        let d = first_disagreement(engine.book(), o, &self.logs, self.block, self.timestamp);
        let Disagreement::Pool { hop, seen } = d else {
            tracing::warn!(
                block = self.block,
                claimed = %o.kept_wei,
                simulated = %gained,
                where_ = ?d,
                "arb: simulation refused the plan (no pool to pin it on)"
            );
            return Ok(());
        };
        let Some((solved, reads)) = view_reads(engine.book(), o, hop.pool) else {
            tracing::warn!(
                block = self.block,
                ?hop,
                ?seen,
                "arb: simulation refused the plan — hop differs on a pool with no view (unconfirmed)"
            );
            return Ok(());
        };
        let chain =
            views_after(sim, victims, env, &reads)?.and_then(|outs| chain_view(&solved, &outs));
        match chain {
            Some(v) if v == solved => self.quarantine(engine, hop, seen, solved),
            Some(v) => tracing::warn!(
                block = self.block,
                ?hop,
                ?seen,
                model = ?solved,
                chain = ?v,
                "arb: simulation refused the plan — the pool held another state (book behind the chain)"
            ),
            None => tracing::warn!(
                block = self.block,
                ?hop,
                ?seen,
                "arb: simulation refused the plan — pool views unreadable (unconfirmed)"
            ),
        }
        Ok(())
    }

    fn quarantine(
        &mut self,
        engine: &mut Engine,
        hop: liq_arb::Hop,
        seen: liq_arb::SwapSeen,
        solved: liq_arb::PoolView,
    ) {
        if !engine.quarantine(hop.pool) {
            return;
        }
        self.quarantined = self.quarantined.saturating_add(1);
        let address = engine
            .book()
            .get(hop.pool)
            .map_or(Address::ZERO, |p| p.address);
        tracing::error!(
            block = self.block,
            pool = %address,
            ?hop,
            ?seen,
            state = ?solved,
            "arb: pool QUARANTINED — its model disagrees with the chain on the exact state"
        );
        let Some(path) = &self.file else {
            return;
        };
        let line = serde_json::json!({
            "pool": address,
            "block": self.block,
            "coin_in": hop.i,
            "coin_out": hop.j,
            "amount_in": hop.amount_in.to_string(),
            "model_out": hop.amount_out.to_string(),
            "chain_in": seen.amount_in.to_string(),
            "chain_out": seen.amount_out.to_string(),
            "state": format!("{solved:?}"),
        });
        let wrote = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| {
                use std::io::Write;
                writeln!(f, "{line}")
            });
        if let Err(e) = wrote {
            tracing::error!(path = %path.display(), error = %e, "arb: quarantine evidence not written");
        }
    }
}

/// The pool addresses `path` (the quarantine evidence) names; empty when
/// there is none.
#[must_use]
pub fn quarantined_pools(path: &Path) -> Vec<Address> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v.get("pool")?.as_str()?.parse::<Address>().ok())
        .collect()
}

/// Our signed call for `o`: `ArbExecutor.execute(plan)` from the operator.
fn our_call(o: &Opportunity, (operator, executor, gas_limit): (Address, Address, u64)) -> SimTx {
    SimTx {
        caller: operator,
        to: executor,
        value: U256::ZERO,
        data: liq_sim::execute_calldata(o.plan.as_bytes()),
        gas_limit,
        access_list: Vec::new(),
    }
}

/// What the engine thread holds across steps.
pub struct ArbIo {
    pub book: Arc<RwLock<PoolBook>>,
    pub flash: Arc<ArcSwap<FlashIndex>>,
    pub ticks: rtrb::Consumer<ArbTick>,
    pub hints: rtrb::Consumer<ArbHint>,
    /// Pending public-mempool transactions from Reth's pool (the ExEx's
    /// listener task).
    pub pending: rtrb::Consumer<PendingPublic>,
    pub out: crossbeam_channel::Sender<liq_exec::path::ExecJob>,
}

/// What a public backrun group was last sent at this block: its pool and
/// the simulated gain. A later search sends again only on more.
type SentGroup = (PoolId, I256);

/// The engine and the buffers its steps reuse.
pub struct ArbThread {
    engine: Engine,
    flashable: Vec<AssetId>,
    per_eth: Vec<U256>,
    sources: Vec<FlashSource>,
    target: u64,
    /// The target block's timestamp (`SLOT_SECONDS` after the committed).
    target_ts: u64,
    /// The committed block's hash: names the state a backrun sims on.
    parent_hash: B256,
    fee: Option<liq_exec::fee::FeeQuote>,
    signer: ArbSigner,
    traces: u64,
    /// The bid table in force (published by the reloader), the one this
    /// thread last took, and its uncontested pools resolved on the book.
    bids: Arc<ArcSwap<BidToml>>,
    bid_now: Arc<BidToml>,
    uncontested: Vec<PoolId>,
    /// The in-process simulator (the node's state + the `ArbExecutor`).
    /// `None`: no node state in this process; public and top-of-block
    /// backruns are not sent.
    sim: Option<ArbSim>,
    /// The built simulator, cached for the block it was built on (each run
    /// resets the overlay, so nothing of an earlier one survives).
    sim_cache: Option<Simulator<liq_sim::BlockState>>,
    /// The committed block the cached simulator is for; `0` = none yet.
    sim_block: u64,
    check: Checker,
    /// Pending public transactions not yet expired, with the block each
    /// arrived in.
    pending: std::collections::VecDeque<(PendingPublic, u64)>,
    /// Each pending transaction's solo run this block: the first book pool
    /// it moves, `None` when it reverts alone or moves none.
    solo: std::collections::HashMap<B256, Option<PoolId>>,
    /// Public groups sent this block.
    sent: Vec<SentGroup>,
    /// Pools a newly run public transaction joined, waiting for their
    /// group's search (in the order they were found).
    dirty: Vec<PoolId>,
    /// The public group search in progress, resumed a unit per step.
    search: Option<GroupSearch>,
    pub lat: Latency,
    /// Blocks seen (latency reports).
    blocks: u64,
    /// Jobs the submit worker's inbox refused (full).
    pub dropped: u64,
}

/// Pending public transactions kept at once; past it the oldest go.
const PENDING_KEEP: usize = 2048;
/// Blocks a pending transaction is kept for: one that has not landed by
/// then is not likely to be the next block's.
const PENDING_BLOCKS: u64 = 2;
/// Most pending transactions searched for a backrun after one arrival,
/// newest first. Past this the oldest wait.
const PENDING_SCAN: usize = 64;

impl ArbThread {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        book: &PoolBook,
        cfg: EngineCfg,
        flashable: Vec<AssetId>,
        n_assets: usize,
        signer: ArbSigner,
        sim: Option<ArbSim>,
        bids: Arc<ArcSwap<BidToml>>,
        check: Checker,
    ) -> Self {
        let bid_now = bids.load_full();
        let mut t = Self {
            engine: Engine::new(book, cfg),
            flashable,
            per_eth: vec![U256::ZERO; n_assets],
            sources: Vec::with_capacity(2 * MAX_FLASHABLE),
            target: 0,
            target_ts: 0,
            parent_hash: B256::ZERO,
            fee: None,
            signer,
            traces: 0,
            bids,
            bid_now,
            uncontested: Vec::new(),
            sim,
            sim_cache: None,
            sim_block: 0,
            check,
            pending: std::collections::VecDeque::with_capacity(PENDING_KEEP),
            solo: std::collections::HashMap::with_capacity(PENDING_KEEP),
            sent: Vec::with_capacity(PENDING_SCAN),
            dirty: Vec::with_capacity(PENDING_SCAN),
            search: None,
            lat: Latency::default(),
            blocks: 0,
            dropped: 0,
        };
        t.resolve_uncontested();
        t
    }

    /// Quarantine every pool at one of `addrs` (evidence read back at
    /// startup). Returns how many.
    pub fn quarantine_addresses(&mut self, addrs: &[Address]) -> usize {
        let ids: Vec<PoolId> = self
            .engine
            .book()
            .pools()
            .iter()
            .enumerate()
            .filter(|(_, p)| addrs.contains(&p.address))
            .filter_map(|(k, _)| u32::try_from(k).ok().map(PoolId))
            .collect();
        ids.into_iter()
            .filter(|&id| self.engine.quarantine(id))
            .count()
    }

    /// The uncontested list of the table in force, as book pools.
    fn resolve_uncontested(&mut self) {
        let list = &self.bid_now.uncontested;
        self.uncontested.clear();
        for (k, p) in self.engine.book().pools().iter().enumerate() {
            if list.contains(&p.address) {
                if let Ok(id) = u32::try_from(k) {
                    self.uncontested.push(PoolId(id));
                }
            }
        }
    }

    /// Take the reloader's table if it changed since the last block.
    fn refresh_bids(&mut self) {
        let cur = self.bids.load_full();
        if !Arc::ptr_eq(&cur, &self.bid_now) {
            self.bid_now = cur;
            self.resolve_uncontested();
            tracing::info!(
                uncontested = self.uncontested.len(),
                "arb: new bid table in force"
            );
        }
    }

    /// One pass: the newest tick (older ones are superseded), then every
    /// queued MEV-Share hint, then the pending ring drained and at most one
    /// unit of public-mempool work (`on_pending`). Hints always go
    /// first: public work is cut into units (one transaction's solo run, or
    /// one subset of a group in every order) and the next step serves any
    /// hint that arrived meanwhile before the next unit. Returns how much
    /// it handled; zero only when there is nothing left to do.
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
        while let Ok((born, h)) = io.hints.pop() {
            handled = handled.saturating_add(1);
            self.on_hint(&h, born, io);
        }
        let mut arrived = 0usize;
        while let Ok(p) = io.pending.pop() {
            arrived = arrived.saturating_add(1);
            if self.pending.len() == PENDING_KEEP {
                self.pending.pop_front();
            }
            self.pending.push_back((p, self.target));
        }
        handled = handled.saturating_add(arrived);
        if self.on_pending(io) {
            handled = handled.saturating_add(1);
        }
        handled
    }

    /// Pending public transactions waiting, oldest first.
    pub fn pending(&self) -> impl Iterator<Item = &(PendingPublic, u64)> {
        self.pending.iter()
    }

    fn on_tick(&mut self, t: &ArbTick, io: &mut ArbIo) {
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
            timestamp: t.timestamp.saturating_add(liq_sim::SLOT_SECONDS),
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
        self.target_ts = target.timestamp;
        self.parent_hash = t.parent_hash;
        self.fee = Some(t.fee);
        self.check.block = t.block;
        self.check.timestamp = target.timestamp;
        let keep_from = target.number.saturating_sub(PENDING_BLOCKS);
        self.pending.retain(|(_, seen)| *seen >= keep_from);
        self.solo.clear();
        self.sent.clear();
        // A new block: what was found for the last one is stale.
        self.dirty.clear();
        self.search = None;
        self.refresh_bids();
        tracing::debug!(
            block = t.block,
            copied,
            sources = self.sources.len(),
            moved = self.engine.top_anchors().count(),
            "arb: block"
        );
        self.on_top(io, t.born);
        self.blocks = self.blocks.saturating_add(1);
        if self.blocks.is_multiple_of(LAT_REPORT_BLOCKS) {
            self.lat.report();
            tracing::info!(
                refused = self.check.refused,
                quarantined = self.check.quarantined,
                dropped = self.dropped,
                "arb: simulation checks"
            );
        }
    }

    fn on_hint(&mut self, h: &MevShareHint, born: Instant, io: &ArbIo) {
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
        let row = self.bid_now.of(Intake::MevShare);
        let unc = &self.uncontested;
        let bid = move |p: PoolId| {
            if unc.contains(&p) {
                row.uncontested_bps
            } else {
                row.contested_bps
            }
        };
        match self.engine.on_victim(&refs, bid) {
            Ok(Ok(o)) => {
                let Some(fee) = self.fee else {
                    return;
                };
                let job = make_job(
                    &mut self.traces,
                    &o,
                    JobTerms {
                        fee,
                        signer: self.signer,
                        target: self.target,
                        intake: Intake::MevShare,
                        gas_limit: o.backrun.gas.saturating_mul(GAS_LIMIT_PCT) / 100,
                        born,
                    },
                    Some(h.hash),
                    None,
                    Vec::new(),
                );
                tracing::info!(
                    trace = job.trace.raw(),
                    hint = %h.hash,
                    net_wei = %o.backrun.net_wei,
                    claimed_wei = %o.kept_wei,
                    gross_wei = %o.gross_wei,
                    bid_bps = o.bid_bps,
                    evals = o.backrun.evals,
                    "arb: backrun"
                );
                self.send(io, job, Intake::MevShare, born);
            }
            Ok(Err(why)) => tracing::trace!(hint = %h.hash, ?why, "arb: no backrun"),
            Err(e) => tracing::warn!(hint = %h.hash, error = %e, "arb: solve refused"),
        }
    }

    /// Hand `job` to the submit worker and record its latency.
    fn send(&mut self, io: &ArbIo, job: liq_exec::path::ExecJob, intake: Intake, born: Instant) {
        let took = born.elapsed();
        self.lat.record(intake, took);
        tracing::info!(
            target: "liq::arb_latency",
            trace = job.trace.raw(),
            intake = ?intake,
            engine_us = u64::try_from(took.as_micros()).unwrap_or(u64::MAX),
            "job out"
        );
        if io.out.try_send(job).is_err() {
            self.dropped = self.dropped.saturating_add(1);
            tracing::warn!(
                dropped = self.dropped,
                ?intake,
                "arb: submit inbox full — job dropped"
            );
        }
    }

    /// The target block's simulation terms: the committed block's state,
    /// the target's fork rules, priced at its base fee.
    fn sim_env(&self) -> Option<(BlockRef, SimEnv)> {
        if self.target == 0 {
            return None;
        }
        let fee = self.fee?;
        let parent = BlockRef {
            number: self.target.saturating_sub(1),
            hash: self.parent_hash,
        };
        let spec = mainnet_spec(self.target_ts)?;
        let mut block_env = block_env_at(self.target, self.target_ts);
        let base_fee = u64::try_from(fee.next_base_fee).unwrap_or(u64::MAX);
        block_env.basefee = base_fee;
        Some((
            parent,
            SimEnv::new(spec, block_env).with_gas_price(base_fee, 0),
        ))
    }

    /// The simulator for `parent`, built once per block. `false` when the
    /// process has none or the node cannot serve the state.
    fn ensure_sim(&mut self, parent: BlockRef, env: &SimEnv) -> bool {
        let Some(cfg) = self.sim.as_ref() else {
            return false;
        };
        if self.sim_block == parent.number && self.sim_cache.is_some() {
            return true;
        }
        self.sim_block = parent.number;
        match cfg.build(parent, env) {
            Ok(s) => {
                self.sim_cache = Some(s);
                true
            }
            Err(e) => {
                self.sim_cache = None;
                tracing::warn!(block = parent.number, error = %e, "arb: sim state unavailable — no backrun");
                false
            }
        }
    }

    /// What the block just committed left open, with no victim: solved on
    /// its largest moves, simulated on its state, and sent alone for the
    /// top of the target block (`ArbTop`).
    fn on_top(&mut self, io: &ArbIo, born: Instant) {
        if self.sim.is_none() || self.engine.top_anchors().next().is_none() {
            return;
        }
        let row = self.bid_now.of(Intake::Top);
        let unc = &self.uncontested;
        let bid = move |p: PoolId| {
            if unc.contains(&p) {
                row.uncontested_bps
            } else {
                row.contested_bps
            }
        };
        let o = match self.engine.top_of_block(bid) {
            Ok(Ok(o)) => o,
            Ok(Err(why)) => {
                tracing::trace!(?why, "arb: nothing open at the top");
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "arb: top-of-block solve refused");
                return;
            }
        };
        let (Some((parent, env)), Some(fee)) = (self.sim_env(), self.fee) else {
            return;
        };
        if !self.ensure_sim(parent, &env) {
            return;
        }
        let (Some(sim), Some(cfg)) = (self.sim_cache.as_mut(), self.sim.as_ref()) else {
            return;
        };
        let ours = (self.signer.operator, cfg.executor, liq_sim::MAX_TX_GAS);
        let s = match self.check.run(&mut self.engine, sim, &env, &[], &o, ours) {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(error = %e, "arb: top-of-block simulation refused");
                return;
            }
        };
        let job = make_job(
            &mut self.traces,
            &o,
            JobTerms {
                fee,
                signer: self.signer,
                target: self.target,
                intake: Intake::Top,
                gas_limit: sim_gas_limit(&o, &s),
                born,
            },
            None,
            None,
            Vec::new(),
        );
        tracing::info!(
            trace = job.trace.raw(),
            target = self.target,
            claimed_wei = %o.kept_wei,
                    gross_wei = %o.gross_wei,
            kept_wei = %s.net_profit_wei,
            bid_bps = o.bid_bps,
            gas = s.gas_used,
            anchor = o.backrun.anchor.pool.0,
            "arb: top-of-block backrun"
        );
        self.send(io, job, Intake::Top, born);
    }

    /// One unit of public-mempool work, `false` when there is none. In
    /// order of preference: the group search in progress takes one more
    /// unit ([`GroupSearch::unit`]); else the newest includable transaction
    /// not yet run alone this block is run, and the first book pool it
    /// moves is marked for a search; else the next marked pool's group (the
    /// waiting transactions that move it first) starts its search. A
    /// finished search sends the best subset and order as an `ArbPublic`
    /// job (the victims in that order, then ours) — again later in the
    /// block only when a search beats what that pool's bundle already
    /// gains. Transactions that cannot pay the target block's base fee,
    /// revert alone, or touch no pool in the book are not bundled. Nothing
    /// is sent without an in-process simulator.
    fn on_pending(&mut self, io: &ArbIo) -> bool {
        if self.sim.is_none() {
            return false;
        }
        let (Some((parent, env)), Some(fee)) = (self.sim_env(), self.fee) else {
            return false;
        };
        let unrun = self.next_unrun(fee.next_base_fee);
        let work = self.search.is_some() || !self.dirty.is_empty() || unrun.is_some();
        if !work {
            return false;
        }
        if !self.ensure_sim(parent, &env) {
            self.search = None;
            self.dirty.clear();
            return false;
        }
        let (Some(sim), Some(cfg)) = (self.sim_cache.as_mut(), self.sim.as_ref()) else {
            return false;
        };
        let ours = (self.signer.operator, cfg.executor, liq_sim::MAX_TX_GAS);
        // 1. The search in progress: one more unit.
        if let Some(search) = self.search.as_mut() {
            let row = self.bid_now.of(Intake::Public);
            let unc = &self.uncontested;
            let bid = |p: PoolId| {
                if unc.contains(&p) {
                    row.uncontested_bps
                } else {
                    row.contested_bps
                }
            };
            match search.unit(&mut self.engine, sim, &env, ours, &bid, &mut self.check) {
                Ok(false) => {}
                Ok(true) => {
                    if let Some(done) = self.search.take() {
                        self.finish(io, done, fee);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "arb: public combo sim refused");
                    self.search = None;
                }
            }
            return true;
        }
        // 2. The newest includable transaction not yet run alone.
        if let Some(k) = unrun {
            let Some((p, _)) = self.pending.get(k) else {
                return false;
            };
            let tx = pending_to_sim_tx(p);
            let mut touched: smallvec::SmallVec<[PoolId; 4]> = smallvec::SmallVec::new();
            let pool = match victim_logs(sim, std::slice::from_ref(&tx), &env) {
                Ok(VictimRun::Logs(per_victim)) => {
                    let mut first = None;
                    for l in per_victim.iter().flatten() {
                        let d = DecodedLog {
                            address: l.address,
                            topics: l.data.topics(),
                            data: &l.data.data,
                            block: parent.number,
                            timestamp: self.target_ts,
                        };
                        touched.clear();
                        self.engine.book().pools_for_log(&d, &mut touched);
                        if let Some(&pool) = touched.first() {
                            first = Some(pool);
                            break;
                        }
                    }
                    first
                }
                Ok(VictimRun::Reverted(_)) => None,
                Err(e) => {
                    tracing::trace!(hash = %p.hash, error = %e, "arb: public solo-sim refused");
                    None
                }
            };
            self.solo.insert(p.hash, pool);
            if let Some(pool) = pool {
                if !self.dirty.contains(&pool) {
                    self.dirty.push(pool);
                }
            }
            return true;
        }
        // 3. The next marked pool's group starts its search.
        if self.dirty.is_empty() {
            return false;
        }
        let pool = self.dirty.remove(0);
        let mut hashes: Vec<B256> = Vec::with_capacity(PENDING_SCAN);
        let mut members: Vec<&PendingPublic> = Vec::new();
        for (p, _) in self.pending.iter().rev() {
            if p.max_fee_per_gas < fee.next_base_fee || hashes.contains(&p.hash) {
                continue;
            }
            hashes.push(p.hash);
            if self.solo.get(&p.hash) == Some(&Some(pool)) {
                members.push(p);
            }
            if hashes.len() == PENDING_SCAN {
                break;
            }
        }
        if !members.is_empty() {
            self.search = Some(GroupSearch::new(pool, &members));
        }
        true
    }

    /// The deque index of the newest includable transaction (within the
    /// [`PENDING_SCAN`] newest) not yet run alone this block.
    fn next_unrun(&self, base_fee: u128) -> Option<usize> {
        let mut seen: smallvec::SmallVec<[B256; PENDING_SCAN]> = smallvec::SmallVec::new();
        for (k, (p, _)) in self.pending.iter().enumerate().rev() {
            if p.max_fee_per_gas < base_fee || seen.contains(&p.hash) {
                continue;
            }
            if !self.solo.contains_key(&p.hash) {
                return Some(k);
            }
            seen.push(p.hash);
            if seen.len() == PENDING_SCAN {
                return None;
            }
        }
        None
    }

    /// A finished group search: send its best subset and order, unless this
    /// block's bundle for the pool already gains as much.
    fn finish(&mut self, io: &ArbIo, done: GroupSearch, fee: liq_exec::fee::FeeQuote) {
        let Some(best) = done.best else {
            return;
        };
        let pool = done.pool;
        if self
            .sent
            .iter()
            .any(|(p, g)| *p == pool && *g >= best.sim.net_profit_wei)
        {
            return;
        }
        // The victims in the chosen order: first is `backrun_tx`, the rest
        // are `parents` (the bundle shape `ArbPublic` expects).
        let chosen: Vec<&(Bytes, Instant)> =
            best.order.iter().filter_map(|&i| done.raw.get(i)).collect();
        let Some(first) = chosen.first().map(|(raw, _)| raw.clone()) else {
            return;
        };
        let parents: Vec<Bytes> = chosen.iter().skip(1).map(|(raw, _)| raw.clone()).collect();
        // Trigger-in: the newest of the chosen victims (the bundle could be
        // built only once it arrived).
        let born = chosen
            .iter()
            .map(|(_, seen)| *seen)
            .max()
            .unwrap_or_else(Instant::now);
        let job = make_job(
            &mut self.traces,
            &best.opportunity,
            JobTerms {
                fee,
                signer: self.signer,
                target: self.target,
                intake: Intake::Public,
                gas_limit: sim_gas_limit(&best.opportunity, &best.sim),
                born,
            },
            None,
            Some(first),
            parents,
        );
        tracing::info!(
            trace = job.trace.raw(),
            target = self.target,
            claimed_wei = %best.opportunity.kept_wei,
            gross_wei = %best.opportunity.gross_wei,
            kept_wei = %best.sim.net_profit_wei,
            bid_bps = best.opportunity.bid_bps,
            gas = best.sim.gas_used,
            victims = best.order.len(),
            "arb: public backrun"
        );
        match self.sent.iter_mut().find(|(p, _)| *p == pool) {
            Some(s) => s.1 = best.sim.net_profit_wei,
            None => self.sent.push((pool, best.sim.net_profit_wei)),
        }
        self.send(io, job, Intake::Public, born);
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

/// A simulated backrun's gas limit: what the simulation needed available
/// plus [`SIM_GAS_PCT`], and never under the planned-gas limit.
fn sim_gas_limit(o: &Opportunity, s: &liq_sim::SimOutcome) -> u64 {
    let planned = o.backrun.gas.saturating_mul(GAS_LIMIT_PCT) / 100;
    let simulated = s.gas_spent.saturating_mul(SIM_GAS_PCT) / 100;
    planned.max(simulated).min(liq_sim::MAX_TX_GAS)
}

/// What a job needs besides the plan and the transactions it follows.
#[derive(Clone, Copy, Debug)]
struct JobTerms {
    fee: liq_exec::fee::FeeQuote,
    signer: ArbSigner,
    target: u64,
    intake: Intake,
    gas_limit: u64,
    born: Instant,
}

/// The submit job for `o`: `ArbExecutor.execute(plan)` from the arb key,
/// for the target block (and [`EXTRA_BLOCKS`] after, unless it is a
/// top-of-block backrun, which is for that block's top only), scoped for
/// the risk gate as the arb protocol on the anchor pool's market, bidding
/// what the plan carries. `hint` is the MEV-Share hint it follows;
/// `backrun_tx` then `parents`, the public victims in order.
fn make_job(
    traces: &mut u64,
    o: &Opportunity,
    t: JobTerms,
    hint: Option<B256>,
    backrun_tx: Option<Bytes>,
    parents: Vec<Bytes>,
) -> liq_exec::path::ExecJob {
    use liq_types::{MarketId, PositionKey, TraceId, ARB_PROTOCOL};
    *traces = traces.wrapping_add(1);
    let market = MarketId(o.backrun.anchor.pool.0);
    let plan = Bytes::from(o.plan.as_bytes().to_vec());
    let max_block = if t.intake == Intake::Top {
        t.target
    } else {
        t.target.saturating_add(EXTRA_BLOCKS)
    };
    liq_exec::path::ExecJob {
        trace: TraceId::from_raw(ARB_TRACE | *traces),
        calldata: liq_sim::execute_calldata(&plan),
        plan,
        trigger: t.intake.trigger(),
        protocol: ARB_PROTOCOL,
        market,
        collateral: o.backrun.start,
        debt: o.other,
        flash: o.flash,
        operator_key: t.signer.operator,
        position: PositionKey {
            protocol: ARB_PROTOCOL,
            market,
            user: Address::ZERO,
        },
        hint_hash: hint,
        backrun_tx,
        parents,
        target_block: t.target,
        max_block,
        fee: t.fee,
        auction_bps: o.bid_bps,
        gas_limit: t.gas_limit,
        chain_id: 1,
        slot: t.signer.slot,
        rpc_verify: false,
        born: Some(t.born),
    }
}

/// What [`start`] hands the rest of startup.
pub struct ArbStarted {
    /// For the drain (`DrainJoin::with_arb`).
    pub feed: ArbFeed,
    /// For the exec worker (`spawn_exec_worker`, its `arb` input).
    pub exec: crate::exec_worker::ArbExec,
    /// For the MEV-Share reader (`spawn_hint_reader`, its `arb` input).
    pub hints: rtrb::Producer<ArbHint>,
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
/// (`ArbExecutor`, the arb key with one nonce per target block, the shared
/// builders and identity, its own shadow log), starts the bid reloader, and
/// spawns `liq-arb-engine` on its `cores.toml` CPU — run unpinned only when
/// `allow_unpinned`.
#[allow(clippy::too_many_arguments)]
pub fn start(
    config_dir: &Path,
    cores_path: &Path,
    allow_unpinned: bool,
    intern: &liq_config::Intern,
    weth: Address,
    arb_executor: Option<Address>,
    shadow: &Path,
    shared: &crate::shared::Shared,
    book: Arc<RwLock<PoolBook>>,
    node_state: Option<Arc<dyn liq_sim::StateProviderFactory>>,
    profit_sink: Option<Address>,
) -> Option<ArbStarted> {
    let toml_path = config_dir.join("arb.toml");
    let cfg = ArbToml::load(&toml_path)?;
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
    let arb_shadow = shadow.join("arb");
    let path = crate::exec_bind::bind_from_config(
        &config_dir.join("builders.toml"),
        &arb_shadow,
        shared,
        Some(secrets),
    )?
    .with_executor(executor)
    .with_nonce_mode(liq_exec::nonce::NonceMode::Exclusive);
    let operator = path.signers.first()?.address();
    // The in-process simulator for public-mempool and top-of-block
    // backruns: the node's state, the ArbExecutor the backrun is sent to,
    // and (only when it is not yet deployed at the placeholder) the
    // compiled code placed beside its modules. Without a node state or
    // PROFIT_SINK they are not simulated and so not sent.
    let arb_sim = match (node_state, profit_sink) {
        (Some(factory), Some(sink)) => {
            let spec = liq_sim::ExecutorSpec::mainnet(operator, operator, sink);
            let code = if executor == liq_sim::PLANNED_ARB_EXECUTOR {
                match liq_sim::arb_executor_stack(&spec) {
                    Ok(c) => Some(c),
                    Err(e) => {
                        tracing::error!(error = %e, "ArbExecutor artifact unreadable (run forge build) — simulated backruns off");
                        return None;
                    }
                }
            } else {
                None
            };
            Some(ArbSim::new(factory, executor, code))
        }
        _ => {
            tracing::warn!("no node state or PROFIT_SINK — public-mempool and top-of-block backruns not simulated or sent");
            None
        }
    };
    let (engine_cfg, flashable) = cfg.engine_cfg(intern, weth)?;
    let n_assets = intern.asset_id_capacity();
    let (tick_tx, tick_rx) = rtrb::RingBuffer::<ArbTick>::new(TICK_RING);
    let (hint_tx, hint_rx) = rtrb::RingBuffer::<ArbHint>::new(HINT_RING);
    let (pending_tx, pending_rx) = rtrb::RingBuffer::<PendingPublic>::new(liq_types::RING_CAP);
    let (job_tx, job_rx) = crossbeam_channel::bounded(JOB_INBOX);
    let core = crate::threads::CoreMap::load(cores_path)
        .and_then(|m| m.slot(ARB_THREAD).map(|s| s.core))
        .ok();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bids = Arc::new(ArcSwap::from_pointee(cfg.bids.clone()));
    if let Err(e) = spawn_bid_reload(toml_path, Arc::clone(&bids), Arc::clone(&stop)) {
        tracing::error!(error = %e, "arb bid reloader not started — bids fixed until restart");
    }
    let quarantine_file = arb_shadow.join("quarantine.jsonl");
    let held = quarantined_pools(&quarantine_file);
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
            let mut engine = ArbThread::new(
                &snapshot,
                engine_cfg,
                thread_flashable,
                n_assets,
                signer,
                arb_sim,
                bids,
                Checker::new(Some(quarantine_file)),
            );
            drop(snapshot);
            let q = engine.quarantine_addresses(&held);
            if q > 0 {
                tracing::warn!(pools = q, "arb: pools quarantined from earlier evidence");
            }
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
    pub opportunity: Opportunity,
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

/// Step `c` (a sorted `k`-combination of indices from `0..n`) to its next
/// combination in lexicographic order; `false` once it has wrapped back to
/// the first (`0..k`). Every `k`-subset of `0..n` is visited exactly once.
pub fn next_combination(c: &mut [usize], n: usize) -> bool {
    let k = c.len();
    if k == 0 || k > n {
        return false;
    }
    let Some(i) = (0..k).rev().find(|&i| {
        c.get(i)
            .is_some_and(|&x| x < n.saturating_sub(k).saturating_add(i))
    }) else {
        for (j, slot) in c.iter_mut().enumerate() {
            *slot = j;
        }
        return false;
    };
    if let Some(x) = c.get_mut(i) {
        *x = x.saturating_add(1);
    }
    for j in i.saturating_add(1)..k {
        let prev = c.get(j.saturating_sub(1)).copied().unwrap_or(0);
        if let Some(x) = c.get_mut(j) {
            *x = prev.saturating_add(1);
        }
    }
    true
}

/// Whether `order` (indices into `senders`) keeps each sender's
/// transactions in ascending, consecutive nonce order: what the chain
/// requires of them in one bundle (the simulator does not check nonces).
#[must_use]
pub fn nonces_in_order(order: &[usize], senders: &[(Address, u64)]) -> bool {
    for (k, &i) in order.iter().enumerate() {
        let Some(&(who, nonce)) = senders.get(i) else {
            return false;
        };
        let prev = order
            .get(..k)
            .unwrap_or(&[])
            .iter()
            .rev()
            .filter_map(|&j| senders.get(j))
            .find(|(s, _)| *s == who);
        if let Some(&(_, n)) = prev {
            if n.checked_add(1) != Some(nonce) {
                return false;
            }
        }
    }
    true
}

/// Most members a group search tries exhaustively (every subset of size
/// 1..=[`MAX_ORDERED`] in every order). Past this each member is first
/// ranked by its solo backrun and only the top [`COMBO_MAX`] are searched.
/// The cost of the exhaustive search is `sum_{k=1..=4} C(n,k) * k!`
/// simulations; at `n = 8` that is 2 080 orders.
pub const COMBO_MAX: usize = 8;

/// One public group's search for the most profitable subset (at most
/// [`MAX_ORDERED`] members) and order, simulated and held to its claim:
/// every subset of size 1..=[`MAX_ORDERED`] in every order
/// ([`best_ordering`]); the subset and order whose simulated backrun keeps
/// the most WETH wins. Past [`COMBO_MAX`] members each is first ranked by
/// its solo backrun and only the top `COMBO_MAX` are searched.
///
/// Resumable: [`Self::unit`] does one member's ranking or one subset in
/// every order, so the engine serves MEV-Share hints between units. The
/// units run in the order a single pass would, with the same comparisons,
/// so the result is the same.
pub struct GroupSearch {
    pub pool: PoolId,
    /// The members (newest first) as the simulator runs them, their
    /// senders and nonces, and their signed bytes and arrival.
    victims: Vec<SimTx>,
    senders: Vec<(Address, u64)>,
    raw: Vec<(Bytes, Instant)>,
    /// Ranking (more than `COMBO_MAX` members): the next to rank, and the
    /// solo results so far.
    rank_next: usize,
    ranked: Vec<(I256, usize)>,
    /// The members searched (indices into `victims`); empty until known.
    cand: Vec<usize>,
    /// The subsets of `cand` still to try; `None` until `cand` is known.
    subsets: Option<Subsets>,
    /// The best so far; its order indexes `victims`.
    pub best: Option<BestOrder>,
}

impl GroupSearch {
    /// A search over `members` (newest first) of `pool`'s group.
    #[must_use]
    pub fn new(pool: PoolId, members: &[&PendingPublic]) -> Self {
        Self::from_parts(
            pool,
            members.iter().map(|p| pending_to_sim_tx(p)).collect(),
            members.iter().map(|p| (p.sender, p.nonce)).collect(),
            members.iter().map(|p| (p.raw.clone(), p.seen_at)).collect(),
        )
    }

    fn from_parts(
        pool: PoolId,
        victims: Vec<SimTx>,
        senders: Vec<(Address, u64)>,
        raw: Vec<(Bytes, Instant)>,
    ) -> Self {
        Self {
            pool,
            victims,
            senders,
            raw,
            rank_next: 0,
            ranked: Vec::new(),
            cand: Vec::new(),
            subsets: None,
            best: None,
        }
    }

    /// One unit of the search; `true` when it is complete ([`Self::best`]).
    #[allow(clippy::too_many_arguments)]
    pub fn unit<P: liq_sim::DatabaseRef<Error = SimError>>(
        &mut self,
        engine: &mut Engine,
        sim: &mut Simulator<P>,
        env: &SimEnv,
        ours: (Address, Address, u64),
        bid: &impl Fn(PoolId) -> u16,
        check: &mut Checker,
    ) -> Result<bool, SimError> {
        let n = self.victims.len();
        if self.subsets.is_none() {
            if n == 0 {
                return Ok(true);
            }
            if n <= COMBO_MAX {
                self.cand = (0..n).collect();
            } else if self.rank_next < n {
                // Rank one member by its solo backrun.
                let i = self.rank_next;
                self.rank_next = i.saturating_add(1);
                if let (Some(v), Some(s)) = (self.victims.get(i..=i), self.senders.get(i..=i)) {
                    if let Some(o) = best_ordering(engine, sim, env, v, s, ours, bid, check)? {
                        self.ranked.push((o.sim.net_profit_wei, i));
                    }
                }
                return Ok(false);
            } else {
                // Stable: ties keep member order.
                self.ranked.sort_by_key(|a| std::cmp::Reverse(a.0));
                self.cand = self
                    .ranked
                    .iter()
                    .take(COMBO_MAX)
                    .map(|&(_, i)| i)
                    .collect();
            }
            if self.cand.is_empty() {
                return Ok(true);
            }
            self.subsets = Some(Subsets::new(self.cand.len()));
        }
        let Some(subsets) = self.subsets.as_mut() else {
            return Ok(true);
        };
        let Some(combo) = subsets.current() else {
            return Ok(true);
        };
        // One subset, in every order.
        let members: smallvec::SmallVec<[usize; MAX_ORDERED]> = combo
            .iter()
            .filter_map(|&c| self.cand.get(c).copied())
            .collect();
        let subset: Vec<SimTx> = members
            .iter()
            .filter_map(|&i| self.victims.get(i).cloned())
            .collect();
        let subset_senders: Vec<(Address, u64)> = members
            .iter()
            .filter_map(|&i| self.senders.get(i).copied())
            .collect();
        if let Some(mut c) =
            best_ordering(engine, sim, env, &subset, &subset_senders, ours, bid, check)?
        {
            for idx in &mut c.order {
                *idx = members.get(*idx).copied().unwrap_or(*idx);
            }
            if self
                .best
                .as_ref()
                .is_none_or(|b| c.sim.net_profit_wei > b.sim.net_profit_wei)
            {
                self.best = Some(c);
            }
        }
        Ok(!self.subsets.as_mut().is_some_and(Subsets::advance))
    }
}

/// Every subset of `0..n` of size 1..=[`MAX_ORDERED`], smallest first and
/// each size in lexicographic order — the order a group search tries them.
#[derive(Clone, Debug)]
pub struct Subsets {
    n: usize,
    k: usize,
    combo: smallvec::SmallVec<[usize; MAX_ORDERED]>,
}

impl Subsets {
    #[must_use]
    pub fn new(n: usize) -> Self {
        Self {
            n,
            k: usize::from(n > 0),
            combo: (0..usize::from(n > 0)).collect(),
        }
    }

    /// The subset to try now; `None` once every one has been.
    #[must_use]
    pub fn current(&self) -> Option<smallvec::SmallVec<[usize; MAX_ORDERED]>> {
        (self.k > 0 && self.k <= self.n.min(MAX_ORDERED)).then(|| self.combo.clone())
    }

    /// Move to the next subset; `false` when there is none.
    pub fn advance(&mut self) -> bool {
        if self.current().is_none() {
            return false;
        }
        if !next_combination(&mut self.combo, self.n) {
            self.k = self.k.saturating_add(1);
            if self.k > self.n.min(MAX_ORDERED) {
                return false;
            }
            self.combo = (0..self.k).collect();
        }
        true
    }
}

/// Every order of `victims` (at most [`MAX_ORDERED`]) that keeps each
/// sender's nonces in order: run them in that order on the simulator's
/// state and take the logs they emit, solve the backrun on those
/// ([`liq_arb::Engine::on_victim`]), then simulate the victims in that order
/// followed by our plan, priced, and hold it to the plan's claim
/// ([`Checker`]). The order whose simulated backrun keeps the most WETH
/// wins. Orders where a victim reverts are skipped (a user's transaction
/// must not fail in our bundle), and so are plans the simulation refuses.
/// `None` when no order leaves a backrun.
#[allow(clippy::too_many_arguments)]
pub fn best_ordering<P: liq_sim::DatabaseRef<Error = SimError>>(
    engine: &mut Engine,
    sim: &mut Simulator<P>,
    env: &SimEnv,
    victims: &[SimTx],
    senders: &[(Address, u64)],
    ours: (Address, Address, u64),
    bid: &impl Fn(PoolId) -> u16,
    check: &mut Checker,
) -> Result<Option<BestOrder>, SimError> {
    let k = victims.len();
    if k == 0 || k > MAX_ORDERED {
        return Ok(None);
    }
    let mut order: smallvec::SmallVec<[usize; MAX_ORDERED]> = (0..k).collect();
    let mut best: Option<BestOrder> = None;
    loop {
        if nonces_in_order(&order, senders) {
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
                if let Ok(Ok(o)) = engine.on_victim(&refs, bid) {
                    if let Some(s) = check.run(engine, sim, env, &ordered, &o, ours)? {
                        if best
                            .as_ref()
                            .is_none_or(|b| s.net_profit_wei > b.sim.net_profit_wei)
                        {
                            best = Some(BestOrder {
                                order: order.clone(),
                                opportunity: o,
                                sim: s,
                            });
                        }
                    }
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
        feed.end_block(
            10,
            120,
            B256::repeat_byte(0xaa),
            fee(),
            Instant::now(),
            price,
        );
        let t = cons.pop().unwrap();
        assert_eq!((t.block, t.timestamp, t.fee), (10, 120, fee()));
        assert_eq!(&t.per_eth[..2], &[U256::from(300u64), U256::from(100u64)]);
        assert!(t.per_eth[2..].iter().all(|p| p.is_zero()));
        assert_eq!(&t.reserved[..usize::from(t.n_reserved)], &[5, 9, 2]);
        feed.begin_block();
        feed.end_block(
            11,
            132,
            B256::repeat_byte(0xbb),
            fee(),
            Instant::now(),
            price,
        );
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

    /// `pending_to_sim_tx` carries every field the simulator needs to run
    /// a pending tx exactly as the pool validated it: caller, callee, value,
    /// calldata, gas, and the EIP-2930 access list. A contract creation
    /// (`to = None`) maps to `Address::ZERO` — the pool listener drops
    /// those at intake, so the simulator never sees one in practice, but
    /// the conversion must not panic on it.
    #[test]
    fn pending_to_sim_tx_carries_every_field() {
        let caller = Address::repeat_byte(0x11);
        let callee = Address::repeat_byte(0x22);
        let key = B256::repeat_byte(0x33);
        let data = Bytes::from_static(&[0x12, 0x34, 0x56, 0x78]);
        let p = PendingPublic {
            hash: B256::repeat_byte(0x01),
            sender: caller,
            to: Some(callee),
            value: U256::from(1_000_000_000_000u64),
            input: data.clone(),
            gas_limit: 250_000,
            nonce: 42,
            access_list: vec![(callee, vec![key])],
            max_priority_fee_per_gas: 2_000_000_000,
            max_fee_per_gas: 50_000_000_000,
            raw: Bytes::from_static(&[0xff]),
            seen_at: std::time::Instant::now(),
        };
        let tx = pending_to_sim_tx(&p);
        assert_eq!(tx.caller, caller);
        assert_eq!(tx.to, callee);
        assert_eq!(tx.value, U256::from(1_000_000_000_000u64));
        assert_eq!(tx.data, data);
        assert_eq!(tx.gas_limit, 250_000);
        assert_eq!(tx.access_list.len(), 1);
        assert_eq!(tx.access_list[0].0, callee);
        assert_eq!(tx.access_list[0].1, vec![key]);
    }

    #[test]
    fn pending_to_sim_tx_creation_maps_to_zero_not_a_panic() {
        let p = PendingPublic {
            hash: B256::repeat_byte(0x02),
            sender: Address::repeat_byte(0x44),
            to: None,
            value: U256::ZERO,
            input: Bytes::new(),
            gas_limit: 100_000,
            nonce: 0,
            access_list: Vec::new(),
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 0,
            raw: Bytes::new(),
            seen_at: std::time::Instant::now(),
        };
        let tx = pending_to_sim_tx(&p);
        assert_eq!(tx.to, Address::ZERO);
    }

    /// Every 2-subset of 0..4 exactly once — C(4,2) = 6 — in lex order, and
    /// the slice back at [0, 1] afterwards.
    #[test]
    fn combinations_cover_every_subset_once() {
        let mut c = [0usize, 1];
        let n = 4;
        let mut seen = std::collections::HashSet::new();
        loop {
            assert!(seen.insert(c), "repeat {c:?}");
            if !next_combination(&mut c, n) {
                break;
            }
        }
        assert_eq!(seen.len(), 6);
        assert_eq!(c, [0, 1]);
    }

    /// C(6,3) = 20 three-subsets, each once, back at [0, 1, 2].
    #[test]
    fn combinations_of_three_from_six() {
        let mut c = [0usize, 1, 2];
        let n = 6;
        let mut seen = std::collections::HashSet::new();
        loop {
            assert!(seen.insert(c), "repeat {c:?}");
            if !next_combination(&mut c, n) {
                break;
            }
        }
        assert_eq!(seen.len(), 20);
        assert_eq!(c, [0, 1, 2]);
    }

    /// A single-element combination visits each index once then wraps.
    #[test]
    fn single_combination_visits_each_index() {
        let mut c = [0usize];
        let n = 5;
        let mut seen = std::collections::HashSet::new();
        loop {
            assert!(seen.insert(c[0]), "repeat {}", c[0]);
            if !next_combination(&mut c, n) {
                break;
            }
        }
        assert_eq!(seen.len(), 5);
        assert_eq!(c, [0]);
    }

    /// k = n visits the one subset (all of them) then wraps.
    #[test]
    fn full_combination_visits_once() {
        let mut c = [0usize, 1, 2];
        let n = 3;
        let mut seen = std::collections::HashSet::new();
        loop {
            assert!(seen.insert(c), "repeat {c:?}");
            if !next_combination(&mut c, n) {
                break;
            }
        }
        assert_eq!(seen.len(), 1);
        assert_eq!(c, [0, 1, 2]);
    }

    /// k > n never yields a step.
    #[test]
    fn combination_with_k_over_n_is_empty() {
        let mut c = [0usize, 1, 2, 3, 4];
        assert!(!next_combination(&mut c, 3));
    }

    /// The total number of (subset, order) pairs `best_combo_search` tries
    /// for `n` victims is `sum_{k=1..=min(n,4)} C(n,k) * k!`. For n = 8 that
    /// is 8 + 56 + 336 + 1680 = 2080 — every pair once.
    #[test]
    fn combo_search_enumerates_every_subset_and_order() {
        let n = 8usize;
        let max_k = n.min(MAX_ORDERED);
        let mut count = 0usize;
        for k in 1..=max_k {
            let mut combo = (0..k).collect::<smallvec::SmallVec<[usize; MAX_ORDERED]>>();
            loop {
                let mut order = combo.to_vec();
                loop {
                    count += 1;
                    if !next_permutation(&mut order) {
                        break;
                    }
                }
                if !next_combination(&mut combo, n) {
                    break;
                }
            }
        }
        // C(8,1)*1! + C(8,2)*2! + C(8,3)*3! + C(8,4)*4!
        // = 8 + 56 + 336 + 1680 = 2080
        assert_eq!(count, 2080);
    }

    fn committed_toml() -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/arb.toml"),
        )
        .unwrap()
    }

    /// The committed `arb.toml` parses and its bid table validates; a
    /// table the submit path would refuse a job from (a MEV-Share bid that
    /// is not a whole percent, a bid over 100%) or that lists a singleton
    /// as an uncontested pool is refused whole.
    #[test]
    fn committed_bids_parse_and_bad_tables_are_refused() {
        let raw = committed_toml();
        let t = ArbToml::parse(&raw).unwrap();
        assert_eq!(t.bids.of(Intake::Top), t.bids.top);
        assert!(ArbToml::parse(&raw.replace(
            "[bids.mevshare]
contested_bps = 5_000",
            "[bids.mevshare]
contested_bps = 5_050"
        ))
        .unwrap_err()
        .contains("whole percent"));
        assert!(ArbToml::parse(&raw.replace(
            "[bids.public]
contested_bps = 5_000",
            "[bids.public]
contested_bps = 10_001"
        ))
        .unwrap_err()
        .contains("10 000"));
        let pm = liq_router::solver::V4_POOL_MANAGER;
        assert!(ArbToml::parse(
            &raw.replace("uncontested = []", &format!("uncontested = [\"{pm}\"]"))
        )
        .unwrap_err()
        .contains("singleton"));
    }

    /// A saved change to `arb.toml` reaches the published table without a
    /// restart; a broken save leaves the table in force.
    #[test]
    fn bid_table_reloads_from_the_file() {
        let dir = std::env::temp_dir().join(format!("arb-bids-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("arb.toml");
        let raw = committed_toml();
        std::fs::write(&path, &raw).unwrap();
        let first = ArbToml::parse(&raw).unwrap().bids;
        let bids = Arc::new(ArcSwap::from_pointee(first.clone()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let h = spawn_bid_reload(path.clone(), Arc::clone(&bids), Arc::clone(&stop)).unwrap();
        let wait_for = |want: &dyn Fn(&BidToml) -> bool| {
            let until = Instant::now() + Duration::from_secs(20);
            while Instant::now() < until {
                if want(&bids.load()) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            false
        };
        // Past the file system's timestamp resolution.
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(
            &path,
            raw.replace(
                "[bids.top]
contested_bps = 5_000",
                "[bids.top]
contested_bps = 7_000",
            ),
        )
        .unwrap();
        assert!(wait_for(&|b| b.top.contested_bps == 7_000), "reloaded");
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(&path, "enabled = [").unwrap();
        std::thread::sleep(Duration::from_secs(5));
        assert_eq!(
            bids.load().top.contested_bps,
            7_000,
            "a broken save changes nothing"
        );
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        h.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every value lands in the bucket whose floor is at or below it and
    /// whose next floor is above it; quantiles come back as those floors.
    #[test]
    fn latency_buckets_bound_their_values() {
        for us in (0u64..5_000).chain([65_535, 1_000_000, 123_456_789]) {
            let b = lat_bucket(us);
            assert!(lat_floor(b) <= us, "{us} floor {}", lat_floor(b));
            assert!(us < lat_floor(b + 1), "{us} next {}", lat_floor(b + 1));
        }
        let mut l = Latency::default();
        assert_eq!(l.quantile_us(Intake::Public, 500), None);
        for us in 1..=100u64 {
            l.record(Intake::Public, Duration::from_micros(us * 100));
        }
        assert_eq!(l.count(Intake::Public), 100);
        assert_eq!(l.count(Intake::Top), 0);
        let p50 = l.quantile_us(Intake::Public, 500).unwrap();
        let p99 = l.quantile_us(Intake::Public, 990).unwrap();
        assert!(p50 <= 5_000 && 5_000 < lat_floor(lat_bucket(p50) + 1));
        assert!(p99 <= 9_900 && 9_900 < lat_floor(lat_bucket(p99) + 1));
    }

    /// One sender's transactions go in ascending, consecutive nonce order
    /// in a bundle; other senders' are free around them.
    #[test]
    fn nonce_order_is_enforced_per_sender() {
        let (a, b) = (Address::repeat_byte(1), Address::repeat_byte(2));
        let s = [(a, 5), (b, 9), (a, 6), (a, 8)];
        assert!(nonces_in_order(&[0, 1, 2], &s));
        assert!(nonces_in_order(&[1, 0, 2], &s));
        assert!(!nonces_in_order(&[2, 0], &s), "6 before 5");
        assert!(!nonces_in_order(&[0, 3], &s), "5 then 8 skips 6 and 7");
        assert!(nonces_in_order(&[3], &s), "alone, any nonce");
    }

    /// A group search, resumed a unit at a time, tries the subsets in the
    /// order the single-pass search did — every size from 1 to
    /// `min(n, MAX_ORDERED)`, each in `next_combination` order — so it
    /// finds the same best. Oracle: that single pass, written out.
    #[test]
    fn subsets_resume_in_the_single_pass_order() {
        for n in 0..=COMBO_MAX {
            let mut single: Vec<Vec<usize>> = Vec::new();
            for k in 1..=n.min(MAX_ORDERED) {
                let mut combo: Vec<usize> = (0..k).collect();
                loop {
                    single.push(combo.clone());
                    if !next_combination(&mut combo, n) {
                        break;
                    }
                }
            }
            let mut resumed: Vec<Vec<usize>> = Vec::new();
            let mut s = Subsets::new(n);
            while let Some(c) = s.current() {
                resumed.push(c.to_vec());
                if !s.advance() {
                    break;
                }
            }
            assert_eq!(resumed, single, "n = {n}");
            assert!(s.current().is_none() || n == 0, "exhausted at n = {n}");
        }
    }
}
