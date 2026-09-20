//! Candidate emission (GUIDE 08 §5) and the bounded priority queue (§6).
//!
//! A [`Candidate`] passes the adapter's [`Quote`] through **intact** — every
//! `repay_options × seize_options` pair, each `SeizeOption` with its
//! [`liq_protocol::BonusCurve`] — so GUIDE 12 decides *when* to fire on a
//! `HealthLinear` curve with its competitor model; the engine only surfaces
//! the position. [`TriggerCause`] is the full GUIDE 08 set: GUIDE 13
//! branches on it for venue and bid strategy, so an `InterestDrift`
//! candidate never pays an auction bid and an `SvrAuction` one is never sent
//! as a naked public transaction.
//!
//! [`CandidateQueue`] is bounded and ordered by `(fireable, est_value)`:
//! under a market-wide cascade it keeps the most valuable candidates, not
//! the first, evicting the lowest and **counting** every drop. Pre-warm
//! (`OraclePredicted`) candidates rank below every fireable one so they can
//! never evict a real opportunity.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::time::Instant;

use alloy_primitives::{Address, Bytes, TxHash, B256, U256};
use liq_protocol::{BlockNum, FlashRoute, Health, LegChoice, Quote};
use liq_types::{AssetId, Confidence, MarketId, PositionId, ProtocolId, TraceId, TriggerKind, Wad};

/// Why a candidate exists. Payloads stay here; [`TriggerKind`] in
/// `liq-types` is the payload-free mirror the halt matrix scopes on
/// (`HaltScope::Trigger`), derived by [`TriggerCause::kind`] — never
/// re-enumerated (00C carry-forward).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TriggerCause {
    // ── auctioned: bid against other searchers via the protocol's own
    //    recapture mechanism; margin compressed by design.
    /// SVR update announced on MEV-Share; `hint` is the hint's hash.
    SvrAuction { hint: B256, deadline: Instant },

    // ── contested but not auctioned: a latency/skill race.
    /// Public oracle transmit in the mempool; bundle behind it.
    OraclePublic { tx: TxHash },
    /// Pull oracle (Pyth/Redstone) update we hold and submit ourselves;
    /// `payload` is the signed update calldata.
    OraclePullHeld { payload: Bytes },
    /// TWAP / LP-collateral pool state moved.
    PoolStateChange { pool: Address },
    /// The borrower's own transaction moved them into range.
    UserAction { tx: TxHash },

    // ── uncontested: won by breadth and correctness, not speed.
    /// A derived rate moved (wstETH `stEthPerToken`, sDAI `chi`, an LRT).
    DerivedRate { source: AssetId },
    /// Governance parameter change; timelocked, schedulable in advance.
    ParamChange { market: MarketId },
    /// Accrual alone crossed the boundary (closed-form crossing time).
    InterestDrift,
    /// Liquidatable on canonical state and untaken.
    Stale { liquidatable_since: BlockNum },

    // ── not fireable: pre-warm only.
    /// Predicted aggregator output; compute, plan, sign — never submit.
    OraclePredicted { conf: Confidence },
}

impl TriggerCause {
    /// Payload-free class for halt scoping.
    #[inline]
    #[must_use]
    pub const fn kind(&self) -> TriggerKind {
        match self {
            Self::SvrAuction { .. } => TriggerKind::SvrAuction,
            Self::OraclePublic { .. } => TriggerKind::OraclePublic,
            Self::OraclePullHeld { .. } => TriggerKind::OraclePullHeld,
            Self::PoolStateChange { .. } => TriggerKind::PoolStateChange,
            Self::UserAction { .. } => TriggerKind::UserAction,
            Self::DerivedRate { .. } => TriggerKind::DerivedRate,
            Self::ParamChange { .. } => TriggerKind::ParamChange,
            Self::InterestDrift => TriggerKind::InterestDrift,
            Self::Stale { .. } => TriggerKind::Stale,
            Self::OraclePredicted { .. } => TriggerKind::OraclePredicted,
        }
    }

    /// `false` only for [`TriggerCause::OraclePredicted`]: a candidate that
    /// must never be submitted on this cause alone.
    #[inline]
    #[must_use]
    pub const fn fireable(&self) -> bool {
        !matches!(self, Self::OraclePredicted { .. })
    }
}

