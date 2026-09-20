//! WP 05E — swap-math parity: Dex-Math-Core-rs formulas vs revm (GUIDE 05 §6b).
//!
//! The named library (`Dex-Math-Core-rs`) cannot enter this tree: it pulls
//! `ethers-core` + a second `U256` (12A-1 recorded that deviation). This
//! module is the same quote surface — V2 `getAmountOut`, V3 `SwapMath`,
//! Curve StableSwap `get_dy`, Kyber Elastic `computeSwapStep` — checked
//! wei-for-wei against **solc output of the official Solidity/Vyper math**
//! executed in revm.
//!
//! **Uniswap V4 is excluded.** Hooks make quoting pool-specific; there is
//! no generic V4 venue to parity-test (GUIDE 12 §3, DEPENDENCIES.md).
//! Balancer is not a routed family here either.
//!
//! **A3 fold:** 05B parquet is decoded events / prices / headers, not pool
//! storage. Real mainnet snapshots need the local archive node (D60).
//! [`recorded_mainnet_states`] returns [`ParityError::A3Deferred`]. An
//! absent archive directory is [`ParityError::ArchiveEmpty`]. Synthetic
//! fixture states are allowed for CI; they are not labelled mainnet.

mod evm;
mod kyber;
mod notional;
mod quote;
mod recorded;

pub use evm::{call_pure, insert_runtime, kyber_oracle_runtime, uni_oracle_runtime, ORACLE};
pub use kyber::compute_swap_step as kyber_compute_swap_step;
pub use notional::{biased_wad, in_traded_band, TRADED_LADDER_WAD, TRADED_WAD_MAX, TRADED_WAD_MIN};
pub use quote::{
    assert_wei_eq, curve_get_dy, v2_get_amount_out, v3_compute_swap_step, CurveQuote, CurveQuoteIn,
    KyberStep, V3Step,
};
pub use recorded::{recorded_mainnet_states, recorded_states_at, RecordedPoolState};

use thiserror::Error;

/// AMM families this harness quotes. Closed: no V4, no Balancer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AmmFamily {
    UniV2,
    UniV3,
    CurveStable,
    KyberElastic,
}

impl AmmFamily {
    /// Every family the router is allowed to price.
    pub const ALL: [Self; 4] = [
        Self::UniV2,
        Self::UniV3,
        Self::CurveStable,
        Self::KyberElastic,
    ];
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParityError {
    #[error("wei divergence family={family:?} rust={rust} revm={revm} (bug until proven)")]
    WeiDivergence {
        family: AmmFamily,
        rust: alloy_primitives::U256,
        revm: alloy_primitives::U256,
    },
    #[error("archive empty at {path}")]
    ArchiveEmpty { path: String },
    #[error("A3 local node deferred (D60): recorded pool storage not in 05B parquet")]
    A3Deferred,
    #[error("math refused: {0}")]
    Math(&'static str),
    #[error("revm: {0}")]
    Revm(String),
    #[error("abi: {0}")]
    Abi(&'static str),
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests;
