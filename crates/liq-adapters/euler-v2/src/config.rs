//! Deployment pin: GenericFactory + EVC + token intern + allowed oracles.
//! Vaults are **not** the live universe — `ProxyCreated` assigns [`MarketId`]s.
//! `vaults` are admitted-address subscriptions sourced from the registry pin.
//!
//! # MarketId allocator (intern-global)
//!
//! `StateStore.market_index` is `MarketId → row` with no `ProtocolId`. Euler
//! vaults use the interned [`MarketId`] `Intern::from_registry` assigned to
//! that vault's `OnChainId::Addr` (`registry.protocols` iteration order).
//! Catalog is an index market, not a vault, and is **not** interned:
//! [`CATALOG_MARKET`]. Vaults absent from intern (new `ProxyCreated`) take
//! sequential ids from [`FIRST_DISCOVERED_MARKET`]. Never 3481..=3510
//! (Liquity V2 rework owns 3508..=3510).
//!
//! Production load is [`Config::load`] / [`Config::from_toml`] then
//! [`Config::bind_from_intern`]. [`Config::from_toml`] leaves `interned` empty;
//! [`crate::EulerV2::new`] refuses that whenever `vaults` is non-empty.

use std::path::Path;

use alloy_primitives::Address;
use liq_config::{Intern, OnChainId, Registry};
use liq_protocol::{BlockNum, FeedId};
use liq_types::{AssetId, MarketId, ProtocolId};

/// Catalog `vault → MarketId` index. Not interned.
pub const CATALOG_MARKET: MarketId = MarketId(3511);
/// First sequential id for vaults not in intern.
pub const FIRST_DISCOVERED_MARKET: MarketId = MarketId(3512);
/// Inclusive range reserved for other adapters (Liquity 3508..=3510).
pub const FOREIGN_MARKET_MIN: u32 = 3481;
pub const FOREIGN_MARKET_MAX: u32 = 3510;

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
    /// First sequential id for vaults **not** in [`Self::interned`].
    pub first_market: MarketId,
    /// Admitted vault addresses at the pin block (subscriptions + backfill).
    /// Discovery of the live set is `ProxyCreated` / `getProxyListSlice`.
    pub vaults: Vec<Address>,
    /// Vault address → intern [`MarketId`] from `Intern::from_registry`.
    /// Bind every interned euler-v2 vault the process may intern from logs
    /// (admitted subscriptions plus registry rows seen via `ProxyCreated`).
    pub interned: Vec<(Address, MarketId)>,
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
    #[error("catalog, first_market, or interned MarketId collide")]
    MarketCollision,
    #[error("market {0:?} is reserved (3481..=3510) or overlaps discovered range")]
    ReservedMarket(MarketId),
    #[error("market {0:?} configured twice")]
    DuplicateMarket(MarketId),
    #[error("address {0} configured twice")]
    DuplicateAddress(Address),
    #[error("asset id {0:?} or underlying configured twice")]
    DuplicateAsset(AssetId),
    #[error("subscribed vault {0} is missing from interned MarketIds")]
    UnboundVault(Address),
    #[error("vaults are configured but interned MarketIds are empty")]
    EmptyInterned,
    #[error("intern has no euler-v2 family")]
    MissingEulerFamily,
    #[error("euler-v2 intern market is not an address")]
    InternNotAddress,
    #[error("toml protocol id does not match intern euler-v2 family")]
    ProtocolMismatch,
    #[error("failed to load {0}")]
    Load(&'static str),
    #[error("protocol toml is malformed")]
    MalformedToml,
}

#[inline]
fn is_foreign(id: MarketId) -> bool {
    id.0 >= FOREIGN_MARKET_MIN && id.0 <= FOREIGN_MARKET_MAX
}

