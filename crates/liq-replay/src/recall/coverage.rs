//! Per-coverage-row rates + STATE.md table format (GUIDE 05 §3 window).

pub const MATRIX_HEADER: &str = "| Row | Status | Evidence |";
pub const MATRIX_SEP: &str = "|---|---|---|";

pub const ROW_INSTANCE: &str = "Every enabled protocol instance";
pub const ROW_FAMILY: &str = "Every collateral family";
pub const ROW_TRIGGER: &str = "Every trigger class";
pub const ROW_VOL: &str = "Top-decile volatility block";
pub const ROW_N50: &str = "≥ 50 **in-scope** observations (D40)";
pub const ROW_SLICE: &str = "Per-row miss rate reported, no all-missed slice";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageCell {
    pub row: &'static str,
    pub status: CoverageStatus,
    pub evidence: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CoverageStatus {
    Uncovered,
    Archive,
    Live,
}

impl CoverageStatus {
    #[must_use]
    pub const fn as_md(self) -> &'static str {
        match self {
            Self::Uncovered => "`uncovered`",
            Self::Archive => "`archive`",
            Self::Live => "`live`",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SliceRate {
    pub key: String,
    pub in_scope: usize,
    pub in_scope_misses: usize,
}

impl SliceRate {
    #[must_use]
    pub fn all_missed(&self) -> bool {
        self.in_scope > 0 && self.in_scope_misses == self.in_scope
    }
}

/// Integer gate: pass iff `misses · 10 ≤ 3 · in_scope` (GUIDE 05 §3a). Floor n=50.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InScopeGate {
    Insufficient { in_scope: usize },
    Pass { misses: usize, in_scope: usize },
    Fail { misses: usize, in_scope: usize },
}

#[must_use]
pub fn in_scope_gate(misses: usize, in_scope: usize) -> InScopeGate {
    if in_scope < 50 {
        return InScopeGate::Insufficient { in_scope };
    }
    match misses.checked_mul(10).zip(in_scope.checked_mul(3)) {
        Some((lhs, rhs)) if lhs <= rhs => InScopeGate::Pass { misses, in_scope },
        Some(_) => InScopeGate::Fail { misses, in_scope },
        None => InScopeGate::Fail { misses, in_scope },
    }
}

/// Honest empty matrix — no fabricated observation counts.
#[must_use]
pub fn uncovered_matrix() -> [CoverageCell; 6] {
    [
        CoverageCell {
            row: ROW_INSTANCE,
            status: CoverageStatus::Uncovered,
            evidence: "*(instance → first observation block)*".into(),
        },
        CoverageCell {
            row: ROW_FAMILY,
            status: CoverageStatus::Uncovered,
            evidence: "*(family → block)*".into(),
        },
        CoverageCell {
            row: ROW_TRIGGER,
            status: CoverageStatus::Uncovered,
            evidence: "*(class → block)*".into(),
        },
        CoverageCell {
            row: ROW_VOL,
            status: CoverageStatus::Uncovered,
            evidence: "*(block, realized vol)* — uncomputable without A3 window; not guessed"
                .into(),
        },
        CoverageCell {
            row: ROW_N50,
            status: CoverageStatus::Uncovered,
            evidence: "*(n in-scope / 50)*".into(),
        },
        CoverageCell {
            row: ROW_SLICE,
            status: CoverageStatus::Uncovered,
            evidence: "*(row → misses/observations)*".into(),
        },
    ]
}

#[must_use]
pub fn matrix_markdown(cells: &[CoverageCell; 6]) -> String {
    let mut s = format!("{MATRIX_HEADER}\n{MATRIX_SEP}\n");
    for c in cells {
        s.push_str("| ");
        s.push_str(c.row);
        s.push_str(" | ");
        s.push_str(c.status.as_md());
        s.push_str(" | ");
        s.push_str(&c.evidence);
        s.push_str(" |\n");
    }
    s
}
