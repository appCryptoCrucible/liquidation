//! Rebuild orchestration (03B D1): stop hot → recover → replay tip→head →
//! respawn → [`RiskGate::observe_reorg_rebuild_done`].
//!
//! Both [`RebuildReason::ReorgTooDeep`] and [`RebuildReason::Inconsistent`]
//! share this path. Missing RPC/archive is [`RebuildOutcome::A3Deferred`]
//! — do not invent chain data, do not clear the halt.

use liq_risk::RiskGate;
use liq_state::{StateError, StateStore, StoreConfig};
use thiserror::Error;

use crate::lease::{acquire, StatePaths};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RebuildReason {
    ReorgTooDeep,
    Inconsistent,
}

impl RebuildReason {
    #[must_use]
    pub fn from_state(e: StateError) -> Option<Self> {
        match e {
            StateError::ReorgTooDeep { .. } => Some(Self::ReorgTooDeep),
            StateError::Inconsistent => Some(Self::Inconsistent),
            _ => None,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RebuildOutcome {
    Done,
    A3Deferred,
}

#[derive(Debug, Error)]
pub enum RebuildError {
    #[error("rebuild lease: {0}")]
    Lease(#[from] crate::lease::LeaseError),
    #[error("rebuild replay: {0}")]
    Replay(String),
    #[error("rebuild respawn: {0}")]
    Respawn(String),
    #[error("rebuild stop: {0}")]
    Stop(String),
}

/// Replay snapshot-tip → head. Absent archive/RPC returns [`RebuildOutcome::A3Deferred`].
pub trait HeadReplay {
    fn replay_to_head(
        &self,
        store: &StateStore,
        from_exclusive: u64,
        head: u64,
    ) -> Result<RebuildOutcome, RebuildError>;
}

/// No archive / no RPC. Fail closed: A3Deferred, never invented blocks.
pub struct AbsentArchive;

impl HeadReplay for AbsentArchive {
    fn replay_to_head(
        &self,
        _store: &StateStore,
        _from_exclusive: u64,
        _head: u64,
    ) -> Result<RebuildOutcome, RebuildError> {
        tracing::error!("rebuild: archive/RPC absent (A3Deferred)");
        Ok(RebuildOutcome::A3Deferred)
    }
}

/// Recover + replay inputs. Grouped so the orchestrator stays under arity 7.
pub struct RebuildIo<'a> {
    pub paths: &'a StatePaths,
    pub capacity: StoreConfig,
    pub replay: &'a dyn HeadReplay,
    pub head: Option<u64>,
}

/// Stop hot, recover, replay, respawn, observe. Same function for both reasons.
pub fn orchestrate(
    reason: RebuildReason,
    io: &RebuildIo<'_>,
    gate: &RiskGate,
    stop_hot: impl FnOnce() -> Result<(), RebuildError>,
    respawn: impl FnOnce(StateStore) -> Result<(), RebuildError>,
) -> Result<RebuildOutcome, RebuildError> {
    tracing::error!(?reason, "rebuild orchestration start");
    stop_hot()?;
    let (store, _lease) = acquire(io.paths, io.capacity, None)?;
    let from = store.tip();
    let Some(head) = io.head else {
        tracing::error!("rebuild: chain head unknown — A3Deferred");
        return Ok(RebuildOutcome::A3Deferred);
    };
    if head < from {
        tracing::error!(from, head, "rebuild: head behind snapshot tip");
        return Err(RebuildError::Replay("head is behind snapshot tip".into()));
    }
    match io.replay.replay_to_head(&store, from, head)? {
        RebuildOutcome::A3Deferred => {
            tracing::error!("rebuild deferred; observe_reorg_rebuild_done not called");
            Ok(RebuildOutcome::A3Deferred)
        }
        RebuildOutcome::Done => {
            respawn(store)?;
            gate.observe_reorg_rebuild_done();
            tracing::info!(?reason, "rebuild done; reorg halt cleared");
            Ok(RebuildOutcome::Done)
        }
    }
}

/// Entry used by ingest when unwind surfaces these errors.
pub fn on_reorg_or_inconsistent(
    err: StateError,
    io: &RebuildIo<'_>,
    gate: &RiskGate,
    stop_hot: impl FnOnce() -> Result<(), RebuildError>,
    respawn: impl FnOnce(StateStore) -> Result<(), RebuildError>,
) -> Result<RebuildOutcome, RebuildError> {
    let Some(reason) = RebuildReason::from_state(err) else {
        return Err(RebuildError::Replay(format!(
            "not a rebuild-triggering error: {err}"
        )));
    };
    orchestrate(reason, io, gate, stop_hot, respawn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::{recover_capacity, write_empty_wal};
    use liq_state::StoreSnapshot;
    use liq_types::{HaltReason, HaltScope, HaltSink};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    fn dir() -> PathBuf {
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("liq-17a-rebuild-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn paths() -> (StatePaths, PathBuf) {
        let d = dir();
        let snap = d.join("s.snap");
        let wal = d.join("w.log");
        StoreSnapshot::empty().write_to(&snap).unwrap();
        write_empty_wal(&wal).unwrap();
        (
            StatePaths {
                snapshot: snap,
                wal,
            },
            d,
        )
    }

    struct DoneReplay;
    impl HeadReplay for DoneReplay {
        fn replay_to_head(
            &self,
            _: &StateStore,
            _: u64,
            _: u64,
        ) -> Result<RebuildOutcome, RebuildError> {
            Ok(RebuildOutcome::Done)
        }
    }

    fn io<'a>(
        paths: &'a StatePaths,
        replay: &'a dyn HeadReplay,
        head: Option<u64>,
    ) -> RebuildIo<'a> {
        RebuildIo {
            paths,
            capacity: recover_capacity(),
            replay,
            head,
        }
    }

    #[test]
    fn both_reasons_share_path_and_observe_has_caller() {
        let (p, _) = paths();
        let gate = RiskGate::new();
        HaltSink::halt(&gate, HaltScope::Global, HaltReason::ReorgTooDeep);
        let mut observed = false;
        let replay = DoneReplay;
        let io1 = io(&p, &replay, Some(0));
        let out = on_reorg_or_inconsistent(
            StateError::ReorgTooDeep { depth: 9, cap: 1 },
            &io1,
            &gate,
            || Ok(()),
            |_| {
                observed = true;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(out, RebuildOutcome::Done);
        assert!(observed, "respawn must run");
        // observe_reorg_rebuild_done cleared the halt
        let q = liq_types::AllowQuery {
            protocol: liq_types::ProtocolId(1),
            market: liq_types::MarketId(0),
            debt: liq_types::AssetId(0),
            collateral: liq_types::AssetId(1),
            flash: liq_types::FlashProvider::Aave,
            trigger: liq_types::TriggerKind::InterestDrift,
            operator_key: alloy_primitives::Address::ZERO,
        };
        assert_eq!(
            gate.allow(liq_types::TraceId::from_raw(1), &q),
            liq_types::Allow::Yes
        );

        let (p2, _) = paths();
        let gate2 = RiskGate::new();
        let called = AtomicBool::new(false);
        let replay2 = DoneReplay;
        let io2 = io(&p2, &replay2, Some(0));
        let out = on_reorg_or_inconsistent(
            StateError::Inconsistent,
            &io2,
            &gate2,
            || Ok(()),
            |_| {
                called.store(true, Ordering::Release);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(out, RebuildOutcome::Done);
        assert!(called.load(Ordering::Acquire));
    }

    #[test]
    fn absent_archive_is_a3_and_does_not_observe() {
        let (p, _) = paths();
        let gate = RiskGate::new();
        HaltSink::halt(&gate, HaltScope::Global, HaltReason::ReorgTooDeep);
        let respawned = AtomicBool::new(false);
        let replay = AbsentArchive;
        let io1 = io(&p, &replay, Some(1));
        let out = orchestrate(
            RebuildReason::ReorgTooDeep,
            &io1,
            &gate,
            || Ok(()),
            |_| {
                respawned.store(true, Ordering::Release);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(out, RebuildOutcome::A3Deferred);
        assert!(!respawned.load(Ordering::Acquire));
        let q = liq_types::AllowQuery {
            protocol: liq_types::ProtocolId(1),
            market: liq_types::MarketId(0),
            debt: liq_types::AssetId(0),
            collateral: liq_types::AssetId(1),
            flash: liq_types::FlashProvider::Aave,
            trigger: liq_types::TriggerKind::InterestDrift,
            operator_key: alloy_primitives::Address::ZERO,
        };
        assert!(matches!(
            gate.allow(liq_types::TraceId::from_raw(1), &q),
            liq_types::Allow::Denied { .. }
        ));
    }

    #[test]
    fn missing_head_is_a3() {
        let (p, _) = paths();
        let gate = RiskGate::new();
        let replay = DoneReplay;
        let io1 = io(&p, &replay, None);
        let out = orchestrate(
            RebuildReason::Inconsistent,
            &io1,
            &gate,
            || Ok(()),
            |_| panic!("respawn must not run"),
        )
        .unwrap();
        assert_eq!(out, RebuildOutcome::A3Deferred);
    }
}
