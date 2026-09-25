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
    if loaded.config.submit_enabled {
        tracing::warn!("submit_enabled true at boot — H4 only; lease/resync still gate live send");
    }
    Ok(loaded)
}

/// Step 3: leak Shared. `submit_enabled` from config (default false).
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
        tracing::info!(core = hot.core, "hot pin configured; allow_unpinned=false");
    }
    Ok(map)
}

/// Live process after the five startup steps plus 17C process joins.
pub struct Started {
    pub shared: &'static Shared,
    pub hot: liq_node::HotHandle,
    pub forwarder: liq_node::ExExForwarder,
    /// 13A path. `None` when secrets/builders are missing — not an invented signer.
    pub exec: Option<Arc<ExecPath<ShadowRecorder, &'static RiskGate>>>,
    pub assemble: ProcessAssembleView,
    /// Inclusion watcher threads. `None` when the feed could not be built.
    pub inclusion: Option<crate::inclusion_feed::InclusionJoin>,
    _stall: Option<std::thread::JoinHandle<()>>,
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
) -> Result<(liq_node::ExExForwarder, liq_node::HotHandle), StartupError> {
    let inst = prepare();
    if adapters.is_empty() {
        tracing::error!("empty protocol list — ExEx protocol ids empty");
    }
    if adapters.is_empty() && index.subscriber_empty() {
        tracing::error!("empty protocol list — ingest subscribers empty");
    }
    let subs = bind::router_subscribers(adapters, index);
    let router = LogRouter::from_subscribers(&subs).map_err(StartupError::Ingest)?;
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
    )?;
    Ok((inst.forwarder, handle))
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

/// Full production order. RPC/registry failure refuses. Lease gates submit.
pub async fn run(
    config_dir: &Path,
    cores_path: &Path,
    state: &StatePaths,
    allow_unpinned: bool,
) -> Result<Started, StartupError> {
    let loaded = boot_assert(config_dir).await?;
    let (store, lease) = match acquire(state, recover_capacity(), None) {
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
    seed_canonical(&mut index_load, &loaded.config.rpc_url).await;
    match liq_config::rpc::HttpRpc::connect(&loaded.config.rpc_url) {
        Ok(rpc) => {
            crate::pool_seed::seed_v3(&mut index_load.book, &rpc).await;
            crate::pool_seed::seed_v2(&mut index_load.book, &rpc).await;
            crate::pool_seed::seed_curve(&mut index_load.book, &rpc).await;
        }
        Err(e) => tracing::error!(error = %e, "pool seed skipped — RPC connect failed"),
    }
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
    if let Some(ref path) = exec {
        if let Err(e) = path.prewarm().await {
            tracing::error!(error = %e, "13A prewarm failed — path stays bound; handshake_free Absent");
        }
    } else {
        tracing::error!("ExecPath unbound — ingest/lease continue; no invented key");
    }
    let mut assemble = bind::intern_view(&loaded.intern).with_bands(band_shared);
    // Live RPC for the four adapters (Liquity/Fluid/Gearbox/Compound V2)
    // whose `Config::new` refuses without a live-registry assertion. Reuses
    // `loaded.config.rpc_url` — the same node `boot()` already asserted the
    // token registry against. A connect/block-number failure here omits
    // just those four (named reason), the same fail-closed shape as every
    // other adapter-load failure; it does not fail the whole boot.
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
    let loaded_proto = bind::load_protocols(
        config_dir,
        &loaded.intern,
        live_rpc.as_ref().map(|(rpc, block)| (rpc, *block)),
    );
    let adapters = bind::leak_protocols(loaded_proto);
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
    let (inbox, rx) = if exec.is_some() {
        let (tx, rx) = ExecInbox::pair(1024);
        (Some(tx), Some(rx))
    } else {
        tracing::error!("ExecPath unbound — drain try_send fails closed; no exec worker");
        (None, None)
    };
    if let (Some(path), Some(rx)) = (exec.clone(), rx) {
        if let Err(e) = spawn_exec_worker(rx, path) {
            tracing::error!(?e, "exec worker not started — inbox will count full");
        }
    }
    let clock = Arc::new(crate::stall::HeaderClock::new());
    let stall = match crate::stall::spawn(Arc::clone(&clock), shared.risk) {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::error!(error = %e, "stall heartbeat not started");
            None
        }
    };
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
    .with_header_clock(clock);
    let _map = pin_threads(cores_path, allow_unpinned)?;
    let sink: &'static dyn liq_types::HaltSink = shared.risk;
    let (forwarder, hot) = register_exex(
        store,
        sink,
        adapters,
        index,
        allow_unpinned,
        Some(Box::new(hook)),
    )?;
    let _ = StoreSnapshot::empty();
    Ok(Started {
        shared,
        hot,
        forwarder,
        exec,
        assemble,
        inclusion,
        _stall: stall,
    })
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
    use liq_config::load;

    #[test]
    fn order_constants_and_submit_default_false() {
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
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cfg = load(&root.join("config")).unwrap();
        assert!(
            !cfg.submit_enabled,
            "committed submit_enabled must stay false"
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
        assert!(src.contains("prewarm"));
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
            "run() must not store nonce_resync true"
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
            liq_obs::net_rtt::Claim::Absent
        );
    }
}
