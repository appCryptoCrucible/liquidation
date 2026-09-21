//! BatchPlan encoder and `validate()` for executor round-trip tests (WP 10B).
//!
//! Inverse of `contracts/src/lib/PlanDecoder.sol` / `liq_exec::wire`. Does
//! not modify the frozen decoder.

#![deny(clippy::todo, clippy::unimplemented)]

pub mod encode;
pub mod error;
pub mod types;
pub mod validate;

pub use encode::{decode_batch, ensure_surplus_borrow_profit_legs};
pub use error::{EncodeError, Result};
pub use types::EncodedPlan;
pub use types::{
    BatchPlan, CompoundMarketPin, FlashGroup, LiqLeg, LiquityTrovePin, MorphoMarketPin, SwapLeg,
    V4ReservePin, ValidateCtx, FLAG_SWEEP, GROUP_HEAD_LEN, HEADER_LEN, LEG_EXACT_OUT,
    LEG_TAKE_BALANCE, LIQ_LEG_LEN, SWAP_LEG_HEAD_LEN, VENUE_ROUTER, VENUE_UNIV3_POOL,
};
pub use validate::{col_per_unit_debt_1e18, morpho_actual_pull, morpho_id, validate};
