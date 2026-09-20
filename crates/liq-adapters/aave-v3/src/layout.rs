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
    pub emode: [EModeCat; 8],
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
    #[inline]
    pub fn emode(&self, id: u8) -> Option<&EModeCat> {
        if id == 0 {
            return None;
        }
        self.emode.iter().find(|c| c.id == id)
    }
}

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
    pub emode_coll: u8,
    pub emode_borrow: u8,
    pub emode_ltv0: u8,
    pub _pad: [u8; 8],
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
    assert!(core::mem::size_of::<Reserve>() == 160);
    assert!(core::mem::size_of::<UserExtra>() == 16);
    assert!(core::mem::size_of::<UserReserve>() == 16);
    assert!(core::mem::align_of::<Reserve>() == 16);
};