/// One liquidatable position with everything GUIDE 12 needs to size, fund,
/// route and price it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub position: PositionId,
    pub protocol: ProtocolId,
    /// Health at the evaluated price vector.
    pub health: Health,
    /// Intact adapter quote, `BonusCurve` per seize leg included.
    pub quote: Quote,
    /// The engine's eligibility winner (GUIDE 07 §5, D26: `bonus − cost`)
    /// — a fundable `(repay, seize)` pair and its flash route. GUIDE 12
    /// may re-search `quote` with exit costs; this pair is known fundable.
    pub legs: LegChoice,
    pub funding: FlashRoute,
    pub cause: TriggerCause,
    /// Ordering estimate only — the bonus the chosen pair would pay on the
    /// smaller of its two legs, in the price vector's numeraire (WAD). Not
    /// a profit figure: no exit cost, gas or flash fee (GUIDE 12 owns
    /// those). Never bid on it.
    pub est_value: Wad,
    /// Present only when the trigger carries one (SVR auction deadline).
    pub deadline: Option<Instant>,
    pub trace: TraceId,
}

impl Candidate {
    /// Whether the cause allows submission.
    #[inline]
    #[must_use]
    pub const fn fireable(&self) -> bool {
        self.cause.fireable()
    }
}

/// Heap key: fireable before pre-warm, then value, then age (older first
/// among equals), then the slot as a total-order tiebreak.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    fireable: bool,
    value: U256,
    age: Reverse<u64>,
    slot: u32,
}

/// Bounded, value-ordered candidate store. Single-threaded: the engine
/// pushes and the router drains on the same hot thread (RUST-CONVENTIONS
/// §3 — a stage-to-stage hop is a function call). Allocation-free after
/// construction: candidates live in a fixed slab, the heap holds 48-byte
/// keys.
#[derive(Debug)]
pub struct CandidateQueue {
    slots: Vec<Option<Candidate>>,
    free: Vec<u32>,
    heap: BinaryHeap<Reverse<Rank>>,
    seq: u64,
    dropped: u64,
    admitted: u64,
}

impl CandidateQueue {
    /// Fixed `capacity` (RUST-CONVENTIONS §3.4: 1024 for engine → router).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let mut slots = Vec::with_capacity(capacity);
        slots.resize_with(capacity, || None);
        let free = (0..capacity)
            .rev()
            .map(|i| u32::try_from(i).unwrap_or(u32::MAX))
            .collect();
        Self {
            slots,
            free,
            heap: BinaryHeap::with_capacity(capacity),
            seq: 0,
            dropped: 0,
            admitted: 0,
        }
    }

    #[inline]
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Candidates dropped because the queue was full — the incoming one
    /// when it ranked lowest, otherwise the evicted lowest. Nonzero in a
    /// calm market means the queue is undersized or the consumer is slow
    /// (GUIDE 08 §6).
    #[inline]
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Candidates accepted so far.
    #[inline]
    #[must_use]
    pub fn admitted(&self) -> u64 {
        self.admitted
    }

    /// Insert; when full, keep the higher-ranked of the incoming candidate
    /// and the current lowest. Returns whether `c` was admitted.
    pub fn push(&mut self, c: Candidate) -> bool {
        let age = Reverse(self.seq);
        self.seq = self.seq.wrapping_add(1);
        let probe = Rank {
            fireable: c.fireable(),
            value: c.est_value.raw(),
            age,
            slot: 0,
        };
        if self.free.is_empty() {
            let Some(&Reverse(lowest)) = self.heap.peek() else {
                return false;
            };
            if probe <= (Rank { slot: 0, ..lowest }) {
                self.dropped = self.dropped.saturating_add(1);
                return false;
            }
            self.heap.pop();
            if let Some(cell) = self.slots.get_mut(lowest.slot as usize) {
                *cell = None;
            }
            self.free.push(lowest.slot);
            self.dropped = self.dropped.saturating_add(1);
        }
        let Some(slot) = self.free.pop() else {
            return false;
        };
        if let Some(cell) = self.slots.get_mut(slot as usize) {
            *cell = Some(c);
        }
        self.heap.push(Reverse(Rank { slot, ..probe }));
        self.admitted = self.admitted.saturating_add(1);
        true
    }

    /// Every queued candidate, highest rank first, leaving the queue empty.
    /// Sorts in place (`into_sorted_vec`), allocates nothing, and hands the
    /// buffer back when the iterator drops.
    pub fn drain(&mut self) -> Drain<'_> {
        let sorted = std::mem::take(&mut self.heap).into_sorted_vec();
        Drain {
            q: self,
            sorted,
            next: 0,
        }
    }
}

