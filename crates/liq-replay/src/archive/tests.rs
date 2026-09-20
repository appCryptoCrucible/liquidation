use super::store::{
    events_path, headers_path, load_covering_parquet, mark_events_reorg, parse_partition_range,
    read_events, read_headers, read_prices, write_events, write_prices,
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

fn tmp_archive(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "liq-05b-d1-{tag}-{}-{}",
        std::process::id(),
        tag.len()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn header_at(block: u64, byte: u8) -> HeaderRow {
    HeaderRow {
        block,
        hash: B256::repeat_byte(byte),
        timestamp: block,
        superseded: false,
    }
}

fn event_at(block: u64, byte: u8) -> EventRow {
    EventRow {
        block,
        block_hash: B256::repeat_byte(byte),
        timestamp: block,
        tx_hash: B256::repeat_byte(byte),
        tx_index: 0,
        log_index: 0,
        address: Address::repeat_byte(byte),
        topics: format!("{:#x}", B256::repeat_byte(0xaa)),
        data_hex: "0xab".into(),
        superseded: false,
    }
}

fn price_at(block: u64, byte: u8) -> ArchivedPrice {
    ArchivedPrice {
        block,
        block_hash: B256::repeat_byte(byte),
        timestamp: block,
        tx: B256::repeat_byte(byte),
        feed: 1,
        aggregator: Address::repeat_byte(byte),
        price_ray: "1".into(),
        decimals: 8,
        superseded: false,
    }
}

fn poison_parquet(dir: &std::path::Path, kind: &str, from: u64, to: u64) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(format!("{kind}_{from}_{to}.parquet")),
        b"not-a-parquet-body",
    )
    .unwrap();
}

#[test]
fn parse_partition_range_fail_closed() {
    assert_eq!(
        parse_partition_range("headers", "headers_100_199.parquet").unwrap(),
        (100, 199)
    );
    assert_eq!(
        parse_partition_range("events", "events_0_0.parquet").unwrap(),
        (0, 0)
    );
    assert_eq!(
        parse_partition_range("prices", "prices_8_8.parquet").unwrap(),
        (8, 8)
    );
    assert!(parse_partition_range("headers", "headers_nope.parquet").is_err());
    assert!(parse_partition_range("headers", "events_1_2.parquet").is_err());
    assert!(parse_partition_range("headers", "headers_1_2_3.parquet").is_err());
    assert!(parse_partition_range("headers", "headers_2_1.parquet").is_err());
    assert!(parse_partition_range("headers", "headers_1.parquet").is_err());
    assert!(parse_partition_range("headers", "foo.parquet").is_err());
}

/// Out-of-range partitions are poison parquet. The pre-fix all-files reader
/// would open them and fail; range skip must not.
#[test]
fn logs_does_not_read_partitions_outside_requested_range() {
    let dir = tmp_archive("skip");
    std::fs::create_dir_all(dir.join("headers")).unwrap();
    std::fs::create_dir_all(dir.join("events")).unwrap();
    std::fs::create_dir_all(dir.join("prices")).unwrap();

    write_headers(&dir.join("headers"), 10, 10, &[header_at(10, 0x11)]).unwrap();
    write_events(&dir.join("events"), 10, 10, &[event_at(10, 0x11)]).unwrap();
    write_prices(&dir.join("prices"), 10, 10, &[price_at(10, 0x11)]).unwrap();

    poison_parquet(&dir.join("headers"), "headers", 1000, 1999);
    poison_parquet(&dir.join("events"), "events", 1000, 1999);
    poison_parquet(&dir.join("prices"), "prices", 1000, 1999);

    let arch = ParquetArchive::new(dir.clone());
    let mut seen = 0u64;
    arch.logs(&[], 10, 10, &mut |log: &DecodedLog<'_>| {
        seen = seen.saturating_add(1);
        assert_eq!(log.block, 10);
        assert_eq!(log.address, Address::repeat_byte(0x11));
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, 1);

    let prices = load_covering_parquet(&dir.join("prices"), "prices", 10, 10, read_prices).unwrap();
    assert_eq!(prices.len(), 1);
    assert_eq!(prices[0].block, 10);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A parseable covering file plus an unparseable `.parquet` name must error,
/// not silently drop the bad name (which would skip a whole archive).
#[test]
fn unparseable_partition_name_fails_closed() {
    let dir = tmp_archive("badname");
    std::fs::create_dir_all(dir.join("headers")).unwrap();
    write_headers(&dir.join("headers"), 10, 10, &[header_at(10, 0x22)]).unwrap();
    std::fs::copy(
        headers_path(&dir, 10, 10),
        dir.join("headers").join("headers_nope.parquet"),
    )
    .unwrap();

    let arch = ParquetArchive::new(dir.clone());
    let err = arch.logs(&[], 10, 10, &mut |_| Ok(())).unwrap_err();
    assert!(matches!(
        err,
        liq_protocol::ProtocolError::Archive(ArchiveError::Unavailable)
            | liq_protocol::ProtocolError::Archive(ArchiveError::Malformed { block: 10 })
    ));
    let _ = std::fs::remove_dir_all(&dir);
}
