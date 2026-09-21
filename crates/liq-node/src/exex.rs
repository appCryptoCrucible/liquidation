//! Thin ExEx forwarder (GUIDE 03 §1; RUST-CONV §5.1).
//!
//! The Reth ExEx future is polled on Reth's Tokio runtime. This module is the
//! owned-payload hop onto the pinned hot thread: **no `&` crosses the ring**.
//! [`FinishedUpTo`] is published only after the hot thread confirms the store
//! is consistent — never before. Emitting early lets Reth prune a block we
//! have not actually folded (GUIDE 03: crash then gaps).
//!
//! A2 (in-process Reth) is deferred (D60). Types here are the fork-fixture
//! seam 17A binds to `ExExContext` once the node crate exists.

use alloy_primitives::B256;
use arc_swap::ArcSwap;
use rtrb::{Consumer, Producer, RingBuffer};

use crate::source::OwnedBlock;
use crate::{BlockNum, IngestError, Result};

/// Canonical notifications are never dropped. Size is the undo-ring depth so
/// a full unwind window can sit in-flight without stalling Reth.
pub const NOTIF_CAP: usize = 128;

/// Block identity. Owned so the ExEx future can drop the Reth notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NumHash {
    pub number: BlockNum,
    pub hash: B256,
}

/// Hot-thread confirmation. The ExEx maps this to `ExExEvent::FinishedHeight`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinishedUpTo {
    pub num_hash: NumHash,
}

/// Owned chain payload. One notification, many blocks (Reth `Chain`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedChain {
    pub blocks: Vec<OwnedBlock>,
    pub tip: NumHash,
}

/// Owned ExEx notification. Mirrors Reth's committed / reverted / reorged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notification {
    /// Apply `new` in order. Confirm [`FinishedUpTo`] at `new.tip`.
    Committed { new: OwnedChain },
    /// Unwind every block in `[first, last.number]` down to `first - 1`.
    /// No [`FinishedUpTo`]: Reth only finishes a committed chain.
    Reverted { first: BlockNum, last: NumHash },
    /// Unwind `old` then apply `new`. Confirm at `new.tip`.
    Reorged {
        old_first: BlockNum,
        old_last: NumHash,
        new: OwnedChain,
    },
}

impl Notification {
    #[inline]
    #[must_use]
    pub fn committed_tip(&self) -> Option<NumHash> {
        match self {
            Self::Committed { new } | Self::Reorged { new, .. } => Some(new.tip),
            Self::Reverted { .. } => None,
        }
    }
}

/// Last height the hot thread has marked consistent. Wait-free readers
/// (arc-swap); the ExEx confirmation ring is the backpressure path to Reth.
pub struct ConsistentHeight {
    inner: ArcSwap<FinishedUpTo>,
}

impl ConsistentHeight {
    #[must_use]
    pub fn new(genesis: NumHash) -> Self {
        Self {
            inner: ArcSwap::from_pointee(FinishedUpTo { num_hash: genesis }),
        }
    }

    #[must_use]
    pub fn load(&self) -> FinishedUpTo {
        *self.inner.load().as_ref()
    }

    /// Hot thread only, after the store matches `done`.
    pub fn publish(&self, done: FinishedUpTo) {
        self.inner.store(std::sync::Arc::new(done));
    }
}

/// ExEx / Tokio side of the two SPSC rings.
pub struct ExExForwarder {
    to_hot: Producer<Notification>,
    from_hot: Consumer<FinishedUpTo>,
    recycle: Consumer<OwnedBlock>,
}

/// Hot-thread side.
pub struct HotIngress {
    pub from_exex: Consumer<Notification>,
    pub to_exex: Producer<FinishedUpTo>,
    pub recycle: Producer<OwnedBlock>,
}

