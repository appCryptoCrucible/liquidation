//! Per-position slot bitmask (GUIDE 02 §2).
//!
//! A **slot** indexes the reserve list of the position's own market (an Aave V4
//! spoke, an Aave V3 pool, a Morpho Blue market): `PositionRef::supply[slot]`,
//! `PositionRef::debt[slot]`, `PositionRef::markets[slot]` and the bits here
//! all share it. The store keeps a bit set exactly when the slot holds a
//! nonzero supply or debt, so `health()` iterates set bits and skips every
//! other reserve. Protocol-specific per-slot state (Aave's "use as collateral"
//! toggle) lives in [`crate::PositionExtraRepr`], not here.

use bytemuck::{Pod, Zeroable};

/// `u128` bitmask over slots `0..128` of one market.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub struct AssetMask(pub u128);

impl AssetMask {
    /// No slot set.
    pub const EMPTY: Self = Self(0);
    /// One bit per slot; a market with more reserves cannot be stored.
    pub const MAX_SLOTS: u16 = 128;

    /// Whether `slot` is set. Slots `>= MAX_SLOTS` are never set.
    #[inline]
    #[must_use]
    pub const fn contains(self, slot: u16) -> bool {
        match 1u128.checked_shl(slot as u32) {
            Some(bit) => self.0 & bit != 0,
            None => false,
        }
    }

    /// `self` with `slot` set; `None` when `slot >= MAX_SLOTS`.
    #[inline]
    #[must_use]
    pub const fn with(self, slot: u16) -> Option<Self> {
        match 1u128.checked_shl(slot as u32) {
            Some(bit) => Some(Self(self.0 | bit)),
            None => None,
        }
    }

    /// `self` with `slot` cleared. Out-of-range slots are a no-op.
    #[inline]
    #[must_use]
    pub const fn without(self, slot: u16) -> Self {
        match 1u128.checked_shl(slot as u32) {
            Some(bit) => Self(self.0 & !bit),
            None => self,
        }
    }

    /// Number of set slots.
    #[inline]
    #[must_use]
    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }

    /// `true` when no slot is set.
    #[inline]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Set slots in ascending order. One `trailing_zeros` per set bit; no
    /// allocation, no branch on unset slots.
    #[inline]
    #[must_use]
    pub const fn iter(self) -> SetSlots {
        SetSlots(self.0)
    }
}

/// Iterator over the set slots of an [`AssetMask`], ascending.
#[derive(Copy, Clone, Debug)]
pub struct SetSlots(u128);

impl Iterator for SetSlots {
    type Item = u16;

    #[inline]
    fn next(&mut self) -> Option<u16> {
        if self.0 == 0 {
            return None;
        }
        let tz = self.0.trailing_zeros();
        // Clear the lowest set bit. `tz < 128` here because `self.0 != 0`.
        self.0 &= self.0.wrapping_sub(1);
        u16::try_from(tz).ok()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.0.count_ones() as usize;
        (n, Some(n))
    }
}

impl ExactSizeIterator for SetSlots {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::AssetMask;

    /// Oracle: definition of a bitmask — set bits come back ascending and
    /// nothing else does. Negative: slot 128 cannot be set.
    #[test]
    fn set_bits_round_trip_and_bound() {
        let m = AssetMask::EMPTY
            .with(0)
            .unwrap()
            .with(127)
            .unwrap()
            .with(5)
            .unwrap();
        assert_eq!(m.iter().collect::<Vec<_>>(), vec![0, 5, 127]);
        assert_eq!(m.len(), 3);
        assert!(m.contains(5));
        assert!(!m.contains(6));
        assert!(!m.contains(128));
        assert_eq!(m.with(128), None, "oracle: MAX_SLOTS = 128");
        assert_eq!(m.without(5).iter().collect::<Vec<_>>(), vec![0, 127]);
        assert!(AssetMask::EMPTY.is_empty());
        assert_eq!(AssetMask::EMPTY.iter().next(), None);
    }
}
