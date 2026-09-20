//! What `Protocol::encode` produces and what `Protocol::health_probe` asks.
//!
//! [`LiquidationPlan`] is one flash group head plus one liquidation leg in the
//! Executor's wire shape (PLAN-ENCODING §1b). `liq-plan` (WP 10B) assembles
//! many of these into a `BatchPlan` (D21 batching) and encodes the bytes; this
//! crate only fixes the fields an adapter must supply.

use alloy_primitives::{Address, Bytes};
use liq_types::{FlashProvider, Ray};

use crate::error::Result;

/// On-chain adapter id, byte 0 of a liquidation leg. Discriminants **are**
/// `Executor.sol`'s `A_*` constants; a drift here liquidates through the wrong
/// protocol path on-chain. New ids arrive only with a redeploy WP (D48).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ExecutorAdapter {
    AaveV3 = 0,
    AaveV4 = 1,
    /// D48 first deploy; `Executor.sol` `A_MORPHO = 2`.
    MorphoBlue = 2,
}

impl ExecutorAdapter {
    /// Inverse of the discriminant; `None` for a byte the Executor rejects
    /// (`PlanDecoder.UnknownAdapter`).
    #[inline]
    #[must_use]
    pub const fn from_wire(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::AaveV3),
            1 => Some(Self::AaveV4),
            2 => Some(Self::MorphoBlue),
            _ => None,
        }
    }

    /// Bytes of adapter-specific tail that follow the 77 fixed leg bytes
    /// (PLAN-ENCODING §1b, `PlanDecoder.TAIL_*`). V3 addresses reserves by
    /// underlying and needs none; V4 `liquidationCall` takes
    /// `(collateralReserveId, debtReserveId)` as two `u16`; Morpho
    /// `liquidate` takes `MarketParams`, recovered on-chain from the 32-byte
    /// market `Id`. Neither fits the fixed bytes.
    #[inline]
    #[must_use]
    pub const fn tail_len(self) -> usize {
        match self {
            Self::AaveV3 => 0,
            Self::AaveV4 => 4,
            Self::MorphoBlue => 32,
        }
    }
}

/// One liquidation leg — 77 fixed wire bytes (PLAN-ENCODING §1b) plus
/// [`ExecutorAdapter::tail_len`] adapter bytes the encoder (`liq-plan`)
/// appends: V4 reserve ids from the adapter config, Morpho's market `Id`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LiquidationLeg {
    pub adapter: ExecutorAdapter,
    /// Aave `Pool` · V4 `Spoke` · Morpho singleton (the market itself is the
    /// `Id` in the leg tail; `liquidate` is called on the singleton).
    pub market: Address,
    pub borrower: Address,
    pub collateral_asset: Address,
    /// What we ask the protocol to take, raw units. `u128` because that is
    /// the wire width; the adapter converts with `try_from` and fails on
    /// overflow rather than truncating (`ProtocolError::AmountTooLarge`).
    pub repay_amount: u128,
}

/// One flash group with a single leg (PLAN-ENCODING §1b: a single position is
/// a group with `liqCount == 1`; `liq-plan` merges groups sharing a debt
/// asset and provider).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LiquidationPlan {
    pub provider: FlashProvider,
    pub flash_source: Address,
    pub debt_asset: Address,
    /// `>=` the leg's repay; the surplus round-trips (over-borrow).
    pub flash_amount: u128,
    pub leg: LiquidationLeg,
}

/// Ground-truth call the drift detector (GUIDE 02 §8) issues off-thread and
/// compares to `health()`. `data` allocates — this never runs on the hot path
/// (RUST-CONVENTIONS §5.2 classifies probe calls as async). No `PartialEq`:
/// function-pointer equality is not meaningful.
#[derive(Clone, Debug)]
pub struct ProbeCall {
    /// Contract to `eth_call`.
    pub to: Address,
    /// ABI-encoded calldata.
    pub data: Bytes,
    /// Decodes the raw return into the normalised health factor (RAY, `1.0`
    /// = boundary). A plain `fn` so the call is `Copy`-cheap and needs no
    /// adapter reference at decode time.
    pub decode: fn(&[u8]) -> Result<Ray>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::ExecutorAdapter;

    /// Oracle: an independent implementation — `Executor.sol` constants
    /// `A_AAVE_V3 = 0`, `A_AAVE_V4 = 1`, `A_MORPHO = 2`.
    #[test]
    fn adapter_discriminants_match_executor_sol() {
        assert_eq!(ExecutorAdapter::AaveV3 as u8, 0);
        assert_eq!(ExecutorAdapter::AaveV4 as u8, 1);
        assert_eq!(ExecutorAdapter::MorphoBlue as u8, 2);
    }

    /// Oracle: `contracts/src/lib/PlanDecoder.sol` `TAIL_AAVE_V3 = 0`,
    /// `TAIL_AAVE_V4 = 4`, `TAIL_MORPHO = 32`; `from_wire` inverts `as u8`.
    #[test]
    fn wire_inverse_and_tail_lengths_match_plan_decoder_sol() {
        for a in [
            ExecutorAdapter::AaveV3,
            ExecutorAdapter::AaveV4,
            ExecutorAdapter::MorphoBlue,
        ] {
            assert_eq!(ExecutorAdapter::from_wire(a as u8), Some(a));
        }
        assert_eq!(ExecutorAdapter::from_wire(3), None);
        assert_eq!(ExecutorAdapter::from_wire(u8::MAX), None);
        assert_eq!(ExecutorAdapter::AaveV3.tail_len(), 0);
        assert_eq!(ExecutorAdapter::AaveV4.tail_len(), 4);
        assert_eq!(ExecutorAdapter::MorphoBlue.tail_len(), 32);
    }
}
