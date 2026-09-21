//! Deployment pin: one TOML family, intern-bound Comptroller `MarketId`s.
//!
//! `Intern::from_registry` assigns `MarketId(i)` in `registry.protocols`
//! BTreeMap key order. Compound V2 occupies **295..=560** (266 rows).
//! Official Unitroller `0x3d9819210A31b4961b30EF54bE2aeD79B9c9Cd3B` is
//! **MarketId 355**. cTokens are not intern markets.
//!
//! [`Config::from_toml`] leaves `interned` empty and
//! `live_registry_asserted` false. [`crate::CompoundV2::new`] refuses both.
//! Production: `from_toml` → [`Config::bind_from_intern`] →
//! [`Config::assert_live_registry`] → `new`. Close factor / incentive are
//! per-comptroller admin storage (pin bounds only). They are read live
//! (`eth_call`) and folded from `NewCloseFactor` / `NewLiquidationIncentive`.
//! Do not invent `1.08`.

use std::path::Path;

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;
use liq_config::{Intern, OnChainId, Registry};
use liq_protocol::{BlockNum, FeedId};
use liq_types::{AssetId, MarketId, ProtocolId};

use crate::events::views::{
    closeFactorMantissaCall, liquidationIncentiveMantissaCall, oracleCall, underlyingCall,
};
use crate::math::close_factor_in_pin_bounds;

/// Official Compound Unitroller (intern key).
pub const OFFICIAL_UNITROLLER: Address =
    alloy_primitives::address!("0x3d9819210A31b4961b30EF54bE2aeD79B9c9Cd3B");
