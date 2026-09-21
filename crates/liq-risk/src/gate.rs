//! [`RiskGate`]: [`HaltSink`] + `allow` (GUIDE 14 §1–§2). Halts log now; 13A gates submit.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use alloy_primitives::Address;
use parking_lot::RwLock;
use tracing::{error, info, warn};

use liq_types::{
    AssetId, FlashProvider, HaltReason, HaltScope, HaltSink, MarketId, ProtocolId, RiskAllow,
    TraceId, TriggerKind,
};

use crate::matrix::{class_of, HaltClass};

/// Re-export so existing `use crate::gate::{Allow, AllowQuery}` keeps compiling.
pub use liq_types::{Allow, AllowQuery};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct HaltEntry {
    reason: HaltReason,
    metric: u64,
}

/// Active halts. Hot-path reads take a parking_lot read lock (off the ingest
/// writer). Global A-halts use atomics so `allow` can fail closed without the map.
pub struct RiskGate {
    global: AtomicU64,
    protocols: RwLock<HashMap<ProtocolId, HaltEntry>>,
    markets: RwLock<HashMap<MarketId, HaltEntry>>,
    assets: RwLock<HashMap<AssetId, HaltEntry>>,
    flash: RwLock<HashMap<FlashProvider, HaltEntry>>,
    triggers: RwLock<HashMap<TriggerKind, HaltEntry>>,
    keys: RwLock<HashMap<Address, HaltEntry>>,
    /// Class B work queue (D13d). Never auto-cleared.
    action_required: RwLock<Vec<ActionRequired>>,
    sim_chain_streak: RwLock<HashMap<ProtocolId, u32>>,
    callback_streak: RwLock<HashMap<FlashProvider, u32>>,
    pub sim_chain_threshold: u32,
    pub callback_revert_threshold: u32,
}

/// One Class B line. Leaves only via [`RiskGate::clear_after_ship`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRequired {
    pub protocol: Option<ProtocolId>,
    pub flash: Option<FlashProvider>,
    pub reason: HaltReason,
    pub metric: u64,
    pub scope: HaltScope,
}

const G_NODE: u64 = 1;
const G_REORG: u64 = 2;

impl Default for RiskGate {
    fn default() -> Self {
        Self::new()
    }
}

impl RiskGate {
    #[must_use]
    pub fn new() -> Self {
        Self {
            global: AtomicU64::new(0),
            protocols: RwLock::new(HashMap::new()),
            markets: RwLock::new(HashMap::new()),
            assets: RwLock::new(HashMap::new()),
            flash: RwLock::new(HashMap::new()),
            triggers: RwLock::new(HashMap::new()),
            keys: RwLock::new(HashMap::new()),
            action_required: RwLock::new(Vec::new()),
            sim_chain_streak: RwLock::new(HashMap::new()),
            callback_streak: RwLock::new(HashMap::new()),
            sim_chain_threshold: 3,
            callback_revert_threshold: 3,
        }
    }

    /// 13A calls this before every send. Before 13A, callers still invoke it so
    /// denials land in logs (real behaviour).
    #[must_use]
    pub fn allow(&self, trace: TraceId, q: &AllowQuery) -> Allow {
        let g = self.global.load(Ordering::Acquire);
        if g & G_NODE != 0 {
            info!(trace = trace.raw(), reason = ?HaltReason::NodeLag, "halt deny");
            return Allow::Denied {
                scope: HaltScope::Global,
                reason: HaltReason::NodeLag,
            };
        }
        if g & G_REORG != 0 {
            info!(trace = trace.raw(), reason = ?HaltReason::ReorgTooDeep, "halt deny");
            return Allow::Denied {
                scope: HaltScope::Global,
                reason: HaltReason::ReorgTooDeep,
            };
        }
        if let Some(e) = self.protocols.read().get(&q.protocol).copied() {
            return deny(trace, HaltScope::Protocol(q.protocol), e.reason);
        }
        if let Some(e) = self.markets.read().get(&q.market).copied() {
            return deny(trace, HaltScope::Market(q.market), e.reason);
        }
        if let Some(e) = self.assets.read().get(&q.debt).copied() {
            return deny(trace, HaltScope::Asset(q.debt), e.reason);
        }
        if let Some(e) = self.assets.read().get(&q.collateral).copied() {
            return deny(trace, HaltScope::Asset(q.collateral), e.reason);
        }
        if let Some(e) = self.flash.read().get(&q.flash).copied() {
            return deny(trace, HaltScope::FlashProvider(q.flash), e.reason);
        }
        if let Some(e) = self.triggers.read().get(&q.trigger).copied() {
            return deny(trace, HaltScope::Trigger(q.trigger), e.reason);
        }
        if let Some(e) = self.keys.read().get(&q.operator_key).copied() {
            return deny(trace, HaltScope::Global, e.reason);
        }
        Allow::Yes
    }
}

