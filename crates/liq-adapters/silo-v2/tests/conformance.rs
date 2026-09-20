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
    CallbackShape, Constraints, DirtySet, FlashRoute, HealthState, LegChoice, MarketFlags,
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
    let p = d.adapter();
    let mut logs = listing_logs(d);
    logs.extend(activity_logs(d, debt));
    let st = store_after(&p, &logs);
    (p, st)
}

fn extra_logs(d: &Deploy) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 2, T0);
    vec![
        log(
            d.silo0,
            &silo::AccruedInterest {
                hooksBefore: U256::ZERO,
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
                    "check {} must stay skipped (no liquidatable encode)",
                    i + 1
                );
            }
            _ => assert!(*n > 0, "check {} was vacuous", i + 1),
        }
    }
    assert_eq!(rep.alloc_metered, alloc_meter().is_some());
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
fn quote_static_bonus_and_encode_unwired() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_DEBT_LIQ);
    let px = prices(RAY_ONE, RAY_ONE);
    let q = p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px, &Constraints::UNBOUNDED)
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
    let route = FlashRoute {
        provider: CallbackShape::ALL[0].provider(),
        source: Address::repeat_byte(0x50),
        asset: DEBT,
        amount: q.repay_options[0].max_repay,
        fee_bps: 0,
        callback: CallbackShape::ALL[0],
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
}

#[test]
fn full_liquidation_required_does_not_shrink_repay() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_DEBT_LIQ);
    let px = prices(RAY_ONE, RAY_ONE);
    let pos = st.view(ALICE_ID, T0).unwrap();
    let full = p
        .quote(pos, &px, &Constraints::UNBOUNDED)
        .unwrap()
        .expect("liquidatable");
    let repay = full.repay_options[0].max_repay;
    assert!(repay > U256::ZERO);
    let tiny = Constraints {
        per_liquidation_notional_cap: liq_types::Wad::from_raw(U256::from(1u8)),
    };
    let capped = p.quote(pos, &px, &tiny).unwrap().expect("still quotes");
    assert_eq!(capped.repay_options[0].max_repay, repay);
    assert!(math::full_liquidation_required(repay, U256::from(1u8)));
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
