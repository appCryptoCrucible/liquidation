//! The block a simulation runs in: its fork rules and header (GUIDE 11).
//!
//! A job targets the block after the store's tip, so it is simulated on the
//! tip's state with the next block's number and timestamp, under the rules
//! mainnet runs at that timestamp. Gas is not priced: base fee and gas price
//! are zero, as in the exec worker's `eth_simulateV1` check (validation
//! off). The Executor reads neither; what it charges for gas is the plan's
//! `gasCostWei`.

use alloy_primitives::B256;
use revm::context::BlockEnv;
use revm::primitives::hardfork::SpecId;

use crate::SimError;

/// A block by number and hash: the state a simulation starts from.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlockRef {
    pub number: u64,
    pub hash: B256,
}

/// Mainnet slot length. The next block is one slot after its parent.
pub const SLOT_SECONDS: u64 = 12;

/// EIP-7825 (Osaka): the most gas one transaction may carry.
pub const MAX_TX_GAS: u64 = 1 << 24;

// Mainnet activations, as `alloy-hardforks` 0.4.9 (`ethereum/mainnet.rs`),
// the table Reth 2.6 runs. BPO1 and BPO2 change blob parameters only.
const CANCUN_TIMESTAMP: u64 = 1_710_338_135;
const PRAGUE_TIMESTAMP: u64 = 1_746_612_311;
const OSAKA_TIMESTAMP: u64 = 1_764_798_551;

/// The EVM rules mainnet runs at `timestamp`. `None` before Cancun: the
/// Executor needs transient storage, and no simulation runs that far back.
/// A fork after Osaka needs a Reth upgrade first (the node would stop
/// following the chain), and this table with it.
#[must_use]
pub const fn mainnet_spec(timestamp: u64) -> Option<SpecId> {
    if timestamp >= OSAKA_TIMESTAMP {
        Some(SpecId::OSAKA)
    } else if timestamp >= PRAGUE_TIMESTAMP {
        Some(SpecId::PRAGUE)
    } else if timestamp >= CANCUN_TIMESTAMP {
        Some(SpecId::CANCUN)
    } else {
        None
    }
}

/// Fork rules and header of one simulated block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimEnv {
    pub spec: SpecId,
    pub block: BlockEnv,
}

impl SimEnv {
    #[must_use]
    pub const fn new(spec: SpecId, block: BlockEnv) -> Self {
        Self { spec, block }
    }
}

/// The block after `parent`: where a job built on `parent`'s state lands.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NextBlock {
    /// The state the simulation starts from: the store's tip.
    pub parent: BlockRef,
    /// The parent header's timestamp.
    pub parent_timestamp: u64,
    /// The parent header's gas limit; `0` = unknown. The next block's is
    /// within 1/1024 of it.
    pub parent_gas_limit: u64,
}

impl NextBlock {
    pub fn number(&self) -> Result<u64, SimError> {
        self.parent
            .number
            .checked_add(1)
            .ok_or(SimError::Malformed("block number overflow"))
    }

    pub fn timestamp(&self) -> Result<u64, SimError> {
        self.parent_timestamp
            .checked_add(SLOT_SECONDS)
            .ok_or(SimError::Malformed("block timestamp overflow"))
    }

    /// Header of the next block: number, timestamp and, when known, the
    /// parent's gas limit. The coinbase is the builder's and unknown here;
    /// it stays zero, and the Executor's bid transfer costs the same either
    /// way (EIP-3651 warms the coinbase).
    pub fn block_env(&self) -> Result<BlockEnv, SimError> {
        let mut block = crate::verify::block_env_at(self.number()?, self.timestamp()?);
        if self.parent_gas_limit != 0 {
            block.gas_limit = self.parent_gas_limit;
        }
        Ok(block)
    }

    /// The next block under the rules mainnet runs at its timestamp.
    pub fn mainnet_env(&self) -> Result<SimEnv, SimError> {
        let block = self.block_env()?;
        let spec =
            mainnet_spec(self.timestamp()?).ok_or(SimError::Malformed("block before Cancun"))?;
        Ok(SimEnv::new(spec, block))
    }

    /// The most gas one transaction in this block can carry: the EIP-7825
    /// cap, or the block's gas limit when that is lower.
    #[must_use]
    pub fn max_tx_gas(&self) -> u64 {
        match self.parent_gas_limit {
            0 => MAX_TX_GAS,
            limit => limit.min(MAX_TX_GAS),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;

    /// Oracle: `alloy-hardforks` 0.4.9 mainnet timestamps. The first second
    /// of each fork runs it; the second before runs the previous one.
    #[test]
    fn mainnet_spec_switches_at_each_activation() {
        assert_eq!(mainnet_spec(1_710_338_134), None);
        assert_eq!(mainnet_spec(1_710_338_135), Some(SpecId::CANCUN));
        assert_eq!(mainnet_spec(1_746_612_310), Some(SpecId::CANCUN));
        assert_eq!(mainnet_spec(1_746_612_311), Some(SpecId::PRAGUE));
        assert_eq!(mainnet_spec(1_764_798_550), Some(SpecId::PRAGUE));
        assert_eq!(mainnet_spec(1_764_798_551), Some(SpecId::OSAKA));
        // 2026-10: BPO2 is the last scheduled fork; the EVM is Osaka's.
        assert_eq!(mainnet_spec(1_790_000_000), Some(SpecId::OSAKA));
    }

    /// The next block is one slot later, numbered one higher, with the
    /// parent's gas limit; transactions in it carry at most 2^24 gas.
    #[test]
    fn next_block_is_one_slot_after_the_parent() {
        let at = NextBlock {
            parent: BlockRef {
                number: 23_000_000,
                hash: B256::repeat_byte(7),
            },
            parent_timestamp: 1_790_000_000,
            parent_gas_limit: 60_000_000,
        };
        let env = at.mainnet_env().unwrap();
        assert_eq!(env.spec, SpecId::OSAKA);
        assert_eq!(env.block.number, U256::from(23_000_001u64));
        assert_eq!(env.block.timestamp, U256::from(1_790_000_012u64));
        assert_eq!(env.block.gas_limit, 60_000_000);
        assert_eq!(env.block.basefee, 0);
        assert_eq!(at.max_tx_gas(), MAX_TX_GAS);

        let small = NextBlock {
            parent_gas_limit: 10_000_000,
            ..at
        };
        assert_eq!(small.max_tx_gas(), 10_000_000);
        let unknown = NextBlock {
            parent_gas_limit: 0,
            ..at
        };
        assert_eq!(unknown.max_tx_gas(), MAX_TX_GAS);
        assert_eq!(unknown.block_env().unwrap().gas_limit, u64::MAX);
    }

    /// Negative: a pre-Cancun parent is refused, not run under a guess.
    #[test]
    fn pre_cancun_block_is_refused() {
        let at = NextBlock {
            parent: BlockRef {
                number: 1,
                hash: B256::ZERO,
            },
            parent_timestamp: 1,
            parent_gas_limit: 0,
        };
        assert!(matches!(at.mainnet_env(), Err(SimError::Malformed(_))));
    }
}
