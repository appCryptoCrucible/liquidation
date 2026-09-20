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
            rpc_url: &'a str,
            risk: &'a crate::config::RiskConfig,
            venues: &'a crate::config::VenuesConfig,
            registry: &'a Registry,
        }
        let bytes = serde_json::to_vec(&Payload {
            chain_id: config.chain_id,
            rpc_url: &config.rpc_url,
            risk: &config.risk,
            venues: &config.venues,
            registry,
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
