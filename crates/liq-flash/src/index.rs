//! `FlashIndex` — the liquidity index (GUIDE 07 §4, §4b).
//!
//! Written by the ingest thread after each block's `apply_log`s, read on
//! that thread as `&FlashIndex` (no synchronisation), and handed to
//! off-thread readers through `ArcSwap<FlashIndex>` republished **only on
//! material change** — not every block.
//!
//! Ingest integration (03B wires it; `ApplyCtx` gains
//! `flash: &'a mut [Box<dyn FlashSource>]` beside `handlers`, routed by the
//! same `(address, topic0)` table from each source's `subscriptions()`):
//!
//! ```text
//! for log in block.logs { for i in routed_flash(log) { flash[i].apply_log(&log) } }
//! index.refresh(&flash);                                    // apply_log is silent: re-read
//! index.publish_if_material(&shared.flash, threshold_bps);   // end of block
//! ```
//!
//! 07A's protocol-event filters (Aave `Supply`, V3 `Mint`, …) are kept: the
//! sources ignore them, and the index re-reads every source after the block
//! regardless of which log arrived, so they cost one routed no-op each.

use alloy_primitives::{Address, U256};
use arc_swap::ArcSwap;
use liq_protocol::{CallbackShape, FlashRoute};
use liq_types::fixed::{mul_div, Rounding};
use liq_types::{AssetId, FlashProvider};
use smallvec::SmallVec;
use std::cmp::Reverse;
use std::sync::Arc;

use crate::FlashSource;

/// `10_000`: basis-point denominator.
pub const BPS: U256 = U256::from_limbs([10_000, 0, 0, 0]);

/// One source's standing for one asset. 64 bytes — one cache line. Carries
/// `source` and `callback` so a published snapshot can mint a
/// [`FlashRoute`] without reaching back to the writer-owned sources.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SourceEntry {
    pub available: U256,
    pub source: Address,
    pub gas_overhead: u64,
    /// `FlashSource::fee_bps` at reference size `available`. Exact for
    /// every 07A source (none is size-tiered); a tiered source would need
    /// a per-size re-query here.
    pub fee_bps: u16,
    pub provider: FlashProvider,
    pub callback: CallbackShape,
}

impl SourceEntry {
    /// The route borrowing `amount` of `asset` from this source.
    #[inline]
    #[must_use]
    pub fn route(&self, asset: AssetId, amount: U256) -> FlashRoute {
        FlashRoute {
            provider: self.provider,
            source: self.source,
            asset,
            amount,
            fee_bps: self.fee_bps,
            callback: self.callback,
        }
    }
}

/// Eligibility haircut on reported availability (GUIDE 07 §4): another
/// searcher's flash can land ahead of ours in the same block. Distinct from
/// the fixed 1 % cascade buffer ([`crate::cascade::planable`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Haircut(u16);

impl Haircut {
    /// 100 %: no haircut.
    pub const NONE: Self = Self(10_000);

    /// `None` above 100 %.
    #[must_use]
    pub const fn from_bps(bps: u16) -> Option<Self> {
        if bps > 10_000 {
            None
        } else {
            Some(Self(bps))
        }
    }

    /// `floor(available · bps / 10_000)`. The result never exceeds the
    /// input, so `Err` is unreachable; it fails closed to `0` regardless.
    #[inline]
    #[must_use]
    pub fn apply(self, available: U256) -> U256 {
        mul_div(available, U256::from(self.0), BPS, Rounding::Down).unwrap_or(U256::ZERO)
    }

    /// Smallest depth whose haircut covers `amount`:
    /// `ceil(amount · 10_000 / bps)`. For integers,
    /// `apply(a) >= amount ⇔ a >= gross_up(amount)`, so one 512-bit divide
    /// per query replaces one per entry. `None` at 0 % (nothing is ever
    /// fundable) or on overflow — both mean "no depth suffices".
    #[inline]
    #[must_use]
    pub fn gross_up(self, amount: U256) -> Option<U256> {
        mul_div(amount, BPS, U256::from(self.0), Rounding::Up).ok()
    }
}

