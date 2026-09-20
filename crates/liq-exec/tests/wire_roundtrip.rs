//! Round trip of the packed plan against the committed fixtures that
//! `contracts/test/unit/PlanDecode.t.sol` decodes with the Solidity decoder.
//!
//! Three independent artefacts must agree: (1) the fixture bytes in
//! `contracts/test/fixtures/*.hex`, produced by the test-local encoder below
//! from the field values in `expected_*()`; (2) this crate's decoder reading
//! those bytes back to the same values; (3) the Solidity test asserting the
//! same constants on the same file. The proptest over randomised plans is
//! WP 10B's (PLAN-ENCODING §4); these fixtures vary group count, legs per
//! group, adapter tails, repay-swap count, profit-swap count and `data`
//! length including the minimum of each.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use alloy_primitives::{Address, B256};
use liq_exec::wire::{
    decode_liq_leg, LegTail, Plan, WireError, FLAG_SWEEP, LEG_EXACT_OUT, LEG_TAKE_BALANCE,
    VENUE_ROUTER, VENUE_UNIV3_POOL,
};
use liq_protocol::ExecutorAdapter;
use liq_types::FlashProvider;

// ───────────────────────── test-local encoder ─────────────────────────
// Deliberately naive: straight-line pushes per PLAN-ENCODING §1, nothing
// shared with the decoder under test.

struct Swap {
    venue: u8,
    token_in: Address,
    token_out: Address,
    flags: u8,
    amount: u128,
    data: Vec<u8>,
}

struct Leg {
    adapter: ExecutorAdapter,
    market: Address,
    borrower: Address,
    collateral: Address,
    repay: u128,
    tail: Vec<u8>,
}

struct Group {
    provider: FlashProvider,
    source: Address,
    debt: Address,
    amount: u128,
    legs: Vec<Leg>,
    repay_swaps: Vec<Swap>,
}

struct Fixture {
    flags: u8,
    bid_bps: u16,
    gas: u128,
    min_profit: u128,
    groups: Vec<Group>,
    profit_swaps: Vec<Swap>,
}

fn enc_swap(b: &mut Vec<u8>, s: &Swap) {
    b.push(s.venue);
    b.extend_from_slice(s.token_in.as_slice());
    b.extend_from_slice(s.token_out.as_slice());
    b.push(s.flags);
    b.extend_from_slice(&s.amount.to_be_bytes());
    b.extend_from_slice(&u16::try_from(s.data.len()).unwrap().to_be_bytes());
    b.extend_from_slice(&s.data);
}

fn encode(f: &Fixture) -> Vec<u8> {
    let mut b = Vec::new();
    b.push(f.flags);
    b.extend_from_slice(&f.bid_bps.to_be_bytes());
    b.extend_from_slice(&f.gas.to_be_bytes());
    b.extend_from_slice(&f.min_profit.to_be_bytes());
    b.push(u8::try_from(f.groups.len()).unwrap());
    for g in &f.groups {
        b.push(g.provider as u8);
        b.extend_from_slice(g.source.as_slice());
        b.extend_from_slice(g.debt.as_slice());
        b.extend_from_slice(&g.amount.to_be_bytes());
        b.push(u8::try_from(g.legs.len()).unwrap());
        b.push(u8::try_from(g.repay_swaps.len()).unwrap());
        for l in &g.legs {
            b.push(l.adapter as u8);
            b.extend_from_slice(l.market.as_slice());
            b.extend_from_slice(l.borrower.as_slice());
            b.extend_from_slice(l.collateral.as_slice());
            b.extend_from_slice(&l.repay.to_be_bytes());
            b.extend_from_slice(&l.tail);
        }
        for s in &g.repay_swaps {
            enc_swap(&mut b, s); // no count prefix
        }
    }
    b.push(u8::try_from(f.profit_swaps.len()).unwrap());
    for s in &f.profit_swaps {
        enc_swap(&mut b, s);
    }
    b
}

