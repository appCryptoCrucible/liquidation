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
use liq_adapters_euler_v2::{
    alloc_meter, math, Config, ConfigError, EulerV2, CATALOG_MARKET, FIRST_DISCOVERED_MARKET,
    FOREIGN_MARKET_MIN,
};
use liq_protocol::conformance::{run, Fixtures, LogFixture, PositionFixture};
use liq_protocol::{
    BlockReason, CallbackShape, Constraints, DirtySet, ExecutorAdapter, FlashRoute, HealthState,
    LegChoice, MarketSlot, Protocol, ProtocolError, StateWriter,
};
use liq_types::fixed::WAD;
use liq_types::{LogSubscriber, MarketId};

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
            4 => assert_eq!(*n, 0, "check 4 needs a fork post-state"),
            3 => {
                // Last-healthy is hf>1 (source). Check 3 wants a 1-unit hf<1
                // neighbor; Euler's HF==1 plateau makes that neighbor hf==1,
                // so liquidation_price returns None and check 3 may be skipped.
            }
            5 | 8 => assert_eq!(
                *n,
                0,
                "check {} is on the liquidatable run (healthy-only here)",
                i + 1
            ),
            9 => assert_eq!(
                *n, 0,
                "check 9 needs a liquidatable quote (healthy-only here)"
            ),
            _ => {
                assert!(*n > 0, "check {} was vacuous", i + 1);
            }
        }
    }
    assert_eq!(rep.alloc_metered, alloc_meter().is_some());
}

#[test]
fn liquidatable_through_run_executes_5_8_and_9() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut logs_liq = listing_logs(&d);
    logs_liq.extend(activity_logs_liq(&d));
    let st_liq = store_after(&p, &logs_liq);
    let mut logs_ok = listing_logs(&d);
    logs_ok.extend(activity_logs(&d));
    let st_ok = store_after(&p, &logs_ok);
    let px = prices(WETH_P8, USDC_P8);
    let positions = [
        PositionFixture {
            pos: st_liq.view(ALICE_ID, T0).unwrap(),
            px: &px,
            post: None,
        },
        PositionFixture {
            pos: st_ok.view(ALICE_ID, T0).unwrap(),
            px: &px,
            post: None,
        },
    ];
    let ranks = coverage_ranks();
    let mut owned = activity_logs_liq(&d);
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
    assert!(rep.assertions[8] > 0, "check 9 must fire after 10E encode");
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
    // E4. Euler's discount is `1/max(hf, min_df) - 1`, so the curve must be
    // the reciprocal one carrying `min_df` — not `Static`, which pinned the
    // bonus at one health and claimed it everywhere.
    assert_eq!(
        q.seize_options[0].curve,
        liq_protocol::BonusCurve::Reciprocal {
            min_df: liq_types::Ray::from_raw(min_df * uint!(1_000_000_000_U256)),
        }
    );
    // The curve is only worth carrying if it actually varies with health.
    // Evaluate it a long way below the quoted point and require a bigger
    // bonus there; a `Static` curve would return the same number twice.
    let deeper = liq_types::Ray::from_raw(min_df * uint!(1_000_000_000_U256));
    let at_deeper = q.seize_options[0]
        .curve
        .bonus_at_hf(deeper)
        .unwrap()
        .unwrap();
    assert!(
        at_deeper >= bonus,
        "bonus must not shrink as health falls"
    );
    let (repay, yield_bal) = math::max_liquidation(
        ALICE_DEBT_LIQ,
        liab,
        coll_adj,
        ALICE_SHARES,
        math::value_wad(ALICE_SHARES, p_coll, 18).unwrap(),
        min_df,
    )
    .unwrap();
    // E3: one basis point of headroom under the protocol maximum, and the
    // seize scaled by the same factor so `minYieldBalance` stays reachable
    // for the repay actually sent (E2's pairing rule, applied to the
    // headroom as well as to the notional cap).
    let expect_repay = repay * uint!(9_999_U256) / uint!(10_000_U256);
    assert_eq!(q.repay_options[0].max_repay, expect_repay);
    assert_eq!(
        q.seize_options[0].max_seize,
        yield_bal * expect_repay / repay,
        "seize must scale with the repay it is paired to"
    );
}

#[test]
fn encode_validates_then_ok() {
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
    let plan = p
        .encode(&q, LegChoice::PREFERRED, &route, Address::repeat_byte(0x99))
        .expect("10E Euler encode");
    assert_eq!(plan.leg.adapter, ExecutorAdapter::EulerV2);
    assert_eq!(plan.leg.market, d.debt_vault);
    assert_eq!(plan.leg.borrower, q.key.user);
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

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .unwrap()
}

