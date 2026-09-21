//! Fail-closed execution errors. Network results are never unwrapped.

use thiserror::Error;

/// 13A execution failure. Every variant is logged by the caller; none panic.
#[derive(Debug, Error)]
pub enum ExecError {
    #[error("builder config: {0}")]
    Config(String),
    #[error("builders.toml lists a public-RPC or sendRaw/sendPrivate URL")]
    PublicRpcForbidden,
    #[error("no curated builders after load")]
    EmptyBuilders,
    #[error("venue is not MevShare")]
    WrongVenueMevShare,
    #[error("venue is not BuilderBundle")]
    WrongVenueBuilder,
    #[error("Venue relay/endpoint does not match this submitter")]
    RelayMismatch,
    #[error("TriggerKind::OraclePredicted is not submittable")]
    PredictedNotSubmittable,
    #[error("SvrAuction requires a hint hash")]
    MissingHintHash,
    #[error("SvrAuction inclusion span must be 2 or 3 blocks, got {0}")]
    BadMevShareSpan(u64),
    #[error("max_block {max} < target_block {target}")]
    InvertedSpan { target: u64, max: u64 },
    #[error("fee quote parent_block {parent} + 1 != target_block {target} (never reuse a prior block's fee)")]
    StaleFee { parent: u64, target: u64 },
    #[error("next base fee is missing or zero")]
    MissingBaseFee,
    #[error("priority fee is required and must be nonzero")]
    MissingPriority,
    #[error("modest priority fee is required and must be nonzero")]
    MissingModestPriority,
    #[error("fee field overflow")]
    FeeOverflow,
    #[error("auction_bps {0} is not a whole percent (must be divisible by 100)")]
    AuctionBpsNotPercent(u16),
    #[error("auction_bps {0} exceeds 10000")]
    AuctionBpsRange(u16),
    #[error("refund percent {0} is not in 0..=100")]
    BadRefundPercent(u8),
    #[error("chain_id must be 1 (D01); got {0}")]
    ChainId(u64),
    #[error("gas_limit is 0")]
    ZeroGasLimit,
    #[error("nonce slot {0} is out of range")]
    BadSlot(usize),
    #[error("nonce overflow")]
    NonceOverflow,
    #[error("nonce {0} is not in-flight")]
    NonceNotInFlight(u64),
    #[error("bundle txs is empty")]
    EmptyBundle,
    #[error("backrun trigger is missing the parent raw tx")]
    MissingBackrunTx,
    #[error("signed liquidation is empty")]
    EmptySignedTx,
    #[error("template patch {0} is missing")]
    MissingPatch(&'static str),
    #[error("calldata patch offset/len out of range")]
    BadInputPatch,
    #[error("signer: {0}")]
    Signer(String),
    #[error("identity header: {0}")]
    Identity(String),
    #[error("relay/builder HTTP: {0}")]
    Http(String),
    #[error("JSON-RPC: {0}")]
    Rpc(String),
    #[error("all builder endpoints unreachable; no public-mempool fallback (D54)")]
    AllBuildersUnreachable,
    #[error("record IntendedSubmission: {0}")]
    Record(String),
    #[error("serde: {0}")]
    Serde(String),
    #[error("inferred_bid is not a U256")]
    BadInferredBid,
}

/// `Result` with [`ExecError`].
pub type Result<T, E = ExecError> = core::result::Result<T, E>;
