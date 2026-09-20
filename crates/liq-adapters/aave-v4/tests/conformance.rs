//! The GUIDE 01 §8 conformance harness over the Aave V4 adapter, plus the
//! adapter-specific oracles the harness cannot know: hand-derived health
//! values from `Spoke._processUserAccountData`, the rational liquidation
//! price, the accrual crossing, `encode`, `health_probe`, the fail-closed
//! oracle-source rule and the halt rule.
//!
//! Supersedes `crates/liq-protocol/tests/conformance_v4.rs` (WP 01 skeleton).
//! Fixture provenance: `common/mod.rs`. Check 4 has no fixture here — the
//! protocol's own post-liquidation state needs a fork (04C) — and the
//! report says so (`assertions[3] == 0`).

// Test-side arithmetic is the oracle: an overflow here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::inconsistent_digit_grouping,
    clippy::needless_update,
    clippy::cast_possible_truncation
)]

mod common;

use alloy_primitives::{uint, Address, U256};
use alloy_sol_types::{SolEvent, SolValue};
use common::*;
use liq_adapters_aave_v4::events::{halt, hub, oracle, spoke};
use liq_adapters_aave_v4::{alloc_meter, math};
use liq_protocol::conformance::{run, Fixtures, LogFixture, PositionFixture};
use liq_protocol::{
    CallbackShape, Constraints, DirtySet, ExecutorAdapter, FlashRoute, HealthState, LegChoice,
    Protocol, ProtocolError,
};
use liq_types::fixed::{mul_div, Rounding, RAY};
use liq_types::{LogSubscriber, Ray, Wad};

/// Chain constants used by the hand derivations (not the adapter's).
const WAD: U256 = uint!(1_000_000_000_000_000_000_U256);
const BPS: U256 = uint!(10_000_U256);
const YEAR: U256 = uint!(31_536_000_U256);

/// `_processUserAccountData` for Alice by hand: one collateral (WETH, cf
/// 80%) and one debt (DAI, drawn + premium) at DAI index `idx`, prices as
/// 8-decimal answers, `decimals == 18` on both sides so `toValue` is
/// `amount · price`.
///
/// Returns `(healthFactor WAD, totalCollateralValue, totalDebtValueRay)`.
fn alice_by_hand(weth_p8: u64, dai_p8: u64, idx: U256) -> (U256, U256, U256) {
    // Collateral: shares → assets is the identity at this pool state
    // (1e18 shares, 1e18 assets, 1e6 virtual on both sides).
    let coll_value = ALICE_WETH * U256::from(weth_p8);
    let weighted = U256::from(WETH_CF) * coll_value;
    // Debt: drawnShares · idx + (premiumShares · idx − offset), offset =
    // premiumShares · RAY (set at index RAY with zero prior premium).
    let premium_ray = ALICE_PREMIUM_SHARES * idx - ALICE_PREMIUM_SHARES * RAY;
    let debt_ray = ALICE_DAI_DEBT * idx + premium_ray;
    let debt_value_ray = debt_ray * U256::from(dai_p8);
    // healthFactor = mulDiv(avgCf.bpsToWad(), RAY, totalDebtValueRay, Floor)
    // with avgCf = weighted / coll_value (bps); bpsToWad = · 1e14. The
    // chain keeps `weighted` un-normalised until the end, so this is
    // floor(weighted · 1e14 · RAY / debt_value_ray).
    let hf = weighted * uint!(100_000_000_000_000_U256) * RAY / debt_value_ray;
    (hf, coll_value, debt_value_ray)
}

/// `calculateLinearInterest` then `rayMulUp(RAY, ·)` — the DAI index at
/// `T1` from the fixture's rate, by hand.
fn dai_index_at_t1() -> U256 {
    let li = RAY + DAI_RATE * U256::from(T1 - T0) / YEAR;
    // rayMulUp(RAY, li) = ceil(RAY · li / RAY) = li.
    li
}

fn value_to_wad(v: U256) -> Wad {
    Wad::from_raw(v / uint!(100_000_000_U256))
}

fn full_store(
    d: &Deploy,
) -> (
    liq_adapters_aave_v4::AaveV4,
    liq_protocol::conformance::JournalStore,
) {
    let p = d.adapter();
    let mut logs = listing_logs(d);
    logs.extend(activity_logs(d));
    let st = store_after(&p, &logs);
    (p, st)
}

