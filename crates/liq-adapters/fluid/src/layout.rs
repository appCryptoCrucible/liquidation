//! Store layout for Fluid vaults
//! (`Instadapp/fluid-contracts-public` @ `9496626f71a761fc296dc3b2efbfd54c504e18f0`).
//!
//! Fluid liquidates a vault's whole underwater tick range at once:
//! `liquidate` takes no position id. The adapter keeps **one position per
//! vault** (`PositionKey.user` = the vault) holding what the vault would
//! liquidate right now, as the vault itself reports it each block.
//!
//! Market `4000 + vaultId`: collateral token rows first (`supply0`, then
//! `supply1` on smart-collateral vaults), then debt token rows (`borrow0`,
//! then `borrow1` on smart-debt vaults). Slot 0's body is [`VaultRow`].
//! The position's supply cell on a collateral row is the collateral a full
//! liquidation pays **in that one token**; its debt cell on a debt row is
//! what the liquidation costs **in that one token**. On a smart side the
//! two tokens are alternatives, not a sum.

use alloy_primitives::{address, Address};
use bytemuck::{Pod, Zeroable};
use liq_types::{AssetId, MarketId};

/// Factory/catalog index. Not a vault. Allocator: Fluid 4000..=4199.
pub const CATALOG_MARKET: MarketId = MarketId(4000);
/// First vault MarketId: `4000 + vaultId` for `vaultId >= 1` ⇒ 4001.
pub const FIRST_VAULT_MARKET: MarketId = MarketId(4001);
/// Inclusive max Fluid MarketId (Gearbox owns 4200..=4299).
pub const LAST_VAULT_MARKET: MarketId = MarketId(4199);

/// `FluidProtocolTypes.VAULT_T1_TYPE`.
pub const VAULT_T1: u32 = 10_000;
/// `VAULT_T2_SMART_COL_TYPE`.
pub const VAULT_T2: u32 = 20_000;
/// `VAULT_T3_SMART_DEBT_TYPE`.
pub const VAULT_T3: u32 = 30_000;
/// `VAULT_T4_SMART_COL_SMART_DEBT_TYPE`.
pub const VAULT_T4: u32 = 40_000;

/// Fluid's native-ETH placeholder. The Executor pays and receives it as
/// WETH, so it is priced and routed as WETH.
pub const NATIVE_TOKEN: Address = address!("0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");

pub const UNMAPPED_ASSET: AssetId = AssetId(u16::MAX);
pub const SLOT0: u16 = 0;
pub const CATALOG_ASSET: AssetId = AssetId(u16::MAX);

/// Slot 0 body of a vault market.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct VaultRow {
    pub vault: [u8; 20],
    /// Collateral side: the DEX on T2/T4, else Liquidity.
    pub supply: [u8; 20],
    /// Debt side: the DEX on T3/T4, else Liquidity.
    pub borrow: [u8; 20],
    pub supply0: [u8; 20],
    pub supply1: [u8; 20],
    pub borrow0: [u8; 20],
    pub borrow1: [u8; 20],
    pub vault_id: u32,
    pub vault_type: u32,
    /// The vault's position id + 1 (0 = never interned).
    pub position_plus1: u32,
    /// Collateral slots occupy `0 .. n_col`.
    pub n_col: u8,
    /// Debt slots occupy `n_col .. n_col + n_debt`.
    pub n_debt: u8,
    pub _pad: [u8; 2],
}

impl VaultRow {
    #[inline]
    #[must_use]
    pub const fn smart_col(&self) -> bool {
        self.vault_type == VAULT_T2 || self.vault_type == VAULT_T4
    }

    #[inline]
    #[must_use]
    pub const fn smart_debt(&self) -> bool {
        self.vault_type == VAULT_T3 || self.vault_type == VAULT_T4
    }

    /// Slot of debt token `i` (0 or 1).
    #[inline]
    #[must_use]
    pub fn debt_slot(&self, i: u8) -> u16 {
        u16::from(self.n_col).saturating_add(u16::from(i))
    }
}

/// Catalog slot: vault address → MarketId.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct CatalogEntry {
    pub vault: [u8; 20],
    pub vault_id: u32,
    pub market: u32,
    pub _pad: [u8; 4],
}

/// The vault position's extra: the liquidation the cells describe, in the
/// vault's own units (what `FluidLiquidateResult` reports: tokens, or DEX
/// shares on a smart side), and the block time it was read at.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct VaultExtra {
    /// Debt units the full liquidation repays.
    pub debt_units: u128,
    /// Collateral units it pays out.
    pub col_units: u128,
    /// Timestamp of the block the vault was asked at (0 = never).
    pub read_ts: u64,
    pub flags: u8,
    pub _pad: [u8; 7],
}

impl VaultExtra {
    /// The amounts are the `absorb_ = true` liquidation.
    pub const ABSORB: u8 = 1 << 0;
}

const _: () = {
    assert!(core::mem::size_of::<VaultRow>() == 156);
    assert!(core::mem::size_of::<VaultRow>() <= 240);
    assert!(core::mem::size_of::<CatalogEntry>() == 32);
    assert!(core::mem::size_of::<VaultExtra>() == 48);
    assert!(core::mem::size_of::<VaultExtra>() <= liq_protocol::PositionExtraRepr::SIZE);
};
