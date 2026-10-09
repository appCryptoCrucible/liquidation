//! Silo V2 interest accrual between events, from the per-block state reads,
//! the pair price read, and per-pair halts.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

mod common;

use alloy_primitives::{Address, U256};
use alloy_sol_types::{sol, SolCall, SolValue};
use common::*;
use liq_adapters_silo_v2::events::{factory, halt, silo};
use liq_adapters_silo_v2::layout::SiloRow;
use liq_protocol::conformance::JournalStore;
use liq_protocol::{
    BlockReason, DirtySet, HealthState, MarketFlags, MarketSlot, Protocol, ProtocolError,
    StateAnswer, StateRead, StateWriter,
};
use liq_types::Ray;

/// [`JournalStore`] as the adapter's `MarketRows`.
struct Rows<'a>(&'a JournalStore);

impl liq_protocol::MarketRows for Rows<'_> {
    fn rows(&self, market: liq_types::MarketId) -> Option<&[liq_protocol::MarketRow]> {
        self.0.markets(market).ok()
    }
}

sol! {
    struct Call3 {
        address target;
        bool allowFailure;
        bytes callData;
    }
    struct Call3Result {
        bool success;
        bytes returnData;
    }
    function aggregate3(Call3[] calls) external payable returns (Call3Result[] memory);
    function quote(uint256 baseAmount, address baseToken) external view returns (uint256);
}

/// A silo's four totals answers, as the chain encodes them.
struct Totals {
    debt_with_interest: u128,
    coll_with_interest: u128,
    stored_coll: u128,
    stored_debt: u128,
    interest_rate_timestamp: u64,
}

fn answer_data(read: &StateRead, t: &Totals) -> Vec<u8> {
    let w = |v: u128| U256::from(v);
    match read.tag >> 16 {
        0 => w(t.debt_with_interest).abi_encode(),
        1 => w(t.coll_with_interest).abi_encode(),
        2 => (w(t.stored_coll), w(t.stored_debt)).abi_encode_params(),
        3 => (
            U256::ZERO,
            U256::ZERO,
            U256::from(t.interest_rate_timestamp),
        )
            .abi_encode_params(),
        k => panic!("unknown read kind {k}"),
    }
}

/// Answer every read with `t` and fold them at `now`.
fn fold(p: &liq_adapters_silo_v2::SiloV2, st: &mut JournalStore, now: u64, t: &Totals) {
    let reads = p.state_reads(&Rows(st));
    let data: Vec<Vec<u8>> = reads.iter().map(|r| answer_data(r, t)).collect();
    let answers: Vec<StateAnswer<'_>> = reads
        .iter()
        .zip(&data)
        .map(|(read, d)| StateAnswer {
            read,
            success: true,
            data: d,
        })
        .collect();
    p.apply_state_reads(st, now, &answers).unwrap();
}

fn debt_row(st: &JournalStore) -> (SiloRow, u32) {
    let row = st
        .market(MarketSlot {
            market: MARKET,
            slot: 1,
        })
        .unwrap();
    (*row.body::<SiloRow>().unwrap(), row.last_update)
}

const D0: u128 = 100_000_000; // ALICE_DEBT_OK: alice's debt is all of silo1's
/// The fixture's interest: 37 debt assets a second on `D0`.
const GROWTH: u128 = 37;
const RAY: u128 = 1_000_000_000_000_000_000_000_000_000;

/// `from` grown to `to` over `dt` seconds as a relative rate per second in
/// RAY, rounded down: the definition the row stores.
fn rate(from: u128, to: u128, dt: u64) -> u128 {
    let r = U256::from(to - from) * U256::from(RAY) / (U256::from(from) * U256::from(dt));
    u128::try_from(r).unwrap()
}

/// `total` grown at `rate_ray` for `dt` seconds, rounded down.
fn grow(total: u128, rate_ray: u128, dt: u64) -> u128 {
    let g = U256::from(total) * U256::from(rate_ray) * U256::from(dt) / U256::from(RAY);
    total + u128::try_from(g).unwrap()
}

/// The first read after an accrual at T0: 1,000 s of interest.
fn first_read() -> Totals {
    Totals {
        debt_with_interest: D0 + GROWTH * 1_000,
        coll_with_interest: 0,
        stored_coll: 0,
        stored_debt: D0,
        interest_rate_timestamp: T0,
    }
}

