//! Store layout for Euler V2 EVK (`euler-xyz/euler-vault-kit` @ `bfb325a6`).
//!
//! One interned [`liq_types::MarketId`] per debt EVault. Slot 0 is the
//! liability vault ([`VaultRow`]); subsequent slots are recognized collateral
//! vaults ([`CollRow`]). A catalog market lists `vault → MarketId` so
//! `ProxyCreated` discovers rows instead of a hand list of the live universe.

use bytemuck::{Pod, Zeroable};
use liq_types::AssetId;

/// Slot 0 of a debt vault: accumulator + governor liquidation params `health`
/// reads. `max_liquidation_discount` is per-vault; never a protocol cap.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct VaultRow {
    pub interest_accumulator_lo: u128,
    pub interest_accumulator_hi: u128,
    pub interest_rate: u128,
    pub vault: [u8; 20],
    pub underlying: [u8; 20],
    pub oracle: [u8; 20],
    pub unit_of_account: [u8; 20],
    pub max_liquidation_discount: u16,
    pub liquidation_cool_off: u16,
    pub interest_fee: u16,
    pub flags: u8,
    pub _pad0: u8,
    pub hooked_ops: u32,
    pub config_flags: u32,
    pub _pad1: [u8; 16],
}

impl VaultRow {
    pub const PRICED: u8 = 1 << 0;
    pub const ACC_KNOWN: u8 = 1 << 1;
    pub const DISCOUNT_KNOWN: u8 = 1 << 2;
    pub const COOL_OFF_KNOWN: u8 = 1 << 3;
    pub const HOOKS_KNOWN: u8 = 1 << 4;
}

/// Collateral vault recognized on a debt vault (`GovSetLTV`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct CollRow {
    pub vault: [u8; 20],
    pub borrow_ltv: u16,
    pub liquidation_ltv: u16,
    pub initial_liquidation_ltv: u16,
    pub share_decimals: u8,
    pub flags: u8,
    pub ramp_duration: u32,
    pub target_timestamp: u64,
    pub share_asset: u16,
    pub _pad: [u8; 22],
}

impl CollRow {
    pub const RECOGNIZED: u8 = 1 << 0;
}

/// Catalog slot: EVault proxy → interned market.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct CatalogEntry {
    pub vault: [u8; 20],
    pub market: u32,
    pub _pad: [u8; 8],
}

/// Per-position EVK state: user accumulator snapshot, EVC enable mask, cool-off.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct UserExtra {
    pub user_accumulator_lo: u128,
    pub user_accumulator_hi: u128,
    pub enabled_mask: u128,
    pub last_status_check: u32,
    pub flags: u8,
    pub _pad: [u8; 11],
}

impl UserExtra {
    pub const ACC_KNOWN: u8 = 1 << 0;
}

pub const CATALOG_ASSET: AssetId = AssetId(u16::MAX);
pub const UNMAPPED_ASSET: AssetId = AssetId(u16::MAX);
pub const DEBT_SLOT: u16 = 0;

/// `OP_LIQUIDATE = 1 << 11` (`Constants.sol`).
pub const OP_LIQUIDATE: u32 = 1 << 11;
/// `OP_MAX_VALUE - 1`: all ops hooked (disabled while hook target is zero).
pub const INITIAL_HOOKED_OPS: u32 = (1 << 15) - 1;
/// `CFG_DONT_SOCIALIZE_DEBT`.
pub const CFG_DONT_SOCIALIZE_DEBT: u32 = 1 << 0;

const _: () = {
    assert!(core::mem::size_of::<VaultRow>() == 160);
    assert!(core::mem::align_of::<VaultRow>() == 16);
    assert!(core::mem::size_of::<CollRow>() == 64);
    assert!(core::mem::size_of::<CatalogEntry>() == 32);
    assert!(core::mem::size_of::<UserExtra>() == 64);
    assert!(core::mem::size_of::<UserExtra>() <= liq_protocol::PositionExtraRepr::SIZE);
};
