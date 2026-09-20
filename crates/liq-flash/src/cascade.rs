//! Multi-source cascade (GUIDE 07 §7, D44): at most **three sequential
//! sibling groups** sharing one debt asset, never nested. `Executor.sol`
//! runs groups one after another (PLAN-ENCODING §1b′, D32); the plan here
//! is a flat list by type — nothing in [`Cascade`] can express a nest.
//!
//! **Deviation from the literal §7 rule 2 (greedy cheapest-first).** With
//! the ≤ 3 cap a greedy fill can miss a feasible plan: the three cheapest
//! sources can be shallower than a subset holding a dearer but deeper one.
//! At block 26M, with the V3 1-bp tier indexed as its own row (§3a: "the
//! 100 tier is where stable/stable and LST/ETH depth lives"), the WETH
//! planables are Morpho 15_721.52 and V4 2_109.84 at 0 bps, V3-100 699.96
//! at 1 bp, then Aave 285_706.28 and V3-500 12_042.30 at 5 bps. For
//! 295_000 WETH the three cheapest fund 18_531.32 and greedy stops there,
//! while {Morpho, Aave} funds 301_427.80. (On the five-arena row alone,
//! without the 1-bp tier, greedy does reach 295_000 — the miss needs a
//! fourth source cheaper than Aave, which §3a's tier rows supply.) The
//! planner instead enumerates every subset of
//! ≤ 3 planable sources (≤ 92 for eight candidates), fills each
//! cheapest-first — optimal within a subset, fees being linear and the
//! per-group cost fixed — and takes the cheapest feasible plan. Rules 1 and
//! 5 fall out: a single source that fits is a size-1 subset, and
//! `close_bps` lets a plan with fewer groups win when its cost is within
//! `need · close_bps / 10_000` of the cheapest (a partial that lands beats
//! a full plan that reverts). When no ≤ 3-subset reaches `need`, the plan
//! with the most funding is returned and `funded < need` says so; sizing
//! the partial against profit is 12A-2's (`size = min4`).

use alloy_primitives::U256;
use liq_protocol::FlashRoute;
use liq_types::fixed::{mul_div, Rounding};
use liq_types::AssetId;
use smallvec::SmallVec;

use crate::index::{FlashIndex, SourceEntry, BPS};
use crate::select::{effective_cost, CostModel};

/// D44: at most three sources / groups for one debt asset in one plan.
pub const MAX_GROUPS: usize = 3;
/// Entries considered, cheapest first. Five arenas plus V3 tiers fit; a
/// longer row is truncated at its most expensive tail.
const MAX_CANDIDATES: usize = 8;

/// `floor(available · 99 / 100)` — the planable buffer (PLAN-ENCODING
/// §1b′, D44). Distinct from the eligibility [`crate::index::Haircut`].
#[inline]
#[must_use]
pub fn planable(available: U256) -> U256 {
    mul_div(
        available,
        U256::from(99u64),
        U256::from(100u64),
        Rounding::Down,
    )
    .unwrap_or(U256::ZERO)
}

/// One debt asset's funding plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cascade {
    /// Sibling groups in execution order, one flash each, all the same
    /// `asset`, each `amount ≤ planable(source)`.
    pub groups: SmallVec<[FlashRoute; MAX_GROUPS]>,
    /// Σ `groups[i].amount`. `< need` means no ≤ 3-source plan reaches
    /// `need` under the buffer: a partial.
    pub funded: U256,
    /// Effective total cost across groups, debt raw units.
    pub cost: U256,
}

#[derive(Copy, Clone)]
struct Cand<'a> {
    planable: U256,
    entry: &'a SourceEntry,
}

