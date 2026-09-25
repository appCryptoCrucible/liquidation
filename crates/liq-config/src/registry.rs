//! Committed registry types (REGISTRY.md §2, §5). Serde is the schema; the
//! JSON Schema at `registry/schema.json` is the same contract for non-Rust
//! tools.

use crate::asset_id::AssetLedger;
use crate::error::ConfigError;
use crate::Result;
use alloy_primitives::{Address, B256};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

/// Chain-derived static data. Keys are lowercase hex as committed.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub chain_id: u64,
    pub generated_at_block: u64,
    pub tokens: BTreeMap<Address, TokenEntry>,
    pub protocols: BTreeMap<String, ProtocolEntry>,
    pub oracles: BTreeMap<Address, OracleEntry>,
    pub pools: BTreeMap<Address, PoolEntry>,
    #[serde(default)]
    pub flash_sources: BTreeMap<Address, serde_json::Value>,
    #[serde(default)]
    pub routers: BTreeMap<Address, serde_json::Value>,
    /// Loaded from `asset-ids.json` beside the registry file. Absent on a
    /// registry built from bytes; [`Intern::from_registry`] then assigns
    /// ids in address order, which is only valid for a synthetic registry.
    #[serde(skip)]
    pub asset_ledger: Option<AssetLedger>,
}

/// One ERC-20 (or bytes32-metadata) token.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TokenEntry {
    /// `None` when discovery could not decode `symbol()` (JSON `null`). Not a guess.
    pub symbol: Option<String>,
    pub decimals: u8,
    #[serde(default)]
    pub quirks: Vec<TokenQuirk>,
    /// §4b identity collision, recorded when the canonical list disagrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol_collision: Option<SymbolCollision>,
}

/// Token behaviour that changes what correct code looks like (REGISTRY.md §5).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenQuirk {
    /// `transfer`/`approve` return no data (USDT, BNB, OMG). `SafeTransfer` required.
    NoReturnData,
    /// Non-zero → non-zero `approve` reverts (USDT). Zero first.
    ApproveNonzeroReverts,
    /// Received ≠ sent; size from the measured balance delta.
    FeeOnTransfer,
    /// Balance moves with no `Transfer` (stETH). Never cache it.
    Rebasing,
    /// Decimals ≪ 18 (USDC/USDT 6, WBTC 8). Rounding headroom is smaller.
    LowDecimals,
    /// `symbol()` returns `bytes32`, not `string` (MKR).
    NonstandardMetadata,
    /// Symbol collides with a canonical token at a different address (§4b).
    SymbolCollision,
}

impl TokenQuirk {
    /// `true` when `symbol()` must be ABI-decoded as `bytes32`.
    #[must_use]
    pub const fn symbol_is_bytes32(self) -> bool {
        matches!(self, Self::NonstandardMetadata)
    }
}

/// Record of a §4b symbol collision against a canonical list.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SymbolCollision {
    pub claims: String,
    pub canonical: Address,
    pub source: String,
}

/// One protocol market / instance. Family-specific fields sit in `extra`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProtocolEntry {
    pub family: String,
    pub market: OnChainId,
    pub deployed_block: u64,
    #[serde(default)]
    pub receipt_tokens: Vec<Address>,
    #[serde(default)]
    pub oracle_adapters: Vec<Address>,
    #[serde(default)]
    pub admitted: bool,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Aggregator proxy (REGISTRY.md §2 `oracles`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OracleEntry {
    pub aggregator: Address,
    pub pair: String,
    pub decimals: u8,
    pub svr: bool,
    #[serde(default)]
    pub source: String,
}

/// DEX pool. `token0`/`token1` are the pool's on-chain order, never sorted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PoolEntry {
    pub venue: PoolVenue,
    pub token0: Address,
    pub token1: Address,
    pub fee: u32,
    pub factory: Address,
    pub deployed_block: u64,
    #[serde(default)]
    pub derived_via: String,
    /// Curve only: every coin in pool order (`coins(i)`); `token0`/`token1`
    /// are `coins[0]`/`coins[1]`. Empty for V2/V3.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coins: Vec<Address>,
}

/// Pool family. Unknown venues fail serde — we must not call `token0`/`fee`
/// against an ABI we have not named.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PoolVenue {
    Univ3,
    /// Uniswap V2 or SushiSwap pair (0.30 %); `factory` says which.
    Univ2,
    /// Curve StableSwap plain pool; `fee` is `fee()` at discovery (1e10).
    Curve,
}

/// Address (Family A markets, UniV3 pools) or 32-byte Morpho market id.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum OnChainId {
    Addr(Address),
    Slot(B256),
}

impl OnChainId {
    fn parse(s: &str) -> Result<Self> {
        match s.len() {
            42 => Address::from_str(s)
                .map(Self::Addr)
                .map_err(|e| ConfigError::BadOnChainId(e.to_string())),
            66 => B256::from_str(s)
                .map(Self::Slot)
                .map_err(|e| ConfigError::BadOnChainId(e.to_string())),
            _ => Err(ConfigError::BadOnChainId(s.to_string())),
        }
    }
}

impl FromStr for OnChainId {
    type Err = ConfigError;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

impl Serialize for OnChainId {
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        match self {
            Self::Addr(a) => serializer.serialize_str(&format!("{a:#x}")),
            Self::Slot(s) => serializer.serialize_str(&format!("{s:#x}")),
        }
    }
}