impl RiskAllow for RiskGate {
    fn allow(&self, trace: TraceId, q: &AllowQuery) -> Allow {
        RiskGate::allow(self, trace, q)
    }
}

impl RiskGate {
    /// Class A clear when the world condition ends.
    pub fn clear_auto(&self, scope: HaltScope, reason: HaltReason) {
        if class_of(reason) != HaltClass::A {
            error!(?scope, ?reason, "refusing auto-clear of Class B");
            return;
        }
        self.remove(scope, reason);
        info!(?scope, ?reason, "class A halt cleared");
    }

    /// Class B leaves the action-required log only after a shipped change.
    pub fn clear_after_ship(&self, scope: HaltScope, reason: HaltReason) {
        if class_of(reason) != HaltClass::B {
            error!(?scope, ?reason, "clear_after_ship is Class B only");
            return;
        }
        self.remove(scope, reason);
        self.action_required
            .write()
            .retain(|a| !(a.scope == scope && a.reason == reason));
        info!(?scope, ?reason, "class B cleared after ship");
    }

    /// Operator protocol halt (acceptance: manual stop). Class B, never Global.
    pub fn operator_halt(&self, protocol: ProtocolId) {
        self.halt(HaltScope::Protocol(protocol), HaltReason::DriftMismatch);
    }

    pub fn action_required(&self) -> Vec<ActionRequired> {
        self.action_required.read().clone()
    }

    /// Node lag: Class A Global. `lag_blocks` vs threshold 2.
    pub fn observe_node_lag(&self, lag_blocks: u64) {
        if lag_blocks > 2 {
            self.halt(HaltScope::Global, HaltReason::NodeLag);
        } else {
            self.clear_auto(HaltScope::Global, HaltReason::NodeLag);
        }
    }

    pub fn observe_reorg_rebuild_done(&self) {
        self.clear_auto(HaltScope::Global, HaltReason::ReorgTooDeep);
    }

    pub fn observe_oracle_fresh(&self, asset: AssetId) {
        self.clear_auto(HaltScope::Asset(asset), HaltReason::OracleStale);
    }

    pub fn observe_mevshare(&self, heartbeat_age_secs: u64) {
        let scope = HaltScope::Trigger(TriggerKind::SvrAuction);
        if heartbeat_age_secs > 60 {
            self.halt(scope, HaltReason::MevShareDisconnected);
        } else {
            self.clear_auto(scope, HaltReason::MevShareDisconnected);
        }
    }

    pub fn observe_gas_key(&self, key: Address, balance_wei: u128, floor_wei: u128) {
        if balance_wei < floor_wei {
            self.keys.write().insert(
                key,
                HaltEntry {
                    reason: HaltReason::GasWalletBelowFloor,
                    metric: 1,
                },
            );
            warn!(?key, balance_wei, floor_wei, "gas wallet below floor");
        } else {
            self.keys.write().remove(&key);
        }
    }

    /// 11/13 seam: sim-pass then on-chain revert. Not a crate dep on sim/exec.
    pub fn observe_sim_chain(&self, protocol: ProtocolId, sim_passed: bool, chain_reverted: bool) {
        if !(sim_passed && chain_reverted) {
            self.sim_chain_streak.write().insert(protocol, 0);
            return;
        }
        let mut m = self.sim_chain_streak.write();
        let n = {
            let e = m.entry(protocol).or_insert(0);
            *e = e.saturating_add(1);
            *e
        };
        drop(m);
        if n >= self.sim_chain_threshold {
            error!(?protocol, streak = n, "sim-pass/chain-revert; dump state");
            self.halt(
                HaltScope::Protocol(protocol),
                HaltReason::SimChainDivergence,
            );
        }
    }

    pub fn observe_callback_revert(&self, provider: FlashProvider, reverted: bool) {
        if !reverted {
            self.callback_streak.write().insert(provider, 0);
            self.clear_auto(
                HaltScope::FlashProvider(provider),
                HaltReason::FlashCallbackRevertStreak,
            );
            return;
        }
        let mut m = self.callback_streak.write();
        let n = {
            let e = m.entry(provider).or_insert(0);
            *e = e.saturating_add(1);
            *e
        };
        drop(m);
        if n >= self.callback_revert_threshold {
            self.halt(
                HaltScope::FlashProvider(provider),
                HaltReason::FlashCallbackRevertStreak,
            );
        }
    }