/// Per-asset flash depth. `by_asset` is indexed by the global [`AssetId`];
/// six inline entries cover the five arenas with room, so the common case
/// never touches the heap. Entries are sorted `(fee_bps, gas_overhead,
/// depth desc)` so the first entry that fits is the cheapest at zero gas
/// price; `select::fallback_chain` re-ranks with a live gas price.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlashIndex {
    by_asset: Vec<SmallVec<[SourceEntry; 6]>>,
    max_available: Vec<U256>,
}

impl FlashIndex {
    /// Empty index over `assets` global ids. Startup only.
    #[must_use]
    pub fn new(assets: usize) -> Self {
        Self {
            by_asset: vec![SmallVec::new(); assets],
            max_available: vec![U256::ZERO; assets],
        }
    }

    /// Deepest single source for `asset`, before haircut. One indexed load;
    /// unknown asset → `0`.
    #[inline]
    #[must_use]
    pub fn available(&self, asset: AssetId) -> U256 {
        self.max_available
            .get(usize::from(asset.0))
            .copied()
            .unwrap_or(U256::ZERO)
    }

    /// Sources holding `asset`, cheapest first. Empty for unknown assets.
    #[inline]
    #[must_use]
    pub fn entries(&self, asset: AssetId) -> &[SourceEntry] {
        self.by_asset
            .get(usize::from(asset.0))
            .map_or(&[], SmallVec::as_slice)
    }

    /// Cheapest single source funding `amount` of `debt` under `haircut`.
    /// Fast reject on the cached max, then a scan of ≤ 6 pre-sorted entries.
    #[inline]
    #[must_use]
    pub fn best_route(&self, debt: AssetId, amount: U256, haircut: Haircut) -> Option<FlashRoute> {
        if amount.is_zero() {
            return None;
        }
        let depth = haircut.gross_up(amount)?;
        if self.available(debt) < depth {
            return None;
        }
        self.entries(debt)
            .iter()
            .find(|e| e.available >= depth)
            .map(|e| e.route(debt, amount))
    }

    /// Re-read every source for every asset. Ingest thread, once per block
    /// after the sources' `apply_log`s. Allocation-free in steady state
    /// (≤ 6 sources per asset stay inline). Zero-depth sources are dropped
    /// from the row: they cannot fund anything.
    pub fn refresh(&mut self, sources: &[Box<dyn FlashSource>]) {
        for (i, (row, max)) in self
            .by_asset
            .iter_mut()
            .zip(self.max_available.iter_mut())
            .enumerate()
        {
            row.clear();
            *max = U256::ZERO;
            let Ok(id) = u16::try_from(i) else {
                continue;
            };
            let asset = AssetId(id);
            for s in sources {
                let available = s.available(asset);
                if available.is_zero() {
                    continue;
                }
                row.push(SourceEntry {
                    available,
                    source: s.source(),
                    gas_overhead: s.gas_overhead(),
                    fee_bps: s.fee_bps(asset, available),
                    provider: s.provider(),
                    callback: s.callback(),
                });
                if available > *max {
                    *max = available;
                }
            }
            row.sort_unstable_by_key(|e| (e.fee_bps, e.gas_overhead, Reverse(e.available)));
        }
    }

    /// `true` when republishing is warranted: any row changed shape, any
    /// fee / gas / identity changed, or any depth moved by more than
    /// `threshold_bps` of the published depth.
    #[must_use]
    pub fn differs_materially(&self, published: &Self, threshold_bps: u16) -> bool {
        if self.by_asset.len() != published.by_asset.len() {
            return true;
        }
        let threshold = U256::from(threshold_bps);
        let moved = |a: U256, b: U256| {
            mul_div(b, threshold, BPS, Rounding::Down).is_ok_and(|lim| a.abs_diff(b) > lim)
        };
        self.by_asset.iter().zip(&published.by_asset).any(|(a, b)| {
            a.len() != b.len()
                || a.iter().zip(b.iter()).any(|(x, y)| {
                    x.provider != y.provider
                        || x.source != y.source
                        || x.fee_bps != y.fee_bps
                        || x.gas_overhead != y.gas_overhead
                        || x.callback != y.callback
                        || moved(x.available, y.available)
                })
        })
    }

