//! Store layout for Silo V2 isolated pairs
//! (`silo-finance/silo-contracts-v2` @ `570a668a98a88a6a2b92697e7b9a3b1c6299dce7`).
//!
//! One interned [`liq_types::MarketId`] per `SiloConfig`. Slot 0 is `getSilos().0`,
//! slot 1 is `getSilos().1`. Liquidation is **not** on these ERC-4626 silos —
//! it is on the pair's hook receiver (`IPartialLiquidation`).

use bytemuck::{Pod, Zeroable};

/// Per-silo row body: storage totals + immutable solvency params from
/// `ISiloConfig.getConfig`. 208 bytes (13 × 16).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct SiloRow {
    pub total_collateral_assets: u128,
    pub total_protected_assets: u128,
    pub total_debt_assets: u128,
    pub total_collateral_shares: u128,
    pub total_protected_shares: u128,
    pub total_debt_shares: u128,
    pub lt: u128,
    pub liquidation_fee: u128,
    pub liquidation_target_ltv: u128,
    pub hook: [u8; 20],
    pub silo: [u8; 20],
    pub config: [u8; 20],
    pub flags: u8,
    pub _pad: [u8; 3],
}

impl SiloRow {
    /// `getConfig` params were written (lt / fee / hook). Health fails closed without it.
    pub const VIEWED: u8 = 1 << 0;
    /// Solvency oracle is the address the config pins.
    pub const PRICED: u8 = 1 << 1;
}

/// Per-position extra: protected shares on each silo + which silo backs debt
/// (`ISiloConfig.borrowerCollateralSilo`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct UserExtra {
    pub protected_0: u128,
    pub protected_1: u128,
    /// Slot of the collateral silo; [`UserExtra::UNSET`] until `Borrow` /
    /// `CollateralTypeChanged` / debt `Transfer`.
    pub collateral_slot: u8,
    pub _pad: [u8; 15],
}

impl UserExtra {
    pub const UNSET: u8 = 0xff;

    #[inline]
    pub const fn protected(self, slot: u16) -> u128 {
        match slot {
            0 => self.protected_0,
            1 => self.protected_1,
            _ => 0,
        }
    }

    #[inline]
    pub fn set_protected(&mut self, slot: u16, shares: u128) {
        match slot {
            0 => self.protected_0 = shares,
            1 => self.protected_1 = shares,
            _ => {}
        }
    }
}

pub const SLOT0: u16 = 0;
pub const SLOT1: u16 = 1;
pub const PAIR_SLOTS: u16 = 2;

const _: () = {
    assert!(core::mem::size_of::<SiloRow>() == 208);
    assert!(core::mem::align_of::<SiloRow>() == 16);
    assert!(core::mem::size_of::<UserExtra>() == 48);
    assert!(core::mem::size_of::<UserExtra>() <= 64);
};
