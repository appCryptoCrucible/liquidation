//! GUIDE 01 harness over the Gearbox V3 adapter.

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

use alloy_primitives::{uint, Address, Bytes, U256};
use common::*;
use liq_adapters_gearbox::config::ConfigError;
use liq_adapters_gearbox::events::{configurator, facade, factory, halt, pool, quota};
use liq_adapters_gearbox::layout::{TokenRow, UNDERLYING_SLOT, UNMAPPED_ASSET};
use liq_adapters_gearbox::{alloc_meter, math, Config, GearboxV3, PROTOCOL};
use liq_protocol::conformance::{run, Fixtures, LogFixture, PositionFixture};
use liq_protocol::{
    CallbackShape, DirtySet, ExecutorAdapter, FlashRoute, HealthState, LegChoice, MarketSlot,
    Protocol, ProtocolError, StateWriter,
};
use liq_types::{LogSubscriber, PositionKey, PriceVector, Ray};

fn full_store(
    d: &Deploy,
    coll: U256,
    debt: U256,
) -> (GearboxV3, liq_protocol::conformance::JournalStore) {
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
            d.facade,
            &facade::StartMultiCall {
                creditAccount: d.alice,
                caller: d.alice,
            },
            b,
            t,
        ),
        log(
            d.facade,
            &facade::Execute {
                creditAccount: d.alice,
                targetContract: Address::repeat_byte(0x44),
            },
            b,
            t,
        ),
        log(d.facade, &facade::FinishMultiCall {}, b, t),
        log(
            d.pool,
            &pool::Repay {
                creditManager: d.manager,
                borrowedAmount: uint!(1_U256),
                profit: U256::ZERO,
                loss: U256::ZERO,
            },
            b,
            t,
        ),
        log(
            d.pool,
            &pool::SetInterestRateModel {
                newInterestRateModel: Address::repeat_byte(0x51),
            },
            b,
            t,
        ),
        log(
            d.configurator,
            &configurator::UpdateFees {
                feeLiquidation: FEE_LIQ,
                liquidationPremium: 500,
                feeLiquidationExpired: FEE_LIQ_EXP,
                liquidationPremiumExpired: 300,
            },
            b,
            t,
        ),
        log(
            d.configurator,
            &configurator::SetTokenLiquidationThreshold {
                token: d.coll,
                liquidationThreshold: LT_COLL,
            },
            b,
            t,
        ),
        log(
            d.configurator,
            &configurator::ForbidToken { token: d.coll },
            b,
            t,
        ),
        log(
            d.configurator,
            &configurator::SetLossPolicy {
                lossPolicy: Address::repeat_byte(0x61),
            },
            b,
            t,
        ),
        log(d.facade, &facade::Paused { account: d.facade }, b, t),
        log(d.facade, &facade::Unpaused { account: d.facade }, b, t),
        log(
            d.quota_keeper,
            &quota::UpdateTokenQuotaRate {
                token: d.coll,
                rate: 1,
            },
            b,
            t,
        ),
        log(
            d.factory,
            &factory::ReturnCreditAccount {
                creditAccount: d.alice,
                creditManager: d.manager,
            },
            b,
            t,
        ),
        log(
            d.facade,
            &halt::Upgraded {
                implementation: Address::repeat_byte(0x77),
            },
            DEPLOY_BLOCK,
            T0,
        ),
        log(
            d.configurator,
            &configurator::SetPriceOracle {
                priceOracle: Address::repeat_byte(0x88),
            },
            DEPLOY_BLOCK,
            T0,
        ),
        log(
            d.facade,
            &facade::LiquidateCreditAccount {
                creditAccount: Address::repeat_byte(0x12),
                liquidator: Address::repeat_byte(0xee),
                to: Address::repeat_byte(0x99),
                remainingFunds: U256::ZERO,
            },
            b,
            t,
        ),
    ]
}