/// Logs for checks 6/7 beyond the activity stream: an accrual, a pause, a
/// dynamic-config change, a source swap and its restoration, a partial
/// repay with its hub restore, a liquidation, and a pre-pin halt log.
fn extra_logs(d: &Deploy) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 2, T1);
    let idx1 = dai_index_at_t1();
    let liquidator = Address::repeat_byte(0xee);
    // Repay 500 DAI at index RAY (t = T0 for the arithmetic; the log's
    // timestamp is T1 but `Repay` carries no time). New premium shares
    // percentMulUp(1000e18, 200) = 20e18; premiumDebtRay at RAY is 0, so
    // the new offset is 20e18 · RAY: delta (−10e18, −10e18 · RAY).
    let repaid = uint!(500_000_000_000_000_000_000_U256);
    let d_shares =
        alloy_primitives::I256::try_from(uint!(10_000_000_000_000_000_000_U256)).unwrap();
    let d_offset =
        alloy_primitives::I256::try_from(uint!(10_000_000_000_000_000_000_U256) * RAY).unwrap();
    let repay_delta = spoke::PremiumDelta {
        sharesDelta: -d_shares,
        offsetRayDelta: -d_offset,
        restoredPremiumRay: U256::ZERO,
    };
    let restore_delta = hub::PremiumDelta {
        sharesDelta: -d_shares,
        offsetRayDelta: -d_offset,
        restoredPremiumRay: U256::ZERO,
    };
    vec![
        log(
            d.hub,
            &hub::UpdateAsset {
                assetId: U256::from(1u8),
                drawnIndex: idx1,
                drawnRate: DAI_RATE,
                accruedFees: U256::ZERO,
            },
            b,
            t,
        ),
        log(
            d.spoke,
            &spoke::UpdateReserveConfig {
                reserveId: U256::ZERO,
                config: spoke::ReserveConfig {
                    collateralRisk: alloy_primitives::aliases::U24::from(WETH_RISK),
                    paused: true,
                    frozen: false,
                    borrowable: true,
                    receiveSharesEnabled: true,
                },
            },
            b,
            t,
        ),
        log(
            d.spoke,
            &spoke::UpdateDynamicReserveConfig {
                reserveId: U256::ZERO,
                dynamicConfigKey: 0,
                config: spoke::DynamicReserveConfig {
                    collateralFactor: 75_00,
                    maxLiquidationBonus: WETH_MAX_BONUS,
                    liquidationFee: WETH_FEE,
                },
            },
            b,
            t,
        ),
        log(
            d.oracle,
            &oracle::UpdateReserveSource {
                reserveId: U256::ZERO,
                source: Address::repeat_byte(0xff),
            },
            b,
            t,
        ),
        log(
            d.oracle,
            &oracle::UpdateReserveSource {
                reserveId: U256::ZERO,
                source: d.src_weth,
            },
            b,
            t,
        ),
        log(
            d.hub,
            &hub::Restore {
                assetId: U256::from(1u8),
                spoke: d.spoke,
                drawnShares: repaid,
                premiumDelta: restore_delta,
                drawnAmount: repaid,
                premiumAmount: U256::ZERO,
            },
            b,
            t,
        ),
        log(
            d.spoke,
            &spoke::Repay {
                reserveId: U256::from(1u8),
                caller: d.alice,
                user: d.alice,
                drawnShares: repaid,
                totalAmountRepaid: repaid,
                premiumDelta: repay_delta,
            },
            b,
            t,
        ),
        log(
            d.spoke,
            &spoke::LiquidationCall {
                collateralReserveId: U256::ZERO,
                debtReserveId: U256::from(1u8),
                user: d.alice,
                liquidator,
                receiveShares: true,
                debtAmountRestored: uint!(100_000_000_000_000_000_000_U256),
                drawnSharesLiquidated: uint!(100_000_000_000_000_000_000_U256),
                premiumDelta: spoke::PremiumDelta {
                    sharesDelta: -alloy_primitives::I256::try_from(uint!(
                        2_000_000_000_000_000_000_U256
                    ))
                    .unwrap(),
                    offsetRayDelta: -alloy_primitives::I256::try_from(
                        uint!(2_000_000_000_000_000_000_U256) * RAY,
                    )
                    .unwrap(),
                    restoredPremiumRay: U256::ZERO,
                },
                collateralAmountRemoved: uint!(50_000_000_000_000_000_U256),
                collateralSharesLiquidated: uint!(50_000_000_000_000_000_U256),
                collateralSharesToLiquidator: uint!(45_000_000_000_000_000_U256),
            },
            b,
            t,
        ),
        log(d.spoke, &halt::Initialized { version: 1 }, DEPLOY_BLOCK, T0),
    ]
}

