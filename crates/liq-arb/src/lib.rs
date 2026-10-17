//! `liq-arb` — backruns solved the way liquidations are: anchored on the
//! pool a victim moved, exited through the liquidation router.
//!
//! A swap leaves one pool off the market. The backrun trades that pool back
//! (the anchor, [`anchor::Anchor`]) and closes the loop through the same
//! exit the liquidation path uses ([`liq_router::solve_pair`]: direct pools
//! water-filled, the hub, graph hubs and chains), refined once at the
//! chosen size ([`liq_router::exact::refine_exit`]). There is no cycle
//! enumeration and no ranking at zero size: the research walk showed that
//! ranking discards the deep, small-edge cycles winning backrunners take,
//! while the anchored exit matched or beat every winner it was set against.
//!
//! * [`size`] — the anchor amount, by Brent's method on integers (parabolic
//!   steps, golden-section fallback) inside a bracket set from the victim's
//!   own size.
//! * [`anchor`] — the anchored backrun in both orientations (flash the
//!   anchor's input and close the loop through the exit, or flash its
//!   output, exit first and finish through the anchor), whichever start
//!   token is flash-loanable.
//! * [`realize`] — the chosen plan replayed swap by swap on scratch state,
//!   in execution order; a plan whose realized output differs from what it
//!   claimed by a single unit is refused.
//! * [`plan`] — the plan `ArbExecutor` runs: one flash group whose swap
//!   legs are the cycle, funded by the V3 anchor's flash swap where there
//!   is one and a free flash loan otherwise, profit closed into WETH.
//! * [`scratch`] — save and restore of pool states for that, without
//!   allocating once a pool's slot has been sized.
//!
//! # Threads, network, allocation
//!
//! Synchronous, single-threaded, no async runtime, no provider: the arb
//! thread owns its book and calls in. Pool states are saved into reused
//! slots ([`liq_router::PoolState::copy_from`]); results are fixed-size
//! apart from the router's own [`liq_router::ExitQuote`].

#![forbid(unsafe_code)]

pub mod anchor;
pub mod engine;
pub mod plan;
pub mod realize;
pub mod scratch;
pub mod size;
#[cfg(test)]
pub(crate) mod testbook;
pub mod victim;

pub use anchor::{best_backrun, Anchor, ArbCtx, Backrun, Orientation};
pub use engine::{
    pool_view, Engine, EngineCfg, NoBackrun, Opportunity, PoolView, Target, PLAN_HOPS, TOP_ANCHORS,
};
pub use plan::{
    build_plan, expected_weth_gross, expected_weth_gross_rec, kept_after_bid, FlashSource, PlanCtx,
};
pub use realize::{apply_exit, realize, Hop};
pub use scratch::Scratch;
pub use size::{maximise, Maximum};
pub use victim::{apply_victim, swap_seen, LogRef, Skip, SwapSeen};

use liq_router::RouteError;

/// Why a backrun could not be solved. Fieldless where it can be: these sit
/// on the arb thread's path and carry no strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArbError {
    /// A pool, asset or price the solve needs is not in the book.
    #[error("missing {0}")]
    Missing(&'static str),
    /// Checked arithmetic refused (overflow, division by zero).
    #[error("arithmetic refused")]
    Math,
    /// The router refused a quote or solve.
    #[error("route: {0:?}")]
    Route(RouteError),
    /// The plan's exit has a part [`realize`] does not replay (an unwrap,
    /// split chains); it is not sent.
    #[error("exit shape not replayable")]
    Unreplayable,
    /// The plan encoder refused the plan (`liq_plan::validate_arb`).
    #[error("plan not encodable")]
    Unencodable,
    /// Replayed swap by swap, the plan returns a different amount than it
    /// claimed. Never sent.
    #[error("realized {realized} != claimed {claimed}")]
    Mismatch {
        claimed: alloy_primitives::U256,
        realized: alloy_primitives::U256,
    },
}

impl From<RouteError> for ArbError {
    fn from(e: RouteError) -> Self {
        Self::Route(e)
    }
}
