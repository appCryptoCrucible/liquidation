//! GUIDE 01 harness over the Morpho Blue adapter.

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
use liq_adapters_morpho_blue::events::{self as ev, halt};
use liq_adapters_morpho_blue::{alloc_meter, math};
use liq_protocol::conformance::{run, Fixtures, LogFixture, PositionFixture};
use liq_protocol::{
    CallbackShape, DirtySet, ExecutorAdapter, FlashRoute, HealthState, LegChoice, Protocol,
    ProtocolError,
};
use liq_types::{LogSubscriber, Ray, Wad};

const WAD: U256 = uint!(1_000_000_000_000_000_000_U256);

fn full_store(
    d: &Deploy,
) -> (
    liq_adapters_morpho_blue::MorphoBlue,
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
            d.morpho,
            &ev::AccrueInterest {
                id: MARKET_ID,
                prevBorrowRate: uint!(1_000_000_000_U256),
                interest: uint!(1_000_000_000_000_000_000_U256),
                feeShares: U256::ZERO,
            },
            b,
            t,
        ),
        log(
            d.morpho,
            &ev::SetFee {
                id: MARKET_ID,
                newFee: uint!(50_000_000_000_000_000_U256),
            },
            b,
            t,
        ),
        log(
            d.morpho,
            &ev::Repay {
                id: MARKET_ID,
                caller: d.alice,
                onBehalf: d.alice,
                assets: uint!(100_000_000_000_000_000_000_U256),
                shares: uint!(100_000_000_000_000_000_000_U256) * uint!(1_000_000_U256),
            },
            b,
            t,
        ),
        log(
            d.morpho,
            &ev::Liquidate {
                id: MARKET_ID,
                caller: Address::repeat_byte(0xee),
                borrower: d.alice,
                repaidAssets: uint!(10_000_000_000_000_000_000_U256),
                repaidShares: uint!(10_000_000_000_000_000_000_U256) * uint!(1_000_000_U256),
                seizedAssets: uint!(10_000_000_000_000_000_U256),
                badDebtAssets: U256::ZERO,
                badDebtShares: U256::ZERO,
            },
            b,
            t,
        ),
        log(
            d.morpho,
            &ev::Withdraw {
                id: MARKET_ID,
                caller: d.bob,
                onBehalf: d.bob,
                receiver: d.bob,
                assets: uint!(1_000_000_000_000_000_000_000_U256),
                shares: uint!(1_000_000_000_000_000_000_000_U256) * uint!(1_000_000_U256),
            },
            b,
            t,
        ),
        log(
            d.morpho,
            &ev::WithdrawCollateral {
                id: MARKET_ID,
                caller: d.alice,
                onBehalf: d.alice,
                receiver: d.alice,
                assets: uint!(1_000_000_000_000_000_U256),
            },
            b,
            t,
        ),
        log(
            d.morpho,
            &halt::Upgraded {
                implementation: Address::repeat_byte(0x77),
            },
            DEPLOY_BLOCK,
            T0,
        ),
        log(
            d.morpho,
            &ev::FlashLoan {
                caller: d.alice,
                token: d.dai,
                assets: U256::ONE,
            },
            b,
            t,
        ),
        log(
            d.morpho,
            &ev::SetOwner {
                newOwner: Address::repeat_byte(0x01),
            },
            b,
            t,
        ),
        log(
            d.morpho,
            &ev::SetFeeRecipient {
                newFeeRecipient: Address::repeat_byte(0x02),
            },
            b,
            t,
        ),
        log(d.morpho, &ev::EnableIrm { irm: d.irm }, b, t),
        log(d.morpho, &ev::EnableLltv { lltv: LLTV }, b, t),
        log(
            d.morpho,
            &ev::SetAuthorization {
                caller: d.alice,
                authorizer: d.alice,
                authorized: d.bob,
                newIsAuthorized: true,
            },
            b,
            t,
        ),
        log(
            d.morpho,
            &ev::IncrementNonce {
                caller: d.alice,
                authorizer: d.alice,
                usedNonce: U256::ZERO,
            },
            b,
            t,
        ),
    ]
}