#[test]
fn ten_checks_pass_with_nonvacuous_assertions() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px_liq = prices(1800_0000_0000, DAI_P8);
    let px_ok = prices(WETH_P8, DAI_P8);
    let positions = [
        PositionFixture {
            pos: st.view(ALICE_ID, T0).unwrap(),
            px: &px_liq,
            post: None,
        },
        PositionFixture {
            pos: st.view(ALICE_ID, T1).unwrap(),
            px: &px_ok,
            post: None,
        },
        PositionFixture {
            pos: st.view(ALICE_ID, T1).unwrap(),
            px: &px_liq,
            post: None,
        },
        PositionFixture {
            pos: st.view(BOB_ID, T1).unwrap(),
            px: &px_ok,
            post: None,
        },
    ];
    let ranks = coverage_ranks();
    let mut owned = activity_logs(&d);
    owned.extend(extra_logs(&d));
    let logs: Vec<LogFixture<'_>> = owned
        .iter()
        .map(|l| LogFixture {
            log: l.view(),
            max_dirty_rank: rank_of(&ranks, l.topics[0]),
        })
        .collect();
    let flash_sources: Vec<(CallbackShape, Address)> = CallbackShape::ALL
        .iter()
        .enumerate()
        .map(|(i, s)| (*s, Address::repeat_byte(0x50 + i as u8)))
        .collect();
    let fx = Fixtures {
        positions: &positions,
        logs: &logs,
        flash_sources: &flash_sources,
        recipient: Address::repeat_byte(0x99),
    };
    // Checks 6/7 run on the listing-only store.
    let mut log_store = store_after(&p, &listing_logs(&d));
    let rep = run(
        &p,
        &mut log_store,
        &fx,
        alloc_meter().map(|m| m as &dyn Fn() -> u64),
    )
    .unwrap_or_else(|f| panic!("{f}"));
    for (i, n) in rep.assertions.iter().enumerate() {
        if i == 3 {
            assert_eq!(*n, 0, "check 4 needs a fork post-state (04C)");
        } else {
            assert!(*n > 0, "check {} was vacuous", i + 1);
        }
    }
    // The meter seam is honest about itself.
    assert_eq!(rep.alloc_metered, alloc_meter().is_some());
}

#[test]
fn health_matches_hand_derivation_at_t0() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px = prices(1800_0000_0000, DAI_P8);
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    let (hf, coll, debt_ray) = alice_by_hand(1800_0000_0000, DAI_P8, RAY);
    // 8000 · 1800e26 · 1e14 · 1e27 / (1500e18 · 1e27 · 1e8) = 0.96e18.
    assert_eq!(hf, uint!(960_000_000_000_000_000_U256));
    assert_eq!(h.hf, Ray::from_raw(hf * uint!(1_000_000_000_U256)));
    assert_eq!(h.collateral_value, value_to_wad(coll));
    assert_eq!(
        h.collateral_value,
        Wad::from_raw(uint!(1_800_000_000_000_000_000_000_U256))
    );
    assert_eq!(
        h.debt_value,
        value_to_wad((debt_ray + RAY - U256::ONE) / RAY)
    );
    assert_eq!(
        h.debt_value,
        Wad::from_raw(uint!(1_500_000_000_000_000_000_000_U256))
    );
    assert_eq!(h.state, HealthState::Liquidatable);
    assert!(h.price_sensitivity.contains(WETH_SLOT));
    assert!(h.price_sensitivity.contains(DAI_SLOT));
    assert!(!h.price_sensitivity.contains(0));
}

#[test]
fn health_at_t1_accrues_base_rate_and_premium() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px = prices(WETH_P8, DAI_P8);
    let idx1 = dai_index_at_t1();
    assert_eq!(idx1, uint!(1_004_109_589_041_095_890_410_958_904_U256));
    let h = p.health(st.view(ALICE_ID, T1).unwrap(), &px).unwrap();
    let (hf, _, debt_ray) = alice_by_hand(WETH_P8, DAI_P8, idx1);
    assert_eq!(h.hf, Ray::from_raw(hf * uint!(1_000_000_000_U256)));
    assert_eq!(
        h.debt_value,
        value_to_wad((debt_ray + RAY - U256::ONE) / RAY)
    );
    assert_eq!(h.state, HealthState::Healthy);
    // Premium is in the debt: with the premium removed the value is lower.
    let no_premium = ALICE_DAI_DEBT * idx1 * U256::from(DAI_P8);
    assert!(debt_ray > no_premium);
    // And the time projection is what the chain would compute: the same
    // view at T0 is the unaccrued debt.
    let h0 = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert!(h0.hf > h.hf);
    // A view before the row's last update is the chain's revert.
    assert_eq!(
        p.health(st.view(ALICE_ID, T0 - 1).unwrap(), &px),
        Err(ProtocolError::TimestampBeforeUpdate)
    );
}

