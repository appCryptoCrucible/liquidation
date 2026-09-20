//! Dense intern tables: `AssetId` / `ProtocolId` / `MarketId` / `FeedId`
//! produced from the committed registry (GUIDE 00 §3).

use crate::error::ConfigError;
use crate::registry::{OnChainId, Registry, TokenQuirk};
use crate::Result;
use alloy_primitives::Address;
use liq_protocol::FeedId;
use liq_types::{AssetId, MarketId, ProtocolId};
use std::collections::BTreeMap;

/// Interned identities the rest of the process uses. Built once at boot.
#[derive(Clone, Debug)]
pub struct Intern {
    assets: Vec<AssetRec>,
    asset_by_addr: BTreeMap<Address, AssetId>,
    protocols: BTreeMap<String, ProtocolId>,
    markets: Vec<MarketRec>,
    feeds: Vec<FeedRec>,
    feed_by_proxy: BTreeMap<Address, FeedId>,
}

/// One interned token.
#[derive(Clone, Debug)]
pub struct AssetRec {
    pub id: AssetId,
    pub address: Address,
    pub symbol: Option<String>,
    pub decimals: u8,
    pub quirks: Vec<TokenQuirk>,
}

/// One interned protocol market.
#[derive(Clone, Debug)]
pub struct MarketRec {
    pub id: MarketId,
    pub protocol: ProtocolId,
    pub key: OnChainId,
    pub admitted: bool,
}

/// One interned oracle proxy → [`FeedId`] (GUIDE 06; [`liq_protocol::MarketRow::price_feed`]).
#[derive(Clone, Debug)]
pub struct FeedRec {
    pub id: FeedId,
    pub proxy: Address,
    pub aggregator: Address,
    pub decimals: u8,
    pub svr: bool,
}

impl Intern {
    /// Assign dense ids in address / family sort order. Overflow of the id
    /// width is a load error, not wrap.
    pub fn from_registry(reg: &Registry) -> Result<Self> {
        let mut assets = Vec::with_capacity(reg.tokens.len());
        let mut asset_by_addr = BTreeMap::new();
        for (addr, entry) in &reg.tokens {
            let id = intern_u16(assets.len(), "assets")?;
            let rec = AssetRec {
                id: AssetId(id),
                address: *addr,
                symbol: entry.symbol.clone(),
                decimals: entry.decimals,
                quirks: entry.quirks.clone(),
            };
            asset_by_addr.insert(*addr, rec.id);
            assets.push(rec);
        }

        let mut families: Vec<&str> = Vec::new();
        for entry in reg.protocols.values() {
            if !families.contains(&entry.family.as_str()) {
                families.push(entry.family.as_str());
            }
        }
        families.sort_unstable();
        let mut protocols = BTreeMap::new();
        for (i, fam) in families.iter().enumerate() {
            let id = intern_u16(i, "protocols")?;
            protocols.insert((*fam).to_string(), ProtocolId(id));
        }

        let mut markets = Vec::with_capacity(reg.protocols.len());
        for entry in reg.protocols.values() {
            let protocol = *protocols.get(&entry.family).ok_or_else(|| {
                ConfigError::Load(format!("intern missing family {}", entry.family))
            })?;
            let id = intern_u32(markets.len(), "markets")?;
            markets.push(MarketRec {
                id: MarketId(id),
                protocol,
                key: entry.market,
                admitted: entry.admitted,
            });
        }

        let mut feeds = Vec::with_capacity(reg.oracles.len());
        let mut feed_by_proxy = BTreeMap::new();
        for (proxy, entry) in &reg.oracles {
            let raw = intern_u16(feeds.len(), "feeds")?;
            let rec = FeedRec {
                id: FeedId(raw),
                proxy: *proxy,
                aggregator: entry.aggregator,
                decimals: entry.decimals,
                svr: entry.svr,
            };
            feed_by_proxy.insert(*proxy, rec.id);
            feeds.push(rec);
        }

        Ok(Self {
            assets,
            asset_by_addr,
            protocols,
            markets,
            feeds,
            feed_by_proxy,
        })
    }

    #[must_use]
    pub fn asset(&self, addr: Address) -> Option<AssetId> {
        self.asset_by_addr.get(&addr).copied()
    }

    #[must_use]
    pub fn asset_rec(&self, id: AssetId) -> Option<&AssetRec> {
        self.assets.get(usize::from(id.0))
    }

    #[must_use]
    pub fn decimals(&self, id: AssetId) -> Option<u8> {
        self.asset_rec(id).map(|r| r.decimals)
    }

    #[must_use]
    pub fn quirks(&self, id: AssetId) -> &[TokenQuirk] {
        self.asset_rec(id)
            .map(|r| r.quirks.as_slice())
            .unwrap_or(&[])
    }

    #[must_use]
    pub fn protocol(&self, family: &str) -> Option<ProtocolId> {
        self.protocols.get(family).copied()
    }

    #[must_use]
    pub fn markets(&self) -> &[MarketRec] {
        &self.markets
    }

    #[must_use]
    pub fn feed(&self, proxy: Address) -> Option<FeedId> {
        self.feed_by_proxy.get(&proxy).copied()
    }

    #[must_use]
    pub fn feeds(&self) -> &[FeedRec] {
        &self.feeds
    }

    #[must_use]
    pub fn assets(&self) -> &[AssetRec] {
        &self.assets
    }
}

fn intern_u16(len: usize, what: &'static str) -> Result<u16> {
    u16::try_from(len).map_err(|_| ConfigError::InternOverflow { what })
}

fn intern_u32(len: usize, what: &'static str) -> Result<u32> {
    u32::try_from(len).map_err(|_| ConfigError::InternOverflow { what })
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::Intern;
    use crate::registry::Registry;
    use alloy_primitives::{address, Address};
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    const WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

    /// Oracle: GUIDE 00 §3 — WETH is one [`liq_types::AssetId`] even though it
    /// appears in many protocol rows. Negative: a (protocol, address) intern
    /// would yield two ids for the same contract.
    #[test]
    fn weth_is_one_asset_id_across_protocols() {
        let reg = Registry::from_path(&workspace_root().join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let weth = intern
            .asset(WETH)
            .expect("WETH is in the committed registry");
        let hits = intern.assets().iter().filter(|a| a.address == WETH).count();
        assert_eq!(hits, 1);
        assert_eq!(intern.asset_rec(weth).unwrap().decimals, 18);
        assert!(intern.protocol("aave-v3").is_some());
        assert!(intern.protocol("morpho-blue").is_some());
        assert_ne!(intern.protocol("aave-v3"), intern.protocol("morpho-blue"));
    }
}
