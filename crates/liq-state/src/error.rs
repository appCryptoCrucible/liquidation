//! Store-native errors. Every variant is `Copy` with no heap data, so an error
//! on the hot thread never allocates (RUST-CONVENTIONS §10). The
//! [`StateWriter`](liq_protocol::StateWriter) methods return
//! `liq_protocol::ProtocolError` because the trait fixes that type; everything
//! the store adds on top (block bookkeeping, unwinding, views) returns this.

use liq_protocol::BlockNum;
use liq_types::{MarketId, PositionId};

/// Failure of a store operation outside the `StateWriter` surface.
#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    /// The position id is not in the store.
    #[error("unknown position {0:?}")]
    UnknownPosition(PositionId),
    /// The market id is not in the store.
    #[error("unknown market {0:?}")]
    UnknownMarket(MarketId),
    /// `begin_block` was given a number other than `tip + 1`. Blocks are
    /// applied contiguously; a gap means missed state and is refused.
    #[error("block {got} does not follow tip {tip}")]
    BlockGap { tip: BlockNum, got: BlockNum },
    /// `unwind_to` was given a target above the current tip.
    #[error("unwind target {target} is above tip {tip}")]
    TargetAboveTip { target: BlockNum, tip: BlockNum },
    /// The undo ring does not hold every block between the target and the
    /// tip. **Nothing was unwound**: the caller halts and rebuilds from a
    /// snapshot (GUIDE 02 §5; GUIDE 14). `cap` is how deep this ring could
    /// have gone right now.
    #[error("reorg depth {depth} exceeds undo ring coverage {cap}")]
    ReorgTooDeep { depth: u64, cap: u64 },
    /// An undo record did not match the state it was to restore, or an
    /// internal index did not resolve. The store is no longer trustworthy;
    /// the caller halts.
    #[error("undo record does not match the state it restores")]
    Inconsistent,
    /// The overlay was built against a different store state than the one it
    /// is now being read or extended over. `writes` is the store's mutation
    /// counter: it separates two states that share a tip and a position
    /// count, which every pair of moments inside one block does.
    #[error("overlay built at tip {overlay} with {overlay_len} positions after {overlay_writes} writes; store is at tip {store} with {store_len} after {store_writes}")]
    OverlayStale {
        overlay: BlockNum,
        overlay_len: usize,
        overlay_writes: u64,
        store: BlockNum,
        store_len: usize,
        store_writes: u64,
    },
    /// The `u16` asset id space is exhausted.
    #[error("asset id space exhausted")]
    AssetIdsExhausted,
}
