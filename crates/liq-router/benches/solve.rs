//! GUIDE 12 acceptance: "allocation solve completes within the routing
//! budget at 6 pools (benchmark, not assumption)". Three V3 pools with
//! several initialized ticks in range plus three V2 pools, one pair, one
//! large exit that uses every pool. Also the same set with a Curve leg
//! (smooth path), and a K = 3 batch.

#![allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::hint::black_box;

use alloy_primitives::{Address, U256};
use criterion::{criterion_group, criterion_main, Criterion};
use liq_router::{
    solve_batch, solve_pair, CurveState, GasTerms, Pool, PoolBook, PoolState, SolveBudget, Tick,
    V2State, V3State,
};
use liq_types::AssetId;
use smallvec::SmallVec;
use uniswap_v3_math::tick_math;

const A0: AssetId = AssetId(0);
const A1: AssetId = AssetId(1);
const A2: AssetId = AssetId(2);
const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);
const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
const L: u128 = 1_000_000_000_000_000_000_000;

fn addr(n: u64) -> Address {
    let mut b = [0u8; 20];
    b[12..].copy_from_slice(&n.to_be_bytes());
    Address::from_slice(&b)
}

fn e18(n: u64) -> U256 {
    U256::from(n) * WAD
}

fn v3(
    n: u64,
    fee: u32,
    spacing: i32,
    positions: &[(i32, i32, u128)],
    assets: [AssetId; 2],
) -> Pool {
    let mut ticks: Vec<Tick> = Vec::new();
    let mut liquidity = 0u128;
    for &(lo, hi, l) in positions {
        for (t, sign) in [(lo, 1i128), (hi, -1i128)] {
            let net = sign * i128::try_from(l).unwrap();
            match ticks.iter_mut().find(|x| x.tick == t) {
                Some(x) => {
                    x.net += net;
                    x.gross += l;
                }
                None => ticks.push(Tick {
                    tick: t,
                    net,
                    gross: l,
                }),
            }
        }
        if lo <= 0 && 0 < hi {
            liquidity += l;
        }
    }
    ticks.sort_by_key(|t| t.tick);
    Pool {
        address: addr(n),
        assets: SmallVec::from_slice(&assets),
        tokens: SmallVec::from_slice(&[
            addr(1000 + u64::from(assets[0].0)),
            addr(1000 + u64::from(assets[1].0)),
        ]),
        hop_gas: 100_000,
        state: PoolState::V3(V3State {
            sqrt_price_x96: Q96,
            tick: tick_math::get_tick_at_sqrt_ratio(Q96).unwrap(),
            liquidity,
            fee_pips: fee,
            tick_spacing: spacing,
            ticks,
        }),
    }
}

fn v2(n: u64, r0: U256, r1: U256, assets: [AssetId; 2]) -> Pool {
    Pool {
        address: addr(n),
        assets: SmallVec::from_slice(&assets),
        tokens: SmallVec::from_slice(&[
            addr(1000 + u64::from(assets[0].0)),
            addr(1000 + u64::from(assets[1].0)),
        ]),
        hop_gas: 100_000,
        state: PoolState::V2(V2State {
            reserve0: r0,
            reserve1: r1,
        }),
    }
}

fn curve(n: u64, bal: U256) -> Pool {
    Pool {
        address: addr(n),
        assets: SmallVec::from_slice(&[A0, A1]),
        tokens: SmallVec::from_slice(&[addr(1000), addr(1001)]),
        hop_gas: 100_000,
        state: PoolState::Curve(CurveState {
            balances: SmallVec::from_slice(&[bal, bal]),
            rates: SmallVec::from_slice(&[WAD, WAD]),
            a: U256::from(200 * 100u64),
            a_precision: U256::from(100u64),
            fee: U256::from(4_000_000u64),
            stale: false,
        }),
    }
}

fn book(pools: Vec<Pool>) -> PoolBook {
    let mut assets = HashMap::new();
    for a in [A0, A1, A2] {
        assets.insert(addr(1000 + u64::from(a.0)), a);
    }
    let mut b = PoolBook::new(assets, None, 100_000);
    for p in pools {
        b.add(p).unwrap();
    }
    b
}

fn six() -> Vec<Pool> {
    vec![
        v3(
            1,
            3000,
            60,
            &[(-6000, 6000, L), (-1200, -600, L), (-3000, -1800, 2 * L)],
            [A0, A1],
        ),
        v3(
            2,
            500,
            10,
            &[(-2000, 2000, L / 2), (-500, 500, L), (-100, 100, 2 * L)],
            [A0, A1],
        ),
        v3(
            3,
            10_000,
            200,
            &[(-20_000, 20_000, 3 * L), (-4000, 0, L)],
            [A0, A1],
        ),
        v2(4, e18(2_000), e18(2_000), [A0, A1]),
        v2(5, e18(700), e18(690), [A0, A1]),
        v2(6, e18(5_000), e18(5_050), [A0, A1]),
    ]
}

fn bench(c: &mut Criterion) {
    let gas = GasTerms {
        base_fee_wei: 30_000_000_000,
        out_per_eth: e18(1),
    };
    let budget = SolveBudget::default();
    let bk = book(six());
    c.bench_function("solve_pair/6 pools (3 V3 + 3 V2), 3000e18", |b| {
        b.iter(|| solve_pair(black_box(&bk), A0, A1, e18(3_000), &gas, &budget).unwrap())
    });
    c.bench_function("solve_pair/6 pools, small 1e18", |b| {
        b.iter(|| solve_pair(black_box(&bk), A0, A1, e18(1), &gas, &budget).unwrap())
    });
    let mut with_curve = six();
    with_curve.pop();
    with_curve.push(curve(7, e18(2_000_000)));
    let bk_c = book(with_curve);
    c.bench_function(
        "solve_pair/6 pools incl. Curve (smooth path), 3000e18",
        |b| b.iter(|| solve_pair(black_box(&bk_c), A0, A1, e18(3_000), &gas, &budget).unwrap()),
    );
    let mut three = six();
    three.push(v2(8, e18(3_000), e18(3_000), [A2, A1]));
    let bk_b = book(three);
    let colls = [(A0, e18(500)), (A2, e18(100)), (A0, e18(200))];
    c.bench_function("solve_batch/K=3 (6 orderings), 7 pools", |b| {
        b.iter(|| solve_batch(black_box(&bk_b), &colls, A1, &gas, &budget).unwrap())
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
