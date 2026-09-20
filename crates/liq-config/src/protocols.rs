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
}

impl AaveV3Toml {
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
    }
}
