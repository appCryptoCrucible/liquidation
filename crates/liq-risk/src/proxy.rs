//! EIP-1967 implementation-slot watcher (GUIDE 14 §1). Protocol **and** flash providers.

use alloy_primitives::{Address, B256};
use tracing::{error, warn};

use liq_types::{FlashProvider, HaltReason, HaltScope, HaltSink, ProtocolId};

/// `bytes32(uint256(keccak256('eip1967.proxy.implementation')) - 1)`
pub const IMPLEMENTATION_SLOT: B256 = B256::new([
    0x36, 0x08, 0x94, 0xa1, 0x3b, 0xa1, 0xa3, 0x21, 0x06, 0x67, 0xc8, 0x28, 0x49, 0x2d, 0xb9, 0x8d,
    0xca, 0x3e, 0x20, 0x76, 0xcc, 0x37, 0x35, 0xa9, 0x20, 0xa3, 0xca, 0x50, 0x5d, 0x38, 0x2b, 0xbc,
]);

/// Storage reader. 03/node supplies the live impl; tests inject a map. Network
/// failure is an error, not a guessed slot.
pub trait SlotReader {
    fn storage_at(&self, address: Address, slot: B256) -> Result<B256, SlotError>;
}

#[derive(Debug, thiserror::Error)]
pub enum SlotError {
    #[error("storage read failed for {address}")]
    ReadFailed { address: Address },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WatchKind {
    Protocol(ProtocolId),
    Flash { provider: FlashProvider },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WatchTarget {
    pub address: Address,
    pub kind: WatchKind,
}

/// Last-seen implementation per watched proxy.
pub struct ProxyWatcher {
    targets: Vec<WatchTarget>,
    last: Vec<B256>,
}

impl ProxyWatcher {
    #[must_use]
    pub fn new(targets: Vec<WatchTarget>) -> Self {
        let n = targets.len();
        Self {
            targets,
            last: vec![B256::ZERO; n],
        }
    }

    #[must_use]
    pub fn targets(&self) -> &[WatchTarget] {
        &self.targets
    }

    /// Seed last-seen without halting (startup).
    pub fn seed(&mut self, reader: &dyn SlotReader) -> Result<(), SlotError> {
        for (i, t) in self.targets.iter().enumerate() {
            let v = reader.storage_at(t.address, IMPLEMENTATION_SLOT)?;
            if let Some(slot) = self.last.get_mut(i) {
                *slot = v;
            }
        }
        Ok(())
    }

    /// Compare every configured address. Implementation change → Class B halt.
    pub fn poll(&mut self, reader: &dyn SlotReader, sink: &dyn HaltSink) -> Result<u32, SlotError> {
        let mut changed = 0u32;
        for (i, t) in self.targets.iter().enumerate() {
            let now = reader.storage_at(t.address, IMPLEMENTATION_SLOT)?;
            let prev = self.last.get(i).copied().unwrap_or(B256::ZERO);
            if prev != B256::ZERO && now != prev {
                changed = changed.saturating_add(1);
                let impl_addr = impl_from_slot(now);
                warn!(
                    address = ?t.address,
                    new_impl = ?impl_addr,
                    ?t.kind,
                    "EIP-1967 implementation changed"
                );
                match t.kind {
                    WatchKind::Protocol(p) => {
                        sink.halt(HaltScope::Protocol(p), HaltReason::ProxyUpgrade);
                    }
                    WatchKind::Flash { provider } => {
                        sink.halt(HaltScope::FlashProvider(provider), HaltReason::ProxyUpgrade);
                    }
                }
            }
            if let Some(slot) = self.last.get_mut(i) {
                *slot = now;
            }
        }
        Ok(changed)
    }
}

fn impl_from_slot(slot: B256) -> Address {
    Address::from_word(slot)
}

/// Fail closed: empty watch list is a config bug once protocols exist.
pub fn assert_watch_covers_config(configured: &[WatchTarget], watcher: &ProxyWatcher) -> bool {
    if configured.is_empty() {
        error!("proxy watch list empty while config named addresses");
        return false;
    }
    for c in configured {
        if !watcher
            .targets()
            .iter()
            .any(|t| t.address == c.address && t.kind == c.kind)
        {
            error!(address = ?c.address, "configured address not in proxy watcher");
            return false;
        }
    }
    true
}

/// Word-to-address: implementation is the low 20 bytes of the slot.
pub fn implementation_address(word: B256) -> Address {
    impl_from_slot(word)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::gate::RiskGate;
    use alloy_primitives::{address, b256};
    use std::collections::HashMap;

    struct MapReader(HashMap<Address, B256>);
    impl SlotReader for MapReader {
        fn storage_at(&self, address: Address, slot: B256) -> Result<B256, SlotError> {
            assert_eq!(slot, IMPLEMENTATION_SLOT);
            self.0
                .get(&address)
                .copied()
                .ok_or(SlotError::ReadFailed { address })
        }
    }

    fn word(a: Address) -> B256 {
        a.into_word()
    }

    #[test]
    fn enumerates_protocol_and_flash_addresses() {
        let p = address!("0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");
        let f = address!("0x2e1e3c1a1f38e9a23e6049f93cb1e157824f3c27");
        let configured = vec![
            WatchTarget {
                address: p,
                kind: WatchKind::Protocol(ProtocolId(0)),
            },
            WatchTarget {
                address: f,
                kind: WatchKind::Flash {
                    provider: FlashProvider::UniV4,
                },
            },
        ];
        let w = ProxyWatcher::new(configured.clone());
        assert!(assert_watch_covers_config(&configured, &w));
        assert_eq!(w.targets().len(), 2);
    }

    #[test]
    fn slot_change_halts_that_protocol_only() {
        let aave = address!("0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");
        let morpho = address!("0xbbbbbbbbbb9cc5e90e3b3af64bdaf62c37eeffcb");
        let impl1 = address!("0x1111111111111111111111111111111111111111");
        let impl2 = address!("0x2222222222222222222222222222222222222222");
        let mut slots = HashMap::new();
        slots.insert(aave, word(impl1));
        slots.insert(morpho, word(impl1));
        let mut w = ProxyWatcher::new(vec![
            WatchTarget {
                address: aave,
                kind: WatchKind::Protocol(ProtocolId(0)),
            },
            WatchTarget {
                address: morpho,
                kind: WatchKind::Protocol(ProtocolId(1)),
            },
        ]);
        let gate = RiskGate::new();
        w.seed(&MapReader(slots.clone())).unwrap();
        slots.insert(aave, word(impl2));
        let n = w.poll(&MapReader(slots), &gate).unwrap();
        assert_eq!(n, 1);
        use crate::gate::{Allow, AllowQuery};
        use liq_types::{AssetId, MarketId, TraceId, TriggerKind};
        let q0 = AllowQuery {
            protocol: ProtocolId(0),
            market: MarketId(0),
            collateral: AssetId(0),
            debt: AssetId(1),
            flash: FlashProvider::Aave,
            trigger: TriggerKind::OraclePublic,
            operator_key: Address::ZERO,
        };
        let q1 = AllowQuery {
            protocol: ProtocolId(1),
            ..q0
        };
        assert!(matches!(
            gate.allow(TraceId::from_raw(1), &q0),
            Allow::Denied {
                reason: HaltReason::ProxyUpgrade,
                ..
            }
        ));
        assert_eq!(gate.allow(TraceId::from_raw(1), &q1), Allow::Yes);
        assert_eq!(gate.action_required().len(), 1);
    }

    #[test]
    fn implementation_slot_matches_eip_1967() {
        assert_eq!(
            IMPLEMENTATION_SLOT,
            b256!("0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc")
        );
        let _ = alloy_primitives::U256::from_be_bytes(IMPLEMENTATION_SLOT.0);
    }

    #[test]
    fn missing_slot_fails_closed() {
        let addr = address!("0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");
        let mut w = ProxyWatcher::new(vec![WatchTarget {
            address: addr,
            kind: WatchKind::Protocol(ProtocolId(0)),
        }]);
        let r = MapReader(HashMap::new());
        assert!(w.seed(&r).is_err());
    }
}