    pub fn observe_liquidity_collapse(&self, asset: AssetId, drop_bps: u32, threshold_bps: u32) {
        if drop_bps > threshold_bps {
            self.halt(HaltScope::Asset(asset), HaltReason::FlashLiquidityCollapse);
        } else {
            self.clear_auto(HaltScope::Asset(asset), HaltReason::FlashLiquidityCollapse);
        }
    }

    fn remove(&self, scope: HaltScope, reason: HaltReason) {
        match scope {
            HaltScope::Global => match reason {
                HaltReason::NodeLag => {
                    self.global.fetch_and(!G_NODE, Ordering::AcqRel);
                }
                HaltReason::ReorgTooDeep => {
                    self.global.fetch_and(!G_REORG, Ordering::AcqRel);
                }
                _ => {}
            },
            HaltScope::Protocol(p) => {
                let mut g = self.protocols.write();
                if g.get(&p).is_some_and(|e| e.reason == reason) {
                    g.remove(&p);
                }
            }
            HaltScope::Market(m) => {
                let mut g = self.markets.write();
                if g.get(&m).is_some_and(|e| e.reason == reason) {
                    g.remove(&m);
                }
            }
            HaltScope::Asset(a) => {
                let mut g = self.assets.write();
                if g.get(&a).is_some_and(|e| e.reason == reason) {
                    g.remove(&a);
                }
            }
            HaltScope::FlashProvider(p) => {
                let mut g = self.flash.write();
                if g.get(&p).is_some_and(|e| e.reason == reason) {
                    g.remove(&p);
                }
            }
            HaltScope::Trigger(t) => {
                let mut g = self.triggers.write();
                if g.get(&t).is_some_and(|e| e.reason == reason) {
                    g.remove(&t);
                }
            }
        }
    }
}

fn deny(trace: TraceId, scope: HaltScope, reason: HaltReason) -> Allow {
    info!(trace = trace.raw(), ?scope, ?reason, "halt deny");
    Allow::Denied { scope, reason }
}

impl HaltSink for RiskGate {
    fn halt(&self, scope: HaltScope, reason: HaltReason) {
        let class = class_of(reason);
        let scope = normalize_scope(scope, reason, class);
        let Some(scope) = scope else {
            return;
        };
        match class {
            HaltClass::C => {
                error!(?reason, "HaltReason mapped to C — impossible");
            }
            HaltClass::A => {
                apply_halt(self, scope, reason, 0);
                warn!(?scope, ?reason, class = "A", "halt");
            }
            HaltClass::B => {
                apply_halt(self, scope, reason, 0);
                let rec = ActionRequired {
                    protocol: match scope {
                        HaltScope::Protocol(p) => Some(p),
                        _ => None,
                    },
                    flash: match scope {
                        HaltScope::FlashProvider(p) => Some(p),
                        _ => None,
                    },
                    reason,
                    metric: 0,
                    scope,
                };
                self.action_required.write().push(rec);
                error!(
                    ?scope,
                    ?reason,
                    class = "B",
                    "ACTION REQUIRED — code change; does not auto-clear"
                );
            }
        }
    }
}

fn normalize_scope(scope: HaltScope, reason: HaltReason, class: HaltClass) -> Option<HaltScope> {
    match reason {
        HaltReason::NodeLag | HaltReason::ReorgTooDeep => Some(HaltScope::Global),
        HaltReason::MevShareDisconnected => Some(HaltScope::Trigger(TriggerKind::SvrAuction)),
        HaltReason::GasWalletBelowFloor => {
            error!("GasWalletBelowFloor must use observe_gas_key, not HaltSink::halt");
            None
        }
        _ => {
            if class == HaltClass::B && matches!(scope, HaltScope::Global) {
                error!(?reason, "refusing Class B Global halt (D13c)");
                return None;
            }
            Some(scope)
        }
    }
}

