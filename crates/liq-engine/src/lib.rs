//! Health engine (GUIDE 08): a price tick in, the exact set of positions
//! that crossed out, in `O(log N + k)`, as prioritised [`Candidate`]s.
//!
//! * [`band`] — recompute cadence per position; a cost optimisation only.
//! * [`threshold`] — the correctness mechanism: sorted per-asset thresholds
//!   with lazy re-registration and generation-based lazy deletion.
//! * [`heap`] — closed-form accrual crossings, generation-guarded.
//! * [`candidate`] — [`Candidate`], the full [`TriggerCause`] set and the
//!   bounded, value-ordered [`CandidateQueue`].
//! * [`engine`] — [`Engine`]: `on_price_tick`, `on_block`, `on_dirty`,
//!   `on_flash_change`; canonical prices mutate state, pending prices only
//!   evaluate, predicted prices never fire.
//! * [`triggers`] — WP 08B: derived-rate fan-out into `on_price_tick`,
//!   `ThresholdIndex` crossing precompute, stale-N gate, `ParamChange` fire
//!   at the execution block.
//!
//! Single-writer by construction (GUIDE 08 §5b): the engine owns every
//! structure, is driven synchronously from the ExEx thread, and holds no
//! lock, no atomic and no `Arc`. Adapters are reached only through the
//! [`liq_protocol::Protocol`] trait — this crate has no `liq-adapters`
//! dependency (GUIDE 00 lint).

pub mod band;
pub mod candidate;
pub mod engine;
pub mod heap;
pub mod threshold;
pub mod triggers;

use liq_types::fixed::FixedError;
use liq_types::{AssetId, ProtocolId};

pub use band::{classify, BandManager};
pub use candidate::{Candidate, CandidateQueue, Drain, TriggerCause};
pub use engine::{Engine, EngineConfig, ProtocolPriceMove, ProtocolPrices, Stats, World};
pub use heap::TimeCrossHeap;
pub use threshold::{Side, ThresholdIndex};
pub use triggers::{
    attach_crossing, crossing_set, fire_param_change, kind, on_derived_tick, registered_set,
    take_ripe, StaleConfig, TriggerError,
};

/// Engine failure. Every variant is `Copy` and heap-free: the error path
/// on the hot thread never allocates (RUST-CONVENTIONS §10). Any `Err` from
/// an `on_*` entry point means the engine's view is no longer trustworthy
/// for that input and the caller halts the protocol (GUIDE 08 §5b) — the
/// engine never substitutes a value.
#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// The asset is outside the engine's sized price universe.
    #[error("asset {0:?} outside the engine's price universe")]
    UnknownAsset(AssetId),
    /// No protocol registered at this id, or the one there reports another.
    #[error("no protocol registered for {0:?}")]
    UnknownProtocol(ProtocolId),
    /// A loaded price vector is not indexed by global asset id: slot `slot`
    /// holds `found`. Adapters index the vector by `AssetId`, so this
    /// layout would price every position wrong; refused outright.
    #[error("price slot {slot} holds asset {found:?}")]
    PriceLayout { slot: u16, found: AssetId },
    #[error(transparent)]
    Protocol(#[from] liq_protocol::ProtocolError),
    #[error(transparent)]
    State(#[from] liq_state::StateError),
    #[error(transparent)]
    Fixed(#[from] FixedError),
}
