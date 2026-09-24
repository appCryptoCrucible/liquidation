//! GUIDE 08 acceptance budgets, measured.
//!
//! * `threshold_crossed/{10k,100k,1M}` — `ThresholdIndex::crossed` with a
//!   fixed `k = 16` hit range over 10⁴ / 10⁵ / 10⁶ registered thresholds:
//!   O(log N + k) means the three numbers are within a few ns of each other.
//! * `threshold_commit_1M` — one block's lazy re-registration: 256 fresh
//!   entries sorted into the delta and (every √N-th block) merged.
//! * `hot_warm_recompute_200` — an announced tick that sweeps 200 Hot+Warm
//!   V4 positions (real adapter `health`, real store, real prices) and
//!   crosses none: the "full exact recompute of Hot+Warm" budget (< 20 µs).
//! * `tick_to_candidate` — the same universe, ETH −5 %: ~70 positions
//!   cross, are quoted, checked for funding and queued. p99 printed
//!   (< 400 µs budget) alongside criterion's distribution.
//!
//! Fixtures: `tests/common/mod.rs` (chain observations at block 26018679;
//! synthetic V4 positions).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    unreachable_pub
)]

#[path = "../tests/common/mod.rs"]
mod common;

use std::hint::black_box;
use std::time::{Duration, Instant};

use alloy_primitives::U256;
use common::*;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use liq_engine::{Side, ThresholdIndex};
use liq_types::{AssetId, PositionId, Ray, SourceKind};

fn r(v: u64) -> Ray {
    Ray::from_raw(U256::from(v))
}

fn index_of(n: u32) -> ThresholdIndex {
    let mut idx = ThresholdIndex::new(1, n as usize);
    let a = AssetId(0);
    // Prices 1..=n on the falling side, one per position, registered in a
    // scrambled order (a multiplicative permutation of Z_n: the multiplier
    // is coprime to every 10^k) so the delta sort does real work.
    for i in 0..n {
        let id = PositionId(i);
        idx.begin(id);
        let price = (u64::from(i) * 2_654_435_761) % u64::from(n) + 1;
        idx.register(id, a, Side::Falling, r(price)).unwrap();
    }
    idx.commit();
    idx
}

fn threshold(c: &mut Criterion) {
    let mut g = c.benchmark_group("threshold_crossed");
    for (name, n) in [("10k", 10_000u32), ("100k", 100_000), ("1M", 1_000_000)] {
        let idx = index_of(n);
        let a = AssetId(0);
        // Old price at the middle, new 16 units lower: exactly 16 hits.
        let old = r(u64::from(n / 2));
        let new = r(u64::from(n / 2) - 15);
        assert_eq!(idx.crossed(a, old, new).count(), 16);
        g.throughput(Throughput::Elements(16));
        g.bench_with_input(BenchmarkId::from_parameter(name), &idx, |b, idx| {
            b.iter(|| black_box(idx.crossed(a, black_box(old), black_box(new)).count()))
        });
    }
    g.finish();

    let mut idx = index_of(1_000_000);
    let a = AssetId(0);
    let mut next = 0u32;
    c.bench_function("threshold_commit_1M", |b| {
        b.iter(|| {
            for _ in 0..256 {
                let id = PositionId(next % 1_000_000);
                idx.begin(id);
                idx.register(id, a, Side::Falling, r(u64::from(next % 1_000_000) + 1))
                    .unwrap();
                next = next.wrapping_add(1);
            }
            idx.commit();
        })
    });
}

/// 200 borrowers spread over Hot+Warm (hf 1.01–1.15 at the pinned prices)
/// plus 800 Cold ones (hf ≈ 2–4), 1 WETH each.
fn universe() -> Vec<Borrower> {
    borrowers(1_000, |i| {
        if i < 200 {
            1_793 + (u64::from(i) * 247) / 199
        } else {
            500 + (u64::from(i) * 500) / 999
        }
    })
}

