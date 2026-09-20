//! Deployment pin: one config row per collateral branch.
//!
//! MCR/CCR/penalties in toml are the pin deploy-script numbers written into
//! each branch `AddressesRegistry` as immutables (`liquity/bold` @ `c8a5a4ee`).
//! [`Config::validate`] checks Constants.sol *bounds* only.
//! Boot is fail-closed: [`Config::from_toml`] → [`Config::assert_live_registry`]
//! → [`crate::LiquityV2::new`]. `new` returns
//! [`ConfigError::LiveRegistryUnasserted`] unless `assert_live_registry`
//! succeeded on that config (the flag starts false; only a successful assert
//! sets it true). There is no unasserted constructor.

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{sol, SolCall};
use liq_protocol::{BlockNum, FeedId};
use liq_types::{AssetId, MarketId, ProtocolId};

use crate::math::{MAX_LIQUIDATION_PENALTY_REDISTRIBUTION, MIN_LIQUIDATION_PENALTY_SP};

sol! {
    interface IAddressesRegistry {
        function MCR() external view returns (uint256);
        function CCR() external view returns (uint256);
        function LIQUIDATION_PENALTY_SP() external view returns (uint256);
        function LIQUIDATION_PENALTY_REDISTRIBUTION() external view returns (uint256);
    }
}

pub use IAddressesRegistry::{
    CCRCall, LIQUIDATION_PENALTY_REDISTRIBUTIONCall, LIQUIDATION_PENALTY_SPCall, MCRCall,
};

/// Synchronous `eth_call` at a block. Boot-only; not on the hot path.
///
/// [`Config::assert_live_registry`] is the only setter of
/// [`Config::live_registry_asserted`]. Tests use a pin-view double; the
/// ignored `live_addresses_registry_matches_toml` test uses `LIQ_RPC_URL`.
pub trait RegistryRpc {
    fn eth_call(
        &self,
        to: Address,
        data: &[u8],
        block: BlockNum,
    ) -> core::result::Result<Bytes, ConfigError>;
}

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
    /// Pin `AddressesRegistry.MCR()` (WAD). Live value is asserted by
    /// [`Config::assert_live_registry`].
    pub mcr: u128,
    /// Pin `AddressesRegistry.CCR()` (WAD).
    pub ccr: u128,
    /// Pin `AddressesRegistry.LIQUIDATION_PENALTY_SP()` (WAD).
    pub penalty_sp: u128,
    /// Pin `AddressesRegistry.LIQUIDATION_PENALTY_REDISTRIBUTION()` (WAD).
    pub penalty_redist: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub protocol: ProtocolId,
    pub bold: AssetConfig,
    pub weth: AssetConfig,
    pub branches: Vec<BranchConfig>,
    pub pinned_through: BlockNum,
    /// False until [`Self::assert_live_registry`] succeeds. [`crate::LiquityV2::new`]
    /// returns [`ConfigError::LiveRegistryUnasserted`] while this is false.
    pub live_registry_asserted: bool,
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
    #[error(
        "live AddressesRegistry.{field} at {registry} != toml (expected {expected}, found {found})"
    )]
    RegistryMismatch {
        registry: Address,
        field: &'static str,
        expected: U256,
        found: U256,
    },
    #[error("AddressesRegistry eth_call failed at {0}")]
    RegistryCall(Address),
    #[error("live AddressesRegistry was not asserted")]
    LiveRegistryUnasserted,
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

    /// `eth_call` each branch `AddressesRegistry` immutable at `block`.
    /// Mismatch or RPC/decode failure → `Err` and
    /// [`Self::live_registry_asserted`] stays false. Bounds are still
    /// [`validate`]. Success is the only path that sets the flag true.
    pub fn assert_live_registry<R: RegistryRpc>(
        &mut self,
        provider: &R,
        block: BlockNum,
    ) -> core::result::Result<(), ConfigError> {
        self.live_registry_asserted = false;
        self.validate()?;
        for b in &self.branches {
            let reg = b.addresses_registry;
            check_view(
                provider,
                reg,
                block,
                &IAddressesRegistry::MCRCall {}.abi_encode(),
                "MCR",
                U256::from(b.mcr),
            )?;
            check_view(
                provider,
                reg,
                block,
                &IAddressesRegistry::CCRCall {}.abi_encode(),
                "CCR",
                U256::from(b.ccr),
            )?;
            check_view(
                provider,
                reg,
                block,
                &IAddressesRegistry::LIQUIDATION_PENALTY_SPCall {}.abi_encode(),
                "LIQUIDATION_PENALTY_SP",
                U256::from(b.penalty_sp),
            )?;
            check_view(
                provider,
                reg,
                block,
                &IAddressesRegistry::LIQUIDATION_PENALTY_REDISTRIBUTIONCall {}.abi_encode(),
                "LIQUIDATION_PENALTY_REDISTRIBUTION",
                U256::from(b.penalty_redist),
            )?;
        }
        self.live_registry_asserted = true;
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

    /// Parse `config/protocols/liquity-v2.toml`. MCR/CCR/penalties are the
    /// pin deploy-script numbers baked into `AddressesRegistry` immutables.
    /// [`validate`] checks Constants.sol bounds. The live-assert flag is
    /// false; [`crate::LiquityV2::new`] refuses this config until
    /// [`Self::assert_live_registry`] succeeds.
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
            live_registry_asserted: false,
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

fn check_view<R: RegistryRpc>(
    rpc: &R,
    registry: Address,
    block: BlockNum,
    data: &[u8],
    field: &'static str,
    expected: U256,
) -> core::result::Result<(), ConfigError> {
    let raw = rpc.eth_call(registry, data, block)?;
    if raw.len() < 32 {
        return Err(ConfigError::RegistryCall(registry));
    }
    let found = U256::from_be_slice(raw.get(..32).ok_or(ConfigError::RegistryCall(registry))?);
    if found != expected {
        return Err(ConfigError::RegistryMismatch {
            registry,
            field,
            expected,
            found,
        });
    }
    Ok(())
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