#[test]
fn ten_checks_pass_with_nonvacuous_assertions() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_COLL, ALICE_DEBT_OK);
    let px = prices(RAY_ONE, RAY_ONE);
    let positions = [PositionFixture {
        pos: st.view(ALICE_ID, T0).unwrap(),
        px: &px,
        post: None,
    }];
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
                    "healthy-only report: check {} has no quote (9 inapplicable)",
                    i + 1
                );
            }
            _ => assert!(*n > 0, "check {} was vacuous", i + 1),
        }
    }
    assert_eq!(rep.alloc_metered, alloc_meter().is_some());

    let (p_u, st_u) = full_store(&d, ALICE_COLL_LIQ, ALICE_DEBT_LIQ);
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
fn health_unhealthy_and_expired_but_healthy() {
    let d = Deploy::new();
    let px = prices(RAY_ONE, RAY_ONE);
    let (p, st) = full_store(&d, ALICE_COLL, ALICE_DEBT_OK);
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(h.state, HealthState::Healthy);
    assert!(h.hf >= Ray::ONE);

    let (p_u, st_u) = full_store(&d, ALICE_COLL_LIQ, ALICE_DEBT_LIQ);
    let hu = p_u.health(st_u.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(hu.state, HealthState::Liquidatable);
    assert!(hu.hf < Ray::ONE);

    let mut cfg = d.config();
    cfg.managers[0] = d.manager_config(true, T0);
    let p_e = GearboxV3::new(cfg).unwrap();
    let mut logs = listing_logs(&d);
    logs.extend(activity_logs_pos(&d, ALICE_COLL, ALICE_DEBT_OK));
    let st_e = store_after(&p_e, &logs);
    let he = p_e.health(st_e.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(he.state, HealthState::Liquidatable);
    assert!(he.hf >= Ray::ONE);
    let q = p_e
        .quote(st_e.view(ALICE_ID, T0).unwrap(), &px)
        .unwrap()
        .expect("expired-but-healthy still quotes partial");
    assert_eq!(q.repay_options[0].asset, UNDERLYING);
    assert_eq!(q.seize_options[0].asset, COLL);
    assert_eq!(
        q.seize_options[0].bonus,
        math::bonus_ray(U256::from(DISCOUNT_EXP)).unwrap()
    );
}

#[test]
fn pull_fed_collateral_with_debt_is_not_a_candidate() {
    let d = Deploy::new();
    let (_plain, st) = full_store(&d, ALICE_COLL_LIQ, ALICE_DEBT_LIQ);
    let px = prices(RAY_ONE, RAY_ONE);
    let view = st.view(ALICE_ID, T0).unwrap();
    let mut cfg = d.config();
    cfg.managers[0].tokens[1].pull = true;
    cfg.managers[0].tokens[1]
        .pull_feeds
        .push(Address::repeat_byte(0xee));
    let blocked = GearboxV3::new(cfg).unwrap();
    assert!(
        blocked.quote(view, &px).unwrap().is_none(),
        "a pull leaf in the enabled mask produces no candidate"
    );
    let open = d.adapter();
    assert!(
        open.quote(view, &px).unwrap().is_some(),
        "the same account with only Chainlink feeds still quotes"
    );
}

#[test]
fn quote_partial_pin_math_and_full_unpriced() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_COLL_LIQ, ALICE_DEBT_LIQ);
    let px = prices(RAY_ONE, RAY_ONE);
    let q = p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px)
        .unwrap()
        .expect("liquidatable");
    assert_eq!(q.repay_options[0].asset, UNDERLYING);
    assert_eq!(q.seize_options[0].asset, COLL);
    let amount = q.repay_options[0].max_repay;
    let seized = math::seized_from_amount(
        amount,
        RAY_ONE,
        uint!(1_000_000_U256),
        RAY_ONE,
        uint!(1_000_000_000_000_000_000_U256),
        U256::from(DISCOUNT),
    )
    .unwrap();
    assert_eq!(q.seize_options[0].max_seize, seized);
    assert!(seized <= ALICE_COLL_LIQ);
    assert_eq!(
        q.seize_options[0].bonus,
        math::bonus_ray(U256::from(DISCOUNT)).unwrap()
    );
    // Partial window: the lower edge restores health (TW 85, D 88,
    // k = 0.985 − 0.85/0.95): ≈ 33.2 USDC, +1 % headroom.
    let lo = q.repay_options[0].min_repay;
    assert!(
        lo > uint!(33_000_000_U256) && lo < uint!(34_000_000_U256),
        "lo {lo}"
    );
    assert!(lo < amount);
    assert_eq!(q.repay_options[0].pair_seize, Some(0));
    // Full leg: all-or-nothing, pays totalValue · 0.95 + 0.5 % margin,
    // withdraws the whole balance less the 1 wei `withdraw(max)` leaves.
    assert_eq!(q.repay_options[1].min_repay, q.repay_options[1].max_repay);
    assert_eq!(q.repay_options[1].max_repay, uint!(95_500_000_U256));
    assert_eq!(q.repay_options[1].pair_seize, Some(1));
    assert_eq!(
        q.seize_options[1].max_seize,
        ALICE_COLL_LIQ - U256::from(1u8)
    );

    let (p0, st0) = full_store(&d, U256::ZERO, ALICE_DEBT_LIQ);
    let q0 = p0.quote(st0.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert!(
        q0.is_none(),
        "full MultiCall unpriced when no seizable non-underlying"
    );
}

