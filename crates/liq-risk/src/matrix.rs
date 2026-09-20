//! Halt matrix (GUIDE 14 §1, D13b–d). Every row has metric, threshold, action, class.

use liq_types::HaltReason;

/// Admission class (D13c).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HaltClass {
    /// Scoped, auto-clears when the world condition ends.
    A,
    /// Scoped, manual: clearing requires a code change. Never `Global`.
    B,
    /// Rolling / market-condition metric. Log only; never a halt.
    C,
}

/// One matrix row. Thresholds are the GUIDE 14 numbers; 05C may retune A-row
/// numeric gates but must not change class.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MatrixRow {
    pub name: &'static str,
    pub metric: &'static str,
    pub threshold: &'static str,
    pub action: &'static str,
    pub class: HaltClass,
    /// `None` for Class C (no [`HaltReason`]).
    pub reason: Option<HaltReason>,
}

/// Complete Step 1 matrix, including Class C and 03C early-warning alerts.
pub const MATRIX: &[MatrixRow] = &[
    MatrixRow {
        name: "node_lag",
        metric: "head_ts vs wall clock",
        threshold: "> 2 blocks (~24s)",
        action: "Halt all submissions (Global); auto-clear on catch-up",
        class: HaltClass::A,
        reason: Some(HaltReason::NodeLag),
    },
    MatrixRow {
        name: "oracle_stale",
        metric: "now − last_update",
        threshold: "> heartbeat × 1.5",
        action: "Mark asset untradeable; clear on next fresh update",
        class: HaltClass::A,
        reason: Some(HaltReason::OracleStale),
    },
    MatrixRow {
        name: "mevshare_disconnected",
        metric: "SSE heartbeat age",
        threshold: "> 60s",
        action: "Halt Trigger(SvrAuction) only",
        class: HaltClass::A,
        reason: Some(HaltReason::MevShareDisconnected),
    },
    MatrixRow {
        name: "flash_liquidity_collapse",
        metric: "FlashIndex available vs baseline",
        threshold: "drop > collapse_bps",
        action: "Mark asset ineligible; never Global",
        class: HaltClass::A,
        reason: Some(HaltReason::FlashLiquidityCollapse),
    },
    MatrixRow {
        name: "flash_callback_revert_streak",
        metric: "consecutive callback reverts per provider",
        threshold: ">= revert_streak",
        action: "Disable that FlashProvider; fall back",
        class: HaltClass::A,
        reason: Some(HaltReason::FlashCallbackRevertStreak),
    },
    MatrixRow {
        name: "reorg_too_deep",
        metric: "unwind ancestor vs undo ring",
        threshold: "ancestor not in ring",
        action: "Halt Global; clear when snapshot rebuild completes",
        class: HaltClass::A,
        reason: Some(HaltReason::ReorgTooDeep),
    },
    MatrixRow {
        name: "gas_wallet_below_floor",
        metric: "operator-key ETH balance",
        threshold: "balance < gas_floor[key]",
        action: "Halt that key only; clear on top-up",
        class: HaltClass::A,
        reason: Some(HaltReason::GasWalletBelowFloor),
    },
    MatrixRow {
        name: "adapter_panic",
        metric: "catch_unwind on protocol apply",
        threshold: "panic this block",
        action: "Halt that protocol; auto-clear after contained unwind",
        class: HaltClass::A,
        reason: Some(HaltReason::AdapterPanic),
    },
    MatrixRow {
        name: "proxy_upgrade",
        metric: "EIP-1967 implementation slot",
        threshold: "slot value ≠ last seen",
        action: "Class B halt that protocol or flash provider",
        class: HaltClass::B,
        reason: Some(HaltReason::ProxyUpgrade),
    },
    MatrixRow {
        name: "drift_mismatch",
        metric: "health() vs health_probe EWMA",
        threshold: "> max_mismatch_bps (02B/05C)",
        action: "Class B halt that protocol (math bug; needs code)",
        class: HaltClass::B,
        reason: Some(HaltReason::DriftMismatch),
    },
    MatrixRow {
        name: "sim_chain_divergence",
        metric: "consecutive sim-pass / chain-revert",
        threshold: ">= sim_chain_streak",
        action: "Class B halt that protocol; dump state",
        class: HaltClass::B,
        reason: Some(HaltReason::SimChainDivergence),
    },
    MatrixRow {
        name: "gas_spend_rate",
        metric: "rolling gas spend vs revenue",
        threshold: "config anomaly window",
        action: "Alert only",
        class: HaltClass::C,
        reason: None,
    },
    MatrixRow {
        name: "pnl_drawdown",
        metric: "rolling realized PnL vs limit",
        threshold: "config drawdown window",
        action: "Alert only (D13b; not a halt)",
        class: HaltClass::C,
        reason: None,
    },
    MatrixRow {
        name: "proxy_admin_ownership",
        metric: "ProxyAdmin.OwnershipTransferred (V4)",
        threshold: "any transfer",
        action: "Alert only (03C early warning)",
        class: HaltClass::C,
        reason: None,
    },
    MatrixRow {
        name: "acl_role_granted",
        metric: "ACL RoleGranted (V3)",
        threshold: "any grant",
        action: "Alert only (03C early warning)",
        class: HaltClass::C,
        reason: None,
    },
    MatrixRow {
        name: "single_flash_source",
        metric: "viable FlashIndex sources per major debt asset",
        threshold: "< 2",
        action: "Alert only",
        class: HaltClass::C,
        reason: None,
    },
    MatrixRow {
        name: "provider_concentration",
        metric: "liquidations through one FlashProvider / total",
        threshold: ">= concentration_bps",
        action: "Alert only",
        class: HaltClass::C,
        reason: None,
    },
    MatrixRow {
        name: "flash_depth_short",
        metric: "available vs median liquidation notional",
        threshold: "available < median",
        action: "Alert only",
        class: HaltClass::C,
        reason: None,
    },
    MatrixRow {
        name: "health_wrong",
        metric: "watcher vs engine HealthWrong (09A)",
        threshold: "any",
        action: "Alert only until 09A alarm wiring",
        class: HaltClass::C,
        reason: None,
    },
];

