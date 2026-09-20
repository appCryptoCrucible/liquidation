//! Store layout of Aave V4 state — the `Pod` views this adapter reads out of
//! the neutral `MarketRow` body, `PositionExtraRepr` and the per-slot extra
//! column (GUIDE 02 §3–4, fixed by this WP).
//!
//! **Markets.** Every Hub is one `MarketId` whose slot `s` is hub `assetId`
//! `s`; the row body is [`HubAsset`] (the eleven accounting words of
//! `IHub.Asset` that `AssetLogic` reads) followed by [`SpokeFlags`]. Every
//! Spoke is one `MarketId` whose slot `0` is the spoke's own configuration
//! ([`SpokeMeta`]) and whose slot `r + 1` is reserve `r` ([`Reserve`]: a copy
//! of its hub asset, fanned out on every hub log, plus the reserve config).
//! Positions live in spoke markets only.
//!
//! **Why a copy and not a pointer.** `health()` reads the hub accounting of
//! every reserve the position holds. Following `hub_market`/`hub_slot` into
//! the hub market at read time would cost a dependent load per slot and put
//! the hub row on the hot path of every spoke; copying on the (rare, per
//! block) hub log keeps the read path to the position's own market.
//!
//! **Line budget** (64-byte lines, header in line 0). A debt-only slot reads
//! the header and cells 0–3 (index, rate, drawn shares, premium shares):
//! two lines. A collateral slot needs `totalAddedAssets`, which is every
//! accounting word: four lines. Nothing in `ReserveCfg` is read by
//! `health()` — the collateral factor a position is charged is the
//! **user's** snapshot in [`UserReserve`], not the reserve's current one
//! (`Spoke._processUserAccountData` reads `_dynamicConfig[reserveId]
//! [userPosition.dynamicConfigKey]`).

use bytemuck::{Pod, Zeroable};
use liq_types::AssetId;

/// Hub-side accounting of one asset (`IHub.Asset` minus addresses and
/// decimals, which live in the row header or are not read). 208 bytes; the
/// leading 13 body cells of both hub rows and spoke reserve rows.
///
/// Order is the read order of `health()`: a debt-only slot stops after
/// `premium_shares`. Signed and 200-bit chain fields are stored as the two
/// `u128` halves of their 256-bit two's-complement / zero-extended form
/// (`lo` first), so the struct stays `Pod` without `U256`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct HubAsset {
    /// `drawnIndex` (`uint120`, RAY) as stored at `last_update`.
    pub drawn_index: u128,
    /// `drawnRate` (`uint96`, RAY per year).
    pub drawn_rate: u128,
    /// `drawnShares` (`uint120`).
    pub drawn_shares: u128,
    /// `premiumShares` (`uint120`).
    pub premium_shares: u128,
    /// `premiumOffsetRay` (`int200`), low half of the `I256` raw bits.
    pub premium_offset_lo: u128,
    /// High half.
    pub premium_offset_hi: u128,
    /// `liquidity` (`uint120`).
    pub liquidity: u128,
    /// `swept` (`uint120`).
    pub swept: u128,
    /// `realizedFees` (`uint120`).
    pub realized_fees: u128,
    /// `addedShares` (`uint120`).
    pub added_shares: u128,
    /// `deficitRay` (`uint200`), low half.
    pub deficit_ray_lo: u128,
    /// High half.
    pub deficit_ray_hi: u128,
    /// `liquidityFee` (`uint16`, bps).
    pub liquidity_fee: u16,
    pub _pad: [u8; 14],
}

/// `IHub.SpokeData.{active, halted}` per tracked spoke, indexed by the
/// spoke's position in `Config::spokes` — the tail of a **hub** row body.
/// Held here because `UpdateSpokeConfig` precedes the spoke's `AddReserve`
/// in every deployment sequence and there is no reserve row to write yet.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct SpokeFlags(pub [u8; SpokeFlags::MAX_SPOKES]);

impl SpokeFlags {
    /// Spokes one adapter instance can track: one byte each in the hub row.
    pub const MAX_SPOKES: usize = 32;
    /// `SpokeData.active`.
    pub const ACTIVE: u8 = 1 << 0;
    /// `SpokeData.halted`.
    pub const HALTED: u8 = 1 << 1;
}

/// Body of a **hub** row.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct HubRow {
    pub asset: HubAsset,
    pub spokes: SpokeFlags,
}

/// Reserve-level configuration of one spoke reserve (`ISpoke.Reserve` and
/// the reserve's **current** `DynamicReserveConfig`). Read by `apply_log`
/// (snapshotting into positions) and `quote()`; never by `health()`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct ReserveCfg {
    /// Current `DynamicReserveConfig.collateralFactor` (bps).
    pub collateral_factor: u16,
    /// Current `DynamicReserveConfig.liquidationFee` (bps).
    pub liquidation_fee: u16,
    /// Current `DynamicReserveConfig.maxLiquidationBonus` (bps, `>= 10_000`).
    pub max_liquidation_bonus: u32,
    /// `Reserve.dynamicConfigKey` — the key the values above belong to.
    pub dyn_key: u32,
    /// `Reserve.collateralRisk` (`uint24`, bps).
    pub collateral_risk: u32,
    /// [`ReserveCfg::PAUSED`] … [`ReserveCfg::SPOKE_HALTED`].
    pub flags: u8,
    pub _pad: [u8; 15],
}

