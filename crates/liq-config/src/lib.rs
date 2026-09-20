//! Figment config load, Validate trait, ConfigVersion, and registry boot assertion.
//!
//! Startup order (GUIDE 17): `assert_registry` → `Validate` → the rest of the
//! process. A registry that disagrees with chain, or an RPC that does not
//! answer, refuses to start. There is no degraded mode (REGISTRY.md §4c).

#![deny(clippy::todo, clippy::unimplemented)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )
)]

pub mod assert;
pub mod config;
pub mod error;
pub mod intern;
pub mod registry;
pub mod rpc;
pub mod validate;
pub mod version;

use std::path::Path;

pub use assert::assert_registry;
pub use config::{load, BotConfig, RiskConfig, VenuesConfig};
pub use error::{ConfigError, Result};
pub use intern::{AssetRec, FeedRec, Intern, MarketRec};
pub use registry::{
    OnChainId, OracleEntry, PoolEntry, PoolVenue, ProtocolEntry, Registry, SymbolCollision,
    TokenEntry, TokenQuirk,
};
pub use rpc::{ChainRpc, HttpRpc};
pub use validate::Validate;
pub use version::ConfigVersion;

/// Fully loaded, interned, and boot-asserted process config.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub config: BotConfig,
    pub registry: Registry,
    pub intern: Intern,
    pub version: ConfigVersion,
}

/// Load config + registry, intern identities, log [`ConfigVersion`], then
/// re-read every token/pool field from chain. Any failure refuses to start.
pub async fn boot(config_dir: &Path) -> Result<Loaded> {
    let config = load(config_dir)?;
    let registry = Registry::from_path(&config.registry_path)?;
    let version = ConfigVersion::hash(&config, &registry)?;
    tracing::info!(
        config_version = %version,
        chain_id = config.chain_id,
        tokens = registry.tokens.len(),
        pools = registry.pools.len(),
        "liq-config loaded"
    );
    let intern = Intern::from_registry(&registry)?;
    let rpc = HttpRpc::connect(&config.rpc_url)?;
    config.validate(&rpc).await?;
    registry.validate(&rpc).await?;
    Ok(Loaded {
        config,
        registry,
        intern,
        version,
    })
}

#[cfg(test)]
mod tests {
    use super::{load, ConfigVersion, Intern, Registry};
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    /// Oracle: GUIDE 00 §4 — the same files hash to the same version. Negative:
    /// hashing a mutated registry must not collide.
    #[test]
    fn config_version_is_stable_and_detects_drift() {
        let root = workspace_root();
        let cfg = load(&root.join("config")).unwrap();
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let a = ConfigVersion::hash(&cfg, &reg).unwrap();
        let b = ConfigVersion::hash(&cfg, &reg).unwrap();
        assert_eq!(a, b);
        let mut drifted = reg.clone();
        let usdc = alloy_primitives::address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        drifted.tokens.get_mut(&usdc).unwrap().decimals = 8;
        let c = ConfigVersion::hash(&cfg, &drifted).unwrap();
        assert_ne!(a, c, "a decimals edit must change ConfigVersion");
        let _ = Intern::from_registry(&reg).unwrap();
    }

    /// Oracle: `registry/schema.json` is JSON. Negative: a truncated schema file
    /// fails this before anyone treats it as the contract.
    #[test]
    fn schema_json_parses() {
        let p = workspace_root().join("registry/schema.json");
        let raw = std::fs::read(&p).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["type"], "object");
        assert!(v["properties"]["tokens"].is_object());
        assert!(v["properties"]["pools"].is_object());
    }
}
