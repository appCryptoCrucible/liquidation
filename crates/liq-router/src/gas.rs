//! Gas oracle (GUIDE 12 §6): exact next base fee and header gas limit.
//! Priority is not estimated. Quotes use [`PRIORITY_FEE_WEI`] (1 gwei).
//!
//! Next base fee is a thin call into [`crate::band::next_base_fee`] (WP
//! 12A-1). This module does not re-derive EIP-1559. The header gas limit
//! is stored from the parent header — never a compiled-in constant.

use alloy_primitives::U256;

/// Default ring length for observed effective priority fees. Construction
/// still takes an explicit cap; this is a documented starting size, not a
/// gas-limit stand-in. The ring is not read when building a fee quote.
pub const DEFAULT_PRIORITY_CAP: usize = 256;

/// Fixed priority fee, wei per gas. 1 gwei. Contested and modest paths
/// both use this. The percentile ring is not a quote input.
pub const PRIORITY_FEE_WEI: u128 = 1_000_000_000;

/// Fail-closed gas-oracle errors. Nothing is estimated in place of a
/// missing header or an empty sample window.
#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GasError {
    #[error("parent gas limit missing or below EIP-1559 elasticity")]
    BadHeader,
    #[error("header gas limit is required and must be read from the block")]
    MissingGasLimit,
    #[error("next base fee has not been observed this block")]
    MissingBaseFee,
    #[error("priority-fee window is empty; no percentile without samples")]
    EmptyPriorityWindow,
    #[error("priority percentile {0} is outside 1..=99")]
    BadPercentile(u8),
    #[error("priority ring capacity is zero")]
    ZeroCapacity,
}

/// Exact next-block base fee (wei / gas). Delegates to the 12A-1
/// implementation so the formula lives in one place.
#[inline]
#[must_use]
pub fn next_base_fee(
    parent_base_fee: u128,
    parent_gas_used: u64,
    parent_gas_limit: u64,
) -> Option<u128> {
    crate::band::next_base_fee(parent_base_fee, parent_gas_used, parent_gas_limit)
}

/// Header gas limit as a required input. `0` is not a valid header.
#[inline]
#[must_use]
pub fn header_gas_limit(parent_gas_limit: u64) -> Option<u64> {
    (parent_gas_limit > 0).then_some(parent_gas_limit)
}

/// Rolling priority-fee samples plus the exact next base fee and the
/// header gas limit for the block being bid into.
///
/// Base fee is overwritten on every [`GasOracle::observe_parent`] — it is
/// never reused across blocks (GUIDE 12 §6).
#[derive(Clone, Debug)]
pub struct GasOracle {
    next_base: Option<u128>,
    gas_limit: Option<u64>,
    pri: Vec<u64>,
    cap: usize,
    next: usize,
    len: usize,
}

impl GasOracle {
    /// `cap == 0` is refused: a zero-length window cannot produce a
    /// percentile and must not silently look like "no priority fee".
    #[must_use]
    pub fn with_priority_cap(cap: usize) -> Option<Self> {
        if cap == 0 {
            return None;
        }
        Some(Self {
            next_base: None,
            gas_limit: None,
            pri: Vec::with_capacity(cap),
            cap,
            next: 0,
            len: 0,
        })
    }

    /// Fold the parent header. `priority_samples` are this block's
    /// effective priority fees (`effectiveGasPrice − baseFee`); the
    /// caller extracts them (GUIDE 05). An empty sample slice is allowed
    /// (a block with no txs) — the ring is left as it was.
    pub fn observe_parent(
        &mut self,
        parent_base_fee: u128,
        parent_gas_used: u64,
        parent_gas_limit: u64,
        priority_samples: &[u64],
    ) -> Result<(), GasError> {
        let limit = header_gas_limit(parent_gas_limit).ok_or(GasError::MissingGasLimit)?;
        let next = next_base_fee(parent_base_fee, parent_gas_used, parent_gas_limit)
            .ok_or(GasError::BadHeader)?;
        self.next_base = Some(next);
        self.gas_limit = Some(limit);
        for &p in priority_samples {
            self.push_priority(p);
        }
        Ok(())
    }

