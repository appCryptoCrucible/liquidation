//! Tests that need the crate root (stage spans, digest, preflight).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use alloy_primitives::B256;
use liq_obs::{all_stages, digest_paths, install_config_version, section4_report, StageLayer};
use liq_types::{stage, TraceId};
use rusqlite::Connection;
use tracing_subscriber::prelude::*;

#[test]
fn stage_spans_all_boundaries_with_config_version() {
    install_config_version(B256::ZERO);
    let _g = tracing_subscriber::registry()
        .with(StageLayer)
        .set_default();
    let t = TraceId::from_raw(7);
    for s in all_stages() {
        stage(t, s);
    }
    assert_eq!(liq_obs::current_config_version(), Some(B256::ZERO));
}

#[test]
fn digest_and_preflight_from_real_files() {
    let dir = std::env::temp_dir().join(format!("liq-obs-digest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sqlite = dir.join("w.sqlite");
    let jsonl = dir.join("o.jsonl");
    let db = Connection::open(&sqlite).unwrap();
    db.execute_batch(
        "CREATE TABLE liquidations (
            block INTEGER NOT NULL,
            tx_index INTEGER NOT NULL,
            log_index INTEGER NOT NULL,
            family TEXT NOT NULL,
            instance TEXT NOT NULL,
            block_hash TEXT NOT NULL,
            tx_hash TEXT NOT NULL,
            payload TEXT NOT NULL,
            PRIMARY KEY (block, tx_index, log_index)
        );",
    )
    .unwrap();
    db.execute(
        "INSERT INTO liquidations VALUES (1,0,0,'aave-v3','core','0x00','0x00','{}')",
        [],
    )
    .unwrap();
    std::fs::write(
        &jsonl,
        "{\"outcome\":\"Declined\"}\n{\"outcome\":\"NotTracked\"}\n{\"halt\":true}\n",
    )
    .unwrap();
    let d = digest_paths(&sqlite, &jsonl).unwrap();
    assert_eq!(d.sqlite_rows, 1);
    assert_eq!(d.not_tracked, 1);
    assert_eq!(d.declined, 1);
    assert_eq!(d.halt_lines, 1);
    let pf = section4_report(&sqlite, &jsonl).unwrap();
    assert_eq!(pf.fields.len(), 6);
    let _ = std::fs::remove_dir_all(&dir);
}