/// Fill `subset` cheapest-first against `need`. `None` when a member would
/// take nothing (that plan is a smaller subset already seen) or a cost
/// overflows.
fn fill(subset: &[Cand<'_>], debt: AssetId, need: U256, m: &CostModel) -> Option<Cascade> {
    let mut remaining = need;
    let mut cost = U256::ZERO;
    let mut groups = SmallVec::new();
    for c in subset {
        let take = c.planable.min(remaining);
        if take.is_zero() {
            return None;
        }
        cost = cost.checked_add(effective_cost(c.entry, take, m)?)?;
        remaining = remaining.checked_sub(take)?;
        groups.push(c.entry.route(debt, take));
    }
    Some(Cascade {
        groups,
        funded: need.checked_sub(remaining)?,
        cost,
    })
}

/// Every non-empty subset of `cands` of size ≤ [`MAX_GROUPS`], each once,
/// in index order (so each subset is already cheapest-first).
fn for_each_subset<'a>(
    cands: &[Cand<'a>],
    pick: &mut SmallVec<[Cand<'a>; MAX_GROUPS]>,
    f: &mut impl FnMut(&[Cand<'a>]),
) {
    let mut rest = cands;
    while let Some((c, tail)) = rest.split_first() {
        pick.push(*c);
        f(pick);
        if pick.len() < MAX_GROUPS {
            for_each_subset(tail, pick, f);
        }
        pick.pop();
        rest = tail;
    }
}

/// More funding, then cheaper, then fewer groups.
fn better_partial(c: &Cascade, b: &Cascade) -> bool {
    c.funded > b.funded
        || (c.funded == b.funded
            && (c.cost < b.cost || (c.cost == b.cost && c.groups.len() < b.groups.len())))
}

/// Plan `need` of `debt`. The cheapest feasible plan, with fewer groups
/// preferred inside the `close_bps` margin; when no ≤ 3-subset reaches
/// `need`, the plan funding the most (`funded < need`). `None` when no
/// source has planable depth or `need == 0`.
#[must_use]
pub fn plan(
    idx: &FlashIndex,
    debt: AssetId,
    need: U256,
    m: &CostModel,
    close_bps: u16,
) -> Option<Cascade> {
    if need.is_zero() {
        return None;
    }
    let cands: SmallVec<[Cand<'_>; MAX_CANDIDATES]> = idx
        .entries(debt)
        .iter()
        .map(|entry| Cand {
            planable: planable(entry.available),
            entry,
        })
        .filter(|c| !c.planable.is_zero())
        .take(MAX_CANDIDATES)
        .collect();

    // Cheapest feasible plan per group count (index = groups − 1), and the
    // best partial.
    let mut feasible: [Option<Cascade>; MAX_GROUPS] = [None, None, None];
    let mut partial: Option<Cascade> = None;
    let mut pick = SmallVec::new();
    for_each_subset(&cands, &mut pick, &mut |subset| {
        let Some(c) = fill(subset, debt, need, m) else {
            return;
        };
        if c.funded < need {
            if partial.as_ref().is_none_or(|b| better_partial(&c, b)) {
                partial = Some(c);
            }
            return;
        }
        let Some(slot) = feasible.get_mut(c.groups.len().wrapping_sub(1)) else {
            return;
        };
        if slot.as_ref().is_none_or(|b| c.cost < b.cost) {
            *slot = Some(c);
        }
    });

    let Some(min_cost) = feasible.iter().flatten().map(|c| c.cost).min() else {
        return partial;
    };
    let margin = mul_div(need, U256::from(close_bps), BPS, Rounding::Down).unwrap_or(U256::ZERO);
    let limit = min_cost.checked_add(margin).unwrap_or(U256::MAX);
    // Fewest groups within the margin; `feasible` is ordered by group count.
    feasible.into_iter().flatten().find(|c| c.cost <= limit)
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use alloy_primitives::U256;
    use liq_types::FlashProvider;
    use std::collections::HashSet;

    use super::{plan, planable, Cascade, MAX_GROUPS};
    use crate::index::fixtures::*;
    use crate::index::FlashIndex;
    use crate::select::CostModel;
    use crate::sources::fixtures::*;

    fn e18(n: u64) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u64)
    }

    fn usdc(m: u64) -> U256 {
        U256::from(m) * U256::from(1_000_000u64)
    }

    /// Structural invariants every plan must satisfy (GUIDE 07 §7, D44):
    /// ≤ 3 groups, flat siblings on one asset, distinct sources, each leg
    /// within its source's planable depth, amounts summing to `funded`.
    fn check_shape(idx: &FlashIndex, c: &Cascade, need: U256) {
        assert!(!c.groups.is_empty() && c.groups.len() <= MAX_GROUPS);
        let mut sources = HashSet::new();
        let mut sum = U256::ZERO;
        for g in &c.groups {
            assert!(sources.insert(g.source), "one group per source");
            let e = idx
                .entries(g.asset)
                .iter()
                .find(|e| e.source == g.source)
                .unwrap();
            assert!(g.amount <= planable(e.available), "leg exceeds 99 % buffer");
            assert!(!g.amount.is_zero());
            assert_eq!(g.callback.provider(), g.provider);
            sum += g.amount;
        }
        assert!(c.groups.iter().all(|g| g.asset == c.groups[0].asset));
        assert_eq!(sum, c.funded);
        assert!(c.funded <= need);
    }

    /// Oracle: hand arithmetic — 99 % of the 26M PoolManager WETH balance
    /// 2_131_150_728_309_835_187_612 is 2_109_839_221_026_736_835_735
    /// (floor of …835.88). Zero stays zero.
    #[test]
    fn planable_is_floor_99_percent() {
        assert_eq!(planable(u256(PM_WETH_26M)), u256("2109839221026736835735"));
        assert_eq!(planable(U256::ZERO), U256::ZERO);
        assert_eq!(planable(U256::from(100u64)), U256::from(99u64));
        assert_eq!(planable(U256::from(1u64)), U256::ZERO);
    }

    /// Oracle: §7 rule 1 — cascade only when the cheapest single source's
    /// planable is short. 50M USDC fits Morpho (0 bps, 107.6M planable) →
    /// one group, zero cost.
    #[test]
    fn single_source_when_cheapest_fits() {
        let idx = index_of(&five());
        let need = usdc(50_000_000);
        let c = plan(&idx, ID_USDC, need, &CostModel::FEE_ONLY, 0).unwrap();
        check_shape(&idx, &c, need);
        assert_eq!(c.groups.len(), 1);
        assert_eq!(c.groups[0].provider, FlashProvider::Morpho);
        assert_eq!(c.funded, need);
        assert_eq!(c.cost, U256::ZERO);
    }

    /// Acceptance fixture (planner half; the fork run is 10C's): 295_000
    /// WETH exceeds every single source at 26M (deepest: aWETH 288_592)
    /// and fits two under the 99 % buffer — Morpho 15_721.52 + Aave
    /// 285_706.28 = 301_427.80. With a 1 % close margin the two-group plan
    /// wins over the 1.05-WETH-cheaper three-group {V4, Morpho, Aave}.
    /// Oracle: Aave `percentMul` on the Aave leg 279_278.478… WETH at
    /// 5 bps = 139_639_239_052_859_622_178 wei (hand computed).
    #[test]
    fn two_sibling_groups_when_no_single_source_fits() {
        let idx = index_of(&five());
        let need = e18(295_000);
        assert!(idx
            .entries(ID_WETH)
            .iter()
            .all(|e| planable(e.available) < need));
        let c = plan(&idx, ID_WETH, need, &CostModel::FEE_ONLY, 100).unwrap();
        check_shape(&idx, &c, need);
        assert_eq!(c.funded, need);
        let order: Vec<_> = c.groups.iter().map(|g| g.provider).collect();
        assert_eq!(order, [FlashProvider::Morpho, FlashProvider::Aave]);
        assert_eq!(c.groups[0].amount, u256("15721521894280755643442"));
        assert_eq!(c.groups[1].amount, u256("279278478105719244356558"));
        assert_eq!(c.groups[0].source, MORPHO);
        assert_eq!(c.groups[1].source, AAVE_POOL);
        assert_eq!(c.cost, u256("139639239052859622178"));
    }

    /// Oracle: same fixture, margin 0 — the strictly cheapest plan is three
    /// groups {V4, Morpho, Aave}: the V4 leg removes 2_109.84 WETH from the
    /// 5-bps Aave leg. Aave leg 277_168.638… → fee
    /// 138_584_319_442_346_253_760 wei (hand computed). Still ≤ 3, flat.
    #[test]
    fn three_groups_when_strictly_cheapest_and_margin_zero() {
        let idx = index_of(&five());
        let need = e18(295_000);
        let c = plan(&idx, ID_WETH, need, &CostModel::FEE_ONLY, 0).unwrap();
        check_shape(&idx, &c, need);
        assert_eq!(c.funded, need);
        let order: Vec<_> = c.groups.iter().map(|g| g.provider).collect();
        assert_eq!(
            order,
            [
                FlashProvider::Morpho,
                FlashProvider::UniV4,
                FlashProvider::Aave
            ]
        );
        assert_eq!(c.cost, u256("138584319442346253760"));
    }

    /// Oracle: the §7 rule-2 deviation on real 26M depths, with the V3
    /// 1-bp USDC/WETH pool indexed as its own row (§3a). Greedy takes the
    /// three cheapest — Morpho and V4 at 0 bps, then V3-100 at 1 bp — and
    /// stops at 18_531.32 WETH, short of 295_000; the subset search finds
    /// {Morpho, Aave} at 301_427.80. Hand arithmetic: 99 % of the 1-bp
    /// pool's 707.031936… WETH is 699.961617… WETH.
    #[test]
    fn greedy_three_cheapest_misses_what_the_subset_search_finds() {
        let mut srcs = five();
        srcs.push(Box::new(crate::UniV3Pool::new(
            UNIV3_USDC_WETH_100,
            USDC,
            WETH,
            ID_USDC,
            ID_WETH,
            100,
            U256::from(V3_100_USDC_26M),
            u256(V3_100_WETH_26M),
        )));
        let idx = index_of(&srcs);
        let need = e18(295_000);

        let row = idx.entries(ID_WETH);
        let fees: Vec<u16> = row.iter().map(|e| e.fee_bps).collect();
        assert_eq!(fees, [0, 0, 1, 5, 5], "cheapest-first row");
        let greedy: U256 = row
            .iter()
            .take(MAX_GROUPS)
            .map(|e| planable(e.available))
            .sum();
        assert_eq!(greedy, u256("18531322732332246087870"));
        assert!(greedy < need, "greedy cheapest-first cannot fund 295k");

        let c = plan(&idx, ID_WETH, need, &CostModel::FEE_ONLY, 100).unwrap();
        check_shape(&idx, &c, need);
        assert_eq!(c.funded, need, "the subset search does");
        let order: Vec<_> = c.groups.iter().map(|g| g.provider).collect();
        assert_eq!(order, [FlashProvider::Morpho, FlashProvider::Aave]);
        assert_eq!(
            c.groups[0].amount + c.groups[1].amount,
            need,
            "two sibling groups, flat"
        );
    }

    /// Oracle: the planner's subset search finds a full plan whenever the
    /// three deepest planables cover need (invariant: max coverage of any
    /// ≤ 3-subset is the top-3 by depth).
    #[test]
    fn planner_funds_whenever_top_three_depths_cover_need() {
        use proptest::prelude::*;
        let idx = index_of(&five());
        let mut depths: Vec<U256> = idx
            .entries(ID_WETH)
            .iter()
            .map(|e| planable(e.available))
            .collect();
        depths.sort_unstable_by(|a, b| b.cmp(a));
        let top3: U256 = depths.iter().take(3).copied().sum();
        assert_eq!(top3, u256("313470100496310349389888"), "Aave + Morpho + V3");

        let cfg = ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        };
        proptest!(cfg, |(need_weth in 1u64..=320_000u64, close in 0u16..=10_000u16)| {
            let need = e18(need_weth);
            let c = plan(&idx, ID_WETH, need, &CostModel::FEE_ONLY, close).unwrap();
            check_shape(&idx, &c, need);
            prop_assert_eq!(c.funded == need, need <= top3);
            if need > top3 {
                prop_assert_eq!(c.funded, top3, "partial funds the most any ≤3 subset can");
                prop_assert_eq!(c.groups.len(), 3);
            }
        });
    }

    /// Oracle: hand sums at 26M — the three deepest USDC planables (Aave
    /// 179_830.08M + Morpho 107_595.52M + V3 73_550.89M) total
    /// 360_976_486_321_827; need above that is a partial with exactly
    /// that funding, cheapest among equal-funding subsets (Morpho free
    /// leg kept, so cost is the 5-bps legs only).
    #[test]
    fn partial_when_need_exceeds_any_three_sources() {
        let idx = index_of(&five());
        let need = usdc(400_000_000);
        let c = plan(&idx, ID_USDC, need, &CostModel::FEE_ONLY, 0).unwrap();
        check_shape(&idx, &c, need);
        assert!(c.funded < need);
        assert_eq!(c.funded, U256::from(360_976_486_321_827u64));
        assert_eq!(c.groups.len(), 3);
        let providers: HashSet<_> = c.groups.iter().map(|g| g.provider).collect();
        assert_eq!(
            providers,
            HashSet::from([
                FlashProvider::Aave,
                FlashProvider::Morpho,
                FlashProvider::UniV3
            ])
        );
    }

    /// Oracle: GUIDE 07 §4b — nothing to plan is `None`, never a panic.
    #[test]
    fn none_without_depth_or_need() {
        let idx = index_of(&five());
        assert_eq!(plan(&idx, NO_DEPTH, usdc(1), &CostModel::FEE_ONLY, 0), None);
        assert_eq!(
            plan(&idx, ID_USDC, U256::ZERO, &CostModel::FEE_ONLY, 0),
            None
        );
        assert_eq!(
            plan(
                &FlashIndex::new(0),
                ID_USDC,
                usdc(1),
                &CostModel::FEE_ONLY,
                0
            ),
            None
        );
        // Sky DSS at 1 wei planable = 0 (floor) → no candidate.
        let mut srcs = vec![dss()];
        let topics = [
            T0_FILE,
            alloy_primitives::b256!(
                "0x6d61780000000000000000000000000000000000000000000000000000000000"
            ),
        ];
        let data = u256_be(U256::from(1u64));
        srcs[0].apply_log(&log(DSS_FLASH, &topics, &data));
        let idx = index_of(&srcs);
        assert_eq!(idx.available(ID_DAI), U256::from(1u64));
        assert_eq!(
            plan(&idx, ID_DAI, U256::from(1u64), &CostModel::FEE_ONLY, 0),
            None
        );
    }

    /// Oracle: §7 rule 5 — with the fixed per-group costs of a real gas
    /// price, the two-group plan is cheaper than three once the V4 leg's
    /// saving (5 bps of 2_109.84 WETH ≈ 1.055 WETH) is below one extra
    /// group's overhead. Premium 1 gas at 2 WETH/gas (an exaggerated unit
    /// price to keep the arithmetic exact) → three groups pay 6 WETH of
    /// overhead vs two paying 4: the two-group plan wins at margin 0.
    #[test]
    fn per_group_overhead_favours_fewer_groups() {
        let idx = index_of(&five());
        let need = e18(295_000);
        let mut m = CostModel::FEE_ONLY;
        m.gas_price_in_debt = e18(2);
        m.failure_premium_gas = [1; 5];
        let c = plan(&idx, ID_WETH, need, &m, 0).unwrap();
        check_shape(&idx, &c, need);
        assert_eq!(c.groups.len(), 2);
        assert_eq!(c.cost, u256("139639239052859622178") + e18(4));
    }
}