#[test]
fn liquidation_price_is_the_rational_boundary() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px = prices(WETH_P8, DAI_P8);
    let pos = st.view(ALICE_ID, T0).unwrap();
    // hf(p) = 8000 · 1e18 · p · 1e14 · 1e27 / (1500e18 · 1e27 · 1e8) >= 1e18
    // ⇔ p >= 1.875e11 = 1875e8: the last healthy WETH answer.
    let lp = p.liquidation_price(pos, &px, WETH).unwrap().unwrap();
    assert_eq!(lp.price, ray_of_p8(1875_0000_0000));
    // DAI (debt side): healthy ⇔ 8000·2000e8·1e18·1e14·1e27 >= 1500e18·1e27·p
    // ⇔ p <= 1.0666…e8; floor = 106_666_666, returned as the last RAY that
    // reads as it.
    let lp = p.liquidation_price(pos, &px, DAI).unwrap().unwrap();
    assert_eq!(
        lp.price,
        Ray::from_raw(U256::from(106_666_667u64) * P8_TO_RAY - U256::ONE)
    );
    // An asset the position does not hold.
    assert_eq!(
        p.liquidation_price(pos, &px, liq_types::AssetId(7))
            .unwrap(),
        None
    );
    // Bob holds DAI without collateral flag or debt: no term, no crossing.
    assert_eq!(
        p.liquidation_price(st.view(BOB_ID, T0).unwrap(), &px, DAI)
            .unwrap(),
        None
    );
}

/// 10 000 round trips: for a sweep of DAI prices and timestamps the WETH
/// liquidation price is healthy at itself and unhealthy one RAY unit toward
/// danger — exact, no tolerance (check 3's rule, at volume).
#[test]
fn liquidation_price_round_trips_ten_thousand_times() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let mut n = 0u32;
    for i in 0..100u64 {
        let dai_p8 = 90_000_000 + i * 233_331; // 0.90 … 1.13 USD, off-grid steps
        let px = prices(WETH_P8, dai_p8);
        for j in 0..100u64 {
            let ts = T0 + j * 86_400 * 3 + j; // ~ 300 days, odd seconds
            let pos = st.view(ALICE_ID, ts).unwrap();
            let lp = p.liquidation_price(pos, &px, WETH).unwrap().unwrap();
            let at = |raw: U256| {
                let mut v = px.clone();
                v.0[0].price = Ray::from_raw(raw);
                p.health(pos, &v).unwrap().hf
            };
            assert!(at(lp.price.raw()) >= Ray::ONE, "i={i} j={j}");
            assert!(at(lp.price.raw() - U256::ONE) < Ray::ONE, "i={i} j={j}");
            n += 1;
        }
    }
    assert_eq!(n, 10_000);
}

#[test]
fn time_to_cross_brackets_the_accrual_crossing() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    // hf(T0) at WETH 1900 = 1.01333…: crosses within the horizon.
    let px = prices(1900_0000_0000, DAI_P8);
    let pos = st.view(ALICE_ID, T0).unwrap();
    let t = p.time_to_cross(pos, &px).unwrap().unwrap();
    assert!(t > T0);
    let hf_at = |ts: u64| p.health(st.view(ALICE_ID, ts).unwrap(), &px).unwrap().hf;
    assert!(hf_at(t) < Ray::ONE);
    assert!(hf_at(t - 1) >= Ray::ONE);
    // By hand: debt(t) = (1500e18 + 30e18) · idx(t) − 30e18 · RAY, idx(t) =
    // RAY + 5e25·dt/YEAR; healthy ⇔ hf = 8000·1900e26·1e14·RAY / (debt·1e8)
    // >= 1e18 ⇔ 1.52e74 >= 1e18 · (1.53e29·idx − 3e54)
    // ⇒ idx* = (1.52e56 + 3e54) / 1.53e29.
    let e = |n: u64| U256::from(10u8).pow(U256::from(n));
    let idx_star =
        (U256::from(152u8) * e(54) + U256::from(3u8) * e(54)) / (U256::from(153u8) * e(27));
    let dt_star = (idx_star - RAY) * YEAR / DAI_RATE;
    let dt = U256::from(t - T0);
    assert!(
        dt >= dt_star && dt - dt_star <= U256::from(2u8),
        "dt={dt} dt*={dt_star}"
    );
    // Already unhealthy: now. Healthy without debt: never.
    assert_eq!(
        p.time_to_cross(pos, &prices(1800_0000_0000, DAI_P8))
            .unwrap(),
        Some(T0)
    );
    assert_eq!(
        p.time_to_cross(st.view(BOB_ID, T0).unwrap(), &px).unwrap(),
        None
    );
}