#[test]
fn reads_ask_only_borrowed_silos_for_their_totals() {
    let d = Deploy::new();
    let (p, st) = full_store_d(&d);
    let reads = p.state_reads(&Rows(&st));
    // silo0 lends nothing; silo1 is borrowed from.
    assert_eq!(reads.len(), 4);
    assert!(reads
        .iter()
        .all(|r| r.target == d.silo1 && r.market == MARKET));
    let kinds: Vec<u64> = reads.iter().map(|r| r.tag >> 16).collect();
    assert_eq!(kinds, [0, 1, 2, 3]);
    assert!(reads.iter().all(|r| r.tag & 0xffff == 1));
}

fn full_store_d(d: &Deploy) -> (liq_adapters_silo_v2::SiloV2, JournalStore) {
    let p = d.adapter();
    let mut logs = listing_logs(d);
    logs.extend(activity_logs(d, U256::from(D0)));
    let st = store_after(&p, &logs);
    (p, st)
}

/// The same position with `debt_assets` already folded into silo1's total
/// by a `Borrow`, as if the interest had arrived as an event.
fn folded_store(d: &Deploy, debt_assets: u128) -> JournalStore {
    let p = d.adapter();
    let mut logs = listing_logs(d);
    let mut act = activity_logs(d, U256::ZERO);
    act.push(log(
        d.silo1,
        &silo::Borrow {
            sender: d.alice,
            receiver: d.alice,
            owner: d.alice,
            assets: U256::from(debt_assets),
            shares: U256::from(D0),
        },
        DEPLOY_BLOCK + 1,
        T0,
    ));
    logs.extend(act);
    store_after(&p, &logs)
}

