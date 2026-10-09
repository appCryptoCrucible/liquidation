//! Frozen markets (`accrueInterest()` reverts) and markets outside the
//! config, against what the Comptroller and the cToken do with them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

mod common;

use alloy_primitives::{Address, Bytes};
use common::*;
use liq_adapters_compound_v2::events::{comptroller as cmp, views::accrueInterestCall};
use liq_adapters_compound_v2::{CompoundV2, Config};
use liq_protocol::conformance::JournalStore;
use liq_protocol::{HealthState, Protocol, ProtocolError, StateWriter};

/// [`JournalStore`] as the adapter's `MarketRows`.
struct Rows<'a>(&'a JournalStore);

impl liq_protocol::MarketRows for Rows<'_> {
    fn rows(&self, market: liq_types::MarketId) -> Option<&[liq_protocol::MarketRow]> {
        self.0.markets(market).ok()
    }
}

fn frozen_cfg(d: &Deploy, ctoken: Address) -> Config {
    let mut cfg = d.config();
    for c in &mut cfg.forks[0].ctokens {
        c.frozen = c.ctoken == ctoken;
    }
    cfg
}

fn store(p: &CompoundV2, d: &Deploy) -> JournalStore {
    let mut logs = listing_logs(d);
    logs.extend(activity_logs(d, ALICE_DEBT_LIQ));
    store_after(p, &logs)
}

/// A frozen market still counts in `getAccountLiquidity` (stored values),
/// so health is the plain fork's; but `liquidateBorrow` accrues both its
/// markets and a frozen one reverts, so no leg uses it.
#[test]
fn a_frozen_market_counts_in_health_and_takes_no_leg() {
    let d = Deploy::new();
    let px = prices(RAY_ONE, RAY_ONE);
    let plain = d.adapter();
    let st_plain = store(&plain, &d);
    let h_plain = plain
        .health(st_plain.view(ALICE_ID, T0).unwrap(), &px)
        .unwrap();
    assert_eq!(h_plain.state, HealthState::Liquidatable);
    assert!(plain
        .quote(st_plain.view(ALICE_ID, T0).unwrap(), &px)
        .unwrap()
        .is_some());

    for frozen in [d.cusdc, d.ceth] {
        let p = CompoundV2::new(assert_pin_registry(frozen_cfg(&d, frozen))).unwrap();
        let st = store(&p, &d);
        let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
        assert_eq!(h, h_plain, "frozen {frozen:#x}");
        // Alice's only debt (cUSDC) or only collateral (cETH) is frozen.
        assert_eq!(
            p.quote(st.view(ALICE_ID, T0).unwrap(), &px),
            Err(ProtocolError::EmptyQuote)
        );
    }
}

/// A frozen market has nothing to accrue: no rate read for it.
#[test]
fn a_frozen_market_is_not_read() {
    let d = Deploy::new();
    let plain = d.adapter();
    let st_plain = store(&plain, &d);
    let plain_reads = plain.state_reads(&Rows(&st_plain));
    assert!(plain_reads.iter().any(|r| r.target == d.cusdc));
    let p = CompoundV2::new(assert_pin_registry(frozen_cfg(&d, d.cusdc))).unwrap();
    let st = store(&p, &d);
    assert!(p
        .state_reads(&Rows(&st))
        .iter()
        .all(|r| r.target != d.cusdc));
}

/// The pin holds only while the market cannot accrue: once
/// `accrueInterest()` answers, the bind drops that cToken (the rest of the
/// fork binds, and an account that entered it fails closed).
#[test]
fn a_frozen_pin_whose_market_accrues_is_refused() {
    use alloy_sol_types::SolCall;
    let d = Deploy::new();
    let cfg = frozen_cfg(&d, d.cusdc);
    let mut rpc = pin_rpc_from_cfg(&cfg);
    rpc.push(
        d.cusdc,
        accrueInterestCall::SELECTOR,
        Bytes::copy_from_slice(&[0u8; 32]),
    );
    let mut cfg = cfg;
    let block = cfg.pinned_through;
    cfg.assert_live_registry(&rpc, block).unwrap();
    let pinned: Vec<Address> = cfg.forks[0].ctokens.iter().map(|c| c.ctoken).collect();
    assert_eq!(pinned, [d.ceth]);
}

/// The Comptroller counts every market an account entered. One outside the
/// config has its token events unfollowed, so its zero balance here proves
/// nothing: the position fails closed.
#[test]
fn an_entered_market_outside_the_config_fails_closed() {
    let d = Deploy::new();
    let p = d.adapter();
    let other = Address::repeat_byte(0x77);
    let mut logs = listing_logs(&d);
    logs.extend(activity_logs(&d, ALICE_DEBT_OK));
    let px = prices(RAY_ONE, RAY_ONE);
    let st = store_after(&p, &logs);
    assert!(p.health(st.view(ALICE_ID, T0).unwrap(), &px).is_ok());
    let (b, t) = (DEPLOY_BLOCK + 2, T0);
    logs.push(log(
        d.comptroller,
        &cmp::MarketListed { cToken: other },
        b,
        t,
    ));
    logs.push(log(
        d.comptroller,
        &cmp::MarketEntered {
            cToken: other,
            account: d.alice,
        },
        b,
        t,
    ));
    let st = store_after(&p, &logs);
    assert_eq!(
        p.health(st.view(ALICE_ID, T0).unwrap(), &px),
        Err(ProtocolError::OracleSourceMismatch)
    );
    // Bob never entered it.
    assert!(p.health(st.view(BOB_ID, T0).unwrap(), &px).is_ok());
}

