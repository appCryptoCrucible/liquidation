//! GUIDE 16 §6 benchmark harness: per-stage p50/p99/p99.9, PanicOnAlloc
//! seam, LLC first-touch, CI gate. Unmeasured budgets are ABSENT/DEFERRED.

mod gate;
mod hist;
mod llc;
mod pipeline;
mod report;
mod segment;

pub use gate::{check, GateError, BASELINE_TEXT};
pub use hist::{p99_9_reliable, percentile_nearest_rank, Percentiles, Samples};
pub use llc::{absent_no_recompute, LlcBaseline, LlcReason};
pub use pipeline::{readiness, sim_historical_status, Blocked, HotSamples};
pub use report::{
    default_paths, encode, invented_percentiles, path_verdict, AllocAssert, PathRow, PathVerdict,
    Report, StageRow, STAGES,
};
pub use segment::{
    a3_status, archive_is_empty, archive_root, pinned_range, probe_archive, workspace_root,
    A3Status, AdapterRoster, ArchiveProbe, ARCHIVE_REL, PATH_A_BUDGET_NS, PATH_B_BUDGET_NS,
    PATH_C_BUDGET_NS, PINNED_RANGE,
};

use crate::bench::llc::absent_no_recompute as llc_absent;
use crate::bench::pipeline::{process_segment, sim_historical_status as sim_hist};
use crate::bench::report::path_verdict as verdict;
use crate::bench::segment::AdapterRoster as Roster;
use liq_oracle::CanonicalBook;
use liq_protocol::Protocol;
use liq_router::{historical_profit_parity, ProfitError};

const UNMEASURED: &str =
    "unmeasured: PINNED_RANGE=None, A3Deferred, archive empty or unpinned, universe unloaded";

#[must_use]
fn router_parity_status() -> &'static str {
    match historical_profit_parity() {
        Err(ProfitError::ArchiveUnavailable) => "DEFERRED",
        Err(_) => "DEFERRED",
        Ok(()) => "measured",
    }
}

#[must_use]
fn alloc_status(blocks: u64) -> AllocAssert {
    #[cfg(feature = "alloc-assert")]
    {
        if blocks > 0 {
            AllocAssert::Pass
        } else {
            AllocAssert::Deferred
        }
    }
    #[cfg(not(feature = "alloc-assert"))]
    {
        let _ = blocks;
        AllocAssert::NotInstalled
    }
}

fn absent_stages() -> [StageRow; 7] {
    [
        StageRow::absent("ingest", UNMEASURED),
        StageRow::absent("state", UNMEASURED),
        StageRow::absent("price", UNMEASURED),
        StageRow::absent("engine", UNMEASURED),
        StageRow::absent("router", UNMEASURED),
        StageRow::absent("sim", UNMEASURED),
        StageRow::absent("sign", UNMEASURED),
    ]
}

/// Run with no adapters (today's process). Does not invent p99 numbers.
#[must_use]
pub fn run() -> Report {
    run_with(Roster::unloaded(), &[], None)
}

