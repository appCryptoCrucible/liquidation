//! Eligibility (GUIDE 07 §5): a position is a target only if **both**
//! market-facing legs work — flash-fundable debt (leg 1, this crate) and a
//! collateral exit (leg 3, `RouteCache`). Every `repay × seize` pair is
//! checked; the winner maximises `bonus − cost` (D26).
//!
//! **Deviation — return type.** `is_eligible` returns the winning
//! [`LegChoice`] with the [`FlashRoute`]: `Protocol::encode` needs both, and
//! the seize choice is exactly what D26 selects. Returning the route alone
//! would discard it.
//!
//! **D26 key.** Exit cost is the router's term and the one-method
//! `RouteCache` cannot supply it (12A-1). The key is therefore
//! `bonus − flash_fee`, both first-order ratios of the position's notional;
//! `exit_cost` joins the key when 12A-1 lands.
//!
//! **Marking, never deleting.** [`Eligibility`] is a bitset over the
//! position universe; a position that fails is cleared and stays tracked
//! (`Band::Unfundable`, GUIDE 08) so the bit flips back when liquidity
//! returns.

use alloy_primitives::{uint, U256};
use fixedbitset::FixedBitSet;
use liq_protocol::{FlashRoute, LegChoice, Quote, RouteCache};
use liq_types::{AssetId, FlashProvider, PositionId};
use smallvec::SmallVec;

use crate::index::{FlashIndex, Haircut};

/// Interim `RouteCache` (GUIDE 07 §5 intro): `has_exit(coll, amount)` is
/// `true` when a single **inventory-backed** flash source holds ≥ `amount`
/// of `coll`. Sky DSS is excluded — its depth is a governance ceiling, not
/// tokens anyone is quoting.
///
/// **Stub, replaced by 12A-1.** It is conservative in one direction only:
/// an asset no flash source holds is rejected (safe). It can over-admit on
/// size — a lending pool's aToken balance is evidence that inventory exists,
/// not that it can be swapped at acceptable slippage. Re-evaluation on
/// route-depth change is therefore also 12A-1's.
pub struct DepthOnlyRouteCache<'a>(pub &'a FlashIndex);

impl RouteCache for DepthOnlyRouteCache<'_> {
    #[inline]
    fn has_exit(&self, coll: AssetId, amount: U256) -> bool {
        self.0
            .entries(coll)
            .iter()
            .any(|e| e.provider != FlashProvider::SkyDss && e.available >= amount)
    }
}

/// `RAY / 10_000 = 1e23`: one basis point in RAY.
const RAY_PER_BP: U256 = uint!(100_000_000_000_000_000_000_000_U256);

/// `bonus − fee` in RAY, saturating at zero. Bonus is per-seize (D26); the
/// flash fee is the only cost the index can price today.
#[inline]
fn net_of_bonus(bonus: U256, fee_bps: u16) -> U256 {
    let fee = U256::from(fee_bps)
        .checked_mul(RAY_PER_BP)
        .unwrap_or(U256::MAX);
    bonus.saturating_sub(fee)
}

/// Two-sided eligibility over every `(repay, seize)` pair. `None` when no
/// pair has both a haircut-fundable debt route and a collateral exit.
/// Ties keep the quote's preference order (`LegChoice::PREFERRED` first).
#[must_use]
pub fn is_eligible(
    q: &Quote,
    idx: &FlashIndex,
    routes: &dyn RouteCache,
    haircut: Haircut,
) -> Option<(LegChoice, FlashRoute)> {
    // The debt route does not depend on the seize leg: price each repay
    // option once, not once per pair.
    let routed: SmallVec<[(usize, FlashRoute); 4]> = q
        .repay_options
        .iter()
        .enumerate()
        .filter_map(|(ri, repay)| {
            Some((ri, idx.best_route(repay.asset, repay.max_repay, haircut)?))
        })
        .collect();
    if routed.is_empty() {
        return None;
    }
    let mut best: Option<(U256, LegChoice, FlashRoute)> = None;
    for (si, seize) in q.seize_options.iter().enumerate() {
        if seize.max_seize.is_zero() || !routes.has_exit(seize.asset, seize.max_seize) {
            continue;
        }
        for &(ri, route) in &routed {
            let key = net_of_bonus(seize.bonus.raw(), route.fee_bps);
            if best.as_ref().is_none_or(|(k, _, _)| key > *k) {
                let (Ok(repay), Ok(seize)) = (u8::try_from(ri), u8::try_from(si)) else {
                    continue;
                };
                best = Some((key, LegChoice { repay, seize }, route));
            }
        }
    }
    best.map(|(_, legs, route)| (legs, route))
}

