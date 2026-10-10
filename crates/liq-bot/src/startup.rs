//! Startup order (GUIDE 00 / GUIDE 17): fail closed, no skip.
//!
//! 1. registry assert (00D)
//! 2. config `Validate`
//! 3. `Shared` built and leaked (`Box::leak` — never `LazyLock`)
//! 4. threads pinned and asserted (`allow_unpinned: false` in prod)
//! 5. ExEx registered

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use arc_swap::ArcSwap;
use liq_config::rpc::ChainRpc;
use liq_config::{boot, BotConfig, Loaded};
use liq_exec::path::{ExecInbox, ExecPath};
use liq_exec::submit::SubmitEnabled;
use liq_flash::FlashIndex;
use liq_node::LogRouter;
use liq_obs::{RttMonitor, ShadowRecorder};
use liq_risk::RiskGate;
use liq_router::WarmRouteCache;
use liq_state::StoreSnapshot;
use thiserror::Error;

use crate::assemble_view::ProcessAssembleView;
use crate::bind;
use crate::drain::DrainJoin;
use crate::exec_bind::{bind_from_config, ProcessSecrets};
use crate::exec_worker::spawn_exec_worker;
use crate::exex_install::{install_hot, prepare};
use crate::lease::{acquire, recover_capacity, StatePaths, SubmitLease};
use crate::routes::{spawn_warm_thread, warm_handles};
use crate::shared::{leak_lease, leak_risk, leak_state_shared, Shared, PROD_ALLOW_UNPINNED};
use crate::threads::{configure_hot_pin, CoreMap, ThreadError};

#[derive(Debug, Error)]
pub enum StartupError {
    #[error("startup config: {0}")]
    Config(#[from] liq_config::ConfigError),
    #[error("startup threads: {0}")]
    Threads(#[from] ThreadError),
    #[error("startup lease: {0}")]
    Lease(#[from] crate::lease::LeaseError),
    #[error("startup ingest: {0}")]
    Ingest(#[from] liq_node::IngestError),
    #[error("startup: {0}")]
    Other(String),
    /// The snapshot was built by a different set of adapter bindings.
    /// The caller discards its head and rebuilds.
    #[error("snapshot bindings {snapshot} differ from the bound adapters {bound}")]
    StaleSnapshot {
        snapshot: alloy_primitives::B256,
        bound: alloy_primitives::B256,
    },
}

/// Recorded step for tests. Production runs them in this order only.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StartupStep {
    RegistryAssert,
    ConfigValidate,
    SharedLeak,
    ThreadsPinned,
    ExExRegistered,
}

/// Production allow-unpinned is false. Tests may override.
#[must_use]
pub const fn prod_allow_unpinned() -> bool {
    PROD_ALLOW_UNPINNED
}

/// Steps 1–2: `liq_config::boot` (registry assert + Validate). Empty RPC fails.
pub async fn boot_assert(config_dir: &Path) -> Result<Loaded, StartupError> {
    let loaded = boot(config_dir).await?;
    tracing::info!(
        submit_enabled = loaded.config.submit_enabled,
        "live send also needs the lease and a chain-nonce resync"
    );
    Ok(loaded)
}

/// Step 3: leak Shared. `submit_enabled` from config.
pub fn leak_process_shared(cfg: &BotConfig, lease: SubmitLease) -> &'static Shared {
    let (_builder, routes) = warm_handles();
    leak_shared(cfg, lease, routes, 0)
}

/// Leak Shared with a caller-owned warm slot (the builder thread keeps the other handle).
pub fn leak_shared(
    cfg: &BotConfig,
    lease: SubmitLease,
    routes: WarmRouteCache,
    flash_assets: usize,
) -> &'static Shared {
    let flag = Arc::new(SubmitEnabled::new(cfg.submit_enabled));
    Shared::leak(
        leak_state_shared(),
        routes,
        Arc::new(ArcSwap::from_pointee(FlashIndex::new(flash_assets))),
        flag,
        leak_risk(),
        leak_lease(lease),
    )
}

/// Step 4: load cores, configure hot pin. Prod: `allow_unpinned == false`.
pub fn pin_threads(cores_path: &Path, allow_unpinned: bool) -> Result<CoreMap, StartupError> {
    let map = CoreMap::load(cores_path)?;
    let hot = map.slot("liq-node-hot")?;
    configure_hot_pin(hot.core);
    if !allow_unpinned {
        #[cfg(target_os = "linux")]
        map.assert_live_l3()?;
        tracing::info!(core = hot.core, "hot pin configured; allow_unpinned=false");
    }
    Ok(map)
}

/// Live process after the five startup steps plus 17C process joins.
pub struct Started {
    /// The block the restored store is at (the snapshot's head record).
    pub head: crate::state_build::SnapshotHead,
    pub shared: &'static Shared,
    pub hot: liq_node::HotHandle,
    pub forwarder: liq_node::ExExForwarder,
    /// Union-filter addresses, rebuilt when subscriptions change at
    /// runtime. The ExEx copies only these logs.
    pub tracked: Arc<liq_node::Resubscribe>,
    /// 13A path. `None` when secrets/builders are missing — not an invented signer.
    pub exec: Option<Arc<ExecPath<ShadowRecorder, &'static RiskGate>>>,
    pub assemble: ProcessAssembleView,
    /// Inclusion watcher threads. `None` when the feed could not be built.
    pub inclusion: Option<crate::inclusion_feed::InclusionJoin>,
    _stall: Option<std::thread::JoinHandle<()>>,
    _svr: Option<std::thread::JoinHandle<()>>,
    _gov: Option<std::thread::JoinHandle<()>>,
}

/// Step 5: split ExEx rings and spawn hot (pin asserted inside).
/// `adapters` is the leaked load — same objects as drain.
/// `index` is flash / book / feeds on the same router; protocol ids stay adapters.
pub fn register_exex(
    store: liq_state::StateStore,
    sink: &'static dyn liq_types::HaltSink,
    adapters: &'static [bind::BoundProtocol],
    index: &'static crate::index::BoundIndex,
    allow_unpinned: bool,
    after_block: Option<Box<dyn liq_node::AfterBlock>>,
) -> Result<
    (
        liq_node::ExExForwarder,
        liq_node::HotHandle,
        Arc<liq_node::Resubscribe>,
    ),
    StartupError,
> {
    let inst = prepare();
    if adapters.is_empty() {
        tracing::error!("empty protocol list — ExEx protocol ids empty");
    }
    if adapters.is_empty() && index.subscriber_empty() {
        tracing::error!("empty protocol list — ingest subscribers empty");
    }
    let subs = bind::router_subscribers(adapters, index);
    let router = LogRouter::from_subscribers(&subs).map_err(StartupError::Ingest)?;
    let tracked = Arc::clone(&index.resubscribe);
    tracked.publish(router.tracked_addresses().clone(), tracked.requested());
    let handle = install_hot(
        store,
        router,
        bind::router_handlers(adapters, index, sink),
        inst.ingress,
        sink,
        bind::protocol_ids(adapters),
        Arc::clone(&inst.height),
        allow_unpinned,
        after_block,
        Some(Arc::clone(&tracked)),
    )?;
    Ok((inst.forwarder, handle, tracked))
}

/// `PROFIT_SINK` is the Executor's sink. Unset or zero leaves inclusion
/// profit unresolved — a successful receipt is not marked Included.
fn profit_sink_from_env() -> Option<alloy_primitives::Address> {
    let raw = match std::env::var("PROFIT_SINK") {
        Ok(v) => v,
        Err(_) => return None,
    };
    if raw.is_empty() {
        return None;
    }
    match raw.parse::<alloy_primitives::Address>() {
        Ok(a) if !a.is_zero() => Some(a),
        Ok(_) => {
            tracing::error!("PROFIT_SINK is zero — inclusion profit stays unresolved");
            None
        }
        Err(e) => {
            tracing::error!(error = %e, "PROFIT_SINK unreadable — inclusion profit stays unresolved");
            None
        }
    }
}

/// Bring a freshly loaded index to chain state before it is leaked and
/// routed: canonical prices, the book's pools, then the flash sources.
/// Everything here is pinned to the head each seed reads; logs keep it
/// current afterwards. The historical replay test seeds through this too.
pub async fn seed_index(load: &mut crate::index::IndexLoad, rpc_url: &str) {
    seed_canonical(load, rpc_url).await;
    match liq_config::rpc::HttpRpc::connect(rpc_url) {
        Ok(rpc) => {
            crate::pool_seed::seed_v3(&mut load.book, &rpc).await;
            crate::pool_seed::seed_v4(&mut load.book, &rpc).await;
            crate::pool_seed::seed_v2(&mut load.book, &rpc).await;
            crate::pool_seed::seed_curve(&mut load.book, &rpc).await;
            crate::flash_seed::seed_flash(&mut load.sources, &rpc).await;
        }
        Err(e) => tracing::error!(error = %e, "pool and flash seed skipped — RPC connect failed"),
    }
}

/// Read every configured aggregator's `latestRoundData` so the engine starts
/// with prices. Without this each slot stays empty until that feed's next
/// `AnswerUpdated`, which on a heartbeat-only feed can be hours.
async fn seed_canonical(load: &mut crate::index::IndexLoad, rpc_url: &str) {
    let Some(book) = load.canonical.as_mut() else {
        return;
    };
    let rpc = match liq_config::rpc::HttpRpc::connect(rpc_url) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "price seed skipped — RPC connect failed");
            return;
        }
    };
    match book.seed(&rpc).await {
        Ok(()) => tracing::info!("canonical prices seeded from latestRoundData"),
        Err(e) => tracing::error!(
            error = %e,
            "price seed failed — slots fill only as each feed next updates"
        ),
    }
}

