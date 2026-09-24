//! In-process revm simulation (GUIDE 11, WP 11).
//!
//! Workers own a `CacheDB` and a [`WarmSet`]. The node's
//! [`StateProviderFactory`] is shared read-only. There is no RPC on this
//! path: a provider that touches the network must panic.
//!
//! **D60 / 05B:** the 100-liquidation 1% profit+gas gate is deferred until
//! the archive crate writes fixtures. [`SimError::ArchiveUnavailable`] is
//! the fail-closed seam — this crate does not invent historical fills.

#![deny(clippy::todo, clippy::unimplemented)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod verify;
pub mod warm;
pub mod worker;

use alloy_primitives::{Address, Bytes, I256, U256};
use liq_types::{Confidence, Ray, TraceId};
use revm::database_interface::{DBErrorMarker, DatabaseRef};
use revm::primitives::{StorageKey, StorageValue, B256};
use revm::state::{AccountInfo, Bytecode};
use std::convert::Infallible;
use std::sync::Arc;

pub use verify::{
    block_env_at, execute_calldata, verify, verify_historical, verify_variants, Bundle,
    HealthProbe, SimTx, Trigger,
};
pub use warm::{
    clear_except, insert_executor, load_executor_creation_bytecode, ExecutorSpec, Simulator,
    WarmSet, PLANNED_EXECUTOR,
};
pub use worker::{spawn_workers, SimPool, SimReply, SimRequest, SimWorker};

/// Failures of `verify`. `Revert` is alerted — it is a bug class, not a
/// market condition (GUIDE 11 Step 6).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SimError {
    #[error("position recovered, hf={hf:?}")]
    GuardRejected { hf: Ray },
    #[error("insufficient liquidity at pool {pool}")]
    InsufficientLiquidity { pool: Address },
    #[error("slippage exceeded: expected {expected}, actual {actual}")]
    SlippageExceeded { expected: U256, actual: U256 },
    #[error("profit below floor: net={net}")]
    ProfitBelowFloor { net: I256 },
    #[error("execution reverted: {reason}")]
    Revert { reason: Bytes },
    #[error("state unavailable")]
    StateUnavailable,
    #[error("sim worker panicked")]
    WorkerPanic,
    /// 05B archive is not on disk. Not a skip and not a fabricated pass.
    #[error("historical archive unavailable (WP 05B deferred)")]
    ArchiveUnavailable,
    #[error("malformed bundle: {0}")]
    Malformed(&'static str),
    #[error("executor bytecode artifact missing or unreadable: {0}")]
    Bytecode(&'static str),
}

impl DBErrorMarker for SimError {}

/// What GUIDE 12 consumes: actual gas and a confidence flag. Both are
/// load-bearing (GUIDE 11 handoff).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimOutcome {
    pub gas_used: u64,
    pub net_profit_wei: I256,
    pub confidence: Confidence,
    pub weth_after: U256,
}

/// Shared read-only factory (GUIDE 11 Step 4c). Reth's provider implements
/// this at wiring time (WP 03B / 17A). `liq-sim` does not depend on
/// `liq-node`.
pub trait StateProviderFactory: Send + Sync + 'static {
    type Provider: DatabaseRef<Error = SimError> + Send + Sync;
    fn latest(&self) -> Result<Arc<Self::Provider>, SimError>;
}

/// `DatabaseRef` that panics on every access. Hot-path tests pin this so a
/// cache miss cannot silently become an RPC.
#[derive(Clone, Debug, Default)]
pub struct PanicNetworkDb;

impl PanicNetworkDb {
    #[inline]
    #[allow(clippy::panic)]
    fn boom() -> ! {
        panic!("oracle: GUIDE-11 — liq-sim must not RPC");
    }
}

impl DatabaseRef for PanicNetworkDb {
    type Error = SimError;

    fn basic_ref(&self, _: Address) -> Result<Option<AccountInfo>, Self::Error> {
        Self::boom()
    }
    fn code_by_hash_ref(&self, _: B256) -> Result<Bytecode, Self::Error> {
        Self::boom()
    }
    fn storage_ref(&self, _: Address, _: StorageKey) -> Result<StorageValue, Self::Error> {
        Self::boom()
    }
    fn block_hash_ref(&self, _: u64) -> Result<B256, Self::Error> {
        Self::boom()
    }
}

/// In-memory factory for tests and pre-H3 CacheDB boots. Never networks.
#[derive(Clone, Debug)]
pub struct MemoryFactory {
    inner: Arc<revm::database::CacheDB<revm::database::EmptyDB>>,
}

impl MemoryFactory {
    #[must_use]
    pub fn from_cache(db: revm::database::CacheDB<revm::database::EmptyDB>) -> Self {
        Self {
            inner: Arc::new(db),
        }
    }

    /// Empty CacheDB. No invented balances or code.
    #[must_use]
    pub fn empty() -> Self {
        Self::from_cache(revm::database::CacheDB::new(
            revm::database::EmptyDB::default(),
        ))
    }
}

impl StateProviderFactory for MemoryFactory {
    type Provider = MemoryProvider;
    fn latest(&self) -> Result<Arc<Self::Provider>, SimError> {
        Ok(Arc::new(MemoryProvider(Arc::clone(&self.inner))))
    }
}

/// Read-only view of a `CacheDB<EmptyDB>`.
#[derive(Clone, Debug)]
pub struct MemoryProvider(Arc<revm::database::CacheDB<revm::database::EmptyDB>>);

impl DatabaseRef for MemoryProvider {
    type Error = SimError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.0.basic_ref(address).map_err(infallible)
    }
    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.0.code_by_hash_ref(code_hash).map_err(infallible)
    }
    fn storage_ref(
        &self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        self.0.storage_ref(address, index).map_err(infallible)
    }
    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.0.block_hash_ref(number).map_err(infallible)
    }
}

#[inline]
fn infallible(_: Infallible) -> SimError {
    SimError::StateUnavailable
}

/// Identity for matching a request to a reply on a SPSC ring.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SimId(pub u64);

impl SimId {
    #[must_use]
    pub const fn new(n: u64) -> Self {
        Self(n)
    }
}

impl From<TraceId> for SimId {
    fn from(t: TraceId) -> Self {
        Self(t.raw())
    }
}

#[cfg(test)]
mod tests;