/// Fundable bitset over the position universe (GUIDE 07 §4b). The
/// `Unfundable` check is one load and a mask; clearing a bit marks, never
/// deletes.
#[derive(Clone, Debug)]
pub struct Eligibility {
    fundable: FixedBitSet,
}

impl Eligibility {
    /// Sized for the full universe at startup; grows if a later position
    /// id exceeds it.
    #[must_use]
    pub fn new(positions: usize) -> Self {
        Self {
            fundable: FixedBitSet::with_capacity(positions),
        }
    }

    /// Tracked capacity in positions (set and clear bits alike).
    #[must_use]
    pub fn len(&self) -> usize {
        self.fundable.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fundable.is_empty()
    }

    /// Re-evaluate `q.position` and record the verdict. Call when flash
    /// liquidity moved materially or the quote changed.
    pub fn evaluate(
        &mut self,
        q: &Quote,
        idx: &FlashIndex,
        routes: &dyn RouteCache,
        haircut: Haircut,
    ) -> Option<(LegChoice, FlashRoute)> {
        let r = is_eligible(q, idx, routes, haircut);
        self.mark(q.position, r.is_some());
        r
    }

    /// Set or clear one position's bit. Never panics: out-of-range grows on
    /// set and is already clear on clear.
    pub fn mark(&mut self, pos: PositionId, fundable: bool) {
        let Ok(i) = usize::try_from(pos.0) else {
            return;
        };
        if fundable {
            self.fundable.grow_and_insert(i);
        } else if i < self.fundable.len() {
            self.fundable.remove(i);
        }
    }

