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
/// protocol path on-chain. Ids 0–8 are the 10E first-deploy set (D63). After
/// H3, a new ABI is `10R-n` + D55-A — do not invent a discriminant here.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ExecutorAdapter {
    AaveV3 = 0,
    AaveV4 = 1,
    MorphoBlue = 2,
    EulerV2 = 3,
    SiloV2 = 4,
    LiquityV2 = 5,
    Fluid = 6,
    Gearbox = 7,
    CompoundV2 = 8,
}

impl ExecutorAdapter {
    /// Inverse of the discriminant; `None` for a byte the Executor rejects
    /// (`PlanDecoder.UnknownAdapter`). Unknown after 10E is `9`, not `3`.
    #[inline]
    #[must_use]
    pub const fn from_wire(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::AaveV3),
            1 => Some(Self::AaveV4),
            2 => Some(Self::MorphoBlue),
            3 => Some(Self::EulerV2),
            4 => Some(Self::SiloV2),
            5 => Some(Self::LiquityV2),
            6 => Some(Self::Fluid),
            7 => Some(Self::Gearbox),
            8 => Some(Self::CompoundV2),
            _ => None,
        }
    }

    /// Bytes of adapter-specific tail that follow the 77 fixed leg bytes
    /// (PLAN-ENCODING §1b, `PlanDecoder.TAIL_*`). Length is a constant per
    /// adapter so the decoder can walk without a length prefix.
    #[inline]
    #[must_use]
    pub const fn tail_len(self) -> usize {
        match self {
            Self::AaveV3 => 0,
            Self::AaveV4 => 4,
            Self::MorphoBlue => 32,
            Self::EulerV2 => 32,
            Self::SiloV2 => 0,
            Self::LiquityV2 => 32,
            Self::Fluid => 32,
            Self::Gearbox => 32,
            Self::CompoundV2 => 21,
        }
    }
}

/// One liquidation leg — 77 fixed wire bytes (PLAN-ENCODING §1b) plus
/// [`ExecutorAdapter::tail_len`] adapter bytes the encoder (`liq-plan`)
/// appends. `Protocol::encode` does not produce the tail; router assembly
/// fills `LegTail` via `LegMeta` / `ValidateCtx` pins.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LiquidationLeg {
    pub adapter: ExecutorAdapter,
    /// Registry address the operator encoded: V3 Pool · V4 Spoke · Morpho
    /// singleton · Euler debt vault · Silo hook · Liquity TroveManager ·
    /// Fluid T1 vault · Gearbox CreditFacadeV3 · Compound debt cToken.
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

    /// Oracle: `Executor.sol` / `PlanDecoder.sol` `A_*` constants (10E).
    #[test]
    fn adapter_discriminants_match_executor_sol() {
        assert_eq!(ExecutorAdapter::AaveV3 as u8, 0);
        assert_eq!(ExecutorAdapter::AaveV4 as u8, 1);
        assert_eq!(ExecutorAdapter::MorphoBlue as u8, 2);
        assert_eq!(ExecutorAdapter::EulerV2 as u8, 3);
        assert_eq!(ExecutorAdapter::SiloV2 as u8, 4);
        assert_eq!(ExecutorAdapter::LiquityV2 as u8, 5);
        assert_eq!(ExecutorAdapter::Fluid as u8, 6);
        assert_eq!(ExecutorAdapter::Gearbox as u8, 7);
        assert_eq!(ExecutorAdapter::CompoundV2 as u8, 8);
    }

    /// Oracle: `PlanDecoder.sol` `TAIL_*`; `from_wire` inverts `as u8`.
    /// Unknown after 10E is id 9, not 3.
    #[test]
    fn wire_inverse_and_tail_lengths_match_plan_decoder_sol() {
        for a in [
            ExecutorAdapter::AaveV3,
            ExecutorAdapter::AaveV4,
            ExecutorAdapter::MorphoBlue,
            ExecutorAdapter::EulerV2,
            ExecutorAdapter::SiloV2,
            ExecutorAdapter::LiquityV2,
            ExecutorAdapter::Fluid,
            ExecutorAdapter::Gearbox,
            ExecutorAdapter::CompoundV2,
        ] {
            assert_eq!(ExecutorAdapter::from_wire(a as u8), Some(a));
        }
        assert_eq!(ExecutorAdapter::from_wire(9), None);
        assert_eq!(ExecutorAdapter::from_wire(u8::MAX), None);
        assert_eq!(ExecutorAdapter::AaveV3.tail_len(), 0);
        assert_eq!(ExecutorAdapter::AaveV4.tail_len(), 4);
        assert_eq!(ExecutorAdapter::MorphoBlue.tail_len(), 32);
        assert_eq!(ExecutorAdapter::EulerV2.tail_len(), 32);
        assert_eq!(ExecutorAdapter::SiloV2.tail_len(), 0);
        assert_eq!(ExecutorAdapter::LiquityV2.tail_len(), 32);
        assert_eq!(ExecutorAdapter::Fluid.tail_len(), 32);
        assert_eq!(ExecutorAdapter::Gearbox.tail_len(), 32);
        assert_eq!(ExecutorAdapter::CompoundV2.tail_len(), 21);
    }
}
