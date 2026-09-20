//! Partitioned parquet + [`liq_protocol::Archive`] (replay, no RPC).

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alloy_primitives::{Address, B256};
use arrow::array::{Array, BooleanArray, StringArray, UInt16Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use liq_protocol::log::DecodedLog;
use liq_protocol::{Archive, ArchiveError, BlockNum, Result as ProtoResult};
use liq_types::LogFilter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;

use super::extract::ExtractError;

/// ~1 day of 12s blocks.
pub const DEFAULT_PARTITION_BLOCKS: u64 = 7_200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderRow {
    pub block: u64,
    pub hash: B256,
    pub timestamp: u64,
    pub superseded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventRow {
    pub block: u64,
    pub block_hash: B256,
    pub timestamp: u64,
    pub tx_hash: B256,
    pub tx_index: u32,
    pub log_index: u32,
    pub address: Address,
    pub topics: String,
    pub data_hex: String,
    pub superseded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedPrice {
    pub block: u64,
    pub block_hash: B256,
    pub timestamp: u64,
    pub tx: B256,
    pub feed: u16,
    pub aggregator: Address,
    pub price_ray: String,
    pub decimals: u16,
    pub superseded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReorgRow {
    pub at_block: u64,
    pub depth: u8,
    pub old_tip: B256,
    pub new_tip: B256,
}

#[must_use]
pub fn partition_bounds(block: u64, part: u64) -> (u64, u64) {
    let p = part.max(1);
    let q = block.checked_div(p).unwrap_or_default();
    let start = q.saturating_mul(p);
    let end = start.saturating_add(p.saturating_sub(1));
    (start, end)
}

fn part_name(kind: &str, from: u64, to: u64) -> String {
    format!("{kind}_{from}_{to}.parquet")
}

pub fn write_headers(
    dir: &Path,
    from: u64,
    to: u64,
    rows: &[HeaderRow],
) -> Result<(), ExtractError> {
    fs::create_dir_all(dir).map_err(|e| ExtractError::Io(e.to_string()))?;
    let schema = Arc::new(Schema::new(vec![
        Field::new("block", DataType::UInt64, false),
        Field::new("hash", DataType::Utf8, false),
        Field::new("timestamp", DataType::UInt64, false),
        Field::new("superseded", DataType::Boolean, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.block))),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| format!("{:#x}", r.hash)),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.timestamp),
            )),
            Arc::new(BooleanArray::from_iter(
                rows.iter().map(|r| Some(r.superseded)),
            )),
        ],
    )
    .map_err(|e| ExtractError::Parquet(e.to_string()))?;
    let path = dir.join(part_name("headers", from, to));
    write_batch(&path, schema, &batch)
}

pub(super) fn write_events(
    dir: &Path,
    from: u64,
    to: u64,
    rows: &[EventRow],
) -> Result<(), ExtractError> {
    fs::create_dir_all(dir).map_err(|e| ExtractError::Io(e.to_string()))?;
    let schema = Arc::new(Schema::new(vec![
        Field::new("block", DataType::UInt64, false),
        Field::new("block_hash", DataType::Utf8, false),
        Field::new("timestamp", DataType::UInt64, false),
        Field::new("tx_hash", DataType::Utf8, false),
        Field::new("tx_index", DataType::UInt64, false),
        Field::new("log_index", DataType::UInt64, false),
        Field::new("address", DataType::Utf8, false),
        Field::new("topics", DataType::Utf8, false),
        Field::new("data_hex", DataType::Utf8, false),
        Field::new("superseded", DataType::Boolean, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.block))),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| format!("{:#x}", r.block_hash)),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.timestamp),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| format!("{:#x}", r.tx_hash)),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| u64::from(r.tx_index)),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| u64::from(r.log_index)),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| format!("{:#x}", r.address)),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.topics.clone()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.data_hex.clone()),
            )),
            Arc::new(BooleanArray::from_iter(
                rows.iter().map(|r| Some(r.superseded)),
            )),
        ],
    )
    .map_err(|e| ExtractError::Parquet(e.to_string()))?;
    write_batch(&dir.join(part_name("events", from, to)), schema, &batch)
}