    /// One load and mask. Unknown positions are not fundable.
    #[inline]
    #[must_use]
    pub fn is_fundable(&self, pos: PositionId) -> bool {
        usize::try_from(pos.0).is_ok_and(|i| self.fundable.contains(i))
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
    use liq_protocol::{BonusCurve, LegChoice, Quote, RepayOption, RouteCache, SeizeOption};
    use liq_types::fixed::RAY;
    use liq_types::{AssetId, FlashProvider, MarketId, PositionId, PositionKey, ProtocolId, Ray};

    use super::{is_eligible, DepthOnlyRouteCache, Eligibility, RAY_PER_BP};
    use crate::index::fixtures::*;
    use crate::index::Haircut;
    use crate::sources::fixtures::*;
    use crate::FlashSource;

    fn bps(n: u64) -> Ray {
        Ray::from_raw(RAY * U256::from(n) / U256::from(10_000u64))
    }

    fn repay(asset: AssetId, max_repay: U256) -> RepayOption {
        RepayOption {
            asset,
            max_repay,
            slot: liq_protocol::SlotRef::ByAsset,
        }
    }

    fn seize(asset: AssetId, max_seize: U256, bonus_bps: u64) -> SeizeOption {
        SeizeOption {
            asset,
            max_seize,
            bonus: bps(bonus_bps),
            curve: BonusCurve::Static {
                bonus: bps(bonus_bps),
            },
            call_target: alloy_primitives::Address::ZERO,
            slot: liq_protocol::SlotRef::ByAsset,
        }
    }

    fn quote(pos: u32, repays: &[RepayOption], seizes: &[SeizeOption]) -> Quote {
        Quote {
            position: PositionId(pos),
            key: PositionKey {
                protocol: ProtocolId(0),
                market: MarketId(0),
                user: Address::ZERO,
            },
            repay_options: repays.iter().copied().collect(),
            seize_options: seizes.iter().copied().collect(),
        }
    }

    fn usdc(m: u64) -> U256 {
        U256::from(m) * U256::from(1_000_000u64)
    }

    fn weth(n: u64) -> U256 {
        U256::from(n) * U256::from(1_000_000_000_000_000_000u64)
    }

    const H90: Haircut = match Haircut::from_bps(9_000) {
        Some(h) => h,
        None => unreachable!(),
    };

    /// Oracle: `RAY / 10_000 = 1e23`, computed here from the published
    /// constant, not from the limb literal under test.
    #[test]
    fn ray_per_bp_is_1e23() {
        assert_eq!(RAY_PER_BP, RAY / U256::from(10_000u64));
    }

    /// Oracle: chain depth at 26M — the deepest inventory-backed WETH
    /// source is aWETH (288_592.198… WETH). Exactly that exits; one wei
    /// more does not; an asset no source holds never does.
    #[test]
    fn depth_only_admits_up_to_inventory_depth() {
        let idx = index_of(&five());
        let rc = DepthOnlyRouteCache(&idx);
        let depth = u256(AWETH_WETH_26M);
        assert!(rc.has_exit(ID_WETH, depth));
        assert!(!rc.has_exit(ID_WETH, depth + U256::from(1u64)));
        assert!(!rc.has_exit(NO_DEPTH, U256::from(1u64)));
        assert!(!rc.has_exit(ID_NATIVE, U256::from(1u64)));
    }

    /// Oracle: GUIDE 07 §3 — the DSS `max` is a governance ceiling, not
    /// inventory. DAI held only by Sky DSS has flash depth but no exit.
    #[test]
    fn depth_only_excludes_governance_ceiling() {
        let idx = index_of(&five());
        assert_eq!(
            idx.available(ID_DAI),
            u256(DSS_MAX_26M),
            "flash side sees it"
        );
        assert!(
            !DepthOnlyRouteCache(&idx).has_exit(ID_DAI, U256::from(1u64)),
            "exit side does not"
        );
    }

    /// Acceptance: a three-debt position fundable only on its second
    /// option. Sources: V4 PoolManager, Morpho, Sky DSS at 26M. Debt sizes
    /// are proptest-drawn against the real haircut depths — WETH above
    /// 90 % of Morpho's 15_880.3 WETH (the deepest WETH here), USDC within
    /// 90 % of Morpho's 108.68M, DAI above 90 % of the 500M DSS ceiling.
    /// Negative: checking only `repay_options[0]` finds nothing.
    #[test]
    fn three_debt_position_fundable_only_on_second_option() {
        use proptest::prelude::*;
        let srcs = vec![v4(), morpho(), dss()];
        let idx = index_of(&srcs);
        let rc = DepthOnlyRouteCache(&idx);
        let weth_lim = 14_292u64; // floor(0.9 · 15_880.325) WETH
        let usdc_lim = 97_814_105u64; // floor(0.9 · 108_682_339.58) USDC
        let dai_lim = 450_000_000u64; // 0.9 · 500M DAI
        let cfg = ProptestConfig {
            cases: 128,
            ..ProptestConfig::default()
        };
        proptest!(cfg, |(
            w in (weth_lim + 1)..=(weth_lim * 4),
            u in 1u64..=usdc_lim,
            d in (dai_lim + 1)..=(dai_lim * 4),
            c in 1u64..=2_131u64, // ≤ PoolManager WETH (2_131.15) so the exit holds
        )| {
            let q = quote(
                7,
                &[repay(ID_WETH, weth(w)), repay(ID_USDC, usdc(u)), repay(ID_DAI, weth(d))],
                &[seize(ID_WETH, weth(c), 500)],
            );
            let (legs, route) = is_eligible(&q, &idx, &rc, H90).unwrap();
            prop_assert_eq!(legs, LegChoice { repay: 1, seize: 0 });
            prop_assert_eq!(route.asset, ID_USDC);
            prop_assert_eq!(route.provider, FlashProvider::Morpho);
            prop_assert_eq!(route.amount, usdc(u));
            prop_assert_eq!(idx.best_route(ID_WETH, weth(w), H90), None);
            prop_assert_eq!(idx.best_route(ID_DAI, weth(d), H90), None);
        });
    }

    /// Acceptance: two-sided. Fully flash-fundable USDC debt with
    /// collateral no source holds → `Unfundable`. Control: the same debt
    /// with WETH collateral is eligible, so the collateral side is what
    /// failed.
    #[test]
    fn flash_fundable_debt_with_no_exit_collateral_is_unfundable() {
        let idx = index_of(&five());
        let rc = DepthOnlyRouteCache(&idx);
        let debt = [repay(ID_USDC, usdc(10_000_000))];
        assert!(idx.best_route(ID_USDC, usdc(10_000_000), H90).is_some());
        let mut el = Eligibility::new(16);
        let q = quote(3, &debt, &[seize(NO_DEPTH, weth(1), 500)]);
        assert_eq!(el.evaluate(&q, &idx, &rc, H90), None);
        assert!(!el.is_fundable(PositionId(3)));
        let q = quote(3, &debt, &[seize(ID_WETH, weth(1), 500)]);
        assert!(el.evaluate(&q, &idx, &rc, H90).is_some());
        assert!(el.is_fundable(PositionId(3)));
    }

    /// Acceptance: marked, not deleted, flips back. One source (V4
    /// PoolManager). Drain it by `Transfer` → position cleared but still
    /// tracked (`len` unchanged); credit it back → fundable again.
    #[test]
    fn unfundable_is_marked_and_flips_back_when_liquidity_returns() {
        let mut srcs: Vec<Box<dyn FlashSource>> = vec![v4()];
        let mut idx = index_of(&srcs);
        let mut el = Eligibility::new(64);
        let q = quote(
            9,
            &[repay(ID_USDC, usdc(10_000_000))],
            &[seize(ID_WETH, weth(1), 500)],
        );
        assert!(el
            .evaluate(&q, &idx, &DepthOnlyRouteCache(&idx), H90)
            .is_some());
        assert!(el.is_fundable(PositionId(9)));

        let out = transfer_topics(POOL_MANAGER, Address::ZERO);
        let data = u256_be(U256::from(PM_USDC_26M));
        srcs[0].apply_log(&log(USDC, &out, &data));
        idx.refresh(&srcs);
        assert_eq!(idx.available(ID_USDC), U256::ZERO);
        assert_eq!(el.evaluate(&q, &idx, &DepthOnlyRouteCache(&idx), H90), None);
        assert!(!el.is_fundable(PositionId(9)));
        assert_eq!(el.len(), 64, "marked, not deleted");

        let back = transfer_topics(Address::ZERO, POOL_MANAGER);
        srcs[0].apply_log(&log(USDC, &back, &data));
        idx.refresh(&srcs);
        let (legs, route) = el
            .evaluate(&q, &idx, &DepthOnlyRouteCache(&idx), H90)
            .unwrap();
        assert_eq!(legs, LegChoice::PREFERRED);
        assert_eq!(route.provider, FlashProvider::UniV4);
        assert!(el.is_fundable(PositionId(9)));
    }

    /// Oracle: D26 — maximise `bonus − cost`. Seize `[1]` pays 8 % against
    /// `[0]`'s 5 % → seize 1. Repay `[1]` (WETH, Morpho, 0 bps) beats
    /// repay `[0]` (USDC, Aave only, 5 bps) at equal bonus → repay 1.
    #[test]
    fn picks_highest_bonus_minus_fee_pair() {
        let idx = index_of(&five());
        let rc = DepthOnlyRouteCache(&idx);
        let q = quote(
            1,
            &[repay(ID_USDC, usdc(1_000_000))],
            &[seize(ID_WETH, weth(1), 500), seize(ID_USDC, usdc(1), 800)],
        );
        let (legs, _) = is_eligible(&q, &idx, &rc, H90).unwrap();
        assert_eq!(legs, LegChoice { repay: 0, seize: 1 });

        // USDC only on Aave (5 bps) vs WETH only on Morpho (0 bps).
        let aave_usdc = crate::AavePool::new(
            AAVE_POOL,
            AAVE_CONFIGURATOR,
            5,
            &[crate::AaveReserve {
                asset: ID_USDC,
                underlying: USDC,
                atoken: A_USDC,
                balance: U256::from(AUSDC_USDC_26M),
                flash_enabled: true,
                active: true,
                paused: false,
            }],
        );
        let srcs: Vec<Box<dyn FlashSource>> = vec![
            Box::new(aave_usdc),
            Box::new(crate::MorphoBlue::new(
                MORPHO,
                &[crate::HeldAsset {
                    asset: ID_WETH,
                    token: WETH,
                    balance: u256(MORPHO_WETH_26M),
                }],
            )),
        ];
        let idx = index_of(&srcs);
        let rc = DepthOnlyRouteCache(&idx);
        let q = quote(
            1,
            &[repay(ID_USDC, usdc(1_000_000)), repay(ID_WETH, weth(100))],
            &[seize(ID_WETH, weth(1), 500)],
        );
        let (legs, route) = is_eligible(&q, &idx, &rc, H90).unwrap();
        assert_eq!(legs, LegChoice { repay: 1, seize: 0 });
        assert_eq!(route.fee_bps, 0);
        assert_eq!(route.provider, FlashProvider::Morpho);
    }

    /// Oracle: `Quote` doc — option lists are ordered by preference, so an
    /// exact tie keeps `LegChoice::PREFERRED`. Zero-size seize legs are
    /// skipped (nothing to exit); a quote with no options is ineligible.
    #[test]
    fn ties_keep_preferred_and_zero_seize_is_skipped() {
        let idx = index_of(&five());
        let rc = DepthOnlyRouteCache(&idx);
        let q = quote(
            2,
            &[repay(ID_USDC, usdc(1_000)), repay(ID_WETH, weth(1))],
            &[seize(ID_WETH, weth(1), 500), seize(ID_USDC, usdc(1), 500)],
        );
        let (legs, _) = is_eligible(&q, &idx, &rc, H90).unwrap();
        assert_eq!(legs, LegChoice::PREFERRED);

        let q = quote(
            2,
            &[repay(ID_USDC, usdc(1_000))],
            &[
                seize(ID_WETH, U256::ZERO, 500),
                seize(ID_USDC, usdc(1), 100),
            ],
        );
        let (legs, _) = is_eligible(&q, &idx, &rc, H90).unwrap();
        assert_eq!(legs, LegChoice { repay: 0, seize: 1 });
        assert_eq!(is_eligible(&quote(2, &[], &[]), &idx, &rc, H90), None);
    }

    /// Oracle: GUIDE 07 §4b — no panic on any position id; unknown ids are
    /// not fundable; setting grows, clearing out of range is a no-op.
    #[test]
    fn bitset_never_panics_and_grows_on_set() {
        let mut el = Eligibility::new(8);
        assert_eq!(el.len(), 8);
        assert!(!el.is_empty());
        assert!(!el.is_fundable(PositionId(u32::MAX)));
        el.mark(PositionId(1_000), false);
        assert_eq!(el.len(), 8);
        el.mark(PositionId(1_000), true);
        assert!(el.is_fundable(PositionId(1_000)));
        assert!(el.len() > 1_000);
        el.mark(PositionId(1_000), false);
        assert!(!el.is_fundable(PositionId(1_000)));
    }
}