fn apply_halt(gate: &RiskGate, scope: HaltScope, reason: HaltReason, metric: u64) {
    let e = HaltEntry { reason, metric };
    match scope {
        HaltScope::Global => match reason {
            HaltReason::NodeLag => {
                gate.global.fetch_or(G_NODE, Ordering::AcqRel);
            }
            HaltReason::ReorgTooDeep => {
                gate.global.fetch_or(G_REORG, Ordering::AcqRel);
            }
            _ => error!(?reason, "non-lag Global A-halt rejected"),
        },
        HaltScope::Protocol(p) => {
            gate.protocols.write().insert(p, e);
        }
        HaltScope::Market(m) => {
            gate.markets.write().insert(m, e);
        }
        HaltScope::Asset(a) => {
            gate.assets.write().insert(a, e);
        }
        HaltScope::FlashProvider(p) => {
            gate.flash.write().insert(p, e);
        }
        HaltScope::Trigger(t) => {
            gate.triggers.write().insert(t, e);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    fn q(protocol: ProtocolId, flash: FlashProvider) -> AllowQuery {
        AllowQuery {
            protocol,
            market: MarketId(1),
            collateral: AssetId(0),
            debt: AssetId(1),
            flash,
            trigger: TriggerKind::OraclePublic,
            operator_key: Address::ZERO,
        }
    }

    #[test]
    fn class_a_auto_clears_class_b_does_not() {
        let g = RiskGate::new();
        let t = TraceId::from_raw(1);
        g.halt(HaltScope::Asset(AssetId(1)), HaltReason::OracleStale);
        assert!(matches!(
            g.allow(t, &q(ProtocolId(1), FlashProvider::Aave)),
            Allow::Denied { .. }
        ));
        g.clear_auto(HaltScope::Asset(AssetId(1)), HaltReason::OracleStale);
        assert_eq!(
            g.allow(t, &q(ProtocolId(1), FlashProvider::Aave)),
            Allow::Yes
        );

        g.halt(
            HaltScope::Protocol(ProtocolId(1)),
            HaltReason::DriftMismatch,
        );
        assert!(!g.action_required().is_empty());
        g.clear_auto(
            HaltScope::Protocol(ProtocolId(1)),
            HaltReason::DriftMismatch,
        );
        assert!(matches!(
            g.allow(t, &q(ProtocolId(1), FlashProvider::Aave)),
            Allow::Denied { .. }
        ));
        g.clear_after_ship(
            HaltScope::Protocol(ProtocolId(1)),
            HaltReason::DriftMismatch,
        );
        assert_eq!(
            g.allow(t, &q(ProtocolId(1), FlashProvider::Aave)),
            Allow::Yes
        );
        assert!(g.action_required().is_empty());
    }

    #[test]
    fn class_b_global_refused() {
        let g = RiskGate::new();
        g.halt(HaltScope::Global, HaltReason::ProxyUpgrade);
        assert_eq!(
            g.allow(
                TraceId::from_raw(2),
                &q(ProtocolId(9), FlashProvider::Morpho)
            ),
            Allow::Yes
        );
        assert!(g.action_required().is_empty());
    }

    #[test]
    fn flash_provider_halt_does_not_stop_other_provider() {
        let g = RiskGate::new();
        g.halt(
            HaltScope::FlashProvider(FlashProvider::UniV4),
            HaltReason::FlashCallbackRevertStreak,
        );
        let t = TraceId::from_raw(3);
        assert!(matches!(
            g.allow(t, &q(ProtocolId(1), FlashProvider::UniV4)),
            Allow::Denied { .. }
        ));
        assert_eq!(
            g.allow(t, &q(ProtocolId(1), FlashProvider::Aave)),
            Allow::Yes
        );
    }

    #[test]
    fn node_lag_is_global_class_a() {
        let g = RiskGate::new();
        g.observe_node_lag(3);
        assert!(matches!(
            g.allow(TraceId::from_raw(4), &q(ProtocolId(1), FlashProvider::Aave)),
            Allow::Denied {
                reason: HaltReason::NodeLag,
                ..
            }
        ));
        g.observe_node_lag(1);
        assert_eq!(
            g.allow(TraceId::from_raw(4), &q(ProtocolId(1), FlashProvider::Aave)),
            Allow::Yes
        );
    }

    #[test]
    fn sim_chain_seam_halts_protocol() {
        let g = RiskGate::new();
        let p = ProtocolId(2);
        g.observe_sim_chain(p, true, true);
        g.observe_sim_chain(p, true, true);
        assert_eq!(
            g.allow(TraceId::from_raw(5), &q(p, FlashProvider::Aave)),
            Allow::Yes
        );
        g.observe_sim_chain(p, true, true);
        assert!(matches!(
            g.allow(TraceId::from_raw(5), &q(p, FlashProvider::Aave)),
            Allow::Denied {
                reason: HaltReason::SimChainDivergence,
                ..
            }
        ));
        assert_eq!(
            g.allow(TraceId::from_raw(5), &q(ProtocolId(3), FlashProvider::Aave)),
            Allow::Yes
        );
    }

    #[test]
    fn operator_halt_is_protocol_scoped() {
        let g = RiskGate::new();
        g.operator_halt(ProtocolId(1));
        assert!(matches!(
            g.allow(TraceId::from_raw(6), &q(ProtocolId(1), FlashProvider::Aave)),
            Allow::Denied { .. }
        ));
        assert_eq!(
            g.allow(TraceId::from_raw(6), &q(ProtocolId(2), FlashProvider::Aave)),
            Allow::Yes
        );
    }
}
