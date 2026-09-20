//! CI regression gate vs the committed pre-tuning baseline.

use crate::bench::report::{invented_percentiles, PathVerdict, Report};

pub const BASELINE_TEXT: &str = include_str!("baseline.txt");

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateError {
    InventedPercentile(&'static str),
    PathPassWithoutUniverse,
    Regression {
        key: &'static str,
        baseline_ns: u64,
        current_ns: u64,
    },
    BaselineHasNumbers,
}

/// Permille of allowed p99.9 growth vs baseline when both are measured.
/// Default 0: any increase is a regression. `LIQ_BENCH_REGRESS_PERMILLE` overrides.
#[must_use]
pub(crate) fn regress_permille() -> u32 {
    std::env::var("LIQ_BENCH_REGRESS_PERMILLE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn parse_kv(text: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = t.split_once('=') {
            out.push((k, v));
        }
    }
    out
}

fn baseline_numeric_p99(text: &str) -> bool {
    parse_kv(text).into_iter().any(|(k, v)| {
        (k.ends_with(".p99_ns") || k.ends_with(".p99_9_ns") || k == "llc_first_touch")
            && v != "ABSENT"
            && v != "DEFERRED"
    })
}

fn baseline_ns(text: &str, key: &str) -> Option<u64> {
    parse_kv(text)
        .into_iter()
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.parse().ok())
}

fn allowed(baseline: u64, permille: u32) -> Option<u64> {
    let b = u128::from(baseline);
    let p = u128::from(permille);
    let extra = b.checked_mul(p)?.checked_div(1000)?;
    let cap = b.checked_add(extra)?;
    u64::try_from(cap).ok()
}

/// Compare a live [`Report`] to the committed baseline.
///
/// * Invented numbers (`n=0` with a p99) fail.
/// * `PASS` on Path A/B/C without a full universe fails.
/// * The committed baseline must itself contain no numeric p99 (this WP
///   records the pre-tuning ABSENT baseline; a numbered baseline is a later
///   measurement on the box).
/// * When both current and baseline have a number, current may not exceed
///   baseline × (1 + permille/1000).
pub fn check(current: &Report, baseline_text: &str) -> Result<(), GateError> {
    if invented_percentiles(current) {
        return Err(GateError::InventedPercentile(
            "numeric p99 with n=0 or Path PASS without full universe",
        ));
    }
    if current
        .paths
        .iter()
        .any(|p| matches!(p.verdict, PathVerdict::Pass) && !current.universe_full)
    {
        return Err(GateError::PathPassWithoutUniverse);
    }
    if baseline_numeric_p99(baseline_text) {
        return Err(GateError::BaselineHasNumbers);
    }
    let tol = regress_permille();
    for (i, name) in crate::bench::report::STAGES.iter().enumerate() {
        let Some(st) = current.stages.get(i) else {
            continue;
        };
        let Some(cur) = st.p99_9_ns else {
            continue;
        };
        let key_stage = *name;
        let key = match key_stage {
            "ingest" => "stage.ingest.p99_9_ns",
            "state" => "stage.state.p99_9_ns",
            "price" => "stage.price.p99_9_ns",
            "engine" => "stage.engine.p99_9_ns",
            "router" => "stage.router.p99_9_ns",
            "sim" => "stage.sim.p99_9_ns",
            "sign" => "stage.sign.p99_9_ns",
            _ => continue,
        };
        if let Some(base) = baseline_ns(baseline_text, key) {
            let cap = allowed(base, tol).unwrap_or(base);
            if cur > cap {
                return Err(GateError::Regression {
                    key,
                    baseline_ns: base,
                    current_ns: cur,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{check, BASELINE_TEXT};
    use crate::bench::report::invented_percentiles;

    #[test]
    fn committed_baseline_has_no_numeric_p99() {
        for line in BASELINE_TEXT.lines() {
            let t = line.trim();
            if t.starts_with('#') || t.is_empty() {
                continue;
            }
            let Some((k, v)) = t.split_once('=') else {
                continue;
            };
            if k.ends_with(".p50_ns") || k.ends_with(".p99_ns") || k.ends_with(".p99_9_ns") {
                assert_eq!(v, "ABSENT", "{k} must be ABSENT in the pre-tuning baseline");
            }
            if k.ends_with(".verdict") {
                assert_eq!(v, "UNMEASURED", "{k}");
            }
            if k == "llc_first_touch" {
                assert_eq!(v, "ABSENT");
            }
        }
    }

    #[test]
    fn live_run_passes_gate() {
        let r = crate::bench::run();
        assert!(!invented_percentiles(&r));
        check(&r, BASELINE_TEXT).expect("ABSENT vs ABSENT is not a regression");
    }
}
