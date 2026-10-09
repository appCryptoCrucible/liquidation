//! Store layout for Silo V2 isolated pairs
//! (`silo-finance/silo-contracts-v2` @ `570a668a98a88a6a2b92697e7b9a3b1c6299dce7`).
//!
//! One interned [`liq_types::MarketId`] per `SiloConfig`. Slot 0 is `getSilos().0`,
//! slot 1 is `getSilos().1`. Liquidation is **not** on these ERC-4626 silos —
//! it is on the pair's hook receiver (`IPartialLiquidation`).

use bytemuck::{Pod, Zeroable};

/// Per-silo row body: storage totals + immutable solvency params from
/// `ISiloConfig.getConfig`, and the interest growth the state reads
/// measure. 240 bytes (15 × 16), the body's whole width.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct SiloRow {
    pub total_collateral_assets: u128,
    pub total_protected_assets: u128,
    pub total_debt_assets: u128,
    pub total_collateral_shares: u128,
    pub total_protected_shares: u128,
    pub total_debt_shares: u128,
    /// Growth of `total_debt_assets` per second from accrued interest,
    /// relative to it, in RAY (`1e27` = the whole total each second), as of
    /// `MarketRow::last_update`: measured by the state reads from
    /// `base_debt` (see `apply_totals`). Valid with
    /// [`SiloRow::GROWTH_KNOWN`]. Collateral grows by the same interest net
    /// of `interest_fee`, as Silo accrues it.
    pub debt_rate_ray: u128,
    /// The with-interest debt the next rate is measured from, at `base_at`.
    pub base_debt: u128,
    /// WAD (`<= 1e18`), from `getConfig`.
    pub lt: u64,
    pub liquidation_fee: u64,
    pub liquidation_target_ltv: u64,
    /// `daoFee + deployerFee` (WAD): the share of accrued interest that is
    /// not the depositors'.
    pub interest_fee: u64,
    /// `utilizationData().interestRateTimestamp` at the last read: an
    /// unchanged one at the next read means only interest moved the totals.
    pub accrued_at: u64,
    pub base_at: u32,
    pub hook: [u8; 20],
    pub silo: [u8; 20],
    pub config: [u8; 20],
    pub flags: u8,
    pub _pad: [u8; 7],
}

impl SiloRow {
    /// `getConfig` params were written (lt / fee / hook). Health fails closed without it.
    pub const VIEWED: u8 = 1 << 0;
    /// Solvency oracle is the address the config pins.
    pub const PRICED: u8 = 1 << 1;
    /// A halt-class log came from this pair after the pin: the pair refuses
    /// liquidation (`MarketFlags::PAUSED`) until the config is re-pinned.
    pub const HALTED: u8 = 1 << 2;
    /// `debt_rate_ray` comes from a state read: totals project from
    /// `MarketRow::last_update` at it.
    pub const GROWTH_KNOWN: u8 = 1 << 3;
    /// A state read wrote the totals, `accrued_at` and the base: the next
    /// read can measure the growth since.
    pub const READ_BASE: u8 = 1 << 4;
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
    assert!(core::mem::size_of::<SiloRow>() == 240);
    assert!(core::mem::align_of::<SiloRow>() == 16);
    assert!(core::mem::size_of::<UserExtra>() == 48);
    assert!(core::mem::size_of::<UserExtra>() <= 64);
};