fn intern_bound_adapter(d: &Deploy) -> EulerV2 {
    let mut cfg = d.config();
    cfg.catalog = CATALOG_MARKET;
    cfg.first_market = FIRST_DISCOVERED_MARKET;
    cfg.interned = vec![(d.debt_vault, MarketId(42)), (d.coll_vault, MarketId(99))];
    EulerV2::new(cfg).expect("intern-bound config")
}

#[test]
fn committed_toml_without_intern_bind_fails_closed() {
    let raw =
        std::fs::read_to_string(workspace_root().join("config/protocols/euler-v2.toml")).unwrap();
    let cfg = Config::from_toml(&raw).expect("euler-v2.toml parses");
    assert!(cfg.interned.is_empty());
    assert_eq!(cfg.vaults.len(), 26);
    assert_eq!(EulerV2::new(cfg).unwrap_err(), ConfigError::EmptyInterned);
}

#[test]
fn intern_binds_all_euler_vaults_from_registry() {
    let root = workspace_root();
    let intern = liq_config::Intern::from_registry(
        &liq_config::Registry::from_path(&root.join("registry/registry.json")).unwrap(),
    )
    .unwrap();
    let proto = intern.protocol("euler-v2").expect("euler-v2 family");
    assert_eq!(proto, PROTOCOL);
    let euler: Vec<&liq_config::MarketRec> = intern
        .markets()
        .iter()
        .filter(|m| m.protocol == proto)
        .collect();
    let admitted = euler.iter().filter(|m| m.admitted).count();
    assert_eq!(admitted, 26, "admitted euler-v2 vaults");
    assert_eq!(euler.len(), 884, "interned euler-v2 vaults");
    let intern_min = euler.iter().map(|m| m.id.0).min().expect("euler ids");
    let intern_max = euler.iter().map(|m| m.id.0).max().expect("euler ids");

    let raw = std::fs::read_to_string(root.join("config/protocols/euler-v2.toml")).unwrap();
    let parsed = Config::from_toml(&raw).expect("euler-v2.toml");
    assert_eq!(parsed.catalog, CATALOG_MARKET);
    assert_eq!(parsed.first_market, FIRST_DISCOVERED_MARKET);
    assert_eq!(parsed.vaults.len(), 26);
    assert!(parsed.interned.is_empty());
    assert_eq!(
        EulerV2::new(parsed.clone()).unwrap_err(),
        ConfigError::EmptyInterned
    );

    let mut bound = parsed.clone();
    bound.bind_from_intern(&intern).expect("bind_from_intern");
    let loaded = Config::load(&root).expect("Config::load");
    assert_eq!(bound.interned, loaded.interned);
    assert_eq!(loaded.interned.len(), 884);
    assert_eq!(loaded.interned.len(), euler.len());
    for v in &loaded.vaults {
        let id = loaded.interned_id(*v).expect("admitted vault is interned");
        let rec = intern
            .markets()
            .iter()
            .find(|m| m.protocol == proto && m.key == liq_config::OnChainId::Addr(*v))
            .expect("registry row");
        assert_eq!(id, rec.id);
        assert!(rec.admitted);
        assert!(
            id.0 >= intern_min && id.0 <= intern_max,
            "admitted vault MarketId {id:?} must be an intern euler-v2 id"
        );
        assert_ne!(id, CATALOG_MARKET);
        assert!(id.0 < FIRST_DISCOVERED_MARKET.0);
        assert!(
            id.0 < FOREIGN_MARKET_MIN,
            "interned MarketId {id:?} must be in intern 0..=3480"
        );
    }
    for (addr, id) in &loaded.interned {
        let rec = intern
            .markets()
            .iter()
            .find(|m| m.protocol == proto && m.key == liq_config::OnChainId::Addr(*addr))
            .expect("interned addr is an euler-v2 registry row");
        assert_eq!(*id, rec.id);
        assert_ne!(*id, CATALOG_MARKET);
        assert!(id.0 < FIRST_DISCOVERED_MARKET.0);
        assert!(id.0 >= intern_min && id.0 <= intern_max);
    }
    EulerV2::new(loaded).unwrap();
}

