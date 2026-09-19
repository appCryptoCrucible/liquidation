//! Ingest: log sources, router, arena decode, dirty accumulation, backfill
//! (GUIDE 03 §2, §4, §4b, §5; WP 03A).
//!
//! The hot path (`apply_block`) is synchronous. RpcPoll is I/O for backfill /
//! 05F / 05B. The ExEx forwarder that fills [`ExExSource`] is WP 03B.

#![deny(clippy::todo, clippy::unimplemented)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use liq_protocol::{ArchiveError, ProtocolError};
use liq_state::StateError;
use liq_types::PositionId;

pub mod apply;
pub mod backfill;
pub mod decode;
pub mod dirty;
pub mod router;
pub mod source;

pub use apply::{apply_block, ApplyCtx, LogHandler};
pub use backfill::{backfill, poller, SnapshotSink};
pub use decode::{DecodeArena, EventKind};
pub use dirty::{as_dirty_sets, CollapsedDirty, DirtyAccumulator};
pub use liq_protocol::conformance::AllocMeter;
pub use router::{LogRouter, Route};
pub use source::{ExExSource, LogSource, OwnedBlock, OwnedLog, Poll, RpcPoll, DEFAULT_PAGE_BLOCKS};

/// Block number. Matches `liq_protocol::BlockNum`.
pub type BlockNum = liq_protocol::BlockNum;
/// Unix seconds. Matches `liq_protocol::Timestamp`.
pub type Timestamp = liq_protocol::Timestamp;

/// Failures of ingest. Every variant is `Copy` and heap-free (RUST-CONVENTIONS §10).
#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IngestError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error("log source unavailable")]
    SourceUnavailable,
    #[error("log range {from}..={to} is truncated")]
    Truncated { from: BlockNum, to: BlockNum },
    #[error("malformed log payload")]
    MalformedLog,
    #[error("snapshot persist failed")]
    Snapshot,
    #[error("block {got} does not follow tip {tip}")]
    BlockGap { tip: BlockNum, got: BlockNum },
    #[error("subscriber count exceeds u16")]
    TooManySubscribers,
    #[error("handler index {idx} missing")]
    HandlerMissing { idx: u16 },
    #[error("hot path allocated")]
    HotPathAlloc,
    #[error("unknown position {0:?} during dirty collapse")]
    UnknownPosition(PositionId),
}

/// `Result` with [`IngestError`].
pub type Result<T, E = IngestError> = core::result::Result<T, E>;
