//! Store layout for one Compound V2 Comptroller (intern `MarketId`).
//! Slot 0 is [`ComptrollerMeta`]; slots `1..` are listed cTokens.

use bytemuck::{Pod, Zeroable};
use liq_types::AssetId;

/// Slot-0 body: per-comptroller admin file (`closeFactorMantissa`,
/// `liquidationIncentiveMantissa`, `oracle`). Never a protocol-wide constant.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct ComptrollerMeta {
    pub close_factor_mantissa: u128,
    pub liquidation_incentive_mantissa: u128,
    pub oracle: [u8; 20],
    pub flags: u8,
    pub _pad: [u8; 11],
}

impl ComptrollerMeta {
    pub const SEIZE_PAUSED: u8 = 1 << 0;
    pub const PARAMS_KNOWN: u8 = 1 << 1;
}

/// One listed cToken. `underlying == [0; 20]` means CEther (no ERC-20
/// `underlying()`), not a guessed symbol.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct CTokenRow {
    pub exchange_rate_mantissa: u128,
    pub borrow_index: u128,
    pub collateral_factor_mantissa: u128,
    pub reserve_factor_mantissa: u128,
    pub cash: u128,
    pub total_borrows: u128,
    pub total_reserves: u128,
    pub total_supply: u128,
    pub ctoken: [u8; 20],
    pub underlying: [u8; 20],
    pub flags: u8,
    pub _pad: [u8; 23],
}

impl CTokenRow {
    pub const LISTED: u8 = 1 << 0;
    pub const CETHER: u8 = 1 << 1;
    pub const PRICED: u8 = 1 << 2;
    pub const EXRATE_KNOWN: u8 = 1 << 3;
    pub const RF_KNOWN: u8 = 1 << 4;
    pub const BORROW_PAUSED: u8 = 1 << 5;
    pub const DEC_KNOWN: u8 = 1 << 6;
}

/// Per-position: entered-market mask (bit = slot). Slot 0 is never entered.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct UserExtra {
    pub entered_mask: u128,
    pub _pad: [u8; 48],
}

/// Per-slot borrow index snapshot (`account.interestIndex` after Borrow/Repay).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BorrowSnap {
    pub interest_index: u128,
    pub _pad: [u8; 48],
}

pub const META_ASSET: AssetId = AssetId(u16::MAX);
pub const UNMAPPED_ASSET: AssetId = AssetId(u16::MAX);
pub const META_SLOT: u16 = 0;
/// `CToken.borrowIndex` initial value (`1e18`).
pub const INITIAL_BORROW_INDEX: u128 = 1_000_000_000_000_000_000;

const _: () = {
    assert!(core::mem::size_of::<ComptrollerMeta>() == 64);
    assert!(core::mem::align_of::<ComptrollerMeta>() == 16);
    assert!(core::mem::size_of::<CTokenRow>() == 192);
    assert!(core::mem::align_of::<CTokenRow>() == 16);
    assert!(core::mem::size_of::<UserExtra>() == 64);
    assert!(core::mem::size_of::<BorrowSnap>() == 64);
    assert!(core::mem::size_of::<UserExtra>() <= liq_protocol::PositionExtraRepr::SIZE);
    assert!(core::mem::size_of::<BorrowSnap>() <= liq_protocol::PositionExtraRepr::SIZE);
};
