//! GUIDE 01 harness + intern bind + pin seize math.

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

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolEvent;
use common::*;
use liq_adapters_compound_v2::config::{
    Config, ConfigError, INTERN_MARKET_MAX, INTERN_MARKET_MIN, OFFICIAL_MARKET, OFFICIAL_UNITROLLER,
};
use liq_adapters_compound_v2::events::halt;
use liq_adapters_compound_v2::events::{comptroller as cmp, ctoken, pause_global, pause_market};
use liq_adapters_compound_v2::layout::{CTokenRow, META_SLOT};
use liq_adapters_compound_v2::math;
use liq_adapters_compound_v2::{alloc_meter, CompoundV2};
use liq_config::{Intern, OnChainId, Registry};
use liq_protocol::conformance::{run, Fixtures, LogFixture, PositionFixture};
use liq_protocol::{
    CallbackShape, DirtySet, ExecutorAdapter, FlashRoute, HealthState, LegChoice, Protocol,
    ProtocolError, StateWriter,
};
use liq_types::{LogSubscriber, ProtocolId, Ray};

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
            d.cusdc,
            &ctoken::RepayBorrow {
                payer: d.alice,
                borrower: d.alice,
                repayAmount: U256::from(1u8),
                accountBorrows: ALICE_DEBT_OK.saturating_sub(U256::from(1u8)),
                totalBorrows: ALICE_DEBT_OK.saturating_sub(U256::from(1u8)),
            },
            b,
            t,
        ),
        log(
            d.ceth,
            &ctoken::Redeem {
                redeemer: d.bob,
                redeemAmount: U256::from(1u8),
                redeemTokens: U256::from(1u8),
            },
            b,
            t,
        ),
        log(
            d.cusdc,
            &ctoken::LiquidateBorrow {
                liquidator: d.bob,
                borrower: d.alice,
                repayAmount: U256::from(1u8),
                cTokenCollateral: d.ceth,
                seizeTokens: U256::from(1u8),
            },
            b,
            t,
        ),
        log(
            d.ceth,
            &ctoken::Transfer {
                from: d.bob,
                to: d.alice,
                amount: U256::from(1u8),
            },
            b,
            t,
        ),
        log(
            d.comptroller,
            &cmp::MarketExited {
                cToken: d.ceth,
                account: d.bob,
            },
            b,
            t,
        ),
        log(
            d.comptroller,
            &cmp::NewPriceOracle {
                oldPriceOracle: d.oracle,
                newPriceOracle: d.oracle,
            },
            b,
            t,
        ),
        log(
            d.comptroller,
            &pause_global::ActionPaused {
                action: String::from("Seize"),
                pauseState: false,
            },
            b,
            t,
        ),
        log(
            d.comptroller,
            &pause_market::ActionPaused {
                cToken: d.ceth,
                action: String::from("Borrow"),
                pauseState: false,
            },
            b,
            t,
        ),
        log(
            d.ceth,
            &halt::Upgraded {
                implementation: Address::repeat_byte(0x77),
            },
            DEPLOY_BLOCK,
            T0,
        ),
        log(
            d.ceth,
            &halt::AdminChanged {
                previousAdmin: Address::ZERO,
                newAdmin: Address::repeat_byte(0x02),
            },
            DEPLOY_BLOCK,
            T0,
        ),
        log(d.ceth, &halt::Initialized { version: 1 }, DEPLOY_BLOCK, T0),
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
                    *n, 0,
                    "healthy-only report: check {} has no quote (4/8 need post or liq; 9 inapplicable)",
                    i + 1
                );
            }
            _ => assert!(*n > 0, "check {} was vacuous", i + 1),
        }
    }
    assert_eq!(rep.alloc_metered, alloc_meter().is_some());

    let (p_u, st_u) = full_store(&d, ALICE_DEBT_LIQ);
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
fn health_from_shortfall_not_aave_hf() {
    let d = Deploy::new();
    let px = prices(RAY_ONE, RAY_ONE);
    let (p, st) = full_store(&d, ALICE_DEBT_OK);
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(h.state, HealthState::Healthy);
    assert!(h.hf >= Ray::ONE);
    let (pl, stl) = full_store(&d, ALICE_DEBT_LIQ);
    let hl = pl.health(stl.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(hl.state, HealthState::Liquidatable);
    assert!(hl.hf < Ray::ONE);
}

#[test]
fn quote_close_factor_capped_and_encode_ok() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_DEBT_LIQ);
    let px = prices(RAY_ONE, RAY_ONE);
    let q = p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px)
        .unwrap()
        .expect("liquidatable");
    assert_eq!(q.repay_options[0].asset, USDC);
    assert_eq!(q.seize_options[0].asset, NATIVE);
    let max_close = math::mul_scalar_truncate(U256::from(CLOSE_FACTOR), ALICE_DEBT_LIQ).unwrap();
    assert_eq!(q.repay_options[0].max_repay, max_close);
    assert!(matches!(
        q.seize_options[0].curve,
        liq_protocol::BonusCurve::Static { .. }
    ));
    assert_eq!(
        q.seize_options[0].bonus,
        math::bonus_ray(U256::from(INCENTIVE)).unwrap()
    );
    let rec = Address::repeat_byte(0x99);
    let route = FlashRoute {
        provider: CallbackShape::ALL[0].provider(),
        source: Address::repeat_byte(0x50),
        asset: USDC,
        amount: q.repay_options[0].max_repay,
        fee_bps: 0,
        callback: CallbackShape::ALL[0],
    };
    let plan = p
        .encode(&q, LegChoice::PREFERRED, &route, rec)
        .expect("10E Compound encode");
    assert_eq!(plan.leg.adapter, ExecutorAdapter::CompoundV2);
    assert_eq!(plan.leg.market, d.cusdc);
    assert_eq!(plan.leg.borrower, q.key.user);
    let mut q2 = q.clone();
    q2.key.protocol = ProtocolId(99);
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
    mismatch.asset = NATIVE;
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
fn seize_math_uses_config_incentive_not_108() {
    let incentive = U256::from(INCENTIVE);
    assert_ne!(incentive, uint_108());
    let pb = math::compound_price_mantissa(RAY_ONE, 18).unwrap();
    let pc = math::compound_price_mantissa(RAY_ONE, 18).unwrap();
    let er = U256::from(1_000_000_000_000_000_000u64);
    let repay = U256::from(10_000_000_000_000_000_000u64);
    let seize = math::liquidate_calculate_seize_tokens(incentive, pb, pc, er, repay).unwrap();
    let with_108 = math::liquidate_calculate_seize_tokens(uint_108(), pb, pc, er, repay).unwrap();
    assert_ne!(seize, with_108);
    // pin: ratio = incentive * pb / (pc * er); seize = ratio * repay / 1e18
    let num = math::mul_exp(incentive, pb).unwrap();
    let den = math::mul_exp(pc, er).unwrap();
    let ratio = math::div_exp(num, den).unwrap();
    assert_eq!(seize, math::mul_scalar_truncate(ratio, repay).unwrap());
}

