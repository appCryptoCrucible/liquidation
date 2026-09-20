//! GUIDE 01 harness over the Liquity V2 adapter.

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

use alloy_primitives::{address, uint, Address, U256};
use alloy_sol_types::{SolCall, SolEvent};
use common::*;
use liq_adapters_liquity_v2::events::{self as ev, halt, liq};
use liq_adapters_liquity_v2::{alloc_meter, math, Config, LiquityV2};
use liq_config::{Intern, OnChainId, Registry};
use liq_protocol::conformance::{run, Fixtures, LogFixture, PositionFixture};
use liq_protocol::{
    BonusCurve, CallbackShape, Constraints, DirtySet, FlashRoute, HealthState, LegChoice, Protocol,
    ProtocolError,
};
use liq_types::{AssetId, LogSubscriber, MarketId, ProtocolId, Ray, Wad};

fn full_store(d: &Deploy) -> (LiquityV2, liq_protocol::conformance::JournalStore) {
    let p = d.adapter();
    let mut logs = listing_logs(d);
    logs.extend(activity_logs(d));
    let st = store_after(&p, &logs);
    (p, st)
}

fn extra_logs(d: &Deploy) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK, T0);
    vec![
        log(
            TM,
            &ev::Liquidation {
                debtOffsetBySP: uint!(1_000_000_000_000_000_000_U256),
                debtRedistributed: U256::ZERO,
                boldGasCompensation: math::ETH_GAS_COMPENSATION,
                collGasCompensation: uint!(5_000_000_000_000_000_U256),
                collSentToSP: uint!(1_000_000_000_000_000_000_U256),
                collRedistributed: U256::ZERO,
                collSurplus: U256::ZERO,
                lColl: uint!(1_000_000_000_000_000_U256),
                lBoldDebt: uint!(2_000_000_000_000_000_U256),
                price: ETH_USD_WAD,
            },
            b,
            t,
        ),
        log(
            TM,
            &ev::Redemption {
                attemptedBoldAmount: WAD,
                actualBoldAmount: WAD,
                ethSent: WAD,
                ethFee: U256::ZERO,
                price: ETH_USD_WAD,
                redemptionPrice: ETH_USD_WAD,
            },
            b,
            t,
        ),
        log(
            TM,
            &ev::RedemptionFeePaidToTrove {
                troveId: d.trove_id,
                ethFee: U256::ZERO,
            },
            b,
            t,
        ),
        log(
            SP,
            &ev::StabilityPoolCollBalanceUpdated { newBalance: WAD },
            b,
            t,
        ),
        log(SP, &ev::P_Updated { P: WAD }, b, t),
        log(
            BO,
            &ev::ShutDown {
                tcr: uint!(2_000_000_000_000_000_000_U256),
            },
            b,
            t,
        ),
        log(
            PRICE_FEED,
            &ev::ShutDownFromOracleFailure {
                failedOracleAddr: Address::repeat_byte(0x11),
            },
            b,
            t,
        ),
        log(
            TM,
            &halt::Upgraded {
                implementation: Address::repeat_byte(0x77),
            },
            DEPLOY_BLOCK,
            T0,
        ),
        log(
            TM,
            &ev::PriceFeedAddressChanged {
                newPriceFeedAddress: PRICE_FEED,
            },
            DEPLOY_BLOCK,
            T0,
        ),
        log(
            TM,
            &ev::BatchUpdated {
                interestBatchManager: Address::repeat_byte(0x44),
                operation: 0,
                debt: U256::ZERO,
                coll: U256::ZERO,
                annualInterestRate: U256::ZERO,
                annualManagementFee: U256::ZERO,
                totalDebtShares: U256::ZERO,
                debtIncreaseFromUpfrontFee: U256::ZERO,
            },
            b,
            t,
        ),
    ]
}

#[test]
fn ten_checks_pass_healthy_only() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px_ok = prices(ETH_USD_WAD, BOLD_USD_WAD);
    let positions = [PositionFixture {
        pos: st.view(ALICE_ID, T0).unwrap(),
        px: &px_ok,
        post: None,
    }];
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
            4 | 5 | 8 | 9 => assert_eq!(
                *n,
                0,
                "check {} must be vacuous: gas-comp quote is not a flash-repay-seize leg",
                i + 1
            ),
            _ => assert!(*n > 0, "check {} was vacuous", i + 1),
        }
    }
    assert_eq!(rep.alloc_metered, alloc_meter().is_some());
}

