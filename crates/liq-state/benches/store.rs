//! GUIDE 02 acceptance budgets, measured (WP 02A):
//!
//! - `sweep_1k`: build a `PositionRef` for each of 1,000 three-slot positions
//!   and fold every field `health()` reads (mask bits, both balances, both
//!   `MarketRow` lines) — the store's share of a sweep. Budget < 50 µs.
//! - `unwind_128_x1k` / `unwind_128_x10k`: unwind 128 blocks of 1,000 (10,000)
//!   mutations over 1,000 positions. Budget < 5 ms.
//! - `apply_block_10k`: apply one 10,000-mutation block (journal + write).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    unreachable_pub
)]

use alloy_primitives::Address;
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use liq_protocol::{FeedId, MarketFlags, MarketRow, MarketSlot, StateWriter};
use liq_state::{StateStore, StoreConfig, UndoCapacity};
use liq_types::{AssetId, MarketId, PositionId, PositionKey, ProtocolId, RayU128};
use std::hint::black_box;

const MARKET: MarketId = MarketId(0);
const ROWS: u16 = 40;
const POSITIONS: u32 = 1_000;
const BASE: u64 = 20_000_000;

fn row(asset: u16, tag: u32) -> MarketRow {
    MarketRow {
        supply_index: RayU128::from_raw(1_000_000_000_000_000_000_000_000_000 + u128::from(tag)),
        debt_index: RayU128::from_raw(1_000_000_000_000_000_000_000_000_000 + u128::from(tag)),
        supply_rate: RayU128::from_raw(u128::from(tag)),
        debt_rate: RayU128::from_raw(u128::from(tag)),
        dust_floor: 0,
        last_update: tag,
        target_hf: 10_500,
        hub_ref: u16::MAX,
        liq_threshold: 8_000,
        ltv: 7_500,
        price_feed: FeedId(asset),
        asset: AssetId(asset),
        max_liq_bonus: 500,
        hf_for_max_bonus: 9_500,
        liq_bonus_factor: 10_000,
        decimals: 18,
        flags: MarketFlags::NONE,
        _pad: [0; 22],
    }
}

fn key(user: u32) -> PositionKey {
    let mut b = [0u8; 20];
    b[..4].copy_from_slice(&user.to_be_bytes());
    PositionKey {
        protocol: ProtocolId(1),
        market: MARKET,
        user: Address::from(b),
    }
}

/// 1,000 positions, each with three slots set (`u % 37`, `+7`, `+19` mod 40),
/// so the balance lines are scattered like a real book.
fn populated(ops: usize) -> StateStore {
    let mut st = StateStore::new(StoreConfig {
        base: BASE,
        positions: POSITIONS as usize,
        markets: 1,
        undo: UndoCapacity {
            ops,
            extras: ops / 4,
            rows: ops / 4,
        },
    });
    for a in 0..ROWS {
        st.push_market(MARKET, row(a, 0)).unwrap();
    }
    st.reserve_positions(MARKET, POSITIONS as usize).unwrap();
    for u in 0..POSITIONS {
        let p = st.intern(&key(u)).unwrap();
        for s in [u % 37, (u + 7) % ROWS as u32, (u + 19) % ROWS as u32] {
            st.set_supply(p, s as u16, 1_000_000 + u128::from(u))
                .unwrap();
            st.set_debt(p, s as u16, 500_000 + u128::from(u)).unwrap();
        }
    }
    st
}

/// One block of `n` mutations: a rotating mix of balance writes (the bulk),
/// extra writes and market-row updates over the populated book.
fn apply_block(st: &mut StateStore, block: u64, n: usize) {
    st.begin_block(block).unwrap();
    for i in 0..n {
        let p = PositionId((i as u32 * 7_919) % POSITIONS);
        let s = ((i * 31) % ROWS as usize) as u16;
        match i % 16 {
            0..=6 => st.set_supply(p, s, (i as u128) << 8).unwrap(),
            7..=13 => st.set_debt(p, s, (i as u128) << 4).unwrap(),
            14 => st
                .set_extra(p, liq_protocol::PositionExtraRepr::ZERO)
                .unwrap(),
            _ => st
                .set_market(
                    MarketSlot {
                        market: MARKET,
                        slot: s,
                    },
                    row(s, block as u32),
                )
                .unwrap(),
        }
    }
}

fn sweep(st: &StateStore) -> u128 {
    let view = st.view(1_700_000_000);
    let mut acc = 0u128;
    for id in 0..POSITIONS {
        let r = view.position(PositionId(id)).unwrap();
        for s in r.config.iter() {
            let s = usize::from(s);
            let m = &r.markets[s];
            acc = acc
                .wrapping_add(r.supply[s])
                .wrapping_add(r.debt[s])
                .wrapping_add(m.supply_index.raw())
                .wrapping_add(m.debt_rate.raw())
                .wrapping_add(u128::from(m.liq_threshold))
                .wrapping_add(u128::from(m.decimals));
        }
    }
    acc
}

fn bench(c: &mut Criterion) {
    let st = populated(1_024);
    c.bench_function("sweep_1k", |b| b.iter(|| black_box(sweep(black_box(&st)))));

    for &n in &[1_000usize, 10_000] {
        c.bench_function(&format!("unwind_128_x{}", n / 1_000 * 1_000), |b| {
            b.iter_batched(
                || {
                    let mut st = populated(n);
                    for k in 1..=128u64 {
                        apply_block(&mut st, BASE + k, n);
                    }
                    assert_eq!(st.undo_overflows(), 0);
                    st
                },
                |mut st| {
                    st.unwind_to(BASE).unwrap();
                    black_box(st)
                },
                BatchSize::LargeInput,
            );
        });
    }

    c.bench_function("apply_block_10k", |b| {
        b.iter_batched(
            || populated(10_000),
            |mut st| {
                apply_block(&mut st, BASE + 1, 10_000);
                black_box(st)
            },
            BatchSize::LargeInput,
        );
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