/// `PNL_LEDGER_PATH` is the operational PnL ledger's SQLite file. Unset
/// leaves resolved outcomes logged (tracing) but not persisted to disk.
fn ledger_path_from_env() -> Option<String> {
    match std::env::var("PNL_LEDGER_PATH") {
        Ok(v) if !v.is_empty() => Some(v),
        Ok(_) => None,
        Err(_) => None,
    }
}

/// [`run`] on a state that exists and matches the bound adapters. No
/// snapshot yet: build it first. A snapshot from different bindings
/// ([`StartupError::StaleSnapshot`]): discard its head, rebuild, start again.
/// Nothing plans or sends while a build runs.
pub async fn run_on_built_state(
    config_dir: &Path,
    cores_path: &Path,
    state: &StatePaths,
    allow_unpinned: bool,
    node_state: Option<Arc<dyn liq_sim::StateProviderFactory>>,
) -> Result<Started, StartupError> {
    let build = || async {
        crate::state_build::build_first_snapshot(config_dir, state)
            .await
            .map_err(|e| StartupError::Other(format!("state build: {e}")))
    };
    if !crate::state_build::head_path(state).exists() {
        tracing::info!("no state snapshot — building it from the node's receipts first");
        build().await?;
    }
    match run(
        config_dir,
        cores_path,
        state,
        allow_unpinned,
        node_state.clone(),
    )
    .await
    {
        Err(StartupError::StaleSnapshot { .. }) => {
            tracing::warn!("rebuilding the state for the new adapter bindings");
            crate::state_build::discard_head(state)
                .map_err(|e| StartupError::Other(e.to_string()))?;
            build().await?;
            run(config_dir, cores_path, state, allow_unpinned, node_state).await
        }
        other => other,
    }
}

