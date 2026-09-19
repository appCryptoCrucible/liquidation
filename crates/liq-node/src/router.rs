//! Union filter over every [`LogSubscriber`], dispatched by `(address, topic0)`
//! hash lookup + [`EventKind`] jump table (GUIDE 03 §2). O(1) + O(k).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use alloy_primitives::{Address, B256};
use liq_protocol::DecodedLog;
use liq_types::LogSubscriber;
use smallvec::SmallVec;

use crate::decode::{decode_typed, fill_topic0, DecodeArena, EventKind};
use crate::source::OwnedLog;
use crate::{IngestError, Result};

/// Subscriber index into the slice passed to [`LogRouter::from_subscribers`].
pub type SubIdx = u16;

/// Outcome of routing one log.
#[derive(Clone, Debug)]
pub enum Route<'a> {
    /// Address is not in the union filter.
    Untracked,
    /// Address is tracked, topic0 is not in any filter for that address.
    /// Not an error (carry-forward: BorrowAllowanceDelegated / DelegateChanged).
    UnknownTopic,
    /// Dispatch to `subs`, borrowed from the router's dispatch map — never
    /// cloned, so a topic with more subscribers than the inline capacity
    /// cannot spill to the heap on the hot path.
    Hit {
        kind: Option<EventKind>,
        decoded: DecodedLog<'a>,
        subs: &'a [SubIdx],
    },
}

/// Hash + jump table built once at startup.
pub struct LogRouter {
    topic0: HashMap<B256, EventKind>,
    dispatch: HashMap<(Address, B256), SmallVec<[SubIdx; 2]>>,
    tracked: HashSet<Address>,
    unknown_topic: AtomicU64,
}

impl LogRouter {
    /// Collect every subscriber's [`liq_types::LogFilter`]. Duplicate
    /// `(address, topic0, idx)` pairs are stored once.
    pub fn from_subscribers(subs: &[&dyn LogSubscriber]) -> Result<Self> {
        let mut topic0 = HashMap::with_capacity(128);
        fill_topic0(&mut topic0);
        let mut dispatch: HashMap<(Address, B256), SmallVec<[SubIdx; 2]>> =
            HashMap::with_capacity(subs.len().saturating_mul(8));
        let mut tracked = HashSet::with_capacity(subs.len().saturating_mul(4));
        for (i, s) in subs.iter().enumerate() {
            let idx = u16::try_from(i).map_err(|_| IngestError::TooManySubscribers)?;
            for f in s.subscriptions() {
                tracked.insert(f.address);
                let slot = dispatch.entry((f.address, f.topic0)).or_default();
                if !slot.contains(&idx) {
                    slot.push(idx);
                }
            }
        }
        Ok(Self {
            topic0,
            dispatch,
            tracked,
            unknown_topic: AtomicU64::new(0),
        })
    }

    /// Known-address / unknown-topic skips since process start.
    #[inline]
    #[must_use]
    pub fn unknown_topic_skips(&self) -> u64 {
        self.unknown_topic.load(Ordering::Relaxed)
    }

