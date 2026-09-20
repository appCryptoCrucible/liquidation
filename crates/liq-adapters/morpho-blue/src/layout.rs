//! Store layout for Morpho Blue (`morpho-org/morpho-blue` @ `8e26ca6a`).
//!
//! One interned [`liq_types::MarketId`] per Morpho `Id`. Slot 0 is the loan
//! side ([`LoanRow`]); slot 1 is the collateral token header (empty body).
//! A catalog market lists `Id → MarketId` so `CreateMarket` discovers rows
//! instead of a hand list.

use bytemuck::{Pod, Zeroable};
use liq_types::AssetId;

/// Slot 0 of a Morpho market: `Market` totals + `MarketParams` words `health`
/// reads. Indexes are implicit in (assets, shares) — Morpho has no ray index.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct LoanRow {
    pub total_supply_assets: u128,
    pub total_supply_shares: u128,
    pub total_borrow_assets: u128,
    pub total_borrow_shares: u128,
    pub last_borrow_rate: u128,
    pub fee: u128,
    pub lltv: u128,
    pub morpho_id: [u8; 32],
    pub oracle: [u8; 20],
    pub irm: [u8; 20],
    pub coll_asset: u16,
    pub coll_decimals: u8,
    pub flags: u8,
    pub _pad: [u8; 4],
}

impl LoanRow {
    pub const PRICED: u8 = 1 << 0;
}

/// Catalog slot: Morpho `Id` → interned market.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct CatalogEntry {
    pub morpho_id: [u8; 32],
    pub market: u32,
    pub _pad: [u8; 12],
}

pub const CATALOG_ASSET: AssetId = AssetId(u16::MAX);
pub const UNMAPPED_ASSET: AssetId = AssetId(u16::MAX);

pub const LOAN_SLOT: u16 = 0;
pub const COLL_SLOT: u16 = 1;

const _: () = {
    assert!(core::mem::size_of::<LoanRow>() == 192);
    assert!(core::mem::size_of::<CatalogEntry>() == 48);
    assert!(core::mem::align_of::<LoanRow>() == 16);
};
