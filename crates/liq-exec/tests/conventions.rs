//! Structural checks: no public mempool, no tokio nonce mutex, JoinSet fan-out.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::fs;
use std::path::PathBuf;

fn src_files() -> Vec<(PathBuf, String)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    for name in [
        "submit.rs",
        "path.rs",
        "nonce.rs",
        "builders.rs",
        "fee.rs",
        "template.rs",
        "inclusion.rs",
        "error.rs",
        "lib.rs",
    ] {
        let p = root.join(name);
        if p.exists() {
            out.push((p.clone(), fs::read_to_string(&p).unwrap()));
        }
    }
    out
}

#[test]
fn no_public_mempool_path() {
    for (p, text) in src_files() {
        for needle in [
            "PublicMempool",
            "eth_sendRawTransaction",
            "eth_sendPrivateTransaction",
            "sendRawTransaction",
            "sendPrivateTransaction",
        ] {
            assert!(
                !text.contains(needle),
                "{} contains forbidden {needle}",
                p.display()
            );
        }
    }
}

#[test]
fn nonce_is_parking_lot_not_tokio_mutex() {
    let text =
        fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/nonce.rs")).unwrap();
    assert!(text.contains("parking_lot::Mutex"));
    assert!(!text.contains("tokio::sync::Mutex"));
}

#[test]
fn builder_fanout_uses_joinset() {
    let text = fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/submit.rs"))
        .unwrap();
    assert!(text.contains("JoinSet"), "fan-out must use JoinSet");
    // Sequential per-builder await of send would look like a for-loop await
    // on the same future kind without spawn. The send loop must spawn.
    assert!(
        text.contains("set.spawn"),
        "JoinSet must spawn, not await in sequence"
    );
}

#[test]
fn shipped_builders_toml_has_no_public_rpc() {
    let text = fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/builders.toml"),
    )
    .unwrap();
    let v: toml::Value = toml::from_str(&text).unwrap();
    let builders = v["builders"].as_array().unwrap();
    for b in builders {
        let ep = b["endpoint"].as_str().unwrap().to_ascii_lowercase();
        assert!(!ep.contains("infura"));
        assert!(!ep.contains("alchemy"));
        assert!(!ep.contains("8545"));
        assert!(!ep.contains("sendraw"));
    }
    let relay = v["mevshare"]["relay"].as_str().unwrap();
    assert_eq!(relay, "https://relay.flashbots.net");
    assert!(text.contains("rpc.beaverbuild.org"));
}

#[test]
fn no_unwrap_on_network_in_submit_modules() {
    for name in ["submit.rs", "path.rs"] {
        let text = fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join(name),
        )
        .unwrap();
        let code = text.split("#[cfg(test)]").next().unwrap();
        assert!(
            !code.contains(".unwrap()"),
            "{name} production code contains .unwrap()"
        );
        assert!(
            !code.contains(".expect("),
            "{name} production code contains .expect("
        );
    }
}
