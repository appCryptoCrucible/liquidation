//! GUIDE 01 harness over the Fluid vault adapter.

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
use alloy_sol_types::{SolCall, SolEvent};
use common::*;
use liq_adapters_fluid::config::FactoryRpc;
use liq_adapters_fluid::events::{admin, factory, halt, vault};
use liq_adapters_fluid::layout::VaultExtra;
use liq_adapters_fluid::{
    alloc_meter, liquidate_selector, math, Config, ConfigError, Fluid, VAULT_T1, VAULT_T2,
    VAULT_T3, VAULT_T4,
};
use liq_protocol::conformance::{run, Fixtures, LogFixture, PositionFixture};
use liq_protocol::{
    CallbackShape, Constraints, FlashRoute, HealthState, LegChoice, Protocol, ProtocolError,
};
use liq_types::{LogSubscriber, Ray};

fn full_store(d: &Deploy, debt: U256) -> (Fluid, liq_protocol::conformance::JournalStore) {
    let p = d.adapter();
    let mut logs = listing_logs(d);
    logs.extend(activity_logs(d, d.vault_t1, ALICE_COLL, debt));
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
            d.factory,
            &factory::Transfer {
                from: Address::ZERO,
                to: Address::repeat_byte(0x11),
                id: U256::from(1u8),
            },
            b,
            t,
        ),
        log(
            d.factory,
            &factory::LogSetDeployer {
                deployer: Address::repeat_byte(0x02),
                allowed: true,
            },
            b,
            t,
        ),
        log(
            d.vault_t1,
            &vault::LogRebalance {
                colAmt_: alloy_primitives::I256::ZERO,
                debtAmt_: alloy_primitives::I256::ZERO,
            },
            b,
            t,
        ),
        log(
            d.vault_t1,
            &vault::LogAbsorb {
                colAbsorbedRaw_: U256::ZERO,
                debtAbsorbedRaw_: U256::ZERO,
            },
            b,
            t,
        ),
        log(
            d.vault_t1,
            &admin::LogUpdateSupplyRateMagnifier {
                supplyRateMagnifier_: U256::from(100u16),
            },
            b,
            t,
        ),
        log(
            d.vault_t1,
            &halt::Upgraded {
                implementation: Address::repeat_byte(0x77),
            },
            DEPLOY_BLOCK,
            T0,
        ),
        log(
            d.vault_t1,
            &halt::AdminChanged {
                previousAdmin: Address::ZERO,
                newAdmin: Address::repeat_byte(0x02),
            },
            DEPLOY_BLOCK,
            T0,
        ),
        log(
            d.vault_t1,
            &halt::Initialized { version: 1 },
            DEPLOY_BLOCK,
            T0,
        ),
    ]
}