/// Full production order. RPC/registry failure refuses. Lease gates submit.
///
/// `node_state` is the node's own state when this runs inside it (the
/// ExEx): jobs are then simulated in-process on the store's tip. `None`
/// (the standalone binary): the exec worker verifies them over RPC.
pub async fn run(
    config_dir: &Path,
    cores_path: &Path,
    state: &StatePaths,
    allow_unpinned: bool,
    node_state: Option<Arc<dyn liq_sim::StateProviderFactory>>,
) -> Result<Started, StartupError> {
    let loaded = boot_assert(config_dir).await?;
    // The snapshot's head record: the block the store is at, which the
    // restored store must match, and where Reth resumes the ExEx.
    let head = crate::state_build::read_head(state).map_err(|e| {
        tracing::error!(error = %e, "no snapshot head — the state has not been built");
        StartupError::Other(e.to_string())
    })?;
    // Live RPC for the four adapters (Liquity/Fluid/Gearbox/Compound V2)
    // whose `Config::new` refuses without a live-registry assertion. Reuses
    // `loaded.config.rpc_url` — the same node `boot()` already asserted the
    // token registry against. A connect/block-number failure here omits
    // those four (named reason), and any omission refuses the start below.
    let live_rpc = match liq_config::rpc::HttpRpc::connect(&loaded.config.rpc_url) {
        Ok(rpc) => match rpc.block_number().await {
            Ok(block) => Some((crate::live_rpc::LiveRpc::new(rpc), block)),
            Err(e) => {
                tracing::error!(error = %e, "live RPC block number unavailable — Liquity/Fluid/Gearbox/Compound omitted");
                None
            }
        },
        Err(e) => {
            tracing::error!(error = %e, "live RPC connect failed — Liquity/Fluid/Gearbox/Compound omitted");
            None
        }
    };
    let live = live_rpc.as_ref().map(|(rpc, block)| (rpc, *block));
    let mut loaded_proto = bind::load_protocols(config_dir, &loaded.intern, live);
    bind::retry_live_omitted(config_dir, &loaded.intern, live, &mut loaded_proto, 3);
    // The snapshot holds every adapter's positions. Running without one would
    // advance the snapshot past blocks whose events for it were never folded,
    // and a later start would plan on those gaps. Refuse instead.
    if !loaded_proto.omitted.is_empty() {
        let names: Vec<String> = loaded_proto
            .omitted
            .iter()
            .map(|(n, why)| format!("{n}: {why}"))
            .collect();
        tracing::error!(omitted = ?names, "adapter bind failed — refusing to start on partial state");
        return Err(StartupError::Other(format!(
            "adapter(s) omitted at bind: {}",
            names.join("; ")
        )));
    }
    let adapters = bind::leak_protocols(loaded_proto);
    // The snapshot was folded by adapters bound to a fingerprinted set of
    // contracts. A different set (a new Gearbox manager, a new adapter, a
    // changed event list) means moved MarketIds or contracts with no history
    // in the snapshot: refuse it before anything is spawned.
    let bindings = crate::state_build::bindings_fingerprint(adapters);
    if bindings != head.bindings {
        tracing::error!(
            snapshot = %head.bindings,
            bound = %bindings,
            "adapter bindings changed since the snapshot — it must be rebuilt"
        );
        return Err(StartupError::StaleSnapshot {
            snapshot: head.bindings,
            bound: bindings,
        });
    }
    let probe = crate::lease::HeadProbe {
        number: head.number,
    };
    let (store, lease) = match acquire(state, recover_capacity(), Some(&probe)) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(?e, "startup lease refused — process will not submit");
            return Err(e.into());
        }
    };
    let (warm_builder, routes) = warm_handles();
    let shared = leak_shared(
        &loaded.config,
        lease,
        routes,
        loaded.intern.asset_id_capacity(),
    );
    let mut index_load = crate::index::load_index(config_dir, &loaded.intern, &loaded.registry);
    seed_index(&mut index_load, &loaded.config.rpc_url).await;
    crate::graph_build::attach(
        &mut index_load,
        &loaded.intern,
        &loaded.config.rpc_url,
        config_dir,
    )
    .await;
    let index = crate::index::leak_index(index_load);
    if let Err(e) = crate::pool_seed::spawn_curve_reseed(
        Arc::clone(&index.book),
        loaded.config.rpc_url.clone(),
        Arc::new(AtomicBool::new(false)),
    ) {
        tracing::error!(
            ?e,
            "curve reseed thread not started — traded Curve pools stay unrouted"
        );
    }
    // The registry is read once, here. No exit joins the running book from
    // the file (decision 2026-10-07): the daily refresh proposes pools and
    // unwraps for review (`data/review/<date>-candidates.json`), a person
    // admits the ones they checked (`tools/registry/admit_reviewed.py`), and
    // a restart takes the batch. `registry_watch` is not started.
    let engine_positions = store.len().saturating_mul(2);
    let band_shared = crate::bands::BandShared::new();
    let stop_warm = Arc::new(AtomicBool::new(false));
    if let Err(e) = spawn_warm_thread(
        warm_builder,
        Arc::clone(&stop_warm),
        Arc::clone(&index.book),
        Some(Arc::clone(&band_shared)),
    ) {
        tracing::error!(
            ?e,
            "warm-builder thread not started — empty cache stays empty"
        );
    }
    let _ = stop_warm;
    spawn_rtt_tick(&config_dir.join("builders.toml"));
    let shadow = state
        .snapshot
        .parent()
        .map(|p| p.join("shadow"))
        .unwrap_or_else(|| {
            tracing::error!("snapshot path has no parent — shadow dir refused");
            std::path::PathBuf::from("shadow-refused")
        });
    let exec = bind_from_config(
        &config_dir.join("builders.toml"),
        &shadow,
        shared,
        ProcessSecrets::from_env(),
    );
    // The prewarm and the re-warm loop run on the exec worker's runtime
    // (`exec_worker::spawn_exec_worker`), so the warm sockets belong to the
    // pinned submit thread and not to this runtime.
    if exec.is_none() {
        tracing::error!("ExecPath unbound — ingest/lease continue; no invented key");
    }
    let mut assemble = bind::intern_view(&loaded.intern).with_bands(band_shared);
    bind::intern_adapter_tokens(&mut assemble, adapters);
    let gas_model = crate::gas_model::GasModel::load(&config_dir.join("liq-gas.toml"));
    let wrap = gas_model.as_ref().map_or_else(
        bind::WrapGas::default,
        crate::gas_model::GasModel::select_wrap,
    );
    let weth = bind::registry_weth(&loaded.intern).unwrap_or_else(|| {
        tracing::error!("registry WETH missing — SelectReady stays None");
        alloy_primitives::Address::ZERO
    });
    let mut select_bind = bind::select_bind(wrap, weth, loaded.intern.protocol("aave-v4"));
    if let Some(sb) = select_bind.as_mut() {
        for pin in bind::compound_validate_pins(adapters) {
            sb.validate.add_compound(pin);
        }
    }
    let bid_cfg = bind::load_bid_config(&config_dir.join("bid.toml"), &loaded.intern);
    let oracle = liq_router::GasOracle::with_priority_cap(liq_router::gas::DEFAULT_PRIORITY_CAP);
    let fee = match oracle.as_ref() {
        Some(o) => bind::fee_from_oracle(o, 0),
        None => {
            tracing::error!("gas oracle cap refused — fee stays None");
            None
        }
    };
    if fee.is_none() {
        tracing::error!("fee window empty — fee stays None (no invented base/priority)");
    }
    let inclusion = if exec.is_some() {
        crate::inclusion_feed::start(
            &loaded.config.rpc_url,
            &loaded.registry,
            loaded.intern.clone(),
            profit_sink_from_env(),
            ledger_path_from_env().as_deref(),
        )
    } else {
        tracing::error!("inclusion feed not started — ExecPath unbound");
        None
    };
    let exec = exec.map(|p| match loaded.config.venues.executor {
        Some(addr) => p.with_executor(addr),
        None => p,
    });
    let exec = match (exec, inclusion.as_ref()) {
        (Some(path), Some(feed)) => Some(Arc::new(path.with_watch(feed.cmd_tx.clone()))),
        (Some(path), None) => {
            tracing::error!("inclusion feed absent — submissions are not tracked");
            Some(Arc::new(path))
        }
        (None, _) => None,
    };
    let operator = exec
        .as_ref()
        .and_then(|p| p.signers.first().map(|s| s.address()));
    let backrun_operator = exec.as_ref().and_then(|p| {
        p.signers
            .get(crate::exec_bind::BACKRUN_SLOT)
            .map(|s| s.address())
    });
    let sim = live_sim(
        node_state,
        loaded.config.venues.executor,
        operator,
        backrun_operator,
        shared.lease,
    );
    let (inbox, rx) = if exec.is_some() {
        let (tx, rx) = ExecInbox::pair(1024);
        (Some(tx), Some(rx))
    } else {
        tracing::error!("ExecPath unbound — drain try_send fails closed; no exec worker");
        (None, None)
    };
    // Governance liquidations: bundles go out through the same exec worker,
    // simulated and nonced against the chain at the base block.
    let (gov_inbox, chain_exec) = if exec.is_some() {
        match liq_exec::chain::ChainClient::new(&loaded.config.rpc_url) {
            Ok(chain) => {
                let (tx, grx) = liq_exec::gov::GovInbox::pair(64);
                (
                    Some(tx),
                    Some(crate::exec_worker::ChainExec {
                        chain,
                        gov_rx: Some(grx),
                        deployed: loaded.config.venues.executor,
                        profit_sink: profit_sink_from_env(),
                    }),
                )
            }
            Err(e) => {
                tracing::error!(error = %e, "chain client refused — no nonce resync, no RPC verification, no governance");
                (None, None)
            }
        }
    } else {
        (None, None)
    };
    if let (Some(path), Some(rx)) = (exec.clone(), rx) {
        // The submit thread's core. Read here because the full map is
        // asserted later (`pin_threads`); a missing slot runs it unpinned.
        let exec_core = match CoreMap::load(cores_path)
            .and_then(|m| m.slot(crate::exec_worker::EXEC_THREAD).map(|s| s.core))
        {
            Ok(core) => Some(core),
            Err(e) => {
                tracing::error!(?e, "no liq-exec-submit core — submit thread unpinned");
                None
            }
        };
        if let Err(e) = spawn_exec_worker(rx, chain_exec, path, exec_core) {
            tracing::error!(?e, "exec worker not started — inbox will count full");
        }
    }
    let (gov, gov_thread) = match gov_inbox {
        Some(inbox) => {
            let (tx, grx) =
                rtrb::RingBuffer::<crate::governance::GovSim>::new(crate::governance::GOV_RING);
            match crate::governance::spawn_gov_worker(
                loaded.registry.clone(),
                loaded.intern.clone(),
                loaded.config.rpc_url.clone(),
                tx,
                Arc::new(AtomicBool::new(false)),
            ) {
                Ok(h) => (Some((grx, inbox)), Some(h)),
                Err(e) => {
                    tracing::error!(?e, "governance worker not started");
                    (None, None)
                }
            }
        }
        None => (None, None),
    };
    let clock = Arc::new(crate::stall::HeaderClock::new());
    let stall = match crate::stall::spawn(Arc::clone(&clock), shared.risk) {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::error!(error = %e, "stall heartbeat not started");
            None
        }
    };
    // Protocol-reported prices: each adapter's own price getters, read off
    // the hot path every block (decision 2026-09-25).
    let price_reader = crate::protocol_prices::ReaderShared::new();
    if let Err(e) = crate::protocol_prices::spawn_reader(
        adapters,
        loaded.config.rpc_url.clone(),
        Arc::clone(&price_reader),
        Arc::new(AtomicBool::new(false)),
    ) {
        tracing::error!(
            ?e,
            "protocol price reader not started — health priced from canonical feeds only"
        );
    }
    // Protocol state only the protocol can answer (Fluid vaults simulate
    // their own liquidation), read every block off the hot path.
    let state_reader = crate::state_reads::StateReaderShared::new();
    if let Err(e) = crate::state_reads::spawn_state_reader(
        adapters,
        loaded.config.rpc_url.clone(),
        Arc::clone(&state_reader),
        Arc::new(AtomicBool::new(false)),
    ) {
        tracing::error!(
            ?e,
            "protocol state reader not started — Fluid vaults never quote"
        );
    }
    let svr = start_svr_reader(index, shared.risk);
    let hook = DrainJoin::live(
        Arc::clone(&shared.flash),
        shared.routes.clone(),
        assemble.clone(),
        inbox,
        operator,
        loaded.config.chain_id,
        adapters,
        select_bind,
        fee,
        oracle,
    )
    .with_engine_capacity(loaded.intern.asset_id_capacity(), engine_positions)
    .with_assets(&loaded.intern)
    .with_gas_model(gas_model.as_ref(), &|f: &str| {
        bind::resolve_family(&loaded.intern, adapters, f)
    })
    .with_index(index)
    .with_bid_cfg(bid_cfg)
    .with_header_clock(clock)
    .with_price_reader(price_reader)
    .with_state_reader(state_reader);
    let hook = match sim {
        Some(sim) => hook.with_sim(Box::new(sim)),
        None => hook,
    };
    let hook = match backrun_operator {
        Some(key) => hook.with_backrun_operator(key),
        None => hook,
    };
    let hook = match crate::state_build::SnapshotWriter::spawn(
        state.clone(),
        loaded.config.snapshot_every_blocks,
        bindings,
    ) {
        Ok(w) => hook.with_snapshots(w),
        Err(e) => {
            tracing::error!(error = %e, "snapshot writer not started — state is not persisted");
            hook
        }
    };
    // Drift: each snapshot's sampled health against the protocols' own
    // views; a drifting position is resynced from chain, then quarantined if
    // it still disagrees (`crate::drift`).
    let drift_cfg = crate::drift::DriftConfig::from_env();
    let hook = match crate::drift::spawn(adapters, loaded.config.rpc_url.clone(), drift_cfg) {
        Ok(d) => {
            tracing::info!(
                max_bps = drift_cfg.max_bps,
                every_blocks = loaded.config.snapshot_every_blocks,
                "drift check started"
            );
            hook.with_drift(d)
        }
        Err(e) => {
            tracing::error!(error = %e, "drift thread not started — drift is not checked");
            hook
        }
    };
    let (hook, svr_thread) = match svr {
        Some((rx, targets, handle)) => (hook.with_svr(rx, targets), Some(handle)),
        None => (hook, None),
    };
    let hook = match gov {
        Some((rx, inbox)) => hook.with_gov(rx, inbox),
        None => hook,
    };
    let _map = pin_threads(cores_path, allow_unpinned)?;
    let sink: &'static dyn liq_types::HaltSink = shared.risk;
    let (forwarder, hot, tracked) = register_exex(
        store,
        sink,
        adapters,
        index,
        allow_unpinned,
        Some(Box::new(hook)),
    )?;
    let _ = StoreSnapshot::empty();
    Ok(Started {
        head,
        shared,
        hot,
        forwarder,
        tracked,
        exec,
        assemble,
        inclusion,
        _stall: stall,
        _svr: svr_thread,
        _gov: gov_thread,
    })
}

