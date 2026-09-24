//! The `Protocol` trait (GUIDE 01 §2) — the single abstraction every lending
//! protocol is expressed through. Adapters implement it; `liq-engine`,
//! `liq-flash` and `liq-router` consume it and never see an adapter crate.
//!
//! If adding protocol #N requires changing `liq-engine`, the trait leaked a
//! protocol assumption: stop and fix it here (ORCHESTRATOR §10).
//!
//! **Deviations from GUIDE 01 §2, and why.** The hot-path methods return
//! `Result`: a price missing from the vector or a fixed-point overflow must
//! surface as an error — the library may neither panic (RUST-CONVENTIONS §2)
//! nor substitute a value (project rule: no fabricated data). `encode` takes a
//! [`LegChoice`] because a `Quote` holds option *sets*; without it the pair to
//! encode is ambiguous. `health_probe` is fallible for the same no-panic
//! reason.

use alloy_primitives::Address;
use liq_types::{AssetId, LogSubscriber, Price, PriceVector, ProtocolId};

use crate::archive::Archive;
use crate::dirty::DirtySet;
use crate::error::Result;
use crate::flash::FlashRoute;
use crate::health::Health;
use crate::log::DecodedLog;
use crate::plan::{LiquidationPlan, ProbeCall};
use crate::posref::PositionRef;
use crate::quote::{LegChoice, Quote};
use crate::statewriter::StateWriter;
use crate::{BlockNum, Timestamp};

/// One lending protocol.
///
/// `LogSubscriber` (from `liq-types`) supplies `subscriptions()`: the
/// `LogRouter` is built from every subscriber — protocols, flash sources,
/// feeds — through that one trait (D46).
pub trait Protocol: LogSubscriber + Send + Sync + 'static {
    fn id(&self) -> ProtocolId;

    // ---- ingest ------------------------------------------------------------

    /// Fold one routed log into state. Every mutation goes through `st`, which
    /// records its inverse. Returns exactly what the log invalidated — no
    /// wider (conformance check 7).
    fn apply_log(&self, st: &mut dyn StateWriter, log: &DecodedLog<'_>) -> Result<DirtySet>;

    /// Rebuild state from `src` up to and including block `to` (GUIDE 03 §5).
    fn backfill(&self, st: &mut dyn StateWriter, src: &dyn Archive, to: BlockNum) -> Result<()>;

    // ---- health: pure, hot path, allocation-free ---------------------------

    /// Normalised health at `px`, evaluated at `pos.timestamp`. Pure and
    /// allocation-free; iterates `pos.config` set bits and touches nothing
    /// else. Must match the protocol's own view function to the wei
    /// (differential fuzz, GUIDE 05 §6). `Err(MissingPrice)` when `px` lacks
    /// an asset the position holds.
    fn health(&self, pos: PositionRef<'_>, px: &PriceVector) -> Result<Health>;

    /// Price of `asset` at which `hf` crosses `1.0`, all other prices fixed:
    /// the **last healthy price** — `health` at it is `>= 1.0` and at the
    /// adjacent price unit toward danger is `< 1.0` (conformance check 3,
    /// exact in the adapter's own rounding). `None` when the position does
    /// not cross on this asset alone (it holds none, has no debt, or holds
    /// it as both collateral and debt such that no crossing exists — GUIDE
    /// 04 §5).
    fn liquidation_price(
        &self,
        pos: PositionRef<'_>,
        px: &PriceVector,
        asset: AssetId,
    ) -> Result<Option<Price>>;

    /// Instant at which accrual alone (base rate **and** any per-position
    /// premium) brings `hf` to `1.0` with prices fixed; `None` if never.
    fn time_to_cross(&self, pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Timestamp>>;

    // ---- action ------------------------------------------------------------

    /// Full economics at the current health. `None` unless the position is
    /// liquidatable now (`Healthy`, `Blocked` and `SoftLiquidating` never
    /// quote — conformance check 10). Called repeatedly by the timing
    /// optimiser (GUIDE 12), so it must be cheap and pure.
    ///
    /// No caller-side size cap: `RepayOption::max_repay` /
    /// `SeizeOption::max_seize` are the protocol rule's own ceiling, full
    /// stop. Sizing below that ceiling is the viability band's job (GUIDE 12
    /// §4b), downstream in `liq-router`, not this call — a per-call cap
    /// here would be a second, adapter-shaped copy of exactly the derived
    /// threshold that section exists to retire.
    fn quote(&self, pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Quote>>;

    /// Turn a chosen `(repay, seize)` pair of `q` and a chosen flash route
    /// into the concrete leg. `funding` is the only place funding enters the
    /// trait (GUIDE 01 §5). Must validate: `legs` index into `q`;
    /// `funding.asset` is the repay leg's asset; `funding.amount >=
    /// max_repay`; `funding.callback.provider() == funding.provider`;
    /// `recipient != 0` — and produce a plan valid under every
    /// `CallbackShape` (conformance check 9).
    fn encode(
        &self,
        q: &Quote,
        legs: LegChoice,
        funding: &FlashRoute,
        recipient: Address,
    ) -> Result<LiquidationPlan>;

    // ---- ground truth ------------------------------------------------------

    /// The `eth_call` whose decoded result the drift detector compares to
    /// `health(pos)?.hf` (GUIDE 02 §8). `Err(ProbeUnavailable)` when the
    /// adapter has no such view for this position.
    fn health_probe(&self, pos: PositionRef<'_>) -> Result<ProbeCall>;
}
