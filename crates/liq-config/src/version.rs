//! Hash of the loaded config + registry, logged at every startup so a decision
//! from three weeks ago can be tied to exactly what was live (GUIDE 00 §4).

use crate::config::BotConfig;
use crate::error::ConfigError;
use crate::registry::Registry;
use alloy_primitives::{keccak256, B256};
use core::fmt;
use serde::Serialize;

/// 32-byte keccak of the canonical JSON of config + registry.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct ConfigVersion(pub B256);

impl ConfigVersion {
    /// Hash the structured config (not filesystem paths — those differ per box)
    /// together with the committed registry.
    pub fn hash(config: &BotConfig, registry: &Registry) -> crate::Result<Self> {
        #[derive(Serialize)]
        struct Payload<'a> {
            chain_id: u64,
            risk: &'a crate::config::RiskConfig,
            venues: &'a crate::config::VenuesConfig,
            registry: &'a Registry,
            /// Present once the registry was loaded from disk. An id edit
            /// must change the version even when `registry.json` did not.
            asset_ids: Option<&'a std::collections::BTreeMap<alloy_primitives::Address, u16>>,
            removed_asset_ids:
                Option<&'a std::collections::BTreeMap<alloy_primitives::Address, u16>>,
            asset_id_next: Option<u32>,
        }
        let ledger = registry.asset_ledger.as_ref();
        let bytes = serde_json::to_vec(&Payload {
            chain_id: config.chain_id,
            risk: &config.risk,
            venues: &config.venues,
            registry,
            asset_ids: ledger.map(|l| l.live()),
            removed_asset_ids: ledger.map(|l| l.removed()),
            asset_id_next: ledger.map(|l| l.next()),
        })
        .map_err(|e| ConfigError::Load(e.to_string()))?;
        Ok(Self(keccak256(&bytes)))
    }
}

impl fmt::Display for ConfigVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}
