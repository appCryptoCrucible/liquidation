//! Tracing subscribers, miss taxonomy, watcher↔engine join, shadow JSONL, digest, perf.

#![deny(clippy::todo, clippy::unimplemented)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::float_arithmetic
    )
)]

pub mod alert;
pub mod digest;
pub mod error;
pub mod join;
pub mod outcome;
pub mod perf;
pub mod preflight;
pub mod recorder;
pub mod stage;

pub use alert::{AlarmSink, AlertConfig, DigestSink, HttpAlarm};
pub use digest::{digest_paths, DigestReport};
pub use error::{ObsError, Result};
pub use join::{parse_engine_emit, EngineEmit, EngineKind, WatchEngineJoin};
pub use outcome::{Outcome, SimErrorClass};
pub use perf::{stage_emit_overhead_permille, CachePair, CacheSample, HotPathCounters};
pub use preflight::{section4_report, PreflightSection4, PREFLIGHT_FIELDS};
pub use recorder::ShadowRecorder;
pub use stage::{all_stages, current_config_version, install_config_version, StageLayer};
