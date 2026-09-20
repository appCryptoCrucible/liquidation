//! GUIDE 01 harness over the Euler V2 EVK adapter.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::inconsistent_digit_grouping,
    clippy::cast_possible_truncation
)]

mod common;

use alloy_primitives::{uint, Address, U256};
use alloy_sol_types::SolEvent;
use common::*;
use liq_adapters_euler_v2::events::{self as ev, evc, halt};
use liq_adapters_euler_v2::{alloc_meter, math};
use liq_protocol::conformance::{run, Fixtures, LogFixture, PositionFixture};
use liq_protocol::{
    CallbackShape, Constraints, DirtySet, FlashRoute, HealthState, LegChoice, Protocol,
    ProtocolError,
};
use liq_types::fixed::WAD;
use liq_types::LogSubscriber;

fn full_store(
    d: &Deploy,
) -> (
    liq_adapters_euler_v2::EulerV2,
    liq_protocol::conformance::JournalStore,
) {
    let p = d.adapter();
    let mut logs = listing_logs(d);
    logs.extend(activity_logs(d));
    let st = store_after(&p, &logs);
    (p, st)
}

fn liq_store(
    d: &Deploy,
) -> (
    liq_adapters_euler_v2::EulerV2,
    liq_protocol::conformance::JournalStore,
) {
    let p = d.adapter();
    let mut logs = listing_logs(d);
    logs.extend(activity_logs_liq(d));
    let st = store_after(&p, &logs);
    (p, st)
}

fn extra_logs(d: &Deploy) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 2, T1);
    vec![
        log(
            d.debt_vault,
            &ev::VaultStatus {
                totalShares: U256::ZERO,
                totalBorrows: U256::ZERO,
                accumulatedFees: U256::ZERO,
                cash: U256::ZERO,
                interestAccumulator: math::INITIAL_INTEREST_ACCUMULATOR,
                interestRate: U256::ZERO,
                timestamp: U256::from(t),
            },
            b,
            t,
        ),
        log(
            d.debt_vault,
            &ev::Repay {
                account: d.alice,
                assets: uint!(1_000000_U256),
            },
            b,
            t,
        ),
        log(
            d.debt_vault,
            &ev::Liquidate {
                liquidator: Address::repeat_byte(0xee),
                violator: d.alice,
                collateral: d.coll_vault,
                repayAssets: uint!(1_000000_U256),
                yieldBalance: uint!(1_000_000_000_000_000_U256),
            },
            b,
            t,
        ),
        log(
            d.coll_vault,
            &ev::Transfer {
                from: d.alice,
                to: d.bob,
                value: uint!(1_000_000_000_000_000_U256),
            },
            b,
            t,
        ),
        log(
            d.coll_vault,
            &ev::Withdraw {
                sender: d.alice,
                receiver: d.alice,
                owner: d.alice,
                assets: uint!(1_000_000_000_000_000_U256),
                shares: uint!(1_000_000_000_000_000_U256),
            },
            b,
            t,
        ),
        log(
            d.factory,
            &halt::Upgraded {
                implementation: Address::repeat_byte(0x77),
            },
            DEPLOY_BLOCK,
            T0,
        ),
        log(
            d.debt_vault,
            &ev::InterestAccrued {
                account: d.alice,
                assets: U256::ONE,
            },
            b,
            t,
        ),
        log(
            d.debt_vault,
            &ev::GovSetFeeReceiver {
                newFeeReceiver: Address::repeat_byte(0x02),
            },
            b,
            t,
        ),
        log(
            d.debt_vault,
            &ev::GovSetInterestRateModel {
                newInterestRateModel: Address::repeat_byte(0x03),
            },
            b,
            t,
        ),
        log(
            d.debt_vault,
            &ev::GovSetCaps {
                newSupplyCap: 0,
                newBorrowCap: 0,
            },
            b,
            t,
        ),
        log(
            d.debt_vault,
            &ev::Approval {
                owner: d.alice,
                spender: d.bob,
                value: U256::ONE,
            },
            b,
            t,
        ),
        log(
            d.evc,
            &evc::OwnerRegistered {
                addressPrefix: alloy_primitives::FixedBytes::ZERO,
                owner: d.alice,
            },
            b,
            t,
        ),
        log(d.factory, &ev::Genesis {}, DEPLOY_BLOCK, T0),
    ]
}