/// Intern `MarketId` of [`OFFICIAL_UNITROLLER`].
pub const OFFICIAL_MARKET: MarketId = MarketId(355);
/// Inclusive intern range for family `compound-v2`.
pub const INTERN_MARKET_MIN: u32 = 295;
pub const INTERN_MARKET_MAX: u32 = 560;
/// ProtocolId of intern family `compound-v2` (sorted families).
pub const FAMILY_PROTOCOL: ProtocolId = ProtocolId(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetConfig {
    pub underlying: Address,
    pub asset: AssetId,
    pub feed: FeedId,
    pub decimals: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CTokenPin {
    pub ctoken: Address,
    /// `Address::ZERO` = CEther (no ERC-20 `underlying`). Not a symbol.
    pub underlying: Address,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkConfig {
    pub comptroller: Address,
    /// Admin file. Zero until [`Config::assert_live_registry`] or a TOML pin
    /// that live-assert compared. Never a shared invented pair.
    pub close_factor_mantissa: u128,
    pub liquidation_incentive_mantissa: u128,
    pub oracle: Address,
    /// Price/flash key for CEther (ETH has no ERC-20). Typically WETH.
    pub native: Address,
    pub ctokens: Vec<CTokenPin>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub protocol: ProtocolId,
    pub forks: Vec<ForkConfig>,
    /// Comptroller address → intern [`MarketId`]. Empty after `from_toml`.
    pub interned: Vec<(Address, MarketId)>,
    pub assets: Vec<AssetConfig>,
    pub pinned_through: BlockNum,
    /// False until [`Self::assert_live_registry`] succeeds.
    pub live_registry_asserted: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Emitter {
    Comptroller(MarketId),
    CToken { market: MarketId, slot: u16 },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("no compound-v2 fork configured")]
    NoForks,
    #[error("official Unitroller is missing from toml")]
    OfficialForkRequired,
    #[error("comptroller {0} is the zero address")]
    ZeroComptroller(Address),
    #[error("address {0} configured twice")]
    DuplicateAddress(Address),
    #[error("market {0:?} configured twice")]
    DuplicateMarket(MarketId),
    #[error("asset id {0:?} or underlying configured twice")]
    DuplicateAsset(AssetId),
    #[error("closeFactorMantissa outside pin 0.05e18..0.9e18")]
    CloseFactorBounds,
    #[error("liquidationIncentiveMantissa is zero")]
    ZeroIncentive,
    #[error("forks are configured but interned MarketIds are empty")]
    EmptyInterned,
    #[error("comptroller {0} is missing from interned MarketIds")]
    UnboundFork(Address),
    #[error("intern has no compound-v2 family")]
    MissingFamily,
    #[error("compound-v2 intern market is not an address")]
    InternNotAddress,
    #[error("toml protocol id does not match intern compound-v2 family")]
    ProtocolMismatch,
    #[error("official Unitroller intern id is not 355")]
    OfficialMarketMismatch,
    #[error("market {0:?} is outside intern 295..=560")]
    ReservedMarket(MarketId),
    #[error("live closeFactor/incentive/oracle was not asserted")]
    LiveRegistryUnasserted,
    #[error(
        "live Comptroller.{field} at {comptroller} != toml (expected {expected}, found {found})"
    )]
    RegistryMismatch {
        comptroller: Address,
        field: &'static str,
        expected: U256,
        found: U256,
    },
    #[error("Comptroller eth_call failed at {0}")]
    RegistryCall(Address),
    #[error("cToken {0} underlying() does not match toml pin")]
    UnderlyingMismatch(Address),
    #[error("CEther cToken {0} returned an ERC-20 underlying()")]
    CetherHasUnderlying(Address),
    #[error("failed to load {0}")]
    Load(&'static str),
    #[error("protocol toml is malformed")]
    MalformedToml,
}

/// Synchronous `eth_call` at a block. Boot-only; not on the hot path.
pub trait RegistryRpc {
    fn eth_call(
        &self,
        to: Address,
        data: &[u8],
        block: BlockNum,
    ) -> core::result::Result<Bytes, ConfigError>;
}

impl Config {
    fn validate_shape(&self) -> core::result::Result<(), ConfigError> {
        if self.forks.is_empty() {
            return Err(ConfigError::NoForks);
        }
        if self.protocol != FAMILY_PROTOCOL {
            return Err(ConfigError::ProtocolMismatch);
        }
        let mut addrs: Vec<Address> = Vec::new();
        for f in &self.forks {
            if f.comptroller == Address::ZERO {
                return Err(ConfigError::ZeroComptroller(f.comptroller));
            }
            if addrs.contains(&f.comptroller) {
                return Err(ConfigError::DuplicateAddress(f.comptroller));
            }
            addrs.push(f.comptroller);
            if f.oracle != Address::ZERO {
                if addrs.contains(&f.oracle) {
                    return Err(ConfigError::DuplicateAddress(f.oracle));
                }
                addrs.push(f.oracle);
            }
            for c in &f.ctokens {
                if c.ctoken == Address::ZERO {
                    return Err(ConfigError::ZeroComptroller(c.ctoken));
                }
                if addrs.contains(&c.ctoken) {
                    return Err(ConfigError::DuplicateAddress(c.ctoken));
                }
                addrs.push(c.ctoken);
            }
            if f.close_factor_mantissa != 0
                && !close_factor_in_pin_bounds(U256::from(f.close_factor_mantissa))
            {
                return Err(ConfigError::CloseFactorBounds);
            }
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
        let mut markets: Vec<MarketId> = Vec::new();
        let mut intern_addrs: Vec<Address> = Vec::new();
        for (addr, id) in &self.interned {
            if id.0 < INTERN_MARKET_MIN || id.0 > INTERN_MARKET_MAX {
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
        if let Some(id) = self.interned_id(OFFICIAL_UNITROLLER) {
            if id != OFFICIAL_MARKET {
                return Err(ConfigError::OfficialMarketMismatch);
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> core::result::Result<(), ConfigError> {
        self.validate_shape()?;
        if self.interned.is_empty() {
            return Err(ConfigError::EmptyInterned);
        }
        for f in &self.forks {
            if self.interned_id(f.comptroller).is_none() {
                return Err(ConfigError::UnboundFork(f.comptroller));
            }
        }
        Ok(())
    }

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

    pub fn bind_from_intern(&mut self, intern: &Intern) -> core::result::Result<(), ConfigError> {
        let proto = intern
            .protocol("compound-v2")
            .ok_or(ConfigError::MissingFamily)?;
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
        self.bind_interned(markets)?;
        self.assets.clear();
        let mut seen: Vec<Address> = Vec::new();
        for f in &self.forks {
            if f.native != Address::ZERO {
                push_asset(&mut self.assets, &mut seen, intern, f.native)?;
            }
            for c in &f.ctokens {
                if c.underlying != Address::ZERO {
                    push_asset(&mut self.assets, &mut seen, intern, c.underlying)?;
                }
            }
        }
        self.validate()
    }

    pub fn load(registry_root: &Path) -> core::result::Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(registry_root.join("config/protocols/compound-v2.toml"))
            .map_err(|_| ConfigError::Load("compound-v2.toml"))?;
        let mut cfg = Self::from_toml(&raw)?;
        let intern = Intern::from_registry(
            &Registry::from_path(&registry_root.join("registry/registry.json"))
                .map_err(|_| ConfigError::Load("registry.json"))?,
        )
        .map_err(|_| ConfigError::Load("intern"))?;
        cfg.bind_from_intern(&intern)?;
        Ok(cfg)
    }

    /// `eth_call` each TOML fork's `closeFactorMantissa`,
    /// `liquidationIncentiveMantissa`, `oracle`, and each CErc20
    /// `underlying()`. CEther must not return an ERC-20 underlying.
    /// A non-zero TOML pin must match; a zero TOML pin is filled from chain.
    /// Success is the only path that sets [`Self::live_registry_asserted`].
    pub fn assert_live_registry<R: RegistryRpc>(
        &mut self,
        provider: &R,
        block: BlockNum,
    ) -> core::result::Result<(), ConfigError> {
        self.live_registry_asserted = false;
        self.validate()?;
        for f in &mut self.forks {
            let to = f.comptroller;
            let cf = call_u256(
                provider,
                to,
                block,
                &closeFactorMantissaCall {}.abi_encode(),
            )?;
            let li = call_u256(
                provider,
                to,
                block,
                &liquidationIncentiveMantissaCall {}.abi_encode(),
            )?;
            let oracle = call_addr(provider, to, block, &oracleCall {}.abi_encode())?;
            if f.close_factor_mantissa == 0 {
                let n = u128::try_from(cf).map_err(|_| ConfigError::RegistryCall(to))?;
                f.close_factor_mantissa = n;
            } else if U256::from(f.close_factor_mantissa) != cf {
                return Err(ConfigError::RegistryMismatch {
                    comptroller: to,
                    field: "closeFactorMantissa",
                    expected: U256::from(f.close_factor_mantissa),
                    found: cf,
                });
            }
            if f.liquidation_incentive_mantissa == 0 {
                let n = u128::try_from(li).map_err(|_| ConfigError::RegistryCall(to))?;
                f.liquidation_incentive_mantissa = n;
            } else if U256::from(f.liquidation_incentive_mantissa) != li {
                return Err(ConfigError::RegistryMismatch {
                    comptroller: to,
                    field: "liquidationIncentiveMantissa",
                    expected: U256::from(f.liquidation_incentive_mantissa),
                    found: li,
                });
            }
            if f.oracle == Address::ZERO {
                f.oracle = oracle;
            } else if f.oracle != oracle {
                return Err(ConfigError::RegistryMismatch {
                    comptroller: to,
                    field: "oracle",
                    expected: addr_u256(f.oracle),
                    found: addr_u256(oracle),
                });
            }
            if !close_factor_in_pin_bounds(U256::from(f.close_factor_mantissa)) {
                return Err(ConfigError::CloseFactorBounds);
            }
            if f.liquidation_incentive_mantissa == 0 {
                return Err(ConfigError::ZeroIncentive);
            }
            for c in &f.ctokens {
                if c.underlying == Address::ZERO {
                    match provider.eth_call(c.ctoken, &underlyingCall {}.abi_encode(), block) {
                        Ok(raw) if raw.len() >= 32 => {
                            let found = addr_from_word(&raw)?;
                            if found != Address::ZERO {
                                return Err(ConfigError::CetherHasUnderlying(c.ctoken));
                            }
                        }
                        Ok(_) | Err(_) => {}
                    }
                } else {
                    let raw = provider
                        .eth_call(c.ctoken, &underlyingCall {}.abi_encode(), block)
                        .map_err(|_| ConfigError::RegistryCall(c.ctoken))?;
                    let found = addr_from_word(&raw)?;
                    if found != c.underlying {
                        return Err(ConfigError::UnderlyingMismatch(c.ctoken));
                    }
                }
            }
        }
        self.live_registry_asserted = true;
        Ok(())
    }

    #[inline]
    #[must_use]
    pub fn interned_id(&self, comptroller: Address) -> Option<MarketId> {
        self.interned
            .iter()
            .find(|(a, _)| *a == comptroller)
            .map(|(_, id)| *id)
    }

    #[inline]
    pub(crate) fn fork_by_comptroller(&self, comptroller: Address) -> Option<&ForkConfig> {
        self.forks.iter().find(|f| f.comptroller == comptroller)
    }

    #[inline]
    pub(crate) fn fork_by_market(&self, market: MarketId) -> Option<&ForkConfig> {
        let addr = self
            .interned
            .iter()
            .find(|(_, id)| *id == market)
            .map(|(a, _)| *a)?;
        self.fork_by_comptroller(addr)
    }

    #[inline]
    pub(crate) fn asset_by_underlying(&self, underlying: Address) -> Option<&AssetConfig> {
        self.assets.iter().find(|a| a.underlying == underlying)
    }

    #[inline]
    /// Debt/seize cToken from config: native asset → CEther (`underlying == 0`);
    /// otherwise the pin whose `underlying` matches the interned token. Never
    /// guessed via an on-chain `underlying()` call.
    pub(crate) fn ctoken_for_asset<'a>(
        &'a self,
        fork: &'a ForkConfig,
        asset: AssetId,
    ) -> Option<&'a CTokenPin> {
        let under = self.underlying_of(asset)?;
        if under == fork.native {
            return fork.ctokens.iter().find(|c| c.underlying.is_zero());
        }
        fork.ctokens.iter().find(|c| c.underlying == under)
    }

    #[inline]
    pub(crate) fn underlying_of(&self, asset: AssetId) -> Option<Address> {
        self.assets
            .iter()
            .find(|a| a.asset == asset)
            .map(|a| a.underlying)
    }

    #[inline]
    pub(crate) fn ctoken_seed(&self, ctoken: Address) -> Option<(Address, &CTokenPin)> {
        for f in &self.forks {
            if let Some(c) = f.ctokens.iter().find(|c| c.ctoken == ctoken) {
                return Some((f.comptroller, c));
            }
        }
        None
    }

    #[inline]
    pub(crate) fn native_asset(&self, fork: &ForkConfig) -> Option<&AssetConfig> {
        if fork.native == Address::ZERO {
            return None;
        }
        self.asset_by_underlying(fork.native)
    }

    pub fn from_toml(raw: &str) -> core::result::Result<Self, ConfigError> {
        let f: TomlFile = toml::from_str(raw).map_err(|_| ConfigError::MalformedToml)?;
        let mut forks = Vec::with_capacity(f.forks.len());
        for fork in f.forks {
            let mut ctokens = Vec::with_capacity(fork.ctokens.len());
            for c in fork.ctokens {
                ctokens.push(CTokenPin {
                    ctoken: parse_addr(&c.address)?,
                    underlying: match c.underlying.as_deref() {
                        None | Some("") => Address::ZERO,
                        Some(u) => parse_addr(u)?,
                    },
                });
            }
            forks.push(ForkConfig {
                comptroller: parse_addr(&fork.comptroller)?,
                close_factor_mantissa: parse_u128_opt(fork.close_factor_mantissa.as_deref())?,
                liquidation_incentive_mantissa: parse_u128_opt(
                    fork.liquidation_incentive_mantissa.as_deref(),
                )?,
                oracle: match fork.oracle.as_deref() {
                    None | Some("") => Address::ZERO,
                    Some(o) => parse_addr(o)?,
                },
                native: match fork.native.as_deref() {
                    None | Some("") => Address::ZERO,
                    Some(n) => parse_addr(n)?,
                },
                ctokens,
            });
        }
        let cfg = Self {
            protocol: ProtocolId(f.protocol),
            forks,
            interned: Vec::new(),
            assets: Vec::new(),
            pinned_through: f.pinned_through,
            live_registry_asserted: false,
        };
        if !cfg
            .forks
            .iter()
            .any(|fk| fk.comptroller == OFFICIAL_UNITROLLER)
        {
            return Err(ConfigError::OfficialForkRequired);
        }
        cfg.validate_shape()?;
        Ok(cfg)
    }
}

fn push_asset(
    assets: &mut Vec<AssetConfig>,
    seen: &mut Vec<Address>,
    intern: &Intern,
    underlying: Address,
) -> core::result::Result<(), ConfigError> {
    if seen.contains(&underlying) {
        return Ok(());
    }
    let Some(id) = intern.asset(underlying) else {
        return Ok(());
    };
    let rec = intern.asset_rec(id).ok_or(ConfigError::MalformedToml)?;
    seen.push(underlying);
    assets.push(AssetConfig {
        underlying,
        asset: id,
        feed: FeedId(0),
        decimals: rec.decimals,
    });
    Ok(())
}

fn parse_addr(s: &str) -> core::result::Result<Address, ConfigError> {
    s.parse().map_err(|_| ConfigError::MalformedToml)
}

fn parse_u128_opt(s: Option<&str>) -> core::result::Result<u128, ConfigError> {
    match s {
        None | Some("") => Ok(0),
        Some(v) => v.parse().map_err(|_| ConfigError::MalformedToml),
    }
}

fn call_u256<R: RegistryRpc>(
    rpc: &R,
    to: Address,
    block: BlockNum,
    data: &[u8],
) -> core::result::Result<U256, ConfigError> {
    let raw = rpc.eth_call(to, data, block)?;
    if raw.len() < 32 {
        return Err(ConfigError::RegistryCall(to));
    }
    Ok(U256::from_be_slice(
        raw.get(..32).ok_or(ConfigError::RegistryCall(to))?,
    ))
}

fn call_addr<R: RegistryRpc>(
    rpc: &R,
    to: Address,
    block: BlockNum,
    data: &[u8],
) -> core::result::Result<Address, ConfigError> {
    let raw = rpc.eth_call(to, data, block)?;
    addr_from_word(&raw)
}

fn addr_from_word(raw: &[u8]) -> core::result::Result<Address, ConfigError> {
    let word = raw
        .get(..32)
        .ok_or(ConfigError::RegistryCall(Address::ZERO))?;
    let tail = word
        .get(12..32)
        .ok_or(ConfigError::RegistryCall(Address::ZERO))?;
    let mut b = [0u8; 20];
    b.copy_from_slice(tail);
    Ok(Address::from(b))
}

fn addr_u256(a: Address) -> U256 {
    U256::from_be_slice(a.as_slice())
}

#[derive(serde::Deserialize)]
struct TomlFile {
    protocol: u16,
    pinned_through: u64,
    forks: Vec<TomlFork>,
}

#[derive(serde::Deserialize)]
struct TomlFork {
    comptroller: String,
    close_factor_mantissa: Option<String>,
    liquidation_incentive_mantissa: Option<String>,
    oracle: Option<String>,
    native: Option<String>,
    #[serde(default)]
    ctokens: Vec<TomlCToken>,
}

#[derive(serde::Deserialize)]
struct TomlCToken {
    address: String,
    underlying: Option<String>,
}