#[test]
fn ten_checks_pass_with_nonvacuous_assertions() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px_liq = prices(1000_0000_0000, DAI_P8);
    let px_ok = prices(WETH_P8, DAI_P8);
    let positions = [
        PositionFixture {
            pos: st.view(ALICE_ID, T0).unwrap(),
            px: &px_liq,
            post: None,
        },
        PositionFixture {
            pos: st.view(ALICE_ID, T0).unwrap(),
            px: &px_ok,
            post: None,
        },
        PositionFixture {
            pos: st.view(BOB_ID, T0).unwrap(),
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
            assert_eq!(*n, 0, "check 4 needs a fork post-state");
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
    let px = prices(1000_0000_0000, DAI_P8);
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    let p_coll = U256::from(1000_0000_0000u64) * P8_TO_RAY;
    let p_loan = U256::from(DAI_P8) * P8_TO_RAY;
    let oracle = p_coll * math::ORACLE_PRICE_SCALE / p_loan;
    let quoted = ALICE_WETH * oracle / math::ORACLE_PRICE_SCALE;
    let max_borrow = quoted * LLTV / WAD;
    let borrowed = ALICE_DAI_DEBT;
    let hf = max_borrow * WAD / borrowed;
    assert_eq!(h.hf, Ray::from_raw(hf * uint!(1_000_000_000_U256)));
    assert_eq!(h.state, HealthState::Liquidatable);
    let _ = Wad::ZERO;
}

#[test]
fn quote_static_bonus_and_encode() {
    let d = Deploy::new();
    let (p, st) = full_store(&d);
    let px = prices(1000_0000_0000, DAI_P8);
    let q = p
        .quote(st.view(ALICE_ID, T0).unwrap(), &px)
        .unwrap()
        .expect("liquidatable");
    assert_eq!(q.repay_options[0].asset, DAI);
    assert_eq!(q.seize_options[0].asset, WETH);
    assert!(matches!(
        q.seize_options[0].curve,
        liq_protocol::BonusCurve::Static { .. }
    ));
    let route = FlashRoute {
        provider: CallbackShape::MorphoFlashCallback.provider(),
        source: Address::repeat_byte(0x50),
        asset: DAI,
        amount: q.repay_options[0].max_repay,
        fee_bps: 0,
        callback: CallbackShape::MorphoFlashCallback,
    };
    let plan = p
        .encode(&q, LegChoice::PREFERRED, &route, Address::repeat_byte(0x99))
        .unwrap();
    assert_eq!(plan.leg.adapter, ExecutorAdapter::MorphoBlue);
    assert_eq!(plan.leg.market, d.morpho);
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
    let mut cfg = d.config();
    cfg.price_sources.clear();
    let p = liq_adapters_morpho_blue::MorphoBlue::new(cfg).unwrap();
    let mut logs = listing_logs(&d);
    logs.extend(activity_logs(&d));
    let st = store_after(&p, &logs);
    let px = prices(1000_0000_0000, DAI_P8);
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
        d.morpho,
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
        d.morpho,
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

use common::OwnedLog;

/// Loan and collateral decimals differ. The threshold is the last healthy
/// price under `health()`, which applies `10^|loanDec-collDec|`. The
/// equal-decimal inversion of the same raw position is off by that factor
/// and is not a crossing.
#[test]
fn liquidation_price_inverts_decimal_scale() {
    let borrow = U256::from(1_400_000_000u64);
    let supply = U256::from(10_000_000_000_000u64);
    let coll = uint!(1_000_000_000_000_000_000_U256);
    let px = prices(WETH_P8, DAI_P8);
    let lift = U256::from(10u64).pow(U256::from(12u8));

    let (p18, st18) = open_market(18, 18, borrow, supply, coll);
    let (p_loan6, st_loan6) = open_market(6, 18, borrow, supply, coll);
    let (p_coll6, st_coll6) = open_market(18, 6, borrow, supply, coll);

    assert!(
        p_loan6
            .health(st_loan6.view(ALICE_ID, T0).unwrap(), &px)
            .unwrap()
            .hf
            >= Ray::ONE
    );

    let weth18 = price_of(&p18, &st18, &px, WETH);
    let dai18 = price_of(&p18, &st18, &px, DAI);
    let weth_loan6 = price_of(&p_loan6, &st_loan6, &px, WETH);
    let dai_loan6 = price_of(&p_loan6, &st_loan6, &px, DAI);
    let weth_coll6 = price_of(&p_coll6, &st_coll6, &px, WETH);
    let dai_coll6 = price_of(&p_coll6, &st_coll6, &px, DAI);

    assert_last_healthy(&p18, &st18, &px, WETH);
    assert_last_healthy(&p18, &st18, &px, DAI);
    assert_last_healthy(&p_loan6, &st_loan6, &px, WETH);
    assert_last_healthy(&p_loan6, &st_loan6, &px, DAI);
    assert_last_healthy(&p_coll6, &st_coll6, &px, WETH);
    assert_last_healthy(&p_coll6, &st_coll6, &px, DAI);

    // Same raw position, so the only difference is the decimal lift.
    // The ratio is exactly 10^12 when `SCALE` divides the product. When it
    // does not, ceil/floor on the two scales disagree by dust, not by the
    // factor itself.
    assert_eq!(weth_loan6 / weth18, lift, "6-dec loan collateral threshold");
    assert_eq!(dai18 / dai_loan6, lift, "6-dec loan debt threshold");
    assert_factor(weth18, weth_coll6, lift);
    assert_factor(dai_coll6, dai18, lift);

    // The dropped-decimals price is the 18/18 inversion. On the 6-dec loan
    // book both neighbors are already unhealthy, so it is not a trigger.
    let wrong = with_price(&px, WETH, Ray::from_raw(weth18));
    let pos = st_loan6.view(ALICE_ID, T0).unwrap();
    assert!(p_loan6.health(pos, &wrong).unwrap().hf < Ray::ONE);
    let wrong_up = with_price(&px, WETH, Ray::from_raw(weth18 + U256::from(1u8)));
    assert!(p_loan6.health(pos, &wrong_up).unwrap().hf < Ray::ONE);
}

fn open_market(
    loan_dec: u8,
    coll_dec: u8,
    borrow: U256,
    supply: U256,
    coll: U256,
) -> (
    liq_adapters_morpho_blue::MorphoBlue,
    liq_protocol::conformance::JournalStore,
) {
    let d = Deploy::new();
    let mut cfg = d.config();
    for a in &mut cfg.assets {
        if a.asset == WETH {
            a.decimals = coll_dec;
        }
        if a.asset == DAI {
            a.decimals = loan_dec;
        }
    }
    let p = liq_adapters_morpho_blue::MorphoBlue::new(cfg).expect("fixture config");
    let share = |assets: U256| assets * uint!(1_000_000_U256);
    let (b, t) = (DEPLOY_BLOCK, T0);
    let logs = vec![
        log(
            d.morpho,
            &ev::CreateMarket {
                id: MARKET_ID,
                marketParams: d.params(),
            },
            b,
            t,
        ),
        log(
            d.morpho,
            &ev::Supply {
                id: MARKET_ID,
                caller: d.bob,
                onBehalf: d.bob,
                assets: supply,
                shares: share(supply),
            },
            b,
            t,
        ),
        log(
            d.morpho,
            &ev::SupplyCollateral {
                id: MARKET_ID,
                caller: d.alice,
                onBehalf: d.alice,
                assets: coll,
            },
            b,
            t,
        ),
        log(
            d.morpho,
            &ev::Borrow {
                id: MARKET_ID,
                caller: d.alice,
                onBehalf: d.alice,
                receiver: d.alice,
                assets: borrow,
                shares: share(borrow),
            },
            b,
            t,
        ),
    ];
    let st = store_after(&p, &logs);
    (p, st)
}

/// `scaled / base` is `lift`, up to the ceil/floor that the two inversions
/// apply independently. A dropped factor would miss by ~`lift`, not by dust.
fn assert_factor(scaled: U256, base: U256, lift: U256) {
    let product = base * lift;
    let diff = if scaled > product {
        scaled - product
    } else {
        product - scaled
    };
    assert!(
        diff * U256::from(1_000_000u64) < scaled,
        "not the decimal lift: scaled {scaled} vs base*lift {product}"
    );
}

fn price_of(
    p: &liq_adapters_morpho_blue::MorphoBlue,
    st: &liq_protocol::conformance::JournalStore,
    px: &liq_types::PriceVector,
    asset: liq_types::AssetId,
) -> U256 {
    p.liquidation_price(st.view(ALICE_ID, T0).unwrap(), px, asset)
        .unwrap()
        .expect("threshold")
        .price
        .raw()
}

fn with_price(
    px: &liq_types::PriceVector,
    asset: liq_types::AssetId,
    price: Ray,
) -> liq_types::PriceVector {
    let mut out = px.clone();
    let slot = out
        .0
        .iter_mut()
        .find(|e| e.asset == asset)
        .expect("asset in the price vector");
    slot.price = price;
    out
}

fn assert_last_healthy(
    p: &liq_adapters_morpho_blue::MorphoBlue,
    st: &liq_protocol::conformance::JournalStore,
    px: &liq_types::PriceVector,
    asset: liq_types::AssetId,
) {
    let pos = st.view(ALICE_ID, T0).unwrap();
    let lp = p
        .liquidation_price(pos, px, asset)
        .unwrap()
        .expect("threshold");
    let at = with_price(px, asset, lp.price);
    let hf_at = p.health(pos, &at).unwrap().hf;
    assert!(hf_at >= Ray::ONE, "hf at returned price is {hf_at:?}");
    let below = with_price(px, asset, Ray::from_raw(lp.price.raw() - U256::from(1u8)));
    let above = with_price(px, asset, Ray::from_raw(lp.price.raw() + U256::from(1u8)));
    let unhealthy_below = p.health(pos, &below).unwrap().hf < Ray::ONE;
    let unhealthy_above = p.health(pos, &above).unwrap().hf < Ray::ONE;
    assert_ne!(
        unhealthy_below, unhealthy_above,
        "price {:?} is not a one-wei boundary",
        lp.price
    );
}
