//! One liquidation through our searcher: the pre-state, the bot built on
//! it, the store seeded, the drain run at the bot's tip; then the job it
//! produced and the real liquidator's transaction, each measured on the same
//! pre-state.

use super::bot::{self, BotSpec};
use super::fixture::Event;
use super::outcome::{measure, Outcome};
use super::rpc::Upstream;
use super::seed::{self, Synth, Views};
use super::server::{Served, Server};
use super::state::{AtBlock, Basis, Header, PreState, Tx};
use alloy_primitives::{address, Address, B256, U256};
use liq_bot::bind::BoundProtocol;
use liq_node::{AfterBlock, AfterBlockCtx};
use liq_sim::{BlockRef, BlockState, SimError, StateProviderFactory};
use liq_state::{StateStore, StoreConfig, UndoCapacity};
use liq_types::PositionKey;
use revm::context::TxEnv;
use revm::database::CacheDB;
use revm::primitives::TxKind;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const OPERATOR: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
pub const BACKRUN: Address = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
pub const SINK: Address = address!("11fa49084B4D63b156a4C8238291A562019bA49d");

/// The bot's own simulator, recording each bundle it is asked to verify, so
/// a refused one can be traced afterwards.
pub struct RecordingSim {
    pub inner: liq_bot::drain::LiveSim,
    pub seen: Arc<std::sync::Mutex<Vec<liq_sim::Bundle>>>,
}

impl liq_bot::drain::DrainSim for RecordingSim {
    fn executor(&self) -> Address {
        self.inner.executor()
    }
    fn verify(
        &self,
        bundle: &liq_sim::Bundle,
        at: &liq_sim::NextBlock,
    ) -> Result<liq_sim::SimOutcome, SimError> {
        if let Ok(mut s) = self.seen.lock() {
            s.push(bundle.clone());
        }
        self.inner.verify(bundle, at)
    }
    fn ready(&self) -> bool {
        self.inner.ready()
    }
}

/// The pre-state, as a node would hand it to the simulator.
struct PreFactory(Arc<CacheDB<AtBlock>>);

impl StateProviderFactory for PreFactory {
    fn state_at(&self, _at: BlockRef) -> Result<BlockState, SimError> {
        Ok(BlockState::new(Arc::clone(&self.0)))
    }
}

#[derive(Debug)]
pub enum Verdict {
    /// Not a market the bot is bound to.
    OutOfScope(String),
    /// The harness could not build the case (not a finding about the bot).
    Harness(String),
    /// The bot built no job for the borrower.
    NoJob,
    /// The bot built a job; `ours` is what it captured on the pre-state.
    Job {
        ours: Outcome,
        gas_limit: u64,
        bid_bps: u16,
    },
}

/// The bot's health for the borrower against the pool's own
/// `getUserAccountData` on the pre-state (the oracle): HF in WAD, collateral
/// and debt in the pool's base currency (USD, 8 decimals).
#[derive(Clone, Debug)]
pub struct HealthCheck {
    pub ours: Option<(U256, U256, U256)>,
    pub chain: (U256, U256, U256),
    pub ours_error: Option<String>,
    /// Aave V2: the pool's base is ETH (18 decimals) while ours is restated
    /// in USD, so only the health factor is comparable.
    pub hf_only: bool,
}

impl HealthCheck {
    #[must_use]
    pub fn agrees(&self) -> bool {
        if self.hf_only {
            return self.ours.map(|o| o.0) == Some(self.chain.0);
        }
        self.ours == Some(self.chain)
    }
}

pub struct Report {
    pub basis: Option<Basis>,
    pub health: Option<HealthCheck>,
    /// Morpho and Compound: the bot's health beside the protocol's own
    /// figures on the pre-state.
    pub health_note: Option<String>,
    pub diag: Vec<String>,
    pub verdict: Verdict,
    pub theirs: Option<Outcome>,
    pub upstream: u64,
    pub served: u64,
    pub unknown: Vec<String>,
    pub seconds: u64,
}

fn store_at(tip: u64) -> StateStore {
    StateStore::new(StoreConfig {
        base: tip - 1,
        positions: 1 << 12,
        markets: 1 << 10,
        undo: UndoCapacity {
            ops: 4096,
            extras: 512,
            rows: 512,
        },
    })
}

/// A bundle the drain's simulation refused, run again on the pre-state with
/// the Executor where the simulator put it, listing every call frame that
/// failed (innermost first).
fn trace_bundle(pre: &PreState, b: &liq_sim::Bundle, executor: Address) -> Vec<String> {
    let mut out = vec![format!(
        "simulated bundle: trigger {:?}, {} call(s), min profit {}",
        b.trigger,
        b.calls.len(),
        b.min_profit
    )];
    let mut db = pre.db.clone();
    let code =
        match liq_sim::executor_stack(&liq_sim::ExecutorSpec::mainnet(OPERATOR, BACKRUN, SINK)) {
            Ok(s) => s.placements(executor).to_vec(),
            Err(e) => {
                out.push(format!("executor stack: {e}"));
                return out;
            }
        };
    for (at, c) in code {
        if let Err(e) = liq_sim::place_code(&mut db, at, c) {
            out.push(format!("place code: {e}"));
            return out;
        }
    }
    for call in &b.calls {
        // The operator's gas money, as `measure` gives it.
        let mut info = revm::database_interface::DatabaseRef::basic_ref(&db, call.caller)
            .ok()
            .flatten()
            .unwrap_or_default();
        info.balance = alloy_primitives::U256::from(10u128.pow(21));
        db.insert_account_info(call.caller, info);
        let env = TxEnv::builder()
            .caller(call.caller)
            .kind(TxKind::Call(call.to))
            .value(call.value)
            .data(call.data.clone())
            .gas_limit(call.gas_limit)
            .gas_price(u128::from(pre.block.base_fee))
            .chain_id(Some(1))
            .build_fill();
        match super::state::trace(&mut db.clone(), &pre.block, env.clone(), true) {
            Ok((_, _, events)) => {
                out.push("every frame and transfer:".into());
                out.extend(events.into_iter().map(|e| format!("    {e}")));
            }
            Err(e) => out.push(format!("trace: {e}")),
        }
        match super::state::trace_reverts(&mut db, &pre.block, env) {
            Ok((ok, frames)) => {
                out.push(format!("call to {:#x}: success {ok}", call.to));
                for f in frames {
                    // Aave's `liquidationCall(collateral, debt, user, debtToCover, receiveAToken)`.
                    let args =
                        if f.selector == [0x00, 0xa7, 0x18, 0xa9] && f.input.len() >= 4 + 5 * 32 {
                            let w = |i: usize| {
                                alloy_primitives::U256::from_be_slice(
                                    &f.input[4 + 32 * i..4 + 32 * (i + 1)],
                                )
                            };
                            format!(
                                " liquidationCall(coll {:#x}, debt {:#x}, debtToCover {})",
                                Address::from_word(w(0).into()),
                                Address::from_word(w(1).into()),
                                w(3)
                            )
                        } else {
                            String::new()
                        };
                    out.push(format!(
                        "  depth {} {:#x} -> {:#x} sel 0x{}{} {} output 0x{}",
                        f.depth,
                        f.from,
                        f.to,
                        alloy_primitives::hex::encode(f.selector),
                        args,
                        f.result,
                        alloy_primitives::hex::encode(&f.output)
                    ));
                }
            }
            Err(e) => out.push(format!("trace: {e}")),
        }
    }
    out
}