/// The in-process simulator on the node's state. Jobs go to the deployed
/// Executor (`venues.executor`), or else to the compiled one at the planned
/// address, built for the operator and `PROFIT_SINK` — the Executor the
/// exec worker's node check simulates. `None`: that check verifies jobs,
/// and the ones that need a parent transaction replayed are not sent.
fn live_sim(
    node_state: Option<Arc<dyn liq_sim::StateProviderFactory>>,
    deployed: Option<alloy_primitives::Address>,
    operator: Option<alloy_primitives::Address>,
    backrun_operator: Option<alloy_primitives::Address>,
    lease: &SubmitLease,
) -> Option<crate::drain::LiveSim> {
    let Some(state) = node_state else {
        tracing::warn!(
            "no node state in this process — jobs are verified over RPC, and SVR replays are not sent"
        );
        return None;
    };
    let sim = match deployed {
        Some(executor) => liq_sim::NodeSim::deployed(state, executor),
        None => {
            let (Some(operator), Some(sink)) = (operator, profit_sink_from_env()) else {
                tracing::error!(
                    "no deployed Executor, and no operator or PROFIT_SINK to build one — in-process simulator off"
                );
                return None;
            };
            // No second key: the compiled Executor gets the first one twice.
            let spec = liq_sim::ExecutorSpec::mainnet(
                operator,
                backrun_operator.unwrap_or(operator),
                sink,
            );
            match liq_sim::NodeSim::compiled(state, &spec) {
                Ok(sim) => sim,
                Err(e) => {
                    tracing::error!(error = %e, "Executor artifact unreadable (run forge build) — in-process simulator off");
                    return None;
                }
            }
        }
    };
    tracing::info!(
        executor = %sim.executor(),
        "in-process simulator on the node's state; it runs once the store reaches the node's head"
    );
    Some(crate::drain::LiveSim::new(sim, lease.held_flag()))
}

