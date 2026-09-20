//! WP 05C — recall harness, miss classifier, declines, timing (GUIDE 05 §3–§4).

mod classifier;
mod coverage;
mod report;
mod timing;

pub use classifier::{
    classify_decline, classify_miss, ClassifyError, DeclaredConfig, DeclineClass, DeclineReason,
    EventFields, ForkFacts, MissClass,
};
pub use coverage::{
    in_scope_gate, matrix_markdown, uncovered_matrix, CoverageCell, CoverageStatus, InScopeGate,
    SliceRate, MATRIX_HEADER, MATRIX_SEP, ROW_FAMILY, ROW_INSTANCE, ROW_N50, ROW_SLICE,
    ROW_TRIGGER, ROW_VOL,
};
pub use report::{
    build_report, event_from_actual, require_fork, Observed, RecallError, RecallReport,
};
pub use timing::{timing, DetectionHit, TimingError, TimingSample};

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests;
