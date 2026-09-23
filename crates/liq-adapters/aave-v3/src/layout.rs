//! Store layout for one Aave V3 pool (`aave-dao/aave-v3-origin` @ `8305565ae`).
//! Slot 0 is [`PoolMeta`]; slot `reserveId + 1` is [`Reserve`].

use bytemuck::{Pod, Zeroable};
use liq_types::AssetId;

/// Slot-0 body: pool-wide flash premium, L2 sentinel snapshot, e-mode table.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct PoolMeta {
    pub flashloan_premium_total: u16,
    pub sentinel_present: u8,
    pub sequencer_answer: i8,
    pub sentinel_grace: u32,
    pub sequencer_updated_at: u32,
    pub _pad0: [u8; 4],
    pub emode: [EModeCat; PoolMeta::EMODE_CAP],
}

/// One e-mode category. `id == 0` is an empty table slot (category 0 is "off").
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct EModeCat {
    pub id: u8,
    pub isolated: u8,
    pub ltv: u16,
    pub liq_threshold: u16,
    pub liq_bonus: u16,
}

impl PoolMeta {
    /// E-mode categories this table holds.
    ///
    /// A6. This was 8, and the ninth `EModeCategoryAdded` returned
    /// `Err(Internal)` from `apply_log` — a hard fold failure, not a skip.
    /// Live Aave V3 deployments carry well over 8, so the adapter stopped
    /// folding on any pool that had grown past the table.
    ///
    /// 28 is the largest value that fits: `MarketRow`'s body is 240 bytes,
    /// this struct's header is 16, and [`EModeCat`] is 8 — `(240 - 16) / 8`.
    /// `layout_size::fits_in_a_market_row_body` pins that, so raising this
    /// past the budget fails the build rather than the fold.
    ///
    /// The category id itself is a `uint8`, so a pool could in principle
    /// configure more than 28. That is why a full table reports a named,
    /// diagnosable error instead of `Internal` — a capacity problem to be
    /// seen and raised, not an unexplained halt.
    pub const EMODE_CAP: usize = 28;

    #[inline]
    pub fn emode(&self, id: u8) -> Option<&EModeCat> {
        if id == 0 {
            return None;
        }
        self.emode.iter().find(|c| c.id == id)
    }

    /// Table position of `id`, which is also its bit in the per-reserve
    /// [`Reserve::emode_coll`] / `emode_borrow` / `emode_ltv0` bitmaps.
    ///
    /// Aave stores the inverse — a `uint128` bitmap of reserve ids on each
    /// category. This adapter transposes it to a bitmap of categories on each
    /// reserve, which is equivalent so long as the bitmap is at least
    /// `EMODE_CAP` bits wide. [`EModeBits`] is `u32` for that reason.
    #[inline]
    pub fn emode_index(&self, id: u8) -> Option<usize> {
        self.emode.iter().position(|c| c.id == id)
    }

    /// Position to write `id` into: its existing row, else the first empty
    /// one. `None` when the table is full and `id` is not already in it.
    #[inline]
    pub fn emode_slot_for(&self, id: u8) -> Option<usize> {
        self.emode
            .iter()
            .position(|c| c.id == id)
            .or_else(|| self.emode.iter().position(|c| c.id == 0))
    }

    /// The bit for a table position, as a mask over the reserve bitmaps.
    #[inline]
    pub fn emode_mask(i: usize) -> Option<EModeBits> {
        u32::try_from(i).ok().and_then(|sh| 1_u32.checked_shl(sh))
    }
}

/// Width of the per-reserve e-mode bitmaps. Must be at least
/// [`PoolMeta::EMODE_CAP`] bits; `layout_size` asserts it.
pub type EModeBits = u32;

