//! Band manager (GUIDE 08 §1): which positions are re-derived on every tick.
//!
//! Bands are a **cost optimisation, never the correctness mechanism**: a
//! `Cold` position gaps straight through `Warm` on a 30 % move and is caught
//! by the threshold index, not by its band. What the band decides is the
//! recompute cadence — `Hot` + `Warm` are recomputed unconditionally on every
//! fired tick, `Cool` joins them on a correlated move, `Cold` is tripwire
//! only, `Dead` (no debt) and `Unfundable` (GUIDE 07) are recomputed only when
//! their own state or the flash index changes.
//!
//! The manager is one `Option<Band>` per `PositionId` plus one dense,
//! swap-removable membership list per enumerable band, so the tick loop can
//! walk "every Hot and Warm position" as two contiguous `u32` slices.

use alloy_primitives::uint;
use liq_types::{Band, PositionId, Ray};

/// `hf < 1.02` → [`Band::Hot`].
pub const HOT_BELOW: Ray = Ray::from_raw(uint!(1_020_000_000_000_000_000_000_000_000_U256));
/// `1.02 <= hf < 1.15` → [`Band::Warm`].
pub const WARM_BELOW: Ray = Ray::from_raw(uint!(1_150_000_000_000_000_000_000_000_000_U256));
/// `1.15 <= hf < 1.50` → [`Band::Cool`]; `hf >= 1.50` → [`Band::Cold`].
pub const COOL_BELOW: Ray = Ray::from_raw(uint!(1_500_000_000_000_000_000_000_000_000_U256));

/// Band from normalised health. `Unfundable` is never produced here — the
/// engine assigns it from the eligibility verdict, and it out-ranks the
/// health band (GUIDE 07 §4b).
#[inline]
#[must_use]
pub fn classify(hf: Ray, has_debt: bool) -> Band {
    if !has_debt {
        Band::Dead
    } else if hf < HOT_BELOW {
        Band::Hot
    } else if hf < WARM_BELOW {
        Band::Warm
    } else if hf < COOL_BELOW {
        Band::Cool
    } else {
        Band::Cold
    }
}

/// Membership list index of an enumerable band; `None` for `Cold`/`Dead`
/// (tripwire / ignored — never walked, so never listed).
#[inline]
const fn set_of(band: Band) -> Option<usize> {
    match band {
        Band::Hot => Some(0),
        Band::Warm => Some(1),
        Band::Cool => Some(2),
        Band::Unfundable => Some(3),
        Band::Cold | Band::Dead => None,
    }
}

/// Sentinel in `slot`: the position's band has no membership list.
const NO_SLOT: u32 = u32::MAX;

/// Per-position band plus dense membership lists for the walked bands.
#[derive(Clone, Debug)]
pub struct BandManager {
    band: Vec<Option<Band>>,
    /// Index of the position inside its band's membership list.
    slot: Vec<u32>,
    /// `[Hot, Warm, Cool, Unfundable]`.
    sets: [Vec<PositionId>; 4],
}

impl BandManager {
    /// Reserve for `positions` ids. The walked lists are reserved for the
    /// GUIDE 08 §1 population estimate (Hot+Warm tens, Cool hundreds) with
    /// ample headroom; a crash that exceeds it grows them once.
    #[must_use]
    pub fn with_capacity(positions: usize) -> Self {
        let walked = positions.clamp(1024, 1 << 16);
        Self {
            band: Vec::with_capacity(positions),
            slot: Vec::with_capacity(positions),
            sets: [
                Vec::with_capacity(walked),
                Vec::with_capacity(walked),
                Vec::with_capacity(walked),
                Vec::with_capacity(walked),
            ],
        }
    }

