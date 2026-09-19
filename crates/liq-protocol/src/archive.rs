//! `Archive` — the replay/backfill event source `Protocol::backfill` folds
//! from (GUIDE 03 §5, GUIDE 05 §1). Implemented by `liq-replay`'s parquet
//! archive (05B) and `liq-node`'s `RpcPoll` source (03A) — the same fold,
//! different feed.

use liq_types::LogFilter;

use crate::error::Result;
use crate::log::DecodedLog;
use crate::BlockNum;

/// Ordered log source over a block range.
pub trait Archive {
    /// Visit every log matching any of `filters` in blocks `from..=to`, in
    /// canonical `(block, tx_index, log_index)` order. Visitor style so the
    /// archive owns its buffers and the fold allocates nothing per log.
    ///
    /// The visitor's `Err` aborts the walk and is returned unchanged; the
    /// archive's own failures arrive as [`crate::ArchiveError`] through
    /// `ProtocolError::Archive`. An archive that cannot prove the range
    /// complete returns [`crate::ArchiveError::Truncated`] rather than an
    /// empty stream (GUIDE 05 §1 — pruned nodes have answered wide ranges
    /// with silence).
    fn logs(
        &self,
        filters: &[LogFilter],
        from: BlockNum,
        to: BlockNum,
        visit: &mut dyn FnMut(&DecodedLog<'_>) -> Result<()>,
    ) -> Result<()>;
}
