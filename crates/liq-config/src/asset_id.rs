//! Append-only `AssetId` ledger (`registry/asset-ids.json`).
//!
//! An id is not the token's position in the sorted registry. Existing
//! tokens keep the id they had when the ledger was written; a new token
//! takes `next`; a removed token's id stays in `removed` and is never
//! reused. Loading fails if the ledger and the registry disagree.

use crate::error::ConfigError;
use crate::registry::Registry;
use crate::Result;
use alloy_primitives::Address;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::str::FromStr;

/// `address → AssetId`, plus ids retired with their token.
#[derive(Clone, Debug)]
pub struct AssetLedger {
    ids: BTreeMap<Address, u16>,
    removed: BTreeMap<Address, u16>,
    /// Next id a newly added token would receive. One past every live
    /// and retired id.
    next: u32,
}

#[derive(Deserialize)]
struct File {
    next: u32,
    ids: BTreeMap<String, u16>,
    #[serde(default)]
    removed: BTreeMap<String, u16>,
}

impl AssetLedger {
    /// Read and parse. Does not look at the registry; [`Self::check`] does.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .map_err(|e| ConfigError::AssetLedger(format!("reading {}: {e}", path.display())))?;
        let file: File =
            serde_json::from_slice(&bytes).map_err(|e| ConfigError::AssetLedger(e.to_string()))?;
        let ids = parse_map("ids", &file.ids)?;
        let removed = parse_map("removed", &file.removed)?;
        Ok(Self {
            ids,
            removed,
            next: file.next,
        })
    }

    /// The id of a live token. `None` when the address is absent or retired.
    #[must_use]
    pub fn id(&self, addr: Address) -> Option<u16> {
        self.ids.get(&addr).copied()
    }

    #[must_use]
    pub const fn next(&self) -> u32 {
        self.next
    }

    #[must_use]
    pub fn live(&self) -> &BTreeMap<Address, u16> {
        &self.ids
    }

    #[must_use]
    pub fn removed(&self) -> &BTreeMap<Address, u16> {
        &self.removed
    }

    /// Fail closed when the ledger and the registry are not the same set
    /// of tokens, or when an id is duplicated, reused, or past `next`.
    pub fn check(&self, reg: &Registry) -> Result<()> {
        if self.next > u32::from(u16::MAX) + 1 {
            return Err(ConfigError::AssetLedger(format!(
                "next {} does not fit in u16",
                self.next
            )));
        }
        let mut seen: BTreeMap<u16, Address> = BTreeMap::new();
        for (addr, id) in &self.ids {
            if u32::from(*id) >= self.next {
                return Err(ConfigError::AssetLedger(format!(
                    "{addr:#x} id {id} is not below next {}",
                    self.next
                )));
            }
            if self.removed.contains_key(addr) {
                return Err(ConfigError::AssetLedger(format!(
                    "{addr:#x} is both live and removed"
                )));
            }
            if let Some(prev) = seen.insert(*id, *addr) {
                return Err(ConfigError::AssetLedger(format!(
                    "id {id} is assigned to {prev:#x} and {addr:#x}"
                )));
            }
        }
        for (addr, id) in &self.removed {
            if u32::from(*id) >= self.next {
                return Err(ConfigError::AssetLedger(format!(
                    "removed {addr:#x} id {id} is not below next {}",
                    self.next
                )));
            }
            if let Some(prev) = seen.insert(*id, *addr) {
                return Err(ConfigError::AssetLedger(format!(
                    "id {id} is assigned to {prev:#x} and removed {addr:#x}"
                )));
            }
        }
        let mut missing = Vec::new();
        for addr in reg.tokens.keys() {
            if !self.ids.contains_key(addr) {
                missing.push(*addr);
            }
        }
        if let Some(token) = missing.first() {
            return Err(ConfigError::AssetLedger(format!(
                "registry token {token:#x} has no id ({} missing)",
                missing.len()
            )));
        }
        for addr in self.ids.keys() {
            if !reg.tokens.contains_key(addr) {
                return Err(ConfigError::AssetLedger(format!(
                    "{addr:#x} is in the ledger but not in the registry and is not removed"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
impl AssetLedger {
    pub fn from_parts(
        ids: BTreeMap<Address, u16>,
        removed: BTreeMap<Address, u16>,
        next: u32,
    ) -> Self {
        Self { ids, removed, next }
    }
}

fn parse_map(field: &str, raw: &BTreeMap<String, u16>) -> Result<BTreeMap<Address, u16>> {
    let mut out = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for (text, id) in raw {
        let addr = Address::from_str(text)
            .map_err(|_| ConfigError::AssetLedger(format!("{field}: {text} is not an address")))?;
        if !ids.insert(*id) {
            return Err(ConfigError::AssetLedger(format!(
                "{field}: id {id} is duplicated"
            )));
        }
        if out.insert(addr, *id).is_some() {
            return Err(ConfigError::AssetLedger(format!(
                "{field}: {addr:#x} is duplicated"
            )));
        }
    }
    Ok(out)
}
