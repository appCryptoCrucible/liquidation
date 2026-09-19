//! Halt scope, reason, and sink (GUIDE 14 §2). `liq-risk` implements [`HaltSink`].

use crate::ids::{AssetId, FlashProvider, MarketId, ProtocolId};

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
}
