//! `Validate` — every config section implements this; startup calls it and
//! refuses to proceed on `Err` (GUIDE 00 §4). There is no degraded mode.

use crate::rpc::ChainRpc;
use crate::Result;

/// Called at startup with a live RPC. MUST fail closed.
///
/// Alloy's `Provider` is not object-safe (generic methods). [`ChainRpc`] is the
/// object-safe surface this crate actually needs: `eth_chainId` and `eth_call`.
/// The production implementor ([`crate::HttpRpc`]) reads the live chain; it
/// does not cache and it does not substitute values.
#[allow(async_fn_in_trait)] // boot-only; awaited in place, never spawned
pub trait Validate {
    /// Check this section against the chain (or against structural invariants
    /// that later WPs fill in with chain reads).
    async fn validate<R: ChainRpc + Sync>(&self, rpc: &R) -> Result<()>;
}
