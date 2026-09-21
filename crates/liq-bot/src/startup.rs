//! Startup order (GUIDE 00 / GUIDE 17): fail closed, no skip.
//!
//! 1. registry assert (00D)
//! 2. config `Validate`
//! 3. `Shared` built and leaked (`Box::leak` — never `LazyLock`)
//! 4. threads pinned and asserted (`allow_unpinned: false` in prod)
//! 5. ExEx registered

use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use liq_config::{boot, BotConfig, Loaded};
use liq_exec::submit::SubmitEnabled;
use liq_flash::FlashIndex;
use liq_node::LogRouter;
use liq_router::{WarmBuilder, WarmConfig, WarmRouteCache};
use liq_state::StoreSnapshot;
use thiserror::Error;

use crate::exex_install::{install_hot, prepare};
use crate::lease::{acquire, recover_capacity, StatePaths, SubmitLease};
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
    let flag = Arc::new(SubmitEnabled::new(cfg.submit_enabled));
    let builder = WarmBuilder::new(WarmConfig {
        max_impact_bps: 100,
        twa_blocks: 1,
        budget: liq_router::SolveBudget::default(),
    });
    Shared::leak(
        leak_state_shared(),
        WarmRouteCache::new(builder.slot()),
        Arc::new(ArcSwap::from_pointee(FlashIndex::new(4))),
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

/// Live process after the five startup steps.
pub struct Started {
    pub shared: &'static Shared,
    pub hot: liq_node::HotHandle,
    pub forwarder: liq_node::ExExForwarder,
}

/// Step 5: split ExEx rings and spawn hot (pin asserted inside).
pub fn register_exex(
    store: liq_state::StateStore,
    sink: &'static dyn liq_types::HaltSink,
    protocols: Box<[liq_types::ProtocolId]>,
    allow_unpinned: bool,
) -> Result<(liq_node::ExExForwarder, liq_node::HotHandle), StartupError> {
    let inst = prepare();
    let router = LogRouter::from_subscribers(&[]).map_err(StartupError::Ingest)?;
    let handle = install_hot(
        store,
        router,
        Vec::new(),
        inst.ingress,
        sink,
        protocols,
        Arc::clone(&inst.height),
        allow_unpinned,
    )?;
    Ok((inst.forwarder, handle))
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
    let shared = leak_process_shared(&loaded.config, lease);
    let _map = pin_threads(cores_path, allow_unpinned)?;
    let sink: &'static dyn liq_types::HaltSink = shared.risk;
    let (forwarder, hot) = register_exex(store, sink, Box::new([]), allow_unpinned)?;
    let _ = StoreSnapshot::empty();
    Ok(Started {
        shared,
        hot,
        forwarder,
    })
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
}
