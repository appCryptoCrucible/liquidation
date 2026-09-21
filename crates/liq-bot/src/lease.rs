//! Submit lease: post-restart integrity gates live send (GUIDE 17 §2).
//!
//! Corrupt snapshot / WAL → lease refused. Never submit from a process that
//! has not passed this check. Chain-nonce resync before first live POST is
//! H4 — ABSENT here; [`SubmitLease::nonce_resync`] stays false.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use liq_state::{
    recover, RecoverError, SnapshotError, StateStore, StoreConfig, UndoCapacity, WalError,
};
use thiserror::Error;

/// Paths recover reads. Both must exist; missing is refuse, not skip.
#[derive(Clone, Debug)]
pub struct StatePaths {
    pub snapshot: PathBuf,
    pub wal: PathBuf,
}

/// Lease state after the drift / integrity check.
///
/// `held` / `nonce_resync` are `Arc` so [`crate::exec_bind::bind`] can attach
/// the same atomics to [`liq_exec::path::ExecPath`]. 17A never stores
/// `nonce_resync` true (`run` / `acquire` included).
pub struct SubmitLease {
    held: Arc<AtomicBool>,
    nonce_resync: Arc<AtomicBool>,
}

impl SubmitLease {
    #[must_use]
    pub fn refused() -> Self {
        Self {
            held: Arc::new(AtomicBool::new(false)),
            nonce_resync: Arc::new(AtomicBool::new(false)),
        }
    }

    #[must_use]
    pub fn granted_shadow() -> Self {
        Self {
            held: Arc::new(AtomicBool::new(true)),
            // H4: chain-nonce resync hook ABSENT. Live POST stays closed.
            nonce_resync: Arc::new(AtomicBool::new(false)),
        }
    }

    #[must_use]
    pub fn held(&self) -> bool {
        self.held.load(Ordering::Acquire)
    }

    /// Always false until an H4 resync hook exists. Not a silent guess.
    #[must_use]
    pub fn nonce_resync(&self) -> bool {
        self.nonce_resync.load(Ordering::Acquire)
    }

    /// Same atomic [`crate::exec_bind::bind`] attaches. Default false.
    #[must_use]
    pub fn held_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.held)
    }

    /// Same atomic [`crate::exec_bind::bind`] attaches. Default false; H4 stores.
    #[must_use]
    pub fn nonce_resync_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.nonce_resync)
    }

    /// Live HTTP = held ∧ `submit_enabled` ∧ nonce resync. Resync is ABSENT.
    /// Process-log query only — the POST reads the atomics on `ExecPath`.
    #[must_use]
    pub fn live_send_permitted(&self, submit_enabled: bool) -> bool {
        self.held() && submit_enabled && self.nonce_resync()
    }
}