fn uint_108() -> U256 {
    U256::from(1_080_000_000_000_000_000u64)
}

#[test]
fn cether_vs_cerc20_by_underlying_absence() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_DEBT_OK);
    let rows = st.markets(MARKET).unwrap();
    let mut saw_eth = false;
    let mut saw_erc = false;
    for (i, r) in rows.iter().enumerate() {
        if i == usize::from(META_SLOT) {
            continue;
        }
        let body: &CTokenRow = r.body().unwrap();
        if liq_adapters_compound_v2::math::addr_from(body.ctoken) == d.ceth {
            assert_ne!(body.flags & CTokenRow::CETHER, 0);
            assert_eq!(body.underlying, [0u8; 20]);
            saw_eth = true;
        }
        if liq_adapters_compound_v2::math::addr_from(body.ctoken) == d.cusdc {
            assert_eq!(body.flags & CTokenRow::CETHER, 0);
            assert_ne!(body.underlying, [0u8; 20]);
            saw_erc = true;
        }
    }
    assert!(saw_eth && saw_erc);
    let _ = p.id();
}

#[test]
fn watch_liquidate_borrow_topic0_matches() {
    let pin = ctoken::LiquidateBorrow::SIGNATURE_HASH;
    let watch = {
        use alloy_sol_types::sol;
        sol! {
            event LiquidateBorrow(address liquidator, address borrower, uint256 repayAmount, address cTokenCollateral, uint256 seizeTokens);
        }
        LiquidateBorrow::SIGNATURE_HASH
    };
    assert_eq!(pin, watch);
}

#[test]
fn committed_toml_without_intern_bind_fails_closed() {
    let raw = include_str!("../../../../config/protocols/compound-v2.toml");
    let cfg = Config::from_toml(raw).expect("compound-v2.toml parses");
    assert!(cfg.interned.is_empty());
    assert_eq!(
        CompoundV2::new(cfg).unwrap_err(),
        ConfigError::EmptyInterned
    );
}

