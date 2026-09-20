//! Store layout for Fluid vaults
//! (`Instadapp/fluid-contracts-public` @ `9496626f71a761fc296dc3b2efbfd54c504e18f0`).
//!
//! Quote unit is **(vault, currently liquidatable debt)** — `liquidate` does
//! not take an NFT id. Catalog market [`CATALOG_MARKET`] maps vault →
//! [`liq_types::MarketId`]. Slot 0 of a vault market is [`VaultRow`] (also
//! copied onto later slots so each reserve header has the same body).

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

pub const UNMAPPED_ASSET: AssetId = AssetId(u16::MAX);
pub const SLOT0: u16 = 0;
pub const CATALOG_ASSET: AssetId = AssetId(u16::MAX);

/// Slot 0 of a vault market: liquidation params + tokens + exchange prices.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct VaultRow {
    pub supply_ex_price: u128,
    pub borrow_ex_price: u128,
    pub vault: [u8; 20],
    pub oracle: [u8; 20],
    pub supply0: [u8; 20],
    pub supply1: [u8; 20],
    pub borrow0: [u8; 20],
    pub borrow1: [u8; 20],
    pub vault_id: u32,
    pub vault_type: u32,
    pub n_nfts: u32,
    /// Packed 3-decimal threshold (`900` = 90%). Event is 1e2; stored `/ 10`.
    pub liq_threshold: u16,
    /// Packed 3-decimal max limit.
    pub liq_max_limit: u16,
    /// Packed 4-decimal penalty (`100` = 1%). Event is 1e2; stored as-is.
    pub liq_penalty: u16,
    pub flags: u8,
    /// Collateral slots occupy `0 .. n_col`.
    pub n_col: u8,
    /// Debt slots occupy `n_col .. n_col + n_debt`.
    pub n_debt: u8,
    pub _pad: [u8; 19],
}

impl VaultRow {
    /// Tokens + type + threshold/penalty written (pin or admin events).
    pub const VIEWED: u8 = 1 << 0;
    /// Primary coll and debt tokens are interned. Missing → UNPRICED.
    pub const PRICED: u8 = 1 << 1;
    /// Exchange prices written (`LogUpdateExchangePrice`).
    pub const EX_KNOWN: u8 = 1 << 2;

    #[inline]
    pub fn debt_slot(self) -> u16 {
        u16::from(self.n_col)
    }

    /// T1 single-token pair only. T2/T3/T4 tick-math is in the vault's
    /// share unit; FluidOracle `getExchangeRateLiquidate` is 1e27
    /// share-per-col, which `PriceVector` token USD cannot express.
    #[inline]
    pub fn is_t1_token_pair(self) -> bool {
        self.vault_type == VAULT_T1 && self.n_col == 1 && self.n_debt == 1
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

/// Vault-level position extra: absorbed liquidity + proven top tick.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct VaultExtra {
    pub absorbed_col_raw: u128,
    pub absorbed_debt_raw: u128,
    pub top_tick: i32,
    /// `1` perfect tick, `2` already liquidated (pin `tickStatus`).
    pub tick_status: u8,
    pub flags: u8,
    pub _pad: [u8; 10],
}

impl VaultExtra {
    /// `top_tick` is proven (single-NFT vault from `LogOperate`).
    pub const TOP_KNOWN: u8 = 1 << 0;
}

const _: () = {
    assert!(core::mem::size_of::<VaultRow>() == 192);
    assert!(core::mem::align_of::<VaultRow>() == 16);
    assert!(core::mem::size_of::<CatalogEntry>() == 32);
    assert!(core::mem::size_of::<VaultExtra>() == 48);
    assert!(core::mem::size_of::<VaultExtra>() <= liq_protocol::PositionExtraRepr::SIZE);
};