impl<'de> Deserialize<'de> for OnChainId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl Registry {
    /// Load and serde-validate a committed registry file. Does not talk to chain.
    pub fn from_path(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| ConfigError::Load(e.to_string()))?;
        let mut reg = Self::from_slice(&bytes)?;
        let ledger_path = path
            .parent()
            .ok_or_else(|| {
                ConfigError::AssetLedger(format!("{} has no directory", path.display()))
            })?
            .join("asset-ids.json");
        let ledger = AssetLedger::load(&ledger_path)?;
        ledger.check(&reg)?;
        reg.asset_ledger = Some(ledger);
        Ok(reg)
    }

    /// Deserialize from JSON bytes.
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(|e| ConfigError::Load(e.to_string()))
    }

    /// `flash_sources[addr].kind` (or a string value). Missing kind is `None`
    /// — callers omit; they must not guess the arena.
    #[must_use]
    pub fn flash_source_kind(&self, addr: Address) -> Option<&str> {
        let v = self.flash_sources.get(&addr)?;
        v.get("kind")
            .and_then(|x| x.as_str())
            .or_else(|| v.as_str())
    }
}

impl TokenEntry {
    /// `true` when `symbol()` is `bytes32` on this token.
    #[must_use]
    pub fn symbol_is_bytes32(&self) -> bool {
        self.quirks
            .iter()
            .copied()
            .any(TokenQuirk::symbol_is_bytes32)
    }
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
    use super::{PoolVenue, Registry, TokenQuirk};
    use alloy_primitives::{address, Address};
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    const USDT: Address = address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");
    const MKR: Address = address!("0x9f8F72aA9304c8B593d555F12eF6589cC3A579A2");
    const STETH: Address = address!("0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84");
    const WBTC: Address = address!("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");

    fn load_committed() -> Registry {
        Registry::from_path(&workspace_root().join("registry/registry.json")).unwrap()
    }

    /// Oracle: the committed registry deserializes. Negative: a schema drift
    /// (unknown quirk, missing decimals) is a load error, not a silent skip.
    #[test]
    fn committed_registry_deserializes() {
        let reg = load_committed();
        assert_eq!(reg.chain_id, 1);
        assert!(!reg.tokens.is_empty());
        assert!(!reg.pools.is_empty());
        for (addr, p) in &reg.pools {
            if p.venue == PoolVenue::Curve {
                assert!(
                    p.coins.len() >= 2 && p.coins[0] == p.token0 && p.coins[1] == p.token1,
                    "curve {addr:#x}: token0/token1 must be coins[0]/coins[1]"
                );
                continue;
            }
            assert!(
                p.token0 < p.token1,
                "V2/V3 factory order is token0 < token1; registry {addr:#x} has token0 {:#x} token1 {:#x}",
                p.token0,
                p.token1
            );
        }
        // JSON null records a token whose `symbol()` reverts (not a guess).
        // Load must accept that; boot assertion requires the chain to still
        // revert there.
        let null_syms: Vec<_> = reg
            .tokens
            .iter()
            .filter(|(_, t)| t.symbol.is_none())
            .map(|(a, _)| *a)
            .collect();
        assert_eq!(null_syms.len(), 1);
    }

    /// Oracle: REGISTRY.md §5 + the committed file. USDT/MKR/stETH/WBTC quirks
    /// are data, not assumptions in the transfer/ABI path.
    #[test]
    fn token_quirks_loaded() {
        let reg = load_committed();
        let usdt = reg.tokens.get(&USDT).unwrap();
        assert!(usdt.quirks.contains(&TokenQuirk::NoReturnData));
        assert!(usdt.quirks.contains(&TokenQuirk::ApproveNonzeroReverts));
        assert!(usdt.quirks.contains(&TokenQuirk::LowDecimals));
        assert_eq!(usdt.decimals, 6);
        assert_eq!(usdt.symbol.as_deref(), Some("USDT"));

        let mkr = reg.tokens.get(&MKR).unwrap();
        assert!(mkr.quirks.contains(&TokenQuirk::NonstandardMetadata));
        assert!(mkr.symbol_is_bytes32());

        let steth = reg.tokens.get(&STETH).unwrap();
        assert!(steth.quirks.contains(&TokenQuirk::Rebasing));

        let wbtc = reg.tokens.get(&WBTC).unwrap();
        assert!(wbtc.quirks.contains(&TokenQuirk::LowDecimals));
        assert_eq!(wbtc.decimals, 8);
    }

    /// Oracle: schema `additionalProperties: false` is enforced by serde, not
    /// just by schema.json. Negative: a typo'd `quirk` key must not load as
    /// `quirks = []`.
    #[test]
    fn unknown_token_field_is_load_error() {
        let err = Registry::from_slice(
            br#"{
            "chain_id": 1,
            "generated_at_block": 0,
            "tokens": {
                "0xdac17f958d2ee523a2206206994597c13d831ec7": {
                    "symbol": "USDT",
                    "decimals": 6,
                    "quirk": ["no_return_data"]
                }
            },
            "protocols": {},
            "oracles": {},
            "pools": {},
            "flash_sources": {},
            "routers": {}
        }"#,
        )
        .unwrap_err();
        match err {
            crate::error::ConfigError::Load(msg) => {
                assert!(
                    msg.contains("unknown field") && msg.contains("quirk"),
                    "expected unknown-field error, got {msg}"
                );
            }
            other => panic!("expected Load, got {other:?}"),
        }
    }
}