/// A deep account: no partial slice restores health and the full close is
/// bad debt (loss policy) — no quote rather than a leg that reverts.
#[test]
fn quote_refuses_unrestorable_bad_debt_account() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_COLL_LIQ, ALICE_DEBT_DEEP);
    let px = prices(RAY_ONE, RAY_ONE);
    assert_eq!(
        p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap().state,
        HealthState::Liquidatable
    );
    assert!(p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px)
        .unwrap()
        .is_none());
}

/// `SetBorrowingLimits` raises `minDebt` to 60 USDC: a partial may leave
/// no less, which caps the slice below what restoring health needs — only
/// the full leg remains.
#[test]
fn min_debt_from_set_borrowing_limits_caps_partial() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut logs = listing_logs(&d);
    logs.extend(activity_logs_pos(&d, ALICE_COLL_LIQ, ALICE_DEBT_LIQ));
    logs.push(log(
        d.configurator,
        &configurator::SetBorrowingLimits {
            minDebt: uint!(60_000_000_U256),
            maxDebt: uint!(1_000_000_000_000_U256),
        },
        DEPLOY_BLOCK + 3,
        T0,
    ));
    let st = store_after(&p, &logs);
    let px = prices(RAY_ONE, RAY_ONE);
    let q = p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px)
        .unwrap()
        .expect("full leg still quotes");
    assert_eq!(q.repay_options.len(), 1, "partial dropped");
    assert_eq!(q.repay_options[0].min_repay, q.repay_options[0].max_repay);
}

#[test]
fn quote_static_bonus_and_encode_ok() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_COLL_LIQ, ALICE_DEBT_LIQ);
    let px = prices(RAY_ONE, RAY_ONE);
    let q = p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px)
        .unwrap()
        .expect("liquidatable");
    let rec = Address::repeat_byte(0x99);
    let route = FlashRoute {
        provider: CallbackShape::ALL[0].provider(),
        source: Address::repeat_byte(0x50),
        asset: UNDERLYING,
        amount: q.repay_options[0].max_repay,
        fee_bps: 0,
        callback: CallbackShape::ALL[0],
    };
    let plan = p
        .encode(&q, LegChoice::PREFERRED, &route, rec)
        .expect("10E Gearbox partial encode");
    assert_eq!(plan.leg.adapter, ExecutorAdapter::Gearbox);
    assert_eq!(plan.leg.market, d.facade);
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
    let mut full = q.clone();
    full.seize_options[0].asset = UNDERLYING;
    assert_eq!(
        p.encode(&full, LegChoice::PREFERRED, &route, rec),
        Err(ProtocolError::ExecutorUnwired)
    );
}

#[test]
fn missing_price_is_missing_price() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_COLL_LIQ, ALICE_DEBT_LIQ);
    let px = PriceVector(vec![liq_types::Price {
        asset: UNDERLYING,
        price: Ray::from_raw(RAY_ONE),
        source: liq_types::SourceKind::Canonical,
        block: DEPLOY_BLOCK,
        ts: T0,
    }]);
    assert_eq!(
        p.health(st.view(ALICE_ID, T0).unwrap(), &px),
        Err(ProtocolError::MissingPrice(COLL))
    );
}

/// Oracle: the band is 64000..=64999 and sits below the store's `u16`
/// market table (`liq_protocol::MARKET_ID_LIMIT`). Negative: the old
/// 71000.. band passed this test's own range check but every
/// `push_market` on the real store (and now on `JournalStore`) refused it.
#[test]
fn market_ids_stay_in_band() {
    let d = Deploy::new();
    let cfg = d.config();
    assert_eq!(cfg.catalog.0, 64_000);
    const { assert!(liq_adapters_gearbox::LAST_MANAGER_MARKET.0 < liq_protocol::MARKET_ID_LIMIT) };
    for m in &cfg.managers {
        assert!(m.market.0 >= 64_001 && m.market.0 <= 64_999);
        assert_ne!(m.market.0, 64_000);
    }
}

#[test]
fn new_refuses_unasserted_fees() {
    let raw = include_str!("../../../../config/protocols/gearbox.toml");
    let cfg = Config::from_toml(raw).expect("gearbox.toml");
    assert!(!cfg.live_fees_asserted);
    assert_eq!(cfg.protocol, PROTOCOL);
    assert_eq!(
        cfg.address_provider,
        alloy_primitives::address!("0xF7f0a609BfAb9a0A98786951ef10e5FE26cC1E38"),
        "managers come from the v3.1 address provider"
    );
    assert!(cfg.register.is_zero());
    assert_eq!(
        GearboxV3::new(cfg).unwrap_err(),
        ConfigError::LiveFeesUnasserted
    );
    let d = Deploy::new();
    let mut un = d.config();
    un.live_fees_asserted = false;
    assert_eq!(
        GearboxV3::new(un).unwrap_err(),
        ConfigError::LiveFeesUnasserted
    );
}

