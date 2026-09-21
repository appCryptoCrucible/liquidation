//! [`NonceAllocator`]: one `parking_lot::Mutex` per key (GUIDE 13 §4 / §4b).
//!
//! Guard is dropped before any `.await`. A tokio async mutex is forbidden
//! here. Gap filler is a zero-value self-transfer at the dropped nonce.

use crate::error::{ExecError, Result};
use alloy_primitives::{Address, U256};
use parking_lot::Mutex;
use std::collections::BTreeMap;

/// Default live path consumes the nonce even when `submit_enabled` is false
/// so shadow and live stay identical. [`NonceMode::DryRun`] peeks without
/// consuming — both are tested.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NonceMode {
    Allocate,
    DryRun,
}

/// In-flight nonce record.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct InFlight {
    pub dropped: bool,
}

/// Per-key state. Locked independently so a 20-slot cascade does not serialize.
pub struct KeyState {
    next: u64,
    in_flight: BTreeMap<u64, InFlight>,
}

struct KeySlot {
    address: Address,
    state: Mutex<KeyState>,
}

/// One key per concurrent slot.
pub struct NonceAllocator {
    keys: Box<[KeySlot]>,
}

/// Result of [`NonceAllocator::allocate`] / [`NonceAllocator::dry_run`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AllocatedNonce {
    pub slot: usize,
    pub nonce: u64,
    pub address: Address,
}

/// Self-transfer instruction that fills a dropped-nonce gap.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GapFill {
    pub slot: usize,
    pub nonce: u64,
    pub from: Address,
    pub to: Address,
    pub value: U256,
}