/// Same harness, with a caller-supplied roster and adapters (17A wiring).
#[must_use]
pub fn run_with(
    roster: AdapterRoster,
    protocols: &[&dyn Protocol],
    book: Option<&mut CanonicalBook>,
) -> Report {
    let root = archive_root();
    let probe = probe_archive(&root);
    let universe_full = roster.is_full();
    let a3 = a3_status();
    let pin = pinned_range();

    liq_bot::alloc::with_hot_path(|| ());

    let mut stages = absent_stages();
    let mut blocks = 0_u64;
    let mut paths = default_paths(universe_full);
    let mut llc = llc_absent();

    if let Ok((from, to)) = readiness(&probe, roster) {
        if let Ok(hot) = process_segment(&root, from, to, protocols, book) {
            blocks = hot.blocks;
            stages = [
                StageRow::from_samples("ingest", &hot.ingest, "pinned segment"),
                StageRow::from_samples("state", &hot.state, "pinned segment"),
                StageRow::from_samples("price", &hot.price, "pinned segment"),
                StageRow::from_samples("engine", &hot.engine, "pinned segment"),
                StageRow::from_samples("router", &hot.router, "DEFERRED: PoolBook/A3"),
                StageRow::from_samples("sim", &hot.sim, "DEFERRED: verify_historical"),
                StageRow::from_samples("sign", &hot.sign, "DEFERRED: 13A submitter"),
            ];
            llc = hot.llc;
            if let Some(px) = hot.path_a.percentiles() {
                if let Some(p) = paths.get_mut(0) {
                    p.p99_9_ns = Some(px.p99_9_ns);
                    p.verdict = verdict(
                        Some(px.p99_9_ns),
                        px.p99_9_reliable,
                        universe_full,
                        PATH_A_BUDGET_NS,
                    );
                }
            }
        }
    }

    Report {
        archive: probe,
        a3,
        universe_full,
        roster,
        pin,
        blocks,
        stages,
        paths,
        llc,
        alloc_assert: alloc_status(blocks),
        sim_historical: sim_hist(&root),
        router_parity: router_parity_status(),
        sign: "DEFERRED",
        tuning: "before",
    }
}

/// Write `LIQ_BENCH_REPORT` when set. Missing env is not an error.
pub fn maybe_write(r: &Report) -> Result<(), std::io::Error> {
    match std::env::var_os("LIQ_BENCH_REPORT") {
        None => Ok(()),
        Some(p) => std::fs::write(p, encode(r)),
    }
}

/// Gate the live report against the committed pre-tuning baseline.
pub fn gate_live() -> Result<(), GateError> {
    let r = run();
    check(&r, BASELINE_TEXT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_has_no_invented_p99() {
        let r = run();
        assert!(!invented_percentiles(&r));
        assert_eq!(r.blocks, 0);
        assert!(!r.universe_full);
        assert!(r.pin.is_none());
        for st in &r.stages {
            assert_eq!(st.n, 0);
            assert!(st.p50_ns.is_none());
            assert!(st.p99_ns.is_none());
            assert!(st.p99_9_ns.is_none());
        }
        for p in &r.paths {
            assert!(p.p99_9_ns.is_none());
            assert_eq!(p.verdict, PathVerdict::Unmeasured);
        }
        assert_eq!(r.sim_historical, "DEFERRED");
        assert_eq!(r.router_parity, "DEFERRED");
        assert_eq!(r.sign, "DEFERRED");
        match r.a3 {
            A3Status::Deferred => {}
            A3Status::Verified => panic!("A3 is not verified (D60)"),
        }
        match r.llc {
            LlcBaseline::Absent(LlcReason::NoRecompute) => {}
            other => panic!("LLC must be ABSENT/no_recompute, got {other:?}"),
        }
        assert_eq!(r.tuning, "before");
        gate_live().expect("ABSENT baseline vs ABSENT run");
    }

    #[test]
    fn encode_deterministic() {
        let a = encode(&run());
        let b = encode(&run());
        assert_eq!(a, b);
    }

    #[test]
    fn maybe_write_ok_without_env() {
        maybe_write(&run()).expect("no env");
    }

    #[test]
    fn with_hot_path_flag_around_empty_replay() {
        assert!(!liq_bot::alloc::hot_path_flag());
        let v = liq_bot::alloc::with_hot_path(|| {
            assert!(liq_bot::alloc::hot_path_flag());
            1_u8
        });
        assert_eq!(v, 1);
        assert!(!liq_bot::alloc::hot_path_flag());
    }

    #[cfg(feature = "alloc-assert")]
    #[test]
    #[allow(clippy::vec_init_then_push)]
    fn alloc_assert_installed_panics_on_vec() {
        let r = std::panic::catch_unwind(|| {
            liq_bot::alloc::with_hot_path(|| {
                let mut v = Vec::<u8>::new();
                v.push(1);
            });
        });
        assert!(r.is_err(), "PanicOnAlloc must be the process allocator");
    }
}