/// One swap leg of a plan, as the Executor reads it.
fn swap_line(s: &liq_plan::SwapLeg) -> String {
    format!(
        "venue {} {:#x} -> {:#x}, amount {}, flags {:#04x}, data 0x{}",
        s.venue,
        s.token_in,
        s.token_out,
        s.amount,
        s.flags,
        alloy_primitives::hex::encode(&s.data)
    )
}

/// Every frame and Transfer of the job's transaction on the pre-state, run
/// as `measure` runs it (`LIQ_HISTORY_TRACE`).
fn trace_job(
    pre: &PreState,
    env: TxEnv,
    code: &[(Address, revm::bytecode::Bytecode)],
) -> Vec<String> {
    let mut db = pre.db.clone();
    for (at, c) in code {
        if let Err(e) = liq_sim::place_code(&mut db, *at, c.clone()) {
            return vec![format!("place code: {e}")];
        }
    }
    let mut info = revm::database_interface::DatabaseRef::basic_ref(&db, OPERATOR)
        .ok()
        .flatten()
        .unwrap_or_default();
    info.balance = alloy_primitives::U256::from(10u128.pow(21));
    db.insert_account_info(OPERATOR, info);
    match super::state::trace(&mut db, &pre.block, env, true) {
        Ok((ok, _, events)) => std::iter::once(format!("our job, success {ok}:"))
            .chain(events.into_iter().map(|e| format!("    {e}")))
            .collect(),
        Err(e) => vec![format!("trace: {e}")],
    }
}

/// The hot thread's call after a committed block, at the tip.
fn ctx<'a>(
    store: &'a StateStore,
    dirty: &'a liq_node::CollapsedDirty,
    tip: &Header,
) -> AfterBlockCtx<'a> {
    AfterBlockCtx {
        store,
        dirty,
        touched: &dirty.positions,
        block: tip.number,
        hash: tip.hash,
        timestamp: tip.timestamp,
        gas_limit: tip.gas_limit,
        gas_used: tip.gas_used,
        base_fee_per_gas: tip.base_fee,
    }
}

fn wait_for(limit: Duration, ready: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    ready()
}

alloy_sol_types::sol! {
    function getUserAccountData(address user) external view returns (
        uint256 totalCollateralBase,
        uint256 totalDebtBase,
        uint256 availableBorrowsBase,
        uint256 currentLiquidationThreshold,
        uint256 ltv,
        uint256 healthFactor
    );
}

/// The protocol prices the bot's own reader published for the tip, for one
/// market, as the price vector the adapter takes.
fn protocol_px(
    prices: &liq_bot::protocol_prices::ReaderShared,
    key: &PositionKey,
    at: &Header,
) -> Result<liq_types::PriceVector, String> {
    let batch = prices.latest.load_full();
    let batch = batch.as_ref().as_ref().ok_or("no price batch")?;
    let mut px: Vec<liq_types::Price> = Vec::new();
    for e in batch
        .entries
        .iter()
        .filter(|e| e.protocol == key.protocol && e.market == key.market)
    {
        let i = usize::from(e.asset.0);
        while px.len() <= i {
            let asset = liq_types::AssetId(u16::try_from(px.len()).map_err(|e| e.to_string())?);
            px.push(liq_types::Price {
                asset,
                price: liq_types::Ray::from_raw(U256::ZERO),
                source: liq_types::SourceKind::Canonical,
                block: at.number,
                ts: at.timestamp,
            });
        }
        px[i].price = e.price;
    }
    Ok(liq_types::PriceVector(px))
}

/// The adapter's health for `id` at `px`, beside the pool's own answer.
fn health_check(
    v: &Views<'_, CacheDB<AtBlock>>,
    proto: &dyn liq_protocol::Protocol,
    store: &StateStore,
    id: liq_types::PositionId,
    px: &liq_types::PriceVector,
) -> Result<HealthCheck, String> {
    let view = store.view(v.at.timestamp);
    let pos = view.position(id).map_err(|e| e.to_string())?;
    let user = pos.key.user;
    let pool = proto.health_probe(pos).map_err(|e| e.to_string())?.to;
    let a = v.call(pool, getUserAccountDataCall { user })?;
    let chain = (a.healthFactor, a.totalCollateralBase, a.totalDebtBase);
    let ours = proto.health(view.position(id).map_err(|e| e.to_string())?, px);
    let scale = U256::from(10u64).pow(U256::from(10u8));
    let (ours, ours_error) = match ours {
        Ok(h) => (
            Some((
                h.hf.raw() / U256::from(1_000_000_000u64),
                h.collateral_value.raw() / scale,
                h.debt_value.raw() / scale,
            )),
            None,
        ),
        Err(e) => (None, Some(e.to_string())),
    };
    Ok(HealthCheck {
        ours,
        chain,
        ours_error,
        hf_only: proto.id() == liq_bot::bind::AAVE_V2_PROTOCOL,
    })
}