    /// Ids the manager has room for without growing.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.band.len()
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.band.is_empty()
    }

    /// Current band; `None` for a position never classified.
    #[inline]
    #[must_use]
    pub fn band(&self, id: PositionId) -> Option<Band> {
        self.band.get(id.0 as usize).copied().flatten()
    }

    /// Members of a walked band (`Hot`, `Warm`, `Cool`, `Unfundable`);
    /// empty for `Cold`/`Dead`, which are never enumerated.
    #[inline]
    #[must_use]
    pub fn members(&self, band: Band) -> &[PositionId] {
        set_of(band)
            .and_then(|i| self.sets.get(i))
            .map_or(&[], Vec::as_slice)
    }

    /// Move `id` to `new`. O(1): swap-remove from the old list, push onto the
    /// new one. Ids past the reserved range grow the tables.
    pub fn set(&mut self, id: PositionId, new: Band) {
        let i = id.0 as usize;
        if i >= self.band.len() {
            let n = i.saturating_add(1);
            self.band.resize(n, None);
            self.slot.resize(n, NO_SLOT);
        }
        let old = self.band.get(i).copied().flatten();
        if old == Some(new) {
            return;
        }
        if let Some(si) = old.and_then(set_of) {
            let at = self.slot.get(i).copied().unwrap_or(NO_SLOT);
            if let Some(set) = self.sets.get_mut(si) {
                if let Some(last) = set.pop() {
                    let at = at as usize;
                    if at < set.len() {
                        if let Some(cell) = set.get_mut(at) {
                            *cell = last;
                        }
                        if let Some(s) = self.slot.get_mut(last.0 as usize) {
                            *s = at as u32;
                        }
                    }
                }
            }
        }
        let slot = match set_of(new).and_then(|si| self.sets.get_mut(si)) {
            Some(set) => {
                let at = u32::try_from(set.len()).unwrap_or(NO_SLOT);
                set.push(id);
                at
            }
            None => NO_SLOT,
        };
        if let Some(b) = self.band.get_mut(i) {
            *b = Some(new);
        }
        if let Some(s) = self.slot.get_mut(i) {
            *s = slot;
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::{classify, BandManager, COOL_BELOW, HOT_BELOW, WARM_BELOW};
    use alloy_primitives::U256;
    use liq_types::fixed::RAY;
    use liq_types::{Band, PositionId, Ray};

    fn hf(bps: u64) -> Ray {
        Ray::from_raw(RAY.checked_mul(U256::from(bps)).unwrap() / U256::from(10_000u64))
    }

    /// Oracle: GUIDE 08 §1's published boundaries (1.02 / 1.15 / 1.50),
    /// restated here from the guide rather than from the constants under
    /// test, and checked one ulp either side of each. Negative: no debt is
    /// `Dead` whatever the health factor.
    #[test]
    fn boundaries_match_guide_08() {
        assert_eq!(HOT_BELOW, hf(10_200));
        assert_eq!(WARM_BELOW, hf(11_500));
        assert_eq!(COOL_BELOW, hf(15_000));
        let ulp = Ray::from_raw(U256::ONE);
        assert_eq!(classify(hf(9_000), true), Band::Hot);
        assert_eq!(
            classify(HOT_BELOW.checked_sub(ulp).unwrap(), true),
            Band::Hot
        );
        assert_eq!(classify(HOT_BELOW, true), Band::Warm);
        assert_eq!(
            classify(WARM_BELOW.checked_sub(ulp).unwrap(), true),
            Band::Warm
        );
        assert_eq!(classify(WARM_BELOW, true), Band::Cool);
        assert_eq!(
            classify(COOL_BELOW.checked_sub(ulp).unwrap(), true),
            Band::Cool
        );
        assert_eq!(classify(COOL_BELOW, true), Band::Cold);
        assert_eq!(classify(hf(1), false), Band::Dead);
        assert_eq!(
            classify(liq_protocol::Health::NO_DEBT_HF, false),
            Band::Dead
        );
    }

    /// Oracle: set semantics — after any sequence of moves every position is
    /// in exactly the list of its current band, and the lists hold nothing
    /// else. Exercises swap-remove from the middle, the end, and re-entry.
    #[test]
    fn membership_lists_track_moves_exactly() {
        let mut m = BandManager::with_capacity(8);
        let ids: Vec<PositionId> = (0..6).map(PositionId).collect();
        for &id in &ids {
            m.set(id, Band::Hot);
        }
        m.set(ids[2], Band::Warm); // middle removal
        m.set(ids[5], Band::Cool); // tail removal
        m.set(ids[0], Band::Cold); // to an unlisted band
        m.set(ids[2], Band::Hot); // re-entry
        m.set(ids[2], Band::Hot); // no-op
        m.set(PositionId(40), Band::Unfundable); // past the reserved range
        let check = |band: Band| {
            let mut got: Vec<u32> = m.members(band).iter().map(|p| p.0).collect();
            got.sort_unstable();
            let want: Vec<u32> = (0..=40u32)
                .filter(|&i| m.band(PositionId(i)) == Some(band))
                .collect();
            assert_eq!(got, want, "{band:?}");
        };
        check(Band::Hot);
        check(Band::Warm);
        check(Band::Cool);
        check(Band::Unfundable);
        assert!(m.members(Band::Cold).is_empty());
        assert!(m.members(Band::Dead).is_empty());
        assert_eq!(m.band(ids[0]), Some(Band::Cold));
        assert_eq!(m.band(PositionId(7)), None);
        assert_eq!(m.members(Band::Hot).len(), 4);
    }
}
