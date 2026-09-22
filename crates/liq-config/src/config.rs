//! Figment load: TOML files under `config/`, env `LIQ_*` overrides (GUIDE 00 §4).

use crate::error::ConfigError;
use crate::rpc::ChainRpc;
use crate::validate::Validate;
use crate::Result;
use alloy_primitives::Address;
use figment::providers::{Env, Format, Toml};
use figment::Figment;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Root process config. Protocol-specific values live in the registry and in
/// `config/protocols/*.toml` (later WPs). Secrets override via `LIQ_*`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BotConfig {
    /// EIP-155 chain id. This project is Ethereum mainnet only (D01).
    #[serde(default = "mainnet")]
    pub chain_id: u64,
    /// Path to `registry/registry.json`. Relative paths resolve against the
    /// parent of the config directory (the workspace root when loading `config/`).
    pub registry_path: PathBuf,
    /// HTTP RPC used for the boot assertion. Empty is invalid: fail closed.
    /// Override with `LIQ_RPC_URL`.
    #[serde(default)]
    pub rpc_url: String,
    /// Halt / caps surface. Filled by WP 14A; empty is structurally valid.
    #[serde(default)]
    pub risk: RiskConfig,
    /// Executor / builder surface. Executor address filled at H3.
    #[serde(default)]
    pub venues: VenuesConfig,
    /// Live HTTP send (H4 flip). Hot-reloadable. Default **false**.
    /// Not a compiled constant. Must never default true.
    #[serde(default)]
    pub submit_enabled: bool,
}

const fn mainnet() -> u64 {
    1
}

const fn default_global_concurrent() -> u32 {
    20
}
const fn default_per_provider_concurrent() -> u32 {
    1
}
const fn default_concentration_alert_bps() -> u32 {
    8_000
}
const fn default_haircut_floor_bps() -> u16 {
    8_000
}
const fn default_haircut_ceil_bps() -> u16 {
    9_900
}

/// Risk section. No per-liquidation notional cap — the viability band is
/// the size filter. Other defaults are the existing 14A concurrency and
/// haircut numbers. Empty `[risk]` deserializes to those.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RiskConfig {
    #[serde(default = "default_global_concurrent")]
    pub global_concurrent: u32,
    #[serde(default = "default_per_provider_concurrent")]
    pub per_provider_concurrent: u32,
    #[serde(default = "default_concentration_alert_bps")]
    pub concentration_alert_bps: u32,
    #[serde(default = "default_haircut_floor_bps")]
    pub haircut_floor_bps: u16,
    #[serde(default = "default_haircut_ceil_bps")]
    pub haircut_ceil_bps: u16,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            global_concurrent: default_global_concurrent(),
            per_provider_concurrent: default_per_provider_concurrent(),
            concentration_alert_bps: default_concentration_alert_bps(),
            haircut_floor_bps: default_haircut_floor_bps(),
            haircut_ceil_bps: default_haircut_ceil_bps(),
        }
    }
}

/// Venues section. `executor` is unset until the H3 deploy is committed.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct VenuesConfig {
    /// On-chain `Executor` (GUIDE 10). `None` until H3.
    #[serde(default)]
    pub executor: Option<Address>,
}

/// Load `config/node.toml` + `risk.toml` + `venues.toml`, then `LIQ_*` env.
///
/// Missing optional files are skipped (figment default). A missing
/// `registry_path` after merge is a load error.
pub fn load(config_dir: &Path) -> Result<BotConfig> {
    let fig = Figment::new()
        .merge(Toml::file(config_dir.join("node.toml")))
        .merge(Toml::file(config_dir.join("risk.toml")))
        .merge(Toml::file(config_dir.join("venues.toml")))
        .merge(Env::prefixed("LIQ_"));
    let mut cfg: BotConfig = fig
        .extract()
        .map_err(|e| ConfigError::Load(e.to_string()))?;
    if cfg.registry_path.is_relative() {
        let root = config_dir.parent().ok_or_else(|| {
            ConfigError::Load("config dir has no parent to resolve registry_path".into())
        })?;
        cfg.registry_path = root.join(&cfg.registry_path);
    }
    Ok(cfg)
}

impl BotConfig {
    /// Structural checks and nested `Validate`. Does not call `eth_chainId`
    /// — `boot` fetches that once and checks config + registry against it.
    pub(crate) async fn validate_local<R: ChainRpc + Sync>(&self, rpc: &R) -> Result<()> {
        if self.chain_id != 1 {
            return Err(ConfigError::ChainIdMismatch {
                expected: 1,
                found: self.chain_id,
            });
        }
        if self.rpc_url.is_empty() {
            return Err(ConfigError::RpcUnavailable {
                cause: "empty rpc_url".into(),
            });
        }
        if !self.registry_path.is_file() {
            return Err(ConfigError::Load(format!(
                "registry not a file: {}",
                self.registry_path.display()
            )));
        }
        self.risk.validate(rpc).await?;
        self.venues.validate(rpc).await?;
        Ok(())
    }
}

impl Validate for BotConfig {
    async fn validate<R: ChainRpc + Sync>(&self, rpc: &R) -> Result<()> {
        self.validate_local(rpc).await?;
        let found = rpc.chain_id().await?;
        if found != self.chain_id {
            return Err(ConfigError::ChainIdMismatch {
                expected: self.chain_id,
                found,
            });
        }
        Ok(())
    }
}

impl Validate for RiskConfig {
    async fn validate<R: ChainRpc + Sync>(&self, _rpc: &R) -> Result<()> {
        Ok(())
    }
}

impl Validate for VenuesConfig {
    async fn validate<R: ChainRpc + Sync>(&self, _rpc: &R) -> Result<()> {
        if let Some(addr) = self.executor {
            if addr.is_zero() {
                return Err(ConfigError::Load(
                    "venues.executor is the zero address".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::load;
    use figment::Jail;

    /// Oracle: GUIDE 00 §4 — env overrides the file. Does not touch
    /// `LIQ_RPC_URL` (that var is also read by live assertion tests in
    /// parallel threads; figment Jail only serializes other Jails).
    #[test]
    #[allow(clippy::result_large_err)]
    fn env_overrides_toml() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "node.toml",
                r#"
chain_id = 1
registry_path = "registry/registry.json"
rpc_url = "http://from-file"
"#,
            )?;
            jail.create_file("risk.toml", "[risk]\n")?;
            jail.create_file("venues.toml", "[venues]\n")?;
            jail.set_env("LIQ_CHAIN_ID", "11155111");
            let cfg = load(jail.directory()).map_err(|e| figment::Error::from(e.to_string()))?;
            assert_eq!(cfg.chain_id, 11155111);
            assert_eq!(cfg.rpc_url, "http://from-file");
            Ok(())
        });
    }
}