#[test]
fn assert_live_registry_refuses_count_mismatch_and_keeps_flag_false() {
    let d = Deploy::new();
    let raw = include_str!("../../../../config/protocols/gearbox.toml");
    let mut cfg = Config::from_toml(raw).expect("gearbox.toml");
    // Single-register path with a pinned cardinality (the v3.0 D15 count).
    cfg.address_provider = alloy_primitives::Address::ZERO;
    cfg.register = d.register;
    cfg.expected_managers = 34;
    let rpc = mock_registry(&d);
    let err = cfg
        .assert_live_registry(&rpc, cfg.pinned_through)
        .expect_err("toml expects 34");
    match err {
        ConfigError::ManagerCount { expected, found } => {
            assert_eq!(expected, 34);
            assert_eq!(found, 1);
        }
        other => panic!("expected ManagerCount, got {other:?}"),
    }
    assert!(!cfg.live_fees_asserted);
    assert_eq!(
        GearboxV3::new(cfg).unwrap_err(),
        ConfigError::LiveFeesUnasserted
    );
}

#[test]
fn assert_live_registry_fees_then_new() {
    let d = Deploy::new();
    let mut cfg = d.config();
    cfg.live_fees_asserted = false;
    cfg.managers.clear();
    let rpc = mock_registry(&d);
    cfg.assert_live_registry(&rpc, DEPLOY_BLOCK)
        .expect("mock fees()");
    assert!(cfg.live_fees_asserted);
    assert_eq!(cfg.managers.len(), 1);
    assert_eq!(cfg.managers[0].fees.liquidation_discount, DISCOUNT);
    assert_eq!(cfg.managers[0].market.0, 64_001);
    // G2. `ltParams(underlying)` reads an unwritten storage slot on the real
    // contract and the mock now returns 0 for it (see the comment at the
    // call site in `common/mod.rs`), so a correct `lt_underlying` here proves
    // it came from `fees`, not from that dead slot.
    assert_eq!(
        cfg.managers[0].lt_underlying,
        DISCOUNT - FEE_LIQ,
        "lt_underlying must derive from fees, not ltParams(underlying)"
    );
    for t in &cfg.managers[0].tokens {
        assert_eq!(t.ramp_start, math::STATIC_LT_RAMP_START);
        assert!(t.ramp_start > u64::from(u32::MAX));
    }
    GearboxV3::new(cfg).expect("asserted config boots");
}

/// G2. `UpdateFees` must re-derive `lt_underlying` on every fold, not only
/// at initial listing — a stale value would drift the moment fees change,
/// even after the listing-time fix.
#[test]
fn update_fees_rederives_lt_underlying() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut st = store_after(&p, &listing_logs(&d));
    let before: &liq_adapters_gearbox::layout::ManagerRow = st
        .market(MarketSlot {
            market: MARKET,
            slot: liq_adapters_gearbox::layout::UNDERLYING_SLOT,
        })
        .unwrap()
        .body()
        .unwrap();
    assert_eq!(before.lt_underlying, DISCOUNT - FEE_LIQ);

    // Different fees than the listing-time config: premium 1200 -> discount
    // 8800, fee 300 -> lt_underlying should become 8500, not stay at the
    // listing-time 9350 and not go to 0.
    let ev = log(
        d.configurator,
        &configurator::UpdateFees {
            feeLiquidation: 300,
            liquidationPremium: 1200,
            feeLiquidationExpired: FEE_LIQ_EXP,
            liquidationPremiumExpired: 300,
        },
        DEPLOY_BLOCK + 1,
        T0,
    );
    p.apply_log(&mut st, &ev.view()).unwrap();
    let after: &liq_adapters_gearbox::layout::ManagerRow = st
        .market(MarketSlot {
            market: MARKET,
            slot: liq_adapters_gearbox::layout::UNDERLYING_SLOT,
        })
        .unwrap()
        .body()
        .unwrap();
    assert_eq!(
        after.lt_underlying, 8_500,
        "lt_underlying must track fees on every fold, not just at listing"
    );
}