#[test]
fn quote_reproduces_calculate_liquidation_amounts() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    // WETH 1860: hf = 8000·1860e26·1e14·RAY / (1500e18·RAY·1e8) = 0.992e18,
    // and neither side trips the 1000-unit dust rule (see below).
    let px = prices(1860_0000_0000, DAI_P8);
    let pos = st.view(ALICE_ID, T0).unwrap();
    let h = p.health(pos, &px).unwrap();
    let hf = uint!(992_000_000_000_000_000_U256);
    assert_eq!(h.hf, Ray::from_raw(hf * uint!(1_000_000_000_U256)));
    let q = p
        .quote(pos, &px, &Constraints::UNBOUNDED)
        .unwrap()
        .expect("liquidatable");
    assert_eq!(q.repay_options.len(), 1);
    assert_eq!(q.seize_options.len(), 1);
    let repay = &q.repay_options[0];
    let seize = &q.seize_options[0];
    assert_eq!(repay.asset, DAI);
    assert_eq!(seize.asset, WETH);
    assert_eq!(seize.max_seize, ALICE_WETH);

    // calculateLiquidationBonus by hand at hf = 0.992e18, max 10_500:
    // min = percentMulDown(500, 2000) + 1e4 = 10_100;
    // rise = mulDivDown(400, 1e18 − 0.992e18, 1e18 − 0.7e18) = ⌊400·8e15/3e17⌋ = 10.
    let bonus_bps = 10_100u64 + 10;
    assert_eq!(
        seize.bonus,
        Ray::from_raw(U256::from(bonus_bps - 10_000) * math::BPS_RAY)
    );
    // The curve is populated, never a scalar.
    assert!(matches!(
        seize.curve,
        liq_protocol::BonusCurve::HealthLinear { .. }
    ));

    // _calculateDebtToTargetHealthFactor, 512-bit as the chain's mulDiv:
    // penalty = percentMulUp(bpsToWad(10_110), 8000) = 0.8088e18;
    // debtRayToTarget = mulDivUp(D_ray, 1e18·(1.05e18 − 0.992e18),
    //                            (1.05e18 − 0.8088e18)·1e8·1e18).
    let d_ray = ALICE_DAI_DEBT * RAY * U256::from(DAI_P8);
    let penalty = mul_div(
        U256::from(bonus_bps) * uint!(100_000_000_000_000_U256),
        U256::from(WETH_CF),
        BPS,
        Rounding::Up,
    )
    .unwrap();
    assert_eq!(penalty, uint!(808_800_000_000_000_000_U256));
    let target = U256::from(TARGET_HF_WAD);
    let num = WAD * (target - hf);
    let den = (target - penalty) * U256::from(DAI_P8) * WAD;
    let to_target = mul_div(d_ray, num, den, Rounding::Up).unwrap();
    // Premium is 0 at index RAY: drawnSharesToLiquidate = divUp(toTarget, RAY)
    // (below the user's shares), premium 0.
    let drawn_liq = mul_div(to_target, U256::ONE, RAY, Rounding::Up).unwrap();
    assert!(drawn_liq < ALICE_DAI_DEBT);
    // ≈ 360.7 DAI; the remaining 1139 USD of debt and the remaining
    // collateral (1 WETH − 360.7·1.011/1860 ≈ 0.804 WETH ≈ 1495 USD) both
    // clear DUST_LIQUIDATION_THRESHOLD, so no full-close override.
    assert!(
        drawn_liq > uint!(360_000_000_000_000_000_000_U256)
            && drawn_liq < uint!(361_000_000_000_000_000_000_U256)
    );
    let remaining_value = (ALICE_DAI_DEBT - drawn_liq) * U256::from(DAI_P8);
    assert!(remaining_value >= math::DUST_VALUE);
    // amountToRestore = rayMulUp(drawn, RAY) + fromRayUp(0) = drawn.
    assert_eq!(repay.max_repay, drawn_liq);
    // The WP's closed form R = (target·D − LT·C) / (target − (1+b)·LT) with
    // C the collateral value and LT·C its weighted value: (1575 − 1488) /
    // (1.05 − 1.011·0.8) USD = 87 / 0.2412 = 360.696… DAI, to the unit.
    let r_units = mul_div(
        target * uint!(1_500_U256)
            - U256::from(WETH_CF) * uint!(1_860_U256) * uint!(100_000_000_000_000_U256),
        uint!(1_000_000_000_000_000_000_U256),
        target - penalty,
        Rounding::Up,
    )
    .unwrap();
    assert_eq!(r_units, repay.max_repay);

    // encode → Executor calldata shape.
    let route = FlashRoute {
        provider: CallbackShape::AaveExecuteOperation.provider(),
        source: Address::repeat_byte(0x50),
        asset: DAI,
        amount: repay.max_repay,
        fee_bps: 0,
        callback: CallbackShape::AaveExecuteOperation,
    };
    let plan = p
        .encode(&q, LegChoice::PREFERRED, &route, Address::repeat_byte(0x99))
        .unwrap();
    assert_eq!(plan.leg.adapter, ExecutorAdapter::AaveV4);
    assert_eq!(plan.leg.market, d.spoke);
    assert_eq!(plan.leg.borrower, d.alice);
    assert_eq!(plan.leg.collateral_asset, d.weth);
    assert_eq!(plan.debt_asset, d.dai);
    assert_eq!(U256::from(plan.leg.repay_amount), repay.max_repay);
}