/// The adapter's health factor beside the protocol's own figures on the
/// pre-state. Morpho: collateral at the market oracle's price times LLTV over
/// the borrow at the stored totals (no interest since `lastUpdate`, so the
/// chain figure is an upper bound). Compound: the Comptroller's
/// `getAccountLiquidity`.
fn health_note(
    v: &Views<'_, CacheDB<AtBlock>>,
    proto: &dyn liq_protocol::Protocol,
    store: &StateStore,
    id: liq_types::PositionId,
    px: &liq_types::PriceVector,
    chain: &Chain,
) -> String {
    let ray = |x: U256| x.saturating_to::<u128>() as f64 / 1e27;
    let view = store.view(v.at.timestamp);
    let ours = match view.position(id) {
        Ok(pos) => match proto.health(pos, px) {
            Ok(h) => format!("ours hf {:.6}", ray(h.hf.raw())),
            Err(e) => format!("ours failed ({e})"),
        },
        Err(e) => format!("no position ({e})"),
    };
    match chain {
        Chain::Aave => ours,
        Chain::Morpho(c) => {
            let borrowed = (U256::from(c.borrow_shares)
                * (U256::from(c.total_borrow_assets) + U256::from(1u8)))
            .div_ceil(U256::from(c.total_borrow_shares) + U256::from(1_000_000u32));
            let e18 = U256::from(10u64).pow(U256::from(18u8));
            let e36 = e18 * e18;
            let max = U256::from(c.collateral) * c.price / e36 * c.lltv / e18;
            let hf = if borrowed.is_zero() {
                f64::INFINITY
            } else {
                max.saturating_to::<u128>() as f64 / borrowed.saturating_to::<u128>() as f64
            };
            format!("{ours}; singleton at stored totals: hf {hf:.6} (max borrow {max}, borrowed {borrowed})")
        }
        Chain::Compound(comptroller) => {
            let pos_user = view.position(id).map(|p| p.key.user).unwrap_or_default();
            match seed::compound_liquidity(v, *comptroller, pos_user) {
                Ok((liq, short)) => {
                    format!("{ours}; Comptroller: shortfall {short}, liquidity {liq}")
                }
                Err(e) => format!("{ours}; Comptroller: {e}"),
            }
        }
        Chain::Euler(vault) => {
            let pos_user = view.position(id).map(|p| p.key.user).unwrap_or_default();
            match seed::euler_liquidity(v, *vault, pos_user) {
                Ok((c, l)) if !l.is_zero() => format!(
                    "{ours}; vault accountLiquidity: hf {:.6} (collateral {c}, liability {l})",
                    c.saturating_to::<u128>() as f64 / l.saturating_to::<u128>() as f64
                ),
                Ok((c, l)) => {
                    format!("{ours}; vault accountLiquidity: collateral {c}, liability {l}")
                }
                Err(e) => format!("{ours}; vault accountLiquidity: {e}"),
            }
        }
        Chain::Silo(silo) => {
            let pos_user = view.position(id).map(|p| p.key.user).unwrap_or_default();
            match seed::silo_solvent(v, *silo, pos_user) {
                Ok(ok) => format!("{ours}; silo isSolvent: {ok}"),
                Err(e) => format!("{ours}; silo isSolvent: {e}"),
            }
        }
    }
}