// ───────────────────────────── fixtures ─────────────────────────────
// Field values are also hard-coded in PlanDecode.t.sol. Change both.

const fn a(b: u8) -> Address {
    Address::repeat_byte(b)
}

const WETH: Address = a(0xEE);
const POOL_V3: Address = a(0xA3);
const SPOKE_V4: Address = a(0xA4);
const MORPHO: Address = a(0xBB);
const DEBT0: Address = a(0xD1);
const DEBT1: Address = a(0xD2);
const DAI: Address = a(0xDA);
const COLL0: Address = a(0xC0);
const COLL1: Address = a(0xC1);
const COLL2: Address = a(0xC2);
const MORPHO_ID: B256 = B256::repeat_byte(0xE1);

fn expected_full() -> Fixture {
    Fixture {
        flags: FLAG_SWEEP,
        bid_bps: 9_800,
        gas: 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10,
        min_profit: 12_345_678_901_234_567_890,
        groups: vec![
            Group {
                provider: FlashProvider::Aave,
                source: a(0x11),
                debt: DEBT0,
                amount: 1_000_000_000_000,
                legs: vec![
                    Leg {
                        adapter: ExecutorAdapter::AaveV3,
                        market: POOL_V3,
                        borrower: a(0xB0),
                        collateral: COLL0,
                        repay: 500_000_000_000,
                        tail: vec![],
                    },
                    Leg {
                        adapter: ExecutorAdapter::AaveV4,
                        market: SPOKE_V4,
                        borrower: a(0xB1),
                        collateral: COLL1,
                        repay: 400_000_000_000,
                        tail: vec![0x00, 0x07, 0x00, 0x03], // collId 7, debtId 3
                    },
                ],
                repay_swaps: vec![Swap {
                    venue: VENUE_UNIV3_POOL,
                    token_in: COLL0,
                    token_out: DEBT0,
                    flags: LEG_EXACT_OUT,
                    amount: 1_000_500_000_000,
                    data: a(0xF0).to_vec(),
                }],
            },
            Group {
                provider: FlashProvider::UniV4,
                source: a(0x22),
                debt: DEBT1,
                amount: 7_000_000_000_000_000_000,
                legs: vec![Leg {
                    adapter: ExecutorAdapter::MorphoBlue,
                    market: MORPHO,
                    borrower: a(0xB2),
                    collateral: COLL2,
                    repay: 6_000_000_000_000_000_000,
                    tail: MORPHO_ID.to_vec(),
                }],
                repay_swaps: vec![], // minimum
            },
            Group {
                provider: FlashProvider::SkyDss,
                source: a(0x44),
                debt: DAI,
                amount: 3_000_000_000_000_000_000,
                legs: vec![Leg {
                    adapter: ExecutorAdapter::AaveV3,
                    market: POOL_V3,
                    borrower: a(0xB3),
                    collateral: COLL0,
                    repay: 2_999_000_000_000_000_000,
                    tail: vec![],
                }],
                repay_swaps: vec![
                    Swap {
                        venue: VENUE_ROUTER,
                        token_in: COLL0,
                        token_out: DAI,
                        flags: LEG_EXACT_OUT,
                        amount: 1_500_000_000_000_000_000,
                        data: {
                            let mut d = a(0x77).to_vec(); // ROUTER_A
                            d.extend_from_slice(&[0x5Au8; 36]); // router calldata
                            d
                        },
                    },
                    Swap {
                        venue: VENUE_UNIV3_POOL,
                        token_in: COLL0,
                        token_out: DAI,
                        flags: LEG_EXACT_OUT,
                        amount: 1_500_000_000_000_000_000,
                        data: a(0xF1).to_vec(),
                    },
                ],
            },
        ],
        profit_swaps: vec![
            Swap {
                venue: VENUE_UNIV3_POOL,
                token_in: COLL0,
                token_out: WETH,
                flags: LEG_TAKE_BALANCE,
                amount: 0,
                data: a(0xF2).to_vec(),
            },
            Swap {
                venue: VENUE_UNIV3_POOL,
                token_in: COLL2,
                token_out: WETH,
                flags: LEG_TAKE_BALANCE,
                amount: 0,
                data: a(0xF3).to_vec(),
            },
        ],
    }
}