impl core::fmt::Debug for SubmitLease {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SubmitLease")
            .field("held", &self.held())
            .field("nonce_resync", &self.nonce_resync())
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum LeaseError {
    #[error("snapshot corrupt (magic/crc/version/truncated)")]
    CorruptSnapshot,
    #[error("WAL corrupt (magic/crc)")]
    CorruptWal,
    #[error("recover failed: {0}")]
    Recover(String),
    #[error("positions present and no integrity probe ran — lease refused")]
    PositionsUnprobed,
    #[error("integrity probe refused the restored store")]
    ProbeRefused,
}

/// Optional post-recover probe. Missing + nonempty store → refuse.
pub trait IntegrityProbe {
    fn check(&self, store: &StateStore) -> Result<(), LeaseError>;
}

/// Default store sizing for recover. Capacity only; not invented chain data.
#[must_use]
pub fn recover_capacity() -> StoreConfig {
    StoreConfig {
        base: 0,
        positions: 4,
        markets: 4,
        undo: UndoCapacity {
            ops: 64,
            extras: 8,
            rows: 8,
        },
    }
}

/// Recover snapshot+WAL. Integrity failure refuses the lease.
pub fn acquire(
    paths: &StatePaths,
    capacity: StoreConfig,
    probe: Option<&dyn IntegrityProbe>,
) -> Result<(StateStore, SubmitLease), LeaseError> {
    let store = match recover(&paths.snapshot, &paths.wal, capacity) {
        Ok(s) => s,
        Err(RecoverError::Snapshot(e)) => {
            tracing::error!(?e, path = %paths.snapshot.display(), "lease: snapshot refused");
            return Err(map_snapshot(e));
        }
        Err(RecoverError::Wal(e)) => {
            tracing::error!(?e, path = %paths.wal.display(), "lease: WAL refused");
            return Err(map_wal(e));
        }
        Err(e) => {
            tracing::error!(?e, "lease: recover failed");
            return Err(LeaseError::Recover(e.to_string()));
        }
    };
    if !store.is_empty() {
        match probe {
            None => {
                tracing::error!(n = store.len(), "lease: positions restored without probe");
                return Err(LeaseError::PositionsUnprobed);
            }
            Some(p) => p.check(&store)?,
        }
    }
    Ok((store, SubmitLease::granted_shadow()))
}

fn map_snapshot(e: SnapshotError) -> LeaseError {
    match e {
        SnapshotError::Crc
        | SnapshotError::Magic
        | SnapshotError::Version(_)
        | SnapshotError::Truncated
        | SnapshotError::Layout => LeaseError::CorruptSnapshot,
        other => LeaseError::Recover(other.to_string()),
    }
}

fn map_wal(e: WalError) -> LeaseError {
    match e {
        WalError::Crc | WalError::Magic | WalError::Corrupt => LeaseError::CorruptWal,
        other => LeaseError::Recover(other.to_string()),
    }
}

/// Write a magic-only WAL (real header, no frames).
pub fn write_empty_wal(path: &Path) -> Result<(), LeaseError> {
    std::fs::write(path, b"LIQWAL01").map_err(|e| LeaseError::Recover(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use liq_state::{StateStore, StoreSnapshot};
    use std::sync::atomic::AtomicU64;

    static N: AtomicU64 = AtomicU64::new(0);

    fn dir() -> PathBuf {
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("liq-17a-lease-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn submit_enabled_false_and_resync_absent_block_live() {
        let l = SubmitLease::granted_shadow();
        assert!(l.held());
        assert!(!l.nonce_resync(), "chain-nonce resync ABSENT (H4)");
        assert!(!l.live_send_permitted(false));
        assert!(
            !l.live_send_permitted(true),
            "true toggle still blocked without resync"
        );
        assert!(!SubmitLease::refused().held());
        let flag = l.nonce_resync_flag();
        flag.store(true, Ordering::Release);
        assert!(l.nonce_resync(), "flag Arc is the lease atomic");
        flag.store(false, Ordering::Release);
        assert!(!l.nonce_resync());
    }

    #[test]
    fn empty_store_grants_shadow_lease() {
        let d = dir();
        let snap = d.join("s.snap");
        let wal = d.join("w.log");
        StoreSnapshot::empty().write_to(&snap).unwrap();
        write_empty_wal(&wal).unwrap();
        let (store, lease) = acquire(
            &StatePaths {
                snapshot: snap,
                wal,
            },
            recover_capacity(),
            None,
        )
        .unwrap();
        assert_eq!(store.len(), 0);
        assert!(lease.held());
        assert!(!lease.nonce_resync());
    }

    /// Real CRC poison (XOR last byte), not a fabricated "looks drifted".
    #[test]
    fn poison_snapshot_crc_refuses_lease() {
        let d = dir();
        let snap = d.join("s.snap");
        let wal = d.join("w.log");
        let mut st = StateStore::new(recover_capacity());
        liq_protocol::StateWriter::push_market(
            &mut st,
            liq_types::MarketId(0),
            liq_protocol::MarketRow::blank(liq_types::AssetId(0), 18),
        )
        .unwrap();
        st.snapshot().write_to(&snap).unwrap();
        write_empty_wal(&wal).unwrap();
        let mut raw = std::fs::read(&snap).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        std::fs::write(&snap, &raw).unwrap();
        let err = acquire(
            &StatePaths {
                snapshot: snap,
                wal,
            },
            recover_capacity(),
            None,
        )
        .unwrap_err();
        assert!(matches!(err, LeaseError::CorruptSnapshot), "got {err:?}");
    }

    #[test]
    fn poison_wal_magic_refuses_lease() {
        let d = dir();
        let snap = d.join("s.snap");
        let wal = d.join("w.log");
        StoreSnapshot::empty().write_to(&snap).unwrap();
        std::fs::write(&wal, b"XXXXXXXX").unwrap();
        let err = acquire(
            &StatePaths {
                snapshot: snap,
                wal,
            },
            recover_capacity(),
            None,
        )
        .unwrap_err();
        assert!(matches!(err, LeaseError::CorruptWal), "got {err:?}");
    }

    struct RefuseProbe;
    impl IntegrityProbe for RefuseProbe {
        fn check(&self, _: &StateStore) -> Result<(), LeaseError> {
            Err(LeaseError::ProbeRefused)
        }
    }

    #[test]
    fn nonempty_without_probe_refuses() {
        let d = dir();
        let snap = d.join("s.snap");
        let wal = d.join("w.log");
        let mut st = StateStore::new(recover_capacity());
        let m = liq_types::MarketId(0);
        liq_protocol::StateWriter::push_market(
            &mut st,
            m,
            liq_protocol::MarketRow::blank(liq_types::AssetId(0), 18),
        )
        .unwrap();
        let k = liq_types::PositionKey {
            protocol: liq_types::ProtocolId(1),
            market: m,
            user: alloy_primitives::Address::repeat_byte(1),
        };
        let _ = liq_protocol::StateWriter::intern(&mut st, &k).unwrap();
        st.snapshot().write_to(&snap).unwrap();
        write_empty_wal(&wal).unwrap();
        let err = acquire(
            &StatePaths {
                snapshot: snap.clone(),
                wal: wal.clone(),
            },
            recover_capacity(),
            None,
        )
        .unwrap_err();
        assert!(matches!(err, LeaseError::PositionsUnprobed));
        let err = acquire(
            &StatePaths {
                snapshot: snap,
                wal,
            },
            recover_capacity(),
            Some(&RefuseProbe),
        )
        .unwrap_err();
        assert!(matches!(err, LeaseError::ProbeRefused));
    }
}
