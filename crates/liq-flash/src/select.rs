//! Source selection (GUIDE 07 §6): rank by **effective total cost**, keep a
//! fallback chain.
//!
//! ```text
//! effective_cost = fee_amount(notional)
//!                + (gas_overhead + failure_premium_gas) · gas_price_in_debt
//! ```
//!
//! All terms are in the debt asset's raw units. The guide writes
//! `gas_overhead · gas_price`; a wei gas price cannot be added to a token
//! amount, so the caller (12A-2, which holds the price vector) converts once
//! into `gas_price_in_debt`. `gas_overhead` is 07A's stub `0` until 10C
//! measures it; `failure_premium_gas` starts at `0`.
//!
//! The re-encode loop that walks the chain on a simulation liquidity
//! failure is 12A-2's; this module supplies the ordered chain.

use alloy_primitives::U256;
use liq_protocol::FlashRoute;
use liq_types::fixed::{mul_div, Rounding};
use liq_types::{AssetId, FlashProvider};
use smallvec::SmallVec;

use crate::index::{FlashIndex, Haircut, SourceEntry, BPS};

/// Aave `PercentageMath.HALF_PERCENTAGE_FACTOR`.
const HALF_BPS: U256 = U256::from_limbs([5_000, 0, 0, 0]);

/// Cost parameters outside the index. Per debt asset, since both fields
/// are in that asset's raw units or gas.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CostModel {
    /// Debt-asset raw units per unit of gas: `gas_price_wei · price(ETH in
    /// debt)`, converted by the caller.
    pub gas_price_in_debt: U256,
    /// Expected gas wasted per attempt because the source was drained
    /// before inclusion, indexed by `FlashProvider as usize`. **Config, not
    /// a measurement made here.** Tuning signal: the provider's
    /// `InsufficientLiquidity` simulation-failure rate (GUIDE 11) times
    /// the revert's gas. Starts at `0` for every provider.
    pub failure_premium_gas: [u64; 5],
}

impl CostModel {
    /// Fee-only ranking: zero gas price, zero premiums.
    pub const FEE_ONLY: Self = Self {
        gas_price_in_debt: U256::ZERO,
        failure_premium_gas: [0; 5],
    };

    #[inline]
    fn premium(&self, p: FlashProvider) -> Option<u64> {
        self.failure_premium_gas.get(p as usize).copied()
    }
}

/// Absolute flash fee on `amount`, rounded exactly as the provider's
/// contract rounds. `None` when it cannot be priced (overflow, or a fee on
/// a source the guide defines as fee-free — a bug upstream, fail closed).
#[must_use]
pub fn fee_amount(provider: FlashProvider, amount: U256, fee_bps: u16) -> Option<U256> {
    if fee_bps == 0 {
        return Some(U256::ZERO);
    }
    let bps = U256::from(fee_bps);
    match provider {
        // PercentageMath.percentMul: (amount · bps + 5_000) / 10_000, half up.
        FlashProvider::Aave => amount
            .checked_mul(bps)?
            .checked_add(HALF_BPS)?
            .checked_div(BPS),
        // FullMath.mulDivRoundingUp(amount, fee, 1e6). `fee = 100 · fee_bps`
        // exactly for every tier (07A: fee_bps = fee / 100), so
        // ceil(amount · fee / 1e6) ≡ ceil(amount · fee_bps / 1e4).
        FlashProvider::UniV3 => mul_div(amount, bps, BPS, Rounding::Up).ok(),
        // `toll · amount / WAD`, floor. 07A rounded `fee_bps` up from `toll`,
        // so this bounds the contract's fee from above.
        FlashProvider::SkyDss => mul_div(amount, bps, BPS, Rounding::Down).ok(),
        // Fee-free by construction (GUIDE 07 §3).
        FlashProvider::UniV4 | FlashProvider::Morpho => None,
    }
}

