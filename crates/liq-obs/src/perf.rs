//! LLC / dTLB on the hot thread. Read **after** recompute. Linux `perf_event_open`;
//! other OS: fail closed (no zero-filled counters).

use crate::error::{ObsError, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheSample {
    /// First recompute after a block arrived.
    pub first_touch: CachePair,
    /// Subsequent recomputes in the same block window.
    pub steady: CachePair,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachePair {
    pub llc_load_misses: u64,
    pub dtlb_load_misses: u64,
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{CachePair, CacheSample};
    use crate::error::{ObsError, Result};
    use perf_event::events::{Cache, CacheOp, CacheResult, WhichCache};
    use perf_event::Builder;

    struct Pair {
        llc: perf_event::Counter,
        dtlb: perf_event::Counter,
    }

    impl Pair {
        fn open() -> Result<Self> {
            let llc = Builder::new()
                .kind(Cache {
                    which: WhichCache::LL,
                    operation: CacheOp::READ,
                    result: CacheResult::MISS,
                })
                .build()
                .map_err(|e| {
                    tracing::error!(error = %e, "perf_event_open LLC failed");
                    ObsError::PerfUnavailable {
                        os: "linux",
                        cause: e.to_string(),
                    }
                })?;
            let dtlb = Builder::new()
                .kind(Cache {
                    which: WhichCache::DTLB,
                    operation: CacheOp::READ,
                    result: CacheResult::MISS,
                })
                .build()
                .map_err(|e| {
                    tracing::error!(error = %e, "perf_event_open dTLB failed");
                    ObsError::PerfUnavailable {
                        os: "linux",
                        cause: e.to_string(),
                    }
                })?;
            Ok(Self { llc, dtlb })
        }

        fn read(&mut self) -> Result<CachePair> {
            let llc_load_misses = self.llc.read().map_err(|e| {
                tracing::error!(error = %e, "LLC counter read failed");
                ObsError::PerfUnavailable {
                    os: "linux",
                    cause: e.to_string(),
                }
            })?;
            let dtlb_load_misses = self.dtlb.read().map_err(|e| {
                tracing::error!(error = %e, "dTLB counter read failed");
                ObsError::PerfUnavailable {
                    os: "linux",
                    cause: e.to_string(),
                }
            })?;
            Ok(CachePair {
                llc_load_misses,
                dtlb_load_misses,
            })
        }

        fn enable(&mut self) -> Result<()> {
            self.llc.enable().map_err(|e| ObsError::PerfUnavailable {
                os: "linux",
                cause: e.to_string(),
            })?;
            self.dtlb.enable().map_err(|e| ObsError::PerfUnavailable {
                os: "linux",
                cause: e.to_string(),
            })?;
            Ok(())
        }

        fn disable(&mut self) -> Result<()> {
            self.llc.disable().map_err(|e| ObsError::PerfUnavailable {
                os: "linux",
                cause: e.to_string(),
            })?;
            self.dtlb.disable().map_err(|e| ObsError::PerfUnavailable {
                os: "linux",
                cause: e.to_string(),
            })?;
            Ok(())
        }
    }

    pub struct LinuxCounters {
        first: Pair,
        steady: Pair,
        in_first: bool,
    }

    impl LinuxCounters {
        pub fn open() -> Result<Self> {
            let mut first = Pair::open()?;
            let mut steady = Pair::open()?;
            first.enable()?;
            steady.enable()?;
            Ok(Self {
                first,
                steady,
                in_first: true,
            })
        }

        pub fn on_block_arrived(&mut self) {
            self.in_first = true;
        }

        /// Call only after the recompute returns (GUIDE 09 §5). Never between stages.
        pub fn read_after_recompute(&mut self) -> Result<CacheSample> {
            let first_touch = self.first.read()?;
            let steady = self.steady.read()?;
            if self.in_first {
                let _ = self.first.disable();
                self.in_first = false;
            }
            metrics::counter!("liq_llc_miss_first_touch").absolute(first_touch.llc_load_misses);
            metrics::counter!("liq_dtlb_miss_first_touch").absolute(first_touch.dtlb_load_misses);
            metrics::counter!("liq_llc_miss_steady").absolute(steady.llc_load_misses);
            metrics::counter!("liq_dtlb_miss_steady").absolute(steady.dtlb_load_misses);
            Ok(CacheSample {
                first_touch,
                steady,
            })
        }
    }
}

/// Open counters on this thread. Linux: `perf_event_open`. Else: error (no fake zeros).
pub struct HotPathCounters {
    #[cfg(target_os = "linux")]
    inner: linux::LinuxCounters,
}

impl HotPathCounters {
    pub fn open() -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            return Ok(Self {
                inner: linux::LinuxCounters::open()?,
            });
        }
        #[cfg(not(target_os = "linux"))]
        {
            tracing::error!("perf_event_open is Linux-only; this OS cannot collect LLC/dTLB");
            Err(ObsError::PerfUnavailable {
                os: std::env::consts::OS,
                cause: "perf_event_open not available".into(),
            })
        }
    }

    pub fn on_block_arrived(&mut self) {
        #[cfg(target_os = "linux")]
        self.inner.on_block_arrived();
        #[cfg(not(target_os = "linux"))]
        {}
    }

    pub fn read_after_recompute(&mut self) -> Result<CacheSample> {
        #[cfg(target_os = "linux")]
        {
            return self.inner.read_after_recompute();
        }
        #[cfg(not(target_os = "linux"))]
        {
            tracing::error!("read_after_recompute on non-Linux");
            Err(ObsError::PerfUnavailable {
                os: std::env::consts::OS,
                cause: "perf_event_open not available".into(),
            })
        }
    }
}

/// Share of `stage()` wall time vs empty loop, in permille of the `stage()` loop.
/// Not a production-box tracing-on vs tracing-off measurement (carry-forward).
#[must_use]
pub fn stage_emit_overhead_permille(iters: u32) -> Option<u64> {
    use liq_types::{stage, Stage, TraceId};
    if iters == 0 {
        tracing::error!("overhead iters == 0");
        return None;
    }
    let t = TraceId::from_raw(1);
    let start = std::time::Instant::now();
    for i in 0..iters {
        let _ = i;
    }
    let baseline = start.elapsed();
    let start = std::time::Instant::now();
    for _ in 0..iters {
        stage(t, Stage::PriceTick);
    }
    let with = start.elapsed();
    let b = baseline.as_nanos();
    let w = with.as_nanos();
    if w == 0 {
        tracing::error!("instrumentation measurement produced zero duration");
        return None;
    }
    let extra = w.saturating_sub(b);
    let permille = extra.checked_mul(1000)?.checked_div(w)?;
    u64::try_from(permille).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_fails_closed_off_linux() {
        let r = HotPathCounters::open();
        #[cfg(not(target_os = "linux"))]
        {
            assert!(r.is_err());
        }
        #[cfg(target_os = "linux")]
        {
            if let Err(e) = r {
                tracing::error!(error = %e, "perf_event_open denied on this Linux (paranoid/permissions)");
            }
        }
    }

    #[test]
    fn overhead_permille_measured() {
        let r = stage_emit_overhead_permille(50_000);
        assert!(r.is_some());
    }
}
