//! GUIDE 02 acceptance: the store's definition contains no shared-ownership
//! pointer, lock or atomic — grep-asserted over every source file of the
//! crate (`src/`, not tests or benches). RUST-CONVENTIONS §1: the single
//! writer is the concurrency proof.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::Path;

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
        if path.extension().is_some_and(|e| e == "rs") {
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
