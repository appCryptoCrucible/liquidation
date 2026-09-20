//! Deployment pin: one config row per collateral branch. MCR/CCR/penalties
//! are the values read from that branch's `AddressesRegistry` at boot — not
//! the WETH/SETH table in `Constants.sol` (those are deploy-script inputs at
//! `c8a5a4ee`). `validate` asserts them against the Constants.sol *bounds*.

use alloy_primitives::{Address, U256};
use liq_protocol::{BlockNum, FeedId};
use liq_types::{AssetId, MarketId, ProtocolId};

use crate::math::{MAX_LIQUIDATION_PENALTY_REDISTRIBUTION, MIN_LIQUIDATION_PENALTY_SP};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetConfig {
    pub underlying: Address,
    pub asset: AssetId,
    pub feed: FeedId,
    pub decimals: u8,
}

/// One BOLD collateral branch (`1.json` `branches[i]`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchConfig {
    pub coll_symbol: String,
    pub market: MarketId,
    pub trove_manager: Address,
    pub borrower_operations: Address,
    pub stability_pool: Address,
    pub sorted_troves: Address,
    pub addresses_registry: Address,
    pub price_feed: Address,
    pub coll_token: Address,
    pub coll_asset: AssetId,
    pub coll_decimals: u8,
    pub coll_feed: FeedId,
    /// `AddressesRegistry.MCR()` at boot (WAD).
    pub mcr: u128,
    /// `AddressesRegistry.CCR()` at boot (WAD).
    pub ccr: u128,
    /// `AddressesRegistry.LIQUIDATION_PENALTY_SP()` at boot (WAD).
    pub penalty_sp: u128,
    /// `AddressesRegistry.LIQUIDATION_PENALTY_REDISTRIBUTION()` at boot (WAD).
    pub penalty_redist: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub protocol: ProtocolId,
    pub bold: AssetConfig,
    pub weth: AssetConfig,
    pub branches: Vec<BranchConfig>,
    pub pinned_through: BlockNum,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Emitter {
    TroveManager(usize),
    StabilityPool(usize),
    BorrowerOperations(usize),
    PriceFeed(usize),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("no collateral branch configured")]
    NoBranches,
    #[error("bold or weth is the zero address")]
    ZeroToken,
    #[error("branch {0} has a zero system address")]
    ZeroBranch(Address),
    #[error("address {0} configured twice")]
    DuplicateAddress(Address),
    #[error("market {0:?} configured twice")]
    DuplicateMarket(MarketId),
    #[error("asset id {0:?} or underlying configured twice")]
    DuplicateAsset(AssetId),
    #[error("AddressesRegistry penalty/MCR/CCR out of Constants.sol bounds")]
    RegistryBounds,
    #[error("protocol toml is malformed")]
    MalformedToml,
}

