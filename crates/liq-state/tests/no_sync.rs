//! GUIDE 02 acceptance: the store's definition contains no shared-ownership
//! pointer, lock or atomic — grep-asserted over every source file of the
//! crate (`src/`, not tests or benches). RUST-CONVENTIONS §1: the single
//! writer is the concurrency proof.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::Path;

/// The single-writer core (GUIDE 02 §7b). WAL/snapshot/drift are the
/// off-thread paths and are allowed `Arc`/`std::sync` (ArcSwap, channels).
const CORE: [&str; 6] = [
    "store.rs",
    "undo.rs",
    "view.rs",
    "interner.rs",
    "error.rs",
    "lib.rs",
];

/// The off-thread files (WP 02B), named so a *new* file cannot join them by
/// default: anything in `src/` that is in neither list fails the test, and
/// whoever adds it has to say which side of the thread boundary it is on.
const OFF_THREAD: [&str; 3] = ["wal.rs", "snapshot.rs", "drift.rs"];

const FORBIDDEN: [&str; 7] = [
    "Arc<",
    "Arc::",
    "Mutex",
    "RwLock",
    "atomic",
    "Atomic",
    "std::sync",
];

#[test]
fn src_has_no_arc_mutex_rwlock_or_atomic() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut checked = 0;
    for entry in fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        assert!(
            CORE.contains(&name) || OFF_THREAD.contains(&name),
            "{} is in neither CORE nor OFF_THREAD: classify it before adding it",
            path.display()
        );
        if CORE.contains(&name) {
            let text = fs::read_to_string(&path).unwrap();
            for token in FORBIDDEN {
                assert!(
                    !text.contains(token),
                    "{} contains `{token}`",
                    path.display()
                );
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 6, "store, undo, view, interner, error, lib");
}