#[test]
fn assert_live_registry_accepts_uint40_max_static_lt_then_new() {
    let d = Deploy::new();
    let mut cfg = d.config();
    cfg.live_fees_asserted = false;
    cfg.managers.clear();
    let rpc = mock_registry(&d);
    cfg.assert_live_registry(&rpc, DEPLOY_BLOCK)
        .expect("pin static LT type(uint40).max is not truncation");
    assert!(cfg.live_fees_asserted);
    for t in &cfg.managers[0].tokens {
        assert_eq!(t.ramp_start, math::STATIC_LT_RAMP_START);
        assert!(t.ramp_start > u64::from(u32::MAX));
    }
    let p = GearboxV3::new(cfg).expect("asserted config boots");
    let mut st = store_after(&p, &listing_logs(&d));
    let l = log(
        d.configurator,
        &configurator::SetTokenLiquidationThreshold {
            token: d.coll,
            liquidationThreshold: LT_COLL,
        },
        DEPLOY_BLOCK + 1,
        T0,
    );
    p.apply_log(&mut st, &l.view())
        .expect("static LT event folds");
    let row = st
        .market(MarketSlot {
            market: MARKET,
            slot: 1,
        })
        .expect("coll token row");
    let tok: &TokenRow = row.body().expect("TokenRow");
    assert_eq!(tok.ramp_start, math::STATIC_LT_RAMP_START);
    assert_eq!(
        math::get_liquidation_threshold(
            tok.lt_initial,
            tok.lt_final,
            tok.ramp_start,
            tok.ramp_duration,
            T0
        )
        .expect("static LT"),
        LT_COLL
    );
}

#[test]
fn health_probe_targets_manager_calc_debt_and_collateral() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_COLL, ALICE_DEBT_OK);
    let probe = p.health_probe(st.view(ALICE_ID, T0).unwrap()).unwrap();
    assert_eq!(probe.to, d.manager);
    assert!(!probe.data.is_empty());
}

#[test]
fn apply_log_journaled_and_subscriptions_nonempty() {
    let d = Deploy::new();
    let p = d.adapter();
    assert!(!p.subscriptions().is_empty());
    let mut st = store_after(&p, &listing_logs(&d));
    let logs = activity_logs(&d, ALICE_DEBT_OK);
    for l in &logs {
        let dirty = p.apply_log(&mut st, &l.view()).unwrap();
        assert!(matches!(dirty, DirtySet::Positions(_)));
    }
    let pos = st.view(ALICE_ID, T0).unwrap();
    assert_eq!(
        *pos.key,
        PositionKey {
            protocol: PROTOCOL,
            market: MARKET,
            user: d.alice,
        }
    );
    assert!(st.debt(ALICE_ID, UNDERLYING_SLOT).unwrap() > 0);
    let _ = UNMAPPED_ASSET;
}

#[test]
fn halt_after_pin() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut st = store_after(&p, &listing_logs(&d));
    let l = log(
        d.facade,
        &halt::Upgraded {
            implementation: Address::repeat_byte(0x77),
        },
        DEPLOY_BLOCK + 1,
        T0,
    );
    assert_eq!(
        p.apply_log(&mut st, &l.view()),
        Err(ProtocolError::HaltSignal)
    );
}

#[test]
#[ignore = "live ContractsRegister getCreditManagers + fees(); set LIQ_RPC_URL"]
fn live_contracts_register_fees() {
    let url = std::env::var("LIQ_RPC_URL").expect("LIQ_RPC_URL required for live registry assert");
    let raw = include_str!("../../../../config/protocols/gearbox.toml");
    let mut cfg = Config::from_toml(raw).expect("gearbox.toml");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio rt");
    let provider = alloy_provider::ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(url.parse().expect("LIQ_RPC_URL parses"));
    struct Live {
        provider: alloy_provider::RootProvider,
        rt: tokio::runtime::Runtime,
    }
    impl liq_adapters_gearbox::RegistryRpc for Live {
        fn eth_call(
            &self,
            to: Address,
            data: &[u8],
            block: u64,
        ) -> core::result::Result<Bytes, ConfigError> {
            use alloy_provider::Provider;
            use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
            let tx = TransactionRequest {
                to: Some(to.into()),
                input: TransactionInput::new(Bytes::copy_from_slice(data)),
                ..Default::default()
            };
            let out = self
                .rt
                .block_on(async { self.provider.call(tx).number(block).await })
                .map_err(|_| ConfigError::RegistryCall(to))?;
            if out.is_empty() {
                return Err(ConfigError::RegistryCall(to));
            }
            Ok(out)
        }
    }
    let rpc = Live { provider, rt };
    cfg.assert_live_registry(&rpc, cfg.pinned_through)
        .expect("live getCreditManagers cardinality and fees() must succeed");
    assert_eq!(cfg.managers.len(), 34);
    GearboxV3::new(cfg).expect("live-asserted config boots");
}