impl ReserveCfg {
    /// `ReserveFlags.paused`.
    pub const PAUSED: u8 = 1 << 0;
    /// `ReserveFlags.frozen`.
    pub const FROZEN: u8 = 1 << 1;
    /// `ReserveFlags.borrowable`.
    pub const BORROWABLE: u8 = 1 << 2;
    /// `ReserveFlags.receiveSharesEnabled`.
    pub const RECEIVE_SHARES: u8 = 1 << 3;
    /// Hub `SpokeData.active` for `(hub asset, this spoke)`.
    pub const SPOKE_ACTIVE: u8 = 1 << 4;
    /// Hub `SpokeData.halted` for `(hub asset, this spoke)`.
    pub const SPOKE_HALTED: u8 = 1 << 5;
}

/// Body of a **spoke reserve** row (slot `reserveId + 1`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct Reserve {
    /// Copy of the hub asset this reserve draws from.
    pub hub: HubAsset,
    pub cfg: ReserveCfg,
}

/// Body of slot `0` of a spoke market: the spoke's `LiquidationConfig` and
/// the per-reserve unpriced mask (`UpdateReserveSource` can arrive before
/// `AddReserve` in the listing transaction, so the mask must not depend on
/// the reserve row existing).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct SpokeMeta {
    /// `LiquidationConfig.targetHealthFactor` (`uint128`, WAD).
    pub target_hf: u128,
    /// `LiquidationConfig.healthFactorForMaxBonus` (`uint64`, WAD).
    pub hf_for_max_bonus: u64,
    /// `LiquidationConfig.liquidationBonusFactor` (`uint16`, bps).
    pub bonus_factor: u16,
    pub _pad: [u8; 6],
    /// Bit `r` set when the last `UpdateReserveSource` for reserve `r`
    /// named exactly the registry's pinned source. A reserve whose bit is
    /// clear — never sourced, or re-pointed elsewhere — is unpriced (fail
    /// closed, 06A-1).
    pub priced: u128,
}

impl SpokeMeta {
    /// Whether reserve `reserve_id` reads a pinned source.
    #[inline]
    #[must_use]
    pub const fn is_priced(&self, reserve_id: u16) -> bool {
        match 1u128.checked_shl(reserve_id as u32) {
            Some(bit) => self.priced & bit != 0,
            None => false,
        }
    }
}

/// Per-user, per-reserve state (`ISpoke.UserPosition` minus the two share
/// columns the store owns, plus the position-status collateral bit and the
/// values behind the user's `dynamicConfigKey`). One `PositionExtraRepr`
/// per slot.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct UserReserve {
    /// `UserPosition.premiumShares` (`uint120`).
    pub premium_shares: u128,
    /// `UserPosition.premiumOffsetRay` (`int200`), low half of the `I256`.
    pub premium_offset_lo: u128,
    /// High half.
    pub premium_offset_hi: u128,
    /// `_dynamicConfig[reserveId][dynamicConfigKey].collateralFactor`.
    pub collateral_factor: u16,
    /// `.liquidationFee`.
    pub liquidation_fee: u16,
    /// `.maxLiquidationBonus`.
    pub max_liquidation_bonus: u32,
    /// `UserPosition.dynamicConfigKey`.
    pub dyn_key: u32,
    /// [`UserReserve::USING_AS_COLLATERAL`].
    pub flags: u8,
    pub _pad: [u8; 3],
}

impl UserReserve {
    /// `PositionStatus` collateral bit for this reserve.
    pub const USING_AS_COLLATERAL: u8 = 1 << 0;
    /// A user-reserve pair never written.
    pub const ZERO: Self = Self {
        premium_shares: 0,
        premium_offset_lo: 0,
        premium_offset_hi: 0,
        collateral_factor: 0,
        liquidation_fee: 0,
        max_liquidation_bonus: 0,
        dyn_key: 0,
        flags: 0,
        _pad: [0; 3],
    };
}

/// Per-user state (`PositionStatus.riskPremium`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct UserExtra {
    /// `riskPremium` (`uint24`, bps).
    pub risk_premium: u32,
    pub _pad: [u8; 12],
}

/// `asset` of a spoke's slot-0 meta row. Not an asset: slot 0 is never a
/// set bit of any position's `config`, so no reader prices it.
pub const META_ASSET: AssetId = AssetId(u16::MAX);

/// `asset` of a row whose underlying the feed registry does not know. Such a
/// row also carries `MarketFlags::UNPRICED`; `health()` refuses it before
/// looking the id up.
pub const UNMAPPED_ASSET: AssetId = AssetId(u16::MAX);

const _: () = {
    assert!(core::mem::size_of::<HubAsset>() == 208);
    assert!(core::mem::size_of::<HubRow>() == 240);
    assert!(core::mem::size_of::<Reserve>() == 240);
    assert!(core::mem::size_of::<ReserveCfg>() == 32);
    assert!(core::mem::size_of::<SpokeMeta>() <= 240);
    assert!(core::mem::size_of::<UserReserve>() == 64);
    assert!(core::mem::size_of::<UserExtra>() == 16);
    assert!(core::mem::align_of::<Reserve>() == 16);
    // A debt-only slot reads the header and the first four cells: the
    // header is 16 bytes, so cells 0..3 end at byte 80 — two lines.
    assert!(core::mem::offset_of!(HubAsset, premium_shares) + 16 + 16 <= 128);
};
