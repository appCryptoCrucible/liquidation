//! Intern tables: `AssetId` / `ProtocolId` / `MarketId` / `FeedId`
//! produced from the committed registry (GUIDE 00 §3). Asset ids come
//! from `registry/asset-ids.json` and may have gaps; the other three
//! are still the sorted position of that table.

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
    /// `AssetId.0` → index in `assets`. `None` is a retired id.
    by_id: Vec<Option<u16>>,
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
    /// Assign asset ids from `registry/asset-ids.json` when the registry
    /// was loaded from disk. A registry built in memory (tests) still
    /// numbers tokens in address order; that order is not stable once a
    /// token is added, which is why production refuses to start without
    /// the ledger.
    pub fn from_registry(reg: &Registry) -> Result<Self> {
        let mut numbered: Vec<(u16, AssetRec)> = Vec::with_capacity(reg.tokens.len());
        let mut asset_by_addr = BTreeMap::new();
        let mut next = 0u32;
        for (ord, (addr, entry)) in reg.tokens.iter().enumerate() {
            let id = if let Some(ledger) = &reg.asset_ledger {
                ledger
                    .id(*addr)
                    .ok_or_else(|| ConfigError::AssetLedger(format!("{addr:#x} has no id")))?
            } else {
                intern_u16(ord, "assets")?
            };
            next = next.max(u32::from(id).saturating_add(1));
            let rec = AssetRec {
                id: AssetId(id),
                address: *addr,
                symbol: entry.symbol.clone(),
                decimals: entry.decimals,
                quirks: entry.quirks.clone(),
            };
            asset_by_addr.insert(*addr, rec.id);
            numbered.push((id, rec));
        }
        if let Some(ledger) = &reg.asset_ledger {
            next = ledger.next();
        }
        numbered.sort_unstable_by_key(|(id, _)| *id);
        let mut assets = Vec::with_capacity(numbered.len());
        let width =
            usize::try_from(next).map_err(|_| ConfigError::InternOverflow { what: "assets" })?;
        let mut by_id = vec![None; width];
        for (pos, (id, rec)) in numbered.into_iter().enumerate() {
            let at = intern_u16(pos, "assets")?;
            let slot = by_id.get_mut(usize::from(id)).ok_or_else(|| {
                ConfigError::AssetLedger(format!("id {id} is past ledger next {next}"))
            })?;
            if slot.is_some() {
                return Err(ConfigError::AssetLedger(format!("id {id} is duplicated")));
            }
            *slot = Some(at);
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
            by_id,
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
        let pos = self.by_id.get(usize::from(id.0)).copied().flatten()?;
        self.assets.get(usize::from(pos))
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

    /// Live tokens, in id order. **Not** the id space: with retired ids
    /// (`registry/asset-ids.json` `removed`) there are gaps, so
    /// `assets().len()` can be less than the highest id + 1. Size any table
    /// indexed by `AssetId` with [`Self::asset_id_capacity`].
    #[must_use]
    pub fn assets(&self) -> &[AssetRec] {
        &self.assets
    }

    /// One past the highest `AssetId` (the ledger's `next`): the length of
    /// every table indexed by `AssetId`. Retired ids inside it have no
    /// [`AssetRec`].
    #[must_use]
    pub fn asset_id_capacity(&self) -> usize {
        self.by_id.len()
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

    /// Oracle: every committed token's id is the ledger's, and at least one
    /// id is not that token's address-sort index (an append must not
    /// renumber). Negative: the same two tokens numbered in reverse keep
    /// those ids, which address-sort would have swapped.
    #[test]
    fn committed_ids_follow_the_ledger_not_sort_order() {
        let reg = Registry::from_path(&workspace_root().join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let ledger = reg.asset_ledger.as_ref().unwrap();
        let mut off_sort = 0usize;
        for (ord, (addr, _)) in reg.tokens.iter().enumerate() {
            let id = intern.asset(*addr).unwrap();
            assert_eq!(ledger.id(*addr), Some(id.0), "{addr:#x}");
            assert_eq!(intern.asset_rec(id).unwrap().address, *addr);
            if usize::from(id.0) != ord {
                off_sort += 1;
            }
        }
        assert!(
            off_sort > 0,
            "appending a token must leave some id different from its sort index"
        );
        assert_eq!(ledger.next(), u32::try_from(reg.tokens.len()).unwrap());

        let raw = br#"{
            "chain_id": 1,
            "generated_at_block": 1,
            "tokens": {
                "0x0000000000000000000000000000000000000001": {"symbol": "A", "decimals": 18},
                "0x0000000000000000000000000000000000000002": {"symbol": "B", "decimals": 18}
            },
            "protocols": {},
            "oracles": {},
            "pools": {}
        }"#;
        let mut synthetic = Registry::from_slice(raw).unwrap();
        let mut ids = std::collections::BTreeMap::new();
        ids.insert(address!("0000000000000000000000000000000000000001"), 1);
        ids.insert(address!("0000000000000000000000000000000000000002"), 0);
        let mut removed = std::collections::BTreeMap::new();
        removed.insert(address!("0000000000000000000000000000000000000003"), 2);
        synthetic.asset_ledger = Some(crate::asset_id::AssetLedger::from_parts(ids, removed, 4));
        synthetic
            .asset_ledger
            .as_ref()
            .unwrap()
            .check(&synthetic)
            .unwrap();
        let intern = Intern::from_registry(&synthetic).unwrap();
        assert_eq!(
            intern
                .asset(address!("0000000000000000000000000000000000000001"))
                .unwrap()
                .0,
            1
        );
        assert_eq!(
            intern
                .asset(address!("0000000000000000000000000000000000000002"))
                .unwrap()
                .0,
            0
        );
        assert!(intern.asset_rec(liq_types::AssetId(2)).is_none());
        // Two live tokens, but ids run to 3: tables indexed by AssetId must
        // be sized by the id space, not the live count.
        assert_eq!(intern.assets().len(), 2);
        assert_eq!(intern.asset_id_capacity(), 4);
        assert_eq!(
            intern
                .asset_rec(liq_types::AssetId(0))
                .unwrap()
                .symbol
                .as_deref(),
            Some("B")
        );

        let mut grown = synthetic.clone();
        grown.tokens.insert(
            address!("0000000000000000000000000000000000000004"),
            crate::registry::TokenEntry {
                symbol: Some("C".to_string()),
                decimals: 6,
                quirks: Vec::new(),
                symbol_collision: None,
            },
        );
        let mut grown_ids = grown.asset_ledger.as_ref().unwrap().live().clone();
        let assigned = grown.asset_ledger.as_ref().unwrap().next();
        grown_ids.insert(
            address!("0000000000000000000000000000000000000004"),
            u16::try_from(assigned).unwrap(),
        );
        let removed = grown.asset_ledger.as_ref().unwrap().removed().clone();
        grown.asset_ledger = Some(crate::asset_id::AssetLedger::from_parts(
            grown_ids,
            removed,
            assigned + 1,
        ));
        grown.asset_ledger.as_ref().unwrap().check(&grown).unwrap();
        let intern = Intern::from_registry(&grown).unwrap();
        assert_eq!(
            intern
                .asset(address!("0000000000000000000000000000000000000001"))
                .unwrap()
                .0,
            1
        );
        assert_eq!(
            intern
                .asset(address!("0000000000000000000000000000000000000002"))
                .unwrap()
                .0,
            0
        );
        assert_eq!(
            intern
                .asset(address!("0000000000000000000000000000000000000004"))
                .unwrap()
                .0,
            4
        );
        assert!(intern.asset_rec(liq_types::AssetId(3)).is_none());

        let mut broken = Registry::from_slice(raw).unwrap();
        let mut only_a = std::collections::BTreeMap::new();
        only_a.insert(address!("0000000000000000000000000000000000000001"), 0);
        broken.asset_ledger = Some(crate::asset_id::AssetLedger::from_parts(
            only_a,
            std::collections::BTreeMap::new(),
            1,
        ));
        let err = broken
            .asset_ledger
            .as_ref()
            .unwrap()
            .check(&broken)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("has no id"), "{msg}");
    }
}