#[test]
fn health_liquidatable_iff_icr_lt_mcr() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let pos = st.view(ALICE_ID, T0).unwrap();
    let h_ok = p.health(pos, &prices(ETH_USD_WAD, BOLD_USD_WAD)).unwrap();
    assert_eq!(h_ok.state, HealthState::Healthy);
    assert!(h_ok.hf >= Ray::ONE);

    let h_liq = p
        .health(pos, &prices(ETH_USD_LIQ_WAD, BOLD_USD_WAD))
        .unwrap();
    assert_eq!(h_liq.state, HealthState::Liquidatable);
    assert!(h_liq.hf < Ray::ONE);

    let icr = math::compute_cr(ALICE_COLL, ALICE_DEBT, ETH_USD_LIQ_WAD).unwrap();
    assert!(icr < U256::from(MCR_WETH));
    let _ = Wad::ZERO;
}

#[test]
fn quote_is_gas_comp_only() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let pos = st.view(ALICE_ID, T0).unwrap();
    assert!(p
        .quote(
            pos,
            &prices(ETH_USD_WAD, BOLD_USD_WAD),
            &Constraints::UNBOUNDED
        )
        .unwrap()
        .is_none());

    let q = p
        .quote(
            pos,
            &prices(ETH_USD_LIQ_WAD, BOLD_USD_WAD),
            &Constraints::UNBOUNDED,
        )
        .unwrap()
        .expect("liquidatable");
    assert_eq!(q.repay_options.len(), 1);
    assert_eq!(q.repay_options[0].asset, BOLD);
    assert_eq!(q.repay_options[0].max_repay, U256::ZERO);
    assert_eq!(q.seize_options.len(), 1);
    assert_eq!(q.seize_options[0].asset, WETH);
    assert_eq!(q.seize_options[0].bonus, Ray::ZERO);
    assert!(matches!(
        q.seize_options[0].curve,
        BonusCurve::Static { bonus } if bonus == Ray::ZERO
    ));
    let coll_gas = math::coll_gas_from_offset(ALICE_COLL, ALICE_DEBT, SP_BOLD).unwrap();
    let want = math::ETH_GAS_COMPENSATION.checked_add(coll_gas).unwrap();
    assert_eq!(q.seize_options[0].max_seize, want);
    assert_eq!(coll_gas, ALICE_COLL / uint!(200_U256));

    let cap = Constraints {
        per_liquidation_notional_cap: Wad::from_raw(U256::ONE),
    };
    assert!(p
        .quote(pos, &prices(ETH_USD_LIQ_WAD, BOLD_USD_WAD), &cap)
        .unwrap()
        .is_none());
}

#[test]
fn encode_validates_then_executor_unwired() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let q = p
        .quote(
            st.view(ALICE_ID, T0).unwrap(),
            &prices(ETH_USD_LIQ_WAD, BOLD_USD_WAD),
            &Constraints::UNBOUNDED,
        )
        .unwrap()
        .expect("liquidatable");
    let route = FlashRoute {
        provider: CallbackShape::AaveExecuteOperation.provider(),
        source: Address::repeat_byte(0x50),
        asset: BOLD,
        amount: U256::ZERO,
        fee_bps: 0,
        callback: CallbackShape::AaveExecuteOperation,
    };
    let rec = Address::repeat_byte(0x99);
    assert_eq!(
        p.encode(&q, LegChoice::PREFERRED, &route, rec),
        Err(ProtocolError::ExecutorUnwired)
    );
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
        p.encode(&q, LegChoice::PREFERRED, &route, Address::ZERO),
        Err(ProtocolError::ZeroRecipient)
    );
    let mut bad = route;
    bad.asset = WETH;
    assert_eq!(
        p.encode(&q, LegChoice::PREFERRED, &bad, rec),
        Err(ProtocolError::FundingAssetMismatch)
    );
    let mut q3 = q.clone();
    q3.repay_options[0].max_repay = U256::from(1u8);
    assert_eq!(
        p.encode(&q3, LegChoice::PREFERRED, &route, rec),
        Err(ProtocolError::FundingShort)
    );
    let mut mismatch = route;
    mismatch.callback = CallbackShape::MorphoFlashCallback;
    assert_eq!(
        p.encode(&q, LegChoice::PREFERRED, &mismatch, rec),
        Err(ProtocolError::CallbackProviderMismatch)
    );
    assert_eq!(
        liq::batchLiquidateTrovesCall::SELECTOR,
        [0xef, 0x49, 0xa6, 0xb4]
    );
}

