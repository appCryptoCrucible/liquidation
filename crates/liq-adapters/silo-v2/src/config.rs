//! Deployment pin: factories + admitted isolated pairs. Markets are discovered
//! from `NewSilo`, never a hand-walk of ids. Params are `getConfig` at pin
//! block 26014442.

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

/// One side of a `SiloConfig` pair (`getConfig(silo)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideConfig {
    pub silo: Address,
    pub token: Address,
    pub protected_share: Address,
    pub debt_share: Address,
    pub solvency_oracle: Address,
    pub lt: u128,
    pub liquidation_fee: u128,
    pub liquidation_target_ltv: u128,
}

/// Isolated pair: one collateral token backs one debt token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairConfig {
    pub silo_config: Address,
    pub hook_receiver: Address,
    pub market: MarketId,
    pub silo0: SideConfig,
    pub silo1: SideConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub protocol: ProtocolId,
    pub factories: Vec<Address>,
    pub pairs: Vec<PairConfig>,
    pub assets: Vec<AssetConfig>,
    pub pinned_through: BlockNum,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ShareKind {
    Collateral,
    Protected,
    Debt,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ShareLoc {
    pub pair: usize,
    pub slot: u16,
    pub kind: ShareKind,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Emitter {
    Factory(usize),
    Hook(usize),
    Silo { pair: usize, slot: u16 },
    Share(ShareLoc),
    SiloConfig(usize),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("no factory configured")]
    NoFactories,
    #[error("no isolated pair configured")]
    NoPairs,
    #[error("address {0} configured twice")]
    DuplicateAddress(Address),
    #[error("market {0:?} configured twice")]
    DuplicateMarket(MarketId),
    #[error("pair hook or silo is the zero address")]
    ZeroPairAddress,
    #[error("asset id {0:?} or underlying configured twice")]
    DuplicateAsset(AssetId),
    #[error("pair token is not in the asset intern")]
    UnknownPairToken,
}

impl Config {
    pub fn validate(&self) -> core::result::Result<(), ConfigError> {
        if self.factories.is_empty() {
            return Err(ConfigError::NoFactories);
        }
        if self.pairs.is_empty() {
            return Err(ConfigError::NoPairs);
        }
        let mut addrs: Vec<Address> = Vec::new();
        for f in &self.factories {
            if *f == Address::ZERO {
                return Err(ConfigError::ZeroPairAddress);
            }
            if addrs.contains(f) {
                return Err(ConfigError::DuplicateAddress(*f));
            }
            addrs.push(*f);
        }
        let mut markets: Vec<MarketId> = Vec::new();
        for p in &self.pairs {
            if p.silo_config == Address::ZERO
                || p.hook_receiver == Address::ZERO
                || p.silo0.silo == Address::ZERO
                || p.silo1.silo == Address::ZERO
            {
                return Err(ConfigError::ZeroPairAddress);
            }
            if markets.contains(&p.market) {
                return Err(ConfigError::DuplicateMarket(p.market));
            }
            markets.push(p.market);
            for a in [p.silo_config, p.hook_receiver, p.silo0.silo, p.silo1.silo] {
                if addrs.contains(&a) {
                    return Err(ConfigError::DuplicateAddress(a));
                }
                addrs.push(a);
            }
            if self.asset_by_underlying(p.silo0.token).is_none()
                || self.asset_by_underlying(p.silo1.token).is_none()
            {
                return Err(ConfigError::UnknownPairToken);
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
    pub(crate) fn pair(&self, i: usize) -> Option<&PairConfig> {
        self.pairs.get(i)
    }

    #[inline]
    #[allow(dead_code)]
    pub(crate) fn pair_by_market(&self, market: MarketId) -> Option<(usize, &PairConfig)> {
        self.pairs
            .iter()
            .enumerate()
            .find(|(_, p)| p.market == market)
    }

    #[inline]
    pub(crate) fn pair_by_config(&self, cfg: Address) -> Option<(usize, &PairConfig)> {
        self.pairs
            .iter()
            .enumerate()
            .find(|(_, p)| p.silo_config == cfg)
    }

    pub(crate) fn emitter(&self, address: Address) -> Option<Emitter> {
        for (i, f) in self.factories.iter().enumerate() {
            if *f == address {
                return Some(Emitter::Factory(i));
            }
        }
        for (i, p) in self.pairs.iter().enumerate() {
            if p.hook_receiver == address {
                return Some(Emitter::Hook(i));
            }
            if p.silo_config == address {
                return Some(Emitter::SiloConfig(i));
            }
            if p.silo0.silo == address {
                return Some(Emitter::Silo { pair: i, slot: 0 });
            }
            if p.silo1.silo == address {
                return Some(Emitter::Silo { pair: i, slot: 1 });
            }
            if let Some(loc) = share_of(i, p, address) {
                return Some(Emitter::Share(loc));
            }
        }
        None
    }

    #[allow(dead_code)]
    pub(crate) fn share(&self, address: Address) -> Option<ShareLoc> {
        match self.emitter(address) {
            Some(Emitter::Share(loc)) => Some(loc),
            _ => None,
        }
    }
}

fn share_of(pair: usize, p: &PairConfig, address: Address) -> Option<ShareLoc> {
    for (slot, s) in [(0u16, &p.silo0), (1, &p.silo1)] {
        if s.silo == address {
            return Some(ShareLoc {
                pair,
                slot,
                kind: ShareKind::Collateral,
            });
        }
        if s.protected_share == address {
            return Some(ShareLoc {
                pair,
                slot,
                kind: ShareKind::Protected,
            });
        }
        if s.debt_share == address {
            return Some(ShareLoc {
                pair,
                slot,
                kind: ShareKind::Debt,
            });
        }
    }
    None
}

impl PairConfig {
    #[inline]
    pub(crate) fn side(&self, slot: u16) -> Option<&SideConfig> {
        match slot {
            0 => Some(&self.silo0),
            1 => Some(&self.silo1),
            _ => None,
        }
    }

    #[inline]
    pub(crate) fn other(&self, slot: u16) -> Option<u16> {
        match slot {
            0 => Some(1),
            1 => Some(0),
            _ => None,
        }
    }
}
