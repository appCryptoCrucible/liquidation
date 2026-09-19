//! Guard for the `sol!` proc-macro pin.
//!
//! `alloy-sol-types` is pinned to `=1.6.0` in `[workspace.dependencies]`, but
//! the proc-macro crates that *generate* code against it — `alloy-sol-macro`,
//! `alloy-sol-macro-expander`, `alloy-sol-macro-input` — are transitive and
//! carry no such pin. A bare `cargo update` floats them to 1.7.x, whose
//! expansion calls `abi_decode_returns_with_config`, which does not exist in
//! `alloy-sol-types` 1.6.0; the build then breaks inside `alloy-provider`
//! 2.4.2, far from the change that caused it.
//!
//! This asserts the lockfile directly so the failure lands on the update.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::Path;

/// Every `[[package]]` version recorded for `name`.
fn locked_versions(lock: &str, name: &str) -> Vec<String> {
    let want = format!("name = \"{name}\"");
    let mut out = Vec::new();
    let mut lines = lock.lines();
    while let Some(l) = lines.next() {
        if l.trim() == want {
            if let Some(v) = lines.next() {
                let v = v.trim();
                if let Some(rest) = v.strip_prefix("version = \"") {
                    out.push(rest.trim_end_matches('"').to_owned());
                }
            }
        }
    }
    out
}

/// Oracle: the build failure the 03A builder hit — `sol!` expansion from
/// `alloy-sol-macro-expander` 1.7.3 against `alloy-sol-types` 1.6.0. Negative:
/// the parser is proven to report a version that is *not* 1.6.0 rather than
/// silently finding nothing.
#[test]
fn sol_macro_crates_stay_on_1_6_0() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let lock = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));

    for name in [
        "alloy-sol-types",
        "alloy-sol-macro",
        "alloy-sol-macro-expander",
        "alloy-sol-macro-input",
    ] {
        let found = locked_versions(&lock, name);
        assert_eq!(
            found,
            vec!["1.6.0".to_owned()],
            "{name} must be locked at exactly 1.6.0 (found {found:?}). \
             `sol!` expansion from 1.7.x calls alloy-sol-types APIs that 1.6.0 \
             does not have and breaks alloy-provider 2.4.2. If you meant to \
             upgrade, move alloy-sol-types, alloy-provider and \
             alloy-rpc-types-eth together and update this test."
        );
    }

    let sample = "[[package]]\nname = \"alloy-sol-macro-input\"\nversion = \"1.7.3\"\n";
    assert_eq!(
        locked_versions(sample, "alloy-sol-macro-input"),
        vec!["1.7.3".to_owned()],
        "the lockfile parser must actually read versions"
    );
}
