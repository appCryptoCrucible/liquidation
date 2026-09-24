//! RiskGate, halt matrix, EIP-1967 watcher, caps, treasury, SQLite PnL ledger (WP 14A).
//!
//! Depends on `liq-types`, `liq-flash`, `liq-state` only. Sim/exec outcomes enter
//! through [`RiskGate::observe_sim_chain`] (liq-types event seam for 11/13).
//! 09A alarm forwarding is a carry-forward: Class C / Class B already log.

#![deny(clippy::todo, clippy::unimplemented)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use tracing::warn;

pub mod caps;
pub mod gate;
pub mod ledger;
pub mod matrix;
pub mod proxy;
pub mod treasury;

pub use caps::{CapConfig, CapError, CapGuard, Caps};
pub use gate::{ActionRequired, Allow, AllowQuery, RiskGate};
pub use ledger::{LedgerError, LiquidationRow, OutcomeRow, PnlLedger, RECONCILE_MAX_BPS};
pub use matrix::{class_of, HaltClass, MatrixRow, MATRIX};
pub use proxy::{
    assert_watch_covers_config, implementation_address, ProxyWatcher, SlotError, SlotReader,
    WatchKind, WatchTarget, IMPLEMENTATION_SLOT,
};
pub use treasury::{Treasury, TreasuryConfig, TRANSFER_GAS_IN_PLAN, TRANSFER_GAS_STANDALONE};

/// 02B `DriftDetector` talks to [`liq_types::HaltSink`]; this crate implements it.
/// Declared so dep-lint documents the 02B edge (liq-risk → liq-state).
#[inline]
pub fn drift_halt_reason() -> liq_types::HaltReason {
    let _ = core::any::type_name::<liq_state::drift::DriftDetector>();
    liq_types::HaltReason::DriftMismatch
}

/// Class C alert intake (09A will forward). Never a halt.
pub fn alert_class_c(kind: &str, detail: &str) {
    warn!(kind, detail, class = "C", "risk alert (no halt)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_reason_is_class_b() {
        assert_eq!(class_of(drift_halt_reason()), HaltClass::B);
    }
}