impl Config {
    fn validate_shape(&self) -> core::result::Result<(), ConfigError> {
        if self.factory == Address::ZERO {
            return Err(ConfigError::ZeroFactory);
        }
        if self.evc == Address::ZERO {
            return Err(ConfigError::ZeroEvc);
        }
        if self.catalog == self.first_market {
            return Err(ConfigError::MarketCollision);
        }
        if is_foreign(self.catalog) {
            return Err(ConfigError::ReservedMarket(self.catalog));
        }
        if is_foreign(self.first_market) {
            return Err(ConfigError::ReservedMarket(self.first_market));
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
        let mut markets: Vec<MarketId> = Vec::new();
        let mut intern_addrs: Vec<Address> = Vec::new();
        for (addr, id) in &self.interned {
            if *id == self.catalog || *id == self.first_market {
                return Err(ConfigError::MarketCollision);
            }
            if is_foreign(*id) || id.0 >= self.first_market.0 {
                return Err(ConfigError::ReservedMarket(*id));
            }
            if markets.contains(id) {
                return Err(ConfigError::DuplicateMarket(*id));
            }
            markets.push(*id);
            if intern_addrs.contains(addr) {
                return Err(ConfigError::DuplicateAddress(*addr));
            }
            intern_addrs.push(*addr);
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

    pub fn validate(&self) -> core::result::Result<(), ConfigError> {
        self.validate_shape()?;
        if !self.vaults.is_empty() {
            if self.interned.is_empty() {
                return Err(ConfigError::EmptyInterned);
            }
            for v in &self.vaults {
                if self.interned_id(*v).is_none() {
                    return Err(ConfigError::UnboundVault(*v));
                }
            }
        }
        Ok(())
    }

    /// Replace the intern map. Call after [`Self::from_toml`] with every
    /// euler-v2 `(OnChainId::Addr, MarketRec.id)` from `Intern::from_registry`.
    pub fn bind_interned(
        &mut self,
        markets: impl IntoIterator<Item = (Address, MarketId)>,
    ) -> core::result::Result<(), ConfigError> {
        self.interned.clear();
        for (addr, id) in markets {
            if self.interned.iter().any(|(a, _)| *a == addr) {
                return Err(ConfigError::DuplicateAddress(addr));
            }
            if self.interned.iter().any(|(_, m)| *m == id) {
                return Err(ConfigError::DuplicateMarket(id));
            }
            self.interned.push((addr, id));
        }
        self.validate()
    }

    /// Copy every euler-v2 `(OnChainId::Addr, MarketRec.id)` from `intern`
    /// (admitted and not), then [`Self::validate`].
    pub fn bind_from_intern(&mut self, intern: &Intern) -> core::result::Result<(), ConfigError> {
        let proto = intern
            .protocol("euler-v2")
            .ok_or(ConfigError::MissingEulerFamily)?;
        if proto != self.protocol {
            return Err(ConfigError::ProtocolMismatch);
        }
        let mut markets = Vec::new();
        for m in intern.markets() {
            if m.protocol != proto {
                continue;
            }
            let OnChainId::Addr(addr) = m.key else {
                return Err(ConfigError::InternNotAddress);
            };
            markets.push((addr, m.id));
        }
        self.bind_interned(markets)
    }

    /// Parse committed `euler-v2.toml` and bind intern MarketIds from
    /// `registry/registry.json` under `registry_root`.
    pub fn load(registry_root: &Path) -> core::result::Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(registry_root.join("config/protocols/euler-v2.toml"))
            .map_err(|_| ConfigError::Load("euler-v2.toml"))?;
        let mut cfg = Self::from_toml(&raw)?;
        let intern = Intern::from_registry(
            &Registry::from_path(&registry_root.join("registry/registry.json"))
                .map_err(|_| ConfigError::Load("registry.json"))?,
        )
        .map_err(|_| ConfigError::Load("intern"))?;
        cfg.bind_from_intern(&intern)?;
        Ok(cfg)
    }

    #[inline]
    #[must_use]
    pub fn interned_id(&self, vault: Address) -> Option<MarketId> {
        self.interned
            .iter()
            .find(|(a, _)| *a == vault)
            .map(|(_, id)| *id)
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
    pub(crate) fn is_vault(&self, address: Address) -> bool {
        self.vaults.contains(&address)
    }

    /// Share-token intern: vault ERC-20 listed as `AssetConfig.underlying`.
    #[inline]
    pub(crate) fn token(&self, address: Address) -> Option<&AssetConfig> {
        self.asset_by_underlying(address)
    }

    /// Parse `config/protocols/euler-v2.toml`. `interned` is empty; bind via
    /// [`Self::bind_from_intern`] or [`Self::load`] before [`crate::EulerV2::new`].
    pub fn from_toml(raw: &str) -> core::result::Result<Self, ConfigError> {
        let f: TomlFile = toml::from_str(raw).map_err(|_| ConfigError::MalformedToml)?;
        let mut vaults = Vec::with_capacity(f.vaults.len());
        for v in f.vaults {
            vaults.push(parse_addr(&v)?);
        }
        let mut assets = Vec::with_capacity(f.assets.len());
        for a in f.assets {
            assets.push(AssetConfig {
                underlying: parse_addr(&a.underlying)?,
                asset: AssetId(a.asset),
                feed: FeedId(a.feed),
                decimals: a.decimals,
            });
        }
        let cfg = Self {
            protocol: ProtocolId(f.protocol),
            factory: parse_addr(&f.factory)?,
            evc: parse_addr(&f.evc)?,
            catalog: MarketId(f.catalog),
            first_market: MarketId(f.first_market),
            vaults,
            interned: Vec::new(),
            assets,
            price_sources: Vec::new(),
            pinned_through: f.pinned_through,
        };
        cfg.validate_shape()?;
        Ok(cfg)
    }
}

fn parse_addr(s: &str) -> core::result::Result<Address, ConfigError> {
    s.parse().map_err(|_| ConfigError::MalformedToml)
}

#[derive(serde::Deserialize)]
struct TomlFile {
    protocol: u16,
    factory: String,
    evc: String,
    catalog: u32,
    first_market: u32,
    pinned_through: u64,
    vaults: Vec<String>,
    assets: Vec<TomlAsset>,
}

#[derive(serde::Deserialize)]
struct TomlAsset {
    underlying: String,
    asset: u16,
    feed: u16,
    decimals: u8,
}