    /// Route one log. Copies into `arena`. Does not error on unknown topic0
    /// at a tracked address.
    pub fn route<'a>(&'a self, arena: &'a DecodeArena, log: &OwnedLog) -> Result<Route<'a>> {
        // Hit first: the union filter means most logs match, so the common
        // path costs one hash. `tracked` is only consulted on a miss.
        let Some(topic0) = log.topics.first() else {
            // Anonymous log. At a tracked address that is malformed input;
            // anywhere else it is somebody else's contract.
            return if self.tracked.contains(&log.address) {
                Err(IngestError::MalformedLog)
            } else {
                Ok(Route::Untracked)
            };
        };
        let Some(subs) = self.dispatch.get(&(log.address, *topic0)) else {
            if !self.tracked.contains(&log.address) {
                return Ok(Route::Untracked);
            }
            self.unknown_topic.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("ingest_unknown_topic").increment(1);
            return Ok(Route::UnknownTopic);
        };
        let decoded = arena.copy(log);
        let kind = self.topic0.get(topic0).copied();
        if let Some(k) = kind {
            decode_typed(k, decoded.topics, decoded.data)?;
        }
        Ok(Route::Hit {
            kind,
            decoded,
            subs,
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::{LogRouter, Route};
    use crate::decode::{encode_v4_supply, DecodeArena, ISpoke};
    use crate::source::OwnedLog;
    use alloy_primitives::{Address, B256, U256};
    use alloy_sol_types::SolEvent;
    use arrayvec::ArrayVec;
    use liq_types::{LogFilter, LogSubscriber};

    struct Sub {
        filters: Vec<LogFilter>,
    }
    impl LogSubscriber for Sub {
        fn subscriptions(&self) -> Vec<LogFilter> {
            self.filters.clone()
        }
    }

    fn addr(b: u8) -> Address {
        Address::repeat_byte(b)
    }

    /// Oracle: GUIDE 03 §2 — dispatch is by (address, topic0), not a linear
    /// scan of event names. Negative: a second subscriber at another address
    /// is not invoked.
    #[test]
    fn dispatch_is_hash_plus_subscribers_for_topic() {
        let t0 = ISpoke::Supply::SIGNATURE_HASH;
        let a = addr(1);
        let s0 = Sub {
            filters: vec![LogFilter {
                address: a,
                topic0: t0,
            }],
        };
        let s1 = Sub {
            filters: vec![LogFilter {
                address: addr(2),
                topic0: t0,
            }],
        };
        let subs: [&dyn LogSubscriber; 2] = [&s0, &s1];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let log =
            encode_v4_supply(a, U256::from(1), a, a, U256::from(1), U256::from(1), 1, 1).unwrap();
        let arena = DecodeArena::with_capacity(4096);
        match router.route(&arena, &log).unwrap() {
            Route::Hit { subs, .. } => {
                assert_eq!(subs, [0]);
            }
            other => panic!("expected hit, got {other:?}"),
        }
    }

    /// Oracle: STATE.md carry-forward 03A/04B. Known address, topic0 not in
    /// any filter — skip, do not error.
    #[test]
    fn known_address_unknown_topic_is_not_an_error() {
        let a = addr(9);
        let s0 = Sub {
            filters: vec![LogFilter {
                address: a,
                topic0: ISpoke::Supply::SIGNATURE_HASH,
            }],
        };
        let subs: [&dyn LogSubscriber; 1] = [&s0];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let mut topics = ArrayVec::new();
        // BorrowAllowanceDelegated topic0 prefix from the coverage carry-forward.
        let t = B256::from(alloy_primitives::keccak256(
            b"BorrowAllowanceDelegated(address,address,address,uint256)",
        ));
        topics.push(t);
        let log = OwnedLog {
            address: a,
            topics,
            data: Vec::new(),
            block: 1,
            timestamp: 1,
            tx_index: 0,
            log_index: 0,
        };
        let arena = DecodeArena::with_capacity(4096);
        match router.route(&arena, &log).unwrap() {
            Route::UnknownTopic => {}
            other => panic!("expected UnknownTopic, got {other:?}"),
        }
        assert_eq!(router.unknown_topic_skips(), 1);
    }

    /// Oracle: GUIDE 03 §2 — the union filter is over subscribed addresses.
    /// An address nobody subscribed to is `Untracked`, and it does **not**
    /// count as a known-address/unknown-topic skip. Negative: an anonymous
    /// (zero-topic) log from such an address is still not an error, because
    /// the ExEx feed carries every log in the block, not just ours.
    #[test]
    fn untracked_address_is_skipped_without_counting() {
        let s0 = Sub {
            filters: vec![LogFilter {
                address: addr(1),
                topic0: ISpoke::Supply::SIGNATURE_HASH,
            }],
        };
        let subs: [&dyn LogSubscriber; 1] = [&s0];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let arena = DecodeArena::with_capacity(4096);

        let foreign = encode_v4_supply(
            addr(2),
            U256::from(1),
            addr(2),
            addr(2),
            U256::from(1),
            U256::from(1),
            1,
            1,
        )
        .unwrap();
        match router.route(&arena, &foreign).unwrap() {
            Route::Untracked => {}
            other => panic!("expected Untracked, got {other:?}"),
        }

        let anonymous = OwnedLog {
            address: addr(2),
            topics: ArrayVec::new(),
            data: Vec::new(),
            block: 1,
            timestamp: 1,
            tx_index: 0,
            log_index: 0,
        };
        match router.route(&arena, &anonymous).unwrap() {
            Route::Untracked => {}
            other => panic!("expected Untracked, got {other:?}"),
        }
        assert_eq!(router.unknown_topic_skips(), 0);
    }
}
