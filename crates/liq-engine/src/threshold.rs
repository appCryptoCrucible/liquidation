//! Threshold index (GUIDE 08 §2): the correctness mechanism.
//!
//! One sorted array per **global** [`AssetId`] and side — `falling` for
//! thresholds crossed when the price drops (net collateral), `rising` for
//! thresholds crossed when it rises (net debt). A tick `old → new` asks for
//! every threshold inside `[min(old,new), max(old,new)]` on the matching
//! side: two binary searches and a contiguous walk, O(log N + k), no
//! allocation, returned as an iterator borrowing the index.
//!
//! **Lazy re-registration.** Registrations land in a per-array `delta` — a
//! sorted prefix plus the unsorted tail of the current block — which
//! `crossed()` searches alongside `base` (two more binary searches; the
//! tail is the current block's few hundred entries). [`ThresholdIndex::commit`]
//! (once per block) drops the delta's stale entries, sorts it, and only
//! when it has outgrown `max(merge_min, |base| / 8)` merges it into `base`:
//! one forward pass into a retained spare buffer (no reallocation, no
//! memmove) that also drops `base`'s stale entries. A 1M-entry base is
//! therefore merged once per ~125k registrations, not once per block, and
//! nothing is ever inserted into the middle of a large array one element
//! at a time.
//!
//! **Lazy deletion.** Every entry carries the position's registration
//! generation; [`ThresholdIndex::begin`] bumps it, so every earlier entry of
//! that position — on every asset, both sides — is stale at once and is
//! skipped by `crossed()`, dropped from the delta at the next commit and
//! from the base at the next merge. Hot positions re-register on every
//! tick, so the delta compaction is what keeps the near-the-money range
//! free of stale entries. No per-position reverse map is kept: the
//! generation is the whole bookkeeping.
//!
//! The index stores what it is given. The margin and the clamp to the
//! current price are the engine's (`engine::margin`).

use alloy_primitives::U256;
use liq_types::{AssetId, PositionId, Ray};

use crate::EngineError;

/// Which price move crosses the threshold.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    /// Crossed when the price falls to or below the threshold (collateral).
    Falling,
    /// Crossed when the price rises to or above the threshold (debt).
    Rising,
}

/// One registered threshold. 40 bytes, sorted by `(price, id, gen)`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Entry {
    price: U256,
    id: PositionId,
    gen: u32,
}

/// `base` is merged once the delta exceeds `|base| / MERGE_DIV` (and
/// `merge_min`): the O(|base|) pass is amortised over that many pushes
/// while the delta stays a few binary-search steps deep.
const MERGE_DIV: usize = 8;

/// Entries with `lo <= price <= hi` of a sorted slice.
#[inline]
fn slice_range(s: &[Entry], lo: U256, hi: U256) -> &[Entry] {
    let start = s.partition_point(|e| e.price < lo);
    let end = s.partition_point(|e| e.price <= hi);
    s.get(start..end).unwrap_or(&[])
}

/// One `(asset, side)` array: a large sorted `base`, a smaller `delta`
/// whose first `sorted` entries are sorted, and the `spare` buffer the
/// next merge writes into (retained so a merge never allocates after the
/// first).
#[derive(Clone, Debug, Default)]
struct Sorted {
    base: Vec<Entry>,
    delta: Vec<Entry>,
    sorted: usize,
    spare: Vec<Entry>,
}