/// The earlier `AccrueInterest(cashPrior, interestAccumulated, borrowIndex,
/// totalBorrows, totalReserves)` (DeFiPie's `PToken`) states the reserves:
/// the row takes them as given, and the exchange rate is DeFiPie's
/// `exchangeRateStoredInternal`, `(cash + borrows − reserves) / supply`.
#[test]
fn the_five_field_accrual_states_the_reserves() {
    use alloy_primitives::U256;
    use liq_adapters_compound_v2::events::ctoken_reserves;
    use liq_adapters_compound_v2::layout::CTokenRow;
    let d = Deploy::new();
    let p = d.adapter();
    let mut logs = listing_logs(&d);
    logs.extend(activity_logs(&d, ALICE_DEBT_OK));
    let (cash, borrows, reserves, index) = (
        U256::from(900_000_000_000_000_000_000u128),
        U256::from(52_000_000_000_000_000_000u128),
        U256::from(7_000_000_000_000_000_000u128),
        U256::from(1_040_000_000_000_000_000u128),
    );
    logs.push(log(
        d.ceth,
        &ctoken_reserves::AccrueInterest {
            cashPrior: cash,
            interestAccumulated: U256::from(2_000_000_000_000_000_000u128),
            borrowIndex: index,
            totalBorrows: borrows,
            totalReserves: reserves,
        },
        DEPLOY_BLOCK + 2,
        T0 + 24,
    ));
    let st = store_after(&p, &logs);
    let row = st
        .markets(MARKET)
        .unwrap()
        .iter()
        .map(|r| *r.body::<CTokenRow>().unwrap())
        .find(|b| Address::from(b.ctoken) == d.ceth)
        .unwrap();
    assert_eq!(U256::from(row.total_reserves), reserves);
    assert_eq!(U256::from(row.total_borrows), borrows);
    assert_eq!(U256::from(row.borrow_index), index);
    let supply = U256::from(row.total_supply);
    assert!(!supply.is_zero());
    let wad = U256::from(1_000_000_000_000_000_000u128);
    assert_eq!(
        U256::from(row.exchange_rate_mantissa),
        (cash + borrows - reserves) * wad / supply
    );
}

/// The original (2019) `AccrueInterest(interestAccumulated, borrowIndex,
/// totalBorrows)` of the official cETH and cUSDC: the index and borrows are
/// taken as given, the cash is left to the deltas, and the reserves grow by
/// the reserve factor's share (`CEther.sol` of `0x4ddc2d19…`:
/// `totalReservesNew = interestAccumulated * reserveFactor + totalReserves`).
#[test]
fn the_original_three_field_accrual_is_folded() {
    use alloy_primitives::U256;
    use liq_adapters_compound_v2::events::{ctoken, ctoken_original};
    use liq_adapters_compound_v2::layout::CTokenRow;
    let d = Deploy::new();
    let p = d.adapter();
    let mut logs = listing_logs(&d);
    logs.extend(activity_logs(&d, ALICE_DEBT_OK));
    let rf = U256::from(100_000_000_000_000_000u128); // 10 %
    logs.push(log(
        d.ceth,
        &ctoken::NewReserveFactor {
            oldReserveFactorMantissa: U256::ZERO,
            newReserveFactorMantissa: rf,
        },
        DEPLOY_BLOCK + 2,
        T0 + 12,
    ));
    let row_of = |st: &JournalStore| {
        st.markets(MARKET)
            .unwrap()
            .iter()
            .map(|r| *r.body::<CTokenRow>().unwrap())
            .find(|b| Address::from(b.ctoken) == d.ceth)
            .unwrap()
    };
    let before = row_of(&store_after(&p, &logs));
    let (interest, index, borrows) = (
        U256::from(3_000_000_000_000_000_000u128),
        U256::from(1_030_000_000_000_000_000u128),
        U256::from(4_000_000_000_000_000_000u128),
    );
    logs.push(log(
        d.ceth,
        &ctoken_original::AccrueInterest {
            interestAccumulated: interest,
            borrowIndex: index,
            totalBorrows: borrows,
        },
        DEPLOY_BLOCK + 3,
        T0 + 24,
    ));
    let after = row_of(&store_after(&p, &logs));
    assert_eq!(U256::from(after.borrow_index), index);
    assert_eq!(U256::from(after.total_borrows), borrows);
    assert_eq!(after.cash, before.cash);
    let wad = U256::from(1_000_000_000_000_000_000u128);
    assert_eq!(
        U256::from(after.total_reserves),
        U256::from(before.total_reserves) + interest * rf / wad
    );
    assert_eq!(u64::from(after.accrual_ts), T0 + 24);
}
