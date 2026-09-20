//! Deployment pin: every address and liquidation constant from the registry.
//! Spark (15A-2) is a different `Config`, not a fork of this crate's numbers.

use alloy_primitives::Address;
use liq_protocol::{BlockNum, FeedId};
use liq_types::{AssetId, MarketId, ProtocolId};

/// One Pool proxy + its AddressesProvider / oracle / configurator / sentinel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolConfig {
    pub address: Address,
    pub market: MarketId,
    pub oracle: Address,
    pub provider: Address,
    pub configurator: Address,
    /// `address(0)` on L1: liquidations are never sentinel-gated.
    pub sentinel: Address,
    /// Sequencer feed the sentinel reads; `address(0)` when unused.
    pub sequencer_oracle: Address,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetConfig {
    pub underlying: Address,
    pub asset: AssetId,
    pub feed: FeedId,
    pub siloed: bool,
    pub isolated: bool,
    pub debt_ceiling: u128,
    pub decimals: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcePin {
    pub pool: Address,
    pub underlying: Address,
    pub source: Address,
}

/// Close-factor / dust thresholds from the **instance** (LiquidationLogic
/// constants on origin; Spark may differ — never compile them in).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiquidationParams {
    pub close_factor_bps: u16,
    pub close_factor_hf_wad: u128,
    pub min_base_max_close: u128,
    /// Oracle answer decimals (`AaveOracle.BASE_CURRENCY_UNIT` log10).
    pub oracle_decimals: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub protocol: ProtocolId,
    pub pools: Vec<PoolConfig>,
    pub assets: Vec<AssetConfig>,
    pub price_sources: Vec<SourcePin>,
    pub liquidation: LiquidationParams,
    pub pinned_through: BlockNum,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Emitter {
    Pool(MarketId),
    Configurator(usize),
    Oracle(usize),
    Provider(usize),
    Sentinel(usize),
    Sequencer(usize),
    AToken { pool: usize, slot: u16 },
    VToken { pool: usize, slot: u16 },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("no pool configured")]
    NoPools,
    #[error("address {0} configured twice")]
    DuplicateAddress(Address),
    #[error("market {0:?} configured twice")]
    DuplicateMarket(MarketId),
    #[error("pool {0} has the zero address as oracle")]
    ZeroOracle(Address),
    #[error("asset id {0:?} or underlying configured twice")]
    DuplicateAsset(AssetId),
    #[error("oracle_decimals must be <= 27")]
    BadOracleDecimals,
}

impl Config {
    pub fn validate(&self) -> core::result::Result<(), ConfigError> {
        if self.pools.is_empty() {
            return Err(ConfigError::NoPools);
        }
        if self.liquidation.oracle_decimals > 27 {
            return Err(ConfigError::BadOracleDecimals);
        }
        let mut markets: Vec<MarketId> = Vec::new();
        let mut addrs: Vec<Address> = Vec::new();
        for p in &self.pools {
            if markets.contains(&p.market) {
                return Err(ConfigError::DuplicateMarket(p.market));
            }
            markets.push(p.market);
            for a in [p.address, p.oracle, p.provider, p.configurator] {
                if addrs.contains(&a) {
                    return Err(ConfigError::DuplicateAddress(a));
                }
                addrs.push(a);
            }
            if p.oracle == Address::ZERO {
                return Err(ConfigError::ZeroOracle(p.address));
            }
            for opt in [p.sentinel, p.sequencer_oracle] {
                if opt != Address::ZERO {
                    if addrs.contains(&opt) {
                        return Err(ConfigError::DuplicateAddress(opt));
                    }
                    addrs.push(opt);
                }
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
        Ok(())
    }

    #[inline]
    pub(crate) fn pool_by_market(&self, market: MarketId) -> Option<&PoolConfig> {
        self.pools.iter().find(|p| p.market == market)
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
    pub(crate) fn pinned_source(&self, pool: Address, underlying: Address) -> Option<Address> {
        self.price_sources
            .iter()
            .find(|p| p.pool == pool && p.underlying == underlying)
            .map(|p| p.source)
    }

    #[inline]
    pub(crate) fn oracle_scale(&self) -> u128 {
        // 10^(27 - d); d <= 27 from validate.
        let exp = 27u32.saturating_sub(u32::from(self.liquidation.oracle_decimals));
        10u128.saturating_pow(exp)
    }
}
