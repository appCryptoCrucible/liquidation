//! MEV-Share client: SSE stream, partial-hint matcher, `mev_sendBundle`
//! signing, history (GUIDE 06 §4). Live mainnet POST is WP 13A.

mod bundle;
mod history;
mod matcher;
mod sign;
mod stream;

pub use bundle::{post_signed, rpc_send_bundle, MevShareSubmitter, SendBundle, SignedRelayRequest};
pub use history::{fetch_history, parse_history_body, HistoryQuery};
pub use matcher::{
    match_hint, publish_source, svr_targets, PriceOrigin, SvrMatch, SvrTarget, TRANSMIT_SELECTOR,
};
pub use sign::{body_id_hex, flashbots_header, sign_body, verify_header, SearcherKey};
pub use stream::{
    backoff_delay, classify_sse_line, drain_connection, parse_hint_json, sse_client, SseItem,
    MEV_SHARE_SSE, PING_IDLE,
};

use thiserror::Error;

/// Flashbots builder endpoint (GUIDE 06 §4 / 13 §2). Leaked once at load.
pub const FLASHBOTS_RELAY: &str = "https://relay.flashbots.net";

/// Config loader leaks a venue string once (00C: `Venue` is `&'static str`).
#[must_use]
pub fn leak_endpoint(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Fail-closed MEV-Share errors. Unknown fields that are required are errors.
#[derive(Debug, Error)]
pub enum MevShareError {
    #[error("hint JSON missing hash")]
    MissingHash,
    #[error("hint functionSelector is not 4 bytes")]
    BadSelector,
    #[error("hint log is not a valid EVM log")]
    BadLog,
    #[error("hint JSON: {0}")]
    HintJson(String),
    #[error("callData present but OCR report could not be decoded")]
    BadCallData,
    #[error("logs present but no AnswerUpdated for aggregator {0:#x}")]
    NoAnswerUpdated(alloy_primitives::Address),
    #[error("matched hint has no extracted or predicted price")]
    MissingPredicted,
    #[error("refund percent {0} is not in 0..=100")]
    BadRefundPercent(u8),
    #[error("Flashbots signature: {0}")]
    Signature(String),
    #[error("X-Flashbots-Signature header is not address:signature")]
    BadAuthHeader,
    #[error("recovered signer {recovered:#x} != header {header:#x}")]
    SignerMismatch {
        recovered: alloy_primitives::Address,
        header: alloy_primitives::Address,
    },
    #[error("venue is not MevShare")]
    WrongVenue,
    #[error("Venue::MevShare relay does not match this submitter")]
    RelayMismatch,
    #[error("live mev_sendBundle POST is WP 13A")]
    LiveSendIs13A,
    #[error("HTTP: {0}")]
    Http(String),
    #[error("JSON-RPC error: {0}")]
    Rpc(String),
    #[error("history response is not a hint list")]
    BadHistory,
    #[error(transparent)]
    Oracle(#[from] crate::OracleError),
}

/// `Result` with [`MevShareError`].
pub type Result<T, E = MevShareError> = core::result::Result<T, E>;