/// WETH 1800: hf 0.96, debtRayToTarget ≈ 567.8 DAI, which would leave 932
/// USD of debt — under `DUST_LIQUIDATION_THRESHOLD` (1000 USD) — so
/// `_calculateDebtToLiquidate` takes the whole position.
#[test]
fn quote_applies_the_debt_dust_rule() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px = prices(1800_0000_0000, DAI_P8);
    let pos = st.view(ALICE_ID, T0).unwrap();
    let q = p
        .quote(pos, &px, &Constraints::UNBOUNDED)
        .unwrap()
        .expect("liquidatable");
    // bonus at 0.96: min 10_100 + ⌊400·4e16/3e17⌋ = 10_153.
    assert_eq!(
        q.seize_options[0].bonus,
        Ray::from_raw(U256::from(153u8) * math::BPS_RAY)
    );
    assert_eq!(q.repay_options[0].max_repay, ALICE_DAI_DEBT);
}

#[test]
fn quote_honours_notional_cap_and_dust() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px = prices(1800_0000_0000, DAI_P8);
    let pos = st.view(ALICE_ID, T0).unwrap();
    let full = p.quote(pos, &px, &Constraints::UNBOUNDED).unwrap().unwrap();
    // Cap at 100 USD (WAD numeraire): 100 DAI raw at price 1.
    let cons = Constraints {
        per_liquidation_notional_cap: Wad::from_raw(uint!(100_000_000_000_000_000_000_U256)),
        ..Constraints::UNBOUNDED
    };
    let capped = p.quote(pos, &px, &cons).unwrap().unwrap();
    assert_eq!(
        capped.repay_options[0].max_repay,
        uint!(100_000_000_000_000_000_000_U256)
    );
    assert!(capped.repay_options[0].max_repay < full.repay_options[0].max_repay);
    // A cap that leaves less than the 1000-unit dust threshold of debt forces
    // the full close: max_repay is then the entire debt.
    let px_deep = prices(1000_0000_0000, DAI_P8);
    let deep = p
        .quote(pos, &px_deep, &Constraints::UNBOUNDED)
        .unwrap()
        .unwrap();
    // hf = 0.5333: collateral is worth 1000 < 1500 debt; all of it goes.
    assert_eq!(deep.seize_options[0].max_seize, ALICE_WETH);
}

