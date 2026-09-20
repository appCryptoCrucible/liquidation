//! LLC first-touch. Sampled only after a real recompute; never zero-filled.

use liq_obs::{HotPathCounters, ObsError};

/// Why first-touch was not recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LlcReason {
    /// No engine recompute ran (archive empty / universe unloaded / unpinned).
    NoRecompute,
    /// `perf_event_open` failed (non-Linux, paranoid, missing cap).
    PerfUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LlcBaseline {
    Absent(LlcReason),
    /// Counters read after the first recompute of a loaded-universe block.
    Sampled {
        llc_load_misses: u64,
        dtlb_load_misses: u64,
    },
}

/// Do not open counters unless a recompute will actually run.
pub(crate) fn begin(will_recompute: bool) -> Result<HotPathCounters, LlcReason> {
    if !will_recompute {
        return Err(LlcReason::NoRecompute);
    }
    HotPathCounters::open().map_err(|e| match e {
        ObsError::PerfUnavailable { .. } => LlcReason::PerfUnavailable,
        _ => LlcReason::PerfUnavailable,
    })
}

/// Read after the recompute returns (GUIDE 09 §5). Never between stages.
#[must_use]
pub(crate) fn finish(c: &mut HotPathCounters) -> LlcBaseline {
    match c.read_after_recompute() {
        Ok(s) => LlcBaseline::Sampled {
            llc_load_misses: s.first_touch.llc_load_misses,
            dtlb_load_misses: s.first_touch.dtlb_load_misses,
        },
        Err(_) => LlcBaseline::Absent(LlcReason::PerfUnavailable),
    }
}

#[must_use]
pub fn absent_no_recompute() -> LlcBaseline {
    LlcBaseline::Absent(LlcReason::NoRecompute)
}

#[cfg(test)]
mod tests {
    use super::{absent_no_recompute, begin, LlcBaseline, LlcReason};

    #[test]
    fn no_recompute_does_not_open() {
        match begin(false) {
            Err(LlcReason::NoRecompute) => {}
            Ok(_) => panic!("opened counters without a recompute"),
            Err(e) => panic!("expected NoRecompute, got {e:?}"),
        }
        match absent_no_recompute() {
            LlcBaseline::Absent(LlcReason::NoRecompute) => {}
            other => panic!("{other:?}"),
        }
    }
}
