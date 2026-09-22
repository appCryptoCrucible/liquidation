//! Fee fields from the 12A-2 gas-oracle pattern (GUIDE 13 §4b).
//!
//! `maxFeePerGas = ceil(next_base_fee × (9/8)^k)` for inclusion span `k`.
//! Not a flat 12.5 %. `maxPriorityFeePerGas` is the caller's priority,
//! which the quote sets to 1 gwei. It is not added into `maxFeePerGas`,
//! and a previous block's base fee is never reused.

use crate::error::{ExecError, Result};
use alloy_primitives::U256;

/// Per-block fee snapshot. `parent_block + 1` must equal the target block
/// or the quote is refused (never reuse a previous block's fee).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FeeQuote {
    pub parent_block: u64,
    /// Exact next-block base fee (wei / gas) from the 12A-2 / 12A-1 identity.
    pub next_base_fee: u128,
    /// Priority (wei / gas). The quote sets this to 1 gwei. Required nonzero.
    pub priority_wei: u128,
    /// Uncontested (InterestDrift / Stale) priority. Same 1 gwei. Required nonzero.
    pub modest_priority_wei: u128,
}

/// Fees bound to a concrete inclusion window.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BoundFees {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub k: u64,
}

/// `ceil(next_base_fee × 9^k / 8^k)` in integer arithmetic.
///
/// `k = 0` → exact next base fee. `k = 1` → +12.5 % rounded up.
/// `k = 2` → ×1.265625 rounded up. `k = 3` → ×1.423828125 rounded up.
pub fn max_fee_per_gas(next_base_fee: u128, k: u64) -> Result<u128> {
    if next_base_fee == 0 {
        return Err(ExecError::MissingBaseFee);
    }
    if k == 0 {
        return Ok(next_base_fee);
    }
    let mut num = U256::from(next_base_fee);
    let mut den = U256::from(1u64);
    for _ in 0..k {
        num = num
            .checked_mul(U256::from(9u64))
            .ok_or(ExecError::FeeOverflow)?;
        den = den
            .checked_mul(U256::from(8u64))
            .ok_or(ExecError::FeeOverflow)?;
    }
    let bump = den
        .checked_sub(U256::from(1u64))
        .ok_or(ExecError::FeeOverflow)?;
    let ceil = num
        .checked_add(bump)
        .ok_or(ExecError::FeeOverflow)?
        .checked_div(den)
        .ok_or(ExecError::FeeOverflow)?;
    u128::try_from(ceil).map_err(|_| ExecError::FeeOverflow)
}

impl FeeQuote {
    /// Bind this quote to `[target_block, max_block]`.
    ///
    /// `k = max_block - target_block`. Parent must be `target_block - 1`.
    pub fn bind(self, target_block: u64, max_block: u64) -> Result<BoundFees> {
        if max_block < target_block {
            return Err(ExecError::InvertedSpan {
                target: target_block,
                max: max_block,
            });
        }
        let expected_target = self
            .parent_block
            .checked_add(1)
            .ok_or(ExecError::FeeOverflow)?;
        if expected_target != target_block {
            return Err(ExecError::StaleFee {
                parent: self.parent_block,
                target: target_block,
            });
        }
        if self.next_base_fee == 0 {
            return Err(ExecError::MissingBaseFee);
        }
        if self.priority_wei == 0 {
            return Err(ExecError::MissingPriority);
        }
        if self.modest_priority_wei == 0 {
            return Err(ExecError::MissingModestPriority);
        }
        let k = max_block
            .checked_sub(target_block)
            .ok_or(ExecError::FeeOverflow)?;
        let max_fee_per_gas = max_fee_per_gas(self.next_base_fee, k)?;
        Ok(BoundFees {
            max_fee_per_gas,
            max_priority_fee_per_gas: self.priority_wei,
            k,
        })
    }

    /// Priority used on the wire for this trigger.
    #[must_use]
    pub fn priority_for(self, modest: bool) -> u128 {
        if modest {
            self.modest_priority_wei
        } else {
            self.priority_wei
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    #[test]
    fn compound_not_flat_125() {
        let base = 1_000u128;
        assert_eq!(max_fee_per_gas(base, 0).unwrap(), 1_000);
        // 1000 × 9/8 = 1125 exact.
        assert_eq!(max_fee_per_gas(base, 1).unwrap(), 1_125);
        // ceil(1000 × 81/64) = ceil(1265.625) = 1266 — not 1125, not 1000+12.5%×2.
        assert_eq!(max_fee_per_gas(base, 2).unwrap(), 1_266);
        // ceil(1000 × 729/512) = ceil(1423.828125) = 1424.
        assert_eq!(max_fee_per_gas(base, 3).unwrap(), 1_424);
        assert!(max_fee_per_gas(0, 1).is_err());
    }

    #[test]
    fn bind_refuses_stale_parent() {
        let q = FeeQuote {
            parent_block: 100,
            next_base_fee: 1_000,
            priority_wei: 1,
            modest_priority_wei: 1,
        };
        assert!(matches!(
            q.bind(102, 104),
            Err(ExecError::StaleFee {
                parent: 100,
                target: 102
            })
        ));
        let b = q.bind(101, 103).unwrap();
        assert_eq!(b.k, 2);
        assert_eq!(b.max_fee_per_gas, 1_266);
        assert_eq!(b.max_priority_fee_per_gas, 1);
    }
}