#[test]
fn ten_checks_pass_with_nonvacuous_assertions() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_DEBT_OK);
    let px = prices(ETH_USD, RAY_ONE);
    let positions = [PositionFixture {
        pos: st.view(T1_ID, T0).unwrap(),
        px: &px,
        post: None,
    }];
    let ranks = coverage_ranks();
    let mut owned = activity_logs(&d, d.vault_t1, ALICE_COLL, ALICE_DEBT_OK);
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
                    "healthy-only report: check {} has no quote (4/8 need post or liq; 9 inapplicable)",
                    i + 1
                );
            }
            _ => assert!(*n > 0, "check {} was vacuous", i + 1),
        }
    }
    assert_eq!(rep.alloc_metered, alloc_meter().is_some());

    let (p_u, st_u) = full_store(&d, ALICE_DEBT_LIQ);
    let px_u = prices(ETH_USD, RAY_ONE);
    let h_u = p_u.health(st_u.view(T1_ID, T0).unwrap(), &px_u).unwrap();
    assert_eq!(h_u.state, HealthState::Liquidatable, "check 10 class");
    let q_u = p_u
        .quote(
            st_u.view(T1_ID, T0).unwrap(),
            &px_u,
            &Constraints::UNBOUNDED,
        )
        .unwrap()
        .expect("check 10: Liquidatable quotes");
    let from_curve = q_u.seize_options[0].curve.bonus_at_hf(h_u.hf).unwrap();
    assert_eq!(from_curve, Some(q_u.seize_options[0].bonus), "check 5");
    let positions_u = [PositionFixture {
        pos: st_u.view(T1_ID, T0).unwrap(),
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
    let err = run(
        &p_u,
        &mut log_store_u,
        &fx_u,
        alloc_meter().map(|m| m as &dyn Fn() -> u64),
    )
    .expect_err("check 9 cannot Ok: encode is ExecutorUnwired until 10R");
    assert_eq!(err.check, 9);
    assert_eq!(err.detail, "ExecutorUnwired");
}

#[test]
fn raw_invariant_survives_exchange_price_update() {
    // Pin: operate converts token→raw at then-current ex and writes raw into
    // vaultVariables bits 82/146. updateExchangePrices changes only
    // supplyExPrice/borrowExPrice. Tick is a raw ratio and must not move.
    let d = Deploy::new();
    let (p, mut st) = full_store(&d, ALICE_DEBT_OK);
    let px = prices(ETH_USD, RAY_ONE);
    let pos0 = st.view(T1_ID, T0).unwrap();
    let extra0: &VaultExtra = pos0.extra.view().unwrap();
    let top0 = extra0.top_tick;
    assert_eq!(
        U256::from(pos0.supply[0]),
        ALICE_COLL,
        "operate at 1e12 stores raw == token"
    );
    assert_eq!(U256::from(pos0.debt[1]), ALICE_DEBT_OK);

    let supply_ex = uint!(1_100_000_000_000_U256);
    let borrow_ex = uint!(1_200_000_000_000_U256);
    assert_ne!(supply_ex, EX_PRICE);
    assert_ne!(borrow_ex, EX_PRICE);
    let l = log(
        d.vault_t1,
        &vault::LogUpdateExchangePrice {
            supplyExPrice_: supply_ex,
            borrowExPrice_: borrow_ex,
        },
        DEPLOY_BLOCK + 2,
        T0,
    );
    p.apply_log(&mut st, &l.view()).expect("second ex update");

    let pos1 = st.view(T1_ID, T0).unwrap();
    let extra1: &VaultExtra = pos1.extra.view().unwrap();
    assert_eq!(
        U256::from(pos1.supply[0]),
        ALICE_COLL,
        "ex update must not rewrite stored raw"
    );
    assert_eq!(U256::from(pos1.debt[1]), ALICE_DEBT_OK);
    let pin_top = math::tick_from_raw(ALICE_COLL, ALICE_DEBT_OK).unwrap();
    assert_eq!(extra1.top_tick, top0);
    assert_eq!(extra1.top_tick, pin_top);

    let inverted_col = math::to_raw(ALICE_COLL, supply_ex).unwrap();
    let inverted_debt = math::to_raw(ALICE_DEBT_OK, borrow_ex).unwrap();
    let inverted_tick = math::tick_from_raw(inverted_col, inverted_debt).unwrap();
    assert_ne!(
        inverted_tick, pin_top,
        "fixture is not the identity path (ex == 1e12)"
    );

    let oracle = math::oracle_debt_per_col_1e27(ETH_USD, RAY_ONE, 18, 6).unwrap();
    let raw_dpc = math::raw_debt_per_col(oracle, supply_ex, borrow_ex).unwrap();
    let liq = math::liquidation_tick(raw_dpc, THRESHOLD).unwrap();
    let pin_hf = math::hf_from_ticks(pin_top, liq).unwrap();
    let h = p.health(pos1, &px).unwrap();
    assert_eq!(h.hf, pin_hf);
}

#[test]
fn t1_healthy_and_liquidatable_by_tick() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_DEBT_OK);
    let px = prices(ETH_USD, RAY_ONE);
    let h = p.health(st.view(T1_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(h.state, HealthState::Healthy);
    assert!(h.hf >= Ray::ONE);
    let (pl, stl) = full_store(&d, ALICE_DEBT_LIQ);
    let hl = pl.health(stl.view(T1_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(hl.state, HealthState::Liquidatable);
    assert!(hl.hf < Ray::ONE);
}

#[test]
fn t3_is_not_t1_cloned_and_selector_collides() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut st = store_after(&p, &listing_logs(&d));
    let px = prices(ETH_USD, RAY_ONE);
    let pos = st.view(T3_ID, T0).unwrap();
    assert_eq!(pos.key.market, MARKET_T3);
    assert_eq!(pos.key.user, d.vault_t3);
    // Pin T3 `_operate` takes DEX share debt; FluidOracle 1e27 is
    // share-per-col. PriceVector cannot express that — fail closed, never
    // T1 USDC token-pair Liquidatable.
    assert_eq!(
        p.health(pos, &px).unwrap_err(),
        ProtocolError::OracleSourceMismatch
    );
    assert_eq!(
        p.quote(pos, &px, &Constraints::UNBOUNDED).unwrap_err(),
        ProtocolError::OracleSourceMismatch
    );

    let activity = activity_logs(&d, d.vault_t3, ALICE_COLL, ALICE_DEBT_LIQ);
    p.apply_log(&mut st, &activity[0].view())
        .expect("T3 NewPositionMinted folds");
    assert_eq!(
        p.apply_log(&mut st, &activity[1].view()).unwrap_err(),
        ProtocolError::OracleSourceMismatch
    );

    let t1 = liquidate_selector(VAULT_T1).unwrap();
    let t2 = liquidate_selector(VAULT_T2).unwrap();
    let t3 = liquidate_selector(VAULT_T3).unwrap();
    let t4 = liquidate_selector(VAULT_T4).unwrap();
    // Pin T2 and T3 `liquidate` share ABI types
    // `(uint256,uint256,uint256,uint256,address,bool)` so the selector collides.
    // Dispatch is vault `TYPE` (20000 vs 30000), not selector. T1 and T4 differ.
    assert_ne!(t1, t2);
    assert_eq!(t2, t3);
    assert_ne!(t1, t4);
    assert_ne!(t2, t4);
    use liq_adapters_fluid::events::{t1, t2 as t2abi, t3 as t3abi, t4 as t4abi};
    assert_eq!(t1, t1::liquidateCall::SELECTOR);
    assert_eq!(t2, t2abi::liquidateCall::SELECTOR);
    assert_eq!(t3, t3abi::liquidateCall::SELECTOR);
    assert_eq!(t4, t4abi::liquidateCall::SELECTOR);
}

#[test]
fn quote_static_bonus_and_encode_unwired() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_DEBT_LIQ);
    let px = prices(ETH_USD, RAY_ONE);
    let q = p
        .quote(st.view(T1_ID, T0).unwrap(), &px, &Constraints::UNBOUNDED)
        .unwrap()
        .expect("liquidatable");
    assert_eq!(q.repay_options[0].asset, DEBT);
    assert_eq!(q.seize_options[0].asset, COLL);
    assert!(matches!(
        q.seize_options[0].curve,
        liq_protocol::BonusCurve::Static { .. }
    ));
    assert_eq!(q.seize_options[0].bonus, math::bonus_ray(PENALTY).unwrap());
    let rec = Address::repeat_byte(0x99);
    let route = FlashRoute {
        provider: CallbackShape::ALL[0].provider(),
        source: Address::repeat_byte(0x50),
        asset: DEBT,
        amount: q.repay_options[0].max_repay,
        fee_bps: 0,
        callback: CallbackShape::ALL[0],
    };
    assert_eq!(
        p.encode(&q, LegChoice::PREFERRED, &route, rec),
        Err(ProtocolError::ExecutorUnwired)
    );
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
fn market_ids_stay_in_fluid_band() {
    assert_eq!(math::market_from_vault_id(1).unwrap(), MARKET_T1);
    assert_eq!(math::market_from_vault_id(2).unwrap(), MARKET_T3);
    assert!(math::market_from_vault_id(0).is_err());
    assert!(math::market_from_vault_id(200).is_err());
}

#[test]
fn new_refuses_unasserted_config() {
    let raw = include_str!("../../../../config/protocols/fluid.toml");
    let cfg = Config::from_toml(raw).expect("fluid.toml");
    assert!(!cfg.live_factory_asserted);
    assert_eq!(
        Fluid::new(cfg).unwrap_err(),
        ConfigError::LiveFactoryUnasserted
    );
    let d = Deploy::new();
    assert!(!d.config().live_factory_asserted);
    assert_eq!(
        Fluid::new(d.config()).unwrap_err(),
        ConfigError::LiveFactoryUnasserted
    );
}

#[test]
fn assert_live_factory_matches_d15_count() {
    let d = Deploy::new();
    let mut cfg = d.config();
    cfg.assert_live_factory(&d.rpc(), DEPLOY_BLOCK)
        .expect("pin view");
    assert!(cfg.live_factory_asserted);
    Fluid::new(cfg).expect("asserted config boots");
}

#[test]
fn assert_live_factory_refuses_count_mismatch() {
    let d = Deploy::new();
    let mut cfg = d.config();
    cfg.d15_vaults = 182;
    let err = cfg
        .assert_live_factory(&d.rpc(), DEPLOY_BLOCK)
        .expect_err("count mismatch");
    match err {
        ConfigError::VaultCountMismatch { expected, found } => {
            assert_eq!(expected, 182);
            assert_eq!(found, 2);
        }
        other => panic!("expected VaultCountMismatch, got {other:?}"),
    }
    assert!(!cfg.live_factory_asserted);
}

#[test]
fn native_vault_is_unpriced() {
    let d = Deploy::new();
    let mut cfg = d.config();
    cfg.vault_pins[0].supply0 = math::NATIVE_TOKEN;
    cfg.assert_live_factory(&d.rpc(), DEPLOY_BLOCK).unwrap();
    let p = Fluid::new(cfg).unwrap();
    let logs = listing_logs(&d);
    let st = store_after(&p, &logs);
    let pos = st.view(T1_ID, T0);
    // listing interned the vault; health without operate still needs VIEWED+PRICED.
    // Native coll is not interned → UNPRICED.
    if let Ok(pos) = pos {
        let err = p.health(pos, &prices(ETH_USD, RAY_ONE)).unwrap_err();
        assert_eq!(err, ProtocolError::OracleSourceMismatch);
    }
}

#[test]
fn halt_after_pin_is_halt_signal() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut st = store_after(&p, &listing_logs(&d));
    let l = log(
        d.vault_t1,
        &halt::Upgraded {
            implementation: Address::repeat_byte(0x88),
        },
        DEPLOY_BLOCK + 10,
        T0,
    );
    assert_eq!(
        p.apply_log(&mut st, &l.view()).unwrap_err(),
        ProtocolError::HaltSignal
    );
}

