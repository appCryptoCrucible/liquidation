//! Fail-closed watcher errors. No degraded decode.

use alloy_primitives::Address;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WatchError {
    #[error("rpc unavailable: {0}")]
    Rpc(String),
    #[error("malformed log")]
    MalformedLog,
    #[error("ABI decode failed for {0}")]
    Abi(&'static str),
    #[error("unknown protocol family {0}")]
    UnknownFamily(String),
    #[error("registry extra field {0} missing or not an address")]
    ExtraAddress(&'static str),
    #[error("intern missing asset {0:#x}")]
    UnknownAsset(Address),
    #[error("intern missing market")]
    UnknownMarket,
    #[error("tx index exceeds u16")]
    TxIndexOverflow,
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("io: {0}")]
    Io(String),
    #[error("parquet: {0}")]
    Parquet(String),
    #[error("json: {0}")]
    Json(String),
    #[error("log range {from}..={to} truncated")]
    Truncated { from: u64, to: u64 },
    #[error("config: {0}")]
    Config(String),
}

impl From<rusqlite::Error> for WatchError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e.to_string())
    }
}

impl From<std::io::Error> for WatchError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

pub type Result<T> = core::result::Result<T, WatchError>;
