//! Integer latency histograms. Empty input → `None`, never a fabricated 0.

/// Nearest-rank percentile. `permille` is 500 (p50), 990 (p99), 999 (p99.9).
///
/// Rank = max(1, ceil(n · permille / 1000)). Returns `None` when `sorted` is
/// empty or `permille > 1000`. Does not interpolate (interpolation would
/// invent a nanosecond that was never observed).
#[must_use]
pub fn percentile_nearest_rank(sorted: &[u64], permille: u32) -> Option<u64> {
    if sorted.is_empty() || permille > 1000 {
        return None;
    }
    let n = u128::try_from(sorted.len()).ok()?;
    let p = u128::from(permille);
    let prod = n.checked_mul(p)?;
    let rank = prod.div_ceil(1000).max(1);
    let idx = usize::try_from(rank.saturating_sub(1)).ok()?;
    sorted.get(idx).copied()
}

/// p99.9 is a distinct tail rank only when at least 1000 samples exist.
#[must_use]
pub const fn p99_9_reliable(n: usize) -> bool {
    n >= 1000
}

/// Per-stage sample buffer. Startup-reserved; `push` after that is the
/// measured path (a growth here is a harness bug, not a p99).
#[derive(Clone, Debug, Default)]
pub struct Samples {
    raw: Vec<u64>,
}

impl Samples {
    #[must_use]
    pub fn with_capacity(n: usize) -> Self {
        Self {
            raw: Vec::with_capacity(n),
        }
    }

    #[inline]
    pub fn push(&mut self, ns: u64) {
        self.raw.push(ns);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// Sorted copy for rank stats. `None` on empty — callers must not
    /// substitute zero.
    #[must_use]
    pub fn percentiles(&self) -> Option<Percentiles> {
        if self.raw.is_empty() {
            return None;
        }
        let mut v = self.raw.clone();
        v.sort_unstable();
        Some(Percentiles {
            n: v.len(),
            p50_ns: percentile_nearest_rank(&v, 500)?,
            p99_ns: percentile_nearest_rank(&v, 990)?,
            p99_9_ns: percentile_nearest_rank(&v, 999)?,
            p99_9_reliable: p99_9_reliable(v.len()),
        })
    }
}

/// Observed rank stats. Every field is a sample that existed in the buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Percentiles {
    pub n: usize,
    pub p50_ns: u64,
    pub p99_ns: u64,
    pub p99_9_ns: u64,
    pub p99_9_reliable: bool,
}

#[cfg(test)]
mod tests {
    use super::{p99_9_reliable, percentile_nearest_rank, Samples};

    #[test]
    fn empty_is_none_not_zero() {
        assert_eq!(percentile_nearest_rank(&[], 500), None);
        assert_eq!(percentile_nearest_rank(&[], 999), None);
        assert!(Samples::default().percentiles().is_none());
    }

    #[test]
    fn known_five_samples() {
        let s = [10_u64, 20, 30, 40, 50];
        assert_eq!(percentile_nearest_rank(&s, 500), Some(30));
        assert_eq!(percentile_nearest_rank(&s, 990), Some(50));
        assert_eq!(percentile_nearest_rank(&s, 999), Some(50));
        assert!(!p99_9_reliable(5));
    }

    #[test]
    fn thousand_samples_p99_9_is_rank_999() {
        let v: Vec<u64> = (1..=1000).collect();
        assert_eq!(percentile_nearest_rank(&v, 999), Some(999));
        assert!(p99_9_reliable(1000));
    }

    #[test]
    fn permille_over_1000_refused() {
        assert_eq!(percentile_nearest_rank(&[1], 1001), None);
    }
}