fn engine(c: &mut Criterion) {
    let mut rig = Rig::new(&universe(), pinned_flash(), 1024);
    rig.resync(T0);
    let hot_warm = rig.engine.bands().members(liq_types::Band::Hot).len()
        + rig.engine.bands().members(liq_types::Band::Warm).len();
    assert!(
        (190..=210).contains(&hot_warm),
        "fixture: {hot_warm} Hot+Warm positions"
    );

    // ETH −0.1 %: nothing crosses, the sweep is exactly Hot+Warm.
    let small = rig.tick(
        WETH,
        ETH_USD_P8 - ETH_USD_P8 / 1_000,
        SourceKind::PendingPublic {
            tx: alloy_primitives::B256::ZERO,
            confidence: liq_types::Confidence::CERTAIN,
        },
    );
    rig.with_world(T0, |e, w| {
        e.on_price_tick(w, &small).unwrap();
        assert_eq!(e.queued(), 0, "no position crosses on −0.1 %");
    });
    c.bench_function("hot_warm_recompute_200", |b| {
        rig.with_world(T0, |e, w| {
            b.iter(|| {
                e.on_price_tick(w, black_box(&small)).unwrap();
            })
        })
    });

    // ETH −5 %: the upper part of the Hot+Warm band crosses and is quoted.
    let crash = rig.tick(
        WETH,
        ETH_USD_P8 - ETH_USD_P8 / 20,
        SourceKind::PendingPublic {
            tx: alloy_primitives::B256::ZERO,
            confidence: liq_types::Confidence::CERTAIN,
        },
    );
    let crossed = rig.with_world(T0, |e, w| {
        e.on_price_tick(w, &crash).unwrap();
        let n = e.queued();
        e.candidates().count();
        n
    });
    assert!(crossed > 40, "fixture: {crossed} candidates on −5 %");
    c.bench_function("tick_to_candidate", |b| {
        rig.with_world(T0, |e, w| {
            b.iter(|| {
                e.on_price_tick(w, black_box(&crash)).unwrap();
                black_box(e.candidates().count());
            })
        })
    });

    // Attribution: the adapter's own cost on the same positions, called
    // directly — what the engine's budget sits on top of.
    {
        use liq_protocol::Protocol;
        let view = rig.st.view(T0);
        let hot_warm_ids: Vec<PositionId> = {
            let mut v: Vec<PositionId> = rig
                .engine
                .bands()
                .members(liq_types::Band::Hot)
                .iter()
                .chain(rig.engine.bands().members(liq_types::Band::Warm))
                .copied()
                .collect();
            v.sort_unstable();
            v
        };
        let px_small = rig.px_with(WETH, ETH_USD_P8 - ETH_USD_P8 / 1_000);
        let px_crash = rig.px_with(WETH, ETH_USD_P8 - ETH_USD_P8 / 20);
        c.bench_function("adapter_health_only_200", |b| {
            b.iter(|| {
                for &id in &hot_warm_ids {
                    let pos = view.position(id).unwrap();
                    black_box(rig.p.health(pos, black_box(&px_small)).unwrap());
                }
            })
        });
        let crossing: Vec<PositionId> = hot_warm_ids
            .iter()
            .copied()
            .filter(|&id| {
                rig.p
                    .health(view.position(id).unwrap(), &px_crash)
                    .unwrap()
                    .hf
                    < Ray::ONE
            })
            .collect();
        assert_eq!(crossing.len(), crossed);
        c.bench_function("adapter_health_and_quote_crashed", |b| {
            b.iter(|| {
                for &id in &hot_warm_ids {
                    let pos = view.position(id).unwrap();
                    let h = rig.p.health(pos, black_box(&px_crash)).unwrap();
                    if h.hf < Ray::ONE {
                        black_box(rig.p.quote(pos, &px_crash).unwrap());
                    }
                }
            })
        });
    }

    // Percentiles the way GUIDE 08 states the budget.
    let mut samples: Vec<Duration> = Vec::with_capacity(5_000);
    rig.with_world(T0, |e, w| {
        for _ in 0..5_000 {
            let t = Instant::now();
            e.on_price_tick(w, &crash).unwrap();
            black_box(e.candidates().count());
            samples.push(t.elapsed());
        }
    });
    report(
        &format!("tick_to_candidate ({crossed} candidates / {hot_warm} Hot+Warm)"),
        samples,
    );
    let mut samples: Vec<Duration> = Vec::with_capacity(5_000);
    rig.with_world(T0, |e, w| {
        for _ in 0..5_000 {
            let t = Instant::now();
            e.on_price_tick(w, &small).unwrap();
            samples.push(t.elapsed());
        }
    });
    report("hot_warm_recompute_200", samples);
}

fn report(name: &str, mut samples: Vec<Duration>) {
    samples.sort_unstable();
    let pct = |p: usize| samples[samples.len() * p / 100];
    println!(
        "{name}: p50 {:?} p90 {:?} p99 {:?} max {:?}",
        pct(50),
        pct(90),
        pct(99),
        samples[samples.len() - 1]
    );
}

criterion_group!(benches, threshold, engine);
criterion_main!(benches);