#[test]
fn a_read_sets_the_chains_totals_and_health_projects_its_growth() {
    let d = Deploy::new();
    let (p, mut st) = full_store_d(&d);
    // The first read after the accrual measures the average since it.
    fold(&p, &mut st, T0 + 1_000, &first_read());
    let (b, at) = debt_row(&st);
    let r = rate(D0, D0 + GROWTH * 1_000, 1_000);
    assert_eq!(b.total_debt_assets, D0 + GROWTH * 1_000);
    assert_eq!(b.debt_rate_ray, r);
    assert_ne!(b.flags & SiloRow::GROWTH_KNOWN, 0);
    assert_eq!(u64::from(at), T0 + 1_000);
    assert_eq!(b.accrued_at, T0);

    // 1,000 s after the read: the health of a store where the grown total
    // came in by event.
    let px = prices(RAY_ONE, RAY_ONE);
    let projected = p
        .health(st.view(ALICE_ID, T0 + 2_000).unwrap(), &px)
        .unwrap();
    let oracle = folded_store(&d, grow(D0 + GROWTH * 1_000, r, 1_000));
    let expect = p.health(oracle.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(projected, expect);
    assert_eq!(
        liq_adapters_silo_v2::health::totals_at(
            st.market(MarketSlot {
                market: MARKET,
                slot: 1
            })
            .unwrap(),
            T0 + 2_000
        )
        .unwrap()
        .0,
        grow(D0 + GROWTH * 1_000, r, 1_000)
    );
    // And it moved: at the read's own instant the debt is lower.
    let at_read = p
        .health(st.view(ALICE_ID, T0 + 1_000).unwrap(), &px)
        .unwrap();
    assert!(at_read.debt_value < projected.debt_value);
}

/// No accrual between two reads (`interestRateTimestamp` unchanged): only
/// interest moved the total, and the rate is its growth between the reads,
/// not the (here slower) average since the accrual.
#[test]
fn a_second_read_without_an_accrual_measures_the_rate_now() {
    let d = Deploy::new();
    let (p, mut st) = full_store_d(&d);
    fold(&p, &mut st, T0 + 1_000, &first_read());
    let (before, now) = (D0 + GROWTH * 1_000, D0 + GROWTH * 1_000 + 600);
    fold(
        &p,
        &mut st,
        T0 + 1_012,
        &Totals {
            debt_with_interest: now,
            coll_with_interest: 0,
            stored_coll: 0,
            stored_debt: D0,
            interest_rate_timestamp: T0,
        },
    );
    let (b, at) = debt_row(&st);
    assert_eq!(b.total_debt_assets, now);
    assert_eq!(b.debt_rate_ray, rate(before, now, 12));
    assert_ne!(b.debt_rate_ray, rate(D0, now, 1_012));
    assert_eq!(u64::from(at), T0 + 1_012);
}

/// A silo whose last accrual is days old: the average since it is not the
/// rate now, so the first read only sets a base, and the rate is measured
/// once the debt has grown enough past it to resolve (100 units).
#[test]
fn a_stale_silo_waits_for_a_resolved_measurement() {
    let d = Deploy::new();
    let (p, mut st) = full_store_d(&d);
    let days = 30 * 86_400;
    let at = |debt: u128| Totals {
        debt_with_interest: debt,
        coll_with_interest: 0,
        stored_coll: 0,
        stored_debt: D0,
        interest_rate_timestamp: T0,
    };
    let base = 2 * D0;
    fold(&p, &mut st, T0 + days, &at(base));
    let (b, _) = debt_row(&st);
    assert_eq!(
        b.flags & SiloRow::GROWTH_KNOWN,
        0,
        "a 30-day average is not a rate"
    );
    assert_eq!(b.total_debt_assets, base);
    // 12 s on, 50 units: not resolved yet; the totals are the chain's.
    fold(&p, &mut st, T0 + days + 12, &at(base + 50));
    let (b, _) = debt_row(&st);
    assert_eq!(b.flags & SiloRow::GROWTH_KNOWN, 0);
    assert_eq!(b.total_debt_assets, base + 50);
    // 24 s on, 120 units: the rate over the 24 s from the base.
    fold(&p, &mut st, T0 + days + 24, &at(base + 120));
    let (b, _) = debt_row(&st);
    assert_ne!(b.flags & SiloRow::GROWTH_KNOWN, 0);
    assert_eq!(b.debt_rate_ray, rate(base, base + 120, 24));
}

/// Collateral grows by the debt's interest less `daoFee + deployerFee`
/// (`getCollateralAmountsWithInterest`), rounded as Silo rounds the fee.
#[test]
fn collateral_grows_by_the_interest_net_of_fees() {
    let d = Deploy::new();
    let (p, mut st) = full_store_d(&d);
    let coll = 5 * D0;
    fold(
        &p,
        &mut st,
        T0 + 1_000,
        &Totals {
            coll_with_interest: coll,
            ..first_read()
        },
    );
    let row = st
        .market(MarketSlot {
            market: MARKET,
            slot: 1,
        })
        .unwrap();
    let r = row.body::<SiloRow>().unwrap().debt_rate_ray;
    let (debt, grown_coll) = liq_adapters_silo_v2::health::totals_at(row, T0 + 1_777).unwrap();
    let accrued = grow(D0 + GROWTH * 1_000, r, 777) - (D0 + GROWTH * 1_000);
    assert_eq!(debt, D0 + GROWTH * 1_000 + accrued);
    let fees = accrued * INTEREST_FEE / 1_000_000_000_000_000_000;
    assert_eq!(grown_coll, coll + accrued - fees);
    assert!(fees > 0);
}

#[test]
fn accrued_interest_after_a_read_is_not_counted_twice() {
    let d = Deploy::new();
    let (p, mut st) = full_store_d(&d);
    fold(&p, &mut st, T0 + 1_000, &first_read());
    let r = debt_row(&st).0.debt_rate_ray;
    // The chain accrues at T0 + 2,000 and emits all the interest since T0;
    // the projection to that instant already holds it.
    let ev = log(
        d.silo1,
        &silo::AccruedInterest {
            accruedInterest: U256::from(GROWTH * 2_000),
        },
        DEPLOY_BLOCK + 2,
        T0 + 2_000,
    );
    p.apply_log(&mut st, &ev.view()).unwrap();
    let (b, at) = debt_row(&st);
    assert_eq!(b.total_debt_assets, grow(D0 + GROWTH * 1_000, r, 1_000));
    assert_eq!(u64::from(at), T0 + 2_000);

    // A read the same second as that accrual has no elapsed time to
    // measure: the totals are the chain's and the rate stays.
    fold(
        &p,
        &mut st,
        T0 + 2_000,
        &Totals {
            debt_with_interest: D0 + GROWTH * 2_000,
            coll_with_interest: 0,
            stored_coll: 0,
            stored_debt: D0 + GROWTH * 2_000,
            interest_rate_timestamp: T0 + 2_000,
        },
    );
    let (b, _) = debt_row(&st);
    assert_eq!(b.total_debt_assets, D0 + GROWTH * 2_000);
    assert_eq!(b.debt_rate_ray, r);
    assert_eq!(b.accrued_at, T0 + 2_000);
}

#[test]
fn before_any_read_accrued_interest_is_folded_from_the_event() {
    let d = Deploy::new();
    let (p, mut st) = full_store_d(&d);
    let ev = log(
        d.silo1,
        &silo::AccruedInterest {
            accruedInterest: U256::from(5_000u64),
        },
        DEPLOY_BLOCK + 2,
        T0 + 100,
    );
    p.apply_log(&mut st, &ev.view()).unwrap();
    let (b, _) = debt_row(&st);
    assert_eq!(b.total_debt_assets, D0 + 5_000);
    assert_eq!(b.flags & SiloRow::GROWTH_KNOWN, 0);
}

#[test]
fn a_read_whose_target_is_not_the_rows_silo_is_ignored() {
    let d = Deploy::new();
    let (p, mut st) = full_store_d(&d);
    let mut reads = p.state_reads(&Rows(&st));
    for r in &mut reads {
        r.target = Address::repeat_byte(0x66);
    }
    let t = Totals {
        debt_with_interest: D0 * 2,
        coll_with_interest: 0,
        stored_coll: 0,
        stored_debt: D0,
        interest_rate_timestamp: T0,
    };
    let data: Vec<Vec<u8>> = reads.iter().map(|r| answer_data(r, &t)).collect();
    let answers: Vec<StateAnswer<'_>> = reads
        .iter()
        .zip(&data)
        .map(|(read, d)| StateAnswer {
            read,
            success: true,
            data: d,
        })
        .collect();
    assert!(p
        .apply_state_reads(&mut st, T0 + 10, &answers)
        .unwrap()
        .is_empty());
    assert_eq!(debt_row(&st).0.total_debt_assets, D0);
}

/// The pair read is one `aggregate3` of both oracles' quotes of a million
/// whole tokens; the decode publishes each side's quote (× 1e18), slot 1
/// (the numeraire, both sides having oracles) first.
#[test]
fn the_pair_price_read_quotes_both_sides_in_one_call() {
    let d = Deploy::new();
    let (p, st) = full_store_d(&d);
    let reads = p.price_reads(&Rows(&st));
    assert_eq!(reads.len(), 1);
    let r = &reads[0];
    assert_eq!(
        r.target,
        alloy_primitives::address!("cA11bde05977b3631167028862bE2a173976CA11")
    );
    assert_eq!(r.assets, [DEBT, COLL]);
    let call = aggregate3Call::abi_decode(&r.calldata).unwrap();
    assert_eq!(call.calls.len(), 2);
    let q0 = quoteCall::abi_decode(&call.calls[0].callData).unwrap();
    assert_eq!(call.calls[0].target, Address::repeat_byte(0xd0));
    assert_eq!(q0.baseToken, d.token0);
    assert_eq!(q0.baseAmount, U256::from(10u64).pow(U256::from(24u64)));
    let q1 = quoteCall::abi_decode(&call.calls[1].callData).unwrap();
    assert_eq!(call.calls[1].target, Address::repeat_byte(0xd1));
    assert_eq!(q1.baseToken, d.token1);
    assert_eq!(q1.baseAmount, U256::from(10u64).pow(U256::from(12u64)));

    let a0 = U256::from(2_500_000_000_000_000_000_000_000u128);
    let a1 = U256::from(999_900_000_000_000_000_000_000u128);
    let ret = aggregate3Call::abi_encode_returns(&vec![
        Call3Result {
            success: true,
            returnData: a0.abi_encode().into(),
        },
        Call3Result {
            success: true,
            returnData: a1.abi_encode().into(),
        },
    ]);
    let mut out = Vec::new();
    p.decode_prices(r, &ret, &mut out).unwrap();
    let wad = U256::from(10u64).pow(U256::from(18u64));
    assert_eq!(
        out,
        [
            (DEBT, Ray::from_raw(a1 * wad)),
            (COLL, Ray::from_raw(a0 * wad))
        ]
    );
}

/// A side without an oracle is its raw amount (`getPositionValues`), needs
/// no call, and is the numeraire published first.
#[test]
fn a_side_without_an_oracle_is_its_raw_amount_and_the_numeraire() {
    let d = Deploy::new();
    let mut cfg = d.config();
    cfg.pairs[0].silo1.solvency_oracle = Address::ZERO;
    let p = liq_adapters_silo_v2::SiloV2::new(cfg).unwrap();
    let mut logs = listing_logs(&d);
    logs.extend(activity_logs(&d, U256::from(D0)));
    let st = store_after(&p, &logs);
    let reads = p.price_reads(&Rows(&st));
    let r = &reads[0];
    assert_eq!(r.assets, [DEBT, COLL]);
    let call = aggregate3Call::abi_decode(&r.calldata).unwrap();
    assert_eq!(call.calls.len(), 1);
    let a0 = U256::from(2_500_000_000_000u128);
    let ret = aggregate3Call::abi_encode_returns(&vec![Call3Result {
        success: true,
        returnData: a0.abi_encode().into(),
    }]);
    let mut out = Vec::new();
    p.decode_prices(r, &ret, &mut out).unwrap();
    let wad = U256::from(10u64).pow(U256::from(18u64));
    let raw1 = U256::from(10u64).pow(U256::from(12u64));
    assert_eq!(
        out,
        [
            (DEBT, Ray::from_raw(raw1 * wad)),
            (COLL, Ray::from_raw(a0 * wad))
        ]
    );
    // A failed quote publishes nothing.
    let failed = aggregate3Call::abi_encode_returns(&vec![Call3Result {
        success: false,
        returnData: Vec::new().into(),
    }]);
    let mut out = Vec::new();
    p.decode_prices(r, &failed, &mut out).unwrap();
    assert!(out.is_empty());
}

fn underwater(d: &Deploy) -> (liq_adapters_silo_v2::SiloV2, JournalStore) {
    let p = d.adapter();
    let mut logs = listing_logs(d);
    logs.extend(activity_logs_pos(d, ALICE_COLL_UNDER, ALICE_DEBT_UNDER));
    let st = store_after(&p, &logs);
    (p, st)
}

#[test]
fn a_halt_log_from_one_pair_halts_that_pair_only() {
    let d = Deploy::new();
    let (p, mut st) = underwater(&d);
    let px = prices(RAY_ONE, RAY_ONE);
    assert_eq!(
        p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap().state,
        HealthState::Liquidatable
    );
    let up = halt::Upgraded {
        implementation: Address::repeat_byte(0x77),
    };
    // Before the pin: history, folded.
    let before = log(d.hook, &up, DEPLOY_BLOCK, T0);
    assert_eq!(
        p.apply_log(&mut st, &before.view()).unwrap(),
        DirtySet::None
    );
    // After: that pair refuses liquidation; the ingest goes on.
    let after = log(d.hook, &up, DEPLOY_BLOCK + 1, T0);
    assert!(matches!(
        p.apply_log(&mut st, &after.view()).unwrap(),
        DirtySet::MarketReprice(_)
    ));
    for slot in [0, 1] {
        let row = st
            .market(MarketSlot {
                market: MARKET,
                slot,
            })
            .unwrap();
        assert!(row.flags.contains(MarketFlags::PAUSED));
        assert_ne!(row.body::<SiloRow>().unwrap().flags & SiloRow::HALTED, 0);
    }
    assert_eq!(
        p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap().state,
        HealthState::Blocked {
            reason: BlockReason::Paused
        }
    );
    // A later event on the pair keeps it halted.
    let dep = log(
        d.silo0,
        &silo::Deposit {
            sender: d.bob,
            owner: d.bob,
            assets: U256::from(1u64),
            shares: U256::from(1u64),
        },
        DEPLOY_BLOCK + 2,
        T0 + 12,
    );
    p.apply_log(&mut st, &dep.view()).unwrap();
    let row = st
        .market(MarketSlot {
            market: MARKET,
            slot: 0,
        })
        .unwrap();
    assert!(row.flags.contains(MarketFlags::PAUSED));
}

#[test]
fn a_hook_swap_on_one_pair_halts_that_pair() {
    let d = Deploy::new();
    let (p, mut st) = underwater(&d);
    let ev = log(
        d.factory,
        &factory::NewSiloHook {
            silo: d.silo0,
            hook: Address::repeat_byte(0xff),
        },
        DEPLOY_BLOCK + 1,
        T0,
    );
    assert!(matches!(
        p.apply_log(&mut st, &ev.view()).unwrap(),
        DirtySet::MarketReprice(_)
    ));
    let px = prices(RAY_ONE, RAY_ONE);
    assert_eq!(
        p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap().state,
        HealthState::Blocked {
            reason: BlockReason::Paused
        }
    );
}

#[test]
fn the_shared_factory_still_halts_the_protocol() {
    let d = Deploy::new();
    let (p, mut st) = underwater(&d);
    let ev = log(
        d.factory,
        &halt::Upgraded {
            implementation: Address::repeat_byte(0x77),
        },
        DEPLOY_BLOCK + 1,
        T0,
    );
    assert_eq!(
        p.apply_log(&mut st, &ev.view()),
        Err(ProtocolError::HaltSignal)
    );
}