/// Answers for one stale account: `creditAccountInfo` (`borrower`, `debt`,
/// enabled `mask`) then the follow-ups the adapter asks for, each answered
/// from `balances` by token address. `skip` drops that token's answer.
fn settle_reads(
    p: &GearboxV3,
    st: &liq_protocol::conformance::JournalStore,
    borrower: Address,
    debt: U256,
    mask: U256,
    balances: &[(Address, U256)],
    skip: Option<Address>,
) -> (Vec<liq_protocol::StateRead>, Vec<(liq_protocol::StateRead, Vec<u8>)>) {
    use alloy_sol_types::SolCall;
    use liq_adapters_gearbox::events::views::{ICreditManagerV3, IPoolQuotaKeeperV3, IERC20};
    let first = p.position_reads(st.view(ALICE_ID, T0).unwrap());
    let mut answered = Vec::new();
    for r in &first {
        let info = ICreditManagerV3::creditAccountInfoCall::abi_encode_returns(
            &ICreditManagerV3::creditAccountInfoReturn {
                debt,
                cumulativeIndexLastUpdate: RAY_ONE,
                cumulativeQuotaInterest: 0,
                quotaFees: 0,
                enabledTokensMask: mask,
                flags: 0,
                lastDebtUpdate: DEPLOY_BLOCK + 2,
                borrower,
            },
        );
        let follow = p.state_follow_ups(liq_protocol::StateAnswer {
            read: r,
            success: true,
            data: &info,
        });
        answered.push((r.clone(), info));
        for f in follow {
            if IPoolQuotaKeeperV3::getQuotaCall::abi_decode(&f.calldata).is_ok() {
                let ret = IPoolQuotaKeeperV3::getQuotaCall::abi_encode_returns(
                    &IPoolQuotaKeeperV3::getQuotaReturn {
                        quota: alloy_primitives::aliases::U96::from(QUOTA as u128),
                        cumulativeIndexLU: alloy_primitives::aliases::U192::from(RAY_ONE),
                    },
                );
                assert_eq!(f.target, Deploy::new().quota_keeper);
                answered.push((f, ret));
                continue;
            }
            if Some(f.target) == skip {
                continue;
            }
            let bal = balances
                .iter()
                .find(|(t, _)| *t == f.target)
                .map_or(U256::ZERO, |(_, b)| *b);
            answered.push((f, IERC20::balanceOfCall::abi_encode_returns(&bal)));
        }
    }
    (first, answered)
}

fn fold_answers(
    p: &GearboxV3,
    st: &mut liq_protocol::conformance::JournalStore,
    answered: &[(liq_protocol::StateRead, Vec<u8>)],
) -> Vec<DirtySet> {
    let answers: Vec<liq_protocol::StateAnswer<'_>> = answered
        .iter()
        .map(|(r, d)| liq_protocol::StateAnswer {
            read: r,
            success: true,
            data: d,
        })
        .collect();
    p.apply_state_reads(st, T0, &answers).unwrap()
}

/// Oracle: `CreditFacadeV3._multicall` @ `510fc654` emits only
/// `StartMultiCall`/`Execute`/`FinishMultiCall`; adapter swaps and
/// `decreaseDebt` (pool `Repay` names no account) leave no amounts. The
/// account's state after the multicall is the chain's `creditAccountInfo`
/// plus `balanceOf` per enabled token — and with those values health must
/// equal what the log-only fixture computes for the same balances and debt.
/// Negative: before the fix `StartMultiCall` was `DirtySet::None` and health
/// kept the pre-multicall collateral (Healthy) for an account the chain
/// holds liquidatable.
#[test]
fn multicall_is_settled_by_chain_reads() {
    let d = Deploy::new();
    let px = prices(RAY_ONE, RAY_ONE);
    let (p, mut st) = full_store(&d, ALICE_COLL, ALICE_DEBT_OK);
    assert!(p.position_reads(st.view(ALICE_ID, T0).unwrap()).is_empty());

    let start = log(
        d.facade,
        &facade::StartMultiCall {
            creditAccount: d.alice,
            caller: d.alice,
        },
        DEPLOY_BLOCK + 2,
        T0,
    );
    assert_eq!(
        p.apply_log(&mut st, &start.view()),
        Ok(DirtySet::Positions(liq_protocol::DirtyPositions::from_slice(&[ALICE_ID])))
    );
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(
        h.state,
        HealthState::Blocked {
            reason: liq_protocol::BlockReason::Unread
        }
    );
    assert_eq!(p.quote(st.view(ALICE_ID, T0).unwrap(), &px).unwrap(), None);

    // The multicall swapped half the collateral away and borrowed more.
    let bals = [(d.underlying, U256::ZERO), (d.coll, ALICE_COLL_LIQ)];
    let (first, answered) = settle_reads(
        &p,
        &st,
        d.alice,
        ALICE_DEBT_LIQ,
        U256::from(0b11),
        &bals,
        Some(d.coll),
    );
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].target, d.manager);
    assert_eq!(answered.len(), 3, "info + underlying + coll quota; coll balance dropped");
    assert_eq!(fold_answers(&p, &mut st, &answered), vec![]);
    assert!(
        !p.position_reads(st.view(ALICE_ID, T0).unwrap()).is_empty(),
        "incomplete reads leave the account stale"
    );

    let (_, answered) = settle_reads(
        &p,
        &st,
        d.alice,
        ALICE_DEBT_LIQ,
        U256::from(0b11),
        &bals,
        None,
    );
    assert_eq!(answered.len(), 4);
    assert_eq!(
        fold_answers(&p, &mut st, &answered),
        vec![DirtySet::Positions(liq_protocol::DirtyPositions::from_slice(&[ALICE_ID]))]
    );
    assert!(p.position_reads(st.view(ALICE_ID, T0).unwrap()).is_empty());
    let settled = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    let (p_ref, st_ref) = full_store(&d, ALICE_COLL_LIQ, ALICE_DEBT_LIQ);
    let reference = p_ref.health(st_ref.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(settled.state, HealthState::Liquidatable);
    assert_eq!(settled.hf, reference.hf);
    assert_eq!(settled.debt_value, reference.debt_value);
    assert_eq!(settled.collateral_value, reference.collateral_value);
}