#[test]
fn halt_logs_fold_before_the_pin_and_error_after() {
    let d = Deploy::new();
    let (p, mut st) = full_store(&d);
    let impl_ = Address::repeat_byte(0x77);
    let before = log(
        TM,
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
        TM,
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
    let bind = log(
        TM,
        &ev::PriceFeedAddressChanged {
            newPriceFeedAddress: Address::repeat_byte(0x42),
        },
        DEPLOY_BLOCK + 1,
        T0,
    );
    assert_eq!(
        p.apply_log(&mut st, &bind.view()),
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
    let (p, st) = full_store(&d);
    assert_eq!(
        p.health_probe(st.view(ALICE_ID, T0).unwrap()).err(),
        Some(ProtocolError::ProbeUnavailable)
    );
}

#[test]
fn missing_bold_price_fails_closed() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px = prices(ETH_USD_WAD, U256::ZERO);
    assert_eq!(
        p.health(st.view(ALICE_ID, T0).unwrap(), &px),
        Err(ProtocolError::MissingPrice(BOLD))
    );
}

#[test]
fn coll_gas_uses_sp_portion_not_entire_coll() {
    let entire_coll = uint!(10_000_000_000_000_000_000_U256);
    let entire_debt = uint!(5_000_000_000_000_000_000_000_U256);
    assert_eq!(
        math::coll_gas_from_offset(entire_coll, entire_debt, U256::ZERO).unwrap(),
        U256::ZERO
    );
    let half_sp = entire_debt / uint!(2_U256) + math::MIN_BOLD_IN_SP;
    let got = math::coll_gas_from_offset(entire_coll, entire_debt, half_sp).unwrap();
    let want = (entire_coll / uint!(2_U256)) / uint!(200_U256);
    assert_eq!(got, want);
    let capped = math::coll_gas_compensation(uint!(1_000_000_000_000_000_000_000_U256)).unwrap();
    assert_eq!(capped, math::COLL_GAS_COMPENSATION_CAP);
}

#[test]
fn toml_matches_pin_1_json_and_intern() {
    let raw = include_str!("../../../../config/protocols/liquity-v2.toml");
    let cfg = Config::from_toml(raw).expect("liquity-v2.toml");
    assert_eq!(cfg.protocol, ProtocolId(9));
    assert_eq!(cfg.pinned_through, 22_516_117);
    assert_eq!(cfg.bold.underlying, BOLD_TOKEN);
    assert_eq!(cfg.bold.asset, AssetId(418));
    assert_eq!(cfg.weth.underlying, WETH_TOKEN);
    assert_eq!(cfg.weth.asset, AssetId(813));
    assert_eq!(cfg.branches.len(), 3);
    assert_eq!(cfg.branches[0].trove_manager, TM);
    assert_eq!(
        cfg.branches[1].trove_manager,
        address!("0xa2895d6a3bf110561dfe4b71ca539d84e1928b22")
    );
    assert_eq!(
        cfg.branches[2].trove_manager,
        address!("0xb2b2abeb5c357a234363ff5d180912d319e3e19e")
    );
    assert_eq!(cfg.branches[0].mcr, MCR_WETH);
    assert_eq!(cfg.branches[1].mcr, 1_200_000_000_000_000_000);
    assert_eq!(cfg.branches[0].market, MarketId(3481));
    assert_eq!(cfg.branches[1].market, MarketId(3482));
    assert_eq!(cfg.branches[2].market, MarketId(3483));
    LiquityV2::new(cfg.clone()).unwrap();

    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .unwrap();
    let intern =
        Intern::from_registry(&Registry::from_path(&root.join("registry/registry.json")).unwrap())
            .unwrap();
    assert_eq!(intern.asset(BOLD_TOKEN), Some(AssetId(418)));
    assert_eq!(intern.asset(WETH_TOKEN), Some(AssetId(813)));
    assert_eq!(
        intern.asset(address!("0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0")),
        Some(AssetId(525))
    );
    assert_eq!(
        intern.asset(address!("0xae78736cd615f374d3085123a210448e74fc6393")),
        Some(AssetId(734))
    );
    assert_eq!(intern.protocol("liquity-v2"), None);
    assert_eq!(intern.protocol("spark"), Some(ProtocolId(8)));
    assert_eq!(intern.markets().len(), 3481);
    let spark = intern
        .markets()
        .iter()
        .find(|m| m.key == OnChainId::Addr(address!("0xc13e21b648a5ee794902342038ff3adab66be987")));
    assert!(spark.is_some());
    assert_eq!(intern.feeds().len(), 15);
}

use common::OwnedLog;
