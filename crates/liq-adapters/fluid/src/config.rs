//! Deployment pin: VaultFactory + interned tokens. Vaults are discovered from
//! `VaultDeployed`, never a hand list of 182. `D15_VAULTS` is a cardinality
//! check on `totalVaults()` at the pin block.
//!
//! # MarketId allocator (intern-global)
//!
//! Fluid is not in `registry.json`. ProtocolId **10**. Markets **4000..=4199**:
//! catalog = 4000; vault `vaultId` → `4000 + vaultId` (vault 1 → 4001).
//! Never intern 0..=3480, never 3481..=3999, never 4200+ (Gearbox).
//!
//! Boot: [`Config::from_toml`] → [`Config::assert_live_factory`] →
//! [`crate::Fluid::new`]. `new` returns [`ConfigError::LiveFactoryUnasserted`]
//! unless the assert succeeded.

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;
use liq_protocol::{BlockNum, FeedId};
use liq_types::{AssetId, MarketId, ProtocolId};

use crate::events::views::{getVaultAddressCall, totalVaultsCall};
use crate::layout::{
    CATALOG_MARKET, FIRST_VAULT_MARKET, LAST_VAULT_MARKET, VAULT_T1, VAULT_T2, VAULT_T3, VAULT_T4,
};
use crate::math::NATIVE_TOKEN;

/// Synchronous `eth_call` at a block. Boot-only; not on the hot path.
pub trait FactoryRpc {
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

/// Optional pin of one vault's `constantsView` / `TYPE` (tests + boot eth_call).
/// Not the live universe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultPin {
    pub vault: Address,
    pub vault_id: u32,
    pub vault_type: u32,
    pub supply0: Address,
    pub supply1: Address,
    pub borrow0: Address,
    pub borrow1: Address,
    pub supply_decimals0: u8,
    pub supply_decimals1: u8,
    pub borrow_decimals0: u8,
    pub borrow_decimals1: u8,
    /// Packed 3-decimal threshold (`900` = 90%).
    pub liq_threshold: u16,
    /// Packed 3-decimal max limit.
    pub liq_max_limit: u16,
    /// Packed 4-decimal penalty (`100` = 1%).
    pub liq_penalty: u16,
    pub oracle: Address,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub protocol: ProtocolId,
    pub factory: Address,
    pub catalog: MarketId,
    pub first_market: MarketId,
    /// D15 cardinality; live `totalVaults()` at pin must equal this.
    pub d15_vaults: u32,
    /// Subscription addresses only — not the discovered universe.
    pub vaults: Vec<Address>,
    pub vault_pins: Vec<VaultPin>,
    pub assets: Vec<AssetConfig>,
    pub pinned_through: BlockNum,
    /// False until [`Self::assert_live_factory`] succeeds.
    pub live_factory_asserted: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Emitter {
    Factory,
    Vault(MarketId),
    Pending,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("fluid factory is the zero address")]
    ZeroFactory,
    #[error("catalog/first_market outside Fluid 4000..=4199")]
    MarketRange,
    #[error("market {0:?} configured twice")]
    DuplicateMarket(MarketId),
    #[error("address {0} configured twice")]
    DuplicateAddress(Address),
    #[error("asset id {0:?} or underlying configured twice")]
    DuplicateAsset(AssetId),
    #[error("vault pin type is not T1–T4")]
    BadVaultType,
    #[error("native token must not be interned as a priced asset")]
    NativeAliasedToWeth,
    #[error("live factory was not asserted")]
    LiveFactoryUnasserted,
    #[error("factory eth_call failed at {0}")]
    FactoryCall(Address),
    #[error("live totalVaults {found} != d15_vaults {expected}")]
    VaultCountMismatch { expected: u32, found: u32 },
    #[error("getVaultAddress(1) is zero")]
    ZeroVaultAddress,
    #[error("protocol toml is malformed")]
    MalformedToml,
}

