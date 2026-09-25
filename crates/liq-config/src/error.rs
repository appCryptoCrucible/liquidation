//! Fail-closed errors. None of these are recoverable at boot: a mismatch or an
//! unreachable RPC means every downstream number is suspect (REGISTRY.md §4c).

use alloy_primitives::Address;
use thiserror::Error;

/// Load, validate, or boot-assertion failure.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Figment / filesystem / JSON failed to produce a typed config.
    #[error("config load failed: {0}")]
    Load(String),
    /// `rpc_url` empty or the transport could not complete a read.
    #[error("rpc unavailable; refusing to start: {cause}")]
    RpcUnavailable { cause: String },
    /// Config or registry `chain_id` disagrees with `eth_chainId`.
    #[error("chain id mismatch: config {expected}, chain {found}")]
    ChainIdMismatch { expected: u64, found: u64 },
    /// Committed registry has no tokens. That is not a valid cache of chain truth.
    #[error("registry has no tokens")]
    EmptyRegistry,
    /// Intern table overflowed the id width.
    #[error("too many {what} to intern")]
    InternOverflow { what: &'static str },
    /// Token `decimals()` disagrees with the committed registry.
    #[error("decimals mismatch for {token:#x}: registry {expected}, chain {found}")]
    DecimalsMismatch {
        token: Address,
        expected: u8,
        found: u8,
    },
    /// Token `symbol` is JSON `null`. Discovery recorded a gap; we do not
    /// invent a symbol, and we will not start with an unassertable field.
    #[error("registry symbol is null for {token:#x} (chain says {found:?}); refusing to start")]
    SymbolMissing { token: Address, found: String },
    /// Token `symbol()` disagrees with the committed registry.
    #[error("symbol mismatch for {token:#x}: registry {expected:?}, chain {found:?}")]
    SymbolMismatch {
        token: Address,
        expected: String,
        found: String,
    },
    /// Pool `token0()` disagrees with the committed registry.
    #[error("token0 mismatch for pool {pool:#x}: registry {expected:#x}, chain {found:#x}")]
    Token0Mismatch {
        pool: Address,
        expected: Address,
        found: Address,
    },
    /// Pool `token1()` disagrees with the committed registry.
    #[error("token1 mismatch for pool {pool:#x}: registry {expected:#x}, chain {found:#x}")]
    Token1Mismatch {
        pool: Address,
        expected: Address,
        found: Address,
    },
    /// V2 pair `factory()` disagrees with the committed registry.
    #[error("pool {pool}: factory() {found}, registry pins {expected}")]
    PoolFactoryMismatch {
        pool: Address,
        expected: Address,
        found: Address,
    },
    /// Curve pool `coins(index)` disagrees with the committed registry.
    #[error("curve pool {pool}: coins({index}) {found}, registry pins {expected}")]
    CurveCoinMismatch {
        pool: Address,
        index: usize,
        expected: Address,
        found: Address,
    },
    /// Oracle proxy `decimals()` disagrees with the committed registry.
    #[error("oracle decimals mismatch for {proxy:#x}: registry {expected}, chain {found}")]
    OracleDecimalsMismatch {
        proxy: Address,
        expected: u8,
        found: u8,
    },
    /// Oracle proxy `aggregator()` disagrees with the committed registry.
    /// A proxy upgraded underneath us is this alert, not a mystery.
    #[error("aggregator mismatch for {proxy:#x}: registry {expected:#x}, chain {found:#x}")]
    AggregatorMismatch {
        proxy: Address,
        expected: Address,
        found: Address,
    },
    /// Pool `fee()` disagrees with the committed registry.
    #[error("fee mismatch for pool {pool:#x}: registry {expected}, chain {found}")]
    FeeMismatch {
        pool: Address,
        expected: u32,
        found: u32,
    },
    /// An `eth_call` reverted, returned empty, or did not ABI-decode.
    #[error("on-chain view failed for {address:#x} ({what})")]
    CallFailed {
        address: Address,
        what: &'static str,
    },
    /// A protocol market id is neither a 20-byte address nor a 32-byte slot.
    #[error("unrecognised on-chain id {0:?}")]
    BadOnChainId(String),
    /// `registry/asset-ids.json` is missing, unreadable, or disagrees with
    /// the registry. Ids are not recovered by sorting.
    #[error("asset id ledger: {0}")]
    AssetLedger(String),
}

/// `Result` with [`ConfigError`].
pub type Result<T, E = ConfigError> = core::result::Result<T, E>;