/// Class for a halt reason. Every [`HaltReason`] is A or B; Class C has none.
#[must_use]
pub const fn class_of(reason: HaltReason) -> HaltClass {
    match reason {
        HaltReason::NodeLag
        | HaltReason::OracleStale
        | HaltReason::MevShareDisconnected
        | HaltReason::FlashLiquidityCollapse
        | HaltReason::FlashCallbackRevertStreak
        | HaltReason::ReorgTooDeep
        | HaltReason::GasWalletBelowFloor
        | HaltReason::AdapterPanic => HaltClass::A,
        HaltReason::ProxyUpgrade | HaltReason::DriftMismatch | HaltReason::SimChainDivergence => {
            HaltClass::B
        }
    }
}

/// Class C names (no [`HaltReason`]).
pub fn class_c_rows() -> impl Iterator<Item = &'static MatrixRow> {
    MATRIX.iter().filter(|r| r.class == HaltClass::C)
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used
)]
mod tests {
    use super::*;
    use liq_types::HaltReason;

    #[test]
    fn every_halt_reason_has_a_or_b_row() {
        let reasons = [
            HaltReason::NodeLag,
            HaltReason::OracleStale,
            HaltReason::MevShareDisconnected,
            HaltReason::FlashLiquidityCollapse,
            HaltReason::FlashCallbackRevertStreak,
            HaltReason::ReorgTooDeep,
            HaltReason::GasWalletBelowFloor,
            HaltReason::ProxyUpgrade,
            HaltReason::DriftMismatch,
            HaltReason::SimChainDivergence,
            HaltReason::AdapterPanic,
        ];
        for r in reasons {
            let row = MATRIX
                .iter()
                .find(|row| row.reason == Some(r))
                .unwrap_or_else(|| panic!("missing matrix row for {r:?}"));
            assert_eq!(row.class, class_of(r));
            assert!(!row.metric.is_empty());
            assert!(!row.threshold.is_empty());
            assert!(!row.action.is_empty());
        }
    }

    #[test]
    fn no_class_b_is_global() {
        for row in MATRIX.iter().filter(|r| r.class == HaltClass::B) {
            assert!(
                !row.action.to_ascii_lowercase().contains("global"),
                "Class B must not be Global: {}",
                row.name
            );
            assert_eq!(class_of(row.reason.expect("B has reason")), HaltClass::B);
        }
    }

    #[test]
    fn rolling_metrics_are_class_c() {
        for name in [
            "gas_spend_rate",
            "pnl_drawdown",
            "provider_concentration",
            "flash_depth_short",
        ] {
            let row = MATRIX.iter().find(|r| r.name == name).expect(name);
            assert_eq!(row.class, HaltClass::C);
            assert!(row.reason.is_none());
        }
        assert!(class_c_rows().count() >= 4);
    }

    #[test]
    fn drift_mismatch_is_class_b() {
        assert_eq!(class_of(HaltReason::DriftMismatch), HaltClass::B);
    }
}
