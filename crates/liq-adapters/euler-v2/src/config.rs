//! Deployment pin: GenericFactory + EVC + token intern + allowed oracles.
//! Vaults are **not** the live universe — `ProxyCreated` assigns [`MarketId`]s.
//! `vaults` are admitted-address subscriptions sourced from the registry pin.

use alloy_primitives::Address;
use liq_protocol::{BlockNum, FeedId};
use liq_types::{AssetId, MarketId, ProtocolId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetConfig {
    pub underlying: Address,
    pub asset: AssetId,
    pub feed: FeedId,
    pub decimals: u8,
}

/// Oracle address EVault proxy metadata must equal (fail closed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePin {
    pub oracle: Address,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub protocol: ProtocolId,
    pub factory: Address,
    /// Ethereum Vault Connector. Collateral/controller enablement is EVC state;
    /// zero is refused at `validate`.
    pub evc: Address,
    /// `vault → MarketId` index market (one row per created proxy).
    pub catalog: MarketId,
    /// First interned EVault; subsequent are `first_market.0 + n`.
    pub first_market: MarketId,
    /// Admitted vault addresses at the pin block (subscriptions + backfill).
    /// Discovery of the live set is `ProxyCreated` / `getProxyListSlice`.
    pub vaults: Vec<Address>,
    pub assets: Vec<AssetConfig>,
    pub price_sources: Vec<SourcePin>,
    pub pinned_through: BlockNum,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Emitter {
    Factory,
    Evc,
    Vault(MarketId),
    /// Admitted address not yet interned via `ProxyCreated` / `EVaultCreated`.
    Pending,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("euler factory is the zero address")]
    ZeroFactory,
    #[error("evc is the zero address")]
    ZeroEvc,
    #[error("catalog and first_market collide")]
    MarketCollision,
    #[error("address {0} configured twice")]
    DuplicateAddress(Address),
    #[error("asset id {0:?} or underlying configured twice")]
    DuplicateAsset(AssetId),
}

impl Config {
    pub fn validate(&self) -> core::result::Result<(), ConfigError> {
        if self.factory == Address::ZERO {
            return Err(ConfigError::ZeroFactory);
        }
        if self.evc == Address::ZERO {
            return Err(ConfigError::ZeroEvc);
        }
        if self.catalog == self.first_market {
            return Err(ConfigError::MarketCollision);
        }
        let mut addrs: Vec<Address> = Vec::new();
        for a in [self.factory, self.evc] {
            if addrs.contains(&a) {
                return Err(ConfigError::DuplicateAddress(a));
            }
            addrs.push(a);
        }
        for v in &self.vaults {
            if addrs.contains(v) {
                return Err(ConfigError::DuplicateAddress(*v));
            }
            addrs.push(*v);
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

    #[inline]
    pub(crate) fn is_vault(&self, address: Address) -> bool {
        self.vaults.contains(&address)
    }

    /// Share-token intern: vault ERC-20 listed as `AssetConfig.underlying`.
    #[inline]
    pub(crate) fn token(&self, address: Address) -> Option<&AssetConfig> {
        self.asset_by_underlying(address)
    }
}
