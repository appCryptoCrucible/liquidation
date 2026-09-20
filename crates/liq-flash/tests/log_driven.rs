//! GUIDE-07: zero RPC polling, five files, no Balancer.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects
)]

use std::fs;
use std::path::Path;

#[test]
fn src_has_no_rpc_surface() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = 0;
    walk(&src, &mut files, &|text, path| {
        for needle in ["alloy_provider", "alloy-provider", "RpcPoll", "reqwest::"] {
            assert!(
                !text.contains(needle),
                "{} contains `{needle}` (oracle: GUIDE-07 / D05 — log-driven)",
                path.display()
            );
        }
        for needle in ["receiveFlashLoan", "BalancerReceive"] {
            assert!(
                !text.contains(needle),
                "{} contains `{needle}` (oracle: D08/D09 — fifth arena is Sky DSS)",
                path.display()
            );
        }
        // Code only: 07A's module doc names `Mutex` to explain why it is absent.
        let code: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        for needle in ["Mutex", "RwLock", "LazyLock", "OnceLock"] {
            assert!(
                !code.contains(needle),
                "{} contains `{needle}` (oracle: GUIDE-07 §4b / GUIDE-08 — single writer, ArcSwap, no locks)",
                path.display()
            );
        }
    });
    assert!(
        files >= 11,
        "expected lib + index + eligibility + select + cascade + sources/{{mod,aave,univ3,univ4,morpho,sky_dss}}"
    );
}

#[test]
fn adding_a_source_is_one_file() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/sources");
    let mut impls = Vec::new();
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap();
        if name == "mod.rs" || path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        impls.push(name.to_string());
    }
    impls.sort();
    assert_eq!(
        impls,
        ["aave.rs", "morpho.rs", "sky_dss.rs", "univ3.rs", "univ4.rs"],
        "oracle: GUIDE-07 — five source files, adding one is one file"
    );
}

fn walk(dir: &Path, files: &mut usize, check: &impl Fn(&str, &Path)) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(&path, files, check);
            continue;
        }
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        *files += 1;
        let text = fs::read_to_string(&path).unwrap();
        check(&text, &path);
    }
}