#[test]
fn health_probe_unavailable() {
    let d = Deploy::new();
    let (p, st) = full_store(&d, ALICE_DEBT_OK);
    assert_eq!(
        p.health_probe(st.view(T1_ID, T0).unwrap()).unwrap_err(),
        ProtocolError::ProbeUnavailable
    );
}

#[test]
fn subscriptions_cover_factory_and_pinned_vaults() {
    let d = Deploy::new();
    let p = d.adapter();
    let subs = p.subscriptions();
    assert!(subs
        .iter()
        .any(|f| f.address == d.factory && f.topic0 == factory::VaultDeployed::SIGNATURE_HASH));
    assert!(subs
        .iter()
        .any(|f| f.address == d.vault_t1 && f.topic0 == vault::LogOperate::SIGNATURE_HASH));
}

#[test]
fn coverage_topics_match_sol() {
    use alloy_sol_types::SolEvent;
    let ranks = coverage_ranks();
    let want = [
        factory::VaultDeployed::SIGNATURE_HASH,
        factory::NewPositionMinted::SIGNATURE_HASH,
        factory::Transfer::SIGNATURE_HASH,
        vault::LogOperate::SIGNATURE_HASH,
        vault::LogUpdateExchangePrice::SIGNATURE_HASH,
        vault::LogLiquidate::SIGNATURE_HASH,
        vault::LogAbsorb::SIGNATURE_HASH,
    ];
    for t in want {
        assert!(ranks.iter().any(|(h, _)| *h == t), "coverage missing {t}");
    }
}

#[test]
#[ignore = "live VaultFactory totalVaults; set LIQ_RPC_URL"]
fn live_factory_total_vaults_matches_toml() {
    let url = std::env::var("LIQ_RPC_URL").expect("LIQ_RPC_URL required for live factory assert");
    let raw = include_str!("../../../../config/protocols/fluid.toml");
    let mut cfg = Config::from_toml(raw).expect("fluid.toml");
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
    impl FactoryRpc for Live {
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
                .map_err(|_| ConfigError::FactoryCall(to))?;
            if out.is_empty() {
                return Err(ConfigError::FactoryCall(to));
            }
            Ok(out)
        }
    }
    let rpc = Live { provider, rt };
    cfg.assert_live_factory(&rpc, cfg.pinned_through)
        .expect("live totalVaults must equal d15_vaults");
    Fluid::new(cfg).expect("live-asserted config boots");
}
