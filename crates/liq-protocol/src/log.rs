//! The log view an adapter folds (GUIDE 03 §2, §4b).
//!
//! The router has already done the protocol-agnostic work — matched
//! `(address, topic0)` against the adapter's `subscriptions()` and split the
//! receipt log into topics and data borrowed from the per-block arena. The
//! ABI-typed decode is the adapter's, via `alloy::sol!`
//! (`SolEvent::decode_raw_log`), because only the adapter knows the event
//! set; keeping the typed structs out of this crate is what keeps
//! `liq-protocol` free of every adapter (GUIDE 01 acceptance).

use alloy_primitives::{Address, B256};

use crate::{BlockNum, Timestamp};

/// One routed log, borrowed from the block arena for lifetime `'a`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DecodedLog<'a> {
    /// Emitting contract.
    pub address: Address,
    /// `topics[0]` is the event signature hash the router matched.
    pub topics: &'a [B256],
    /// ABI-encoded non-indexed fields.
    pub data: &'a [u8],
    /// Block the log was emitted in.
    pub block: BlockNum,
    /// That block's timestamp — `lastUpdateTimestamp` for index updates.
    pub timestamp: Timestamp,
}
