//! GUIDE 01 §8 harness over the Aave V3 adapter.

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
use alloy_sol_types::{SolEvent, SolValue};
use common::*;
use liq_adapters_aave_v3::events::{halt, oracle, pool, token};
use liq_adapters_aave_v3::{alloc_meter, math};
use liq_protocol::conformance::{run, Fixtures, LogFixture, PositionFixture};
use liq_protocol::{
    CallbackShape, Constraints, DirtySet, ExecutorAdapter, FlashRoute, HealthState, LegChoice,
    Protocol, ProtocolError,
};
use liq_types::fixed::RAY;
use liq_types::{LogSubscriber, Ray, Wad};

const WAD: U256 = uint!(1_000_000_000_000_000_000_U256);
const YEAR: U256 = uint!(31_536_000_U256);

fn alice_by_hand(weth_p8: u64, dai_p8: u64, liq_idx: U256, debt_idx: U256) -> (U256, U256, U256) {
    let coll_assets = ALICE_WETH * liq_idx / RAY; // rayMulFloor at RAY is identity
    let coll_base = coll_assets * U256::from(weth_p8) / uint!(1_000_000_000_000_000_000_U256);
    let weighted = coll_base * U256::from(WETH_LT);
    let debt_assets = (ALICE_DAI_DEBT * debt_idx + RAY - U256::ONE) / RAY; // ceil
    let debt_base = (debt_assets * U256::from(dai_p8) + uint!(1_000_000_000_000_000_000_U256)
        - U256::ONE)
        / uint!(1_000_000_000_000_000_000_U256);
    let hf = (weighted * WAD + debt_base / U256::from(2u8)) / debt_base / uint!(10_000_U256);
    (hf, coll_base, debt_base)
}

fn full_store(
    d: &Deploy,
) -> (
    liq_adapters_aave_v3::AaveV3,
    liq_protocol::conformance::JournalStore,
) {
    let p = d.adapter();
    let mut logs = listing_logs(d);
    logs.extend(activity_logs(d));
    let st = store_after(&p, &logs);
    (p, st)
}

fn extra_logs(d: &Deploy) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 2, T1);
    vec![
        log(
            d.pool,
            &pool::ReserveDataUpdated {
                reserve: d.dai,
                liquidityRate: DAI_RATE,
                stableBorrowRate: U256::ZERO,
                variableBorrowRate: DAI_RATE,
                liquidityIndex: RAY,
                variableBorrowIndex: RAY + DAI_RATE * U256::from(T1 - T0) / YEAR,
            },
            b,
            t,
        ),
        log(
            d.configurator,
            &liq_adapters_aave_v3::events::cfg::ReservePaused {
                asset: d.weth,
                paused: true,
            },
            b,
            t,
        ),
        log(
            d.oracle,
            &oracle::AssetSourceUpdated {
                asset: d.weth,
                source: Address::repeat_byte(0xff),
            },
            b,
            t,
        ),
        log(
            d.oracle,
            &oracle::AssetSourceUpdated {
                asset: d.weth,
                source: d.src_weth,
            },
            b,
            t,
        ),
        log(
            d.pool,
            &pool::Repay {
                reserve: d.dai,
                user: d.alice,
                repayer: d.alice,
                amount: uint!(500_000_000_000_000_000_000_U256),
                useATokens: false,
            },
            b,
            t,
        ),
        log(
            d.pool,
            &pool::LiquidationCall {
                collateralAsset: d.weth,
                debtAsset: d.dai,
                user: d.alice,
                debtToCover: uint!(100_000_000_000_000_000_000_U256),
                liquidatedCollateralAmount: uint!(50_000_000_000_000_000_U256),
                liquidator: Address::repeat_byte(0xee),
                receiveAToken: true,
            },
            b,
            t,
        ),
        log(
            d.pool,
            &halt::Upgraded {
                implementation: Address::repeat_byte(0x77),
            },
            DEPLOY_BLOCK,
            T0,
        ),
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
    assert_eq!(rep.alloc_metered, alloc_meter().is_some());
}