#[test]
fn ten_checks_pass_with_nonvacuous_assertions() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px = prices(WETH_P8, USDC_P8);
    let positions = [
        PositionFixture {
            pos: st.view(ALICE_ID, T0).unwrap(),
            px: &px,
            post: None,
        },
        PositionFixture {
            pos: st.view(BOB_ID, T0).unwrap(),
            px: &px,
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
    let mut log_store = store_after(&p, &listing_logs(&d));
    let rep = run(
        &p,
        &mut log_store,
        &fx,
        alloc_meter().map(|m| m as &dyn Fn() -> u64),
    )
    .unwrap_or_else(|f| panic!("{f}"));
    for (i, n) in rep.assertions.iter().enumerate() {
        match i + 1 {
            4 | 5 | 8 | 9 => {
                assert_eq!(
                    *n,
                    0,
                    "check {} is vacuous: 10R encode unwired / no liquidatable in this run",
                    i + 1
                );
            }
            _ => {
                assert!(*n > 0, "check {} was vacuous", i + 1);
            }
        }
    }
    assert_eq!(rep.alloc_metered, alloc_meter().is_some());
}

#[test]
fn health_matches_hand_derivation_at_t0() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px = prices(WETH_P8, USDC_P8);
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    let p_coll = U256::from(WETH_P8) * P8_TO_RAY;
    let p_debt = U256::from(USDC_P8) * P8_TO_RAY;
    let quoted = math::value_wad(ALICE_SHARES, p_coll, 18).unwrap();
    let coll_adj = math::collateral_adj_value(ALICE_SHARES, p_coll, 18, LIQ_LTV).unwrap();
    let liab = math::value_wad(ALICE_DEBT_HEALTHY, p_debt, 6).unwrap();
    let hf = math::mul_div_down(coll_adj, WAD, liab).unwrap();
    assert_eq!(quoted, uint!(1000_000000000000000000_U256));
    assert_eq!(coll_adj, uint!(800_000000000000000000_U256));
    assert_eq!(liab, uint!(700_000000000000000000_U256));
    assert_eq!(h.hf, math::hf_wad_to_ray(hf).unwrap());
    assert_eq!(h.state, HealthState::Healthy);
    assert!(coll_adj > liab);
}

#[test]
fn check_liquidation_zero_when_healthy() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px = prices(WETH_P8, USDC_P8);
    let q = p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px, &Constraints::UNBOUNDED)
        .unwrap();
    assert!(q.is_none());
}

#[test]
fn liquidatable_when_coll_adj_not_greater_than_liability() {
    let d = Deploy::new();
    let (p, st) = liq_store(&d);
    let px = prices(WETH_P8, USDC_P8);
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    let p_coll = U256::from(WETH_P8) * P8_TO_RAY;
    let p_debt = U256::from(USDC_P8) * P8_TO_RAY;
    let coll_adj = math::collateral_adj_value(ALICE_SHARES, p_coll, 18, LIQ_LTV).unwrap();
    let liab = math::value_wad(ALICE_DEBT_LIQ, p_debt, 6).unwrap();
    assert!(coll_adj <= liab);
    assert_eq!(h.state, HealthState::Liquidatable);
    let min_df = math::min_discount_factor(MAX_DISCOUNT).unwrap();
    assert_eq!(min_df, uint!(850_000000000000000_U256));
    let df = math::discount_factor(coll_adj, liab, min_df).unwrap();
    let q = p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px, &Constraints::UNBOUNDED)
        .unwrap()
        .expect("liquidatable");
    assert_eq!(q.repay_options[0].asset, USDC);
    assert_eq!(q.seize_options[0].asset, WETH_SHARES);
    let bonus = math::bonus_ray(df).unwrap();
    assert_eq!(q.seize_options[0].bonus, bonus);
    assert!(matches!(
        q.seize_options[0].curve,
        liq_protocol::BonusCurve::Static { .. }
    ));
    let (repay, yield_bal) = math::max_liquidation(
        ALICE_DEBT_LIQ,
        liab,
        coll_adj,
        ALICE_SHARES,
        math::value_wad(ALICE_SHARES, p_coll, 18).unwrap(),
        min_df,
    )
    .unwrap();
    assert_eq!(q.repay_options[0].max_repay, repay);
    assert_eq!(q.seize_options[0].max_seize, yield_bal);
}