/// Oracle: a closed account's `creditAccountInfo.borrower` is zero
/// (`CreditManagerV3.closeCreditAccount` deletes the borrower). Negative: the
/// account must not keep its stale balances.
#[test]
fn closed_account_reads_zero_the_position() {
    let d = Deploy::new();
    let px = prices(RAY_ONE, RAY_ONE);
    let (p, mut st) = full_store(&d, ALICE_COLL, ALICE_DEBT_OK);
    let start = log(
        d.facade,
        &facade::StartMultiCall {
            creditAccount: d.alice,
            caller: d.alice,
        },
        DEPLOY_BLOCK + 2,
        T0,
    );
    p.apply_log(&mut st, &start.view()).unwrap();
    let (_, answered) = settle_reads(&p, &st, Address::ZERO, U256::ZERO, U256::ZERO, &[], None);
    assert_eq!(answered.len(), 1, "no balance reads for a closed account");
    fold_answers(&p, &mut st, &answered);
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(h.state, HealthState::Healthy);
    assert_eq!(st.supply(ALICE_ID, 1).unwrap(), 0);
    assert_eq!(st.debt(ALICE_ID, UNDERLYING_SLOT).unwrap(), 0);
    assert!(p.position_reads(st.view(ALICE_ID, T0).unwrap()).is_empty());
}

/// Oracle: a reorg can unwind a position's creation and the store hands the
/// id to the next account interned. A read issued for the old account must
/// not settle the new one. Negative: matching on the tag's id alone writes
/// account A's debt and balances into account B.
#[test]
fn reads_for_another_account_are_not_folded() {
    use alloy_sol_types::SolCall;
    use liq_adapters_gearbox::events::views::ICreditManagerV3;
    let d = Deploy::new();
    let (p, mut st) = full_store(&d, ALICE_COLL, ALICE_DEBT_OK);
    let start = log(
        d.facade,
        &facade::StartMultiCall {
            creditAccount: d.alice,
            caller: d.alice,
        },
        DEPLOY_BLOCK + 2,
        T0,
    );
    p.apply_log(&mut st, &start.view()).unwrap();
    let bals = [(d.underlying, U256::ZERO), (d.coll, ALICE_COLL_LIQ)];
    let (_, mut answered) = settle_reads(
        &p,
        &st,
        d.alice,
        ALICE_DEBT_LIQ,
        U256::from(0b11),
        &bals,
        None,
    );
    answered[0].0.calldata = ICreditManagerV3::creditAccountInfoCall {
        creditAccount: Address::repeat_byte(0x77),
    }
    .abi_encode()
    .into();
    assert_eq!(fold_answers(&p, &mut st, &answered), vec![]);
    assert_eq!(st.supply(ALICE_ID, 1).unwrap(), u128::try_from(ALICE_COLL).unwrap());
    assert!(!p.position_reads(st.view(ALICE_ID, T0).unwrap()).is_empty());
}

