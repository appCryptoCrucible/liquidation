//! PRE-FLIGHT.md §4 inputs from shadow + watcher files. Absent data is ABSENT, never guessed.

use std::path::Path;

use crate::digest::digest_paths;
use crate::error::{ObsError, Result};

pub const PREFLIGHT_FIELDS: [&str; 6] = [
    "expected_profit_per_opportunity_by_trigger",
    "modelled_win_rate_vs_F_beta",
    "liquidator_concentration_top3",
    "in_scope_miss_rate_and_coverage_matrix",
    "infrastructure_cost_per_month",
    "declined_system_breakdown",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightSection4 {
    pub fields: Vec<(String, String)>,
}

/// Produce the human-readable §4 table. Values are read from digest files when
/// present; otherwise the cell is `ABSENT` and this returns `Err` so H2 cannot
/// proceed on empty shadow (09B has not run; 05C recall wiring deferred).
pub fn section4_report(sqlite: &Path, outcomes_jsonl: &Path) -> Result<PreflightSection4> {
    let d = digest_paths(sqlite, outcomes_jsonl)?;
    let mut fields = Vec::new();
    // Profit / F(β) / concentration require 09B shadow + 12B fits. Do not invent them.
    if d.jsonl_rows == 0 && d.sqlite_rows == 0 {
        tracing::error!("PRE-FLIGHT §4 input not produced yet");
        return Err(ObsError::PreflightAbsent("no_shadow_or_watcher_rows"));
    }
    fields.push((
        PREFLIGHT_FIELDS[0].into(),
        "ABSENT: needs 09B shadow + 12A-2 profit model (not 09A)".into(),
    ));
    fields.push((
        PREFLIGHT_FIELDS[1].into(),
        "ABSENT: needs 12B F(β) fit on ≥200 contested (not 09A)".into(),
    ));
    fields.push((
        PREFLIGHT_FIELDS[2].into(),
        "ABSENT: needs watcher parquet liquidator histogram (W+C4)".into(),
    ));
    fields.push((
        PREFLIGHT_FIELDS[3].into(),
        format!(
            "watcher_sqlite_rows={} NotTracked={} HealthWrong={} (05C recall harness DEFERRED; in-scope miss rate not computed here)",
            d.sqlite_rows, d.not_tracked, d.health_wrong
        ),
    ));
    fields.push((
        PREFLIGHT_FIELDS[4].into(),
        "ABSENT: operator must supply monthly infra cost; 09A does not guess it".into(),
    ));
    fields.push((
        PREFLIGHT_FIELDS[5].into(),
        format!(
            "Declined={} (MARKET/SYSTEM/KNOWN split is GUIDE 05 Step 3 / 05C — DEFERRED)",
            d.declined
        ),
    ));
    Ok(PreflightSection4 { fields })
}

impl PreflightSection4 {
    #[must_use]
    pub fn render(&self) -> String {
        let mut s = String::from("# PRE-FLIGHT §4 inputs (09A)\n\n");
        s.push_str("Abandonment is manual (D13). These are the inputs, not a kill rule.\n\n");
        for (k, v) in &self.fields {
            s.push_str("- ");
            s.push_str(k);
            s.push_str(": ");
            s.push_str(v);
            s.push('\n');
        }
        s
    }
}
