//! Recorded mainnet pool states. Fail closed until A3 (D60) can supply them.

use std::path::Path;

use alloy_primitives::U256;

use super::{AmmFamily, ParityError};

/// A pool snapshot as the quote API consumes it. Never invented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedPoolState {
    pub family: AmmFamily,
    pub block: u64,
    pub amount_in: U256,
}

/// 05B parquet does not store Uniswap/Curve/Kyber storage slots. Folding
/// those from `eth_getStorageAt` / `debug_trace` is A3's job (local node).
pub fn recorded_mainnet_states() -> Result<Vec<RecordedPoolState>, ParityError> {
    Err(ParityError::A3Deferred)
}

/// Same fold, but distinguish a missing directory from A3-not-ready.
pub fn recorded_states_at(root: &Path) -> Result<Vec<RecordedPoolState>, ParityError> {
    if !root.exists() {
        return Err(ParityError::ArchiveEmpty {
            path: root.display().to_string(),
        });
    }
    let mut any = false;
    if root.is_dir() {
        let rd = std::fs::read_dir(root).map_err(|_| ParityError::ArchiveEmpty {
            path: root.display().to_string(),
        })?;
        for e in rd {
            let e = e.map_err(|_| ParityError::ArchiveEmpty {
                path: root.display().to_string(),
            })?;
            any = true;
            let _ = e;
        }
    }
    if !any {
        return Err(ParityError::ArchiveEmpty {
            path: root.display().to_string(),
        });
    }
    // Directory has files (likely 05B headers/events). Those are not pool
    // storage snapshots. Do not decode Swap logs into fake reserves.
    Err(ParityError::A3Deferred)
}
