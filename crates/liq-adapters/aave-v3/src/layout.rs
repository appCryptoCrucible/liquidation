//! Store layout for one Aave V3 pool (`aave-dao/aave-v3-origin` @ `8305565ae`).
//! Slot 0 is [`PoolMeta`]; slots `1..=EMODE_ROWS` are [`EModeRow`]s; each
//! reserve then takes the next slot as it is initialized, from
//! [`FIRST_RESERVE`].

use bytemuck::{Pod, Zeroable};
use liq_types::AssetId;

/// Slot-0 body: pool-wide flash premium and L2 sentinel snapshot.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct PoolMeta {
    pub flashloan_premium_total: u16,
    pub sentinel_present: u8,
    pub sequencer_answer: i8,
    pub sentinel_grace: u32,
    pub sequencer_updated_at: u32,
    /// A halt-class log came from this pool, its configurator, oracle,
    /// provider or one of its tokens after the pin: the market's view is
    /// not trusted, so nothing in it is liquidated until the config is
    /// re-pinned. Other markets keep running.
    pub halted: u8,
    /// Aave V2 `LendingPool.Paused` (no V2 reserve pauses on its own).
    pub pool_paused: u8,
    pub _pad0: [u8; 2],
}

/// One e-mode category. `id == 0` is an entry not configured (category 0
/// is "off").
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct EModeCat {
    pub id: u8,
    pub isolated: u8,
    pub ltv: u16,
    pub liq_threshold: u16,
    pub liq_bonus: u16,
}

/// Body of slots `1..=EMODE_ROWS`: the pool's e-mode categories, by id.
///
/// A6. The table was 28 entries inside [`PoolMeta`], all one row body
/// holds, and the 29th `EModeCategoryAdded` failed the fold. Aave V3 Core
/// had 48 categories by block 26,112,136, so the adapter could not follow
/// it at all. A category id is a `uint8` and Aave never deletes a
/// category, so the table now has a place for every id: category `id` is
/// entry `(id - 1) % PER_ROW` of slot `1 + (id - 1) / PER_ROW`
/// ([`emode_place`]). Nothing here can fill up.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct EModeRow {
    pub cats: [EModeCat; EModeRow::PER_ROW],
}

impl EModeRow {
    /// Categories one row body holds: `240 / 8`.
    pub const PER_ROW: usize = 30;
}

/// Rows holding categories `1..=255`: `ceil(255 / PER_ROW)`.
pub const EMODE_ROWS: u16 = 9;

/// Slot of the first reserve. A position's slot mask is 128 bits wide
/// ([`liq_protocol::AssetMask::MAX_SLOTS`]), so a pool can hold
/// `128 - FIRST_RESERVE` reserves; Core had 67 at block 26,112,136.
pub const FIRST_RESERVE: u16 = EMODE_ROWS + 1;

/// Where category `id` is held: `(slot, entry)`. `None` for 0 (e-mode off).
#[inline]
#[must_use]
pub fn emode_place(id: u8) -> Option<(u16, usize)> {
    let k = usize::from(id).checked_sub(1)?;
    let row = u16::try_from(k.checked_div(EModeRow::PER_ROW)?).ok()?;
    Some((row.checked_add(1)?, k.checked_rem(EModeRow::PER_ROW)?))
}

/// A set of e-mode category ids, one bit per id, as wide as Aave's
/// `uint8` id.
///
/// Aave keeps the transpose, a `uint128` bitmap of reserve ids on each
/// category. Holding the set on each reserve instead is equivalent, and
/// lets the health walk read it from the row it already has.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(transparent)]
pub struct EModeSet(pub [u64; 4]);

impl EModeSet {
    #[inline]
    #[must_use]
    pub fn contains(&self, id: u8) -> bool {
        let (word, bit) = (usize::from(id >> 6), u32::from(id & 63));
        self.0
            .get(word)
            .and_then(|w| w.checked_shr(bit))
            .is_some_and(|w| w & 1 != 0)
    }

