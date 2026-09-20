//! Miss taxonomy (GUIDE 09 §2). Variants stay split; a blended win-rate is not a metric.

use alloy_primitives::{Address, Bytes, I256, U256};
use liq_types::{PositionKey, Ray};

/// Named sim-failure class copied from an emitted record. Not `liq-sim::SimError`
/// (D46: this crate must not depend on `liq-sim`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SimErrorClass {
    GuardRejected,
    InsufficientLiquidity,
    SlippageExceeded,
    ProfitBelowFloor,
    Revert,
    StateUnavailable,
    WorkerPanic,
    ArchiveUnavailable,
    Malformed,
    Bytecode,
}

impl SimErrorClass {
    pub fn parse(name: &str) -> Result<Self, crate::ObsError> {
        match name {
            "GuardRejected" => Ok(Self::GuardRejected),
            "InsufficientLiquidity" => Ok(Self::InsufficientLiquidity),
            "SlippageExceeded" => Ok(Self::SlippageExceeded),
            "ProfitBelowFloor" => Ok(Self::ProfitBelowFloor),
            "Revert" => Ok(Self::Revert),
            "StateUnavailable" => Ok(Self::StateUnavailable),
            "WorkerPanic" => Ok(Self::WorkerPanic),
            "ArchiveUnavailable" => Ok(Self::ArchiveUnavailable),
            "Malformed" => Ok(Self::Malformed),
            "Bytecode" => Ok(Self::Bytecode),
            _ => {
                tracing::error!(
                    name,
                    "unknown SimError class on emit; refusing taxonomy row"
                );
                Err(crate::ObsError::Emit("sim_error_class"))
            }
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GuardRejected => "GuardRejected",
            Self::InsufficientLiquidity => "InsufficientLiquidity",
            Self::SlippageExceeded => "SlippageExceeded",
            Self::ProfitBelowFloor => "ProfitBelowFloor",
            Self::Revert => "Revert",
            Self::StateUnavailable => "StateUnavailable",
            Self::WorkerPanic => "WorkerPanic",
            Self::ArchiveUnavailable => "ArchiveUnavailable",
            Self::Malformed => "Malformed",
            Self::Bytecode => "Bytecode",
        }
    }
}

/// Every tracked-protocol liquidation maps to exactly one of these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Won {
        realized_pnl: I256,
        bid: U256,
    },
    LostToCompetitor {
        winner: Address,
    },
    NotTracked {
        position: PositionKey,
    },
    HealthWrong {
        computed: Ray,
        actual: Ray,
    },
    Declined {
        reason: String,
    },
    DetectedLate {
        delta_ms: i64,
    },
    RejectedUnprofitable {
        est_net: I256,
        winner_bid: Option<U256>,
    },
    Outbid {
        our_bid: U256,
        winner_bid: U256,
    },
    WaitedTooLong {
        our_target_hf: Ray,
        actual_hf_at_fill: Ray,
    },
    SimFalseNegative {
        class: SimErrorClass,
    },
    RevertedOnChain {
        reason: Bytes,
    },
}

impl Outcome {
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Won { .. } => "Won",
            Self::LostToCompetitor { .. } => "LostToCompetitor",
            Self::NotTracked { .. } => "NotTracked",
            Self::HealthWrong { .. } => "HealthWrong",
            Self::Declined { .. } => "Declined",
            Self::DetectedLate { .. } => "DetectedLate",
            Self::RejectedUnprofitable { .. } => "RejectedUnprofitable",
            Self::Outbid { .. } => "Outbid",
            Self::WaitedTooLong { .. } => "WaitedTooLong",
            Self::SimFalseNegative { .. } => "SimFalseNegative",
            Self::RevertedOnChain { .. } => "RevertedOnChain",
        }
    }

    /// Phone alarm is `NotTracked` / `HealthWrong` only (GUIDE 09 §4a).
    #[must_use]
    pub const fn alarms(&self) -> bool {
        matches!(self, Self::NotTracked { .. } | Self::HealthWrong { .. })
    }

    /// Daily digest, not the phone (GUIDE 09 §4a).
    #[must_use]
    pub const fn to_digest(&self) -> bool {
        matches!(self, Self::Declined { .. })
    }
}