impl Config {
    pub fn validate(&self) -> core::result::Result<(), ConfigError> {
        if self.branches.is_empty() {
            return Err(ConfigError::NoBranches);
        }
        if self.bold.underlying == Address::ZERO || self.weth.underlying == Address::ZERO {
            return Err(ConfigError::ZeroToken);
        }
        if self.bold.asset == self.weth.asset || self.bold.underlying == self.weth.underlying {
            return Err(ConfigError::DuplicateAsset(self.bold.asset));
        }
        let mut markets: Vec<MarketId> = Vec::new();
        let mut addrs: Vec<Address> = Vec::new();
        for b in &self.branches {
            if markets.contains(&b.market) {
                return Err(ConfigError::DuplicateMarket(b.market));
            }
            markets.push(b.market);
            for a in [
                b.trove_manager,
                b.borrower_operations,
                b.stability_pool,
                b.sorted_troves,
                b.addresses_registry,
                b.price_feed,
                b.coll_token,
            ] {
                if a == Address::ZERO {
                    return Err(ConfigError::ZeroBranch(b.trove_manager));
                }
                if addrs.contains(&a) {
                    return Err(ConfigError::DuplicateAddress(a));
                }
                addrs.push(a);
            }
            if b.coll_token != self.weth.underlying && b.coll_asset == self.weth.asset {
                return Err(ConfigError::DuplicateAsset(b.coll_asset));
            }
            if b.coll_asset == self.bold.asset {
                return Err(ConfigError::DuplicateAsset(b.coll_asset));
            }
            let mcr = U256::from(b.mcr);
            let ccr = U256::from(b.ccr);
            let sp = U256::from(b.penalty_sp);
            let red = U256::from(b.penalty_redist);
            if mcr.is_zero() || ccr < mcr {
                return Err(ConfigError::RegistryBounds);
            }
            if sp < MIN_LIQUIDATION_PENALTY_SP || red > MAX_LIQUIDATION_PENALTY_REDISTRIBUTION {
                return Err(ConfigError::RegistryBounds);
            }
            if red < sp {
                return Err(ConfigError::RegistryBounds);
            }
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn branch_by_market(&self, market: MarketId) -> Option<&BranchConfig> {
        self.branches.iter().find(|b| b.market == market)
    }

    #[inline]
    pub(crate) fn emitter(&self, addr: Address) -> Option<Emitter> {
        for (i, b) in self.branches.iter().enumerate() {
            if addr == b.trove_manager {
                return Some(Emitter::TroveManager(i));
            }
            if addr == b.stability_pool {
                return Some(Emitter::StabilityPool(i));
            }
            if addr == b.borrower_operations {
                return Some(Emitter::BorrowerOperations(i));
            }
            if addr == b.price_feed {
                return Some(Emitter::PriceFeed(i));
            }
        }
        None
    }

    #[inline]
    pub(crate) fn underlying_of(&self, asset: AssetId) -> Option<Address> {
        if asset == self.bold.asset {
            return Some(self.bold.underlying);
        }
        if asset == self.weth.asset {
            return Some(self.weth.underlying);
        }
        self.branches
            .iter()
            .find(|b| b.coll_asset == asset)
            .map(|b| b.coll_token)
    }

    /// Parse `config/protocols/liquity-v2.toml`. MCR/CCR/penalties in the file
    /// are the boot-time `AddressesRegistry` reads, not runtime Constants.sol
    /// table lookups.
    pub fn from_toml(raw: &str) -> core::result::Result<Self, ConfigError> {
        let f: TomlFile = toml::from_str(raw).map_err(|_| ConfigError::MalformedToml)?;
        let mut branches = Vec::with_capacity(f.branches.len());
        for b in f.branches {
            branches.push(BranchConfig {
                coll_symbol: b.coll_symbol,
                market: MarketId(b.market),
                trove_manager: parse_addr(&b.trove_manager)?,
                borrower_operations: parse_addr(&b.borrower_operations)?,
                stability_pool: parse_addr(&b.stability_pool)?,
                sorted_troves: parse_addr(&b.sorted_troves)?,
                addresses_registry: parse_addr(&b.addresses_registry)?,
                price_feed: parse_addr(&b.price_feed)?,
                coll_token: parse_addr(&b.coll_token)?,
                coll_asset: AssetId(b.coll_asset),
                coll_decimals: b.coll_decimals,
                coll_feed: FeedId(b.coll_feed),
                mcr: u128::from(b.mcr),
                ccr: u128::from(b.ccr),
                penalty_sp: u128::from(b.penalty_sp),
                penalty_redist: u128::from(b.penalty_redist),
            });
        }
        let cfg = Self {
            protocol: ProtocolId(f.protocol),
            bold: parse_asset(f.bold)?,
            weth: parse_asset(f.weth)?,
            branches,
            pinned_through: f.pinned_through,
        };
        cfg.validate()?;
        Ok(cfg)
    }
}

fn parse_addr(s: &str) -> core::result::Result<Address, ConfigError> {
    s.parse().map_err(|_| ConfigError::MalformedToml)
}

fn parse_asset(a: TomlAsset) -> core::result::Result<AssetConfig, ConfigError> {
    Ok(AssetConfig {
        underlying: parse_addr(&a.underlying)?,
        asset: AssetId(a.asset),
        feed: FeedId(a.feed),
        decimals: a.decimals,
    })
}

#[derive(serde::Deserialize)]
struct TomlFile {
    protocol: u16,
    pinned_through: u64,
    bold: TomlAsset,
    weth: TomlAsset,
    branches: Vec<TomlBranch>,
}

#[derive(serde::Deserialize)]
struct TomlAsset {
    underlying: String,
    asset: u16,
    feed: u16,
    decimals: u8,
}

#[derive(serde::Deserialize)]
struct TomlBranch {
    coll_symbol: String,
    market: u32,
    trove_manager: String,
    borrower_operations: String,
    stability_pool: String,
    sorted_troves: String,
    addresses_registry: String,
    price_feed: String,
    coll_token: String,
    coll_asset: u16,
    coll_decimals: u8,
    coll_feed: u16,
    mcr: u64,
    ccr: u64,
    penalty_sp: u64,
    penalty_redist: u64,
}
