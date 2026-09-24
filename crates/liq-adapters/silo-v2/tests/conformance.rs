//! GUIDE 01 harness over the Silo V2 adapter.

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
use liq_adapters_silo_v2::events::halt;
use liq_adapters_silo_v2::events::{factory, hook, silo};
use liq_adapters_silo_v2::{alloc_meter, math};
use liq_protocol::conformance::{run, Fixtures, LogFixture, PositionFixture};
use liq_protocol::{
    CallbackShape, DirtySet, ExecutorAdapter, FlashRoute, HealthState, LegChoice, MarketFlags,
    MarketRow, Protocol, ProtocolError, StateWriter,
};
use liq_types::{LogSubscriber, PositionKey, Ray};

fn full_store(
    d: &Deploy,
    debt: U256,
) -> (
    liq_adapters_silo_v2::SiloV2,
    liq_protocol::conformance::JournalStore,
) {
    full_store_pos(d, ALICE_COLL, debt)
}

fn full_store_pos(
    d: &Deploy,
    coll: U256,
    debt: U256,
) -> (
    liq_adapters_silo_v2::SiloV2,
    liq_protocol::conformance::JournalStore,
) {
    let p = d.adapter();
    let mut logs = listing_logs(d);
    logs.extend(activity_logs_pos(d, coll, debt));
    let st = store_after(&p, &logs);
    (p, st)
}

fn flash_sources() -> Vec<(CallbackShape, Address)> {
    CallbackShape::ALL
        .iter()
        .enumerate()
        .map(|(i, s)| (*s, Address::repeat_byte(0x50 + i as u8)))
        .collect()
}

fn extra_logs(d: &Deploy) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 2, T0);
    vec![
        log(
            d.silo0,
            &silo::AccruedInterest {
                accruedInterest: U256::ZERO,
            },
            b,
            t,
        ),
        log(d.silo1, &silo::FlashLoan { amount: U256::ONE }, b, t),
        log(
            d.hook,
            &hook::LiquidationCall {
                liquidator: Address::repeat_byte(0xee),
                silo: d.silo1,
                borrower: d.alice,
                repayDebtAssets: uint!(10_000_000_U256),
                withdrawCollateral: uint!(10_000_000_000_000_000_U256),
                receiveSToken: false,
            },
            b,
            t,
        ),
        log(d.hook, &hook::LiquidationStart { liquidationType: 0 }, b, t),
        log(
            d.silo1,
            &silo::Repay {
                sender: d.alice,
                owner: d.alice,
                assets: uint!(10_000_000_U256),
                shares: uint!(10_000_000_U256),
            },
            b,
            t,
        ),
        log(
            d.silo0,
            &silo::Withdraw {
                sender: d.bob,
                receiver: d.bob,
                owner: d.bob,
                assets: uint!(1_000_000_000_000_000_000_U256),
                shares: uint!(1_000_000_000_000_000_000_U256),
            },
            b,
            t,
        ),
        log(
            d.silo0,
            &silo::DepositProtected {
                sender: d.bob,
                owner: d.bob,
                assets: uint!(1_000_000_000_000_000_000_U256),
                shares: uint!(1_000_000_000_000_000_000_U256),
            },
            b,
            t,
        ),
        log(
            d.silo0,
            &silo::Transfer {
                from: d.bob,
                to: d.alice,
                value: uint!(1_000_000_000_000_000_U256),
            },
            b,
            t,
        ),
        log(
            d.debt1,
            &silo::Transfer {
                from: d.alice,
                to: d.bob,
                value: uint!(1_000_000_U256),
            },
            b,
            t,
        ),
        log(
            d.silo0,
            &silo::CollateralTypeChanged { borrower: d.alice },
            b,
            t,
        ),
        log(
            d.silo0,
            &silo::WithdrawnFees {
                daoFees: U256::ONE,
                deployerFees: U256::ZERO,
                redirectedDeployerFees: false,
            },
            b,
            t,
        ),
        log(
            d.silo0,
            &silo::DeployerFeesRedirected {
                deployerFees: U256::ZERO,
            },
            b,
            t,
        ),
        log(
            d.factory,
            &factory::NewSiloShareTokens {
                protectedShareToken: d.prot0,
                collateralShareToken: d.silo0,
                debtShareToken: d.debt0,
            },
            b,
            t,
        ),
        log(
            d.factory,
            &factory::NewSiloHook {
                silo: d.silo0,
                hook: d.hook,
            },
            b,
            t,
        ),
        log(
            d.hook,
            &halt::Upgraded {
                implementation: Address::repeat_byte(0x77),
            },
            DEPLOY_BLOCK,
            T0,
        ),
        log(
            d.hook,
            &halt::AdminChanged {
                previousAdmin: Address::ZERO,
                newAdmin: Address::repeat_byte(0x02),
            },
            DEPLOY_BLOCK,
            T0,
        ),
        log(d.hook, &halt::Initialized { version: 1 }, DEPLOY_BLOCK, T0),
    ]
}

