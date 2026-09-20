//! `BatchPlan` — the off-chain value the encoder packs (PLAN-ENCODING §2).

use alloy_primitives::{Address, B256};
use liq_exec::wire::LegTail;
use liq_protocol::ExecutorAdapter;
use liq_types::FlashProvider;

/// Header flag bit 0 — sweep WETH to `PROFIT_SINK` after this plan.
pub const FLAG_SWEEP: u8 = liq_exec::wire::FLAG_SWEEP;
/// Swap-leg flag bit 0 — spend the whole `tokenIn` balance.
pub const LEG_TAKE_BALANCE: u8 = liq_exec::wire::LEG_TAKE_BALANCE;
/// Swap-leg flag bit 1 — `amount` is an exact output.
pub const LEG_EXACT_OUT: u8 = liq_exec::wire::LEG_EXACT_OUT;
pub const VENUE_UNIV3_POOL: u8 = liq_exec::wire::VENUE_UNIV3_POOL;
pub const VENUE_ROUTER: u8 = liq_exec::wire::VENUE_ROUTER;

pub const HEADER_LEN: usize = liq_exec::wire::HEADER_LEN;
pub const GROUP_HEAD_LEN: usize = liq_exec::wire::GROUP_HEAD_LEN;
pub const LIQ_LEG_LEN: usize = liq_exec::wire::LIQ_LEG_LEN;
pub const SWAP_LEG_HEAD_LEN: usize = liq_exec::wire::SWAP_LEG_HEAD_LEN;

/// One packed plan ready for `Executor.execute(bytes)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedPlan(pub Vec<u8>);

impl EncodedPlan {
    #[inline]
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[inline]
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchPlan {
    pub flags: u8,
    pub bid_bps: u16,
    pub gas_cost_wei: u128,
    pub min_profit_wei: u128,
    pub groups: Vec<FlashGroup>,
    pub profit_swaps: Vec<SwapLeg>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlashGroup {
    pub provider: FlashProvider,
    pub flash_source: Address,
    pub debt_asset: Address,
    pub flash_amount: u128,
    pub liqs: Vec<LiqLeg>,
    pub repay_swaps: Vec<SwapLeg>,
}

/// Liquidation leg: 77 fixed bytes plus [`ExecutorAdapter::tail_len`].
///
/// `protocol_pull` is not on the wire. It is the amount the protocol will
/// actually take (V3 close-factor / V4 target-HF clamp / Morpho share
/// rounding). Validate sizes `EXACT_OUT` repay legs to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiqLeg {
    pub adapter: ExecutorAdapter,
    pub market: Address,
    pub borrower: Address,
    pub collateral_asset: Address,
    pub repay_amount: u128,
    pub tail: LegTail,
    pub protocol_pull: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapLeg {
    pub venue: u8,
    pub token_in: Address,
    pub token_out: Address,
    pub flags: u8,
    pub amount: u128,
    pub data: Vec<u8>,
}

/// Registry pins `validate` checks against. Built from adapter config /
/// Morpho `MarketParams` (keccak id), never guessed.
#[derive(Clone, Debug, Default)]
pub struct ValidateCtx {
    pub weth: Address,
    /// `(spoke, reserve_id) → underlying`.
    pub v4_underlying: Vec<V4ReservePin>,
    /// Morpho markets whose `Id` is `keccak256(abi.encode(MarketParams))`.
    pub morpho: Vec<MorphoMarketPin>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct V4ReservePin {
    pub spoke: Address,
    pub reserve_id: u16,
    pub underlying: Address,
}

/// Morpho Blue market. `id` must equal `keccak256(abi.encode(params))`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MorphoMarketPin {
    pub id: B256,
    pub morpho: Address,
    pub loan_token: Address,
    pub collateral_token: Address,
    pub oracle: Address,
    pub irm: Address,
    pub lltv: alloy_primitives::U256,
}

impl ValidateCtx {
    #[must_use]
    pub fn v4_pin(&self, spoke: Address, id: u16) -> Option<Address> {
        self.v4_underlying
            .iter()
            .find(|p| p.spoke == spoke && p.reserve_id == id)
            .map(|p| p.underlying)
    }

    #[must_use]
    pub fn morpho_pin(&self, id: B256) -> Option<&MorphoMarketPin> {
        self.morpho.iter().find(|p| p.id == id)
    }
}
