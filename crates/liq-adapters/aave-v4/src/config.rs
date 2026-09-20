//! Adapter configuration: the deployment this adapter instance tracks. Every
//! value is an operator-pinned address or id from the registry
//! (`REGISTRY.md`, WP 06A-1) — the adapter never discovers a contract from a
//! log it did not subscribe to, and never guesses an asset id or a feed.

use alloy_primitives::Address;
use liq_protocol::{BlockNum, FeedId};
use liq_types::{AssetId, MarketId, ProtocolId};

use crate::layout::SpokeFlags;

/// One Hub proxy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubConfig {
    pub address: Address,
    /// The hub's own market: slot `s` is hub asset `s`.
    pub market: MarketId,
}

/// One Spoke proxy with its `AaveOracle` (`Spoke.ORACLE`, immutable).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpokeConfig {
    pub address: Address,
    /// The spoke's market: slot `0` is the spoke meta row, slot `r + 1` is
    /// reserve `r`.
    pub market: MarketId,
    pub oracle: Address,
}

/// One underlying token the registry knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetConfig {
    pub underlying: Address,
    /// Global id (GUIDE 00 §3).
    pub asset: AssetId,
    /// Feed the engine prices `asset` with.
    pub feed: FeedId,
}

/// The registry's pin of a reserve's price source (`AaveOracle._sources
/// [reserveId]`). A source the chain reports that differs from this — or a
/// reserve with no pin — is unpriced (fail closed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePin {
    pub spoke: Address,
    pub reserve_id: u16,
    pub source: Address,
}

/// Everything [`crate::AaveV4`] needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub protocol: ProtocolId,
    pub hubs: Vec<HubConfig>,
    /// At most [`SpokeFlags::MAX_SPOKES`]; the index into this list is the
    /// spoke's byte in every hub row.
    pub spokes: Vec<SpokeConfig>,
    pub assets: Vec<AssetConfig>,
    pub price_sources: Vec<SourcePin>,
    /// Block through which the deployed bytecode was verified against the
    /// pinned commit (`docs/coverage/aave-v4.md` header). Halt-class logs at
    /// or before it are the audited deployment itself (proxy creation,
    /// initializer, authority set-up) and fold to `DirtySet::None`; any
    /// later one is `Err(HaltSignal)`.
    pub pinned_through: BlockNum,
}

/// Which tracked contract emitted a log.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Emitter {
    Hub(MarketId),
    /// `(spoke market, index into `Config::spokes`)`.
    Spoke(MarketId, usize),
    /// The oracle of the spoke at this index.
    Oracle(usize),
}

/// A [`Config`] the adapter refuses to run with.
#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("no hub configured")]
    NoHubs,
    #[error("no spoke configured")]
    NoSpokes,
    #[error("{0} spokes exceed the hub-row budget of {max}", max = SpokeFlags::MAX_SPOKES)]
    TooManySpokes(usize),
    #[error("address {0} configured twice")]
    DuplicateAddress(Address),
    #[error("market {0:?} configured twice")]
    DuplicateMarket(MarketId),
    #[error("spoke {0} has the zero address as oracle")]
    ZeroOracle(Address),
    #[error("asset id {0:?} or underlying configured twice")]
    DuplicateAsset(AssetId),
}

impl Config {
    /// Validates the shape: at least one hub and spoke, no duplicate
    /// addresses, market ids or assets, spokes within the hub-row budget.
    pub fn validate(&self) -> core::result::Result<(), ConfigError> {
        if self.hubs.is_empty() {
            return Err(ConfigError::NoHubs);
        }
        if self.spokes.is_empty() {
            return Err(ConfigError::NoSpokes);
        }
        if self.spokes.len() > SpokeFlags::MAX_SPOKES {
            return Err(ConfigError::TooManySpokes(self.spokes.len()));
        }
        let mut markets: Vec<MarketId> = Vec::new();
        let mut addrs: Vec<Address> = Vec::new();
        for (market, address) in self
            .hubs
            .iter()
            .map(|h| (h.market, h.address))
            .chain(self.spokes.iter().map(|s| (s.market, s.address)))
        {
            if markets.contains(&market) {
                return Err(ConfigError::DuplicateMarket(market));
            }
            if addrs.contains(&address) {
                return Err(ConfigError::DuplicateAddress(address));
            }
            markets.push(market);
            addrs.push(address);
        }
        for s in &self.spokes {
            if s.oracle == Address::ZERO {
                return Err(ConfigError::ZeroOracle(s.address));
            }
            if addrs.contains(&s.oracle) {
                return Err(ConfigError::DuplicateAddress(s.oracle));
            }
            addrs.push(s.oracle);
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

    /// Who emitted a log at `address`, if tracked.
    #[inline]
    pub(crate) fn emitter(&self, address: Address) -> Option<Emitter> {
        if let Some(h) = self.hubs.iter().find(|h| h.address == address) {
            return Some(Emitter::Hub(h.market));
        }
        if let Some((i, s)) = self
            .spokes
            .iter()
            .enumerate()
            .find(|(_, s)| s.address == address)
        {
            return Some(Emitter::Spoke(s.market, i));
        }
        self.spokes
            .iter()
            .position(|s| s.oracle == address)
            .map(Emitter::Oracle)
    }

    #[inline]
    pub(crate) fn hub_market(&self, address: Address) -> Option<MarketId> {
        self.hubs
            .iter()
            .find(|h| h.address == address)
            .map(|h| h.market)
    }

    #[inline]
    pub(crate) fn spoke_index(&self, address: Address) -> Option<usize> {
        self.spokes.iter().position(|s| s.address == address)
    }

    #[inline]
    pub(crate) fn spoke_by_market(&self, market: MarketId) -> Option<&SpokeConfig> {
        self.spokes.iter().find(|s| s.market == market)
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

    /// The pinned source for `(spoke, reserve)`, if any.
    #[inline]
    pub(crate) fn pinned_source(&self, spoke: Address, reserve_id: u16) -> Option<Address> {
        self.price_sources
            .iter()
            .find(|p| p.spoke == spoke && p.reserve_id == reserve_id)
            .map(|p| p.source)
    }
}