#[test]
fn ten_checks_pass_with_nonvacuous_assertions() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_DEBT_OK);
    let px = prices(RAY_ONE, RAY_ONE);
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
    let mut owned = activity_logs(&d, ALICE_DEBT_OK);
    owned.extend(extra_logs(&d));
    let logs: Vec<LogFixture<'_>> = owned
        .iter()
        .map(|l| LogFixture {
            log: l.view(),
            max_dirty_rank: rank_of(&ranks, l.topics[0]),
        })
        .collect();
    let sources = flash_sources();
    let fx = Fixtures {
        positions: &positions,
        logs: &logs,
        flash_sources: &sources,
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
                    "healthy-only report: check {} has no quote (4/8/9 need post or liq)",
                    i + 1
                );
            }
            _ => assert!(*n > 0, "check {} was vacuous", i + 1),
        }
    }
    assert_eq!(rep.alloc_metered, alloc_meter().is_some());

    // Liquidatable (LTV ≥ 1e18, coll remaining) through `run` so checks 5/9/10
    // fire. Do not starve those checks with healthy-only fixtures.
    let (p_u, st_u) = full_store_pos(&d, ALICE_COLL_UNDER, ALICE_DEBT_UNDER);
    let px_u = prices(RAY_ONE, RAY_ONE);
    let h_u = p_u.health(st_u.view(ALICE_ID, T0).unwrap(), &px_u).unwrap();
    assert_eq!(h_u.state, HealthState::Liquidatable, "check 10 class");
    let q_u = p_u
        .quote(st_u.view(ALICE_ID, T0).unwrap(), &px_u)
        .unwrap()
        .expect("check 10: Liquidatable quotes");
    let from_curve = q_u.seize_options[0].curve.bonus_at_hf(h_u.hf).unwrap();
    assert_eq!(from_curve, Some(q_u.seize_options[0].bonus), "check 5");
    let positions_u = [PositionFixture {
        pos: st_u.view(ALICE_ID, T0).unwrap(),
        px: &px_u,
        post: None,
    }];
    let fx_u = Fixtures {
        positions: &positions_u,
        logs: &[],
        flash_sources: &sources,
        recipient: Address::repeat_byte(0x99),
    };
    let mut log_store_u = store_after(&p_u, &listing_logs(&d));
    let rep = run(
        &p_u,
        &mut log_store_u,
        &fx_u,
        alloc_meter().map(|m| m as &dyn Fn() -> u64),
    )
    .unwrap_or_else(|f| panic!("{f}"));
    assert!(rep.assertions[8] > 0, "check 9 must fire after 10E encode");
}

#[test]
fn health_matches_is_solvent_at_pin_math() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_DEBT_OK);
    let px = prices(RAY_ONE, RAY_ONE);
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(h.state, HealthState::Healthy);
    assert!(h.hf >= Ray::ONE);
    let liq = full_store(&d, ALICE_DEBT_LIQ);
    let hl = liq
        .0
        .health(liq.1.view(ALICE_ID, T0).unwrap(), &px)
        .unwrap();
    assert_eq!(hl.state, HealthState::Liquidatable);
    assert!(hl.hf < Ray::ONE);
}

