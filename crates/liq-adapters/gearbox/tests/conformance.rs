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
    CallbackShape, Constraints, DirtySet, ExecutorAdapter, FlashRoute, HealthState, LegChoice,
    MarketSlot, Protocol, ProtocolError, StateWriter,
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
        .quote(
            st_u.view(ALICE_ID, T0).unwrap(),
            &px_u,
            &Constraints::UNBOUNDED,
        )
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
        .quote(
            st_e.view(ALICE_ID, T0).unwrap(),
            &px,
            &Constraints::UNBOUNDED,
        )
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
fn quote_partial_pin_math_and_full_unpriced() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_COLL_LIQ, ALICE_DEBT_LIQ);
    let px = prices(RAY_ONE, RAY_ONE);
    let q = p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px, &Constraints::UNBOUNDED)
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

    let (p0, st0) = full_store(&d, U256::ZERO, ALICE_DEBT_LIQ);
    let q0 = p0
        .quote(
            st0.view(ALICE_ID, T0).unwrap(),
            &px,
            &Constraints::UNBOUNDED,
        )
        .unwrap();
    assert!(
        q0.is_none(),
        "full MultiCall unpriced when no seizable non-underlying"
    );
}

#[test]
fn quote_static_bonus_and_encode_ok() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_COLL_LIQ, ALICE_DEBT_LIQ);
    let px = prices(RAY_ONE, RAY_ONE);
    let q = p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px, &Constraints::UNBOUNDED)
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

#[test]
fn market_ids_stay_in_band() {
    let d = Deploy::new();
    let cfg = d.config();
    assert_eq!(cfg.catalog.0, 4200);
    for m in &cfg.managers {
        assert!(m.market.0 >= 4201 && m.market.0 <= 4299);
        assert_ne!(m.market.0, 4200);
    }
}

#[test]
fn new_refuses_unasserted_fees() {
    let raw = include_str!("../../../../config/protocols/gearbox.toml");
    let cfg = Config::from_toml(raw).expect("gearbox.toml");
    assert!(!cfg.live_fees_asserted);
    assert_eq!(cfg.protocol, PROTOCOL);
    assert_eq!(cfg.expected_managers, 34);
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
    cfg.register = d.register;
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
    assert_eq!(cfg.managers[0].market.0, 4201);
    for t in &cfg.managers[0].tokens {
        assert_eq!(t.ramp_start, math::STATIC_LT_RAMP_START);
        assert!(t.ramp_start > u64::from(u32::MAX));
    }
    GearboxV3::new(cfg).expect("asserted config boots");
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
