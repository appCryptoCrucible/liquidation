//! Figment config load, Validate trait, ConfigVersion, and registry boot assertion.
//!
//! Startup order (GUIDE 17): load → intern → a single `eth_chainId` checked
//! against config and registry → token/pool/oracle views. A registry that
//! disagrees with chain, or an RPC that does not answer, refuses to start.
//! There is no degraded mode (REGISTRY.md §4c).

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
pub mod protocols;
pub mod registry;
pub mod rpc;
pub mod validate;
pub mod version;

use std::path::Path;

pub use assert::assert_registry;
pub use config::{load, BotConfig, RiskConfig, VenuesConfig};
pub use error::{ConfigError, Result};
pub use intern::{AssetRec, FeedRec, Intern, MarketRec};
pub use protocols::{AaveV3Toml, AaveV4Toml, MorphoBlueToml};
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
/// re-read every token/pool/oracle field from chain. Any failure refuses to
/// start. `eth_chainId` is fetched once.
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
    config.validate_local(&rpc).await?;
    let found = rpc.chain_id().await?;
    if found != config.chain_id {
        return Err(ConfigError::ChainIdMismatch {
            expected: config.chain_id,
            found,
        });
    }
    if found != registry.chain_id {
        return Err(ConfigError::ChainIdMismatch {
            expected: registry.chain_id,
            found,
        });
    }
    crate::assert::assert_registry_views(&registry, &rpc).await?;
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
        let mut other_box = cfg.clone();
        other_box.rpc_url = "http://other-box:8545".into();
        let d = ConfigVersion::hash(&other_box, &reg).unwrap();
        assert_eq!(a, d, "rpc_url is per-box and must not enter ConfigVersion");
        let mut flipped = cfg.clone();
        flipped.submit_enabled = true;
        let e = ConfigVersion::hash(&flipped, &reg).unwrap();
        assert_eq!(
            a, e,
            "submit_enabled is hot-reloadable and must not enter ConfigVersion"
        );
        assert!(!cfg.submit_enabled, "submit_enabled default is false");
        let _ = Intern::from_registry(&reg).unwrap();
    }

    /// Oracle: `registry/schema.json` is the contract for the committed file.
    /// Negative: a protocol key or missing pool field the schema rejects is a
    /// test failure, not a silent skip.
    #[test]
    fn schema_json_validates_committed_registry() {
        let root = workspace_root();
        let schema: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("registry/schema.json")).unwrap())
                .unwrap();
        let instance: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("registry/registry.json")).unwrap())
                .unwrap();
        assert_eq!(schema["type"], "object");
        let validator = jsonschema::validator_for(&schema).expect("schema.json compiles");
        let errors: Vec<String> = validator
            .iter_errors(&instance)
            .map(|e| e.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "registry.json failed schema.json: {errors:?}"
        );
    }
}
