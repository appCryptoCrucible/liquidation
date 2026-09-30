//! BatchPlan encoder and `validate()` for executor round-trip tests (WP 10B).
//!
//! Inverse of `contracts/src/lib/PlanDecoder.sol` / `liq_wire::wire`. Does
//! not modify the frozen decoder.

#![deny(clippy::todo, clippy::unimplemented)]

pub mod encode;
pub mod error;
pub mod types;
pub mod validate;

pub use encode::{decode_batch, ensure_surplus_borrow_profit_legs};
pub use error::{EncodeError, Result};
/// Fluid tail kinds and flag bits (`PlanDecoder.sol` `FLUID_*`).
pub use liq_wire::wire::{
    FLUID_ABSORB, FLUID_COL_TOKEN1, FLUID_DEBT_TOKEN1, FLUID_FLAGS, FLUID_NATIVE_COL,
    FLUID_NATIVE_DEBT, FLUID_T1, FLUID_T2, FLUID_T3, FLUID_T4,
};
pub use types::EncodedPlan;
pub use types::{
    is_unwrap_venue, BatchPlan, CompoundMarketPin, FlashGroup, LiqLeg, LiquityTrovePin,
    MorphoMarketPin, SwapLeg, V4ReservePin, ValidateCtx, FLAG_GOV_EXEC, FLAG_GOV_SPELL, FLAG_SWEEP,
    GROUP_HEAD_LEN, HEADER_LEN, LEG_EXACT_OUT, LEG_TAKE_BALANCE, LIQ_LEG_LEN, SWAP_LEG_HEAD_LEN,
    V2_FACTORY_SUSHI, V2_FACTORY_UNISWAP, VENUE_CURVE_CRYPTO_POOL, VENUE_CURVE_LP_ONE_COIN,
    VENUE_CURVE_POOL, VENUE_PENDLE_MARKET_SELL, VENUE_PENDLE_PT_REDEEM, VENUE_ROUTER,
    VENUE_UNIV2_POOL, VENUE_UNIV3_POOL, VENUE_UNWRAP_4626,
};
pub use validate::{col_per_unit_debt_1e18, morpho_actual_pull, morpho_id, validate};
