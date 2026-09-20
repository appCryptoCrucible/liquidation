//! Price feeds, canonical PriceVector publish, MEV-Share, and fusion.
//!
//! WP 06A-1: feed registry (`feeds`), canonical book (`canonical`),
//! triple-buffer publish (`publish`). 06A-2 derived; 06B MEV-Share; 08B
//! governance timelock poller.

#![deny(clippy::todo, clippy::unimplemented)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::float_arithmetic
    )
)]

use alloy_primitives::Address;
use thiserror::Error;

pub mod canonical;
pub mod derived;
pub mod feeds;
pub mod governance;
pub mod mevshare;
pub mod publish;

pub use canonical::{answer_to_ray, stale_after, CanonicalBook, ANSWER_UPDATED_TOPIC0};
pub use derived::{
    CrossOrder, DerivedBook, DerivedSpec, Formula, RateScale, DERIVED_CONFIDENCE_CERTAIN,
};
pub use feeds::{
    assert_protocol_sources, resolve_registry, FeedFailure, FeedSet, FeedSpec, FeedsBoot,
    FeedsConfig, Mechanism, RegistryOracle,
};
pub use governance::{
    execution_due, GovernanceConfig, GovernancePoller, PayloadView, Timelock, TimelockKind,
    PIN_BLOCK,
};
pub use publish::{split, PricePublish, PriceRead};

/// Fail-closed oracle errors. None are recoverable at boot or on a bad log.
#[derive(Debug, Error)]
pub enum OracleError {
    #[error("feed config load failed: {0}")]
    Load(String),
    #[error(
        "feed aggregator mismatch for proxy {proxy:#x}: registry {expected:#x}, config {found:#x}"
    )]
    AggregatorMismatch {
        proxy: Address,
        expected: Address,
        found: Address,
    },
    #[error("feed proxy {proxy:#x} is not in the registry")]
    ProxyNotInRegistry { proxy: Address },
    #[error("feed decimals mismatch for proxy {proxy:#x}: registry {expected}, config {found}")]
    DecimalsMismatch {
        proxy: Address,
        expected: u8,
        found: u8,
    },
    #[error("resolved registry oracle {proxy:#x} ({pair}) has no feed TOML row")]
    MissingToml { proxy: Address, pair: String },
    #[error("heartbeat_secs is 0 for proxy {proxy:#x}")]
    ZeroHeartbeat { proxy: Address },
    #[error("deviation_bps is 0 for proxy {proxy:#x}")]
    ZeroDeviation { proxy: Address },
    #[error("feed has no aggregator address (proxy {proxy:#x})")]
    NoAggregator { proxy: Address },
    #[error("aggregator {aggregator:#x} is the zero address")]
    ZeroAggregator { aggregator: Address },
    #[error("intern missing asset {asset:#x}")]
    InternAsset { asset: Address },
    #[error("intern missing protocol {0}")]
    InternProtocol(String),
    #[error("intern missing market {market}")]
    InternMarket { market: String },
    #[error("intern missing feed for proxy {proxy:#x}")]
    InternFeed { proxy: Address },
    #[error("price scale overflow (decimals {decimals})")]
    ScaleOverflow { decimals: u8 },
    #[error("aggregator answer is not a positive price")]
    NonPositiveAnswer,
    #[error("AnswerUpdated decode failed")]
    BadAnswerUpdated,
    #[error(
        "protocol oracle source migrated: asset {asset:#x} expected {expected:#x} found {found:#x}"
    )]
    SourceMigrated {
        asset: Address,
        expected: Address,
        found: Address,
    },
    #[error("protocol getSourceOfAsset({asset:#x}) = {found:#x}, feed proxy {expected:#x}")]
    ProtocolOracleMismatch {
        asset: Address,
        expected: Address,
        found: Address,
    },
    #[error("derived rate unknown or zero at {contract:#x}")]
    UnknownRate { contract: Address },
    #[error("derived dep price missing (ts=0) for asset id {0}")]
    MissingDep(u16),
    #[error("derived rate log decode failed")]
    BadRateLog,
    #[error("LP totalSupply is zero")]
    ZeroLpSupply,
    #[error("derived fixed-point: {0}")]
    Fixed(#[from] liq_types::fixed::FixedError),
    #[error("governance read failed: {0}")]
    Governance(String),
    #[error("payload {id} ABI too short ({len} bytes)")]
    PayloadTruncated { id: u64, len: usize },
    #[error("payload_lookback is 0")]
    ZeroPayloadLookback,
    #[error(transparent)]
    Config(#[from] liq_config::ConfigError),
}

/// `Result` with [`OracleError`].
pub type Result<T, E = OracleError> = core::result::Result<T, E>;