#[test]
fn healthy_and_no_debt_positions_do_not_quote() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px = prices(WETH_P8, DAI_P8);
    assert_eq!(
        p.quote(st.view(ALICE_ID, T0).unwrap(), &px, &Constraints::UNBOUNDED)
            .unwrap(),
        None
    );
    let bob = p.health(st.view(BOB_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(bob.hf, liq_protocol::Health::NO_DEBT_HF);
    assert_eq!(bob.state, HealthState::Healthy);
    assert_eq!(
        p.quote(st.view(BOB_ID, T0).unwrap(), &px, &Constraints::UNBOUNDED)
            .unwrap(),
        None
    );
}

#[test]
fn paused_collateral_blocks_and_bad_debt_classifies() {
    let d = Deploy::new();
    let (p, mut st) = full_store(&d);
    let px = prices(1800_0000_0000, DAI_P8);
    // Pause WETH: the only seizable side is gone → Blocked.
    let pause = log(
        d.spoke,
        &spoke::UpdateReserveConfig {
            reserveId: U256::ZERO,
            config: spoke::ReserveConfig {
                collateralRisk: alloy_primitives::aliases::U24::from(WETH_RISK),
                paused: true,
                frozen: false,
                borrowable: true,
                receiveSharesEnabled: true,
            },
        },
        DEPLOY_BLOCK + 2,
        T0,
    );
    let dirty = p.apply_log(&mut st, &pause.view()).unwrap();
    assert!(matches!(dirty, DirtySet::MarketReprice(_)));
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert!(matches!(h.state, HealthState::Blocked { .. }));
    assert_eq!(
        p.quote(st.view(ALICE_ID, T0).unwrap(), &px, &Constraints::UNBOUNDED)
            .unwrap(),
        None
    );
    // Disable the collateral flag: nothing counted → BadDebt with the debt.
    let off = log(
        d.spoke,
        &spoke::SetUsingAsCollateral {
            reserveId: U256::ZERO,
            caller: d.alice,
            user: d.alice,
            usingAsCollateral: false,
        },
        DEPLOY_BLOCK + 2,
        T0,
    );
    p.apply_log(&mut st, &off.view()).unwrap();
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(
        h.state,
        HealthState::BadDebt {
            deficit: Wad::from_raw(uint!(1_500_000_000_000_000_000_000_U256))
        }
    );
    assert_eq!(h.hf, Ray::ZERO);
}

#[test]
fn oracle_source_swap_fails_closed_and_restores() {
    let d = Deploy::new();
    let (p, mut st) = full_store(&d);
    let px = prices(1800_0000_0000, DAI_P8);
    let rogue = Address::repeat_byte(0xff);
    let swap = log(
        d.oracle,
        &oracle::UpdateReserveSource {
            reserveId: U256::ZERO,
            source: rogue,
        },
        DEPLOY_BLOCK + 2,
        T0,
    );
    assert_eq!(
        oracle::UpdateReserveSource::SIGNATURE_HASH,
        "0xb828dda2b9aa56f34e592f8a1c065bf11753e12bed944560d220d26367bb8140"
            .parse::<alloy_primitives::B256>()
            .unwrap()
    );
    let dirty = p.apply_log(&mut st, &swap.view()).unwrap();
    assert!(matches!(dirty, DirtySet::MarketReprice(_)));
    assert_eq!(
        p.health(st.view(ALICE_ID, T0).unwrap(), &px),
        Err(ProtocolError::OracleSourceMismatch)
    );
    assert_eq!(
        p.quote(st.view(ALICE_ID, T0).unwrap(), &px, &Constraints::UNBOUNDED),
        Err(ProtocolError::OracleSourceMismatch)
    );
    // The spoke-side log for the same swap is the same rule.
    let back = log(
        d.spoke,
        &spoke::UpdateReservePriceSource {
            reserveId: U256::ZERO,
            priceSource: d.src_weth,
        },
        DEPLOY_BLOCK + 2,
        T0,
    );
    p.apply_log(&mut st, &back.view()).unwrap();
    assert!(p.health(st.view(ALICE_ID, T0).unwrap(), &px).is_ok());
}

#[test]
fn halt_logs_fold_before_the_pin_and_error_after() {
    let d = Deploy::new();
    let (p, mut st) = full_store(&d);
    let impl_ = Address::repeat_byte(0x77);
    let before = log(
        d.hub,
        &halt::Upgraded {
            implementation: impl_,
        },
        DEPLOY_BLOCK,
        T0,
    );
    assert_eq!(
        p.apply_log(&mut st, &before.view()).unwrap(),
        DirtySet::None
    );
    let after = log(
        d.spoke,
        &halt::Upgraded {
            implementation: impl_,
        },
        DEPLOY_BLOCK + 1,
        T0,
    );
    assert_eq!(
        p.apply_log(&mut st, &after.view()),
        Err(ProtocolError::HaltSignal)
    );
    let authority = log(
        d.oracle,
        &halt::AuthorityUpdated { authority: impl_ },
        DEPLOY_BLOCK + 1,
        T0,
    );
    assert_eq!(
        p.apply_log(&mut st, &authority.view()),
        Err(ProtocolError::HaltSignal)
    );
}

#[test]
fn unknown_emitter_and_unknown_slot_are_errors_not_state() {
    let d = Deploy::new();
    let (p, mut st) = full_store(&d);
    let before = st.clone();
    let stranger = log(
        Address::repeat_byte(0x01),
        &spoke::Supply {
            reserveId: U256::ZERO,
            caller: d.alice,
            user: d.alice,
            suppliedShares: U256::ONE,
            suppliedAmount: U256::ONE,
        },
        DEPLOY_BLOCK + 2,
        T0,
    );
    assert_eq!(
        p.apply_log(&mut st, &stranger.view()),
        Err(ProtocolError::UnexpectedLog)
    );
    // A reserve the spoke never listed.
    let ghost = log(
        d.spoke,
        &spoke::Supply {
            reserveId: U256::from(9u8),
            caller: d.alice,
            user: d.alice,
            suppliedShares: U256::ONE,
            suppliedAmount: U256::ONE,
        },
        DEPLOY_BLOCK + 2,
        T0,
    );
    assert!(matches!(
        p.apply_log(&mut st, &ghost.view()),
        Err(ProtocolError::SlotOutOfRange(_))
    ));
    // A hub asset out of listing order.
    let skip = log(
        d.hub,
        &hub::AddAsset {
            assetId: U256::from(5u8),
            underlying: Address::repeat_byte(0xa5),
            decimals: 6,
        },
        DEPLOY_BLOCK + 2,
        T0,
    );
    assert_eq!(
        p.apply_log(&mut st, &skip.view()),
        Err(ProtocolError::SlotMismatch {
            expected: 2,
            got: 5
        })
    );
    // Nothing above left a trace.
    let mark = st.mark();
    st.undo_to(mark);
    assert_eq!(st, before);
}

#[test]
fn health_probe_targets_the_spoke_and_decodes_hf() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let pos = st.view(ALICE_ID, T0).unwrap();
    let call = p.health_probe(pos).unwrap();
    assert_eq!(call.to, d.spoke);
    // getUserAccountData(address) selector + the user word.
    assert_eq!(
        &call.data[..4],
        &alloy_primitives::keccak256("getUserAccountData(address)")[..4]
    );
    assert_eq!(&call.data[16..36], d.alice.as_slice());
    // Decode the chain's tuple: healthFactor is the third word.
    let ret = (
        U256::from(200u8),
        U256::from(8000u64),
        uint!(960_000_000_000_000_000_U256),
        U256::ZERO,
        U256::ZERO,
        U256::ONE,
        U256::ONE,
    )
        .abi_encode();
    assert_eq!(
        (call.decode)(&ret).unwrap(),
        Ray::from_raw(uint!(960_000_000_000_000_000_000_000_000_U256))
    );
    assert_eq!((call.decode)(&ret[..40]), Err(ProtocolError::ProbeDecode));
}