/// The routing comparison for one (seize, repay) pair at the whole seize:
/// the exit today's routing finds (direct and through WETH: the book
/// without its graph), the one graph routing finds (what the bot now
/// executes: two-hop exits through the graph's intermediate tokens too),
/// and the graph's best route of up to four hops (not executable until the
/// Executor's exact-chain venue: shown for comparison). Each net of its
/// hop gas, in the target's raw units; into the debt and into WETH.
fn compare_routing(
    book: &liq_router::PoolBook,
    coll: liq_types::AssetId,
    debt: liq_types::AssetId,
    seized: alloy_primitives::U256,
    snap: &liq_bot::bands::BandInputsSnapshot,
) -> Vec<String> {
    use liq_router::graph::{search, SearchBudget};
    let budget = liq_router::SolveBudget::default();
    let mut plain = book.clone();
    plain.set_graph(None);
    let mut out = Vec::new();
    let targets = [Some(("debt", debt)), book.hub().map(|w| ("WETH", w))];
    for (name, to) in targets.into_iter().flatten() {
        let Some(&per) = snap.per_eth.get(&to) else {
            out.push(format!(
                "  routing {}->{} ({name}): no gas price for the target",
                coll.0, to.0
            ));
            continue;
        };
        let gas = liq_router::GasTerms {
            base_fee_wei: snap.base_fee,
            priority_fee_wei: snap.priority_fee,
            out_per_eth: per,
        };
        let net = |q: &Result<liq_router::ExitQuote, liq_router::RouteError>| match q {
            Ok(q) => {
                let cost = gas.cost_in_out(q.hop_gas).unwrap_or_default();
                let via = match (&q.hub, &q.chain) {
                    (Some(h), _) => format!("via {}", h.hub.0),
                    (None, Some(c)) => {
                        let path: Vec<String> = c
                            .hops
                            .iter()
                            .filter_map(|l| {
                                let p = book.get(l.pool)?;
                                Some(p.assets.get(usize::from(l.j))?.0.to_string())
                            })
                            .collect();
                        let split = if q.allocs.is_empty() { "" } else { "direct + " };
                        let more = if c.with.is_empty() {
                            String::new()
                        } else {
                            format!(" + {} more chains", c.with.len())
                        };
                        format!(
                            "{split}chain {} hops: {}{more}",
                            c.hops.len(),
                            path.join(">")
                        )
                    }
                    (None, None) if q.allocs.is_empty() && q.unwrap.is_none() => "no swap".into(),
                    (None, None) => "direct".into(),
                };
                let via = match &q.unwrap {
                    Some(u) => format!("unwrap {}, {via}", u.into.0),
                    None => via,
                };
                format!(
                    "{} ({via}, gas {})",
                    q.amount_out.saturating_sub(cost),
                    q.hop_gas
                )
            }
            Err(e) => format!("none ({e:?})"),
        };
        let today = liq_router::solve_pair(&plain, coll, to, seized, &gas, &budget);
        // Each direct pool's capacity to its seeded window edge, against the
        // size sold: whether the windows bind on this exit.
        let caps: Vec<String> = book
            .legs(coll, to)
            .iter()
            .filter_map(|l| {
                let p = book.get(l.pool)?;
                let cap = p.capacity_in(l.i, l.j)?;
                Some(format!(
                    "{:#x}: {} ({})",
                    p.address,
                    cap,
                    if cap < seized {
                        "under the size"
                    } else {
                        "covers it"
                    }
                ))
            })
            .collect();
        if !caps.is_empty() {
            out.push(format!(
                "  window capacity {}->{} selling {}: {}",
                coll.0,
                to.0,
                seized,
                caps.join("; ")
            ));
        }
        // As the bot sizes a leg: the exit, then refined once (split, flow).
        let graph = liq_router::solve_pair(book, coll, to, seized, &gas, &budget)
            .and_then(|e| liq_router::exact::refine_exit(book, coll, to, &gas, &budget, e));
        let multi = match book.graph() {
            None => "no graph".to_string(),
            Some(g) => match search(
                &g.graph,
                &g.table,
                book,
                &gas,
                coll,
                to,
                seized,
                SearchBudget {
                    max_hops: 4,
                    top_n: 1,
                    max_quotes: 20_000,
                    min_share_bps: 0,
                },
            ) {
                Ok(r) => match r.routes.first() {
                    Some(best) => {
                        let path: Vec<String> = best
                            .edges
                            .iter()
                            .filter_map(|e| g.graph.edge(*e))
                            .map(|e| e.to.0.to_string())
                            .collect();
                        format!(
                            "{} ({} hops: {}{})",
                            best.net,
                            best.edges.len(),
                            path.join(">"),
                            if r.exhausted { ", capped" } else { "" }
                        )
                    }
                    None => "none".into(),
                },
                Err(e) => format!("none ({e:?})"),
            },
        };
        out.push(format!(
            "  routing {}->{} ({name}): today {} | graph {} | graph best <=4 hops {}",
            coll.0,
            to.0,
            net(&today),
            net(&graph),
            multi
        ));
    }
    out
}

/// What the bot's engine and flash index made of the borrower: the fold
/// counts, the adapter's quote, and the flash depth for each repay asset.
#[allow(clippy::too_many_arguments)]
fn diagnose(
    hook: &liq_bot::drain::DrainJoin,
    flash: &liq_flash::FlashIndex,
    routes: &liq_router::RouteTable,
    bands: &liq_bot::bands::BandShared,
    book: &liq_router::PoolBook,
    proto: &dyn liq_protocol::Protocol,
    store: &StateStore,
    id: liq_types::PositionId,
    px: &liq_types::PriceVector,
    ts: u64,
) -> Vec<String> {
    let st = hook.engine.stats();
    let mut out = vec![format!(
        "engine: folds {} emitted {} fold errors {} band {:?}",
        st.folds,
        st.emitted,
        st.fold_errors,
        hook.engine.band(id)
    )];
    let view = store.view(ts);
    let Ok(pos) = view.position(id) else {
        out.push("no position".into());
        return out;
    };
    match proto.quote(pos, px) {
        Ok(Some(q)) => {
            out.push(format!(
                "warm routes: {} pairs at block {}; bands {} over {} pair terms",
                routes.len(),
                routes.block,
                bands.table.load().bands.len(),
                bands.inputs.lock().terms.len()
            ));
            for s in &q.seize_options {
                out.push(format!(
                    "seize asset {} up to {}: exit cap {}",
                    s.asset.0,
                    s.max_seize,
                    routes.exit_cap(s.asset)
                ));
            }
            // The band each (seize, repay) pair gets from the bot's own
            // calculation on its own book: the exit legs the book holds for
            // the pair, how many of those pools carry on-chain state, the
            // published terms, and what `compute_one` makes of them.
            let snap = bands.inputs.lock().clone();
            for s in &q.seize_options {
                for r in &q.repay_options {
                    let key = (pos.key.protocol, s.asset, r.asset);
                    let legs = book.exit_legs(s.asset, r.asset);
                    let live = legs
                        .iter()
                        .filter(|l| book.get(l.pool).is_some_and(liq_router::Pool::is_live))
                        .count();
                    let terms = snap.terms.get(&key).copied();
                    let per = snap.per_eth.get(&r.asset).copied();
                    let band = match (terms.as_ref(), per) {
                        (Some(t), Some(p)) => format!(
                            "{:?}",
                            liq_bot::bands::compute_one(
                                key,
                                t,
                                p,
                                snap.base_fee,
                                snap.priority_fee,
                                snap.block,
                                book,
                                routes,
                                &liq_router::SolveBudget::default(),
                            )
                        ),
                        _ => "not computable (terms or price missing)".into(),
                    };
                    out.push(format!(
                        "pair {} -> {}: {} exit legs in the book ({} live); terms {:?}; per_eth {:?}; base fee {} block {}; band {}",
                        s.asset.0,
                        r.asset.0,
                        legs.len(),
                        live,
                        terms,
                        per,
                        snap.base_fee,
                        snap.block,
                        band
                    ));
                    out.extend(compare_routing(book, s.asset, r.asset, s.max_seize, &snap));
                }
            }
            for r in &q.repay_options {
                out.push(format!(
                    "repay asset {} up to {}: flash depth {} over {} sources",
                    r.asset.0,
                    r.max_repay,
                    flash.available(r.asset),
                    flash.entries(r.asset).len()
                ));
            }
        }
        Ok(None) => out.push("adapter quote: not liquidatable".into()),
        Err(e) => out.push(format!("adapter quote failed: {e}")),
    }
    out
}

