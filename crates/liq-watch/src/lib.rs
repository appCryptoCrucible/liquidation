//! Liquidation watcher: one decoder, streaming SQLite/JSONL + batch parquet.
//! Independent of `liq-state` / `liq-engine` (GUIDE 09 §4a).

#![deny(clippy::todo, clippy::unimplemented)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )
)]

pub mod abi;
pub mod batch;
pub mod decode;
pub mod error;
pub mod join;
pub mod source;
pub mod stream;
pub mod types;

pub use decode::WatchDecoder;
pub use error::{Result, WatchError};
pub use join::{EngineJoin, NoEngineJoin};
pub use source::{LogSource, OwnedBlock, OwnedLog, Poll, RpcPoll, DEFAULT_PAGE_BLOCKS};
pub use stream::StreamSink;
pub use types::{ActualLiquidation, CoverageDims, DecodedLiquidation, TriggerClass};