pub(super) fn write_prices(
    dir: &Path,
    from: u64,
    to: u64,
    rows: &[ArchivedPrice],
) -> Result<(), ExtractError> {
    fs::create_dir_all(dir).map_err(|e| ExtractError::Io(e.to_string()))?;
    let schema = Arc::new(Schema::new(vec![
        Field::new("block", DataType::UInt64, false),
        Field::new("block_hash", DataType::Utf8, false),
        Field::new("timestamp", DataType::UInt64, false),
        Field::new("tx", DataType::Utf8, false),
        Field::new("feed", DataType::UInt16, false),
        Field::new("aggregator", DataType::Utf8, false),
        Field::new("price_ray", DataType::Utf8, false),
        Field::new("decimals", DataType::UInt16, false),
        Field::new("superseded", DataType::Boolean, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.block))),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| format!("{:#x}", r.block_hash)),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| r.timestamp),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| format!("{:#x}", r.tx)),
            )),
            Arc::new(UInt16Array::from_iter_values(rows.iter().map(|r| r.feed))),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| format!("{:#x}", r.aggregator)),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.price_ray.clone()),
            )),
            Arc::new(UInt16Array::from_iter_values(
                rows.iter().map(|r| r.decimals),
            )),
            Arc::new(BooleanArray::from_iter(
                rows.iter().map(|r| Some(r.superseded)),
            )),
        ],
    )
    .map_err(|e| ExtractError::Parquet(e.to_string()))?;
    write_batch(&dir.join(part_name("prices", from, to)), schema, &batch)
}

pub(super) fn append_reorgs(dir: &Path, rows: &[ReorgRow]) -> Result<(), ExtractError> {
    if rows.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dir).map_err(|e| ExtractError::Io(e.to_string()))?;
    let path = dir.join("reorgs.parquet");
    let mut all = if path.exists() {
        read_reorgs(&path)?
    } else {
        Vec::new()
    };
    all.extend_from_slice(rows);
    let schema = Arc::new(Schema::new(vec![
        Field::new("at_block", DataType::UInt64, false),
        Field::new("depth", DataType::UInt16, false),
        Field::new("old_tip", DataType::Utf8, false),
        Field::new("new_tip", DataType::Utf8, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from_iter_values(
                all.iter().map(|r| r.at_block),
            )),
            Arc::new(UInt16Array::from_iter_values(
                all.iter().map(|r| u16::from(r.depth)),
            )),
            Arc::new(StringArray::from_iter_values(
                all.iter().map(|r| format!("{:#x}", r.old_tip)),
            )),
            Arc::new(StringArray::from_iter_values(
                all.iter().map(|r| format!("{:#x}", r.new_tip)),
            )),
        ],
    )
    .map_err(|e| ExtractError::Parquet(e.to_string()))?;
    write_batch(&path, schema, &batch)
}

fn write_batch(path: &Path, schema: Arc<Schema>, batch: &RecordBatch) -> Result<(), ExtractError> {
    let file = File::create(path).map_err(|e| ExtractError::Io(e.to_string()))?;
    let mut w = ArrowWriter::try_new(file, schema, None)
        .map_err(|e| ExtractError::Parquet(e.to_string()))?;
    w.write(batch)
        .map_err(|e| ExtractError::Parquet(e.to_string()))?;
    w.close()
        .map_err(|e| ExtractError::Parquet(e.to_string()))?;
    Ok(())
}

fn parse_b256(s: &str) -> Result<B256, ExtractError> {
    s.parse()
        .map_err(|_| ExtractError::Parquet(format!("bad hash {s}")))
}

fn parse_addr(s: &str) -> Result<Address, ExtractError> {
    s.parse()
        .map_err(|_| ExtractError::Parquet(format!("bad addr {s}")))
}

fn read_reorgs(path: &Path) -> Result<Vec<ReorgRow>, ExtractError> {
    let file = File::open(path).map_err(|e| ExtractError::Io(e.to_string()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| ExtractError::Parquet(e.to_string()))?
        .build()
        .map_err(|e| ExtractError::Parquet(e.to_string()))?;
    let mut out = Vec::new();
    for batch in reader {
        let b = batch.map_err(|e| ExtractError::Parquet(e.to_string()))?;
        let at = col_u64(&b, "at_block")?;
        let depth = col_u16(&b, "depth")?;
        let old = col_str(&b, "old_tip")?;
        let new = col_str(&b, "new_tip")?;
        let n = b.num_rows();
        for i in 0..n {
            let d = u8::try_from(u16_at(depth, i)?)
                .map_err(|_| ExtractError::Parquet("reorg depth".into()))?;
            out.push(ReorgRow {
                at_block: u64_at(at, i)?,
                depth: d,
                old_tip: parse_b256(str_at(old, i)?)?,
                new_tip: parse_b256(str_at(new, i)?)?,
            });
        }
    }
    Ok(out)
}

pub(super) fn read_headers(path: &Path) -> Result<Vec<HeaderRow>, ExtractError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path).map_err(|e| ExtractError::Io(e.to_string()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| ExtractError::Parquet(e.to_string()))?
        .build()
        .map_err(|e| ExtractError::Parquet(e.to_string()))?;
    let mut out = Vec::new();
    for batch in reader {
        let b = batch.map_err(|e| ExtractError::Parquet(e.to_string()))?;
        let block = col_u64(&b, "block")?;
        let hash = col_str(&b, "hash")?;
        let ts = col_u64(&b, "timestamp")?;
        let sup = col_bool(&b, "superseded")?;
        for i in 0..b.num_rows() {
            out.push(HeaderRow {
                block: u64_at(block, i)?,
                hash: parse_b256(str_at(hash, i)?)?,
                timestamp: u64_at(ts, i)?,
                superseded: bool_at(sup, i)?,
            });
        }
    }
    Ok(out)
}

