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

/// A pending public-mempool transaction for the backrun engine, as Reth's
/// pool validated it: what simulating it needs, its raw signed bytes for a
/// bundle, and what fixes its place in priority-fee order (the reference
/// an ordered backrun must leave no user worse off than).
#[derive(Clone, Debug)]
pub struct PendingPublic {
    pub hash: TxHash,
    pub sender: Address,
    /// `None` is a contract creation (never backrun; dropped at intake).
    pub to: Option<Address>,
    pub value: alloy_primitives::U256,
    pub input: Bytes,
    pub gas_limit: u64,
    pub nonce: u64,
    /// EIP-2930 access list, `(address, storage keys)`.
    pub access_list: Vec<(Address, Vec<alloy_primitives::B256>)>,
    /// What it pays per gas above the base fee at most (a legacy
    /// transaction's gas price).
    pub max_priority_fee_per_gas: u128,
    pub max_fee_per_gas: u128,
    /// EIP-2718 encoding: what a builder bundle carries.
    pub raw: Bytes,
    /// When the pool accepted it: priority-fee order's tie-break.
    pub seen_at: std::time::Instant,
}