#[test]
fn encode_validates_then_executor_unwired() {
    let d = Deploy::new();
    let (p, st) = liq_store(&d);
    let px = prices(WETH_P8, USDC_P8);
    let q = p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px, &Constraints::UNBOUNDED)
        .unwrap()
        .expect("liquidatable");
    let route = FlashRoute {
        provider: CallbackShape::MorphoFlashCallback.provider(),
        source: Address::repeat_byte(0x50),
        asset: USDC,
        amount: q.repay_options[0].max_repay,
        fee_bps: 0,
        callback: CallbackShape::MorphoFlashCallback,
    };
    assert_eq!(
        p.encode(&q, LegChoice::PREFERRED, &route, Address::repeat_byte(0x99)),
        Err(ProtocolError::ExecutorUnwired)
    );
    let mut q2 = q.clone();
    q2.key.protocol = liq_types::ProtocolId(99);
    assert_eq!(
        p.encode(
            &q2,
            LegChoice::PREFERRED,
            &route,
            Address::repeat_byte(0x99)
        ),
        Err(ProtocolError::ProtocolMismatch)
    );
    assert_eq!(
        p.encode(&q, LegChoice::PREFERRED, &route, Address::ZERO),
        Err(ProtocolError::ZeroRecipient)
    );
    let mut short = route;
    short.amount = U256::ZERO;
    assert_eq!(
        p.encode(&q, LegChoice::PREFERRED, &short, Address::repeat_byte(0x99)),
        Err(ProtocolError::FundingShort)
    );
    let mut mismatch = route;
    mismatch.asset = WETH_SHARES;
    assert_eq!(
        p.encode(
            &q,
            LegChoice::PREFERRED,
            &mismatch,
            Address::repeat_byte(0x99)
        ),
        Err(ProtocolError::FundingAssetMismatch)
    );
    let mut cb = route;
    cb.callback = CallbackShape::AaveExecuteOperation;
    assert_eq!(
        p.encode(&q, LegChoice::PREFERRED, &cb, Address::repeat_byte(0x99)),
        Err(ProtocolError::CallbackProviderMismatch)
    );
    assert_eq!(
        p.encode(
            &q,
            LegChoice { repay: 9, seize: 0 },
            &route,
            Address::repeat_byte(0x99)
        ),
        Err(ProtocolError::LegOutOfRange)
    );
}

#[test]
fn oracle_source_swap_fails_closed() {
    let d = Deploy::new();
    let mut cfg = d.config();
    cfg.price_sources.clear();
    let p = liq_adapters_euler_v2::EulerV2::new(cfg).unwrap();
    let mut logs = listing_logs(&d);
    logs.extend(activity_logs(&d));
    let st = store_after(&p, &logs);
    let px = prices(WETH_P8, USDC_P8);
    assert_eq!(
        p.health(st.view(ALICE_ID, T0).unwrap(), &px),
        Err(ProtocolError::OracleSourceMismatch)
    );
}

#[test]
fn halt_logs_fold_before_the_pin_and_error_after() {
    let d = Deploy::new();
    let (p, mut st) = full_store(&d);
    let impl_ = Address::repeat_byte(0x77);
    let before = log(
        d.factory,
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
        d.factory,
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
}

#[test]
fn subscriptions_cover_tracked_topics() {
    let d = Deploy::new();
    let p = d.adapter();
    let ranks = coverage_ranks();
    let subs = p.subscriptions();
    assert!(!subs.is_empty());
    for f in &subs {
        let halt_topics = [
            halt::Upgraded::SIGNATURE_HASH,
            halt::AdminChanged::SIGNATURE_HASH,
            halt::Initialized::SIGNATURE_HASH,
            ev::SetImplementation::SIGNATURE_HASH,
            ev::SetUpgradeAdmin::SIGNATURE_HASH,
            ev::GovSetGovernorAdmin::SIGNATURE_HASH,
        ];
        if halt_topics.contains(&f.topic0) {
            continue;
        }
        let _ = rank_of(&ranks, f.topic0);
    }
}

#[test]
fn probe_is_account_liquidity() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let probe = p.health_probe(st.view(ALICE_ID, T0).unwrap()).unwrap();
    assert_eq!(probe.to, d.debt_vault);
}

#[test]
fn min_discount_factor_from_source() {
    assert_eq!(math::min_discount_factor(0).unwrap(), WAD);
    assert_eq!(
        math::min_discount_factor(1_500).unwrap(),
        uint!(850_000000000000000_U256)
    );
    assert!(math::min_discount_factor(10_000).is_err());
}

#[test]
fn to_assets_up_matches_owed_lib() {
    assert_eq!(math::to_assets_up(U256::ZERO).unwrap(), U256::ZERO);
    assert_eq!(
        math::to_assets_up(U256::from(math::DEBT_SCALE)).unwrap(),
        U256::ONE
    );
    assert_eq!(math::to_assets_up(U256::ONE).unwrap(), U256::ONE);
}

use common::OwnedLog;