/// Split both rings. Owned payloads only.
#[must_use]
pub fn split_exex() -> (ExExForwarder, HotIngress) {
    let (to_hot, from_exex) = RingBuffer::<Notification>::new(NOTIF_CAP);
    let (to_exex, from_hot) = RingBuffer::<FinishedUpTo>::new(NOTIF_CAP);
    let (recycle_p, recycle_c) = RingBuffer::<OwnedBlock>::new(NOTIF_CAP);
    (
        ExExForwarder {
            to_hot,
            from_hot,
            recycle: recycle_c,
        },
        HotIngress {
            from_exex,
            to_exex,
            recycle: recycle_p,
        },
    )
}

impl ExExForwarder {
    /// Push an owned notification. Never drops: a full ring is a stall, not a
    /// skip (canonical stream is unbounded in policy; the ring is the
    /// FinishedHeight backpressure window).
    pub fn push(&mut self, n: Notification) -> Result<()> {
        self.to_hot.push(n).map_err(|_| {
            tracing::error!("ExEx → hot ring full; canonical notification not dropped");
            IngestError::HotStalled
        })
    }

    /// Non-blocking. The ExEx future sends `FinishedHeight` for each.
    pub fn take_finished(&mut self) -> Option<FinishedUpTo> {
        self.from_hot.pop().ok()
    }

    /// Recycle an [`OwnedBlock`] buffer from the hot thread (03A return-ring).
    pub fn take_recycle(&mut self) -> Option<OwnedBlock> {
        self.recycle.pop().ok()
    }
}

impl HotIngress {
    pub fn pop(&mut self) -> Option<Notification> {
        self.from_exex.pop().ok()
    }

    /// Confirm consistency. Must be called only after apply/unwind succeeded.
    pub fn confirm(&mut self, done: FinishedUpTo) -> Result<()> {
        self.to_exex.push(done).map_err(|_| {
            tracing::error!("hot → ExEx FinishedHeight ring full");
            IngestError::HotStalled
        })
    }

    pub fn recycle_block(&mut self, mut block: OwnedBlock) {
        block.clear();
        let _ = self.recycle.push(block);
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
    use super::{split_exex, Notification, NumHash, OwnedChain};
    use crate::source::OwnedBlock;
    use alloy_primitives::B256;

    fn tip(n: u64) -> NumHash {
        NumHash {
            number: n,
            hash: B256::repeat_byte(n as u8),
        }
    }

    fn chain(n: u64) -> OwnedChain {
        OwnedChain {
            blocks: vec![OwnedBlock {
                number: n,
                timestamp: n,
                gas_limit: 0,
                gas_used: 0,
                base_fee_per_gas: 0,
                logs: Vec::new(),
            }],
            tip: tip(n),
        }
    }

    /// Oracle: committed_tip is the only height Reth may prune to.
    /// Negative: a revert must not look committed.
    #[test]
    fn committed_tip_absent_on_revert() {
        let c = Notification::Committed { new: chain(3) };
        assert_eq!(c.committed_tip().unwrap().number, 3);
        let r = Notification::Reverted {
            first: 2,
            last: tip(3),
        };
        assert!(r.committed_tip().is_none());
        let g = Notification::Reorged {
            old_first: 3,
            old_last: tip(3),
            new: chain(3),
        };
        assert_eq!(g.committed_tip().unwrap().number, 3);
    }

    /// Oracle: push then take_finished is empty until the hot side confirms.
    #[test]
    fn finished_height_not_emitted_before_confirm() {
        let (mut fwd, mut hot) = split_exex();
        fwd.push(Notification::Committed { new: chain(1) }).unwrap();
        assert!(fwd.take_finished().is_none());
        let n = hot.pop().unwrap();
        let done = super::FinishedUpTo {
            num_hash: n.committed_tip().unwrap(),
        };
        hot.confirm(done).unwrap();
        let got = fwd.take_finished().unwrap();
        assert_eq!(got.num_hash.number, 1);
        assert!(fwd.take_finished().is_none());
    }
}