/// One group, one leg, no repay swaps, no profit swaps — every minimum.
fn expected_min() -> Fixture {
    Fixture {
        flags: 0,
        bid_bps: 0,
        gas: 0,
        min_profit: 1,
        groups: vec![Group {
            provider: FlashProvider::Morpho,
            source: MORPHO,
            debt: WETH,
            amount: 1,
            legs: vec![Leg {
                adapter: ExecutorAdapter::AaveV4,
                market: SPOKE_V4,
                borrower: a(0x01),
                collateral: COLL1,
                repay: 1,
                tail: vec![0xFF, 0xFF, 0x00, 0x00], // collId 65535, debtId 0
            }],
            repay_swaps: vec![],
        }],
        profit_swaps: vec![],
    }
}

fn fixture_bytes(name: &str) -> Vec<u8> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/test/fixtures/"
    );
    let hex = std::fs::read_to_string(format!("{path}{name}.hex"))
        .unwrap_or_else(|e| panic!("fixture {name}: {e}"));
    let hex = hex.trim().trim_start_matches("0x");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

// ─────────────────────────────── tests ───────────────────────────────

/// Regenerates the fixture hex when the layout changes (PLAN-ENCODING §6):
/// `cargo test -p liq-exec --test wire_roundtrip print_fixtures -- --ignored --nocapture`.
#[test]
#[ignore = "bootstrap tool, prints fixture hex"]
fn print_fixtures() {
    for (name, f) in [
        ("plan_v1_full", expected_full()),
        ("plan_v1_min", expected_min()),
    ] {
        println!("{name}: 0x{}", alloy_primitives::hex::encode(encode(&f)));
    }
}

#[test]
fn full_fixture_bytes_match_committed_file() {
    assert_eq!(encode(&expected_full()), fixture_bytes("plan_v1_full"));
    assert_eq!(encode(&expected_min()), fixture_bytes("plan_v1_min"));
}