/// Per-reserve body. Indexes/rates first (health hot path).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct Reserve {
    pub liquidity_index: u128,
    pub variable_borrow_index: u128,
    pub liquidity_rate: u128,
    pub variable_borrow_rate: u128,
    pub deficit: u128,
    pub debt_ceiling: u128,
    pub a_token: [u8; 20],
    pub v_token: [u8; 20],
    pub grace_until: u32,
    pub ltv: u16,
    pub liq_threshold: u16,
    pub liq_bonus: u16,
    pub liq_protocol_fee: u16,
    pub flags: u8,
    pub _pad1: [u8; 3],
    /// Bitmaps over e-mode TABLE POSITIONS (see [`PoolMeta::emode_index`]),
    /// not over category ids and not over reserve ids. `u32` so all
    /// [`PoolMeta::EMODE_CAP`] positions are addressable — as `u8` they
    /// silently could not reach past the eighth category.
    pub emode_coll: EModeBits,
    pub emode_borrow: EModeBits,
    pub emode_ltv0: EModeBits,
    pub _pad: [u8; 12],
}

impl Reserve {
    pub const ACTIVE: u8 = 1 << 0;
    pub const FROZEN: u8 = 1 << 1;
    pub const PAUSED: u8 = 1 << 2;
    pub const BORROWING: u8 = 1 << 3;
    pub const FLASH: u8 = 1 << 4;
    pub const SILOED: u8 = 1 << 5;
    pub const ISOLATED: u8 = 1 << 6;
    pub const PRICED: u8 = 1 << 7;
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct UserExtra {
    pub emode: u8,
    pub _pad: [u8; 15],
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct UserReserve {
    pub flags: u8,
    pub _pad: [u8; 15],
}

impl UserReserve {
    pub const USING_AS_COLLATERAL: u8 = 1 << 0;
    pub const ZERO: Self = Self {
        flags: 0,
        _pad: [0; 15],
    };
}

pub const META_ASSET: AssetId = AssetId(u16::MAX);
pub const UNMAPPED_ASSET: AssetId = AssetId(u16::MAX);

const _: () = {
    assert!(core::mem::size_of::<EModeCat>() == 8);
    assert!(core::mem::size_of::<PoolMeta>() <= 240);
    assert!(core::mem::size_of::<Reserve>() == 176);
    assert!(core::mem::size_of::<UserExtra>() == 16);
    assert!(core::mem::size_of::<UserReserve>() == 16);
    assert!(core::mem::align_of::<Reserve>() == 16);
};

#[cfg(test)]
mod layout_size {
    use super::{EModeBits, EModeCat, PoolMeta, Reserve};

    /// `MarketRow::body` is 240 bytes. Both bodies must fit, or `body()`
    /// returns `BodyLayout` at runtime for every row in the protocol.
    #[test]
    fn fits_in_a_market_row_body() {
        const BUDGET: usize = 240;
        assert!(
            core::mem::size_of::<PoolMeta>() <= BUDGET,
            "PoolMeta is {} bytes, over the {BUDGET}-byte body budget - lower EMODE_CAP",
            core::mem::size_of::<PoolMeta>()
        );
        assert!(core::mem::size_of::<Reserve>() <= BUDGET);
    }

    /// The bitmaps index table positions, so they must address every one.
    #[test]
    fn emode_bitmap_covers_the_whole_table() {
        assert!(
            PoolMeta::EMODE_CAP <= core::mem::size_of::<EModeBits>() * 8,
            "EModeBits is too narrow for EMODE_CAP categories"
        );
        assert!(PoolMeta::emode_mask(PoolMeta::EMODE_CAP - 1).is_some());
        assert_eq!(
            PoolMeta::emode_mask(core::mem::size_of::<EModeBits>() * 8),
            None,
            "a shift past the width must report None, never wrap to bit 0"
        );
    }

    /// The cap is the largest that fits, so the next category up would not.
    #[test]
    fn cap_is_the_largest_that_fits() {
        let header = core::mem::size_of::<PoolMeta>()
            - core::mem::size_of::<EModeCat>() * PoolMeta::EMODE_CAP;
        assert!(header + core::mem::size_of::<EModeCat>() * (PoolMeta::EMODE_CAP + 1) > 240);
    }
}