/// Effective total cost of borrowing `notional` from `e`, in debt raw
/// units. `None` on overflow or an unpriceable fee.
#[must_use]
pub fn effective_cost(e: &SourceEntry, notional: U256, m: &CostModel) -> Option<U256> {
    let fee = fee_amount(e.provider, notional, e.fee_bps)?;
    let gas = e.gas_overhead.checked_add(m.premium(e.provider)?)?;
    fee.checked_add(U256::from(gas).checked_mul(m.gas_price_in_debt)?)
}

/// Every source able to fund `need` of `debt` under `haircut`, cheapest
/// first. `[0]` is the primary; the rest is the fallback chain 12A-2
/// re-encodes against on a liquidity sim failure. Stable on cost ties, so
/// the index order (deeper first) is kept. Empty for `need == 0`.
#[must_use]
pub fn fallback_chain(
    idx: &FlashIndex,
    debt: AssetId,
    need: U256,
    haircut: Haircut,
    m: &CostModel,
) -> SmallVec<[FlashRoute; 6]> {
    if need.is_zero() {
        return SmallVec::new();
    }
    let Some(depth) = haircut.gross_up(need) else {
        return SmallVec::new();
    };
    let mut ranked: SmallVec<[(U256, FlashRoute); 6]> = idx
        .entries(debt)
        .iter()
        .filter(|e| e.available >= depth)
        .filter_map(|e| Some((effective_cost(e, need, m)?, e.route(debt, need))))
        .collect();
    ranked.sort_by_key(|&(cost, _)| cost);
    ranked.into_iter().map(|(_, r)| r).collect()
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
    use liq_types::fixed::{mul_div, Rounding};
    use liq_types::FlashProvider;

    use super::{effective_cost, fallback_chain, fee_amount, CostModel};
    use crate::index::fixtures::*;
    use crate::index::{Haircut, SourceEntry};
    use crate::sources::fixtures::*;
    use crate::{AavePool, AaveReserve, FlashSource, HeldAsset, UniV4PoolManager};

    const H90: Haircut = match Haircut::from_bps(9_000) {
        Some(h) => h,
        None => unreachable!(),
    };

    fn usdc(m: u64) -> U256 {
        U256::from(m) * U256::from(1_000_000u64)
    }

    /// Acceptance: V4 over Aave at equal depth. Aave is seeded with the
    /// PoolManager's real 26M USDC balance so depth is identical; only the
    /// fee differs (0 vs 5 bps). Oracle: GUIDE 07 §6 — lower effective cost.
    #[test]
    fn v4_over_aave_at_equal_depth() {
        let srcs: Vec<Box<dyn FlashSource>> = vec![
            Box::new(AavePool::new(
                AAVE_POOL,
                AAVE_CONFIGURATOR,
                5,
                &[AaveReserve {
                    asset: ID_USDC,
                    underlying: USDC,
                    atoken: A_USDC,
                    balance: U256::from(PM_USDC_26M),
                    flash_enabled: true,
                    active: true,
                    paused: false,
                }],
            )),
            Box::new(UniV4PoolManager::new(
                POOL_MANAGER,
                &[HeldAsset {
                    asset: ID_USDC,
                    token: USDC,
                    balance: U256::from(PM_USDC_26M),
                }],
            )),
        ];
        let idx = index_of(&srcs);
        let chain = fallback_chain(&idx, ID_USDC, usdc(50_000_000), H90, &CostModel::FEE_ONLY);
        let order: Vec<_> = chain.iter().map(|r| r.provider).collect();
        assert_eq!(order, [FlashProvider::UniV4, FlashProvider::Aave]);
        assert_eq!(chain[0].source, POOL_MANAGER);
        assert_eq!(chain[0].fee_bps, 0);
        assert_eq!(chain[1].fee_bps, 5);
    }

    /// Acceptance: Aave over V4 when V4 is shallow. Real 26M depths: V4
    /// holds 66.23M USDC (59.61M after the 90 % haircut), Aave 181.65M.
    /// 100M fits only Aave, so the chain is `[Aave]` — V4 is not even a
    /// fallback.
    #[test]
    fn aave_over_shallow_v4() {
        let srcs: Vec<Box<dyn FlashSource>> = vec![aave(), v4()];
        let idx = index_of(&srcs);
        let chain = fallback_chain(&idx, ID_USDC, usdc(100_000_000), H90, &CostModel::FEE_ONLY);
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider, FlashProvider::Aave);
        assert_eq!(chain[0].amount, usdc(100_000_000));
    }

    /// Oracle: chain depths at 26M and GUIDE 07 §6 ordering — all four
    /// USDC sources fund 50M under a 90 % haircut; fee-free first (deeper
    /// first within a tier: Morpho, V4), then 5 bps (Aave, V3). None fund
    /// 170M. Zero need → empty.
    #[test]
    fn chain_lists_every_fitting_source_cheapest_first() {
        let idx = index_of(&five());
        let chain = fallback_chain(&idx, ID_USDC, usdc(50_000_000), H90, &CostModel::FEE_ONLY);
        let order: Vec<_> = chain.iter().map(|r| r.provider).collect();
        assert_eq!(
            order,
            [
                FlashProvider::Morpho,
                FlashProvider::UniV4,
                FlashProvider::Aave,
                FlashProvider::UniV3
            ]
        );
        assert!(chain
            .iter()
            .all(|r| r.asset == ID_USDC && r.amount == usdc(50_000_000)));
        assert!(
            fallback_chain(&idx, ID_USDC, usdc(170_000_000), H90, &CostModel::FEE_ONLY).is_empty()
        );
        assert!(fallback_chain(&idx, ID_USDC, U256::ZERO, H90, &CostModel::FEE_ONLY).is_empty());
        assert!(fallback_chain(&idx, NO_DEPTH, usdc(1), H90, &CostModel::FEE_ONLY).is_empty());
    }

    /// Oracle: Aave `PercentageMath.percentMul` = `(v·p + 5000) / 10000`
    /// by hand at 5 bps: 1_000_000 → 500; 1_001_000 → 501 (5_010_000 /
    /// 10_000); 998_999 → 499 (4_999_995 / 10_000 floors). The last case
    /// separates half-up from ceil: V3 gives 500 there.
    #[test]
    fn aave_fee_rounds_half_up() {
        let f = |v: u64| fee_amount(FlashProvider::Aave, U256::from(v), 5).unwrap();
        assert_eq!(f(1_000_000), U256::from(500u64));
        assert_eq!(f(1_001_000), U256::from(501u64));
        assert_eq!(f(998_999), U256::from(499u64));
        assert_eq!(
            fee_amount(FlashProvider::UniV3, U256::from(998_999u64), 5).unwrap(),
            U256::from(500u64)
        );
        assert_eq!(
            fee_amount(FlashProvider::Aave, usdc(50_000_000), 5).unwrap(),
            U256::from(25_000_000_000u64),
            "50M USDC at 5 bps is 25_000 USDC"
        );
        assert_eq!(
            fee_amount(FlashProvider::Aave, U256::MAX, 5),
            None,
            "overflow → None"
        );
    }

    /// Oracle: an independent implementation — the V3 contract's own
    /// formula `mulDivRoundingUp(amount, fee, 1e6)` with the pool's `fee`
    /// in pips, against our bps form, for every tier and random amounts.
    /// Hand case: 1_000_001 at 500 → ceil(500.0005) = 501.
    #[test]
    fn univ3_fee_matches_full_math_rounding_up() {
        use proptest::prelude::*;
        assert_eq!(
            fee_amount(FlashProvider::UniV3, U256::from(1_000_001u64), 5).unwrap(),
            U256::from(501u64)
        );
        let cfg = ProptestConfig {
            cases: 512,
            ..ProptestConfig::default()
        };
        proptest!(cfg, |(amount in 1u128..=u128::MAX, tier in prop::sample::select(vec![100u32, 500, 3_000, 10_000]))| {
            let contract = mul_div(
                U256::from(amount),
                U256::from(tier),
                U256::from(1_000_000u64),
                Rounding::Up,
            )
            .unwrap();
            let ours = fee_amount(FlashProvider::UniV3, U256::from(amount), u16::try_from(tier / 100).unwrap()).unwrap();
            prop_assert_eq!(ours, contract);
        });
    }

    /// Oracle: GUIDE 07 §3 — V4 / Morpho are fee-free; Sky DSS `flashFee`
    /// is 0 today. A non-zero fee on a fee-free source is unpriceable.
    #[test]
    fn fee_free_sources_and_fail_closed() {
        for p in [
            FlashProvider::UniV4,
            FlashProvider::Morpho,
            FlashProvider::SkyDss,
        ] {
            assert_eq!(fee_amount(p, U256::MAX, 0), Some(U256::ZERO));
        }
        assert_eq!(fee_amount(FlashProvider::UniV4, usdc(1), 1), None);
        assert_eq!(fee_amount(FlashProvider::Morpho, usdc(1), 1), None);
        // DSS: floor(amount · bps / 1e4); 1e18 at 1 bp = 1e14.
        assert_eq!(
            fee_amount(
                FlashProvider::SkyDss,
                U256::from(1_000_000_000_000_000_000u64),
                1
            ),
            Some(U256::from(100_000_000_000_000u64))
        );
    }

    /// Oracle: GUIDE 07 §6 formula by hand. Aave 50M USDC at 5 bps →
    /// 25_000 USDC fee; gas (0 + 120_000 premium) at 2 raw units/gas →
    /// 240_000 raw; total 25_000_240_000. A model with zero gas price
    /// reduces to the fee. Missing premium slot is unreachable (5 slots,
    /// 5 providers) but `None` rather than a panic.
    #[test]
    fn effective_cost_adds_fee_gas_and_premium() {
        let idx = index_of(&five());
        let aave: &SourceEntry = idx
            .entries(ID_USDC)
            .iter()
            .find(|e| e.provider == FlashProvider::Aave)
            .unwrap();
        let mut m = CostModel::FEE_ONLY;
        assert_eq!(
            effective_cost(aave, usdc(50_000_000), &m),
            Some(U256::from(25_000_000_000u64))
        );
        m.gas_price_in_debt = U256::from(2u64);
        m.failure_premium_gas[FlashProvider::Aave as usize] = 120_000;
        assert_eq!(
            effective_cost(aave, usdc(50_000_000), &m),
            Some(U256::from(25_000_240_000u64))
        );
        let v4 = idx
            .entries(ID_USDC)
            .iter()
            .find(|e| e.provider == FlashProvider::UniV4)
            .unwrap();
        assert_eq!(effective_cost(v4, usdc(50_000_000), &m), Some(U256::ZERO));
        m.gas_price_in_debt = U256::MAX;
        assert_eq!(effective_cost(aave, usdc(1), &m), None, "overflow → None");
    }

    /// Oracle: GUIDE 07 §6 — with a gas price, a per-group premium can
    /// re-order the chain. Premium on Morpho large enough to exceed
    /// Aave's fee moves Aave ahead of Morpho.
    #[test]
    fn premium_reorders_chain() {
        let idx = index_of(&five());
        let need = usdc(1_000);
        let mut m = CostModel::FEE_ONLY;
        m.gas_price_in_debt = U256::from(1u64);
        // Aave fee on 1_000 USDC at 5 bps = 500_000 raw (0.5 USDC).
        m.failure_premium_gas[FlashProvider::Morpho as usize] = 500_001;
        let chain = fallback_chain(&idx, ID_USDC, need, H90, &m);
        let order: Vec<_> = chain.iter().map(|r| r.provider).collect();
        assert_eq!(
            order,
            [
                FlashProvider::UniV4,
                FlashProvider::Aave,
                FlashProvider::UniV3,
                FlashProvider::Morpho
            ]
        );
    }
}