    fn push_priority(&mut self, p: u64) {
        if self.pri.len() < self.cap {
            self.pri.push(p);
            self.len = self.pri.len();
            self.next = self.len.checked_rem(self.cap).unwrap_or(0);
            return;
        }
        if let Some(slot) = self.pri.get_mut(self.next) {
            *slot = p;
        }
        self.next = self
            .next
            .checked_add(1)
            .and_then(|n| n.checked_rem(self.cap))
            .unwrap_or(0);
        self.len = self.cap;
    }

    /// Exact next base fee, wei / gas. `None` before the first successful
    /// observe.
    #[inline]
    #[must_use]
    pub fn base_fee_wei(&self) -> Option<u128> {
        self.next_base
    }

    /// Gas limit from the last observed header. Never a constant.
    #[inline]
    #[must_use]
    pub fn block_gas_limit(&self) -> Option<u64> {
        self.gas_limit
    }

    /// Nearest-rank `pct`-th percentile of the ring, integer, no
    /// interpolation. `pct` in `1..=99`. Empty ring → error.
    pub fn priority_percentile(&self, pct: u8) -> Result<u64, GasError> {
        if !(1..=99).contains(&pct) {
            return Err(GasError::BadPercentile(pct));
        }
        if self.len == 0 {
            return Err(GasError::EmptyPriorityWindow);
        }
        let n = self.len;
        let mut tmp: Vec<u64> = self.pri.iter().copied().take(n).collect();
        tmp.sort_unstable();
        // ceil(n * pct / 100) − 1, in integer arithmetic.
        let num = n
            .checked_mul(usize::from(pct))
            .ok_or(GasError::BadPercentile(pct))?;
        let rank = num
            .checked_add(99)
            .and_then(|v| v.checked_div(100))
            .ok_or(GasError::BadPercentile(pct))?;
        let idx = rank.saturating_sub(1).min(n.saturating_sub(1));
        tmp.get(idx).copied().ok_or(GasError::EmptyPriorityWindow)
    }

    /// `gas_used × next_base_fee` in wei. Base fee only — priority is not
    /// folded into this number. Accounting uses [`Self::inclusion_cost_wei`].
    pub fn base_fee_cost_wei(&self, gas_used: u64) -> Result<U256, GasError> {
        let bf = self.base_fee_wei().ok_or(GasError::MissingBaseFee)?;
        U256::from(gas_used)
            .checked_mul(U256::from(bf))
            .ok_or(GasError::BadHeader)
    }