#[test]
fn health_matches_hand_derivation_at_t0() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px = prices(1800_0000_0000, DAI_P8);
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    let (hf, coll, debt) = alice_by_hand(1800_0000_0000, DAI_P8, RAY, RAY);
    assert_eq!(h.hf, Ray::from_raw(hf * uint!(1_000_000_000_U256)));
    assert_eq!(
        h.collateral_value,
        Wad::from_raw(coll * uint!(10_000_000_000_U256))
    );
    assert_eq!(
        h.debt_value,
        Wad::from_raw(debt * uint!(10_000_000_000_U256))
    );
    assert_eq!(h.state, HealthState::Liquidatable);
}

#[test]
fn quote_static_bonus_and_encode() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px = prices(1800_0000_0000, DAI_P8);
    let pos = st.view(ALICE_ID, T0).unwrap();
    let q = p
        .quote(pos, &px, &Constraints::UNBOUNDED)
        .unwrap()
        .expect("liquidatable");
    assert_eq!(q.repay_options[0].asset, DAI);
    assert_eq!(q.seize_options[0].asset, WETH);
    assert_eq!(
        q.seize_options[0].bonus,
        Ray::from_raw(U256::from(500u16) * math::BPS_RAY)
    );
    assert!(matches!(
        q.seize_options[0].curve,
        liq_protocol::BonusCurve::Static { .. }
    ));
    let route = FlashRoute {
        provider: CallbackShape::AaveExecuteOperation.provider(),
        source: Address::repeat_byte(0x50),
        asset: DAI,
        amount: q.repay_options[0].max_repay,
        fee_bps: 0,
        callback: CallbackShape::AaveExecuteOperation,
    };
    let plan = p
        .encode(&q, LegChoice::PREFERRED, &route, Address::repeat_byte(0x99))
        .unwrap();
    assert_eq!(plan.leg.adapter, ExecutorAdapter::AaveV3);
    assert_eq!(plan.leg.market, d.pool);
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
fn oracle_source_swap_fails_closed() {
    let d = Deploy::new();
    let (p, mut st) = full_store(&d);
    let px = prices(1800_0000_0000, DAI_P8);
    let swap = log(
        d.oracle,
        &oracle::AssetSourceUpdated {
            asset: d.weth,
            source: Address::repeat_byte(0xff),
        },
        DEPLOY_BLOCK + 2,
        T0,
    );
    let dirty = p.apply_log(&mut st, &swap.view()).unwrap();
    assert!(matches!(dirty, DirtySet::MarketReprice(_)));
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
        d.pool,
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
        d.pool,
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
fn known_addr_unknown_topic_is_none() {
    let d = Deploy::new();
    let (p, mut st) = full_store(&d);
    let l = log(
        d.v_dai,
        &token::BorrowAllowanceDelegated {
            fromUser: d.alice,
            toUser: d.bob,
            asset: d.dai,
            amount: U256::ONE,
        },
        DEPLOY_BLOCK + 2,
        T0,
    );
    assert_eq!(p.apply_log(&mut st, &l.view()).unwrap(), DirtySet::None);
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
fn probe_decodes_get_user_account_data() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let call = p.health_probe(st.view(ALICE_ID, T0).unwrap()).unwrap();
    assert_eq!(call.to, d.pool);
    let ret = (
        U256::ONE,
        U256::ONE,
        U256::ZERO,
        U256::ZERO,
        U256::ZERO,
        uint!(960_000_000_000_000_000_U256),
    )
        .abi_encode();
    assert_eq!(
        (call.decode)(&ret).unwrap(),
        Ray::from_raw(uint!(960_000_000_000_000_000_000_000_000_U256))
    );
}

#[test]
fn encode_rejects_cross_protocol_quote() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px = prices(1800_0000_0000, DAI_P8);
    let q = p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px, &Constraints::UNBOUNDED)
        .unwrap()
        .unwrap();
    let route = FlashRoute {
        provider: CallbackShape::AaveExecuteOperation.provider(),
        source: Address::repeat_byte(0x50),
        asset: DAI,
        amount: q.repay_options[0].max_repay,
        fee_bps: 0,
        callback: CallbackShape::AaveExecuteOperation,
    };
    let mut q2 = q;
    q2.key.protocol = liq_types::ProtocolId(4);
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

use common::OwnedLog;