pub(super) fn read_events(path: &Path) -> Result<Vec<EventRow>, ExtractError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path).map_err(|e| ExtractError::Io(e.to_string()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| ExtractError::Parquet(e.to_string()))?
        .build()
        .map_err(|e| ExtractError::Parquet(e.to_string()))?;
    let mut out = Vec::new();
    for batch in reader {
        let b = batch.map_err(|e| ExtractError::Parquet(e.to_string()))?;
        let block = col_u64(&b, "block")?;
        let bh = col_str(&b, "block_hash")?;
        let ts = col_u64(&b, "timestamp")?;
        let tx = col_str(&b, "tx_hash")?;
        let txi = col_u64(&b, "tx_index")?;
        let li = col_u64(&b, "log_index")?;
        let addr = col_str(&b, "address")?;
        let topics = col_str(&b, "topics")?;
        let data = col_str(&b, "data_hex")?;
        let sup = col_bool(&b, "superseded")?;
        for i in 0..b.num_rows() {
            let tx_index = u32::try_from(u64_at(txi, i)?)
                .map_err(|_| ExtractError::Parquet("tx_index overflow".into()))?;
            let log_index = u32::try_from(u64_at(li, i)?)
                .map_err(|_| ExtractError::Parquet("log_index overflow".into()))?;
            out.push(EventRow {
                block: u64_at(block, i)?,
                block_hash: parse_b256(str_at(bh, i)?)?,
                timestamp: u64_at(ts, i)?,
                tx_hash: parse_b256(str_at(tx, i)?)?,
                tx_index,
                log_index,
                address: parse_addr(str_at(addr, i)?)?,
                topics: str_at(topics, i)?.to_string(),
                data_hex: str_at(data, i)?.to_string(),
                superseded: bool_at(sup, i)?,
            });
        }
    }
    Ok(out)
}

fn u64_at(a: &UInt64Array, i: usize) -> Result<u64, ExtractError> {
    if i >= a.len() {
        return Err(ExtractError::Parquet("u64 oob".into()));
    }
    Ok(a.value(i))
}

fn u16_at(a: &UInt16Array, i: usize) -> Result<u16, ExtractError> {
    if i >= a.len() {
        return Err(ExtractError::Parquet("u16 oob".into()));
    }
    Ok(a.value(i))
}

fn str_at(a: &StringArray, i: usize) -> Result<&str, ExtractError> {
    if i >= a.len() {
        return Err(ExtractError::Parquet("str oob".into()));
    }
    Ok(a.value(i))
}

fn bool_at(a: &BooleanArray, i: usize) -> Result<bool, ExtractError> {
    if i >= a.len() {
        return Err(ExtractError::Parquet("bool oob".into()));
    }
    Ok(a.value(i))
}

