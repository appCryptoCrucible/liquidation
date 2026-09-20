//! Store layout for one Liquity V2 collateral branch (`liquity/bold` @ `c8a5a4ee`).
//!
//! Slot 0 is BOLD debt ([`BranchRow`] in the body). Slot 1 is the branch
//! collateral token (header only). Trove identity and stake live in
//! [`TroveExtra`]; redistribution snapshots and batch denorm in slot extras.

use bytemuck::{Pod, Zeroable};
use liq_types::AssetId;

/// Slot 0 body: branch immutables (toml snapshot of `AddressesRegistry`,
/// copied only after [`crate::LiquityV2::new`] which requires
/// [`crate::Config::assert_live_registry`]) plus `L_coll` / `L_boldDebt` /
/// SP deposits / shutdown.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct BranchRow {
    pub mcr: u128,
    pub ccr: u128,
    pub penalty_sp: u128,
    pub penalty_redist: u128,
    pub l_coll: u128,
    pub l_bold_debt: u128,
    pub sp_bold_deposits: u128,
    pub shutdown_time: u32,
    pub flags: u8,
    pub coll_decimals: u8,
    pub coll_asset: u16,
    pub bold_asset: u16,
    pub weth_asset: u16,
    pub _pad: [u8; 4],
}

impl BranchRow {
    pub const SEEDED: u8 = 1 << 0;
}

/// Per-trove extra. `trove_id` is the full `uint256` NFT id (the intern key
/// only holds the low 160 bits).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct TroveExtra {
    pub trove_id: [u8; 32],
    pub stake: u128,
    pub annual_interest_rate: u64,
    pub last_debt_update: u32,
    pub status: u8,
    pub _pad: [u8; 3],
}

/// `ITroveManager.Status` @ `c8a5a4ee` (no `unredeemable`).
impl TroveExtra {
    pub const STATUS_NONEXISTENT: u8 = 0;
    pub const STATUS_ACTIVE: u8 = 1;
    pub const STATUS_CLOSED_BY_OWNER: u8 = 2;
    pub const STATUS_CLOSED_BY_LIQUIDATION: u8 = 3;
    pub const STATUS_ZOMBIE: u8 = 4;

    #[inline]
    #[must_use]
    pub const fn is_active_or_zombie(self) -> bool {
        self.status == Self::STATUS_ACTIVE || self.status == Self::STATUS_ZOMBIE
    }
}

/// Slot 0 extra: BOLD redistribution snapshot + batch denorm.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct TroveDebtExtra {
    pub snapshot_bold: u128,
    pub batch_debt_shares: u128,
    pub batch_recorded_debt: u128,
    pub batch_total_shares: u128,
}

/// Slot 1 extra: coll redistribution snapshot + batch manager / fee.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct TroveCollExtra {
    pub snapshot_coll: u128,
    pub batch_manager: [u8; 20],
    pub _pad0: [u8; 4],
    pub batch_management_fee: u64,
    pub _pad1: [u8; 16],
}

pub const BOLD_SLOT: u16 = 0;
pub const COLL_SLOT: u16 = 1;
pub const UNMAPPED_ASSET: AssetId = AssetId(u16::MAX);

const _: () = {
    assert!(core::mem::size_of::<BranchRow>() == 128);
    assert!(core::mem::align_of::<BranchRow>() == 16);
    assert!(core::mem::size_of::<TroveExtra>() == 64);
    assert!(core::mem::size_of::<TroveDebtExtra>() == 64);
    assert!(core::mem::size_of::<TroveCollExtra>() == 64);
    assert!(core::mem::size_of::<TroveExtra>() <= liq_protocol::PositionExtraRepr::SIZE);
};
