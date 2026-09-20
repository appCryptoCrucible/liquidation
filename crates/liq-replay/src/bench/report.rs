//! Stage / path report. Unmeasured cells are ABSENT or DEFERRED, never 0.

use crate::bench::hist::{Percentiles, Samples};
use crate::bench::llc::{LlcBaseline, LlcReason};
use crate::bench::segment::{
    A3Status, AdapterRoster, ArchiveProbe, PATH_A_BUDGET_NS, PATH_B_BUDGET_NS, PATH_C_BUDGET_NS,
};

/// GUIDE 16 §6 stage names, in order.
pub const STAGES: [&str; 7] = [
    "ingest", "state", "price", "engine", "router", "sim", "sign",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocAssert {
    /// Default build: `with_hot_path` is linked, PanicOnAlloc is not installed.
    NotInstalled,
    /// `--features alloc-assert`, but no pinned loaded-universe replay ran.
    Deferred,
    /// Feature on and every hot-path block completed without a panic.
    Pass,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathVerdict {
    /// No p99.9 of a full-universe Path A/B/C run exists.
    Unmeasured,
    /// Measured, reliable p99.9, full universe, inside the §4b budget.
    Pass,
    /// Measured, reliable p99.9, full universe, over budget.
    OverBudget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageRow {
    pub name: &'static str,
    pub n: usize,
    pub p50_ns: Option<u64>,
    pub p99_ns: Option<u64>,
    pub p99_9_ns: Option<u64>,
    pub p99_9_reliable: bool,
    pub note: &'static str,
}

impl StageRow {
    #[must_use]
    pub fn absent(name: &'static str, note: &'static str) -> Self {
        Self {
            name,
            n: 0,
            p50_ns: None,
            p99_ns: None,
            p99_9_ns: None,
            p99_9_reliable: false,
            note,
        }
    }

    #[must_use]
    pub fn from_samples(name: &'static str, s: &Samples, note: &'static str) -> Self {
        match s.percentiles() {
            None => Self::absent(name, note),
            Some(Percentiles {
                n,
                p50_ns,
                p99_ns,
                p99_9_ns,
                p99_9_reliable,
            }) => Self {
                name,
                n,
                p50_ns: Some(p50_ns),
                p99_ns: Some(p99_ns),
                p99_9_ns: Some(p99_9_ns),
                p99_9_reliable,
                note,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathRow {
    pub id: &'static str,
    pub budget_ns: u64,
    pub p99_9_ns: Option<u64>,
    pub verdict: PathVerdict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub archive: ArchiveProbe,
    pub a3: A3Status,
    pub universe_full: bool,
    pub roster: AdapterRoster,
    pub pin: Option<(u64, u64)>,
    pub blocks: u64,
    pub stages: [StageRow; 7],
    pub paths: [PathRow; 3],
    pub llc: LlcBaseline,
    pub alloc_assert: AllocAssert,
    pub sim_historical: &'static str,
    pub router_parity: &'static str,
    pub sign: &'static str,
    pub tuning: &'static str,
}

#[must_use]
pub fn path_verdict(
    p99_9: Option<u64>,
    reliable: bool,
    universe_full: bool,
    budget_ns: u64,
) -> PathVerdict {
    match p99_9 {
        Some(ns) if reliable && universe_full => {
            if ns <= budget_ns {
                PathVerdict::Pass
            } else {
                PathVerdict::OverBudget
            }
        }
        _ => PathVerdict::Unmeasured,
    }
}

fn cell(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "ABSENT".into(),
    }
}

fn archive_tag(p: &ArchiveProbe) -> &'static str {
    match p {
        ArchiveProbe::Empty { .. } => "EMPTY",
        ArchiveProbe::PresentUnpinned { .. } => "PRESENT_UNPINNED",
    }
}

fn a3_tag(s: A3Status) -> &'static str {
    match s {
        A3Status::Deferred => "DEFERRED",
        A3Status::Verified => "VERIFIED",
    }
}

fn llc_tag(l: LlcBaseline) -> String {
    match l {
        LlcBaseline::Absent(LlcReason::NoRecompute) => "ABSENT".into(),
        LlcBaseline::Absent(LlcReason::PerfUnavailable) => "ABSENT".into(),
        LlcBaseline::Sampled {
            llc_load_misses, ..
        } => llc_load_misses.to_string(),
    }
}

fn llc_reason(l: LlcBaseline) -> &'static str {
    match l {
        LlcBaseline::Absent(LlcReason::NoRecompute) => "no_recompute",
        LlcBaseline::Absent(LlcReason::PerfUnavailable) => "perf_unavailable",
        LlcBaseline::Sampled { .. } => "sampled",
    }
}

fn alloc_tag(a: AllocAssert) -> &'static str {
    match a {
        AllocAssert::NotInstalled => "not_installed",
        AllocAssert::Deferred => "DEFERRED",
        AllocAssert::Pass => "PASS",
    }
}

fn verdict_tag(v: PathVerdict) -> &'static str {
    match v {
        PathVerdict::Unmeasured => "UNMEASURED",
        PathVerdict::Pass => "PASS",
        PathVerdict::OverBudget => "OVER_BUDGET",
    }
}

/// Deterministic text report. Numeric p99/p99.9 appear only from [`StageRow`]
/// options that came from real samples.
#[must_use]
pub fn encode(r: &Report) -> String {
    let mut s = String::from("# liq-bench 16C (GUIDE 16 §6). Unmeasured cells ABSENT/DEFERRED.\n");
    s.push_str("schema=16C.1\n");
    s.push_str("tuning=");
    s.push_str(r.tuning);
    s.push('\n');
    s.push_str("archive=");
    s.push_str(archive_tag(&r.archive));
    s.push('\n');
    s.push_str("a3=");
    s.push_str(a3_tag(r.a3));
    s.push('\n');
    s.push_str("universe_full=");
    s.push_str(if r.universe_full { "true" } else { "false" });
    s.push('\n');
    s.push_str("roster_constructed=");
    s.push_str(&r.roster.constructed.to_string());
    s.push('\n');
    s.push_str("roster_admitted=");
    s.push_str(&r.roster.admitted.to_string());
    s.push('\n');
    s.push_str("pin=");
    match r.pin {
        None => s.push_str("ABSENT\n"),
        Some((a, b)) => {
            s.push_str(&a.to_string());
            s.push('-');
            s.push_str(&b.to_string());
            s.push('\n');
        }
    }
    s.push_str("blocks=");
    s.push_str(&r.blocks.to_string());
    s.push('\n');
    for st in &r.stages {
        s.push_str("stage.");
        s.push_str(st.name);
        s.push_str(".n=");
        s.push_str(&st.n.to_string());
        s.push('\n');
        s.push_str("stage.");
        s.push_str(st.name);
        s.push_str(".p50_ns=");
        s.push_str(&cell(st.p50_ns));
        s.push('\n');
        s.push_str("stage.");
        s.push_str(st.name);
        s.push_str(".p99_ns=");
        s.push_str(&cell(st.p99_ns));
        s.push('\n');
        s.push_str("stage.");
        s.push_str(st.name);
        s.push_str(".p99_9_ns=");
        s.push_str(&cell(st.p99_9_ns));
        s.push('\n');
        s.push_str("stage.");
        s.push_str(st.name);
        s.push_str(".p99_9_reliable=");
        s.push_str(if st.p99_9_reliable { "true" } else { "false" });
        s.push('\n');
        s.push_str("stage.");
        s.push_str(st.name);
        s.push_str(".note=");
        s.push_str(st.note);
        s.push('\n');
    }
    for p in &r.paths {
        s.push_str("path.");
        s.push_str(p.id);
        s.push_str(".budget_ns=");
        s.push_str(&p.budget_ns.to_string());
        s.push('\n');
        s.push_str("path.");
        s.push_str(p.id);
        s.push_str(".p99_9_ns=");
        s.push_str(&cell(p.p99_9_ns));
        s.push('\n');
        s.push_str("path.");
        s.push_str(p.id);
        s.push_str(".verdict=");
        s.push_str(verdict_tag(p.verdict));
        s.push('\n');
    }
    s.push_str("llc_first_touch=");
    s.push_str(&llc_tag(r.llc));
    s.push('\n');
    s.push_str("llc_reason=");
    s.push_str(llc_reason(r.llc));
    s.push('\n');
    s.push_str("alloc_assert=");
    s.push_str(alloc_tag(r.alloc_assert));
    s.push('\n');
    s.push_str("sim_historical=");
    s.push_str(r.sim_historical);
    s.push('\n');
    s.push_str("router_parity=");
    s.push_str(r.router_parity);
    s.push('\n');
    s.push_str("sign=");
    s.push_str(r.sign);
    s.push('\n');
    s
}

/// True when a numeric percentile exists with `n == 0` (fabricated).
#[must_use]
pub fn invented_percentiles(r: &Report) -> bool {
    r.stages.iter().any(|st| {
        st.n == 0 && (st.p50_ns.is_some() || st.p99_ns.is_some() || st.p99_9_ns.is_some())
    }) || r
        .paths
        .iter()
        .any(|p| p.p99_9_ns.is_some() && matches!(p.verdict, PathVerdict::Pass) && !r.universe_full)
}

#[must_use]
pub fn default_paths(universe_full: bool) -> [PathRow; 3] {
    let mk = |id, budget| PathRow {
        id,
        budget_ns: budget,
        p99_9_ns: None,
        verdict: path_verdict(None, false, universe_full, budget),
    };
    [
        mk("A", PATH_A_BUDGET_NS),
        mk("B", PATH_B_BUDGET_NS),
        mk("C", PATH_C_BUDGET_NS),
    ]
}

#[cfg(test)]
mod tests {
    use super::{invented_percentiles, path_verdict, PathVerdict, StageRow};
    use crate::bench::segment::PATH_A_BUDGET_NS;

    #[test]
    fn absent_stage_has_no_numbers() {
        let s = StageRow::absent("ingest", "empty");
        assert_eq!(s.n, 0);
        assert!(s.p50_ns.is_none());
        assert!(s.p99_ns.is_none());
        assert!(s.p99_9_ns.is_none());
    }

    #[test]
    fn path_pass_requires_full_universe_and_reliable() {
        assert_eq!(
            path_verdict(Some(1), true, false, PATH_A_BUDGET_NS),
            PathVerdict::Unmeasured
        );
        assert_eq!(
            path_verdict(Some(1), false, true, PATH_A_BUDGET_NS),
            PathVerdict::Unmeasured
        );
        assert_eq!(
            path_verdict(Some(1), true, true, PATH_A_BUDGET_NS),
            PathVerdict::Pass
        );
        assert_eq!(
            path_verdict(
                Some(PATH_A_BUDGET_NS.saturating_add(1)),
                true,
                true,
                PATH_A_BUDGET_NS
            ),
            PathVerdict::OverBudget
        );
    }

    #[test]
    fn invented_flag_on_n0_with_p99() {
        let mut r = crate::bench::run();
        assert!(!invented_percentiles(&r));
        r.stages[0].p99_ns = Some(12);
        assert!(invented_percentiles(&r));
    }
}
