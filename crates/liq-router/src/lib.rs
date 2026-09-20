//! `liq-router` — GUIDE 12.
//!
//! **12A-1** (routing): two tiers over one log-driven pool book.
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
//! **12A-2** (additional modules): [`profit`] sizes (`min4`) and scores
//! combinations serially; [`select`] is a type-level eligibility **gate**
//! then pre-gas ranking; [`assemble`] emits a `BatchPlan` that
//! `liq_plan::validate` accepts; [`gas`] is the header-driven oracle;
//! [`bid`] is the learning-phase near-cap bid (`F(β)` is 12B).
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
//! [`Venue`] is a closed enum: `UniV2`, `UniV3`, `CurveStable`. Uniswap V4,
//! Balancer, and Kyber have no variant and cannot be constructed. 05E N1:
//! do not invent Kyber legs.
//!
//! # Threads and network
//!
//! No `thread::spawn`, no `thread::scope`, no `rayon`, no provider, no HTTP
//! client anywhere in this crate (asserted in tests). Combination search
//! is serial (GUIDE 12 §3b). State arrives as logs.

#![forbid(unsafe_code)]

pub mod assemble;
pub mod band;
pub mod bid;
pub mod cache;
pub mod exact;
pub mod gas;
pub mod profit;
pub mod select;
pub mod solver;
pub mod warm;

#[cfg(test)]
pub(crate) mod fixtures;

pub use assemble::{assemble, reencode_with, AssembleError, AssembleView, Assembled, LegMeta};
pub use band::{
    build_table, compute_band, next_base_fee, BandCtx, BandInputs, BandTable, PairTerms,
    ViabilityBand,
};
pub use bid::{bid, searcher_net, Bid, BidConfig, BidError};
pub use cache::WarmRouteCache;
pub use exact::{
    solve_batch, solve_on, solve_pair, Allocation, BatchQuote, ExitQuote, GasTerms, SolveBudget,
};
pub use gas::{header_gas_limit, GasError, GasOracle};
pub use profit::{
    best_plan, delta_net, evaluate, expected_contrib_per_gas, historical_profit_parity, min4,
    seized_for, MarketView, ProfitCtx, ProfitError, SizedLeg, LEARNING_P_RAY,
};
pub use select::{
    admit, is_liquidatable, select, Eligible, PositionInput, SelectCfg, SelectedPlan,
};
pub use solver::{
    CurveState, Leg, Pool, PoolBook, PoolId, PoolState, RouteError, Tick, V2State, V3State, Venue,
};
pub use warm::{Bucket, RouteEntry, RouteTable, WarmBuilder, WarmConfig, WarmInputs};

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::panic, clippy::unwrap_used)]
mod crate_rules {
    /// GUIDE 12 acceptance: no HTTP client and no thread spawning in this
    /// crate — asserted on the new 12A-2 sources as well as 12A-1.
    #[test]
    fn no_http_no_threads_in_12a2() {
        let srcs = [
            include_str!("profit.rs"),
            include_str!("select.rs"),
            include_str!("assemble.rs"),
            include_str!("gas.rs"),
            include_str!("bid.rs"),
            include_str!("lib.rs"),
        ];
        let banned = [
            concat!("thread::", "spawn"),
            concat!("thread::", "scope"),
            concat!("ray", "on"),
            concat!("req", "west"),
        ];
        for s in srcs {
            let code_only: String = s
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            for b in banned {
                assert!(!code_only.contains(b), "{b} found in 12A-2 source");
            }
        }
        let manifest = include_str!("../Cargo.toml");
        for banned in [
            concat!("req", "west"),
            "hyper",
            concat!("alloy-", "provider"),
            "tokio",
            concat!("ray", "on"),
        ] {
            assert!(
                !manifest.contains(banned),
                "{banned} in liq-router manifest"
            );
        }
    }
}
