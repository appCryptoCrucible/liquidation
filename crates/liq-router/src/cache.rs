//! The real [`RouteCache`] (GUIDE 07 §5, GUIDE 12 §3). Supersedes
//! `liq_flash::eligibility::DepthOnlyRouteCache` at the wiring layer
//! (carry-forward 17A: construct this from [`crate::WarmBuilder::slot`]
//! and pass it to `liq_flash::is_eligible` / `Eligibility::evaluate`).
//!
//! `has_exit(coll, amount)` is `true` when the warm tier solved a bucket
//! `≥ amount` for `coll` into *some* debt asset whose realised
//! `min(spot, twa)` output stays within the configured impact bound. One
//! `ArcSwap::load`, one indexed read, no allocation, no lock.

use std::sync::Arc;

use alloy_primitives::U256;
use arc_swap::ArcSwap;
use liq_protocol::RouteCache;
use liq_types::AssetId;

use crate::warm::RouteTable;

/// Reader handle over the warm tier's published table.
#[derive(Clone)]
pub struct WarmRouteCache {
    slot: Arc<ArcSwap<RouteTable>>,
}

impl WarmRouteCache {
    #[must_use]
    pub fn new(slot: Arc<ArcSwap<RouteTable>>) -> Self {
        Self { slot }
    }

    /// Snapshot of the current table (for the exact tier's pool sets).
    #[must_use]
    pub fn table(&self) -> Arc<RouteTable> {
        self.slot.load_full()
    }
}

