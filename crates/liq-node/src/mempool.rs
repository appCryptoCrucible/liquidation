//! Public-mempool producer (GUIDE 03 §4b, §6; RUST-CONV §3.1).
//!
//! SPSC `rtrb` of [`liq_types::PendingTx`] with [`liq_types::RING_CAP`]
//! (4096). This is the producer 06D (`liq-oracle`) drains. Types live in
//! `liq-types` (D1) — this module must not redefine them.
//!
//! Full ring: never block canonical ingest. Drop-oldest: overwrite by popping
//! the resident consumer is impossible (06D owns it). The producer therefore
//! **rejects the incoming tx after counting** when `push` fails — that is the
//! GUIDE §4b snippet — and logs. A second local overwrite buffer would
//! silently desync 06D's consumer from what this process observed.

use liq_types::{PendingTx, RING_CAP};
use rtrb::{Consumer, Producer, RingBuffer};

/// Producer half. Single writer, no lock.
pub struct MempoolProducer {
    prod: Producer<PendingTx>,
    dropped: u64,
}

/// Split the 06D ring. Capacity is [`RING_CAP`], not a local constant.
#[must_use]
pub fn split_mempool() -> (MempoolProducer, Consumer<PendingTx>) {
    let (prod, cons) = RingBuffer::<PendingTx>::new(RING_CAP);
    (MempoolProducer { prod, dropped: 0 }, cons)
}

impl MempoolProducer {
    /// Never blocks. On full: count, log, drop the incoming item.
    pub fn push(&mut self, tx: PendingTx) {
        if self.prod.push(tx).is_err() {
            self.dropped = self.dropped.saturating_add(1);
            metrics::counter!("mempool_dropped").increment(1);
            tracing::error!(
                dropped = self.dropped,
                cap = RING_CAP,
                "mempool ring full; pending tx dropped (counted)"
            );
        }
    }

    #[inline]
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
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
    use super::{split_mempool, RING_CAP};
    use alloy_primitives::{Address, Bytes, B256};
    use liq_types::PendingTx;

    fn tx(i: u8) -> PendingTx {
        PendingTx {
            hash: B256::repeat_byte(i),
            to: Address::repeat_byte(i),
            input: Bytes::from(vec![i]),
        }
    }

    /// Oracle: D1 — producer uses liq_types::RING_CAP == 4096.
    #[test]
    fn ring_cap_is_liq_types() {
        assert_eq!(RING_CAP, 4096);
        assert_eq!(liq_types::RING_CAP, RING_CAP);
    }

    /// Oracle: overflow is counted. Negative: silent drop would leave
    /// dropped() == 0 after a full ring + one more.
    #[test]
    fn full_ring_counts_drop_and_does_not_block() {
        let (mut prod, mut cons) = split_mempool();
        for i in 0..RING_CAP {
            prod.push(tx((i % 251) as u8));
        }
        assert_eq!(prod.dropped(), 0);
        prod.push(tx(255));
        assert_eq!(prod.dropped(), 1);
        prod.push(tx(254));
        assert_eq!(prod.dropped(), 2);
        let mut n = 0usize;
        while cons.pop().is_ok() {
            n += 1;
        }
        assert_eq!(n, RING_CAP);
    }

    /// 06D drain shape: consumer pops until empty, never waits.
    #[test]
    fn consumer_drain_never_waits() {
        let (mut prod, mut cons) = split_mempool();
        prod.push(tx(1));
        prod.push(tx(2));
        assert!(cons.pop().is_ok());
        assert!(cons.pop().is_ok());
        assert!(cons.pop().is_err());
    }
}