#[test]
fn full_fixture_decodes_field_for_field() {
    let bytes = fixture_bytes("plan_v1_full");
    let plan = Plan::parse(&bytes).unwrap();
    let h = plan.header();
    assert_eq!(h.flags, FLAG_SWEEP);
    assert_eq!(h.bid_bps, 9_800);
    assert_eq!(h.gas_cost_wei, 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10);
    assert_eq!(h.min_profit, 12_345_678_901_234_567_890);
    assert_eq!(h.group_count, 3);

    let groups: Vec<_> = plan.groups().map(Result::unwrap).collect();
    assert_eq!(groups.len(), 3);
    // Iterator and re-walk agree on every group.
    for (i, g) in groups.iter().enumerate() {
        assert_eq!(plan.group(u8::try_from(i).unwrap()).unwrap().head, g.head);
    }

    let g0 = &groups[0];
    assert_eq!(g0.head.provider, FlashProvider::Aave);
    assert_eq!(g0.head.flash_source, a(0x11));
    assert_eq!(g0.head.debt_asset, DEBT0);
    assert_eq!(g0.head.flash_amount, 1_000_000_000_000);
    assert_eq!(g0.head.liq_count, 2);
    assert_eq!(g0.head.repay_swap_count, 1);
    assert_eq!(g0.head.liq_offset, 36 + 59);
    let legs: Vec<_> = g0.liq_legs().map(Result::unwrap).collect();
    assert_eq!(legs[0].adapter, ExecutorAdapter::AaveV3);
    assert_eq!(legs[0].market, POOL_V3);
    assert_eq!(legs[0].borrower, a(0xB0));
    assert_eq!(legs[0].collateral_asset, COLL0);
    assert_eq!(legs[0].repay_amount, 500_000_000_000);
    assert_eq!(legs[0].tail, LegTail::None);
    assert_eq!(legs[1].adapter, ExecutorAdapter::AaveV4);
    assert_eq!(legs[1].market, SPOKE_V4);
    assert_eq!(legs[1].borrower, a(0xB1));
    assert_eq!(legs[1].collateral_asset, COLL1);
    assert_eq!(legs[1].repay_amount, 400_000_000_000);
    assert_eq!(
        legs[1].tail,
        LegTail::AaveV4 {
            collateral_reserve_id: 7,
            debt_reserve_id: 3
        }
    );
    // 77 + 0 + 77 + 4 bytes of legs.
    assert_eq!(g0.head.repay_swap_offset, g0.head.liq_offset + 77 + 81);
    let rs: Vec<_> = g0.repay_swaps().map(Result::unwrap).collect();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0].venue, VENUE_UNIV3_POOL);
    assert_eq!(rs[0].token_in, COLL0);
    assert_eq!(rs[0].token_out, DEBT0);
    assert_eq!(rs[0].flags, LEG_EXACT_OUT);
    assert_eq!(rs[0].amount, 1_000_500_000_000);
    assert_eq!(rs[0].data, a(0xF0).as_slice());
    assert_eq!(g0.head.next, g0.head.repay_swap_offset + 60 + 20);

    let g1 = &groups[1];
    assert_eq!(g1.head.provider, FlashProvider::UniV4);
    assert_eq!(g1.head.flash_source, a(0x22));
    assert_eq!(g1.head.debt_asset, DEBT1);
    assert_eq!(g1.head.flash_amount, 7_000_000_000_000_000_000);
    assert_eq!(g1.head.liq_count, 1);
    assert_eq!(g1.head.repay_swap_count, 0);
    assert_eq!(g1.head.liq_offset, g0.head.next + 59);
    let l = g1.liq_legs().next().unwrap().unwrap();
    assert_eq!(l.adapter, ExecutorAdapter::MorphoBlue);
    assert_eq!(l.market, MORPHO);
    assert_eq!(l.borrower, a(0xB2));
    assert_eq!(l.collateral_asset, COLL2);
    assert_eq!(l.repay_amount, 6_000_000_000_000_000_000);
    assert_eq!(
        l.tail,
        LegTail::Morpho {
            market_id: MORPHO_ID
        }
    );
    assert_eq!(g1.repay_swaps().count(), 0);
    assert_eq!(g1.head.next, g1.head.liq_offset + 77 + 32);

    let g2 = &groups[2];
    assert_eq!(g2.head.provider, FlashProvider::SkyDss);
    assert_eq!(g2.head.flash_source, a(0x44));
    assert_eq!(g2.head.debt_asset, DAI);
    assert_eq!(g2.head.flash_amount, 3_000_000_000_000_000_000);
    assert_eq!(g2.head.liq_count, 1);
    assert_eq!(g2.head.repay_swap_count, 2);
    let l = g2.liq_legs().next().unwrap().unwrap();
    assert_eq!(l.borrower, a(0xB3));
    assert_eq!(l.repay_amount, 2_999_000_000_000_000_000);
    let rs: Vec<_> = g2.repay_swaps().map(Result::unwrap).collect();
    assert_eq!(rs.len(), 2);
    assert_eq!(rs[0].venue, VENUE_ROUTER);
    assert_eq!(rs[0].token_in, COLL0);
    assert_eq!(rs[0].token_out, DAI);
    assert_eq!(rs[0].flags, LEG_EXACT_OUT);
    assert_eq!(rs[0].amount, 1_500_000_000_000_000_000);
    assert_eq!(rs[0].data.len(), 56);
    assert_eq!(&rs[0].data[..20], a(0x77).as_slice());
    assert!(rs[0].data[20..].iter().all(|b| *b == 0x5A));
    assert_eq!(rs[1].venue, VENUE_UNIV3_POOL);
    assert_eq!(rs[1].data, a(0xF1).as_slice());

    // Profit blob: count byte, then legs.
    assert_eq!(h.profit_swap_offset, g2.head.next);
    assert_eq!(bytes[h.profit_swap_offset], 2);
    let ps: Vec<_> = plan.profit_swaps().unwrap().map(Result::unwrap).collect();
    assert_eq!(ps.len(), 2);
    assert_eq!(ps[0].token_in, COLL0);
    assert_eq!(ps[0].token_out, WETH);
    assert_eq!(ps[0].flags, LEG_TAKE_BALANCE);
    assert_eq!(ps[0].amount, 0);
    assert_eq!(ps[0].data, a(0xF2).as_slice());
    assert_eq!(ps[1].token_in, COLL2);
    assert_eq!(ps[1].data, a(0xF3).as_slice());
}