pub(super) fn read_prices(path: &Path) -> Result<Vec<ArchivedPrice>, ExtractError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(path).map_err(|e| ExtractError::Io(e.to_string()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| ExtractError::Parquet(e.to_string()))?
        .build()
        .map_err(|e| ExtractError::Parquet(e.to_string()))?;
    let mut out = Vec::new();
    for batch in reader {
        let b = batch.map_err(|e| ExtractError::Parquet(e.to_string()))?;
        let block = col_u64(&b, "block")?;
        let bh = col_str(&b, "block_hash")?;
        let ts = col_u64(&b, "timestamp")?;
        let tx = col_str(&b, "tx")?;
        let feed = col_u16(&b, "feed")?;
        let agg = col_str(&b, "aggregator")?;
        let ray = col_str(&b, "price_ray")?;
        let dec = col_u16(&b, "decimals")?;
        let sup = col_bool(&b, "superseded")?;
        for i in 0..b.num_rows() {
            out.push(ArchivedPrice {
                block: u64_at(block, i)?,
                block_hash: parse_b256(str_at(bh, i)?)?,
                timestamp: u64_at(ts, i)?,
                tx: parse_b256(str_at(tx, i)?)?,
                feed: u16_at(feed, i)?,
                aggregator: parse_addr(str_at(agg, i)?)?,
                price_ray: str_at(ray, i)?.to_string(),
                decimals: u16_at(dec, i)?,
                superseded: bool_at(sup, i)?,
            });
        }
    }
    Ok(out)
}

fn col_u64<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a UInt64Array, ExtractError> {
    b.column_by_name(name)
        .ok_or_else(|| ExtractError::Parquet(format!("missing {name}")))?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| ExtractError::Parquet(format!("{name} type")))
}

fn col_u16<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a UInt16Array, ExtractError> {
    b.column_by_name(name)
        .ok_or_else(|| ExtractError::Parquet(format!("missing {name}")))?
        .as_any()
        .downcast_ref::<UInt16Array>()
        .ok_or_else(|| ExtractError::Parquet(format!("{name} type")))
}

fn col_str<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a StringArray, ExtractError> {
    b.column_by_name(name)
        .ok_or_else(|| ExtractError::Parquet(format!("missing {name}")))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| ExtractError::Parquet(format!("{name} type")))
}

fn col_bool<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a BooleanArray, ExtractError> {
    b.column_by_name(name)
        .ok_or_else(|| ExtractError::Parquet(format!("missing {name}")))?
        .as_any()
        .downcast_ref::<BooleanArray>()
        .ok_or_else(|| ExtractError::Parquet(format!("{name} type")))
}

/// Mark headers whose hash is `old` at `block`; keep the row (GUIDE 05 reorg).
pub fn apply_reorg_headers(
    rows: &mut [HeaderRow],
    block: u64,
    old: B256,
    new: B256,
) -> Option<ReorgRow> {
    let mut hit = false;
    for r in rows.iter_mut() {
        if r.block == block && r.hash == old && !r.superseded {
            r.superseded = true;
            hit = true;
        }
    }
    hit.then_some(ReorgRow {
        at_block: block,
        depth: 1,
        old_tip: old,
        new_tip: new,
    })
}

pub(super) fn mark_events_reorg(rows: &mut [EventRow], block: u64, old: B256) {
    for r in rows.iter_mut() {
        if r.block == block && r.block_hash == old {
            r.superseded = true;
        }
    }
}

pub(super) fn mark_prices_reorg(rows: &mut [ArchivedPrice], block: u64, old: B256) {
    for r in rows.iter_mut() {
        if r.block == block && r.block_hash == old {
            r.superseded = true;
        }
    }
}

/// Every block in `from..=to` has a non-superseded header. Gaps are
/// [`ArchiveError::Truncated`], never a silent empty stream.
pub fn headers_complete(rows: &[HeaderRow], from: u64, to: u64) -> Result<(), ArchiveError> {
    if from > to {
        return Err(ArchiveError::Truncated { from });
    }
    let mut n = from;
    loop {
        let ok = rows.iter().any(|r| r.block == n && !r.superseded);
        if !ok {
            return Err(ArchiveError::Truncated { from: n });
        }
        if n == to {
            return Ok(());
        }
        n = n
            .checked_add(1)
            .ok_or(ArchiveError::Truncated { from: n })?;
    }
}

