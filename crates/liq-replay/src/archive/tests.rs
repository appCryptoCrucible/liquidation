use super::store::{
    events_path, headers_path, mark_events_reorg, read_events, read_headers, write_events,
};
use super::*;
use alloy_primitives::{Address, B256};
use liq_protocol::{Archive, ArchiveError, DecodedLog};
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

#[test]
fn c3_filter_matches_committed_count() {
    let addrs = load_c3_addresses(&workspace_root()).unwrap();
    assert_eq!(addrs.len(), C3_EXPECTED);
    assert!(addrs.iter().all(|a| *a != Address::ZERO));
}

#[test]
fn parse_rejects_garbage_line() {
    let t = "[prune.segments.receipts_log_filter]\nnot-an-address = 1\n";
    assert!(parse_receipts_log_filter(t).is_err());
}

#[test]
fn partition_aligns() {
    assert_eq!(partition_bounds(0, 100), (0, 99));
    assert_eq!(partition_bounds(100, 100), (100, 199));
    assert_eq!(partition_bounds(250, 100), (200, 299));
}

#[test]
fn headers_gap_is_truncated() {
    let rows = [HeaderRow {
        block: 1,
        hash: B256::ZERO,
        timestamp: 1,
        superseded: false,
    }];
    assert_eq!(
        headers_complete(&rows, 1, 2),
        Err(ArchiveError::Truncated { from: 2 })
    );
    assert!(headers_complete(&rows, 1, 1).is_ok());
}

#[test]
fn reorg_marks_old_hash() {
    let old = B256::from([1u8; 32]);
    let new = B256::from([2u8; 32]);
    let mut rows = [HeaderRow {
        block: 9,
        hash: old,
        timestamp: 1,
        superseded: false,
    }];
    let r = apply_reorg_headers(&mut rows, 9, old, new).unwrap();
    assert!(rows[0].superseded);
    assert_eq!(r.old_tip, old);
    assert_eq!(r.new_tip, new);
    assert_eq!(r.depth, 1);
}

#[test]
fn reorg_marks_events_by_block_hash() {
    let old = B256::from([7u8; 32]);
    let mut ev = [EventRow {
        block: 4,
        block_hash: old,
        timestamp: 1,
        tx_hash: B256::from([8u8; 32]),
        tx_index: 0,
        log_index: 0,
        address: Address::ZERO,
        topics: String::new(),
        data_hex: "0x".into(),
        superseded: false,
    }];
    mark_events_reorg(&mut ev, 4, old);
    assert!(ev[0].superseded);
}

#[test]
fn parquet_headers_roundtrip_and_archive_truncated_without_files() {
    let dir = std::env::temp_dir().join(format!("liq-05b-h-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("headers")).unwrap();
    let h = HeaderRow {
        block: 5,
        hash: B256::from([3u8; 32]),
        timestamp: 10,
        superseded: false,
    };
    write_headers(&dir.join("headers"), 5, 5, std::slice::from_ref(&h)).unwrap();
    let back = read_headers(&headers_path(&dir, 5, 5)).unwrap();
    assert_eq!(back, vec![h]);

    let empty = ParquetArchive::new(dir.join("missing"));
    let err = empty.logs(&[], 1, 2, &mut |_| Ok(())).unwrap_err();
    assert!(matches!(
        err,
        liq_protocol::ProtocolError::Archive(ArchiveError::Truncated { from: 1 })
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parquet_events_visit_skips_superseded() {
    let dir = std::env::temp_dir().join(format!("liq-05b-e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("headers")).unwrap();
    std::fs::create_dir_all(dir.join("events")).unwrap();
    let hash = B256::from([9u8; 32]);
    write_headers(
        &dir.join("headers"),
        8,
        8,
        &[HeaderRow {
            block: 8,
            hash,
            timestamp: 100,
            superseded: false,
        }],
    )
    .unwrap();
    let live = EventRow {
        block: 8,
        block_hash: hash,
        timestamp: 100,
        tx_hash: B256::from([1u8; 32]),
        tx_index: 0,
        log_index: 0,
        address: Address::repeat_byte(0x11),
        topics: format!("{:#x}", B256::repeat_byte(0xaa)),
        data_hex: "0xab".into(),
        superseded: false,
    };
    let dead = EventRow {
        superseded: true,
        tx_hash: B256::from([2u8; 32]),
        log_index: 1,
        ..live.clone()
    };
    write_events(&dir.join("events"), 8, 8, &[dead, live.clone()]).unwrap();
    let back = read_events(&events_path(&dir, 8, 8)).unwrap();
    assert_eq!(back.len(), 2);

    let arch = ParquetArchive::new(dir.clone());
    let mut seen = 0usize;
    arch.logs(&[], 8, 8, &mut |log: &DecodedLog<'_>| {
        seen = seen.saturating_add(1);
        assert_eq!(log.address, live.address);
        assert_eq!(log.block, 8);
        assert_eq!(log.data, &[0xab]);
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rpc_url_refused_when_unset() {
    let prev = std::env::var("LIQ_ARCHIVE_RPC").ok();
    std::env::remove_var("LIQ_ARCHIVE_RPC");
    assert!(matches!(archive_rpc_url(), Err(ExtractError::NoRpc)));
    if let Some(v) = prev {
        std::env::set_var("LIQ_ARCHIVE_RPC", v);
    }
}

#[test]
fn a3_second_source_is_deferred() {
    assert!(matches!(
        second_source_verify(),
        Err(ExtractError::A3Deferred)
    ));
}