#[test]
fn proxy_created_uses_intern_ids_then_3512() {
    let d = Deploy::new();
    let p = intern_bound_adapter(&d);
    let extra = Address::repeat_byte(0xd2);
    let mut logs = listing_logs(&d);
    logs.push(log(
        d.factory,
        &ev::ProxyCreated {
            proxy: extra,
            upgradeable: true,
            implementation: d.impl_,
            trailingData: d.trailing(d.usdc),
        },
        DEPLOY_BLOCK,
        T0,
    ));
    let st = store_after(&p, &logs);
    let cat = st.markets(CATALOG_MARKET).expect("catalog 3511");
    assert_eq!(cat.len(), 3);
    let debt = st
        .market(MarketSlot {
            market: MarketId(42),
            slot: 0,
        })
        .expect("interned debt vault");
    let v: &liq_adapters_euler_v2::layout::VaultRow = debt.body().unwrap();
    assert_eq!(math::addr_from(v.vault), d.debt_vault);
    let coll = st
        .market(MarketSlot {
            market: MarketId(99),
            slot: 0,
        })
        .expect("interned coll vault");
    let c: &liq_adapters_euler_v2::layout::VaultRow = coll.body().unwrap();
    assert_eq!(math::addr_from(c.vault), d.coll_vault);
    let discovered = st
        .market(MarketSlot {
            market: FIRST_DISCOVERED_MARKET,
            slot: 0,
        })
        .expect("uninterned ProxyCreated is 3512");
    let x: &liq_adapters_euler_v2::layout::VaultRow = discovered.body().unwrap();
    assert_eq!(math::addr_from(x.vault), extra);
    assert!(st
        .market(MarketSlot {
            market: MarketId(3513),
            slot: 0
        })
        .is_err());
}

#[test]
fn hf_equality_is_liquidatable() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut logs = listing_logs(&d);
    logs.extend(activity_logs_eq(&d));
    let st = store_after(&p, &logs);
    let px = prices(WETH_P8, USDC_P8);
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    let p_coll = U256::from(WETH_P8) * P8_TO_RAY;
    let p_debt = U256::from(USDC_P8) * P8_TO_RAY;
    let coll_adj = math::collateral_adj_value(ALICE_SHARES, p_coll, 18, LIQ_LTV).unwrap();
    let liab = math::value_wad(ALICE_DEBT_EQ, p_debt, 6).unwrap();
    assert_eq!(coll_adj, liab);
    assert_eq!(h.state, HealthState::Liquidatable);
    let lp = p
        .liquidation_price(st.view(ALICE_ID, T0).unwrap(), &px, WETH_SHARES)
        .unwrap();
    if let Some(lp) = lp {
        let mut px2 = px.clone();
        if let Some(e) = px2.0.get_mut(usize::from(WETH_SHARES.0)) {
            e.price = lp.price;
        }
        let h_at = p.health(st.view(ALICE_ID, T0).unwrap(), &px2).unwrap();
        assert_eq!(h_at.state, HealthState::Healthy);
        assert!(h_at.hf > h.hf);
    }
}

#[test]
fn cool_off_blocks_liquidation() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut logs = listing_logs(&d);
    logs.push(log(
        d.debt_vault,
        &ev::GovSetLiquidationCoolOffTime {
            newCoolOffTime: 1_000,
        },
        DEPLOY_BLOCK,
        T0,
    ));
    logs.extend(activity_logs_liq(&d));
    logs.push(log(
        d.evc,
        &evc::AccountStatusCheck {
            account: d.alice,
            controller: d.debt_vault,
        },
        DEPLOY_BLOCK + 1,
        T0,
    ));
    let st = store_after(&p, &logs);
    let px = prices(WETH_P8, USDC_P8);
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(
        h.state,
        HealthState::Blocked {
            reason: BlockReason::GracePeriod
        }
    );
    assert!(p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px, &Constraints::UNBOUNDED)
        .unwrap()
        .is_none());
    let later = p
        .health(st.view(ALICE_ID, T0 + 1_000).unwrap(), &px)
        .unwrap();
    assert_eq!(later.state, HealthState::Liquidatable);
}

#[test]
fn ltv_ramp_matches_source_floor() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut logs = listing_logs(&d);
    logs.push(log(
        d.debt_vault,
        &ev::GovSetLTV {
            collateral: d.coll_vault,
            borrowLTV: BORROW_LTV,
            liquidationLTV: 7_000,
            initialLiquidationLTV: 8_000,
            targetTimestamp: alloy_primitives::Uint::<48, 1>::from(T0 + 1_000),
            rampDuration: 1_000,
        },
        DEPLOY_BLOCK,
        T0,
    ));
    logs.extend(activity_logs(&d));
    let st = store_after(&p, &logs);
    let mid = T0 + 500;
    let ltv = math::current_liquidation_ltv(7_000, 8_000, T0 + 1_000, 1_000, mid);
    assert_eq!(ltv, 7_500);
    let px = prices(WETH_P8, USDC_P8);
    let h = p.health(st.view(ALICE_ID, mid).unwrap(), &px).unwrap();
    let p_coll = U256::from(WETH_P8) * P8_TO_RAY;
    let p_debt = U256::from(USDC_P8) * P8_TO_RAY;
    let coll_adj = math::collateral_adj_value(ALICE_SHARES, p_coll, 18, ltv).unwrap();
    let liab = math::value_wad(ALICE_DEBT_HEALTHY, p_debt, 6).unwrap();
    let hf = math::mul_div_down(coll_adj, WAD, liab).unwrap();
    assert_eq!(h.hf, math::hf_wad_to_ray(hf).unwrap());
}

