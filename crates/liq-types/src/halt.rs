//! Halt scope, reason, and sink (GUIDE 14 §2). `liq-risk` implements [`HaltSink`].
//! [`Allow`] / [`AllowQuery`] / [`RiskAllow`] are the 13A submit gate (D46: types live here).

use alloy_primitives::Address;

use crate::ids::{AssetId, FlashProvider, MarketId, ProtocolId};
use crate::trace::TraceId;

/// Trigger class for [`HaltScope::Trigger`] (GUIDE 08 §5 / GUIDE 14 §2).
/// Payloads stay on `TriggerCause` in `liq-engine` (WP 08A).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TriggerKind {
    SvrAuction,
    OraclePublic,
    OraclePullHeld,
    PoolStateChange,
    UserAction,
    DerivedRate,
    ParamChange,
    InterestDrift,
    Stale,
    OraclePredicted,
}

/// Granular halt target (GUIDE 14 §2). `Global` is reserved for node lag.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum HaltScope {
    Global,
    Protocol(ProtocolId),
    Market(MarketId),
    Asset(AssetId),
    FlashProvider(FlashProvider),
    Trigger(TriggerKind),
}

/// Why a halt fired. Variants are the Class A/B matrix rows that actually halt
/// (GUIDE 14 §1) plus `AdapterPanic` (RUST-CONVENTIONS §2.5).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum HaltReason {
    NodeLag,
    OracleStale,
    MevShareDisconnected,
    FlashLiquidityCollapse,
    FlashCallbackRevertStreak,
    ReorgTooDeep,
    GasWalletBelowFloor,
    ProxyUpgrade,
    DriftMismatch,
    SimChainDivergence,
    AdapterPanic,
}

/// Implemented by `liq-risk`. Callers in ingest/state depend on this trait, not
/// on `liq-risk` (D46).
pub trait HaltSink: Send + Sync {
    fn halt(&self, scope: HaltScope, reason: HaltReason);

    /// Drop an auto-clearing halt after a fresh observation. Default is a
    /// no-op so test sinks do not have to implement it.
    fn clear(&self, _scope: HaltScope, _reason: HaltReason) {}
}

/// What [`RiskAllow::allow`] returns. 13A treats [`Allow::Denied`] as do-not-send.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Allow {
    Yes,
    Denied {
        scope: HaltScope,
        reason: HaltReason,
    },
}

/// Candidate identity for a gated submission (13A fills this from the job).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AllowQuery {
    pub protocol: ProtocolId,
    pub market: MarketId,
    pub collateral: AssetId,
    pub debt: AssetId,
    pub flash: FlashProvider,
    pub trigger: TriggerKind,
    pub operator_key: Address,
}

/// 13A calls this before every send. Implemented by `liq-risk::RiskGate`.
/// Lives here so `liq-exec` does not take a `liq-risk` → `liq-state` edge
/// (forbid.txt: `liq-exec` must not reach `liq-state`).
pub trait RiskAllow: Send + Sync {
    fn allow(&self, trace: TraceId, q: &AllowQuery) -> Allow;
}