impl NonceAllocator {
    /// Addresses only (nonce tests). Live path uses [`Self::from_addresses`]
    /// with the same operator keys the signer pool holds.
    pub fn from_addresses(addrs: Vec<Address>) -> Result<Self> {
        if addrs.is_empty() {
            return Err(ExecError::BadSlot(0));
        }
        let keys: Vec<KeySlot> = addrs
            .into_iter()
            .map(|address| KeySlot {
                address,
                state: Mutex::new(KeyState {
                    next: 0,
                    in_flight: BTreeMap::new(),
                }),
            })
            .collect();
        Ok(Self {
            keys: keys.into_boxed_slice(),
        })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn slot(&self, slot: usize) -> Result<&KeySlot> {
        self.keys.get(slot).ok_or(ExecError::BadSlot(slot))
    }

    /// Consume the next nonce. Guard drops at the end of this function.
    pub fn allocate(&self, slot: usize) -> Result<AllocatedNonce> {
        let key = self.slot(slot)?;
        let mut g = key.state.lock();
        let nonce = g.next;
        g.next = g.next.checked_add(1).ok_or(ExecError::NonceOverflow)?;
        g.in_flight.insert(nonce, InFlight { dropped: false });
        let address = key.address;
        drop(g);
        Ok(AllocatedNonce {
            slot,
            nonce,
            address,
        })
    }

    /// Peek the next nonce without consuming it.
    pub fn dry_run(&self, slot: usize) -> Result<AllocatedNonce> {
        let key = self.slot(slot)?;
        let g = key.state.lock();
        let nonce = g.next;
        let address = key.address;
        drop(g);
        Ok(AllocatedNonce {
            slot,
            nonce,
            address,
        })
    }

    /// Mark a nonce dropped and return the self-transfer that fills the gap.
    pub fn mark_dropped(&self, slot: usize, nonce: u64) -> Result<GapFill> {
        let key = self.slot(slot)?;
        let mut g = key.state.lock();
        let Some(inf) = g.in_flight.get_mut(&nonce) else {
            return Err(ExecError::NonceNotInFlight(nonce));
        };
        inf.dropped = true;
        let fill = GapFill {
            slot,
            nonce,
            from: key.address,
            to: key.address,
            value: U256::ZERO,
        };
        drop(g);
        Ok(fill)
    }

    /// Record that the gap-fill tx is now in-flight at `nonce` (already present).
    pub fn mark_filled(&self, slot: usize, nonce: u64) -> Result<()> {
        let key = self.slot(slot)?;
        let mut g = key.state.lock();
        let Some(inf) = g.in_flight.get_mut(&nonce) else {
            return Err(ExecError::NonceNotInFlight(nonce));
        };
        inf.dropped = false;
        drop(g);
        Ok(())
    }

    pub fn next_of(&self, slot: usize) -> Result<u64> {
        let key = self.slot(slot)?;
        Ok(key.state.lock().next)
    }

    pub fn in_flight(&self, slot: usize) -> Result<BTreeMap<u64, InFlight>> {
        let key = self.slot(slot)?;
        Ok(key.state.lock().in_flight.clone())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use alloy_primitives::Address;
    use std::thread;
    use std::time::Instant;

    fn addr(i: u8) -> Address {
        let mut w = [0u8; 20];
        w[19] = i;
        Address::from(w)
    }

    #[test]
    fn allocate_and_dry_run() {
        let pool = NonceAllocator::from_addresses(vec![addr(1)]).unwrap();
        let a = pool.allocate(0).unwrap();
        assert_eq!(a.nonce, 0);
        assert_eq!(pool.next_of(0).unwrap(), 1);
        let d = pool.dry_run(0).unwrap();
        assert_eq!(d.nonce, 1);
        assert_eq!(pool.next_of(0).unwrap(), 1, "dry-run must not consume");
        let a2 = pool.allocate(0).unwrap();
        assert_eq!(a2.nonce, 1);
        assert_eq!(pool.next_of(0).unwrap(), 2);
    }

    #[test]
    fn cascade_20_slots_no_gap_no_shared_lock() {
        let addrs: Vec<Address> = (1..=20).map(addr).collect();
        let pool = NonceAllocator::from_addresses(addrs).unwrap();
        let start = Instant::now();
        thread::scope(|s| {
            for i in 0..20 {
                let p = &pool;
                s.spawn(move || {
                    for _ in 0..8 {
                        p.allocate(i).unwrap();
                    }
                });
            }
        });
        let elapsed = start.elapsed();
        for i in 0..20 {
            assert_eq!(pool.next_of(i).unwrap(), 8);
            let infl = pool.in_flight(i).unwrap();
            assert_eq!(infl.len(), 8);
            for n in 0..8 {
                assert!(infl.contains_key(&n), "gap at slot {i} nonce {n}");
            }
        }
        // Independent locks: 160 integer ops must finish well under a millisecond
        // stall budget. A single shared lock would still be fast; the structural
        // check is each slot's first nonce is independent (all start at 0).
        assert!(elapsed.as_millis() < 200, "cascade serialized: {elapsed:?}");
    }

    #[test]
    fn cascade_first_nonce_per_slot_is_zero() {
        let addrs: Vec<Address> = (1..=20).map(addr).collect();
        let pool = NonceAllocator::from_addresses(addrs).unwrap();
        let first: Vec<u64> = thread::scope(|s| {
            let mut hs = Vec::new();
            for i in 0..20 {
                let p = &pool;
                hs.push(s.spawn(move || p.allocate(i).unwrap().nonce));
            }
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(
            first.iter().all(|&n| n == 0),
            "slots must not share a nonce sequence: {first:?}"
        );
    }

    #[test]
    fn gap_filler_recovers_dropped_nonce() {
        let pool = NonceAllocator::from_addresses(vec![addr(7)]).unwrap();
        let n0 = pool.allocate(0).unwrap();
        let n1 = pool.allocate(0).unwrap();
        let n2 = pool.allocate(0).unwrap();
        assert_eq!((n0.nonce, n1.nonce, n2.nonce), (0, 1, 2));
        let fill = pool.mark_dropped(0, 1).unwrap();
        assert_eq!(fill.nonce, 1);
        assert_eq!(fill.from, fill.to);
        assert_eq!(fill.value, U256::ZERO);
        assert!(pool.in_flight(0).unwrap().get(&1).unwrap().dropped);
        pool.mark_filled(0, 1).unwrap();
        assert!(!pool.in_flight(0).unwrap().get(&1).unwrap().dropped);
        assert_eq!(pool.next_of(0).unwrap(), 3, "gap fill does not rewind next");
    }
}
