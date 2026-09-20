//! Fail-closed observability errors. No degraded or guessed telemetry.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ObsError {
    #[error("io: {0}")]
    Io(String),
    #[error("json: {0}")]
    Json(String),
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("toml: {0}")]
    Toml(String),
    #[error("alert config missing field {0}")]
    AlertConfig(&'static str),
    #[error("alert http: {0}")]
    AlertHttp(String),
    #[error("clock before unix epoch")]
    Clock,
    #[error("perf counters unavailable on {os}: {cause}")]
    PerfUnavailable { os: &'static str, cause: String },
    #[error("engine emit missing field {0}")]
    Emit(&'static str),
    #[error("shadow path is not a directory: {0}")]
    ShadowDir(String),
    #[error("pre-flight input absent: {0}")]
    PreflightAbsent(&'static str),
}

impl From<std::io::Error> for ObsError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<serde_json::Error> for ObsError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e.to_string())
    }
}

impl From<rusqlite::Error> for ObsError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e.to_string())
    }
}

pub type Result<T> = core::result::Result<T, ObsError>;