    /// `(next_base_fee + priority_wei) × gas_used`. The two fees stay
    /// separate inputs; the sum exists only as the accounting product.
    pub fn inclusion_cost_wei(&self, gas_used: u64, priority_wei: u128) -> Result<U256, GasError> {
        let bf = self.base_fee_wei().ok_or(GasError::MissingBaseFee)?;
        let per = bf.checked_add(priority_wei).ok_or(GasError::BadHeader)?;
        U256::from(gas_used)
            .checked_mul(U256::from(per))
            .ok_or(GasError::BadHeader)
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    /// Oracle: 12A-1 `band::next_base_fee` on the EIP-1559 identity and
    /// ±12.5 % bounds. `gas.rs` must not drift from that function.
    #[test]
    fn next_base_fee_is_the_12a1_function() {
        let cases: [(u128, u64, u64); 4] = [
            (1_000_000_000, 15_000_000, 30_000_000),
            (1_000_000_000, 30_000_000, 30_000_000),
            (1_000_000_000, 0, 30_000_000),
            (7, 1, 2),
        ];
        for (b, used, limit) in cases {
            assert_eq!(
                next_base_fee(b, used, limit),
                crate::band::next_base_fee(b, used, limit)
            );
        }
        assert_eq!(next_base_fee(1, 0, 1), None, "limit < elasticity");
        // Used = 2 × target → +12.5 % (max(1, floor(base·delta/target/8))).
        assert_eq!(
            next_base_fee(8_000, 30_000_000, 30_000_000),
            Some(8_000 + 8_000 / 8)
        );
    }

    /// Oracle: gas limit is the header value; `0` is refused so a missing
    /// header cannot become a hardcoded 30M.
    #[test]
    fn gas_limit_comes_from_the_header() {
        assert_eq!(header_gas_limit(60_000_000), Some(60_000_000));
        assert_eq!(header_gas_limit(0), None);
        let mut o = GasOracle::with_priority_cap(4).unwrap();
        assert!(o.observe_parent(1, 0, 0, &[]).is_err());
        o.observe_parent(1_000, 15_000_000, 45_000_000, &[])
            .unwrap();
        assert_eq!(o.block_gas_limit(), Some(45_000_000));
        assert_eq!(
            o.base_fee_wei(),
            next_base_fee(1_000, 15_000_000, 45_000_000)
        );
    }

    /// Independent implementation: sort a known window, nearest-rank.
    /// Window `[1, 10, 100]`, p50 → ceil(3·50/100)−1 = 1 → 10.
    #[test]
    fn priority_percentile_matches_nearest_rank() {
        let mut o = GasOracle::with_priority_cap(8).unwrap();
        o.observe_parent(1, 1, 30_000_000, &[1, 10, 100]).unwrap();
        assert_eq!(o.priority_percentile(50).unwrap(), 10);
        assert_eq!(o.priority_percentile(1).unwrap(), 1);
        assert_eq!(o.priority_percentile(99).unwrap(), 100);
        assert_eq!(o.priority_percentile(0), Err(GasError::BadPercentile(0)));
        assert_eq!(
            o.priority_percentile(100),
            Err(GasError::BadPercentile(100))
        );
        let empty = GasOracle::with_priority_cap(4).unwrap();
        assert_eq!(
            empty.priority_percentile(50),
            Err(GasError::EmptyPriorityWindow)
        );
        assert!(GasOracle::with_priority_cap(0).is_none());
    }

    /// Ring overwrites oldest; after cap=2 and samples 1,2,3 the window is
    /// `[3,2]` sorted `[2,3]`, p50 → 2.
    #[test]
    fn priority_ring_is_rolling() {
        let mut o = GasOracle::with_priority_cap(2).unwrap();
        o.observe_parent(1, 1, 30_000_000, &[1, 2, 3]).unwrap();
        assert_eq!(o.priority_percentile(50).unwrap(), 2);
        assert_eq!(o.priority_percentile(99).unwrap(), 3);
    }

    /// `gas × base` in wei; priority is not mixed in.
    #[test]
    fn base_fee_cost_excludes_priority() {
        let mut o = GasOracle::with_priority_cap(2).unwrap();
        o.observe_parent(30, 15_000_000, 30_000_000, &[5]).unwrap();
        assert_eq!(o.base_fee_cost_wei(10).unwrap(), U256::from(300u64));
    }

    /// Accounting is `(base + priority) × gas`. Priority is not stored on
    /// the oracle and is not added into `base_fee_wei`.
    #[test]
    fn inclusion_cost_is_base_plus_priority_times_gas() {
        let mut o = GasOracle::with_priority_cap(2).unwrap();
        o.observe_parent(30, 15_000_000, 30_000_000, &[5]).unwrap();
        assert_eq!(o.base_fee_wei().unwrap(), 30);
        assert_eq!(o.inclusion_cost_wei(10, 7).unwrap(), U256::from(370u64));
        assert_eq!(o.base_fee_cost_wei(10).unwrap(), U256::from(300u64));
    }
}