    #[inline]
    pub fn set(&mut self, id: u8, on: bool) {
        let (word, bit) = (usize::from(id >> 6), u32::from(id & 63));
        let (Some(w), Some(mask)) = (self.0.get_mut(word), 1u64.checked_shl(bit)) else {
            return;
        };
        if on {
            *w |= mask;
        } else {
            *w &= !mask;
        }
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
    /// E-mode categories this reserve is collateral in.
    pub emode_coll: EModeSet,
    /// E-mode categories in which this reserve counts at LTV 0.
    pub emode_ltv0: EModeSet,
    pub a_token: [u8; 20],
    pub v_token: [u8; 20],
    /// `StableDebtToken` from `ReserveInitialized` (zero on 3.2+ pools).
    pub s_token: [u8; 20],
    pub grace_until: u32,
    pub ltv: u16,
    pub liq_threshold: u16,
    pub liq_bonus: u16,
    pub liq_protocol_fee: u16,
    pub flags: u8,
    /// `reserve_id` holds the pool's id for this reserve (read by
    /// `crate::resync`, not derived from the slot).
    pub id_known: u8,
    /// `ReserveData.id`: this reserve's bit pair in a user's configuration
    /// bitmap. Valid only when `id_known != 0`.
    pub reserve_id: u16,
    pub _pad: [u8; 4],
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
    pub _pad: [u8; 7],
    /// Reserve slots on which the account holds stable debt, which this
    /// adapter does not model (bit 63 also stands for every slot >= 63).
    /// Nonzero: the account is not quoted.
    pub stable_slots: u64,
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
    assert!(core::mem::size_of::<PoolMeta>() == 16);
    assert!(core::mem::size_of::<EModeRow>() == 240);
    assert!(core::mem::size_of::<EModeSet>() == 32);
    assert!(core::mem::size_of::<Reserve>() == 240);
    assert!(core::mem::size_of::<UserExtra>() == 16);
    assert!(core::mem::size_of::<UserReserve>() == 16);
    assert!(core::mem::align_of::<Reserve>() == 16);
};

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod layout_size {
    use super::{emode_place, EModeRow, EModeSet, PoolMeta, Reserve, EMODE_ROWS, FIRST_RESERVE};

    /// `MarketRow::body` is 240 bytes. Every body must fit, or `body()`
    /// returns `BodyLayout` at runtime for every row in the protocol.
    #[test]
    fn fits_in_a_market_row_body() {
        const BUDGET: usize = 240;
        assert!(core::mem::size_of::<PoolMeta>() <= BUDGET);
        assert!(core::mem::size_of::<EModeRow>() <= BUDGET);
        assert!(core::mem::size_of::<Reserve>() <= BUDGET);
    }

    /// Every category id Aave can configure has its own entry in the rows
    /// before the first reserve, and no two ids share one.
    #[test]
    fn every_category_id_has_its_own_place() {
        assert_eq!(emode_place(0), None, "category 0 is e-mode off");
        let mut seen = std::collections::BTreeSet::new();
        for id in 1..=u8::MAX {
            let (slot, i) = emode_place(id).unwrap();
            assert!(
                (1..=EMODE_ROWS).contains(&slot),
                "category {id} at slot {slot}"
            );
            assert!(slot < FIRST_RESERVE);
            assert!(i < EModeRow::PER_ROW);
            assert!(seen.insert((slot, i)), "category {id} shares a place");
        }
        assert_eq!(emode_place(1), Some((1, 0)));
        assert_eq!(emode_place(30), Some((1, 29)));
        assert_eq!(emode_place(31), Some((2, 0)));
        assert_eq!(emode_place(u8::MAX), Some((EMODE_ROWS, 14)));
    }

    /// The per-reserve sets hold every id, each on its own bit.
    #[test]
    fn emode_set_holds_every_id() {
        let mut s = EModeSet::default();
        for id in [0u8, 1, 28, 29, 48, 63, 64, 127, 128, 200, 255] {
            assert!(!s.contains(id));
            s.set(id, true);
            assert!(s.contains(id), "id {id}");
        }
        s.set(29, false);
        assert!(!s.contains(29));
        assert!(s.contains(28) && s.contains(48));
        let ones: u32 = s.0.iter().map(|w| w.count_ones()).sum();
        assert_eq!(ones, 10);
    }
}