fn hex_to_bytes(s: &str) -> Result<Vec<u8>, ExtractError> {
    let t = s.strip_prefix("0x").unwrap_or(s);
    if !t.len().is_multiple_of(2) {
        return Err(ExtractError::Parquet("odd hex".into()));
    }
    let mut out = Vec::with_capacity(t.len() / 2);
    let bytes = t.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = bytes
            .get(i)
            .copied()
            .ok_or_else(|| ExtractError::Parquet("hex".into()))?;
        let lo = bytes
            .get(i.saturating_add(1))
            .copied()
            .ok_or_else(|| ExtractError::Parquet("hex".into()))?;
        let v = hex_nibble(hi)?
            .saturating_mul(16)
            .saturating_add(hex_nibble(lo)?);
        out.push(v);
        i = i.saturating_add(2);
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Result<u8, ExtractError> {
    match c {
        b'0'..=b'9' => Ok(c.saturating_sub(b'0')),
        b'a'..=b'f' => Ok(c.saturating_sub(b'a').saturating_add(10)),
        b'A'..=b'F' => Ok(c.saturating_sub(b'A').saturating_add(10)),
        _ => Err(ExtractError::Parquet("hex nibble".into())),
    }
}

fn parse_topics(s: &str) -> Result<Vec<B256>, ExtractError> {
    if s.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for part in s.split(',') {
        out.push(parse_b256(part.trim())?);
    }
    Ok(out)
}

/// Disk archive. Completeness is the header set, not log count.
pub struct ParquetArchive {
    root: PathBuf,
}

impl ParquetArchive {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn header_files_covering(&self, from: u64, to: u64) -> Result<Vec<HeaderRow>, ExtractError> {
        let dir = self.root.join("headers");
        if !dir.is_dir() {
            return Err(ExtractError::Truncated { from, to });
        }
        let mut rows = Vec::new();
        let rd = fs::read_dir(&dir).map_err(|e| ExtractError::Io(e.to_string()))?;
        for ent in rd {
            let ent = ent.map_err(|e| ExtractError::Io(e.to_string()))?;
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("parquet") {
                continue;
            }
            rows.extend(read_headers(&p)?);
        }
        Ok(rows)
    }
}

impl Archive for ParquetArchive {
    fn logs(
        &self,
        filters: &[LogFilter],
        from: BlockNum,
        to: BlockNum,
        visit: &mut dyn FnMut(&DecodedLog<'_>) -> ProtoResult<()>,
    ) -> ProtoResult<()> {
        let headers = match self.header_files_covering(from, to) {
            Ok(h) => h,
            Err(ExtractError::Truncated { from, .. }) => {
                return Err(ArchiveError::Truncated { from }.into());
            }
            Err(_) => return Err(ArchiveError::Unavailable.into()),
        };
        headers_complete(&headers, from, to)?;
        let dir = self.root.join("events");
        if !dir.is_dir() {
            return Ok(());
        }
        let rd = fs::read_dir(&dir).map_err(|_| ArchiveError::Unavailable)?;
        let mut events = Vec::new();
        for ent in rd {
            let ent = ent.map_err(|_| ArchiveError::Unavailable)?;
            events.extend(
                read_events(&ent.path()).map_err(|_| ArchiveError::Malformed { block: from })?,
            );
        }
        events.sort_by_key(|e| (e.block, e.tx_index, e.log_index));
        for ev in &events {
            if ev.superseded || ev.block < from || ev.block > to {
                continue;
            }
            if !filters.is_empty() {
                let topics = parse_topics(&ev.topics)
                    .map_err(|_| ArchiveError::Malformed { block: ev.block })?;
                let t0 = topics.first().copied();
                let hit = filters
                    .iter()
                    .any(|f| f.address == ev.address && t0.is_some_and(|t| t == f.topic0));
                if !hit {
                    continue;
                }
            }
            let topics = parse_topics(&ev.topics)
                .map_err(|_| ArchiveError::Malformed { block: ev.block })?;
            let data = hex_to_bytes(&ev.data_hex)
                .map_err(|_| ArchiveError::Malformed { block: ev.block })?;
            let log = DecodedLog {
                address: ev.address,
                topics: &topics,
                data: &data,
                block: ev.block,
                timestamp: ev.timestamp,
            };
            visit(&log)?;
        }
        Ok(())
    }
}

impl From<ArchiveError> for ExtractError {
    fn from(e: ArchiveError) -> Self {
        match e {
            ArchiveError::Truncated { from } => Self::Truncated { from, to: from },
            ArchiveError::Unavailable => Self::Rpc("archive unavailable".into()),
            ArchiveError::Malformed { block } => Self::Parquet(format!("malformed {block}")),
        }
    }
}

pub(super) fn headers_path(root: &Path, from: u64, to: u64) -> PathBuf {
    root.join("headers").join(part_name("headers", from, to))
}

pub(super) fn events_path(root: &Path, from: u64, to: u64) -> PathBuf {
    root.join("events").join(part_name("events", from, to))
}

pub(super) fn prices_path(root: &Path, from: u64, to: u64) -> PathBuf {
    root.join("prices").join(part_name("prices", from, to))
}

pub(super) fn actual_path(root: &Path, from: u64, to: u64) -> PathBuf {
    root.join("actual").join(part_name("actual", from, to))
}
