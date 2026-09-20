//! Public-mempool seam types (GUIDE 03 §4b, GUIDE 06 §5).
//!
//! Shared between the producer (03B, `liq-node` hot-thread mempool ring) and
//! the consumer (06D, `liq-oracle` `transmit()` decoder) via this cycle-breaker
//! crate, so `liq-node` never depends on `liq-oracle` and vice-versa.

use alloy_primitives::{Address, Bytes, TxHash};

/// Ring capacity 03B allocates for the drop-oldest mempool overlay (`GUIDE 03` §4b).
/// Producer and consumer must agree; centralised here to prevent drift.
pub const RING_CAP: usize = 4096;

/// A pending public-mempool transaction observed by 03B. `to` is the callee
/// (e.g. a Chainlink aggregator). 06D decodes `input` when `to` is a watched
/// aggregator; everything else is passed through untouched.
#[derive(Clone, Debug)]
pub struct PendingTx {
    pub hash: TxHash,
    pub to: Address,
    pub input: Bytes,
}
