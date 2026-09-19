//! Interning (GUIDE 02 §1): dense ids assigned in first-sight order.
//!
//! Position ids are **never reused or removed** while the block they were
//! created in stands; the one exception is a `Created` undo (the block was
//! reorged away, so the position never existed on the canonical chain), which
//! pops the most recent id. A position that repays to zero keeps its id.
//!
//! Asset ids are global (GUIDE 00 §3): one `Address` is one [`AssetId`] across
//! every protocol, so the interner is keyed by address alone.

use std::collections::HashMap;

use alloy_primitives::Address;
use liq_types::{AssetId, PositionId, PositionKey};

use crate::error::StateError;

/// Reverse-table entry for one position: its natural key (what `PositionRef::key`
/// borrows) and its index inside its market's balance block. Co-located so the
/// read path pays one line for both; `align(32)` puts exactly two per cache
/// line, never straddling.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(C, align(32))]
pub(crate) struct PosEntry {
    pub(crate) key: PositionKey,
    pub(crate) local: u32,
}

const _: () = assert!(core::mem::size_of::<PosEntry>() == 32);

/// `PositionKey ↔ PositionId`, dense.
pub(crate) struct PositionTable {
    map: HashMap<PositionKey, PositionId>,
    pub(crate) entries: Vec<PosEntry>,
}

impl PositionTable {
    pub(crate) fn with_capacity(n: usize) -> Self {
        Self {
            map: HashMap::with_capacity(n),
            entries: Vec::with_capacity(n),
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    pub(crate) fn get(&self, key: &PositionKey) -> Option<PositionId> {
        self.map.get(key).copied()
    }

    #[inline]
    pub(crate) fn entry(&self, i: usize) -> Option<&PosEntry> {
        self.entries.get(i)
    }

    /// The id the next `push` will receive; `None` when `u32` is exhausted.
    #[inline]
    pub(crate) fn next_id(&self) -> Option<PositionId> {
        u32::try_from(self.entries.len()).ok().map(PositionId)
    }

    /// Append `key` as `id` (the caller passes [`Self::next_id`]).
    #[inline]
    pub(crate) fn push(&mut self, key: PositionKey, id: PositionId, local: u32) {
        self.map.insert(key, id);
        self.entries.push(PosEntry { key, local });
    }

    /// Remove the most recent position (a `Created` undo).
    #[inline]
    pub(crate) fn pop(&mut self) -> Option<PosEntry> {
        let e = self.entries.pop()?;
        self.map.remove(&e.key);
        Some(e)
    }
}

/// `Address ↔ AssetId`, global across protocols. Populated from config at
/// startup; never mutated on the hot path.
#[derive(Clone, Debug, Default)]
pub struct AssetInterner {
    map: HashMap<Address, AssetId>,
    addrs: Vec<Address>,
}

impl AssetInterner {
    #[must_use]
    pub fn with_capacity(n: usize) -> Self {
        Self {
            map: HashMap::with_capacity(n),
            addrs: Vec::with_capacity(n),
        }
    }

    /// Id for `addr`, assigning the next dense one on first sight.
    pub fn intern(&mut self, addr: Address) -> Result<AssetId, StateError> {
        if let Some(id) = self.map.get(&addr) {
            return Ok(*id);
        }
        let id = u16::try_from(self.addrs.len())
            .map(AssetId)
            .map_err(|_| StateError::AssetIdsExhausted)?;
        self.map.insert(addr, id);
        self.addrs.push(addr);
        Ok(id)
    }

    #[inline]
    #[must_use]
    pub fn get(&self, addr: Address) -> Option<AssetId> {
        self.map.get(&addr).copied()
    }

    #[inline]
    #[must_use]
    pub fn address(&self, id: AssetId) -> Option<Address> {
        self.addrs.get(usize::from(id.0)).copied()
    }

    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.addrs.len()
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.addrs.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::AssetInterner;
    use alloy_primitives::{address, Address};
    use liq_types::AssetId;

    /// Mainnet WETH9. Oracle: the chain.
    const WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    /// Mainnet USDC. Oracle: the chain.
    const USDC: Address = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

    /// Carry-forward from the 00C review: the production interner must
    /// re-assert the global-`AssetId` property against its own code. Oracle:
    /// the chain (one WETH9 contract) + GUIDE 00 §3 (id is a function of the
    /// address, not the protocol). Negative: two contracts are two ids.
    #[test]
    fn weth_from_two_protocol_configs_is_one_asset_id() {
        let aave_tokens = [WETH, USDC];
        let morpho_tokens = [WETH];
        let mut it = AssetInterner::with_capacity(4);
        let weth_aave = it.intern(aave_tokens[0]).unwrap();
        let usdc_aave = it.intern(aave_tokens[1]).unwrap();
        let weth_morpho = it.intern(morpho_tokens[0]).unwrap();
        assert_eq!(weth_aave, weth_morpho);
        assert_ne!(weth_aave, usdc_aave);
        assert_eq!(it.len(), 2, "two distinct contracts, two ids");
        assert_eq!(it.address(weth_aave), Some(WETH));
        assert_eq!(it.get(USDC), Some(usdc_aave));
        assert_eq!(it.address(AssetId(2)), None);
    }
}