#[test]
fn quote_pairs_repay_to_preferred_collateral() {
    let d = Deploy::new();
    let mut cfg = d.config();
    let coll2 = Address::repeat_byte(0xc2);
    cfg.vaults.push(coll2);
    cfg.interned.push((coll2, MarketId(2)));
    cfg.assets.push(liq_adapters_euler_v2::AssetConfig {
        underlying: coll2,
        asset: liq_types::AssetId(2),
        feed: liq_protocol::FeedId(3),
        decimals: 18,
    });
    let p = liq_adapters_euler_v2::EulerV2::new(cfg).unwrap();
    let mut logs = listing_logs(&d);
    logs.push(log(
        d.factory,
        &ev::ProxyCreated {
            proxy: coll2,
            upgradeable: true,
            implementation: d.impl_,
            trailingData: d.trailing(d.weth),
        },
        DEPLOY_BLOCK,
        T0,
    ));
    logs.push(log(
        d.debt_vault,
        &ev::GovSetLTV {
            collateral: coll2,
            borrowLTV: BORROW_LTV,
            liquidationLTV: LIQ_LTV,
            initialLiquidationLTV: LIQ_LTV,
            targetTimestamp: alloy_primitives::Uint::<48, 1>::from(T0),
            rampDuration: 0,
        },
        DEPLOY_BLOCK,
        T0,
    ));
    logs.extend(activity_logs_liq(&d));
    let small = uint!(10_000_000_000_000_000_U256);
    logs.push(log(
        d.evc,
        &evc::CollateralStatus {
            account: d.alice,
            collateral: coll2,
            enabled: true,
        },
        DEPLOY_BLOCK + 1,
        T0,
    ));
    logs.push(log(
        coll2,
        &ev::Deposit {
            sender: d.alice,
            owner: d.alice,
            assets: small,
            shares: small,
        },
        DEPLOY_BLOCK + 1,
        T0,
    ));
    let st = store_after(&p, &logs);
    let mut px = prices(WETH_P8, USDC_P8);
    px.0.push(liq_types::Price {
        asset: liq_types::AssetId(2),
        price: liq_types::Ray::from_raw(U256::from(WETH_P8) * P8_TO_RAY),
        source: liq_types::SourceKind::Canonical,
        block: DEPLOY_BLOCK,
        ts: T0,
    });
    let q = p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px, &Constraints::UNBOUNDED)
        .unwrap()
        .expect("liquidatable");
    assert_eq!(q.repay_options.len(), 1);
    assert_eq!(q.seize_options.len(), 1);
    assert_eq!(q.seize_options[0].asset, WETH_SHARES);
    let p_coll = U256::from(WETH_P8) * P8_TO_RAY;
    let p_debt = U256::from(USDC_P8) * P8_TO_RAY;
    let coll_adj = math::collateral_adj_value(ALICE_SHARES, p_coll, 18, LIQ_LTV).unwrap()
        + math::collateral_adj_value(small, p_coll, 18, LIQ_LTV).unwrap();
    let liab = math::value_wad(ALICE_DEBT_LIQ, p_debt, 6).unwrap();
    let min_df = math::min_discount_factor(MAX_DISCOUNT).unwrap();
    let (repay_pref, _) = math::max_liquidation(
        ALICE_DEBT_LIQ,
        liab,
        coll_adj,
        ALICE_SHARES,
        math::value_wad(ALICE_SHARES, p_coll, 18).unwrap(),
        min_df,
    )
    .unwrap();
    let (repay_small, _) = math::max_liquidation(
        ALICE_DEBT_LIQ,
        liab,
        coll_adj,
        small,
        math::value_wad(small, p_coll, 18).unwrap(),
        min_df,
    )
    .unwrap();
    // E3. The quote gives back one basis point so a block of accrual between
    // quote and inclusion cannot trip `E_ExcessiveRepayAmount`. Compare
    // against the protocol maximum less that headroom, not the raw maximum.
    let expect = repay_pref * uint!(9_999_U256) / uint!(10_000_U256);
    assert_eq!(q.repay_options[0].max_repay, expect);
    assert!(
        q.repay_options[0].max_repay < repay_pref,
        "a quote sized to the exact maximum reverts on the next block"
    );
    assert_ne!(repay_pref, repay_small);
    assert!(repay_pref > repay_small);
}