    /// End of block on the writer thread: swap a clone into `slot` iff
    /// [`Self::differs_materially`] from what readers currently see.
    /// Returns whether a publish happened.
    pub fn publish_if_material(&self, slot: &ArcSwap<Self>, threshold_bps: u16) -> bool {
        if !self.differs_materially(&slot.load(), threshold_bps) {
            return false;
        }
        slot.store(Arc::new(self.clone()));
        true
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
pub(crate) mod fixtures {
    //! Index built from the five 07A sources at block 26_000_000.

    use alloy_primitives::U256;

    use super::FlashIndex;
    use crate::sources::fixtures::*;
    use crate::{
        AavePool, AaveReserve, FlashSource, HeldAsset, MorphoBlue, SkyDssFlash, UniV3Pool,
        UniV4PoolManager,
    };

    /// Asset slots in the test universe: USDC, WETH, DAI, native, plus
    /// `NO_DEPTH`, an id no source holds.
    pub(crate) const ASSETS: usize = 6;
    pub(crate) const NO_DEPTH: liq_types::AssetId = liq_types::AssetId(5);

    pub(crate) fn aave() -> Box<dyn FlashSource> {
        Box::new(AavePool::new(
            AAVE_POOL,
            AAVE_CONFIGURATOR,
            5,
            &[
                AaveReserve {
                    asset: ID_USDC,
                    underlying: USDC,
                    atoken: A_USDC,
                    balance: U256::from(AUSDC_USDC_26M),
                    flash_enabled: true,
                    active: true,
                    paused: false,
                },
                AaveReserve {
                    asset: ID_WETH,
                    underlying: WETH,
                    atoken: A_WETH,
                    balance: u256(AWETH_WETH_26M),
                    flash_enabled: true,
                    active: true,
                    paused: false,
                },
            ],
        ))
    }

    pub(crate) fn v3() -> Box<dyn FlashSource> {
        Box::new(UniV3Pool::new(
            UNIV3_USDC_WETH_500,
            USDC,
            WETH,
            ID_USDC,
            ID_WETH,
            500,
            U256::from(V3_500_USDC_26M),
            u256(V3_500_WETH_26M),
        ))
    }

    pub(crate) fn v4() -> Box<dyn FlashSource> {
        Box::new(UniV4PoolManager::new(
            POOL_MANAGER,
            &[
                HeldAsset {
                    asset: ID_USDC,
                    token: USDC,
                    balance: U256::from(PM_USDC_26M),
                },
                HeldAsset {
                    asset: ID_WETH,
                    token: WETH,
                    balance: u256(PM_WETH_26M),
                },
            ],
        ))
    }

    pub(crate) fn morpho() -> Box<dyn FlashSource> {
        Box::new(MorphoBlue::new(
            MORPHO,
            &[
                HeldAsset {
                    asset: ID_USDC,
                    token: USDC,
                    balance: U256::from(MORPHO_USDC_26M),
                },
                HeldAsset {
                    asset: ID_WETH,
                    token: WETH,
                    balance: u256(MORPHO_WETH_26M),
                },
            ],
        ))
    }

    pub(crate) fn dss() -> Box<dyn FlashSource> {
        Box::new(SkyDssFlash::new(
            DSS_FLASH,
            END,
            ID_DAI,
            u256(DSS_MAX_26M),
            U256::ZERO,
            true,
        ))
    }

    pub(crate) fn five() -> Vec<Box<dyn FlashSource>> {
        vec![aave(), v3(), v4(), morpho(), dss()]
    }

    pub(crate) fn index_of(sources: &[Box<dyn FlashSource>]) -> FlashIndex {
        let mut idx = FlashIndex::new(ASSETS);
        idx.refresh(sources);
        idx
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use alloy_primitives::{Address, U256};
    use arc_swap::ArcSwap;
    use liq_types::{AssetId, FlashProvider};
    use std::sync::Arc;

    use super::fixtures::*;
    use super::{FlashIndex, Haircut, SourceEntry};
    use crate::sources::fixtures::*;

    /// Oracle: GUIDE 07 §4b — missing configuration degrades to "cannot
    /// fund", never a panic. Out-of-range ids included.
    #[test]
    fn empty_index_is_zero_everywhere() {
        let idx = FlashIndex::new(3);
        for a in [0u16, 2, 3, u16::MAX] {
            assert_eq!(idx.available(AssetId(a)), U256::ZERO);
            assert!(idx.entries(AssetId(a)).is_empty());
            assert_eq!(
                idx.best_route(AssetId(a), U256::from(1u64), Haircut::NONE),
                None
            );
        }
    }

    /// Oracle: field widths 32 + 20 + 8 + 2 + 1 + 1 = 64 — one cache line,
    /// as the struct doc claims.
    #[test]
    fn source_entry_is_one_cache_line() {
        assert_eq!(std::mem::size_of::<SourceEntry>(), 64);
    }

    /// Oracle: the chain at block 26_000_000 (07A's `eth_call` balances)
    /// plus GUIDE 07 §4 ordering — fee-free sources first, deeper first
    /// within a fee tier. Sky DSS holds no USDC and so has no USDC row.
    #[test]
    fn refresh_from_five_sources_at_26m() {
        let idx = index_of(&five());
        let usdc = idx.entries(ID_USDC);
        let order: Vec<_> = usdc.iter().map(|e| e.provider).collect();
        assert_eq!(
            order,
            [
                FlashProvider::Morpho, // 0 bps, 108.68M
                FlashProvider::UniV4,  // 0 bps, 66.23M
                FlashProvider::Aave,   // 5 bps, 181.65M
                FlashProvider::UniV3,  // 5 bps, 74.29M
            ]
        );
        assert_eq!(idx.available(ID_USDC), U256::from(AUSDC_USDC_26M));
        assert_eq!(usdc[0].available, U256::from(MORPHO_USDC_26M));
        assert_eq!(usdc[0].source, MORPHO);
        assert_eq!(usdc[2].source, AAVE_POOL);
        assert_eq!(usdc[2].fee_bps, 5, "oracle: FLASHLOAN_PREMIUM_TOTAL at 26M");
        assert_eq!(usdc[3].fee_bps, 5, "oracle: pool.fee() = 500 → 5 bps");
        for e in usdc {
            assert_eq!(e.callback.provider(), e.provider);
            assert_eq!(e.gas_overhead, crate::GAS_OVERHEAD_STUB);
        }
        let dai = idx.entries(ID_DAI);
        assert_eq!(dai.len(), 1);
        assert_eq!(dai[0].provider, FlashProvider::SkyDss);
        assert_eq!(idx.available(ID_DAI), u256(DSS_MAX_26M));
        assert!(
            idx.entries(ID_NATIVE).is_empty(),
            "native ETH is never flashed"
        );
        assert!(idx.entries(NO_DEPTH).is_empty());
    }

    /// Oracle: GUIDE 07 §4 — the index is driven by the log stream. A
    /// `Transfer` out of the PoolManager moves the V4 entry by exactly the
    /// transferred amount after `refresh`; nothing here can reach an RPC
    /// (the `PanicLogSource` methods stay uncalled).
    #[test]
    fn refresh_is_log_driven() {
        let _rpc = PanicLogSource;
        let mut srcs = five();
        let mut idx = index_of(&srcs);
        let before = idx.entries(ID_USDC)[1].available;
        assert_eq!(before, U256::from(PM_USDC_26M));
        let amt = U256::from(1_000_000u64);
        let topics = transfer_topics(POOL_MANAGER, Address::ZERO);
        let data = u256_be(amt);
        let lg = log(USDC, &topics, &data);
        for s in &mut srcs {
            s.apply_log(&lg);
        }
        idx.refresh(&srcs);
        let v4 = idx
            .entries(ID_USDC)
            .iter()
            .find(|e| e.provider == FlashProvider::UniV4)
            .unwrap();
        assert_eq!(v4.available, before - amt);
        assert_eq!(
            idx.available(ID_USDC),
            U256::from(AUSDC_USDC_26M),
            "max is Aave"
        );
    }

    /// Oracle: GUIDE 07 §4 — a source at zero depth cannot fund; its row
    /// entry disappears and the max is unaffected when it was not the max.
    #[test]
    fn drained_source_leaves_the_row() {
        let mut srcs = five();
        let topics = transfer_topics(POOL_MANAGER, Address::ZERO);
        let data = u256_be(U256::from(PM_USDC_26M));
        let lg = log(USDC, &topics, &data);
        for s in &mut srcs {
            s.apply_log(&lg);
        }
        let idx = index_of(&srcs);
        assert_eq!(idx.entries(ID_USDC).len(), 3);
        assert!(idx
            .entries(ID_USDC)
            .iter()
            .all(|e| e.provider != FlashProvider::UniV4));
        assert_eq!(idx.available(ID_USDC), U256::from(AUSDC_USDC_26M));
    }

    /// Oracle: hand arithmetic — 90 % of the 26M Morpho USDC balance
    /// 108_682_339_582_079 is 97_814_105_623_871 (floor). 100 % is identity;
    /// > 100 % is rejected.
    #[test]
    fn haircut_floor_and_bounds() {
        let h = Haircut::from_bps(9_000).unwrap();
        assert_eq!(
            h.apply(U256::from(MORPHO_USDC_26M)),
            U256::from(97_814_105_623_871u64)
        );
        assert_eq!(Haircut::NONE.apply(U256::MAX), U256::MAX);
        assert_eq!(Haircut::from_bps(10_001), None);
        assert_eq!(Haircut::from_bps(0).unwrap().apply(U256::MAX), U256::ZERO);
        assert_eq!(
            Haircut::from_bps(0).unwrap().gross_up(U256::from(1u64)),
            None
        );
        assert_eq!(
            Haircut::NONE.gross_up(U256::from(7u64)),
            Some(U256::from(7u64))
        );
    }

    /// Oracle: a mathematical invariant — for integers,
    /// `floor(a·h/1e4) >= n ⇔ a >= ceil(n·1e4/h)`. This is what lets
    /// `best_route` do one divide per query instead of one per entry.
    /// Biased to the boundary: `a` is drawn around `gross_up(n)`.
    #[test]
    fn gross_up_is_exact_inverse_of_apply() {
        use proptest::prelude::*;
        let cfg = ProptestConfig {
            cases: 2_048,
            ..ProptestConfig::default()
        };
        proptest!(cfg, |(n in 1u128..=u128::MAX, bps in 1u16..=10_000u16, delta in -3i8..=3i8)| {
            let h = Haircut::from_bps(bps).unwrap();
            let n = U256::from(n);
            let g = h.gross_up(n).unwrap();
            let a = if delta < 0 {
                g.saturating_sub(U256::from(delta.unsigned_abs()))
            } else {
                g.saturating_add(U256::from(delta.unsigned_abs()))
            };
            prop_assert_eq!(h.apply(a) >= n, a >= g, "a={} n={} bps={}", a, n, bps);
        });
    }

    /// Oracle: chain depths at 26M under a 90 % haircut (hand computed:
    /// Morpho 97.81M, V4 59.61M, Aave 163.48M, V3 66.86M) and GUIDE 07 §6 —
    /// cheapest source that fits. 50M → Morpho (0 bps). 100M → Aave (only
    /// 5-bps source deep enough; Morpho and V4 are out). 170M → none.
    #[test]
    fn best_route_is_cheapest_that_fits_under_haircut() {
        let idx = index_of(&five());
        let h = Haircut::from_bps(9_000).unwrap();
        let usdc = |m: u64| U256::from(m) * U256::from(1_000_000u64);
        let r = idx.best_route(ID_USDC, usdc(50_000_000), h).unwrap();
        assert_eq!(r.provider, FlashProvider::Morpho);
        assert_eq!(r.source, MORPHO);
        assert_eq!(r.asset, ID_USDC);
        assert_eq!(r.amount, usdc(50_000_000));
        assert_eq!(r.fee_bps, 0);
        assert_eq!(r.callback, liq_protocol::CallbackShape::MorphoFlashCallback);
        let r = idx.best_route(ID_USDC, usdc(100_000_000), h).unwrap();
        assert_eq!(r.provider, FlashProvider::Aave);
        assert_eq!(r.fee_bps, 5);
        assert_eq!(idx.best_route(ID_USDC, usdc(170_000_000), h), None);
        // Boundary: exactly the haircut depth of the max source fits;
        // one raw unit more does not.
        let lim = U256::from(163_481_890_531_543u64);
        assert!(idx.best_route(ID_USDC, lim, h).is_some());
        assert_eq!(idx.best_route(ID_USDC, lim + U256::from(1u64), h), None);
        assert_eq!(
            idx.best_route(ID_USDC, U256::ZERO, h),
            None,
            "zero is not a loan"
        );
    }

    /// Oracle: GUIDE 07 §4b — republish only past a threshold. 1 % of the
    /// 26M PoolManager USDC balance 66_230_362_793_739 is 662_303_627_937
    /// (floor). A move of exactly that is not material; one unit more is.
    /// A fee change is material at any depth.
    #[test]
    fn differs_materially_at_threshold_boundary() {
        let base = index_of(&five());
        assert!(!base.differs_materially(&base, 100));
        let drain = |amt: U256| {
            let mut srcs = five();
            let topics = transfer_topics(POOL_MANAGER, Address::ZERO);
            let data = u256_be(amt);
            let lg = log(USDC, &topics, &data);
            for s in &mut srcs {
                s.apply_log(&lg);
            }
            index_of(&srcs)
        };
        let lim = U256::from(662_303_627_937u64);
        assert!(!drain(lim).differs_materially(&base, 100));
        assert!(drain(lim + U256::from(1u64)).differs_materially(&base, 100));
        assert!(
            drain(lim).differs_materially(&base, 0),
            "threshold 0: any move"
        );

        let mut srcs = five();
        let t0 = crate::sources::fixtures::T0_PREMIUM;
        let topics = [t0];
        let data = [0u8; 64];
        srcs[0].apply_log(&log(AAVE_CONFIGURATOR, &topics, &data));
        assert!(index_of(&srcs).differs_materially(&base, 10_000));
        assert!(FlashIndex::new(2).differs_materially(&base, 10_000));
    }

    /// Oracle: `arc-swap` semantics — the reader's `Arc` is unchanged when
    /// nothing material moved, replaced when it did.
    #[test]
    fn publish_only_on_material_change() {
        let base = index_of(&five());
        let slot = ArcSwap::from_pointee(base.clone());
        let published = slot.load_full();
        assert!(!base.publish_if_material(&slot, 100));
        assert!(Arc::ptr_eq(&published, &slot.load_full()));

        let mut srcs = five();
        let topics = transfer_topics(POOL_MANAGER, Address::ZERO);
        let data = u256_be(U256::from(PM_USDC_26M));
        let lg = log(USDC, &topics, &data);
        for s in &mut srcs {
            s.apply_log(&lg);
        }
        let moved = index_of(&srcs);
        assert!(moved.publish_if_material(&slot, 100));
        let now = slot.load_full();
        assert!(!Arc::ptr_eq(&published, &now));
        assert_eq!(*now, moved);
        assert!(!moved.publish_if_material(&slot, 100), "idempotent");
    }

    /// Oracle: mathematical invariants of the index over proptest depths —
    /// `available(a)` equals the max over `entries(a)`, rows are sorted
    /// `(fee, gas, depth desc)`, and no zero-depth entry survives.
    #[test]
    fn max_matches_entries_and_rows_are_sorted() {
        use crate::{HeldAsset, MorphoBlue, UniV4PoolManager};
        use proptest::prelude::*;
        let cfg = ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        };
        proptest!(cfg, |(pm in 0u128..=u128::MAX, mo in 0u128..=u128::MAX, aave_on in any::<bool>())| {
            let mut srcs: Vec<Box<dyn crate::FlashSource>> = vec![
                Box::new(UniV4PoolManager::new(POOL_MANAGER, &[HeldAsset { asset: ID_USDC, token: USDC, balance: U256::from(pm) }])),
                Box::new(MorphoBlue::new(MORPHO, &[HeldAsset { asset: ID_USDC, token: USDC, balance: U256::from(mo) }])),
            ];
            if aave_on {
                srcs.push(aave());
            }
            let idx = index_of(&srcs);
            let row = idx.entries(ID_USDC);
            let max = row.iter().map(|e| e.available).max().unwrap_or(U256::ZERO);
            prop_assert_eq!(idx.available(ID_USDC), max);
            prop_assert!(row.iter().all(|e| !e.available.is_zero()));
            let sorted = row.windows(2).all(|w| {
                (w[0].fee_bps, w[0].gas_overhead, std::cmp::Reverse(w[0].available))
                    <= (w[1].fee_bps, w[1].gas_overhead, std::cmp::Reverse(w[1].available))
            });
            prop_assert!(sorted, "row not sorted (fee, gas, depth desc)");
            let expect = usize::from(pm > 0) + usize::from(mo > 0) + usize::from(aave_on);
            prop_assert_eq!(row.len(), expect);
        });
    }
}