#[test]
fn intern_binds_official_355_and_second_fork_in_range() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .unwrap();
    let intern =
        Intern::from_registry(&Registry::from_path(&root.join("registry/registry.json")).unwrap())
            .unwrap();
    let proto = intern.protocol("compound-v2").expect("compound-v2 family");
    assert_eq!(proto, PROTOCOL);
    let cv: Vec<&liq_config::MarketRec> = intern
        .markets()
        .iter()
        .filter(|m| m.protocol == proto)
        .collect();
    assert_eq!(cv.len(), 266);
    let official = cv
        .iter()
        .find(|m| m.key == OnChainId::Addr(OFFICIAL_UNITROLLER))
        .expect("official unitroller interned");
    assert_eq!(official.id, OFFICIAL_MARKET);
    assert_eq!(official.id.0, 355);
    let second = cv
        .iter()
        .find(|m| m.id != OFFICIAL_MARKET)
        .expect("second interned fork");
    assert!(
        second.id.0 >= INTERN_MARKET_MIN && second.id.0 <= INTERN_MARKET_MAX,
        "second fork {:?} outside 295..=560",
        second.id
    );
    assert!(cv
        .iter()
        .all(|m| m.id.0 >= INTERN_MARKET_MIN && m.id.0 <= INTERN_MARKET_MAX));
    assert!(
        cv.iter().all(|m| m.id.0 < 3508),
        "no Liquity/Euler/Fluid collision"
    );

    let raw = std::fs::read_to_string(root.join("config/protocols/compound-v2.toml")).unwrap();
    let parsed = Config::from_toml(&raw).expect("toml");
    assert!(parsed.interned.is_empty());
    assert_eq!(
        CompoundV2::new(parsed.clone()).unwrap_err(),
        ConfigError::EmptyInterned
    );
    let mut bound = parsed;
    bound.bind_from_intern(&intern).expect("bind_from_intern");
    assert_eq!(
        CompoundV2::new(bound.clone()).unwrap_err(),
        ConfigError::LiveRegistryUnasserted
    );
    assert_eq!(bound.interned.len(), 266);
    assert_eq!(
        bound.interned_id(OFFICIAL_UNITROLLER),
        Some(OFFICIAL_MARKET)
    );
    let loaded = Config::load(&root).expect("Config::load");
    assert_eq!(
        loaded.interned_id(OFFICIAL_UNITROLLER),
        Some(OFFICIAL_MARKET)
    );
    assert_eq!(loaded.interned.len(), 266);
}

#[test]
fn halt_logs_fold_before_the_pin_and_error_after() {
    let d = Deploy::new();
    let (p, mut st) = full_store(&d, ALICE_DEBT_OK);
    let before = log(
        d.ceth,
        &halt::Upgraded {
            implementation: Address::repeat_byte(0x77),
        },
        DEPLOY_BLOCK,
        T0,
    );
    assert_eq!(
        p.apply_log(&mut st, &before.view()).unwrap(),
        DirtySet::None
    );
    let after = log(
        d.ceth,
        &halt::Upgraded {
            implementation: Address::repeat_byte(0x77),
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
fn unexpected_emitter_is_refused() {
    let d = Deploy::new();
    let (p, mut st) = full_store(&d, ALICE_DEBT_OK);
    let ev = log(
        Address::repeat_byte(0x99),
        &ctoken::Mint {
            minter: d.alice,
            mintAmount: U256::ONE,
            mintTokens: U256::ONE,
        },
        DEPLOY_BLOCK + 1,
        T0,
    );
    assert_eq!(
        p.apply_log(&mut st, &ev.view()),
        Err(ProtocolError::UnexpectedLog)
    );
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
#[ignore = "live Comptroller views; set LIQ_RPC_URL"]
fn live_unitroller_matches_toml_or_fills() {
    let url = std::env::var("LIQ_RPC_URL").expect("LIQ_RPC_URL required for live registry assert");
    let raw = include_str!("../../../../config/protocols/compound-v2.toml");
    let mut cfg = Config::from_toml(raw).expect("compound-v2.toml");
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .unwrap();
    let intern =
        Intern::from_registry(&Registry::from_path(&root.join("registry/registry.json")).unwrap())
            .unwrap();
    cfg.bind_from_intern(&intern).expect("bind");
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
    impl liq_adapters_compound_v2::RegistryRpc for Live {
        fn eth_call(
            &self,
            to: Address,
            data: &[u8],
            block: u64,
        ) -> core::result::Result<alloy_primitives::Bytes, ConfigError> {
            use alloy_provider::Provider;
            use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
            let tx = TransactionRequest {
                to: Some(to.into()),
                input: TransactionInput::new(alloy_primitives::Bytes::copy_from_slice(data)),
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
        .expect("live Unitroller views");
    assert!(cfg.forks[0].close_factor_mantissa != 0);
    assert!(cfg.forks[0].liquidation_incentive_mantissa != 0);
    CompoundV2::new(cfg).expect("live-asserted config boots");
}
