//! Production process. The `reth` binary (feature `node`) is Reth with the
//! liquidation ExEx in-process. `liq-bot` does not start a node.
//!
//! Feature `convert` typechecks the notification copy without the node.
//! Reth 2.6's storage provider does not compile on Windows.

#[cfg(feature = "convert")]
pub mod convert;