impl Sorted {
    #[inline]
    fn range(&self, lo: U256, hi: U256) -> impl Iterator<Item = &Entry> + '_ {
        let (prefix, tail) = self.delta.split_at(self.sorted.min(self.delta.len()));
        slice_range(&self.base, lo, hi)
            .iter()
            .chain(slice_range(prefix, lo, hi))
            .chain(tail.iter().filter(move |e| e.price >= lo && e.price <= hi))
    }

    #[inline]
    fn len(&self) -> usize {
        self.base.len().saturating_add(self.delta.len())
    }

    /// Drop the delta's stale entries and sort it; fold it into `base` once
    /// it is large enough that the O(|base|) pass is amortised. A quiet
    /// array (no registration since the last commit) costs one compare.
    fn commit(&mut self, gen: &[u32], merge_min: usize) {
        if self.delta.len() == self.sorted {
            return;
        }
        self.delta.retain(|e| is_live(gen, e));
        self.delta.sort_unstable();
        self.sorted = self.delta.len();
        if self.delta.len() > merge_min.max(self.base.len() / MERGE_DIV) {
            self.merge(gen);
        }
    }

    /// Forward merge of `base` (stale entries dropped) and the live, sorted
    /// `delta` into `spare`, then swap. One pass, no reallocation once
    /// `spare` has grown to size, no memmove.
    fn merge(&mut self, gen: &[u32]) {
        let mut out = std::mem::take(&mut self.spare);
        out.clear();
        out.reserve(self.base.len().saturating_add(self.delta.len()));
        let (mut i, mut j) = (0usize, 0usize);
        loop {
            match (self.base.get(i), self.delta.get(j)) {
                (Some(l), Some(r)) if l <= r => {
                    if is_live(gen, l) {
                        out.push(*l);
                    }
                    i = i.saturating_add(1);
                }
                (Some(_), Some(r)) | (None, Some(r)) => {
                    out.push(*r);
                    j = j.saturating_add(1);
                }
                (Some(l), None) => {
                    if is_live(gen, l) {
                        out.push(*l);
                    }
                    i = i.saturating_add(1);
                }
                (None, None) => break,
            }
        }
        self.spare = std::mem::replace(&mut self.base, out);
        self.delta.clear();
        self.sorted = 0;
    }
}

#[inline]
fn is_live(gen: &[u32], e: &Entry) -> bool {
    gen.get(e.id.0 as usize).copied() == Some(e.gen)
}

/// Sorted thresholds per global asset and side, with generation-based lazy
/// deletion. See the module docs.
#[derive(Clone, Debug)]
pub struct ThresholdIndex {
    falling: Vec<Sorted>,
    rising: Vec<Sorted>,
    /// Current registration generation per position; an entry is live iff
    /// its `gen` equals this. Wraps after 2³² re-registrations of one
    /// position, by which time every entry of the old cycle has long been
    /// merged out.
    gen: Vec<u32>,
    merge_min: usize,
}

impl ThresholdIndex {
    /// Delta size below which a commit never pays for a base merge.
    pub const MERGE_MIN: usize = 256;

    /// `assets` global ids, room for `positions` generations. Startup only.
    #[must_use]
    pub fn new(assets: usize, positions: usize) -> Self {
        Self {
            falling: vec![Sorted::default(); assets],
            rising: vec![Sorted::default(); assets],
            gen: Vec::with_capacity(positions),
            merge_min: Self::MERGE_MIN,
        }
    }

    /// Entries held, live and stale, across every array.
    #[must_use]
    pub fn len(&self) -> usize {
        self.falling
            .iter()
            .chain(&self.rising)
            .map(Sorted::len)
            .fold(0usize, usize::saturating_add)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Start a fresh registration for `id`: every earlier entry of this
    /// position is stale from here on. Call once per recompute, before the
    /// `register` calls; calling it alone deregisters the position.
    pub fn begin(&mut self, id: PositionId) {
        let i = id.0 as usize;
        if i >= self.gen.len() {
            self.gen.resize(i.saturating_add(1), 0);
        }
        if let Some(g) = self.gen.get_mut(i) {
            *g = g.wrapping_add(1);
        }
    }

    /// Register `price` for `id` on `asset`/`side` under the current
    /// generation. `Err(UnknownAsset)` for an id past the sized universe.
    pub fn register(
        &mut self,
        id: PositionId,
        asset: AssetId,
        side: Side,
        price: Ray,
    ) -> Result<(), EngineError> {
        let gen = self.gen.get(id.0 as usize).copied().unwrap_or(0);
        let arrays = match side {
            Side::Falling => &mut self.falling,
            Side::Rising => &mut self.rising,
        };
        let s = arrays
            .get_mut(usize::from(asset.0))
            .ok_or(EngineError::UnknownAsset(asset))?;
        s.delta.push(Entry {
            price: price.raw(),
            id,
            gen,
        });
        Ok(())
    }

    /// Positions whose live threshold on `asset` lies between `old` and
    /// `new` (inclusive) on the side the move crosses. Borrows the index,
    /// allocates nothing; may repeat an id registered on both sides at the
    /// same price (the caller sorts and dedups its batch anyway). Unknown
    /// assets yield nothing.
    pub fn crossed(
        &self,
        asset: AssetId,
        old: Ray,
        new: Ray,
    ) -> impl Iterator<Item = PositionId> + '_ {
        let (arrays, lo, hi) = if new < old {
            (&self.falling, new.raw(), old.raw())
        } else {
            (&self.rising, old.raw(), new.raw())
        };
        arrays
            .get(usize::from(asset.0))
            .into_iter()
            .flat_map(move |s| s.range(lo, hi))
            .filter(move |e| is_live(&self.gen, e))
            .map(|e| e.id)
    }