/// MEV-Share reader for configured SVR aggregators. No targets, or a
/// refused book, does not open the stream.
fn start_svr_reader(
    index: &'static crate::index::BoundIndex,
    sink: &'static dyn liq_types::HaltSink,
) -> Option<(
    rtrb::Consumer<liq_types::MevShareHint>,
    Vec<liq_oracle::mevshare::SvrTarget>,
    std::thread::JoinHandle<()>,
)> {
    let Some(book) = index.canonical.as_ref() else {
        tracing::error!("canonical book absent — MEV-Share reader not started");
        return None;
    };
    let targets = match book.lock().svr_targets() {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "SVR targets refused — MEV-Share reader not started");
            return None;
        }
    };
    if targets.is_empty() {
        tracing::error!("no chainlink-svr aggregators — MEV-Share reader not started");
        return None;
    }
    let (tx, rx) = rtrb::RingBuffer::<liq_types::MevShareHint>::new(1024);
    match liq_oracle::mevshare::spawn_hint_reader(sink, tx) {
        Ok(handle) => Some((rx, targets, handle)),
        Err(e) => {
            tracing::error!(error = %e, "SVR stream thread refused");
            None
        }
    }
}

/// 16D monitor. Empty window stays ABSENT. Does not write a numeric p99.
fn spawn_rtt_tick(builders: &Path) {
    let mon = match RttMonitor::from_builders_toml(builders) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "RttMonitor construct refused");
            return;
        }
    };
    if let Err(e) = std::thread::Builder::new()
        .name("liq-bot-rtt".into())
        .spawn(move || loop {
            let rep = mon.report();
            if rep.invented_p99() {
                tracing::error!("rtt tick invented a p99 — refuse to treat as measured");
            } else {
                tracing::info!(
                    builders = rep.builders.len(),
                    path_a = ?rep.path_a.verdict,
                    "rtt tick (empty window ABSENT; no numeric p99 written)"
                );
            }
            std::thread::sleep(std::time::Duration::from_secs(30));
        })
    {
        tracing::error!(?e, "RttMonitor tick thread not started");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_constants() {
        assert!(!prod_allow_unpinned());
        assert_eq!(
            [
                StartupStep::RegistryAssert,
                StartupStep::ConfigValidate,
                StartupStep::SharedLeak,
                StartupStep::ThreadsPinned,
                StartupStep::ExExRegistered,
            ]
            .len(),
            5
        );
    }

    #[test]
    fn leak_shared_after_validate_shape() {
        let cfg = BotConfig {
            chain_id: 1,
            registry_path: std::path::PathBuf::from("registry/registry.json"),
            rpc_url: String::new(),
            risk: liq_config::RiskConfig::default(),
            venues: liq_config::VenuesConfig::default(),
            submit_enabled: false,
            backfill_from: 0,
            snapshot_every_blocks: 100,
        };
        let s = leak_process_shared(&cfg, SubmitLease::refused());
        assert!(!s.submit_enabled.get());
        assert!(!s.lease.held());
    }

    #[test]
    fn run_source_joins_bind_warm_rtt_assemble() {
        let src = include_str!("startup.rs");
        assert!(
            src.contains("bind_from_config"),
            "old unbound run() never called exec_bind"
        );
        assert!(src.contains("exec:"));
        assert!(src.contains("spawn_warm_thread"));
        assert!(src.contains("RttMonitor"));
        assert!(src.contains("ProcessAssembleView"));
        let worker = include_str!("exec_worker.rs");
        assert!(worker.contains("path.prewarm()"));
        assert!(
            worker.contains("spawn_rewarm"),
            "warm sockets need the re-warm loop, on the exec runtime"
        );
        assert!(src.contains("EXEC_THREAD"), "submit thread gets its core");
        assert!(src.contains("ProcessSecrets::from_env"));
        assert!(
            src.contains("DrainJoin"),
            "old unbound run() never joined drain"
        );
        assert!(
            src.contains("spawn_exec_worker"),
            "old unbound run() never spawned exec worker"
        );
        assert!(
            src.contains("ExecInbox"),
            "old unbound run() never constructed the inbox"
        );
        assert!(
            src.contains("load_protocols"),
            "17E run() must load protocol TOML"
        );
        assert!(src.contains("intern_view"));
        assert!(src.contains("fee_from_oracle"));
        assert!(src.contains("DrainJoin::live"));
        assert!(
            src.contains("hook.with_sim(") && src.contains("NodeSim::deployed"),
            "run() must attach the in-process simulator on the node's state"
        );
        assert!(
            src.contains("leak_protocols"),
            "17F must leak the load once for ingest and drain"
        );
        assert!(src.contains("router_subscribers"));
        assert!(src.contains("router_handlers"));
        assert!(src.contains("protocol_ids"));
        assert!(src.contains("load_index"));
        assert!(src.contains("leak_index"));
        assert!(src.contains("with_index"));
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        assert!(
            !prod.contains(".store(true"),
            "only ExecPath::sync_nonce stores nonce_resync, after a chain read"
        );
        assert!(
            !prod.contains("30_000_000") && !prod.contains("30000000"),
            "run() must not invent header gas 30M"
        );
        assert!(
            !prod.contains("MemoryFactory"),
            "run() must not attach MemoryFactory::empty"
        );
        assert!(
            !prod.contains("from_subscribers(&[])"),
            "17F must bind loaded adapters, not empty subscribers"
        );
        assert!(
            !prod.contains("Box::new([])"),
            "17F must pass loaded protocol ids, not an empty box"
        );
        assert!(!prod.contains("BidConfig::new"));
        assert!(!prod.contains("gas_failed: 50_000"));
    }

    #[test]
    fn rtt_tick_empty_window_absent() {
        let path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/builders.toml");
        let mon = RttMonitor::from_builders_toml(&path).unwrap();
        let rep = mon.report();
        assert!(rep.builders.iter().all(|r| r.p99_ns.is_none()));
        assert!(rep.mevshare.p99_ns.is_none());
        assert!(!rep.invented_p99());
        assert_eq!(
            liq_obs::thirteen_a_http_pool_seam().handshake_free_critical,
            liq_obs::net_rtt::Claim::ProvenOnSubmitClient
        );
    }
}