#[test]
fn quote_static_bonus_and_encode_ok() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_DEBT_LIQ);
    let px = prices(RAY_ONE, RAY_ONE);
    let q = p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px)
        .unwrap()
        .expect("liquidatable");
    assert_eq!(q.repay_options[0].asset, DEBT);
    assert_eq!(q.seize_options[0].asset, COLL);
    assert!(matches!(
        q.seize_options[0].curve,
        liq_protocol::BonusCurve::Static { .. }
    ));
    assert_eq!(
        q.seize_options[0].bonus,
        math::bonus_ray(U256::from(FEE)).unwrap()
    );
    let rec = Address::repeat_byte(0x99);
    let route = FlashRoute {
        provider: CallbackShape::ALL[0].provider(),
        source: Address::repeat_byte(0x50),
        asset: DEBT,
        amount: q.repay_options[0].max_repay,
        fee_bps: 0,
        callback: CallbackShape::ALL[0],
    };
    let plan = p
        .encode(&q, LegChoice::PREFERRED, &route, rec)
        .expect("10E Silo encode");
    assert_eq!(plan.leg.adapter, ExecutorAdapter::SiloV2);
    assert_eq!(plan.leg.market, d.hook);
    assert_eq!(plan.leg.borrower, q.key.user);
    let mut q2 = q.clone();
    q2.key.protocol = liq_types::ProtocolId(99);
    assert_eq!(
        p.encode(&q2, LegChoice::PREFERRED, &route, rec),
        Err(ProtocolError::ProtocolMismatch)
    );
    assert_eq!(
        p.encode(&q, LegChoice { repay: 9, seize: 0 }, &route, rec),
        Err(ProtocolError::LegOutOfRange)
    );
    assert_eq!(
        p.encode(&q, LegChoice { repay: 0, seize: 9 }, &route, rec),
        Err(ProtocolError::LegOutOfRange)
    );
    let mut cb = route;
    cb.callback = CallbackShape::MorphoFlashCallback;
    assert_eq!(
        p.encode(&q, LegChoice::PREFERRED, &cb, rec),
        Err(ProtocolError::CallbackProviderMismatch)
    );
    assert_eq!(
        p.encode(&q, LegChoice::PREFERRED, &route, Address::ZERO),
        Err(ProtocolError::ZeroRecipient)
    );
    let mut mismatch = route;
    mismatch.asset = COLL;
    assert_eq!(
        p.encode(&q, LegChoice::PREFERRED, &mismatch, rec),
        Err(ProtocolError::FundingAssetMismatch)
    );
    let mut short = route;
    short.amount = U256::ZERO;
    assert_eq!(
        p.encode(&q, LegChoice::PREFERRED, &short, rec),
        Err(ProtocolError::FundingShort)
    );
    let mut huge = route;
    huge.amount = U256::MAX;
    assert_eq!(
        p.encode(&q, LegChoice::PREFERRED, &huge, rec),
        Err(ProtocolError::AmountTooLarge)
    );
}

#[test]
fn ltv_ge_one_with_remaining_collateral_is_liquidatable() {
    let d = Deploy::new();
    let (p, st) = full_store_pos(&d, ALICE_COLL_UNDER, ALICE_DEBT_UNDER);
    let px = prices(RAY_ONE, RAY_ONE);
    let pos = st.view(ALICE_ID, T0).unwrap();
    let h = p.health(pos, &px).unwrap();
    assert_eq!(h.state, HealthState::Liquidatable);
    assert!(h.hf < Ray::ONE);
    let tot_coll = BOB_COLL.checked_add(ALICE_COLL_UNDER).unwrap();
    let coll_assets =
        math::convert_to_assets(ALICE_COLL_UNDER, tot_coll, tot_coll, false, false).unwrap();
    let debt_assets = math::convert_to_assets(
        ALICE_DEBT_UNDER,
        ALICE_DEBT_UNDER,
        ALICE_DEBT_UNDER,
        true,
        true,
    )
    .unwrap();
    assert!(coll_assets > U256::ZERO);
    let coll_value = math::value_from_price(coll_assets, RAY_ONE, 18).unwrap();
    let debt_value = math::value_from_price(debt_assets, RAY_ONE, 6).unwrap();
    let ltv = math::ltv_math(debt_value, coll_value).unwrap();
    assert!(ltv >= math::BAD_DEBT_WAD);
    // Pin example: cover 50, coll 100, fee 4% → seize 52 (any-cover preview).
    assert_eq!(
        math::calculate_collateral_to_liquidate(uint!(50_U256), uint!(100_U256), U256::from(FEE))
            .unwrap(),
        uint!(52_U256)
    );
    let q = p
        .quote(pos, &px)
        .unwrap()
        .expect("maxLiquidation quotes when LTV >= 1e18 with coll remaining");
    let (seize, repay) = math::max_liquidation(
        coll_assets,
        coll_value,
        debt_assets,
        debt_value,
        U256::from(TARGET_LTV),
        U256::from(FEE),
    )
    .unwrap();
    assert_eq!(q.repay_options[0].max_repay, repay);
    assert_eq!(q.seize_options[0].max_seize, seize);
    assert!(repay > U256::ZERO);
    assert!(seize > U256::ZERO);
}

