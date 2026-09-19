//! Crate error types. Every variant is `Copy` and carries no heap data, so an
//! error path on the hot thread never allocates (RUST-CONVENTIONS §10).

use liq_types::fixed::FixedError;
use liq_types::{AssetId, MarketId, PositionId};

use crate::market::MarketSlot;

/// `Result` with the crate error, as used throughout the [`crate::Protocol`]
/// trait.
pub type Result<T, E = ProtocolError> = core::result::Result<T, E>;

/// Failure inside an adapter, the state writer it drives, or the plan encoder.
#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    /// Fixed-point arithmetic failed (overflow, underflow, division by zero).
    #[error(transparent)]
    Fixed(#[from] FixedError),
    /// The position id is not in the store.
    #[error("unknown position {0:?}")]
    UnknownPosition(PositionId),
    /// The market id is not in the store.
    #[error("unknown market {0:?}")]
    UnknownMarket(MarketId),
    /// The slot does not exist in that market, or exceeds
    /// [`crate::AssetMask::MAX_SLOTS`].
    #[error("slot {} out of range for market {:?}", .0.slot, .0.market)]
    SlotOutOfRange(MarketSlot),
    /// The price vector has no entry for an asset the position holds. The
    /// feed registry (GUIDE 06 §2) is incomplete; health is **not** computed
    /// with a substitute.
    #[error("no price for asset {0:?}")]
    MissingPrice(AssetId),
    /// A typed view of [`crate::PositionExtraRepr`] does not fit its bytes
    /// (size or alignment).
    #[error("position-extra view does not fit the fixed representation")]
    ExtraLayout,
    /// `apply_log` received a log this adapter never subscribed to.
    #[error("log not recognised by this adapter")]
    UnexpectedLog,
    /// A subscribed log's topics/data do not have the expected shape.
    #[error("malformed log payload")]
    MalformedLog,
    /// The flash route funds a different asset than the chosen repay leg.
    #[error("flash route asset does not match the repay asset")]
    FundingAssetMismatch,
    /// The flash route borrows less than the leg must repay.
    #[error("flash amount below repay amount")]
    FundingShort,
    /// `FlashRoute::callback` is not the shape `FlashRoute::provider` uses.
    #[error("callback shape does not belong to the flash provider")]
    CallbackProviderMismatch,
    /// `encode` was given the zero address as recipient.
    #[error("recipient is the zero address")]
    ZeroRecipient,
    /// An amount does not fit the wire's `u128` (PLAN-ENCODING §1b).
    #[error("amount exceeds the u128 wire width")]
    AmountTooLarge,
    /// `quote` produced no repay or no seize option.
    #[error("quote has an empty option set")]
    EmptyQuote,
    /// `encode` was asked for a repay/seize index the quote does not have.
    #[error("leg choice outside the quote's option sets")]
    LegOutOfRange,
    /// An adapter invariant that its own parameters guarantee did not hold
    /// (a solve failed to bracket, a denominator the protocol keeps positive
    /// was not). The parameters were read wrong; never silently continue.
    #[error("adapter internal invariant violated")]
    Internal,
    /// The replay archive failed while backfilling.
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    /// The adapter has no ground-truth view call for this position (no view
    /// function on the protocol, or the market's contract is not configured).
    /// The drift detector records the gap instead of comparing.
    #[error("health probe unavailable for this position")]
    ProbeUnavailable,
    /// The on-chain probe returned bytes the adapter cannot decode.
    #[error("health probe result malformed")]
    ProbeDecode,
}

/// Failure of an [`crate::Archive`] read.
///
/// `Truncated` exists because pruned-node `eth_getLogs` has returned silently
/// empty ranges instead of erroring (GUIDE 05 §1). An archive that cannot
/// prove a range complete must say so, never return an empty stream.
#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArchiveError {
    /// The source is not reachable or not open.
    #[error("archive unavailable")]
    Unavailable,
    /// The archive cannot serve blocks at or beyond `from`.
    #[error("archive truncated at block {from}")]
    Truncated { from: u64 },
    /// A stored record failed to decode.
    #[error("archive record malformed at block {block}")]
    Malformed { block: u64 },
}
