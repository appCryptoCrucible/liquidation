//! GUIDE 04 acceptance budget, measured: `health()` on a two-slot Aave V4
//! position (collateral + debt with accrued premium, projected 30 days past
//! the row's `last_update`) — budget p99 < 2 µs. Also `liquidation_price`
//! (one walk + one division) and `quote` (not hot-path; for reference).
//!
//! Fixtures: `tests/common/mod.rs` (published-rule provenance).

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
    let px = prices(1800_0000_0000, DAI_P8);
    let pos = st.view(ALICE_ID, T1).unwrap();

    c.bench_function("health_2slot_premium_30d", |b| {
        b.iter(|| black_box(p.health(black_box(pos), black_box(&px)).unwrap()))
    });
    c.bench_function("liquidation_price_weth", |b| {
        b.iter(|| {
            black_box(
                p.liquidation_price(black_box(pos), black_box(&px), WETH)
                    .unwrap(),
            )
        })
    });
    c.bench_function("quote_unbounded", |b| {
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