/// Answers for the per-block interest reads, from fixed pool / keeper state.
fn interest_answers(p: &GearboxV3) -> Vec<(liq_protocol::StateRead, Vec<u8>)> {
    use alloy_primitives::aliases::{U192, U40, U96};
    use alloy_sol_types::SolCall;
    use liq_adapters_gearbox::events::views::{IPoolQuotaKeeperV3, IPoolV3};
    struct NoRows;
    impl liq_protocol::MarketRows for NoRows {
        fn rows(&self, _: liq_types::MarketId) -> Option<&[liq_protocol::MarketRow]> {
            None
        }
    }
    p.state_reads(&NoRows)
        .into_iter()
        .map(|r| {
            let c = &r.calldata;
            let ret = if IPoolV3::baseInterestIndexLUCall::abi_decode(c).is_ok() {
                IPoolV3::baseInterestIndexLUCall::abi_encode_returns(&POOL_INDEX_LU)
            } else if IPoolV3::baseInterestRateCall::abi_decode(c).is_ok() {
                IPoolV3::baseInterestRateCall::abi_encode_returns(&POOL_RATE)
            } else if IPoolV3::lastBaseInterestUpdateCall::abi_decode(c).is_ok() {
                IPoolV3::lastBaseInterestUpdateCall::abi_encode_returns(&U40::from(T0 - 1_000))
            } else if IPoolQuotaKeeperV3::lastQuotaRateUpdateCall::abi_decode(c).is_ok() {
                IPoolQuotaKeeperV3::lastQuotaRateUpdateCall::abi_encode_returns(&U40::from(
                    T0 - 2_000,
                ))
            } else if IPoolQuotaKeeperV3::getTokenQuotaParamsCall::abi_decode(c).is_ok() {
                IPoolQuotaKeeperV3::getTokenQuotaParamsCall::abi_encode_returns(
                    &IPoolQuotaKeeperV3::getTokenQuotaParamsReturn {
                        rate: 100,
                        cumulativeIndexLU: U192::from(TOKEN_INDEX_LU),
                        quotaIncreaseFee: 0,
                        totalQuoted: U96::ZERO,
                        limit: U96::MAX,
                        isActive: true,
                    },
                )
            } else {
                panic!("unexpected interest read {r:?}")
            };
            (r, ret)
        })
        .collect()
}

const POOL_INDEX_LU: U256 = uint!(1_020_000_000_000_000_000_000_000_000_U256);
const POOL_RATE: U256 = uint!(50_000_000_000_000_000_000_000_000_U256);
const TOKEN_INDEX_LU: U256 = uint!(1_001_000_000_000_000_000_000_000_000_U256);

/// Oracle: the total debt `CreditManagerV3._calcDebtAndCollateral` @
/// `510fc654` reports a day after the reads, computed outside this crate in
/// exact integers from the pin's formulas: `PoolV3._calcBaseInterestIndex`
/// (`indexLU * (RAY + rate * dt / YEAR) / RAY`), `QuotasLogic.cumulativeIndexSince`
/// (`LU + RAY/1e4 * dt * rate / YEAR`), `calcAccruedQuotaInterest`, and
/// `accruedFees = quotaFees + base * feeInterest / 1e4 + quota * feeInterest / 1e4`.
/// Debt 50e6 at account index RAY; pool LU 1.02 RAY at 5%/yr updated
/// `T0-1000`; coll quota 1e15 at 100 bps from 1.001 RAY updated `T0-2000`,
/// account quota index RAY; `cumulativeQuotaInterest` the 1-wei sentinel.
/// Total 1_130_885_709_497. Negative: before this change the adapter held
/// the index fixed and reported the bare 50e6 forever.
#[test]
fn debt_accrues_base_and_quota_interest_from_reads() {
    let d = Deploy::new();
    let px = prices(RAY_ONE, RAY_ONE);
    let (p, mut st) = full_store(&d, ALICE_COLL, ALICE_DEBT_OK);
    let start = log(
        d.facade,
        &facade::StartMultiCall {
            creditAccount: d.alice,
            caller: d.alice,
        },
        DEPLOY_BLOCK + 2,
        T0,
    );
    p.apply_log(&mut st, &start.view()).unwrap();
    let bals = [(d.underlying, U256::ZERO), (d.coll, ALICE_COLL)];
    let (_, mut answered) = settle_reads(
        &p,
        &st,
        d.alice,
        ALICE_DEBT_OK,
        U256::from(0b11),
        &bals,
        None,
    );
    let reads = interest_answers(&p);
    assert_eq!(reads.len(), 5, "pool x3, keeper, one quoted token");
    answered.extend(reads);
    let sets = fold_answers(&p, &mut st, &answered);
    assert_eq!(sets.len(), 2, "interest rows and the settled account: {sets:?}");
    assert!(matches!(sets[0], DirtySet::MarketAccrual(_)));

    let at_read = p.health(st.view(ALICE_ID, T0 - 1_000).unwrap(), &px).unwrap();
    let day = p.health(st.view(ALICE_ID, T0 + 86_400).unwrap(), &px).unwrap();
    let want = math::value_wad(uint!(1_130_885_709_497_U256), RAY_ONE, 6).unwrap();
    assert_eq!(day.debt_value.raw(), want);
    assert!(day.debt_value > at_read.debt_value, "debt grows with time");
}