/// See [`CandidateQueue::drain`].
#[derive(Debug)]
pub struct Drain<'a> {
    q: &'a mut CandidateQueue,
    /// Ascending `Reverse<Rank>` = descending rank.
    sorted: Vec<Reverse<Rank>>,
    next: usize,
}

impl Iterator for Drain<'_> {
    type Item = Candidate;

    fn next(&mut self) -> Option<Candidate> {
        loop {
            let &Reverse(r) = self.sorted.get(self.next)?;
            self.next = self.next.saturating_add(1);
            self.q.free.push(r.slot);
            if let Some(c) = self.q.slots.get_mut(r.slot as usize).and_then(Option::take) {
                return Some(c);
            }
        }
    }
}

impl Drop for Drain<'_> {
    fn drop(&mut self) {
        // Anything not yet yielded is discarded (its slot freed), then the
        // buffer goes back to the queue.
        while self.next().is_some() {}
        let mut v = std::mem::take(&mut self.sorted);
        v.clear();
        self.q.heap = BinaryHeap::from(v);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{Candidate, CandidateQueue, TriggerCause};
    use alloy_primitives::{Address, B256, U256};
    use liq_protocol::{
        AssetMask, CallbackShape, FlashRoute, Health, HealthState, LegChoice, Quote,
    };
    use liq_types::{
        Confidence, FlashProvider, MarketId, PositionId, PositionKey, ProtocolId, Ray, TraceId,
        TriggerKind, Wad,
    };
    use smallvec::SmallVec;

    fn cand(id: u32, value: u64, cause: TriggerCause) -> Candidate {
        Candidate {
            position: PositionId(id),
            protocol: ProtocolId(0),
            health: Health {
                hf: Ray::ZERO,
                debt_value: Wad::ZERO,
                collateral_value: Wad::ZERO,
                price_sensitivity: AssetMask::EMPTY,
                state: HealthState::Liquidatable,
            },
            quote: Quote {
                position: PositionId(id),
                key: PositionKey {
                    protocol: ProtocolId(0),
                    market: MarketId(0),
                    user: Address::ZERO,
                },
                repay_options: SmallVec::new(),
                seize_options: SmallVec::new(),
            },
            legs: LegChoice::PREFERRED,
            funding: FlashRoute {
                provider: FlashProvider::Aave,
                source: Address::ZERO,
                asset: liq_types::AssetId(0),
                amount: U256::ZERO,
                fee_bps: 0,
                callback: CallbackShape::AaveExecuteOperation,
            },
            cause,
            est_value: Wad::from_raw(U256::from(value)),
            deadline: None,
            trace: TraceId::from_raw(0),
        }
    }

    fn stale() -> TriggerCause {
        TriggerCause::Stale {
            liquidatable_since: 0,
        }
    }

    /// Oracle: `TriggerKind` (liq-types) is the payload-free mirror; every
    /// variant maps to the like-named kind and only `OraclePredicted` is
    /// unfireable (GUIDE 08 §5 grouping).
    #[test]
    fn kind_mirrors_trigger_kind_and_only_predicted_is_unfireable() {
        let all = [
            (
                TriggerCause::SvrAuction {
                    hint: B256::ZERO,
                    deadline: std::time::Instant::now(),
                },
                TriggerKind::SvrAuction,
            ),
            (
                TriggerCause::OraclePublic { tx: B256::ZERO },
                TriggerKind::OraclePublic,
            ),
            (
                TriggerCause::OraclePullHeld {
                    payload: alloy_primitives::Bytes::new(),
                },
                TriggerKind::OraclePullHeld,
            ),
            (
                TriggerCause::PoolStateChange {
                    pool: Address::ZERO,
                },
                TriggerKind::PoolStateChange,
            ),
            (
                TriggerCause::UserAction { tx: B256::ZERO },
                TriggerKind::UserAction,
            ),
            (
                TriggerCause::DerivedRate {
                    source: liq_types::AssetId(0),
                },
                TriggerKind::DerivedRate,
            ),
            (
                TriggerCause::ParamChange {
                    market: MarketId(0),
                },
                TriggerKind::ParamChange,
            ),
            (TriggerCause::InterestDrift, TriggerKind::InterestDrift),
            (stale(), TriggerKind::Stale),
            (
                TriggerCause::OraclePredicted {
                    conf: Confidence(5_000),
                },
                TriggerKind::OraclePredicted,
            ),
        ];
        assert_eq!(all.len(), 10, "GUIDE 08 §5: ten trigger causes");
        for (cause, kind) in all {
            assert_eq!(cause.kind(), kind);
            assert_eq!(cause.fireable(), kind != TriggerKind::OraclePredicted);
        }
    }

    /// Acceptance (GUIDE 08 §6): under synthetic cascade load the queue keeps
    /// the highest values, drops the lowest, counts every drop, and drains
    /// highest first. Oracle: sort of the same values in the test. Negative:
    /// an incoming candidate below the current lowest is itself dropped.
    #[test]
    fn keeps_highest_drops_lowest_and_counts() {
        let mut q = CandidateQueue::with_capacity(4);
        let values = [5u64, 1, 9, 3, 7, 2, 8];
        for (i, &v) in values.iter().enumerate() {
            q.push(cand(i as u32, v, stale()));
        }
        assert_eq!(q.len(), 4);
        assert_eq!(
            q.dropped(),
            3,
            "7 pushed, 4 kept: 1 and 3 evicted, 2 refused"
        );
        assert_eq!(q.admitted(), 6, "6 admitted (two of them later evicted)");
        assert!(!q.push(cand(99, 0, stale())), "below the lowest: dropped");
        assert_eq!(q.dropped(), 4);
        let got: Vec<u64> = q.drain().map(|c| c.est_value.raw().to::<u64>()).collect();
        let mut want = values.to_vec();
        want.sort_unstable_by(|a, b| b.cmp(a));
        want.truncate(4);
        assert_eq!(got, want);
        assert!(q.is_empty());
        // The buffer is reusable after a drain.
        q.push(cand(1, 1, stale()));
        assert_eq!(q.len(), 1);
        assert_eq!(q.capacity(), 4);
    }

    /// Oracle: GUIDE 08 §4 — a `Predicted` tick never produces a fireable
    /// candidate, and pre-warm candidates rank below every fireable one so
    /// a cascade of predictions cannot evict a real opportunity.
    #[test]
    fn prewarm_ranks_below_fireable() {
        let mut q = CandidateQueue::with_capacity(2);
        let pre = |id, v| {
            cand(
                id,
                v,
                TriggerCause::OraclePredicted {
                    conf: Confidence(9_000),
                },
            )
        };
        q.push(pre(1, 1_000));
        q.push(pre(2, 2_000));
        assert!(
            q.push(cand(3, 1, stale())),
            "tiny but fireable evicts a pre-warm"
        );
        assert!(
            q.push(pre(4, 5_000)),
            "a richer pre-warm evicts the poorer pre-warm…"
        );
        assert!(q.push(pre(5, 9_000)));
        assert_eq!(q.len(), 2);
        let got: Vec<(u32, bool)> = q.drain().map(|c| (c.position.0, c.fireable())).collect();
        assert_eq!(
            got,
            vec![(3, true), (5, false)],
            "…and never the fireable one, however small"
        );
        assert_eq!(q.dropped(), 3);
    }

    /// Oracle: FIFO among equal values (older first) — a deterministic
    /// order GUIDE 12 can rely on when est_value ties.
    #[test]
    fn equal_values_drain_oldest_first() {
        let mut q = CandidateQueue::with_capacity(3);
        for id in [10u32, 11, 12] {
            q.push(cand(id, 5, stale()));
        }
        let got: Vec<u32> = q.drain().map(|c| c.position.0).collect();
        assert_eq!(got, vec![10, 11, 12]);
    }
}
