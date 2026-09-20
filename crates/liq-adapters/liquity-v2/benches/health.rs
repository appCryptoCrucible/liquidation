//! Liquity V2 health budget, WETH-branch two-slot trove.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    unreachable_pub
)]

#[path = "../tests/common/mod.rs"]
mod common;

use common::*;
use criterion::{criterion_group, criterion_main, Criterion};
use liq_protocol::{Constraints, Protocol};
use std::hint::black_box;

fn bench(c: &mut Criterion) {
    let d = Deploy::new();
    let p = d.adapter();
    let mut logs = listing_logs(&d);
    logs.extend(activity_logs(&d));
    let st = store_after(&p, &logs);
    let px = prices(ETH_USD_WAD, BOLD_USD_WAD);
    let pos = st.view(ALICE_ID, T0).unwrap();

    c.bench_function("health_liquity_v2_weth_branch", |b| {
        b.iter(|| black_box(p.health(black_box(pos), black_box(&px)).unwrap()))
    });
    c.bench_function("quote_liquity_v2_unbounded", |b| {
        b.iter(|| {
            black_box(
                p.quote(black_box(pos), black_box(&px), &Constraints::UNBOUNDED)
                    .unwrap(),
            )
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
