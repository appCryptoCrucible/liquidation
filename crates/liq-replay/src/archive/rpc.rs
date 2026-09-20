//! Public-RPC seam (`LIQ_ARCHIVE_RPC`). No guessed endpoint.

use alloy_provider::{Provider, ProviderBuilder};

use super::extract::ExtractError;

/// Fail closed: extraction has no default RPC and must not invent one.
pub fn archive_rpc_url() -> Result<String, ExtractError> {
    std::env::var("LIQ_ARCHIVE_RPC").map_err(|_| {
        tracing::error!("LIQ_ARCHIVE_RPC unset; archive extract refused");
        ExtractError::NoRpc
    })
}

/// HTTP alloy provider. Replay must use [`super::ParquetArchive`], not this.
pub fn connect_http(url: &str) -> Result<impl Provider + Clone, ExtractError> {
    let parsed = url.parse().map_err(|e| ExtractError::Rpc(format!("{e}")))?;
    Ok(ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(parsed))
}
