//! `liq-router` — WP 12A-1: exit routing (GUIDE 12 §3, §3b, §4, §4b, §4c).
//!
//! Two tiers over one log-driven pool book:
//!
//! * **warm** ([`warm`]) — the wiring layer's builder thread re-solves
//!   every routable `(coll, debt)` pair at a bucket ladder after each block
//!   and publishes a [`warm::RouteTable`] through `ArcSwap`. The hot path
//!   reads it with one `load` and one indexed read: no allocation, no lock,
//!   no swap math.
//! * **exact** ([`exact`]) — on a real candidate, water-fill the seized
//!   amount across the pair's pool set on *marginal* cost (fee + impact),
//!   greedy on hop gas, then order K collaterals by realised output.
//!
//! [`band`] turns a pair's exact quote into the per-block
//! [`band::ViabilityBand`] on the exact next base fee; [`cache`] is the
//! [`liq_protocol::RouteCache`] implementation the 07B eligibility filter
//! consumes (supersedes `liq_flash::eligibility::DepthOnlyRouteCache`).
//!
//! # Dependency deviation (recorded for 17A / 05E)
//!
//! The work package named `amms-rs` and `Dex-Math-Core-rs`. Both were
//! evaluated and rejected for this tree:
//!
//! * `amms` pulls `rug`/`gmp-mpfr-sys` (a C GMP build; no MSVC toolchain
//!   path), `rayon` (forbidden in this crate by GUIDE 12 acceptance), and
//!   an `alloy` line that does not unify with the workspace's. Its pool
//!   sync is provider-driven (`eth_call` batches), which is what the
//!   hot-path rule forbids.
//! * `Dex-Math-Core-rs` depends on `ethers-core` + `primitive-types::U256`
//!   (a second big-int type across the seam) and its V3 path is not a
//!   chain-exact `SwapMath` port (fee applied to the whole input rather
//!   than per step).
//!
//! Both libraries bottom out in `uniswap_v3_math` (0xKitsune's line-by-line
//! Solidity port of `SwapMath`/`SqrtPriceMath`/`TickMath`/`FullMath`). This
//! crate takes that primitive directly, ports V2 (`getAmountOut`) and Curve
//! StableSwap (`get_D`/`get_y` Newton, `exchange` rounding) itself, and
//! folds pool state from the log stream (a [`liq_types::LogSubscriber`]).
//! The 05E parity acceptance (these quotes vs `revm` on recorded states) is
//! unchanged in meaning; the implementation under test is [`solver`].
//!
//! # Venue policy
//!
//! [`Venue`] is a closed enum: `UniV2`, `UniV3`, `CurveStable`. Uniswap V4
//! and Balancer have no variant and cannot be constructed.
//!
//! # Threads and network
//!
//! No `thread::spawn`, no `rayon`, no provider, no HTTP client anywhere in
//! this crate (asserted in `cache::tests`). State arrives as logs.

#![forbid(unsafe_code)]

pub mod band;
pub mod cache;
pub mod exact;
pub mod solver;
pub mod warm;

#[cfg(test)]
pub(crate) mod fixtures;

pub use band::{
    build_table, compute_band, next_base_fee, BandCtx, BandInputs, BandTable, PairTerms,
    ViabilityBand,
};
pub use cache::WarmRouteCache;
pub use exact::{
    solve_batch, solve_on, solve_pair, Allocation, BatchQuote, ExitQuote, GasTerms, SolveBudget,
};
pub use solver::{
    CurveState, Leg, Pool, PoolBook, PoolId, PoolState, RouteError, Tick, V2State, V3State, Venue,
};
pub use warm::{Bucket, RouteEntry, RouteTable, WarmBuilder, WarmConfig, WarmInputs};