#[test]
fn subscriptions_cover_every_tracked_topic_in_the_coverage_table() {
    let d = Deploy::new();
    let p = d.adapter();
    let ranks = coverage_ranks();
    let subs = p.subscriptions();
    assert!(!subs.is_empty());
    for f in &subs {
        assert!(
            [d.hub, d.spoke, d.oracle].contains(&f.address),
            "filter on untracked address {}",
            f.address
        );
        // Proxy/initializer/authority logs are `halt` rows that the table
        // lists under their emitter; ERC-1967 topics are shared across all
        // three, so accept any halt-class hash as well.
        let halt_topics = [
            halt::Upgraded::SIGNATURE_HASH,
            halt::AdminChanged::SIGNATURE_HASH,
            halt::Initialized::SIGNATURE_HASH,
            halt::AuthorityUpdated::SIGNATURE_HASH,
        ];
        if halt_topics.contains(&f.topic0) {
            continue;
        }
        let _ = rank_of(&ranks, f.topic0);
    }
    // And every routed spoke/hub/oracle row of the table is subscribed.
    let routed = [
        hub::UpdateAsset::SIGNATURE_HASH,
        spoke::LiquidationCall::SIGNATURE_HASH,
        spoke::RefreshPremiumDebt::SIGNATURE_HASH,
        oracle::UpdateReserveSource::SIGNATURE_HASH,
    ];
    for t in routed {
        assert!(subs.iter().any(|f| f.topic0 == t), "{t} not subscribed");
    }
}

#[test]
fn config_rejects_malformed_deployments() {
    use liq_adapters_aave_v4::ConfigError;
    let d = Deploy::new();
    let mut c = d.config();
    c.spokes[0].oracle = Address::ZERO;
    assert_eq!(c.validate(), Err(ConfigError::ZeroOracle(d.spoke)));
    let mut c = d.config();
    c.spokes[0].address = d.hub;
    assert_eq!(c.validate(), Err(ConfigError::DuplicateAddress(d.hub)));
    let mut c = d.config();
    c.assets[1].asset = WETH;
    assert_eq!(c.validate(), Err(ConfigError::DuplicateAsset(WETH)));
    let mut c = d.config();
    c.hubs.clear();
    assert_eq!(c.validate(), Err(ConfigError::NoHubs));
}
