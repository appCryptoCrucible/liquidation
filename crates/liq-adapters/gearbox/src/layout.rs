//! Store layout for one Gearbox V3 `CreditManagerV3`
//! (`Gearbox-protocol/core-v3` @ `510fc6541c3767ce825929b4c311826fe81d6fa5`).
//!
//! One interned [`liq_types::MarketId`] per credit manager (4201..=4299).
//! Slot 0 is the manager underlying (`UNDERLYING_TOKEN_MASK = 1`). Later
//! slots follow `getTokenByMask(1 << slot)` order.

use bytemuck::{Pod, Zeroable};
use liq_types::AssetId;

/// Slot 0 body: per-manager `fees()` + facade/pool pins + underlying LT.
/// 128 bytes (8 × 16).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct ManagerRow {
    pub manager: [u8; 20],
    pub facade: [u8; 20],
    pub pool: [u8; 20],
    pub underlying: [u8; 20],
    pub fee_interest: u16,
    pub fee_liquidation: u16,
    pub liquidation_discount: u16,
    pub fee_liquidation_expired: u16,
    pub liquidation_discount_expired: u16,
    pub lt_underlying: u16,
    pub expiration_date: u32,
    pub quoted_tokens_mask: u64,
    pub flags: u8,
    pub expirable: u8,
    pub token_count: u8,
    /// Facade `debtLimits().minDebt`, u128 big-endian (unaligned so the row
    /// stays 128 bytes). A partial liquidation must leave at least this.
    pub min_debt: [u8; 16],
    pub _pad: [u8; 5],
}

impl ManagerRow {
    /// `fees()` was written. Health fails closed without it.
    pub const FEES: u8 = 1 << 0;
    /// Token/underlying interned against a known [`AssetId`].
    pub const PRICED: u8 = 1 << 1;
    /// Facade `Paused`.
    pub const PAUSED: u8 = 1 << 2;
}

/// Slots 1.. : one collateral token's LT ramp (`CreditLogic.getLiquidationThreshold`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct TokenRow {
    pub token: [u8; 20],
    pub lt_initial: u16,
    pub lt_final: u16,
    pub ramp_start: u64,
    pub ramp_duration: u32,
    pub flags: u8,
    pub _pad: [u8; 11],
}

impl TokenRow {
    pub const LISTED: u8 = 1 << 0;
    pub const PRICED: u8 = 1 << 1;
}

/// Per-account extra (`CreditAccountInfo` minus principal, which is `debt[0]`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct AccountExtra {
    pub cumulative_index_last_update: u128,
    pub cumulative_quota_interest: u128,
    pub quota_fees: u128,
    pub enabled_tokens_mask: u64,
    pub last_debt_update: u32,
    pub flags: u8,
    pub _pad: [u8; 3],
}

impl AccountExtra {
    pub const OPEN: u8 = 1 << 0;
}

/// Per-slot quota (`IPoolQuotaKeeperV3.getQuota`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct QuotaExtra {
    pub quota: u128,
    pub flags: u8,
    pub _pad0: [u8; 32],
    pub _pad1: [u8; 15],
}

impl QuotaExtra {
    /// `UpdateQuota` has been observed for this slot (token is in the
    /// account's quoted set). Unquoted non-underlying tokens are omitted
    /// from pin `CollateralLogic.calcCollateral`.
    pub const QUOTED: u8 = 1 << 0;
}

pub const UNDERLYING_SLOT: u16 = 0;
pub const UNDERLYING_TOKEN_MASK: u64 = 1;
pub const UNMAPPED_ASSET: AssetId = AssetId(u16::MAX);
/// Pin `MAX_SANE_ENABLED_TOKENS`.
pub const MAX_TOKENS: u8 = 20;

const _: () = {
    assert!(core::mem::size_of::<ManagerRow>() == 128);
    assert!(core::mem::align_of::<ManagerRow>() <= 16);
    assert!(core::mem::size_of::<TokenRow>() == 48);
    assert!(core::mem::size_of::<AccountExtra>() == 64);
    assert!(core::mem::size_of::<AccountExtra>() <= liq_protocol::PositionExtraRepr::SIZE);
    assert!(core::mem::size_of::<QuotaExtra>() == 64);
};