/// Their transaction on the pre-state, valued for its sender and target.
/// The winning liquidation alone, on the state before it: what it needed
/// of its own block, and what it captured and paid the builder.
pub fn winner(
    up: &Arc<Upstream>,
    block: u64,
    hash: B256,
    index: u64,
) -> Result<(super::state::Basis, Outcome), String> {
    let (pre, basis) = PreState::establish(up, block, hash, index)?;
    Ok((basis, theirs(up, &pre, hash)?))
}

fn theirs(up: &Arc<Upstream>, pre: &PreState, hash: B256) -> Result<Outcome, String> {
    let tx = Tx::fetch(up, hash)?;
    let mut parties = vec![tx.from];
    parties.extend(tx.to);
    measure(pre, tx.env, &parties, &[], None)
}

const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
/// Aave V2's `LendingPool`.
const AAVE_V2_POOL: Address =
    alloy_primitives::address!("7d2768dE32b0b80b7a3454c06BdAc94A69DDc7A9");
const AAVE_CORE: Address = address!("87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");

/// What a liquidation event says, per protocol family.
enum Kind {
    Aave,
    Morpho {
        id: B256,
    },
    /// `collateral` is the seized cToken.
    Compound {
        collateral: Address,
    },
    Euler,
    /// `silo` is the one the liquidation named.
    Silo {
        silo: Address,
    },
}

struct Case {
    toml: &'static str,
    user: Address,
    tokens: Vec<Address>,
    kind: Kind,
}

/// The protocol's own figures for the borrower, for the health line.
enum Chain {
    Aave,
    Morpho(seed::MorphoChain),
    Compound(Address),
    /// The debt vault.
    Euler(Address),
    /// The silo the liquidation named (the debt's).
    Silo(Address),
}

fn word(data: &[u8], i: usize) -> Result<B256, String> {
    data.get(32 * i..32 * (i + 1))
        .map(B256::from_slice)
        .ok_or_else(|| "event data too short".to_string())
}

fn case_of(
    ev: &Event,
    v: &Views<'_, CacheDB<AtBlock>>,
    repo_config: &Path,
) -> Result<Result<Case, Verdict>, String> {
    let topic = |i: usize| ev.topics.get(i).copied().ok_or("event topic missing");
    Ok(Ok(match ev.family.as_str() {
        // V2 and V3 share `LiquidationCall`; the V2 pool runs on the V3
        // adapter's V2 version, from its own config.
        "aave-v3" => Case {
            toml: if ev.emitter == AAVE_V2_POOL {
                "aave-v2.toml"
            } else {
                "aave-v3.toml"
            },
            user: Address::from_word(topic(3)?),
            tokens: vec![Address::from_word(topic(1)?), Address::from_word(topic(2)?)],
            kind: Kind::Aave,
        },
        "morpho-blue" => {
            let id = topic(1)?;
            let (coll, loan) = seed::morpho_tokens(v, ev.emitter, id)?;
            Case {
                toml: "morpho-blue.toml",
                user: Address::from_word(topic(3)?),
                tokens: vec![coll, loan],
                kind: Kind::Morpho { id },
            }
        }
        "compound-v2" => {
            // A cToken's `LiquidateBorrow` has no indexed fields:
            // (liquidator, borrower, repayAmount, cTokenCollateral, seizeTokens).
            if ev.topics.len() != 1 {
                return Ok(Err(Verdict::OutOfScope(format!(
                    "{:#x} emits an indexed LiquidateBorrow: not a Compound V2 cToken",
                    ev.emitter
                ))));
            }
            let collateral = Address::from_word(word(&ev.data, 3)?);
            let under = |c: Address| seed::ctoken_underlying(v, c).unwrap_or(WETH);
            Case {
                toml: "compound-v2.toml",
                user: Address::from_word(word(&ev.data, 1)?),
                tokens: vec![under(collateral), under(ev.emitter)],
                kind: Kind::Compound { collateral },
            }
        }
        "euler-v2" => {
            // Decided from the committed config, before a bot is built: the
            // adapter follows the vaults its TOML lists.
            let toml = std::fs::read_to_string(repo_config.join("protocols/euler-v2.toml"))
                .map_err(|e| e.to_string())?;
            let vault = format!("{:x}", ev.emitter);
            if !toml.to_lowercase().contains(&vault) {
                return Ok(Err(Verdict::OutOfScope(format!(
                    "vault {:#x} is not among the bot's Euler vaults",
                    ev.emitter
                ))));
            }
            // Liquidate(liquidator, violator, collateral, repayAssets,
            // yieldBalance): the seized collateral is the first data word.
            let collateral = Address::from_word(word(&ev.data, 0)?);
            let mut tokens = vec![seed::euler_asset(v, ev.emitter)?, collateral];
            if let Ok(a) = seed::euler_asset(v, collateral) {
                tokens.push(a);
            }
            Case {
                toml: "euler-v2.toml",
                user: Address::from_word(topic(2)?),
                tokens,
                kind: Kind::Euler,
            }
        }
        "silo-v2" => {
            // The hook's LiquidationCall(liquidator, silo, borrower, ...).
            let silo = Address::from_word(topic(2)?);
            let (s0, s1) = seed::silo_pair(v, silo)?;
            Case {
                toml: "silo-v2.toml",
                user: Address::from_word(topic(3)?),
                tokens: vec![seed::euler_asset(v, s0)?, seed::euler_asset(v, s1)?],
                kind: Kind::Silo { silo },
            }
        }
        other => {
            return Ok(Err(Verdict::OutOfScope(format!(
                "family {other} has no replay"
            ))))
        }
    }))
}

