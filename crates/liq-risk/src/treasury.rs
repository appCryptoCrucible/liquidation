//! Gas floor, sweep flag, max age, disposal (GUIDE 14 §5).

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use alloy_primitives::{Address, U256};
use tracing::warn;

/// In-plan `WETH.transfer` (F_SWEEP).
pub const TRANSFER_GAS_IN_PLAN: u64 = 30_000;
/// Standalone `sweep()`.
pub const TRANSFER_GAS_STANDALONE: u64 = 50_000;

#[derive(Clone, Debug)]
pub struct TreasuryConfig {
    pub gas_floor: HashMap<Address, U256>,
    /// k in `balance > k × transfer_gas × base_fee` (GUIDE: ≈ 20–50).
    pub sweep_k: u64,
    pub sweep_destination: Address,
    pub max_age: Duration,
    /// CEX disposal: multiple of (withdrawal fee + transfer gas).
    pub disposal_k: u64,
    pub disposal_max_age: Duration,
    pub disposal_venue_floor_wei: U256,
}

impl Default for TreasuryConfig {
    fn default() -> Self {
        Self {
            gas_floor: HashMap::new(),
            sweep_k: 20,
            sweep_destination: Address::ZERO,
            max_age: Duration::from_secs(7 * 24 * 3600),
            disposal_k: 20,
            disposal_max_age: Duration::from_secs(14 * 24 * 3600),
            disposal_venue_floor_wei: U256::ZERO,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Treasury {
    pub cfg: TreasuryConfig,
    last_sweep: Option<SystemTime>,
    last_disposal: Option<SystemTime>,
}

impl Treasury {
    #[must_use]
    pub fn new(cfg: TreasuryConfig) -> Self {
        Self {
            cfg,
            last_sweep: None,
            last_disposal: None,
        }
    }

    #[must_use]
    pub fn gas_floor(&self, key: Address) -> Option<U256> {
        self.cfg.gas_floor.get(&key).copied()
    }

    /// True iff the residual is worth the transfer or max-age elapsed.
    /// `base_fee` and balances are caller-supplied chain values — never guessed.
    #[must_use]
    pub fn sweep_flag(
        &self,
        balance_wei: U256,
        base_fee_wei: u64,
        in_plan: bool,
        now: SystemTime,
    ) -> bool {
        if self.cfg.sweep_destination.is_zero() {
            warn!("sweep_destination is zero; sweep flag stays false");
            return false;
        }
        let gas = if in_plan {
            TRANSFER_GAS_IN_PLAN
        } else {
            TRANSFER_GAS_STANDALONE
        };
        let cost = match U256::from(gas)
            .checked_mul(U256::from(base_fee_wei))
            .and_then(|c| c.checked_mul(U256::from(self.cfg.sweep_k)))
        {
            Some(c) => c,
            None => {
                warn!("sweep cost overflow; refusing sweep");
                return false;
            }
        };
        if balance_wei > cost {
            return true;
        }
        match self.last_sweep {
            Some(t) => match now.duration_since(t) {
                Ok(d) => d >= self.cfg.max_age && balance_wei > U256::ZERO,
                Err(_) => {
                    warn!("sweep clock went backwards");
                    false
                }
            },
            None => false,
        }
    }

    pub fn mark_swept(&mut self, at: SystemTime) {
        self.last_sweep = Some(at);
    }

    #[must_use]
    pub fn disposal_flag(
        &self,
        accumulated_eth_wei: U256,
        withdrawal_fee_wei: U256,
        transfer_gas_cost_wei: U256,
        now: SystemTime,
    ) -> bool {
        let unit = match withdrawal_fee_wei.checked_add(transfer_gas_cost_wei) {
            Some(u) => u,
            None => {
                warn!("disposal unit overflow");
                return false;
            }
        };
        let thresh = match unit.checked_mul(U256::from(self.cfg.disposal_k)) {
            Some(t) => t,
            None => {
                warn!("disposal threshold overflow");
                return false;
            }
        };
        if accumulated_eth_wei > thresh {
            return true;
        }
        match self.last_disposal {
            Some(t) => match now.duration_since(t) {
                Ok(d) => d >= self.cfg.disposal_max_age && accumulated_eth_wei > U256::ZERO,
                Err(_) => false,
            },
            None => false,
        }
    }

    pub fn mark_disposed(&mut self, at: SystemTime) {
        self.last_disposal = Some(at);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn sweep_when_balance_exceeds_k_times_gas() {
        let dest = Address::repeat_byte(0xAB);
        let t = Treasury::new(TreasuryConfig {
            sweep_k: 20,
            sweep_destination: dest,
            ..TreasuryConfig::default()
        });
        let base = 1_000_000_000u64;
        let cost = U256::from(TRANSFER_GAS_STANDALONE)
            .checked_mul(U256::from(base))
            .unwrap()
            .checked_mul(U256::from(20u64))
            .unwrap();
        assert!(!t.sweep_flag(cost, base, false, UNIX_EPOCH));
        assert!(t.sweep_flag(cost + U256::from(1u64), base, false, UNIX_EPOCH));
    }

    #[test]
    fn zero_destination_never_sweeps() {
        let t = Treasury::new(TreasuryConfig::default());
        assert!(!t.sweep_flag(U256::MAX, 1, true, UNIX_EPOCH));
    }

    #[test]
    fn max_age_sweeps_small_balance() {
        let dest = Address::repeat_byte(0xCD);
        let mut t = Treasury::new(TreasuryConfig {
            sweep_destination: dest,
            max_age: Duration::from_secs(10),
            ..TreasuryConfig::default()
        });
        t.mark_swept(UNIX_EPOCH);
        let later = UNIX_EPOCH + Duration::from_secs(11);
        assert!(t.sweep_flag(U256::from(1u64), 1, true, later));
    }
}
