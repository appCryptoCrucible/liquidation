//! WP 05B — archive extraction + parquet ground truth (GUIDE 05 §1, §2).
//!
//! Extraction talks to an RPC (`LIQ_ARCHIVE_RPC`). Replay reads parquet only
//! ([`ParquetArchive`]) — no HTTP on the fold.

mod extract;
mod filter;
mod rpc;
mod store;

pub use extract::{extract_window, second_source_verify, ExtractError, ExtractReport};
pub use filter::{load_c3_addresses, parse_receipts_log_filter, C3_EXPECTED};
pub use rpc::{archive_rpc_url, connect_http};
pub use store::{
    apply_reorg_headers, headers_complete, partition_bounds, write_headers, ArchivedPrice,
    EventRow, HeaderRow, ParquetArchive, ReorgRow, DEFAULT_PARTITION_BLOCKS,
};

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests;