    /// Every position with a live threshold on `asset`, either side — the
    /// set a `MarketReprice` on that asset must re-derive. O(N_asset); may
    /// repeat an id registered on both sides. Unknown assets yield nothing.
    pub fn registered(&self, asset: AssetId) -> impl Iterator<Item = PositionId> + '_ {
        let i = usize::from(asset.0);
        self.falling
            .get(i)
            .into_iter()
            .chain(self.rising.get(i))
            .flat_map(|s| s.base.iter().chain(&s.delta))
            .filter(move |e| is_live(&self.gen, e))
            .map(|e| e.id)
    }

    /// Once per block: sort every delta tail, merge the large ones.
    pub fn commit(&mut self) {
        for s in self.falling.iter_mut().chain(self.rising.iter_mut()) {
            s.commit(&self.gen, self.merge_min);
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
    use super::{Side, ThresholdIndex};
    use alloy_primitives::U256;
    use liq_types::{AssetId, PositionId, Ray};
    use std::collections::BTreeSet;

    fn r(v: u64) -> Ray {
        Ray::from_raw(U256::from(v))
    }

    fn ids(it: impl Iterator<Item = PositionId>) -> BTreeSet<u32> {
        it.map(|p| p.0).collect()
    }

    /// Reference model: the live `(side, price)` per position, queried by
    /// brute force. Oracle: an independent implementation of the same
    /// contract (inclusive range on the crossed side).
    fn brute(model: &[(u32, Side, u64)], asset_ok: bool, old: u64, new: u64) -> BTreeSet<u32> {
        if !asset_ok {
            return BTreeSet::new();
        }
        let (side, lo, hi) = if new < old {
            (Side::Falling, new, old)
        } else {
            (Side::Rising, old, new)
        };
        model
            .iter()
            .filter(|(_, s, p)| *s == side && *p >= lo && *p <= hi)
            .map(|(id, _, _)| *id)
            .collect()
    }

    /// Oracle: brute force over the reference model, across every tier the
    /// index has — unsorted tail, sorted delta, merged base — and after
    /// re-registration makes the old entries stale. Negative: a stale entry
    /// inside the range is not reported; an unknown asset yields nothing.
    #[test]
    fn crossed_matches_brute_force_across_tiers_and_staleness() {
        let a = AssetId(3);
        let mut idx = ThresholdIndex::new(4, 64);
        let mut model: Vec<(u32, Side, u64)> = Vec::new();
        // 40 positions, alternating sides, spread prices.
        for i in 0..40u32 {
            let side = if i % 2 == 0 {
                Side::Falling
            } else {
                Side::Rising
            };
            let p = 1_000 + u64::from(i) * 37;
            idx.begin(PositionId(i));
            idx.register(PositionId(i), a, side, r(p)).unwrap();
            model.push((i, side, p));
        }
        let queries = [
            (2_000u64, 1_100u64),
            (1_100, 2_000),
            (1_500, 1_500),
            (0, 5_000),
        ];
        // Tier 1: everything in the unsorted tail.
        for &(old, new) in &queries {
            assert_eq!(
                ids(idx.crossed(a, r(old), r(new))),
                brute(&model, true, old, new),
                "tail {old}->{new}"
            );
        }
        // Tier 2: sorted delta (commit below the merge threshold).
        idx.commit();
        for &(old, new) in &queries {
            assert_eq!(
                ids(idx.crossed(a, r(old), r(new))),
                brute(&model, true, old, new),
                "delta {old}->{new}"
            );
        }
        // Tier 3: force a base merge by exceeding the threshold.
        idx.merge_min = 8;
        for i in 40..60u32 {
            idx.begin(PositionId(i));
            idx.register(PositionId(i), a, Side::Falling, r(900 + u64::from(i)))
                .unwrap();
            model.push((i, Side::Falling, 900 + u64::from(i)));
        }
        idx.commit();
        assert!(idx.falling[3].delta.is_empty(), "merged into base");
        for &(old, new) in &queries {
            assert_eq!(
                ids(idx.crossed(a, r(old), r(new))),
                brute(&model, true, old, new),
                "base {old}->{new}"
            );
        }
        // Staleness: re-register 0 and 1 elsewhere, drop 2 entirely.
        idx.begin(PositionId(0));
        idx.register(PositionId(0), a, Side::Rising, r(4_000))
            .unwrap();
        idx.begin(PositionId(1));
        idx.register(PositionId(1), a, Side::Falling, r(1_000))
            .unwrap();
        idx.begin(PositionId(2));
        model.retain(|(id, _, _)| *id > 2);
        model.push((0, Side::Rising, 4_000));
        model.push((1, Side::Falling, 1_000));
        for &(old, new) in &queries {
            assert_eq!(
                ids(idx.crossed(a, r(old), r(new))),
                brute(&model, true, old, new),
                "stale {old}->{new}"
            );
        }
        assert_eq!(
            ids(idx.crossed(a, r(3_000), r(5_000))),
            BTreeSet::from([0]),
            "re-registered on the other side"
        );
        // A merge drops the stale entries physically: three are stale (0 and
        // 2 falling, 1 rising); push both arrays past √|base| so both merge.
        let before = idx.len();
        idx.merge_min = 0;
        for i in 61..=80u32 {
            let (side, p) = if i <= 70 {
                (Side::Rising, u64::from(i))
            } else {
                (Side::Falling, 5_000 + u64::from(i))
            };
            idx.begin(PositionId(i));
            idx.register(PositionId(i), a, side, r(p)).unwrap();
            model.push((i, side, p));
        }
        idx.commit();
        assert_eq!(idx.len(), before + 20 - 3, "stale entries compacted");
        assert!(idx.falling[3].delta.is_empty() && idx.rising[3].delta.is_empty());
        for &(old, new) in &queries {
            assert_eq!(
                ids(idx.crossed(a, r(old), r(new))),
                brute(&model, true, old, new),
                "compacted {old}->{new}"
            );
        }
        // Unknown asset: nothing, no panic; register refuses it.
        assert!(idx.crossed(AssetId(9), r(0), r(u64::MAX)).next().is_none());
        assert_eq!(
            idx.register(PositionId(1), AssetId(9), Side::Rising, r(1)),
            Err(crate::EngineError::UnknownAsset(AssetId(9)))
        );
    }

    /// Oracle: the merge is a permutation of the live entries into sorted
    /// order — checked against `BTreeSet` (an independent sorted container)
    /// on interleaved keys, including duplicates across base and delta.
    #[test]
    fn merge_is_sorted_and_complete() {
        let a = AssetId(0);
        let mut idx = ThresholdIndex::new(1, 128);
        idx.merge_min = 0;
        let mut want: BTreeSet<(u64, u32)> = BTreeSet::new();
        for round in 0..3u32 {
            for i in 0..30u32 {
                let id = round * 30 + i;
                let p = u64::from((id * 7919) % 101);
                idx.begin(PositionId(id));
                idx.register(PositionId(id), a, Side::Falling, r(p))
                    .unwrap();
                want.insert((p, id));
            }
            idx.commit();
            let got: Vec<(u64, u32)> = idx.falling[0]
                .base
                .iter()
                .map(|e| (e.price.to::<u64>(), e.id.0))
                .collect();
            let mut sorted = got.clone();
            sorted.sort_unstable();
            assert_eq!(got, sorted, "base sorted after round {round}");
            assert_eq!(
                got.into_iter().collect::<BTreeSet<_>>(),
                want,
                "base complete after round {round}"
            );
            assert!(idx.falling[0].delta.is_empty());
        }
    }
}