/// A liquidation event replayed through our searcher.
pub fn replay(up: &Arc<Upstream>, ev: &Event, repo_config: &Path, scratch: &Path) -> Report {
    let started = Instant::now();
    let mut report = Report {
        basis: None,
        health: None,
        health_note: None,
        diag: Vec::new(),
        verdict: Verdict::Harness("not run".into()),
        theirs: None,
        upstream: 0,
        served: 0,
        unknown: Vec::new(),
        seconds: 0,
    };
    let fetched0 = up.fetched.load(Ordering::Relaxed);
    let result = (|| -> Result<Verdict, String> {
        let (pre, basis) = PreState::establish(up, ev.block, ev.tx, ev.tx_index)?;
        report.basis = Some(basis);
        report.theirs = theirs(up, &pre, ev.tx).ok();
        // Their transaction frame by frame, beside ours (`LIQ_HISTORY_TRACE`);
        // appended once the diagnosis below has filled `report.diag`.
        let their_trace: Vec<String> = if std::env::var_os("LIQ_HISTORY_TRACE").is_some() {
            match Tx::fetch(up, ev.tx) {
                Ok(tx) => {
                    let mut db = pre.db.clone();
                    match super::state::trace(&mut db, &pre.block, tx.env, true) {
                        Ok((ok, _, events)) => {
                            std::iter::once(format!("their transaction, success {ok}:"))
                                .chain(events.into_iter().map(|e| format!("    {e}")))
                                .collect()
                        }
                        Err(e) => vec![format!("their trace: {e}")],
                    }
                }
                Err(e) => vec![format!("their transaction: {e}")],
            }
        } else {
            Vec::new()
        };
        let tip = pre.parent.clone();
        let db = Arc::new(pre.db.clone());
        let case = match case_of(ev, &Views { db: &*db, at: &tip }, repo_config)? {
            Ok(c) => c,
            Err(v) => return Ok(v),
        };
        let user = case.user;
        let server = Server::start(Served {
            up: Arc::clone(up),
            db: Arc::clone(&db),
            parent: tip.clone(),
            overrides: pre.overrides.to_json(),
            unknown: Default::default(),
            requests: Default::default(),
        })
        .map_err(|e| e.to_string())?;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let bot = rt.block_on(bot::build(BotSpec {
            repo_config,
            protocol_toml: case.toml,
            focus: ev.emitter,
            rpc_url: &server.url,
            tip: tip.number,
            tokens: &case.tokens,
            state: Arc::new(PreFactory(Arc::clone(&db))),
            operator: OPERATOR,
            backrun_operator: BACKRUN,
            profit_sink: SINK,
            scratch,
        }))?;
        // Seed the market as events through the bot's ingest. Flash sources
        // get nothing from the harness: they hold what the bot's own startup
        // gives them.
        let views = Views { db: &*db, at: &tip };
        let mut synth = Synth::new(&tip);
        // A Silo pair whose totals are written from the chain after ingest.
        let mut silo_pair: Option<&liq_adapters_silo_v2::config::PairConfig> = None;
        let (proto, market, chain): (&dyn liq_protocol::Protocol, liq_types::MarketId, Chain) =
            match &case.kind {
                Kind::Aave => {
                    let Some((proto, pool_cfg)) = bot.adapters.iter().find_map(|b| match b {
                        BoundProtocol::AaveV3(p) => p
                            .config()
                            .pools
                            .iter()
                            .find(|c| c.address == ev.emitter)
                            .map(|c| (b.as_dyn(), c.clone())),
                        _ => None,
                    }) else {
                        return Ok(Verdict::OutOfScope(format!(
                            "pool {:#x} is not bound",
                            ev.emitter
                        )));
                    };
                    let v2 = bot.adapters.iter().any(|b| match b {
                        BoundProtocol::AaveV3(p) => {
                            p.config().liquidation.version == liq_adapters_aave_v3::AaveVersion::V2
                                && p.config().pools.iter().any(|c| c.address == ev.emitter)
                        }
                        _ => false,
                    });
                    if v2 {
                        let configured: Vec<Address> = bot
                            .adapters
                            .iter()
                            .find_map(|b| match b {
                                BoundProtocol::AaveV3(p)
                                    if p.config().pools.iter().any(|c| c.address == ev.emitter) =>
                                {
                                    Some(p.config().assets.iter().map(|a| a.underlying).collect())
                                }
                                _ => None,
                            })
                            .unwrap_or_default();
                        seed::aave_v2(
                            &views,
                            pool_cfg.address,
                            pool_cfg.configurator,
                            pool_cfg.grace_sentinel,
                            &configured,
                            &[user],
                            &mut synth,
                        )?;
                    } else {
                        seed::aave_v3(
                            &views,
                            pool_cfg.address,
                            pool_cfg.configurator,
                            &[user],
                            &mut synth,
                        )?;
                    }
                    (proto, pool_cfg.market, Chain::Aave)
                }
                Kind::Morpho { id } => {
                    let Some((proto, first)) = bot.adapters.iter().find_map(|b| match b {
                        BoundProtocol::MorphoBlue(p) if p.config().morpho == ev.emitter => {
                            Some((b.as_dyn(), p.config().first_market))
                        }
                        _ => None,
                    }) else {
                        return Ok(Verdict::OutOfScope(format!(
                            "singleton {:#x} is not bound",
                            ev.emitter
                        )));
                    };
                    let c = seed::morpho(&views, ev.emitter, *id, user, &mut synth)?;
                    (proto, first, Chain::Morpho(c))
                }
                Kind::Compound { collateral } => {
                    let Some((proto, comptroller, market)) =
                        bot.adapters.iter().find_map(|b| match b {
                            BoundProtocol::CompoundV2(p) => p.config().forks.iter().find_map(|f| {
                                let has = |c: Address| f.ctokens.iter().any(|t| t.ctoken == c);
                                (has(ev.emitter) && has(*collateral))
                                    .then(|| p.config().interned_id(f.comptroller))
                                    .flatten()
                                    .map(|m| (b.as_dyn(), f.comptroller, m))
                            }),
                            _ => None,
                        })
                    else {
                        return Ok(Verdict::OutOfScope(format!(
                            "cToken {:#x} (collateral {collateral:#x}) is not in a bound Comptroller",
                            ev.emitter
                        )));
                    };
                    seed::compound(&views, comptroller, user, &mut synth)?;
                    (proto, market, Chain::Compound(comptroller))
                }
                Kind::Euler => {
                    let Some((proto, cfg)) = bot.adapters.iter().find_map(|b| match b {
                        BoundProtocol::EulerV2(p) if p.config().vaults.contains(&ev.emitter) => {
                            Some((b.as_dyn(), p.config()))
                        }
                        _ => None,
                    }) else {
                        return Ok(Verdict::OutOfScope(format!(
                            "vault {:#x} is not among the bot's Euler vaults",
                            ev.emitter
                        )));
                    };
                    let Some(market) = cfg.interned_id(ev.emitter) else {
                        return Ok(Verdict::OutOfScope(format!(
                            "vault {:#x} has no registry market id",
                            ev.emitter
                        )));
                    };
                    seed::euler(&views, cfg.factory, cfg.evc, ev.emitter, user, &mut synth)?;
                    (proto, market, Chain::Euler(ev.emitter))
                }
                Kind::Silo { silo } => {
                    let Some((proto, factory, pair)) = bot.adapters.iter().find_map(|b| match b {
                        BoundProtocol::SiloV2(p) => p
                            .config()
                            .pairs
                            .iter()
                            .find(|x| x.hook_receiver == ev.emitter)
                            .and_then(|x| Some((b.as_dyn(), *p.config().factories.first()?, x))),
                        _ => None,
                    }) else {
                        return Ok(Verdict::OutOfScope(format!(
                            "hook {:#x} is not a bound Silo pair's",
                            ev.emitter
                        )));
                    };
                    seed::silo(&views, factory, pair, user, &mut synth)?;
                    silo_pair = Some(pair);
                    (proto, pair.market, Chain::Silo(*silo))
                }
            };
        // The other families price through Aave's oracle too (the drain's
        // USD getter), so the Core pool's reserves are seeded beside them.
        if !matches!(chain, Chain::Aave) {
            for b in bot.adapters {
                if let BoundProtocol::AaveV3(p) = b {
                    if let Some(c) = p.config().pools.iter().find(|c| c.address == AAVE_CORE) {
                        seed::aave_v3(&views, c.address, c.configurator, &[], &mut synth)?;
                    }
                }
            }
        }
        let mut store = store_at(tip.number);
        let block = synth.block(&tip);
        let dirty = seed::ingest(&mut store, bot.adapters, bot.index, bot.shared.risk, &block)?;
        if let Some(pair) = silo_pair {
            seed::silo_totals(&views, &mut store, pair)?;
        }
        let key = PositionKey {
            protocol: proto.id(),
            market,
            user,
        };
        if store.position_id(&key).is_none() {
            return Ok(Verdict::OutOfScope(
                "the adapter did not admit the market (its tokens or oracle are not in the bot's config)"
                    .into(),
            ));
        }

        // The drain at the tip publishes its read sets; its own readers
        // answer them on the pre-state, and the drain folds the state batch
        // (Aave's reserve ids) as the hot thread does every spin.
        let mut hook = bot.hook;
        hook.after_block(ctx(&store, &dirty, &tip));
        let at_tip = |b: Option<u64>| b.is_some_and(|b| b >= tip.number);
        // Morpho publishes no state reads, so its state reader never has a
        // batch to answer. Compound reads each cToken's borrow rate.
        let reads_state = matches!(chain, Chain::Aave | Chain::Compound(_) | Chain::Silo(_));
        // Euler's resync reads are answered by the harness itself (below).
        let waited = wait_for(Duration::from_secs(180), || {
            (!reads_state
                || at_tip(
                    bot.state_reader
                        .latest
                        .load()
                        .as_ref()
                        .as_ref()
                        .map(|b| b.block),
                ))
                && at_tip(
                    bot.price_reader
                        .latest
                        .load()
                        .as_ref()
                        .as_ref()
                        .map(|b| b.block),
                )
        });
        if !waited {
            return Err("the bot's readers did not answer for the tip".into());
        }
        let mut acc = liq_node::DirtyAccumulator::new();
        hook.amend(&mut store, &mut acc);

        // The borrower, through the adapter's resync reads (the stand-in for
        // the backfill that builds accounts in production).
        // Morpho and Compound have no resync reads: their events are the state.
        if matches!(chain, Chain::Aave | Chain::Euler(_)) {
            seed::resync(&views, proto, &mut store, &key)?;
        }
        let id = store.position_id(&key).ok_or("borrower not interned")?;
        let px = protocol_px(&bot.price_reader, &key, &tip)?;
        match &chain {
            Chain::Aave => report.health = Some(health_check(&views, proto, &store, id, &px)?),
            other => report.health_note = Some(health_note(&views, proto, &store, id, &px, other)),
        }

        let mut again = dirty.clone();
        again.positions.push(id);
        hook.after_block(ctx(&store, &again, &tip));
        // Searcher time (`LIQ_HISTORY_TIMING`): the block that made the job,
        // then the same block again with every state read the simulation
        // makes already in memory, as the node's own state is in production;
        // the warm pass is the reading. Its job is dropped.
        let cold = hook.last_timing();
        let timing = if std::env::var_os("LIQ_HISTORY_TIMING").is_some() && cold.candidates > 0 {
            hook.after_block(ctx(&store, &again, &tip));
            let warm = hook.last_timing();
            let warm_job = bot.jobs.recv_timeout(Duration::from_secs(5)).ok();
            Some((cold, warm, warm_job))
        } else {
            None
        };
        report.diag = diagnose(
            &hook,
            &bot.shared.flash.load(),
            &bot.shared.routes.table(),
            &bot.bands,
            &bot.index.book.read(),
            proto,
            &store,
            id,
            &px,
            // As the bot quotes: at the next slot, when its job would land.
            tip.timestamp + 12,
        );
        report.diag.extend(their_trace);
        bot.stop.store(true, Ordering::Relaxed);
        report.served = server.served.requests.load(Ordering::Relaxed);
        report.unknown = server.unknown_methods();

        if let Some((cold, warm, warm_job)) = &timing {
            report.diag.push(timing_line("cold", cold));
            report.diag.push(timing_line("warm", warm));
            if let Some(j) = warm_job {
                report.diag.push(sign_timing(j));
            }
        }
        let Ok(job) = bot.jobs.recv_timeout(Duration::from_secs(5)) else {
            let last = bot.simulated.lock().ok().and_then(|s| s.last().cloned());
            if let Some(b) = last {
                report.diag.extend(trace_bundle(&pre, &b, bot.executor));
            }
            return Ok(Verdict::NoJob);
        };
        let code =
            liq_sim::executor_stack(&liq_sim::ExecutorSpec::mainnet(OPERATOR, BACKRUN, SINK))
                .map_err(|e| e.to_string())?
                .placements(liq_sim::PLANNED_EXECUTOR)
                .to_vec();
        let env = TxEnv::builder()
            .caller(OPERATOR)
            .kind(TxKind::Call(liq_sim::PLANNED_EXECUTOR))
            .data(job.calldata.clone())
            .gas_limit(job.gas_limit)
            .gas_price(u128::from(pre.block.base_fee) + job.fee.priority_wei)
            .gas_priority_fee(Some(job.fee.priority_wei))
            .chain_id(Some(1))
            .build_fill();
        if std::env::var_os("LIQ_HISTORY_TRACE").is_some() {
            report.diag.extend(trace_job(&pre, env.clone(), &code));
        }
        let ours = measure(
            &pre,
            env,
            &[OPERATOR, liq_sim::PLANNED_EXECUTOR, SINK],
            &code,
            Some(OPERATOR),
        )?;
        if let Ok(plan) = liq_plan::decode_batch(&job.plan) {
            report.diag.push(format!(
                "plan: bid {} bps, gas cost {} wei (≈ {} gas at {} wei/gas), min profit {} wei, {} group(s), {} repay swap(s), {} profit swap(s); priority {} wei/gas; ran {} gas",
                plan.bid_bps,
                plan.gas_cost_wei,
                plan.gas_cost_wei / (u128::from(pre.block.base_fee) + job.fee.priority_wei).max(1),
                u128::from(pre.block.base_fee) + job.fee.priority_wei,
                plan.min_profit_wei,
                plan.groups.len(),
                plan.groups.iter().map(|g| g.repay_swaps.len()).sum::<usize>(),
                plan.profit_swaps.len(),
                job.fee.priority_wei,
                ours.gas_used
            ));
            for g in &plan.groups {
                report.diag.push(format!(
                    "  flash {:?} {:#x}: {} of {:#x}, fee {} bps",
                    g.provider, g.flash_source, g.flash_amount, g.debt_asset, g.fee_bps
                ));
                for l in &g.liqs {
                    report.diag.push(format!(
                        "  liquidate {:#x}: repay {}, collateral {:#x}, pull {}",
                        l.borrower, l.repay_amount, l.collateral_asset, l.protocol_pull
                    ));
                }
                for s in &g.repay_swaps {
                    report.diag.push(format!("  repay swap {}", swap_line(s)));
                }
            }
            for s in &plan.profit_swaps {
                report.diag.push(format!("  profit swap {}", swap_line(s)));
            }
        }
        Ok(Verdict::Job {
            ours,
            gas_limit: job.gas_limit,
            bid_bps: job.auction_bps,
        })
    })();
    report.verdict = result.unwrap_or_else(Verdict::Harness);
    report.upstream = up.fetched.load(Ordering::Relaxed) - fetched0;
    report.seconds = started.elapsed().as_secs();
    report
}

