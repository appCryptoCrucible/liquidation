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
    /// A typed view of [`crate::MarketRow::body`] does not fit its bytes
    /// (size or alignment).
    #[error("market-row body view does not fit the fixed representation")]
    BodyLayout,
    /// `encode` was handed a quote another adapter built
    /// (`q.key.protocol != self.id()`). Encoding it would put this adapter's
    /// `ExecutorAdapter` byte in front of the other protocol's borrower.
    #[error("quote belongs to another protocol")]
    ProtocolMismatch,
    /// The protocol re-pointed a reserve's price source at an address other
    /// than the one the feed registry pins, or listed an underlying the
    /// registry does not know (GUIDE 06 §2, 06A-1 carry-forward). Fail
    /// closed: the reserve is unpriced (`MarketFlags::UNPRICED`) until the
    /// registry is updated; a position holding it has no health.
    #[error("reserve price source or underlying is not pinned by the registry")]
    OracleSourceMismatch,
    /// A halt-class log (proxy upgrade, authority change, oracle binding —
    /// `docs/coverage/aave-v4.md` `halt` rows): the adapter's view of the
    /// contract can no longer be trusted. The caller routes this to the halt
    /// matrix; state was not modified.
    #[error("halt-class log: adapter must stop")]
    HaltSignal,
    /// The evaluation time is earlier than a row's `last_update`: the view
    /// is inconsistent (the chain itself reverts here —
    /// `MathUtils.calculateLinearInterest`).
    #[error("evaluation time precedes the row's last update")]
    TimestampBeforeUpdate,
    /// A listing log's slot is not the next free slot of its market: a log
    /// was skipped or replayed out of order. The store is not patched.
    #[error("listing slot {got} is not the next free slot {expected}")]
    SlotMismatch { expected: u16, got: u16 },
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
    /// A fixed-capacity adapter table is full. Distinct from [`Self::Internal`]
    /// because it is a sizing decision that was outgrown, not a broken
    /// invariant: the fix is to raise the table, and the message has to say
    /// which one and how big it is for that to be actionable. Aave V3's
    /// e-mode table is the case this exists for.
    #[error("{table} is full at {cap} entries")]
    TableFull {
        /// Name of the table, as it appears in the adapter's layout.
        table: &'static str,
        /// The capacity that was exhausted.
        cap: usize,
    },
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
    /// `encode` cannot emit an Executor wire plan: the path is unwired on
    /// purpose (Fluid T2/T3/T4, Gearbox full-close MultiCall). Do not invent
    /// an ABI, share rate, or MultiCall fill. After H3, a new family is 10R-n.
    #[error("executor adapter not wired (unpriced or non-T1 path)")]
    ExecutorUnwired,
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
