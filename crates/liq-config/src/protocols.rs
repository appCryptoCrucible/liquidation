//! Protocol instance TOML (`config/protocols/*.toml`). Maps onto adapter
//! `Config` fields without taking a dependency on any adapter crate.

use crate::error::ConfigError;
use crate::Result;
use alloy_primitives::Address;
use serde::Deserialize;
use std::path::Path;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AaveV3Toml {
    pub protocol: u16,
    pub pinned_through: u64,
    pub pools: Vec<AaveV3PoolToml>,
    pub assets: Vec<AaveV3AssetToml>,
    pub price_sources: Vec<AaveV3SourceToml>,
    pub liquidation: AaveV3LiquidationToml,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AaveV3PoolToml {
    pub address: Address,
    pub market: u32,
    pub oracle: Address,
    pub provider: Address,
    pub configurator: Address,
    pub sentinel: Address,
    pub sequencer_oracle: Address,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AaveV3AssetToml {
    pub underlying: Address,
    pub asset: u16,
    pub feed: u16,
    pub siloed: bool,
    pub isolated: bool,
    pub debt_ceiling: u128,
    pub decimals: u8,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AaveV3SourceToml {
    pub pool: Address,
    pub underlying: Address,
    pub source: Address,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AaveV3LiquidationToml {
    pub close_factor_bps: u16,
    pub close_factor_hf_wad: u128,
    pub min_base_max_close: u128,
    pub oracle_decimals: u8,
    /// Absent on Aave V3: `token-math-35`. Spark sets `wad-ray-half-up`.
    #[serde(default = "default_balance_model")]
    pub balance_model: String,
    /// Absent on Aave V3: `position-base`. Spark sets `reserve-debt`.
    #[serde(default = "default_close_scope")]
    pub close_factor_scope: String,
}

fn default_balance_model() -> String {
    "token-math-35".into()
}

fn default_close_scope() -> String {
    "position-base".into()
}

impl AaveV3Toml {
    pub fn from_path(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| ConfigError::Load(e.to_string()))?;
        let text = std::str::from_utf8(&bytes).map_err(|e| ConfigError::Load(e.to_string()))?;
        toml::from_str(text).map_err(|e| ConfigError::Load(e.to_string()))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AaveV4Toml {
    pub protocol: u16,
    pub pinned_through: u64,
    pub hubs: Vec<AaveV4HubToml>,
    pub spokes: Vec<AaveV4SpokeToml>,
    pub assets: Vec<AaveV4AssetToml>,
    pub price_sources: Vec<AaveV4SourceToml>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AaveV4HubToml {
    pub address: Address,
    pub market: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AaveV4SpokeToml {
    pub address: Address,
    pub market: u32,
    pub oracle: Address,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AaveV4AssetToml {
    pub underlying: Address,
    pub asset: u16,
    pub feed: u16,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AaveV4SourceToml {
    pub spoke: Address,
    pub reserve_id: u16,
    pub source: Address,
}

impl AaveV4Toml {
    pub fn from_path(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| ConfigError::Load(e.to_string()))?;
        let text = std::str::from_utf8(&bytes).map_err(|e| ConfigError::Load(e.to_string()))?;
        toml::from_str(text).map_err(|e| ConfigError::Load(e.to_string()))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct MorphoBlueToml {
    pub protocol: u16,
    pub pinned_through: u64,
    pub morpho: Address,
    pub catalog: u32,
    pub first_market: u32,
    pub assets: Vec<MorphoAssetToml>,
    pub price_sources: Vec<MorphoSourceToml>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct MorphoAssetToml {
    pub underlying: Address,
    pub asset: u16,
    pub feed: u16,
    pub decimals: u8,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct MorphoSourceToml {
    pub oracle: Address,
    /// T16: the pair this oracle is pinned for. Morpho markets are isolated
    /// including their oracle, so an address alone does not say which pair
    /// it is safe to price.
    pub collateral: u16,
    pub loan: u16,
}

impl MorphoBlueToml {
    pub fn from_path(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| ConfigError::Load(e.to_string()))?;
        let text = std::str::from_utf8(&bytes).map_err(|e| ConfigError::Load(e.to_string()))?;
        toml::from_str(text).map_err(|e| ConfigError::Load(e.to_string()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::AaveV3Toml;
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    #[test]
    fn spark_toml_deserializes() {
        let path = workspace_root().join("config/protocols/spark.toml");
        let cfg = AaveV3Toml::from_path(&path).expect("spark.toml");
        assert_eq!(cfg.protocol, 8);
        assert_eq!(cfg.pools.len(), 1);
        assert_eq!(cfg.assets.len(), 20);
        assert_eq!(cfg.price_sources.len(), 20);
        assert_eq!(cfg.liquidation.close_factor_bps, 5_000);
        assert_eq!(cfg.liquidation.close_factor_hf_wad, 950_000_000_000_000_000);
        assert_eq!(cfg.liquidation.min_base_max_close, 0);
        assert_eq!(cfg.liquidation.oracle_decimals, 8);
        assert_eq!(cfg.liquidation.balance_model, "wad-ray-half-up");
        assert_eq!(cfg.liquidation.close_factor_scope, "reserve-debt");
    }

    /// Oracle: the committed registry's intern. Every pool market id, asset
    /// id and decimals in the generated aave-v3.toml must be the intern's —
    /// adding a registry token shifts asset ids, and this test is what fails
    /// until the toml is regenerated (tools/registry/gen_aave_v3_toml.py).
    #[test]
    fn aave_v3_toml_ids_match_registry_intern() {
        use crate::registry::OnChainId;
        use crate::{Intern, Registry};
        let root = workspace_root();
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let cfg = AaveV3Toml::from_path(&root.join("config/protocols/aave-v3.toml")).unwrap();
        assert_eq!(
            Some(liq_types::ProtocolId(cfg.protocol)),
            intern.protocol("aave-v3")
        );
        assert_eq!(cfg.pools.len(), 3, "Core, Prime, EtherFi");
        for p in &cfg.pools {
            let m = intern
                .markets()
                .iter()
                .find(|m| m.key == OnChainId::Addr(p.address))
                .unwrap_or_else(|| panic!("pool {} not interned", p.address));
            assert_eq!(m.id.0, p.market, "pool {} market id", p.address);
        }
        for a in &cfg.assets {
            let id = intern.asset(a.underlying).expect("asset interned");
            assert_eq!(id.0, a.asset, "{} asset id", a.underlying);
            assert_eq!(
                intern.decimals(id),
                Some(a.decimals),
                "{} decimals",
                a.underlying
            );
        }
        assert_eq!(cfg.liquidation.close_factor_bps, 5_000);
        assert_eq!(cfg.liquidation.close_factor_hf_wad, 950_000_000_000_000_000);
        assert_eq!(cfg.liquidation.min_base_max_close, 2_000 * 100_000_000);
        assert_eq!(cfg.liquidation.balance_model, "token-math-35");
        assert_eq!(cfg.liquidation.close_factor_scope, "position-base");
        assert!(cfg
            .price_sources
            .iter()
            .all(|s| cfg.assets.iter().any(|a| a.underlying == s.underlying)));
    }

    /// Same oracle for the generated aave-v4.toml and morpho-blue.toml
    /// (tools/registry/gen_aave_v4_toml.py, gen_morpho_toml.py).
    #[test]
    fn aave_v4_and_morpho_toml_ids_match_registry_intern() {
        use super::{AaveV4Toml, MorphoBlueToml};
        use crate::registry::OnChainId;
        use crate::{Intern, Registry};
        use alloy_primitives::Address;
        let root = workspace_root();
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let market_of = |a: Address| {
            intern
                .markets()
                .iter()
                .find(|m| m.key == OnChainId::Addr(a))
                .map(|m| m.id.0)
        };

        let v4 = AaveV4Toml::from_path(&root.join("config/protocols/aave-v4.toml")).unwrap();
        assert_eq!(
            Some(liq_types::ProtocolId(v4.protocol)),
            intern.protocol("aave-v4")
        );
        assert!(!v4.spokes.is_empty() && v4.spokes.len() <= 32);
        for h in &v4.hubs {
            assert_eq!(market_of(h.address), Some(h.market), "hub {}", h.address);
        }
        for s in &v4.spokes {
            assert_eq!(market_of(s.address), Some(s.market), "spoke {}", s.address);
        }
        for a in &v4.assets {
            assert_eq!(intern.asset(a.underlying).map(|i| i.0), Some(a.asset));
        }

        let m = MorphoBlueToml::from_path(&root.join("config/protocols/morpho-blue.toml")).unwrap();
        assert_eq!(
            Some(liq_types::ProtocolId(m.protocol)),
            intern.protocol("morpho-blue")
        );
        assert!(
            m.catalog > 4299 && m.first_market > m.catalog,
            "above Gearbox's band"
        );
        for a in &m.assets {
            let id = intern.asset(a.underlying).expect("asset interned");
            assert_eq!(id.0, a.asset);
            assert_eq!(intern.decimals(id), Some(a.decimals));
        }
        let ids: std::collections::BTreeSet<u16> = m.assets.iter().map(|a| a.asset).collect();
        assert!(m
            .price_sources
            .iter()
            .all(|p| ids.contains(&p.collateral) && ids.contains(&p.loan)));
    }
}