/// Phase 4 on the live book: how long the graph, the zero-size table and
/// the exact search take on every committed pool seeded from chain, with
/// pools under a USD depth floor left out as dust.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod graph_live {
    use std::collections::HashMap;

    use alloy_primitives::{address, U256};
    use liq_router::graph::{
        pool_depths, raw_value, search, RawValue, SearchBudget, TokenGraph, ZeroTable,
    };
    use liq_types::AssetId;

    #[test]
    #[ignore = "needs MAINNET_RPC_URL"]
    fn live_graph_table_and_search_timings() {
        let url = std::env::var("MAINNET_RPC_URL").unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = liq_config::Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = liq_config::Intern::from_registry(&reg).unwrap();
        let mut load = crate::index::load_index(&root.join("config"), &intern, &reg);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(super::seed_index(&mut load, &url));
        let book = &load.book;
        let decimals = |a: AssetId| {
            intern
                .assets()
                .iter()
                .find(|r| r.id == a)
                .map_or(18, |r| r.decimals)
        };
        // Canonical USD prices seed the depth pricing.
        let canonical = load.canonical.as_ref().unwrap();
        let seed: HashMap<AssetId, RawValue> = intern
            .assets()
            .iter()
            .filter_map(|r| {
                let p = canonical.price(r.id).filter(|p| p.ts != 0)?;
                Some((r.id, raw_value(p.price.raw(), r.decimals)?))
            })
            .collect();
        let usdc = intern
            .asset(address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"))
            .unwrap();
        let gas = liq_router::GasTerms {
            base_fee_wei: 2_000_000_000,
            priority_fee_wei: 0,
            out_per_eth: U256::from(3_000_000_000u64),
        };
        let usd = |n: u64| U256::from(n) * U256::from(10u64).pow(U256::from(18u64));
        eprintln!(
            "book: {} pools ({} live), {} unwraps; {} seed prices",
            book.pools().len(),
            book.pools().iter().filter(|p| p.is_live()).count(),
            book.unwraps().count(),
            seed.len()
        );
        for floor in [10_000u64, 100_000] {
            let t0 = std::time::Instant::now();
            let (depth, prices) = pool_depths(book, &seed, usd(floor), 6);
            let depth_us = t0.elapsed().as_micros();
            let kept = depth
                .iter()
                .filter(|d| d.is_some_and(|d| d >= usd(floor)))
                .count();
            let g = TokenGraph::build_with(book, |id| {
                usize::try_from(id.0)
                    .ok()
                    .and_then(|i| depth.get(i).copied().flatten())
                    .is_some_and(|d| d >= usd(floor))
            })
            .unwrap();
            let t0 = std::time::Instant::now();
            let table = ZeroTable::build(&g, book, 4).unwrap();
            let table_ms = t0.elapsed().as_millis();
            eprintln!(
                "floor ${floor}: {} tokens priced, depths in {depth_us} us; {kept} pools kept; graph {} tokens, {} edges; table K=4 {table_ms} ms",
                prices.len(),
                g.nodes().len(),
                g.edges().len()
            );
            let mut stats = Vec::new();
            for &from in g.nodes() {
                if from == usdc || table.best(&g, from, usdc).is_none() {
                    continue;
                }
                let amount =
                    U256::from(1_000u64) * U256::from(10u64).pow(U256::from(decimals(from)));
                let budget = SearchBudget {
                    max_hops: 4,
                    top_n: 4,
                    max_quotes: 50_000,
                    min_share_bps: 5_000,
                };
                let t0 = std::time::Instant::now();
                let r = search(&g, &table, book, &gas, from, usdc, amount, budget).unwrap();
                stats.push((
                    t0.elapsed().as_micros(),
                    r.quotes,
                    r.pruned,
                    r.exhausted,
                    r.routes.len(),
                ));
            }
            stats.sort_unstable();
            let n = stats.len();
            if n == 0 {
                continue;
            }
            let found = stats.iter().filter(|s| s.4 > 0).count();
            let capped = stats.iter().filter(|s| s.3).count();
            eprintln!("  search into USDC from {n} tokens (K=4, top 4): {found} found a route, {capped} hit the quote cap");
            for p in [50usize, 90, 99, 100] {
                let s = stats[(n - 1) * p / 100];
                eprintln!("    p{p}: {} us, {} quotes, {} pruned", s.0, s.1, s.2);
            }
        }
    }
}

