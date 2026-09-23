//! `liq-protocol` — the `Protocol` trait and the storage contract every
//! adapter implements and the engine consumes (GUIDE 01; D46).
//!
//! Types only, no logic beyond a bonus-curve evaluation and the conformance
//! harness. Depends on `liq-types` and nothing else in the workspace: not on
//! `liq-state` (it defines the `StateWriter`, `PositionRef`, `MarketRow`,
//! `PositionExtraRepr` that `liq-state` implements and lays out), not on any
//! adapter (adapters implement `Protocol`; the conformance harness is generic
//! over it). The dependency lint enforces both edges.
//!
//! Module map: `protocol` (the trait) · `health` · `quote` + `bonus` · `dirty`
//! · `posref` + `mask` + `market` + `extra` (the storage contract) · `flash` +
//! `plan` (funding as an `encode` input, the leg as its output) · `log` ·
//! `statewriter` · `routecache` · `archive` (the three dependency-inversion
//! traits) · `conformance` (the ten-check harness).

#![deny(clippy::todo, clippy::unimplemented)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod archive;
pub mod bonus;
pub mod conformance;
pub mod dirty;
pub mod error;
pub mod extra;
pub mod flash;
pub mod health;
pub mod log;
pub mod market;
pub mod mask;
pub mod plan;
pub mod posref;
pub mod protocol;
pub mod quote;
pub mod routecache;
pub mod statewriter;

/// Block number. Plain `u64`, matching `liq_types::Price::block`.
pub type BlockNum = u64;
/// Unix seconds. Plain `u64`, matching `liq_types::Price::ts` and the chain's
/// `block.timestamp`.
pub type Timestamp = u64;

pub use archive::Archive;
pub use bonus::{AuctionCurve, BonusCurve};
pub use dirty::{DirtyPositions, DirtyRows, DirtySet};
pub use error::{ArchiveError, ProtocolError, Result};
pub use extra::PositionExtraRepr;
pub use flash::{CallbackShape, FlashRoute};
pub use health::{BlockReason, Health, HealthState};
pub use log::DecodedLog;
pub use market::{FeedId, MarketFlags, MarketRow, MarketSlot};
pub use mask::{AssetMask, SetSlots};
pub use plan::{ExecutorAdapter, LiquidationLeg, LiquidationPlan, ProbeCall};
pub use posref::PositionRef;
pub use protocol::Protocol;
pub use quote::{Constraints, LegChoice, Quote, RepayOption, SeizeOption, SlotRef};
pub use routecache::RouteCache;
pub use statewriter::StateWriter;
