//! When interest alone makes a Compound position liquidatable, against
//! `accrueInterest` (`a3214f67`): after `n` blocks at a borrow rate `r` per
//! block, a market's borrow index is `index · (1 + r · n / 1e18)`, and the
//! account's borrow grows with it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

mod common;

use alloy_primitives::U256;
use common::*;
use liq_adapters_compound_v2::layout::CTokenRow;
use liq_adapters_compound_v2::math::addr_from;
use liq_protocol::{HealthState, MarketSlot, Protocol, StateWriter};

/// Alice holds 100 cETH-worth at a 0.75 collateral factor against 50 of
/// USDC debt: health 1.5. cUSDC's rate is set to 0.5e18 / 1000.5 per block
/// (truncated), so the debt passes the 75 limit when `r · n > 0.5e18`:
/// after 1,000 blocks it is 74.99 (healthy), after 1,001 75.01. A block is
/// 12 s from the market's last accrual, so the first liquidatable second is
/// `T0 + 1001 · 12`. cETH's rate stays unread, so the collateral holds.
#[test]
fn interest_alone_crosses_at_the_block_accrue_interest_says() {
    let d = Deploy::new();
    let (p, mut st) = full_store(&d, ALICE_DEBT_OK);
    let px = prices(RAY_ONE, RAY_ONE);
    let h = p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    assert_eq!(h.state, HealthState::Healthy);
    assert_eq!(
        h.hf.raw(),
        RAY_ONE * U256::from(3u64) / U256::from(2u64),
        "health 1.5"
    );
    assert_eq!(
        p.time_to_cross(st.view(ALICE_ID, T0).unwrap(), &px)
            .unwrap(),
        None,
        "rate unread"
    );

    let rate = 500_000_000_000_000_000u128 * 2 / 2001;
    let rows = st.markets(MARKET).unwrap().len();
    let mut found = false;
    for slot in 0..u16::try_from(rows).unwrap() {
        let at = MarketSlot {
            market: MARKET,
            slot,
        };
        let mut row = *st.market(at).unwrap();
        let Ok(b) = row.body_mut::<CTokenRow>() else {
            continue;
        };
        if addr_from(b.ctoken) != CUSDC {
            continue;
        }
        b.borrow_rate_per_block = u64::try_from(rate).unwrap();
        b.accrual_ts = u32::try_from(T0).unwrap();
        b.flags |= CTokenRow::RATE_KNOWN;
        st.set_market(at, row).unwrap();
        found = true;
    }
    assert!(found, "cUSDC row");

    let want = T0 + 1001 * 12;
    let state = |ts: u64| p.health(st.view(ALICE_ID, ts).unwrap(), &px).unwrap().state;
    assert_eq!(state(T0 + 1000 * 12), HealthState::Healthy);
    assert_eq!(
        state(want - 1),
        HealthState::Healthy,
        "the slot has not ended"
    );
    assert_eq!(state(want), HealthState::Liquidatable);
    assert_eq!(
        p.time_to_cross(st.view(ALICE_ID, T0).unwrap(), &px)
            .unwrap(),
        Some(want)
    );
    // Already past it: now.
    assert_eq!(
        p.time_to_cross(st.view(ALICE_ID, want + 100).unwrap(), &px)
            .unwrap(),
        Some(want + 100)
    );
}