/// The curated graph on the live book: each provider's ten deepest pools,
/// each pairing taken once by its deepest provider.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod curated_live {
    use std::collections::HashMap;

    use alloy_primitives::{address, U256};
    use liq_router::graph::{
        curated_pools, raw_value, search, Provider, Quota, RawValue, SearchBudget, TokenGraph,
        ZeroTable,
    };
    use liq_types::AssetId;

    #[test]
    #[ignore = "needs MAINNET_RPC_URL"]
    fn live_curated_deepest_pools() {
        let url = std::env::var("MAINNET_RPC_URL").unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = liq_config::Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = liq_config::Intern::from_registry(&reg).unwrap();
        let mut load = crate::index::load_index(&root.join("config"), &intern, &reg);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(super::seed_index(&mut load, &url));
        let book = &load.book;
        let rec = |a: AssetId| intern.assets().iter().find(|r| r.id == a);
        let sym = |a: AssetId| {
            rec(a)
                .and_then(|r| r.symbol.clone())
                .unwrap_or_else(|| format!("#{}", a.0))
        };
        let canonical = load.canonical.as_ref().unwrap();
        let seed: HashMap<AssetId, RawValue> = intern
            .assets()
            .iter()
            .filter_map(|r| {
                let p = canonical.price(r.id).filter(|p| p.ts != 0)?;
                Some((r.id, raw_value(p.price.raw(), r.decimals)?))
            })
            .collect();
        let wad = U256::from(10u64).pow(U256::from(18u64));
        // Every token's total supply, for the market-cap check on prices
        // only one pool offers.
        let supply: HashMap<AssetId, U256> = {
            use alloy_sol_types::{sol, SolCall};
            sol! { function totalSupply() returns (uint256); }
            let rpc = liq_config::rpc::HttpRpc::connect(&url).unwrap();
            let block = rt
                .block_on(liq_config::rpc::ChainRpc::block_number(&rpc))
                .unwrap();
            let recs: Vec<_> = intern.assets().iter().collect();
            let mut out = HashMap::new();
            for chunk in recs.chunks(300) {
                let calls = chunk
                    .iter()
                    .map(|r| crate::pool_seed::call(r.address, totalSupplyCall {}.abi_encode()))
                    .collect();
                if let Some(res) = rt.block_on(crate::pool_seed::aggregate(&rpc, calls, block)) {
                    for (r, x) in chunk.iter().zip(res) {
                        if x.success && x.returnData.len() >= 32 {
                            out.insert(r.id, U256::from_be_slice(&x.returnData[..32]));
                        }
                    }
                }
            }
            out
        };
        let t0 = std::time::Instant::now();
        let trace = liq_router::graph::pool_depths_traced(
            book,
            &seed,
            U256::from(10_000u64) * wad,
            6,
            Some(&supply),
        );
        let (depth, prices) = (trace.depths.clone(), trace.prices.clone());
        eprintln!(
            "depths (2 % slippage, exact quotes) in {} ms",
            t0.elapsed().as_millis()
        );
        let mut per: HashMap<Provider, usize> = HashMap::new();
        for (p, d) in book.pools().iter().zip(&depth) {
            if d.is_some() {
                if let Some(pr) = Provider::of(p) {
                    *per.entry(pr).or_default() += 1;
                }
            }
        }
        // Uniswap V2, V3 and V4: every unique pool of $1M or more, at most
        // 50 each. The others: their ten deepest.
        let t0 = std::time::Instant::now();
        let curated = curated_pools(book, &depth, |p| match p {
            Provider::UniswapV2 | Provider::UniswapV3 | Provider::UniswapV4 => Quota {
                max: 50,
                min_depth: U256::from(1_000_000u64) * wad,
            },
            _ => Quota {
                max: 10,
                min_depth: U256::ZERO,
            },
        });
        eprintln!("curated in {} us", t0.elapsed().as_micros());
        for pr in Provider::ALL {
            eprintln!(
                "{pr:?} ({} pools with a measured depth):",
                per.get(&pr).copied().unwrap_or(0)
            );
            for c in curated.iter().filter(|c| c.provider == pr) {
                let pool = book.get(c.pool).unwrap();
                let names: Vec<String> = c.pairing.iter().map(|a| sym(*a)).collect();
                eprintln!(
                    "  {:#x}  {:<28} ${}",
                    pool.address,
                    names.join("/"),
                    c.depth / wad
                );
            }
        }
        // Implausible depths: each coin's derived price (USD per whole token).
        let per_token = |a: AssetId| -> String {
            let Some(v) = prices.get(&a) else {
                return "unpriced".into();
            };
            let d = rec(a).map_or(18, |r| r.decimals);
            // v = USD (WAD) of 1e18 raw; per whole token: v · 10^d / 1e18, in micro-USD.
            let micro =
                *v * U256::from(10u64).pow(U256::from(d)) / wad / U256::from(1_000_000_000_000u64);
            format!(
                "${}.{:06}{}",
                micro / U256::from(1_000_000u64),
                micro % U256::from(1_000_000u64),
                if seed.contains_key(&a) { " (feed)" } else { "" }
            )
        };
        for c in curated
            .iter()
            .filter(|c| c.depth > U256::from(1_000_000_000u64) * wad)
        {
            let pool = book.get(c.pool).unwrap();
            let coins: Vec<String> = c
                .pairing
                .iter()
                .map(|a| format!("{} {}", sym(*a), per_token(*a)))
                .collect();
            eprintln!(
                "  suspicious {:?} {:#x}: {}",
                c.provider,
                pool.address,
                coins.join(", ")
            );
        }
        // Where FRAX's and rswETH's prices came from, back to a feed.
        for target in [
            address!("853d955acef822db058eb8505911ed77f175b99e"),
            address!("fae103dc9cf190ed75350761e95403b7b8afa6c0"),
        ] {
            let Some(mut at) = intern.asset(target) else {
                continue;
            };
            eprintln!("chain for {}:", sym(at));
            for _ in 0..8 {
                let Some(o) = trace.chosen.get(&at) else {
                    eprintln!("    {} {} (seed)", sym(at), per_token(at));
                    break;
                };
                let pool = book.get(o.pool).unwrap();
                let n = trace.offers.get(&at).map_or(0, Vec::len);
                eprintln!(
                    "    {} {} <- pass {} {:?} {:#x} from {} (weight ${}; {n} offers)",
                    sym(at),
                    per_token(at),
                    o.pass,
                    Provider::of(pool),
                    pool.address,
                    sym(o.from),
                    o.depth / wad
                );
                at = o.from;
            }
        }
        // Oracle for every pool's zero-size rate: an exact quote of $100.
        // ρ² / 2^192 must equal quote / amount to within the quote's own
        // slippage (well under 1 % at $100 on a pool deeper than $10k).
        {
            use liq_router::solver::Q96;
            let mut bad: HashMap<Provider, (usize, usize, Vec<String>)> = HashMap::new();
            for (pidx, pool) in book.pools().iter().enumerate() {
                let Some(pr) = Provider::of(pool) else {
                    continue;
                };
                if depth
                    .get(pidx)
                    .copied()
                    .flatten()
                    .is_none_or(|d| d < U256::from(10_000u64) * wad)
                {
                    continue;
                }
                let n = pool.assets.len();
                for i in 0..n {
                    for j in 0..n {
                        if i == j {
                            continue;
                        }
                        let (Some(v), Ok(i8), Ok(j8)) = (
                            prices.get(&pool.assets[i]),
                            u8::try_from(i),
                            u8::try_from(j),
                        ) else {
                            continue;
                        };
                        let amount = U256::from(100u64) * wad * wad / *v;
                        if amount.is_zero() {
                            continue;
                        }
                        let (Ok(rho), Ok(out)) = (
                            pool.rho_at_zero(i8, j8),
                            pool.quote_exact_in(i8, j8, amount),
                        ) else {
                            continue;
                        };
                        let ideal = amount * rho / Q96 * rho / Q96;
                        let e = bad.entry(pr).or_default();
                        e.0 += 1;
                        // |out/ideal − 1| > 1 %
                        let off = out * U256::from(100u64) < ideal * U256::from(99u64)
                            || out * U256::from(100u64) > ideal * U256::from(101u64);
                        if off {
                            e.1 += 1;
                            if e.2.len() < 4 {
                                e.2.push(format!(
                                    "{:#x} {}->{} quote {} ideal {}",
                                    pool.address,
                                    sym(pool.assets[i]),
                                    sym(pool.assets[j]),
                                    out,
                                    ideal
                                ));
                            }
                        }
                    }
                }
            }
            for pr in Provider::ALL {
                if let Some((n, k, ex)) = bad.get(&pr) {
                    eprintln!("rho check {pr:?}: {k} of {n} directions off by more than 1 %");
                    for e in ex {
                        eprintln!("    {e}");
                    }
                }
            }
        }
        // Oracle for V3 / V4 depth: Uniswap's own quoters (QuoterV2 runs the
        // pool's `swap` and reverts with the result; V4Quoter the
        // PoolManager's), at each curated pool's 2 % size, both ways.
        {
            use alloy_sol_types::{sol, SolCall};
            use liq_config::rpc::ChainRpc;
            use liq_router::solver::PoolState;
            sol! {
                struct QV3 { address tokenIn; address tokenOut; uint256 amountIn; uint24 fee; uint160 sqrtPriceLimitX96; }
                function quoteExactInputSingle(QV3 params) returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed, uint256 gasEstimate);
                struct QPoolKey { address currency0; address currency1; uint24 fee; int24 tickSpacing; address hooks; }
                struct QV4 { QPoolKey poolKey; bool zeroForOne; uint128 exactAmount; bytes hookData; }
                function quoteV4(QV4 params) returns (uint256 amountOut, uint256 gasEstimate);
            }
            const V3_QUOTER: alloy_primitives::Address =
                address!("61fFE014bA17989E743c5F6cB21bF9697530B21e");
            const V4_QUOTER: alloy_primitives::Address =
                address!("52f0e24d1c21c8a0cb1e5a5dd6198556bd9e1203");
            let rpc = liq_config::rpc::HttpRpc::connect(&url).unwrap();
            let mut targets: Vec<liq_router::PoolId> = curated
                .iter()
                .filter(|c| matches!(c.provider, Provider::UniswapV3 | Provider::UniswapV4))
                .map(|c| c.pool)
                .collect();
            for a in [
                address!("fad866e71675b9f0b79ac90948ccb3d07763719c"),
                address!("c0e502446a2013c4bff35b0686e4766f23c7ee98"),
            ] {
                if let Some(id) = book.by_address(a) {
                    if !targets.contains(&id) {
                        targets.push(id);
                    }
                }
            }
            let (mut same, mut off, mut refused) = (0usize, 0usize, 0usize);
            for id in targets {
                let pool = book.get(id).unwrap();
                let PoolState::V3(s) = &pool.state else {
                    continue;
                };
                for (i, j) in [(0u8, 1u8), (1, 0)] {
                    let Some(v) = prices.get(&pool.assets[usize::from(i)]) else {
                        continue;
                    };
                    let Some(usd) = liq_router::graph::slip_depth(pool, i, j, *v) else {
                        continue;
                    };
                    let amount = usd * wad / *v;
                    if amount.is_zero() || amount > U256::from(u128::MAX) {
                        continue;
                    }
                    let ours = pool.quote_exact_in(i, j, amount).ok();
                    let data: alloy_primitives::Bytes = match &s.v4 {
                        None => quoteExactInputSingleCall {
                            params: QV3 {
                                tokenIn: pool.tokens[usize::from(i)],
                                tokenOut: pool.tokens[usize::from(j)],
                                amountIn: amount,
                                fee: alloy_primitives::aliases::U24::from(s.fee_pips),
                                sqrtPriceLimitX96: alloy_primitives::aliases::U160::ZERO,
                            },
                        }
                        .abi_encode()
                        .into(),
                        Some(k) => quoteV4Call {
                            params: QV4 {
                                poolKey: QPoolKey {
                                    currency0: k.currency0,
                                    currency1: k.currency1,
                                    fee: alloy_primitives::aliases::U24::from(k.fee),
                                    tickSpacing: alloy_primitives::aliases::I24::try_from(
                                        k.tick_spacing,
                                    )
                                    .unwrap(),
                                    hooks: k.hooks,
                                },
                                zeroForOne: i == 0,
                                exactAmount: u128::try_from(amount).unwrap(),
                                hookData: alloy_primitives::Bytes::new(),
                            },
                        }
                        .abi_encode()
                        .into(),
                    };
                    // The V4 call is encoded from a local name: stamp the
                    // V4Quoter's own selector.
                    let mut data = data.to_vec();
                    if s.v4.is_some() {
                        let sig = alloy_primitives::keccak256(
                            "quoteExactInputSingle(((address,address,uint24,int24,address),bool,uint128,bytes))",
                        );
                        data[..4].copy_from_slice(&sig[..4]);
                    }
                    let to = if s.v4.is_some() { V4_QUOTER } else { V3_QUOTER };
                    let theirs = rt
                        .block_on(rpc.call(to, data.into()))
                        .ok()
                        .and_then(|raw| raw.get(..32).map(U256::from_be_slice));
                    let label = format!(
                        "{:?} {:#x} {}->{} ${}",
                        Provider::of(pool),
                        pool.address,
                        sym(pool.assets[usize::from(i)]),
                        sym(pool.assets[usize::from(j)]),
                        usd / wad
                    );
                    match (ours, theirs) {
                        (Some(o), Some(t)) => {
                            let close = o * U256::from(1000u64) >= t * U256::from(995u64)
                                && o * U256::from(1000u64) <= t * U256::from(1005u64);
                            if close {
                                same += 1;
                            } else {
                                off += 1;
                                eprintln!("  quoter MISMATCH {label}: ours {o}, quoter {t}");
                            }
                        }
                        (o, t) => {
                            refused += 1;
                            eprintln!("  quoter refused {label}: ours {o:?}, quoter {t:?}");
                        }
                    }
                }
            }
            eprintln!("quoter check: {same} within 0.5 %, {off} mismatched, {refused} refused by one side");
        }
        // A graph of only these pools (and every unwrap), and what it reaches.
        let keep: std::collections::HashSet<u32> = curated.iter().map(|c| c.pool.0).collect();
        let g = TokenGraph::build_with(book, |id| keep.contains(&id.0)).unwrap();
        let t0 = std::time::Instant::now();
        let table = ZeroTable::build(&g, book, 4).unwrap();
        let table_us = t0.elapsed().as_micros();
        let usdc = intern
            .asset(address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"))
            .unwrap();
        let gas = liq_router::GasTerms {
            base_fee_wei: 2_000_000_000,
            priority_fee_wei: 0,
            out_per_eth: U256::from(3_000_000_000u64),
        };
        let mut times = Vec::new();
        let (mut reach, mut found) = (0usize, 0usize);
        for &from in g.nodes() {
            if from == usdc || table.best(&g, from, usdc).is_none() {
                continue;
            }
            reach += 1;
            let dec = rec(from).map_or(18, |r| r.decimals);
            let amount = U256::from(1_000u64) * U256::from(10u64).pow(U256::from(dec));
            let budget = SearchBudget {
                max_hops: 4,
                top_n: 4,
                max_quotes: 50_000,
                min_share_bps: 5_000,
            };
            let t0 = std::time::Instant::now();
            let r = search(&g, &table, book, &gas, from, usdc, amount, budget).unwrap();
            times.push((t0.elapsed().as_micros(), r.quotes, r.exhausted));
            found += usize::from(!r.routes.is_empty());
        }
        times.sort_unstable();
        eprintln!(
            "curated graph: {} pools, {} tokens, {} edges (with {} unwraps); table K=4 {table_us} us; {reach} tokens reach USDC, {found} with a route at 1 000 tokens",
            curated.len(),
            g.nodes().len(),
            g.edges().len(),
            book.unwraps().count()
        );
        if let (Some(mid), Some(max)) = (times.get(times.len() / 2), times.last()) {
            eprintln!(
                "  search p50 {} us ({} quotes), max {} us ({} quotes); {} hit the cap",
                mid.0,
                mid.1,
                max.0,
                max.1,
                times.iter().filter(|t| t.2).count()
            );
        }
    }
}
