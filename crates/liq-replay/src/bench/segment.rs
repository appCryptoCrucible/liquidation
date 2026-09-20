//! Fixed archive segment probe. Empty / unpinned → fail closed, no range guess.

use std::fs;
use std::path::{Path, PathBuf};

use crate::archive::second_source_verify;

/// Workspace-relative parquet root (05B).
pub const ARCHIVE_REL: &str = "data/archive";

/// Committed replay window. `None` until A3 retention produces a span that
/// is the same load every run (GUIDE 16 §6). Do not infer min/max from
/// whatever happens to sit on disk.
pub const PINNED_RANGE: Option<(u64, u64)> = None;

/// GUIDE 16 §4b Path A (ExEx → signed bundle on the wire), nanoseconds.
pub const PATH_A_BUDGET_NS: u64 = 2_800_000;
/// Path B (SSE byte → `mev_sendBundle` sent).
pub const PATH_B_BUDGET_NS: u64 = 2_000_000;
/// Path C (CEX tick → pre-warmed plan). Soft budget.
pub const PATH_C_BUDGET_NS: u64 = 10_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArchiveProbe {
    Empty {
        path: String,
    },
    /// Parquet exists but 16C has no committed pin and/or A3 is not verified.
    PresentUnpinned {
        path: String,
        parquet: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum A3Status {
    Deferred,
    /// `second_source_verify` returned `Ok` — not claimed today (D60).
    Verified,
}

/// Live adapter roster. Counts are what this process actually constructed,
/// never a crate-directory listing. `admitted == 0` means the operator has
/// not supplied the GUIDE 15 roster size — not a guess of 190.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdapterRoster {
    pub constructed: u32,
    pub admitted: u32,
}

impl AdapterRoster {
    #[must_use]
    pub const fn unloaded() -> Self {
        Self {
            constructed: 0,
            admitted: 0,
        }
    }

    /// Full universe: a known admitted set, every member constructed.
    #[must_use]
    pub const fn is_full(self) -> bool {
        self.admitted > 0 && self.constructed == self.admitted
    }
}

#[must_use]
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[must_use]
pub fn archive_root() -> PathBuf {
    workspace_root().join(ARCHIVE_REL)
}

/// True when `dir` has no non-hidden `*.parquet` (nested included). `.gitkeep`
/// does not populate the archive (05D D1).
#[must_use]
pub fn archive_is_empty(dir: &Path) -> bool {
    !has_parquet(dir)
}

#[must_use]
pub(crate) fn has_parquet(root: &Path) -> bool {
    if !root.is_dir() {
        return false;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd {
            let Ok(ent) = ent else {
                continue;
            };
            let name = ent.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let path = ent.path();
            let Ok(ft) = ent.file_type() else {
                continue;
            };
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) == Some("parquet") {
                return true;
            }
        }
    }
    false
}

#[must_use]
pub(crate) fn parquet_file_count(root: &Path) -> u64 {
    if !root.is_dir() {
        return 0;
    }
    let mut n = 0_u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd {
            let Ok(ent) = ent else {
                continue;
            };
            let name = ent.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let path = ent.path();
            let Ok(ft) = ent.file_type() else {
                continue;
            };
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) == Some("parquet") {
                n = n.saturating_add(1);
            }
        }
    }
    n
}

#[must_use]
pub fn probe_archive(root: &Path) -> ArchiveProbe {
    let path = root.display().to_string();
    if archive_is_empty(root) {
        return ArchiveProbe::Empty { path };
    }
    ArchiveProbe::PresentUnpinned {
        path,
        parquet: parquet_file_count(root),
    }
}

#[must_use]
pub fn a3_status() -> A3Status {
    match second_source_verify() {
        Ok(()) => A3Status::Verified,
        Err(_) => A3Status::Deferred,
    }
}

#[must_use]
pub fn pinned_range() -> Option<(u64, u64)> {
    PINNED_RANGE
}

#[cfg(test)]
mod tests {
    use super::{archive_is_empty, pinned_range, PINNED_RANGE};
    use std::fs;

    #[test]
    fn pin_is_none_until_a3() {
        assert!(PINNED_RANGE.is_none());
        assert!(pinned_range().is_none());
    }

    #[test]
    fn gitkeep_only_is_empty() {
        let dir = std::env::temp_dir().join(format!(
            "liq-16c-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".gitkeep"), []).unwrap();
        assert!(archive_is_empty(&dir));
        let _ = fs::remove_dir_all(&dir);
    }
}
