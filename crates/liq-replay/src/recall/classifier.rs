//! Pure miss/decline classifier (GUIDE 05 §3a).
//!
//! Inputs: the liquidation event's own fields, **declared** config, and
//! fork-derived facts at block N−1. Never our position store, flash-source
//! cache, or router cache.
//!
//! TESTING.md §4 **mutation #13** (review item, no runtime test catches it):
//! pointing this module at our own position state instead of a fork call
//! silently invalidates the recall gate. The compile-time/grep test in
//! `tests.rs` is the design check.

use alloy_primitives::{Address, U256};
use liq_types::{AssetId, ProtocolId};
use thiserror::Error;

/// GUIDE 05 §3a — every `never_detected` entry is one of these.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MissClass {
    InScope,
    OutOfScopeProtocol,
    OutOfScopeAsset,
    BelowBand,
    AboveBand,
    NotFlashloanable,
    ExecutorUnprofitable,
    SelfOrKeeper,
}

/// GUIDE 05 §3 decline classes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeclineClass {
    Market,
    System,
    Known,
}

/// GUIDE 05 §3 — `OutsideViabilityBand` split into opposite-meaning edges.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeclineReason {
    DebtNotFlashloanable { exotic: bool },
    BelowBand,
    AboveBand,
    NoCollateralExit { registry_gap: bool },
    ProtocolNotEnabled,
}

impl DeclineReason {
    #[must_use]
    pub const fn class(self) -> DeclineClass {
        match self {
            Self::BelowBand => DeclineClass::Market,
            Self::AboveBand => DeclineClass::System,
            Self::ProtocolNotEnabled => DeclineClass::Known,
            Self::DebtNotFlashloanable { exotic } => {
                if exotic {
                    DeclineClass::Market
                } else {
                    DeclineClass::System
                }
            }
            Self::NoCollateralExit { registry_gap } => {
                if registry_gap {
                    DeclineClass::System
                } else {
                    DeclineClass::Market
                }
            }
        }
    }
}

/// Event fields only (GUIDE 05 §3a table).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct EventFields {
    pub protocol: ProtocolId,
    pub user: Address,
    pub liquidator: Address,
    pub repay_asset: AssetId,
    pub repay_amount: U256,
    pub seize_asset: AssetId,
    pub seize_amount: U256,
    pub block: u64,
    pub tx_index: u16,
}

/// Declared inputs — config, not computed state.
#[derive(Copy, Clone, Debug)]
pub struct DeclaredConfig<'a> {
    pub enabled_protocols: &'a [ProtocolId],
    pub target_assets: &'a [AssetId],
    pub keepers: &'a [Address],
    /// Declared: we have no venue listed for this pair (registry gap → SYSTEM).
    pub exit_venue_declared: bool,
    /// Declared: debt is treated as exotic (MARKET) vs index gap (SYSTEM).
    pub debt_is_exotic: bool,
}

/// Independent chain facts at N−1 / the liquidating tx receipt.
///
/// All fields required. Do not construct this with guessed values; if the
/// fork/receipt is unavailable, return [`ClassifyError::ForkUnavailable`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ForkFacts {
    pub flash_available_at_size: bool,
    pub exit_quote_exists: bool,
    /// Gas (base fee × declared gas units) exceeds bonus in ETH numeraire.
    pub gas_exceeds_bonus: bool,
    /// Convex impact / declared routing capacity: seize too large.
    pub size_exceeds_routing: bool,
    /// Executor realized economics in wei (receipt deltas − gas). `<= 0` → unprofitable.
    pub executor_realized_wei: i128,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ClassifyError {
    #[error("fork/receipt facts required at N-1 and are unavailable (A3/local node); refused, not guessed")]
    ForkUnavailable,
}

#[must_use]
fn protocol_enabled(ev: &EventFields, cfg: &DeclaredConfig<'_>) -> bool {
    cfg.enabled_protocols.contains(&ev.protocol)
}

#[must_use]
fn assets_in_target(ev: &EventFields, cfg: &DeclaredConfig<'_>) -> bool {
    cfg.target_assets.contains(&ev.repay_asset) && cfg.target_assets.contains(&ev.seize_asset)
}

#[must_use]
fn self_or_keeper(ev: &EventFields, cfg: &DeclaredConfig<'_>) -> bool {
    ev.liquidator == ev.user || cfg.keepers.contains(&ev.liquidator)
}

/// Miss class from event + config + fork. Fail closed if fork facts are missing
/// after config-only exclusions.
pub fn classify_miss(
    ev: &EventFields,
    cfg: &DeclaredConfig<'_>,
    fork: Option<ForkFacts>,
) -> Result<MissClass, ClassifyError> {
    if !protocol_enabled(ev, cfg) {
        return Ok(MissClass::OutOfScopeProtocol);
    }
    if !assets_in_target(ev, cfg) {
        return Ok(MissClass::OutOfScopeAsset);
    }
    if self_or_keeper(ev, cfg) {
        return Ok(MissClass::SelfOrKeeper);
    }
    let f = fork.ok_or(ClassifyError::ForkUnavailable)?;
    if f.executor_realized_wei <= 0 {
        return Ok(MissClass::ExecutorUnprofitable);
    }
    if f.gas_exceeds_bonus {
        return Ok(MissClass::BelowBand);
    }
    if f.size_exceeds_routing {
        return Ok(MissClass::AboveBand);
    }
    if !f.flash_available_at_size {
        return Ok(MissClass::NotFlashloanable);
    }
    if !f.exit_quote_exists {
        return Ok(MissClass::AboveBand);
    }
    Ok(MissClass::InScope)
}

/// `Ok(None)` = would pursue (not a decline). Fork required once protocol is enabled.
pub fn classify_decline(
    ev: &EventFields,
    cfg: &DeclaredConfig<'_>,
    fork: Option<ForkFacts>,
) -> Result<Option<DeclineReason>, ClassifyError> {
    if !protocol_enabled(ev, cfg) {
        return Ok(Some(DeclineReason::ProtocolNotEnabled));
    }
    let f = fork.ok_or(ClassifyError::ForkUnavailable)?;
    if !f.flash_available_at_size {
        return Ok(Some(DeclineReason::DebtNotFlashloanable {
            exotic: cfg.debt_is_exotic,
        }));
    }
    if f.gas_exceeds_bonus {
        return Ok(Some(DeclineReason::BelowBand));
    }
    if f.size_exceeds_routing {
        return Ok(Some(DeclineReason::AboveBand));
    }
    if !f.exit_quote_exists {
        return Ok(Some(DeclineReason::NoCollateralExit {
            registry_gap: !cfg.exit_venue_declared,
        }));
    }
    Ok(None)
}