#[test]
fn zero_collateral_with_debt_is_bad_debt() {
    let d = Deploy::new();
    let (p, st) = full_store_pos(&d, U256::ZERO, ALICE_DEBT_OK);
    let px = prices(RAY_ONE, RAY_ONE);
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert!(matches!(h.state, HealthState::BadDebt { .. }));
    assert!(p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px)
        .unwrap()
        .is_none());
}

#[test]
fn unread_get_config_fails_closed() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut st = liq_protocol::conformance::JournalStore::new();
    st.push_market(MARKET, MarketRow::blank(COLL, 18)).unwrap();
    st.push_market(MARKET, MarketRow::blank(DEBT, 6)).unwrap();
    let pos = st
        .intern(&PositionKey {
            protocol: PROTOCOL,
            market: MARKET,
            user: d.alice,
        })
        .unwrap();
    st.set_supply(pos, 0, 1_000).unwrap();
    st.set_debt(pos, 1, 100).unwrap();
    let px = prices(RAY_ONE, RAY_ONE);
    assert_eq!(
        p.health(st.view(pos, T0).unwrap(), &px),
        Err(ProtocolError::OracleSourceMismatch)
    );
    let _ = MarketFlags::UNPRICED;
}

#[test]
fn halt_logs_fold_before_the_pin_and_error_after() {
    let d = Deploy::new();
    let (p, mut st) = full_store(&d, ALICE_DEBT_OK);
    let impl_ = Address::repeat_byte(0x77);
    let before = log(
        d.hook,
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
        d.hook,
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
fn new_silo_hook_mismatch_after_pin_halts() {
    let d = Deploy::new();
    let (p, mut st) = full_store(&d, ALICE_DEBT_OK);
    let ev = log(
        d.factory,
        &factory::NewSiloHook {
            silo: d.silo0,
            hook: Address::repeat_byte(0xff),
        },
        DEPLOY_BLOCK + 1,
        T0,
    );
    assert_eq!(
        p.apply_log(&mut st, &ev.view()),
        Err(ProtocolError::HaltSignal)
    );
}

#[test]
fn pin_liquidation_topic_is_not_watch_four_arg() {
    let pin = hook::LiquidationCall::SIGNATURE_HASH;
    let watch = liq_watch_silo_topic();
    assert_ne!(pin, watch);
}

fn liq_watch_silo_topic() -> alloy_primitives::B256 {
    use alloy_sol_types::sol;
    sol! {
        event LiquidationCall(address indexed liquidator, address indexed borrower, uint256 repayDebtAssets, uint256 withdrawCollateral);
    }
    LiquidationCall::SIGNATURE_HASH
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
        ];
        if halt_topics.contains(&f.topic0) {
            continue;
        }
        let _ = rank_of(&ranks, f.topic0);
    }
}

#[test]
fn probe_unavailable() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_DEBT_OK);
    assert_eq!(
        p.health_probe(st.view(ALICE_ID, T0).unwrap()).err(),
        Some(ProtocolError::ProbeUnavailable)
    );
}

#[test]
fn unexpected_emitter_is_refused() {
    let d = Deploy::new();
    let (p, mut st) = full_store(&d, ALICE_DEBT_OK);
    let ev = log(
        Address::repeat_byte(0x99),
        &silo::FlashLoan { amount: U256::ONE },
        DEPLOY_BLOCK + 1,
        T0,
    );
    assert_eq!(
        p.apply_log(&mut st, &ev.view()),
        Err(ProtocolError::UnexpectedLog)
    );
}

use common::OwnedLog;
