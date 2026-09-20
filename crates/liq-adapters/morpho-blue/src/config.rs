//! Deployment pin: Morpho singleton + token intern + allowed oracles.
//! Markets are **not** listed here — `CreateMarket` assigns [`MarketId`]s.

use alloy_primitives::{Address, B256};
use liq_protocol::{BlockNum, FeedId};
use liq_types::{AssetId, MarketId, ProtocolId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetConfig {
    pub underlying: Address,
    pub asset: AssetId,
    pub feed: FeedId,
    pub decimals: u8,
}

/// Oracle address Morpho `MarketParams.oracle` must equal (fail closed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePin {
    pub oracle: Address,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub protocol: ProtocolId,
    pub morpho: Address,
    /// `Id → MarketId` index market (one row per created market).
    pub catalog: MarketId,
    /// First interned Morpho market; subsequent are `first_market.0 + n`.
    pub first_market: MarketId,
    pub assets: Vec<AssetConfig>,
    pub price_sources: Vec<SourcePin>,
    pub pinned_through: BlockNum,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Emitter {
    Morpho,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("morpho singleton is the zero address")]
    ZeroMorpho,
    #[error("catalog and first_market collide")]
    MarketCollision,
    #[error("address {0} configured twice")]
    DuplicateAddress(Address),
    #[error("asset id {0:?} or underlying configured twice")]
    DuplicateAsset(AssetId),
}

impl Config {
    pub fn validate(&self) -> core::result::Result<(), ConfigError> {
        if self.morpho == Address::ZERO {
            return Err(ConfigError::ZeroMorpho);
        }
        if self.catalog == self.first_market {
            return Err(ConfigError::MarketCollision);
        }
        let mut oracles: Vec<Address> = Vec::new();
        for p in &self.price_sources {
            if oracles.contains(&p.oracle) {
                return Err(ConfigError::DuplicateAddress(p.oracle));
            }
            oracles.push(p.oracle);
        }
        for (i, a) in self.assets.iter().enumerate() {
            if self
                .assets
                .iter()
                .skip(i.saturating_add(1))
                .any(|b| b.asset == a.asset || b.underlying == a.underlying)
            {
                return Err(ConfigError::DuplicateAsset(a.asset));
            }
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn asset_by_underlying(&self, underlying: Address) -> Option<&AssetConfig> {
        self.assets.iter().find(|a| a.underlying == underlying)
    }

    #[inline]
    pub(crate) fn underlying_of(&self, asset: AssetId) -> Option<Address> {
        self.assets
            .iter()
            .find(|a| a.asset == asset)
            .map(|a| a.underlying)
    }

    #[inline]
    pub(crate) fn oracle_pinned(&self, oracle: Address) -> bool {
        self.price_sources.iter().any(|p| p.oracle == oracle)
    }

    #[inline]
    pub(crate) fn assigned_market(&self, catalog_slot: u16) -> MarketId {
        MarketId(self.first_market.0.saturating_add(u32::from(catalog_slot)))
    }
}

/// Morpho `Id` as 32 bytes.
pub type MorphoId = B256;