impl RouteCache for WarmRouteCache {
    #[inline]
    fn has_exit(&self, coll: AssetId, amount: U256) -> bool {
        !amount.is_zero() && amount <= self.slot.load().exit_cap(coll)
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
    use std::collections::HashMap;

    use alloy_primitives::{Address, U256};
    use liq_flash::{
        is_eligible, Eligibility, FlashIndex, FlashSource, Haircut, HeldAsset, MorphoBlue,
    };
    use liq_protocol::{BonusCurve, LegChoice, Quote, RepayOption, RouteCache, SeizeOption};
    use liq_types::fixed::RAY;
    use liq_types::{AssetId, MarketId, PositionId, PositionKey, ProtocolId, Ray};
    use smallvec::SmallVec;

    use super::*;
    use crate::exact::SolveBudget;
    use crate::fixtures::*;
    use crate::solver::{Pool, PoolBook};
    use crate::warm::{WarmBuilder, WarmConfig, WarmInputs};

    struct In(SmallVec<[U256; 4]>);
    impl WarmInputs for In {
        fn bucket_sizes(&self, _: AssetId) -> Option<SmallVec<[U256; 4]>> {
            Some(self.0.clone())
        }
        fn per_eth(&self, _: AssetId) -> Option<U256> {
            Some(e18(1))
        }
        fn next_base_fee(&self) -> u128 {
            30_000_000_000
        }
        fn priority_fee_wei(&self) -> u128 {
            0
        }
        fn block(&self) -> u64 {
            1
        }
    }

    fn book(pools: Vec<Pool>) -> PoolBook {
        let mut assets = HashMap::new();
        assets.insert(tok(0), A0);
        assets.insert(tok(1), A1);
        let mut b = PoolBook::new(assets, None, HOP_GAS);
        for p in pools {
            b.add(p).unwrap();
        }
        b
    }

    fn warm() -> WarmBuilder {
        WarmBuilder::new(WarmConfig {
            max_impact_bps: 100, // 1 %
            twa_blocks: 1,
            budget: SolveBudget::default(),
        })
    }

    const LADDER: [u64; 4] = [1, 10, 100, 1_000];

    fn ladder() -> In {
        In(LADDER.iter().map(|&n| e18(n)).collect())
    }

    /// Flash side: Morpho holds 10M of the debt asset (A1) — never the
    /// binding constraint in these tests.
    fn flash() -> (Vec<Box<dyn FlashSource>>, FlashIndex) {
        let srcs: Vec<Box<dyn FlashSource>> = vec![Box::new(MorphoBlue::new(
            addr(0xA0),
            &[HeldAsset {
                asset: A1,
                token: tok(1),
                balance: e18(10_000_000),
            }],
        ))];
        let mut idx = FlashIndex::new(4);
        idx.refresh(&srcs);
        (srcs, idx)
    }

    fn quote(pos: u32, seize: U256) -> Quote {
        Quote {
            position: PositionId(pos),
            key: PositionKey {
                protocol: ProtocolId(0),
                market: MarketId(0),
                user: Address::ZERO,
            },
            repay_options: SmallVec::from_slice(&[RepayOption {
                asset: A1,
                max_repay: e18(50),
            }]),
            seize_options: SmallVec::from_slice(&[SeizeOption {
                asset: A0,
                max_seize: seize,
                bonus: Ray::from_raw(RAY / U256::from(20u64)),
                curve: BonusCurve::Static {
                    bonus: Ray::from_raw(RAY / U256::from(20u64)),
                },
            }]),
        }
    }

    const H: Haircut = match Haircut::from_bps(9_000) {
        Some(h) => h,
        None => unreachable!(),
    };

    /// Oracle: `has_exit` is the largest bucket whose `min(spot, twa)`
    /// output is within `max_impact_bps` of the zero-size marginal. A
    /// 10k/10k V2 pool at 1 % impact admits the 100 bucket (impact ≈ 1 %
    /// at 100/10k → exactly the edge: 0.997·100·10000/(10000+99.7) vs
    /// 0.997·100·0.99) and rejects 1000.
    #[test]
    fn has_exit_is_bucketed_impact_bound() {
        let bk = book(vec![v2(1, e18(10_000), e18(10_000))]);
        let mut w = warm();
        let t = w.rebuild(&bk, &ladder());
        let rc = WarmRouteCache::new(w.slot());
        // Direct check of the rule for the 100 bucket.
        let out = bk
            .get(crate::PoolId(0))
            .unwrap()
            .quote_exact_in(0, 1, e18(100))
            .unwrap();
        let zero_impact = e18(100) * U256::from(997u64) / U256::from(1000u64);
        let within = out * U256::from(10_000u64) >= zero_impact * U256::from(9_900u64);
        assert_eq!(within, e18(100) <= t.exit_cap(A0));
        assert!(rc.has_exit(A0, e18(10)));
        assert!(!rc.has_exit(A0, e18(1_000)));
        assert!(!rc.has_exit(A0, U256::ZERO));
        assert!(rc.has_exit(A1, e18(1)), "the pool quotes both directions");
        assert!(!rc.has_exit(A2, e18(1)), "asset with no pool: no exit");
    }

    /// 07 deferred criterion 1 — **route-depth fixture**: flash depth is
    /// ample (10M debt), the debt leg funds, yet the position is
    /// ineligible because the collateral exit cannot absorb `max_seize`
    /// within the impact bound. The 07B stub would have admitted it only
    /// if a flash source held the collateral; here the *router* decides.
    #[test]
    fn route_depth_binds_eligibility() {
        let bk = book(vec![v2(1, e18(10_000), e18(10_000))]);
        let mut w = warm();
        w.rebuild(&bk, &ladder());
        let rc = WarmRouteCache::new(w.slot());
        let (_, idx) = flash();
        assert!(idx.best_route(A1, e18(50), H).is_some(), "debt leg funds");
        let big = quote(1, e18(1_000));
        assert_eq!(is_eligible(&big, &idx, &rc, H), None, "route depth binds");
        let ok = quote(1, e18(10));
        let (legs, route) = is_eligible(&ok, &idx, &rc, H).unwrap();
        assert_eq!(legs, LegChoice::PREFERRED);
        assert_eq!(route.asset, A1);
    }

    /// 07 deferred criterion 2 — **re-evaluate on route change**: the
    /// same position flips eligible when the warm tier republishes after
    /// pool depth grows (a `Sync` with 100× reserves), and back when it
    /// shrinks — marked, never deleted, on the 07B bitset.
    #[test]
    fn reevaluates_on_route_change() {
        let mut bk = book(vec![v2(1, e18(10_000), e18(10_000))]);
        let mut w = warm();
        w.rebuild(&bk, &ladder());
        let rc = WarmRouteCache::new(w.slot());
        let (_, idx) = flash();
        let mut el = Eligibility::new(8);
        let q = quote(3, e18(1_000));
        assert_eq!(el.evaluate(&q, &idx, &rc, H), None);
        assert!(!el.is_fundable(PositionId(3)));

        bk.apply_log(&v2_sync_log(addr(1), e18(1_000_000), e18(1_000_000)).decoded());
        let t = w.rebuild(&bk, &ladder());
        assert!(t.generation > 0);
        assert!(
            el.evaluate(&q, &idx, &rc, H).is_some(),
            "same handle, new table"
        );
        assert!(el.is_fundable(PositionId(3)));

        bk.apply_log(&v2_sync_log(addr(1), e18(10_000), e18(10_000)).decoded());
        w.rebuild(&bk, &ladder());
        assert_eq!(el.evaluate(&q, &idx, &rc, H), None);
        assert!(!el.is_fundable(PositionId(3)));
        assert_eq!(el.len(), 8, "marked, not deleted");
    }

    /// The published entry carries a pool **set**, not one route, and the
    /// bucket ladder as given (GUIDE 12 §3).
    #[test]
    fn table_holds_pool_sets_per_bucket() {
        let bk = book(vec![
            v2(1, e18(10_000), e18(10_000)),
            v3(
                2,
                500,
                10,
                SQRT_ONE,
                &[(-6000, 6000, 5_000_000_000_000_000_000_000)],
            ),
            v2(3, e18(100), e18(100)),
        ]);
        let mut w = warm();
        let t = w.rebuild(&bk, &ladder());
        let e = t.entry(A0, A1).unwrap();
        assert_eq!(e.legs.len(), 3, "every live leg is in the set");
        assert_eq!(e.legs[0].pool, crate::PoolId(1), "best ρ₀ (5 bp V3) first");
        assert_eq!(e.buckets.len(), 4);
        for (b, &n) in e.buckets.iter().zip(&LADDER) {
            assert_eq!(b.size_in, e18(n));
            assert!(b.out_spot.is_some());
            assert!(b.pools >= 1);
        }
        assert!(
            e.buckets[3].pools >= 2,
            "large bucket splits across the set"
        );
        assert!(t.entry(A1, A0).is_some(), "reverse direction solved too");
        assert_eq!(t.len(), 2);
        assert_eq!(t.block, 1);
    }

    /// Missing inputs skip the pair (logged), never default it.
    #[test]
    fn missing_inputs_skip_pair() {
        struct NoPrice;
        impl WarmInputs for NoPrice {
            fn bucket_sizes(&self, _: AssetId) -> Option<SmallVec<[U256; 4]>> {
                Some(SmallVec::from_slice(&[e18(1)]))
            }
            fn per_eth(&self, _: AssetId) -> Option<U256> {
                None
            }
            fn next_base_fee(&self) -> u128 {
                1
            }
            fn priority_fee_wei(&self) -> u128 {
                0
            }
            fn block(&self) -> u64 {
                1
            }
        }
        let bk = book(vec![v2(1, e18(10_000), e18(10_000))]);
        let mut w = warm();
        let t = w.rebuild(&bk, &NoPrice);
        assert!(t.is_empty());
        assert!(!WarmRouteCache::new(w.slot()).has_exit(A0, e18(1)));
    }
}
