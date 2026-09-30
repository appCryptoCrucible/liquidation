//! Changing what the log handlers subscribe to while the node runs.
//!
//! The hot thread routes logs by a table built from its handlers'
//! subscriptions, and the ExEx forwards only receipts from the addresses in
//! that table. Both are fixed at startup unless something asks for a
//! rebuild: a handler that starts following a new address (a pool added to
//! the book from the registry, a Uniswap V3 pool created on chain) calls
//! [`Resubscribe::request`]; the hot thread rebuilds its router from the same
//! handlers, publishes the new address set here for the ExEx, and marks the
//! request applied. Logs of blocks forwarded before that are not seen by the
//! new subscriptions — a caller that must not miss one waits for
//! [`Resubscribe::wait_applied`] and reads state from chain after it.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_primitives::Address;
use arc_swap::ArcSwap;

/// Shared by the hot thread, the ExEx forwarder and whoever changes
/// subscriptions.
#[derive(Debug, Default)]
pub struct Resubscribe {
    requested: AtomicU64,
    applied: AtomicU64,
    tracked: ArcSwap<HashSet<Address>>,
}

impl Resubscribe {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the hot thread to rebuild its router. Returns the epoch to wait on.
    pub fn request(&self) -> u64 {
        self.requested
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
    }

    #[must_use]
    pub fn requested(&self) -> u64 {
        self.requested.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn applied(&self) -> u64 {
        self.applied.load(Ordering::Acquire)
    }

    /// Addresses whose receipts the ExEx forwards.
    #[must_use]
    pub fn tracked(&self) -> Arc<HashSet<Address>> {
        self.tracked.load_full()
    }

    /// Publish the router's address set and mark `epoch` applied. The set is
    /// stored first, so a caller that sees the epoch sees the set.
    pub fn publish(&self, tracked: HashSet<Address>, epoch: u64) {
        self.tracked.store(Arc::new(tracked));
        self.applied.store(epoch, Ordering::Release);
    }

    /// Wait until the hot thread has applied `epoch` (or a later one).
    /// `false` on timeout.
    #[must_use]
    pub fn wait_applied(&self, epoch: u64, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            if self.applied() >= epoch {
                return true;
            }
            if deadline.is_none_or(|d| Instant::now() >= d) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_applied_in_order_and_the_set_is_published_first() {
        let r = Resubscribe::new();
        assert!(r.tracked().is_empty());
        let e1 = r.request();
        let e2 = r.request();
        assert_eq!((e1, e2, r.requested()), (1, 2, 2));
        assert!(!r.wait_applied(e1, Duration::from_millis(10)));
        let a = Address::repeat_byte(7);
        r.publish(HashSet::from([a]), e2);
        assert!(
            r.wait_applied(e1, Duration::ZERO),
            "a later epoch covers an earlier one"
        );
        assert!(r.tracked().contains(&a));
    }
}