#[test]
fn min_fixture_decodes_and_hits_every_minimum() {
    let bytes = fixture_bytes("plan_v1_min");
    // 35 + 1 + 59 + 77 + 4 + 1
    assert_eq!(bytes.len(), 177);
    let plan = Plan::parse(&bytes).unwrap();
    let h = plan.header();
    assert_eq!(
        (h.flags, h.bid_bps, h.gas_cost_wei, h.min_profit),
        (0, 0, 0, 1)
    );
    assert_eq!(h.group_count, 1);
    let g = plan.group(0).unwrap();
    assert_eq!(g.head.provider, FlashProvider::Morpho);
    assert_eq!(g.head.flash_source, MORPHO);
    assert_eq!(g.head.debt_asset, WETH);
    assert_eq!(g.head.flash_amount, 1);
    assert_eq!(g.head.repay_swap_count, 0);
    let l = g.liq_legs().next().unwrap().unwrap();
    assert_eq!(
        l.tail,
        LegTail::AaveV4 {
            collateral_reserve_id: u16::MAX,
            debt_reserve_id: 0
        }
    );
    assert_eq!(l.repay_amount, 1);
    assert_eq!(h.profit_swap_offset, 176);
    assert_eq!(plan.profit_swaps().unwrap().count(), 0);
    assert_eq!(plan.group(1), Err(WireError::NoGroups));
}

/// Mutation #2 (TESTING.md §4): an off-by-one in the leg stride. Dropping or
/// adding a byte anywhere inside a leg shifts every later field and the
/// walked length no longer matches — the decoder must reject, not misread.
#[test]
fn stride_mutations_are_rejected() {
    let good = fixture_bytes("plan_v1_full");
    assert!(Plan::parse(&good).is_ok());

    // Remove one byte from the V4 tail of group 0 leg 1.
    let leg1 = 36 + 59 + 77;
    let mut short = good.clone();
    short.remove(leg1 + 77 + 3);
    assert!(Plan::parse(&short).is_err());

    // Add one byte inside the last profit leg's data: every count and offset
    // is unchanged, so the walk lands one byte short of the blob.
    let mut long = good.clone();
    long.insert(good.len() - 5, 0);
    assert_eq!(
        Plan::parse(&long),
        Err(WireError::BadPlanLength {
            walked: good.len(),
            actual: good.len() + 1
        })
    );

    // Unknown adapter byte in the V3 leg fails at that leg, before anything
    // after it is read.
    let mut bad = good.clone();
    bad[36 + 59] = 3;
    assert_eq!(Plan::parse(&bad), Err(WireError::UnknownAdapter(3)));
    assert_eq!(
        decode_liq_leg(&bad, 36 + 59).map(|(l, _)| l.adapter),
        Err(WireError::UnknownAdapter(3))
    );

    // Trailing garbage: walked length != actual.
    let mut trailing = good;
    trailing.push(0);
    assert!(matches!(
        Plan::parse(&trailing),
        Err(WireError::BadPlanLength { .. })
    ));
}