/// One [`liq_bot::drain::BlockTiming`] as a diagnosis line, in microseconds.
fn timing_line(pass: &str, t: &liq_bot::drain::BlockTiming) -> String {
    let us = |d: std::time::Duration| d.as_micros();
    format!(
        "  timing ({pass}): total {} us = setup {} + engine {} + prepare {} + select {} + assemble {} + sim {} + handoff {} (+ the rest {}); {} candidates, {} jobs",
        us(t.total),
        us(t.setup),
        us(t.engine),
        us(t.prepare),
        us(t.select),
        us(t.assemble),
        us(t.sim),
        us(t.handoff),
        us(t.total
            .saturating_sub(t.setup + t.engine + t.prepare + t.select + t.assemble + t.sim + t.handoff)),
        t.candidates,
        t.jobs,
    )
}

/// Signing the job as the exec thread does (`liq_exec::template::sign_call`,
/// a precomputed secp256k1 key), timed: the median of 1,000 signatures, so a
/// cold first one does not stand for it.
fn sign_timing(job: &liq_exec::path::ExecJob) -> String {
    use liq_exec::template::{sign_call, CallSpec, PrecomputedSigner};
    // Anvil's first key: a test signer, not an operator.
    let signer = match PrecomputedSigner::from_secret(alloy_primitives::b256!(
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
    )) {
        Ok(s) => s,
        Err(e) => return format!("  timing (sign): no signer ({e})"),
    };
    let mut ns: Vec<u128> = Vec::with_capacity(1_000);
    for nonce in 0..1_000u64 {
        let t = Instant::now();
        // As the exec path does: bind the job's fees, then sign.
        let bound = match job.fee.bind(job.target_block, job.max_block) {
            Ok(b) => b,
            Err(e) => return format!("  timing (sign): fees did not bind ({e})"),
        };
        let r = sign_call(
            &signer,
            CallSpec {
                chain_id: 1,
                nonce,
                to: liq_sim::PLANNED_EXECUTOR,
                input: job.calldata.clone(),
                gas_limit: job.gas_limit,
                fees: &bound,
                priority: job.fee.priority_wei,
            },
        );
        ns.push(t.elapsed().as_nanos());
        if let Err(e) = r {
            return format!("  timing (sign): failed ({e})");
        }
    }
    ns.sort_unstable();
    format!(
        "  timing (sign): median {} us, worst {} us over 1,000 signatures of the job's calldata",
        ns[ns.len() / 2] / 1_000,
        ns[ns.len() - 1] / 1_000
    )
}
