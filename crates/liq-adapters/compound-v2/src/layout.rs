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
    /// A halt-class log (proxy upgrade or admin change on the Comptroller or
    /// one of its cTokens, or a close factor outside the pin's bounds) after
    /// the pin: this fork's view is no longer trusted. Its positions are
    /// `Blocked` until a person re-pins the config; every other fork keeps
    /// running.
    pub const HALTED: u8 = 1 << 2;
}

/// One listed cToken. `underlying == [0; 20]` means CEther (no ERC-20
/// `underlying()`), not a guessed symbol.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct CTokenRow {
    pub exchange_rate_mantissa: u128,
    pub borrow_index: u128,
    pub cash: u128,
    pub total_borrows: u128,
    pub total_reserves: u128,
    pub total_supply: u128,
    /// Fuse: `totalFuseFees + totalAdminFees`; Moma: `totalFees +
    /// totalMomaFees`. The exchange rate subtracts it beside the reserves.
    /// Read each block (a Fuse fee withdrawal emits no event); `0` on a
    /// plain V2 fork.
    pub total_fees: u128,
    /// At most `1e18` (Compound caps it at 0.9e18).
    pub collateral_factor_mantissa: u64,
    /// At most `1e18` (`reserveFactorMaxMantissa`).
    pub reserve_factor_mantissa: u64,
    /// The sum of the fork's two fee rates (Fuse: `fuseFeeMantissa` and
    /// `adminFeeMantissa`; Moma: `feeFactorMantissa` and
    /// `getMomaFeeFactor()`): the share of each accrual's interest added to
    /// [`Self::total_fees`]. One combined rate
    /// differs from the cToken's two truncations by at most one wei per
    /// accrual, and the per-block read of the totals corrects it.
    pub fee_mantissa: u64,
    pub ctoken: [u8; 20],
    pub underlying: [u8; 20],
    pub flags: u8,
    /// [`Self::FUSE`], [`Self::FEES_KNOWN`].
    pub flags2: u8,
    pub _pad0: [u8; 2],
    /// Time of the last `AccrueInterest`: `borrow_index`, `total_borrows`
    /// and the reserves are as of then. `0` = none seen.
    pub accrual_ts: u32,
    /// `borrowRatePerBlock()` on the stored state (read each block): the
    /// rate the next `accrueInterest` applies. Valid with [`Self::RATE_KNOWN`].
    pub borrow_rate_per_block: u64,
}

impl CTokenRow {
    pub const LISTED: u8 = 1 << 0;
    pub const CETHER: u8 = 1 << 1;
    pub const PRICED: u8 = 1 << 2;
    pub const EXRATE_KNOWN: u8 = 1 << 3;
    pub const RF_KNOWN: u8 = 1 << 4;
    pub const BORROW_PAUSED: u8 = 1 << 5;
    pub const DEC_KNOWN: u8 = 1 << 6;
    pub const RATE_KNOWN: u8 = 1 << 7;
    /// `flags2`: a Rari Fuse cToken (`CToken.sol` of implementation
    /// `0x67db14e7…`): the exchange rate is `(cash + borrows − (reserves +
    /// fuseFees + adminFees)) / supply`.
    pub const FUSE: u8 = 1 << 0;
    /// `flags2`: the fee totals and rates have been read; until then a Fuse
    /// or Moma cToken's exchange rate is unknown.
    pub const FEES_KNOWN: u8 = 1 << 1;
    /// `flags2`: a Moma Lending Pool mToken (`MToken.sol` of implementation
    /// `0x1d0fcc81…`): the exchange rate is `(cash + borrows − reserves −
    /// totalFees − totalMomaFees) / supply`.
    pub const MOMA: u8 = 1 << 2;
    /// `flags2`: either variant with two fee accumulators beside the
    /// reserves (held summed in [`Self::total_fees`]).
    pub const FEE_ACCUMULATORS: u8 = Self::FUSE | Self::MOMA;
    /// `flags2`: a frozen market: its `accrueInterest()` reverts (the rate
    /// model reverts), so its stored totals never move and every
    /// liquidation that touches it reverts. It counts in the account's
    /// liquidity at its stored values; no leg repays or seizes it.
    pub const FROZEN: u8 = 1 << 3;
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