impl Config {
    fn validate_shape(&self) -> core::result::Result<(), ConfigError> {
        if self.factory == Address::ZERO {
            return Err(ConfigError::ZeroFactory);
        }
        if self.catalog != CATALOG_MARKET || self.first_market != FIRST_VAULT_MARKET {
            return Err(ConfigError::MarketRange);
        }
        if self.protocol != ProtocolId(10) {
            return Err(ConfigError::MalformedToml);
        }
        let mut addrs: Vec<Address> = vec![self.factory];
        for v in &self.vaults {
            if *v == Address::ZERO || addrs.contains(v) {
                return Err(ConfigError::DuplicateAddress(*v));
            }
            addrs.push(*v);
        }
        let mut pins: Vec<Address> = Vec::new();
        let mut ids: Vec<u32> = Vec::new();
        for p in &self.vault_pins {
            if p.vault == Address::ZERO {
                return Err(ConfigError::DuplicateAddress(Address::ZERO));
            }
            if pins.contains(&p.vault) {
                return Err(ConfigError::DuplicateAddress(p.vault));
            }
            pins.push(p.vault);
            if ids.contains(&p.vault_id) {
                return Err(ConfigError::DuplicateMarket(MarketId(
                    CATALOG_MARKET.0.saturating_add(p.vault_id),
                )));
            }
            ids.push(p.vault_id);
            if p.vault_type != VAULT_T1
                && p.vault_type != VAULT_T2
                && p.vault_type != VAULT_T3
                && p.vault_type != VAULT_T4
            {
                return Err(ConfigError::BadVaultType);
            }
            if p.vault_id == 0 || CATALOG_MARKET.0.saturating_add(p.vault_id) > LAST_VAULT_MARKET.0
            {
                return Err(ConfigError::MarketRange);
            }
        }
        for (i, a) in self.assets.iter().enumerate() {
            if a.underlying == NATIVE_TOKEN {
                return Err(ConfigError::NativeAliasedToWeth);
            }
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
        self.validate_shape()
    }

    /// `eth_call` factory `totalVaults()` and `getVaultAddress(1)` at `block`.
    pub fn assert_live_factory<R: FactoryRpc>(
        &mut self,
        provider: &R,
        block: BlockNum,
    ) -> core::result::Result<(), ConfigError> {
        self.live_factory_asserted = false;
        self.validate()?;
        let tv = provider.eth_call(self.factory, &totalVaultsCall {}.abi_encode(), block)?;
        let total = decode_u256(&tv).ok_or(ConfigError::FactoryCall(self.factory))?;
        let found = u32::try_from(total).map_err(|_| ConfigError::FactoryCall(self.factory))?;
        if found != self.d15_vaults {
            return Err(ConfigError::VaultCountMismatch {
                expected: self.d15_vaults,
                found,
            });
        }
        let ga = provider.eth_call(
            self.factory,
            &getVaultAddressCall {
                vaultId: U256::from(1u8),
            }
            .abi_encode(),
            block,
        )?;
        let addr = decode_addr(&ga).ok_or(ConfigError::FactoryCall(self.factory))?;
        if addr == Address::ZERO {
            return Err(ConfigError::ZeroVaultAddress);
        }
        self.live_factory_asserted = true;
        Ok(())
    }

    #[inline]
    pub(crate) fn pin_of(&self, vault: Address) -> Option<&VaultPin> {
        self.vault_pins.iter().find(|p| p.vault == vault)
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
    pub(crate) fn is_vault(&self, address: Address) -> bool {
        self.vaults.contains(&address) || self.vault_pins.iter().any(|p| p.vault == address)
    }

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
            catalog: MarketId(f.catalog),
            first_market: MarketId(f.first_market),
            d15_vaults: f.d15_vaults,
            vaults,
            vault_pins: Vec::new(),
            assets,
            pinned_through: f.pinned_through,
            live_factory_asserted: false,
        };
        cfg.validate_shape()?;
        Ok(cfg)
    }
}

fn parse_addr(s: &str) -> core::result::Result<Address, ConfigError> {
    s.parse().map_err(|_| ConfigError::MalformedToml)
}

fn decode_u256(raw: &Bytes) -> Option<U256> {
    let w = raw.get(..32)?;
    Some(U256::from_be_slice(w))
}

fn decode_addr(raw: &Bytes) -> Option<Address> {
    let w = raw.get(..32)?;
    let a = w.get(12..32)?;
    Address::try_from(a).ok()
}

#[derive(serde::Deserialize)]
struct TomlFile {
    protocol: u16,
    factory: String,
    catalog: u32,
    first_market: u32,
    d15_vaults: u32,
    pinned_through: u64,
    #[serde(default)]
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
