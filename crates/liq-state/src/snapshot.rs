//! On-disk snapshot and the published in-memory view (GUIDE 02 §7, §7b).
//!
//! **On disk.** A 64-byte header plus the columns, `stride`, `n_pos`,
//! `market_index` and the `writes` counter (02A carry-forward). The file is
//! a flat Pod dump of that layout (the shape `memmap2` would map).
//!
//! **In memory.** [`StateStore::snapshot`] copies columns into `Arc<[T]>`
//! (the store itself cannot hold `Arc`; GUIDE 02 §7b's share-the-buffer
//! alternative is blocked by the no-sync acceptance on `StateStore`).
//! The copy is **not free**: measured 7.8 ms for 50k positions / 12 MB of
//! columns (release, 02B review). GUIDE 02 §7b's sanctioned escape is the
//! other one — publish every N blocks, not every block — because a per-block
//! publish on the writer thread would not fit 03B's p99 block→consistent
//! budget of 500 µs. Whoever wires the publish cadence owns that choice.
//! [`Shared`] is built once at startup with [`Shared::leak`] (`Box::leak`,
//! never `LazyLock`).
//!
//! **Load path (D59).** `fs::read` of the same 64-byte-aligned Pod layout a
//! map would expose; mmap was evaluated and declined. CRC is `crc32fast`
//! (IEEE / ISO-HDLC, same check vector as the bit-serial loop it replaced).
//! `forbid(unsafe_code)` stays (D58).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use alloy_primitives::Address;
use arc_swap::ArcSwap;
use bytemuck::{Pod, Zeroable};
use liq_protocol::{AssetMask, BlockNum, MarketRow, PositionExtraRepr, PositionRef, Timestamp};
use liq_types::{MarketId, PositionId, PositionKey, ProtocolId};

use crate::error::StateError;
use crate::interner::{PosEntry, PositionTable};
use crate::store::{market_idx, Market, StateStore, StoreConfig, LINE_CELLS};

const MAGIC: [u8; 8] = *b"LIQSNAP1";
/// 2: 256-byte `MarketRow` and the per-slot `slot_extra` column (04A).
/// A version-1 file is refused (`SnapshotError::Version`), never reinterpreted.
const VERSION: u32 = 2;
const HEADER: usize = 64;
const ALIGN: usize = 64;

/// IEEE CRC-32 (ISO-HDLC) step: consumes `data` into a running `crc`
/// without the final xor. `crc32fast` — same polynomial, seed, init, and
/// final-xor as the bit-serial loop it replaced. Oracle: ITU-T V.42 check
/// of `"123456789"` is `0xCBF4_3926` via [`crc32`].
pub(crate) fn crc32_feed(crc: u32, data: &[u8]) -> u32 {
    // `crc` is the non-finalized running state (`0xFFFF_FFFF` at start).
    // `Hasher::new_with_initial` takes a finalized CRC (`Hasher::new` ≡
    // `new_with_initial(0)`), so invert to resume.
    let mut hasher = crc32fast::Hasher::new_with_initial(!crc);
    hasher.update(data);
    !hasher.finalize()
}

/// IEEE CRC-32 (ISO-HDLC) of a single buffer. WAL frames use this too.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

/// Failure loading or writing a snapshot. Off the hot path; may carry I/O.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("snapshot magic mismatch")]
    Magic,
    #[error("unsupported snapshot version {0}")]
    Version(u32),
    #[error("snapshot CRC mismatch")]
    Crc,
    #[error("snapshot truncated")]
    Truncated,
    #[error("snapshot layout does not match the store")]
    Layout,
    #[error(transparent)]
    State(#[from] StateError),
}

/// One interned key plus its per-market local index.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SnapKey {
    pub key: PositionKey,
    pub local: u32,
}

/// One market's captured columns. `stride`/`n_pos` are first-class: the
/// columns alone do not determine the layout (02A carry-forward).
#[derive(Clone, Debug)]
pub struct SnapMarket {
    pub stride: usize,
    pub n_pos: u32,
    pub rows: Arc<[MarketRow]>,
    pub supply: Arc<[u128]>,
    pub debt: Arc<[u128]>,
    /// Per-slot adapter cells, same stride as `supply`/`debt`.
    pub slot_extra: Arc<[PositionExtraRepr]>,
}

/// Immutable store view: cold-start artifact and the `ArcSwap` payload.
#[derive(Clone, Debug)]
pub struct StoreSnapshot {
    pub tip: BlockNum,
    pub writes: u64,
    pub config: Arc<[AssetMask]>,
    pub extra: Arc<[PositionExtraRepr]>,
    pub keys: Arc<[SnapKey]>,
    pub market_index: Arc<[u16]>,
    pub markets: Arc<[SnapMarket]>,
}

impl StoreSnapshot {
    /// Empty view for [`Shared::leak`] at startup, before the first store
    /// snapshot exists.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            tip: 0,
            writes: 0,
            config: Vec::new().into(),
            extra: Vec::new().into(),
            keys: Vec::new().into(),
            market_index: Vec::new().into(),
            markets: Vec::new().into(),
        }
    }

    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Borrowed position at `timestamp`, allocation-free.
    pub fn position(
        &self,
        id: PositionId,
        timestamp: Timestamp,
    ) -> Result<PositionRef<'_>, StateError> {
        let i = id.0 as usize;
        let config = *self.config.get(i).ok_or(StateError::UnknownPosition(id))?;
        let key = self.keys.get(i).ok_or(StateError::UnknownPosition(id))?;
        let extra = self.extra.get(i).ok_or(StateError::UnknownPosition(id))?;
        let mi = market_idx(&self.market_index, key.key.market)
            .ok_or(StateError::UnknownMarket(key.key.market))?;
        let m = self
            .markets
            .get(mi)
            .ok_or(StateError::UnknownMarket(key.key.market))?;
        let start = (key.local as usize)
            .checked_mul(m.stride)
            .ok_or(StateError::Inconsistent)?;
        let end = start
            .checked_add(m.rows.len())
            .ok_or(StateError::Inconsistent)?;
        let supply = m.supply.get(start..end).ok_or(StateError::Inconsistent)?;
        let debt = m.debt.get(start..end).ok_or(StateError::Inconsistent)?;
        let slot_extra = m
            .slot_extra
            .get(start..end)
            .ok_or(StateError::Inconsistent)?;
        Ok(PositionRef {
            id,
            key: &key.key,
            config,
            supply,
            debt,
            extra,
            slot_extra,
            markets: &m.rows,
            timestamp,
        })
    }

    /// Capture the live store. Copies columns into `Arc<[T]>` (see module docs).
    pub fn from_store(store: &StateStore) -> Self {
        let keys: Vec<SnapKey> = store
            .positions
            .entries
            .iter()
            .map(|e| SnapKey {
                key: e.key,
                local: e.local,
            })
            .collect();
        let markets: Vec<SnapMarket> = store
            .markets
            .iter()
            .map(|m| SnapMarket {
                stride: m.stride,
                n_pos: m.n_pos,
                rows: m.rows.clone().into(),
                supply: m.supply.clone().into(),
                debt: m.debt.clone().into(),
                slot_extra: m.slot_extra.clone().into(),
            })
            .collect();
        Self {
            tip: store.tip(),
            writes: store.writes(),
            config: store.config.clone().into(),
            extra: store.extra.clone().into(),
            keys: keys.into(),
            market_index: store.market_index.clone().into(),
            markets: markets.into(),
        }
    }

    /// Rebuild a store at this tip with `depth == 0` (`cfg.base` is ignored;
    /// the snapshot tip is the restored floor).
    pub fn into_store(&self, capacity: StoreConfig) -> Result<StateStore, SnapshotError> {
        if self.config.len() != self.extra.len() || self.config.len() != self.keys.len() {
            return Err(SnapshotError::Layout);
        }
        let entries: Vec<PosEntry> = self
            .keys
            .iter()
            .map(|k| PosEntry {
                key: k.key,
                local: k.local,
            })
            .collect();
        let positions = PositionTable::from_entries(entries)?;
        let mut markets = Vec::with_capacity(self.markets.len());
        for m in self.markets.iter() {
            if m.stride % LINE_CELLS != 0 && m.stride != 0 {
                return Err(SnapshotError::Layout);
            }
            if m.rows.len() > m.stride {
                return Err(SnapshotError::Layout);
            }
            let cells = (m.n_pos as usize)
                .checked_mul(m.stride)
                .ok_or(SnapshotError::Layout)?;
            if m.supply.len() != cells || m.debt.len() != cells || m.slot_extra.len() != cells {
                return Err(SnapshotError::Layout);
            }
            markets.push(Market {
                rows: m.rows.as_ref().to_vec(),
                stride: m.stride,
                n_pos: m.n_pos,
                supply: m.supply.as_ref().to_vec(),
                debt: m.debt.as_ref().to_vec(),
                slot_extra: m.slot_extra.as_ref().to_vec(),
            });
        }
        let cfg = StoreConfig {
            base: self.tip,
            positions: capacity.positions,
            markets: capacity.markets,
            undo: capacity.undo,
        };
        Ok(StateStore::from_captured(
            cfg,
            self.writes,
            self.config.as_ref().to_vec(),
            positions,
            self.extra.as_ref().to_vec(),
            markets,
            self.market_index.as_ref().to_vec(),
        )?)
    }

    /// Write the temp file, fsync it, then rename over `path` in one step.
    /// `fs::rename` replaces an existing destination on both Unix (`rename`)
    /// and Windows (`MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`), so the
    /// previous snapshot is never unlinked first: a crash mid-write leaves
    /// either the old snapshot or the new one, never neither. The containing
    /// directory is not fsynced (no portable API); the WAL is what closes
    /// that window. Not on the hot path.
    pub fn write_to(&self, path: &Path) -> Result<(), SnapshotError> {
        let buf = encode(self)?;
        let tmp = tmp_path(path);
        {
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(&buf)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Cold-start load of the mmap-shaped dump via `fs::read` (D59).
    pub fn load(path: &Path) -> Result<Self, SnapshotError> {
        let bytes = fs::read(path)?;
        if bytes.len() < HEADER {
            return Err(SnapshotError::Truncated);
        }
        decode(&bytes)
    }
}

impl StateStore {
    /// Cheap-to-publish owned view of the columns (GUIDE 02 §7b).
    #[must_use]
    pub fn snapshot(&self) -> StoreSnapshot {
        StoreSnapshot::from_store(self)
    }
}

/// `Shared.snapshot: ArcSwap<StoreSnapshot>` — constructed once, leaked
/// (`Box::leak`), never `LazyLock` (GUIDE 02 §7b, RUST-CONVENTIONS §3.3).
pub struct Shared {
    pub snapshot: ArcSwap<StoreSnapshot>,
}

impl Shared {
    /// Startup only. The allocation lives for the process; every later
    /// access is a field offset.
    #[must_use]
    pub fn leak(initial: StoreSnapshot) -> &'static Self {
        Box::leak(Box::new(Self {
            snapshot: ArcSwap::from_pointee(initial),
        }))
    }

    /// Writer thread, end of block: swap in a new view. The previous
    /// `Arc` is released; leaking the `Shared` itself is the long-running
    /// process trade (GUIDE 02 §7b).
    pub fn publish(&self, snap: StoreSnapshot) {
        self.snapshot.store(Arc::new(snap));
    }
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct KeyRec {
    protocol: u16,
    _pad: u16,
    market: u32,
    user: [u8; 20],
    local: u32,
}

const _: () = assert!(core::mem::size_of::<KeyRec>() == 32);

fn tmp_path(path: &Path) -> std::path::PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".tmp");
    std::path::PathBuf::from(os)
}

fn pad_to(buf: &mut Vec<u8>, align: usize) {
    let rem = match buf.len().checked_rem(align) {
        Some(r) => r,
        None => return,
    };
    if rem != 0 {
        buf.resize(buf.len().saturating_add(align.saturating_sub(rem)), 0);
    }
}

fn encode(snap: &StoreSnapshot) -> Result<Vec<u8>, SnapshotError> {
    let n_pos = u32::try_from(snap.keys.len()).map_err(|_| SnapshotError::Layout)?;
    let n_markets = u32::try_from(snap.markets.len()).map_err(|_| SnapshotError::Layout)?;
    let ix_len = u32::try_from(snap.market_index.len()).map_err(|_| SnapshotError::Layout)?;
    let mut body = Vec::new();
    body.extend_from_slice(bytemuck::cast_slice(snap.config.as_ref()));
    body.extend_from_slice(bytemuck::cast_slice(snap.extra.as_ref()));
    let keys: Vec<KeyRec> = snap
        .keys
        .iter()
        .map(|k| {
            let mut user = [0u8; 20];
            let src = k.key.user.as_slice();
            if let Some(dst) = user.get_mut(..src.len()) {
                dst.copy_from_slice(src);
            }
            KeyRec {
                protocol: k.key.protocol.0,
                _pad: 0,
                market: k.key.market.0,
                user,
                local: k.local,
            }
        })
        .collect();
    body.extend_from_slice(bytemuck::cast_slice(&keys));
    pad_to(&mut body, ALIGN);
    body.extend_from_slice(bytemuck::cast_slice(snap.market_index.as_ref()));
    pad_to(&mut body, ALIGN);
    for m in snap.markets.iter() {
        let stride = u32::try_from(m.stride).map_err(|_| SnapshotError::Layout)?;
        let n_rows = u32::try_from(m.rows.len()).map_err(|_| SnapshotError::Layout)?;
        body.extend_from_slice(&stride.to_le_bytes());
        body.extend_from_slice(&m.n_pos.to_le_bytes());
        body.extend_from_slice(&n_rows.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        pad_to(&mut body, ALIGN);
        body.extend_from_slice(bytemuck::cast_slice(m.rows.as_ref()));
        body.extend_from_slice(bytemuck::cast_slice(m.supply.as_ref()));
        body.extend_from_slice(bytemuck::cast_slice(m.debt.as_ref()));
        body.extend_from_slice(bytemuck::cast_slice(m.slot_extra.as_ref()));
        pad_to(&mut body, ALIGN);
    }
    let mut out = vec![0u8; HEADER];
    if let Some(d) = out.get_mut(..8) {
        d.copy_from_slice(&MAGIC);
    }
    put_u32(&mut out, 8, VERSION);
    put_u32(&mut out, 12, n_pos);
    put_u64(&mut out, 16, snap.tip);
    put_u64(&mut out, 24, snap.writes);
    put_u32(&mut out, 32, n_markets);
    put_u32(&mut out, 36, ix_len);
    // CRC covers the header minus the CRC field itself (`out[..40]`) then
    // the body — a flip in `tip` or `writes` must fail load, not silently
    // mis-scope WAL replay or admit/reject overlays.
    let hdr = out.get(..40).ok_or(SnapshotError::Truncated)?;
    let crc = !crc32_feed(crc32_feed(0xFFFF_FFFF, hdr), &body);
    put_u32(&mut out, 40, crc);
    out.extend_from_slice(&body);
    Ok(out)
}

fn put_u32(buf: &mut [u8], at: usize, v: u32) {
    if let Some(d) = buf.get_mut(at..at.saturating_add(4)) {
        d.copy_from_slice(&v.to_le_bytes());
    }
}

fn put_u64(buf: &mut [u8], at: usize, v: u64) {
    if let Some(d) = buf.get_mut(at..at.saturating_add(8)) {
        d.copy_from_slice(&v.to_le_bytes());
    }
}

fn u32_at(buf: &[u8], at: usize) -> Result<u32, SnapshotError> {
    let s = buf
        .get(at..at.saturating_add(4))
        .ok_or(SnapshotError::Truncated)?;
    let a: [u8; 4] = s.try_into().map_err(|_| SnapshotError::Truncated)?;
    Ok(u32::from_le_bytes(a))
}

fn u64_at(buf: &[u8], at: usize) -> Result<u64, SnapshotError> {
    let s = buf
        .get(at..at.saturating_add(8))
        .ok_or(SnapshotError::Truncated)?;
    let a: [u8; 8] = s.try_into().map_err(|_| SnapshotError::Truncated)?;
    Ok(u64::from_le_bytes(a))
}

fn decode(bytes: &[u8]) -> Result<StoreSnapshot, SnapshotError> {
    let hdr = bytes.get(..HEADER).ok_or(SnapshotError::Truncated)?;
    if hdr.get(..8) != Some(MAGIC.as_slice()) {
        return Err(SnapshotError::Magic);
    }
    let version = u32_at(hdr, 8)?;
    if version != VERSION {
        return Err(SnapshotError::Version(version));
    }
    let n_pos = u32_at(hdr, 12)? as usize;
    let tip = u64_at(hdr, 16)?;
    let writes = u64_at(hdr, 24)?;
    let n_markets = u32_at(hdr, 32)? as usize;
    let ix_len = u32_at(hdr, 36)? as usize;
    let crc = u32_at(hdr, 40)?;
    let body = bytes.get(HEADER..).ok_or(SnapshotError::Truncated)?;
    let prefix = hdr.get(..40).ok_or(SnapshotError::Truncated)?;
    if !crc32_feed(crc32_feed(0xFFFF_FFFF, prefix), body) != crc {
        return Err(SnapshotError::Crc);
    }
    let mut off = 0usize;
    let config = copy_pod::<AssetMask>(body, &mut off, n_pos)?;
    let extra = copy_pod::<PositionExtraRepr>(body, &mut off, n_pos)?;
    let recs = copy_pod::<KeyRec>(body, &mut off, n_pos)?;
    align_off(&mut off, ALIGN);
    let market_index = copy_pod::<u16>(body, &mut off, ix_len)?;
    align_off(&mut off, ALIGN);
    let mut markets = Vec::with_capacity(n_markets);
    for _ in 0..n_markets {
        let stride = u32_at(body, off)? as usize;
        let n = u32_at(body, off.saturating_add(4))?;
        let n_rows = u32_at(body, off.saturating_add(8))? as usize;
        off = off.saturating_add(16);
        align_off(&mut off, ALIGN);
        let rows = copy_pod::<MarketRow>(body, &mut off, n_rows)?;
        let cells = (n as usize)
            .checked_mul(stride)
            .ok_or(SnapshotError::Layout)?;
        let supply = copy_pod::<u128>(body, &mut off, cells)?;
        let debt = copy_pod::<u128>(body, &mut off, cells)?;
        let slot_extra = copy_pod::<PositionExtraRepr>(body, &mut off, cells)?;
        align_off(&mut off, ALIGN);
        markets.push(SnapMarket {
            stride,
            n_pos: n,
            rows: rows.into(),
            supply: supply.into(),
            debt: debt.into(),
            slot_extra: slot_extra.into(),
        });
    }
    let keys: Result<Vec<SnapKey>, SnapshotError> = recs
        .into_iter()
        .map(|r| {
            Ok(SnapKey {
                key: PositionKey {
                    protocol: ProtocolId(r.protocol),
                    market: MarketId(r.market),
                    user: Address::from(r.user),
                },
                local: r.local,
            })
        })
        .collect();
    Ok(StoreSnapshot {
        tip,
        writes,
        config: config.into(),
        extra: extra.into(),
        keys: keys?.into(),
        market_index: market_index.into(),
        markets: markets.into(),
    })
}

fn align_off(off: &mut usize, align: usize) {
    let rem = match off.checked_rem(align) {
        Some(r) => r,
        None => return,
    };
    if rem != 0 {
        *off = off.saturating_add(align.saturating_sub(rem));
    }
}

fn copy_pod<T: Pod>(body: &[u8], off: &mut usize, n: usize) -> Result<Vec<T>, SnapshotError> {
    let sz = core::mem::size_of::<T>();
    let need = n.checked_mul(sz).ok_or(SnapshotError::Layout)?;
    let end = off.checked_add(need).ok_or(SnapshotError::Layout)?;
    let slice = body.get(*off..end).ok_or(SnapshotError::Truncated)?;
    *off = end;
    if let Ok(s) = bytemuck::try_cast_slice::<u8, T>(slice) {
        if s.len() != n {
            return Err(SnapshotError::Layout);
        }
        return Ok(s.to_vec());
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let start = i.checked_mul(sz).ok_or(SnapshotError::Layout)?;
        let stop = start.checked_add(sz).ok_or(SnapshotError::Layout)?;
        let chunk = slice.get(start..stop).ok_or(SnapshotError::Truncated)?;
        out.push(bytemuck::pod_read_unaligned(chunk));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::{crc32, crc32_feed, decode, Shared, StoreSnapshot, HEADER};
    use crate::store::{StateStore, StoreConfig};
    use crate::undo::UndoCapacity;
    use crate::view::Overlay;
    use alloy_primitives::Address;
    use liq_protocol::{FeedId, MarketRow, StateWriter};
    use liq_types::{AssetId, MarketId, PositionId, PositionKey, ProtocolId};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    static N: AtomicU64 = AtomicU64::new(0);

    fn tmp() -> std::path::PathBuf {
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("liq-02b-snap-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p.join("state.snap")
    }

    fn row(asset: u16, tag: u32) -> MarketRow {
        let mut r = MarketRow {
            price_feed: FeedId(asset),
            last_update: tag,
            ..MarketRow::blank(AssetId(asset), 18)
        };
        r.body[0] = u128::from(tag) << 64;
        r.body[1] = u128::from(tag);
        r.body[14] = 7;
        r
    }

    fn cfg(positions: usize) -> StoreConfig {
        StoreConfig {
            base: 1_000,
            positions,
            markets: 4,
            undo: UndoCapacity {
                ops: 64,
                extras: 8,
                rows: 8,
            },
        }
    }

    fn seeded() -> StateStore {
        let mut st = StateStore::new(cfg(32));
        let m = MarketId(3);
        for a in 0..5u16 {
            st.push_market(m, row(a, 1)).unwrap();
        }
        for u in 0..4u8 {
            let p = st
                .intern(&PositionKey {
                    protocol: ProtocolId(1),
                    market: m,
                    user: Address::repeat_byte(u),
                })
                .unwrap();
            st.set_supply(p, 0, 1_000 + u128::from(u)).unwrap();
            st.set_debt(p, 1, 500 + u128::from(u)).unwrap();
        }
        st
    }

    /// Oracle: ITU-T V.42 / ISO-HDLC published check value.
    #[test]
    fn crc32_ieee_check_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// Oracle: mathematical identity — restore equals the live digest.
    /// Negative: a flipped CRC byte is refused.
    #[test]
    fn snapshot_round_trip_and_corrupt_crc_is_refused() {
        let st = seeded();
        let snap = st.snapshot();
        assert_eq!(snap.tip, st.tip());
        assert_eq!(snap.writes, st.writes());
        assert_eq!(snap.markets[0].stride, 8);
        assert_eq!(snap.markets[0].n_pos, 4);
        let path = tmp();
        snap.write_to(&path).unwrap();
        let loaded = StoreSnapshot::load(&path).unwrap();
        let restored = loaded.into_store(cfg(32)).unwrap();
        assert_eq!(restored.tip(), st.tip());
        assert_eq!(restored.floor(), restored.tip(), "depth == 0");
        assert_eq!(restored.writes(), st.writes());
        assert_eq!(restored.len(), st.len());
        for i in 0..st.len() as u32 {
            let id = PositionId(i);
            assert_eq!(restored.supply(id, 0).unwrap(), st.supply(id, 0).unwrap());
            assert_eq!(restored.debt(id, 1).unwrap(), st.debt(id, 1).unwrap());
        }
        assert_eq!(
            restored.markets(MarketId(3)).unwrap(),
            st.markets(MarketId(3)).unwrap()
        );

        let mut raw = std::fs::read(&path).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        std::fs::write(&path, &raw).unwrap();
        assert!(matches!(
            StoreSnapshot::load(&path),
            Err(super::SnapshotError::Crc)
        ));
    }

    /// Negative: a single-bit flip in `tip` (offset 16) or `writes` (offset
    /// 24) is refused. Body-only CRC would accept both and silently
    /// mis-scope WAL replay / overlay `check_base`.
    #[test]
    fn snapshot_header_crc_refuses_tip_or_writes_flip() {
        let st = seeded();
        let path = tmp();
        st.snapshot().write_to(&path).unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert!(raw.len() > 24, "header must contain tip and writes");

        let mut tip_flip = raw.clone();
        tip_flip[16] ^= 1;
        std::fs::write(&path, &tip_flip).unwrap();
        assert!(
            matches!(StoreSnapshot::load(&path), Err(super::SnapshotError::Crc)),
            "flipped tip (offset 16) must be SnapshotError::Crc"
        );

        let mut writes_flip = raw;
        writes_flip[24] ^= 1;
        std::fs::write(&path, &writes_flip).unwrap();
        assert!(
            matches!(StoreSnapshot::load(&path), Err(super::SnapshotError::Crc)),
            "flipped writes (offset 24) must be SnapshotError::Crc"
        );
    }

    /// Oracle: the rename-over-existing contract `write_to` relies on — the
    /// second write must replace the first (not fail, not leave the temp
    /// behind). Negative: the stale tip is gone from the loaded file.
    #[test]
    fn second_write_replaces_the_previous_snapshot() {
        let mut st = seeded();
        let path = tmp();
        st.snapshot().write_to(&path).unwrap();
        let first = StoreSnapshot::load(&path).unwrap();
        st.begin_block(1_001).unwrap();
        st.set_supply(PositionId(0), 0, 12_345).unwrap();
        st.snapshot().write_to(&path).unwrap();
        let second = StoreSnapshot::load(&path).unwrap();
        assert_eq!(first.tip, 1_000);
        assert_eq!(second.tip, 1_001);
        assert_ne!(second.writes, first.writes);
        assert_eq!(
            second
                .into_store(cfg(32))
                .unwrap()
                .supply(PositionId(0), 0)
                .unwrap(),
            12_345
        );
        assert!(
            !super::tmp_path(&path).exists(),
            "temp file was renamed, not copied"
        );
    }

    /// Carry-forward (c): `writes` is in the snapshot so an overlay built
    /// against the live store is accepted on the restored one. Negative:
    /// one extra mutation on the restored store makes `check_base` refuse.
    #[test]
    fn overlay_survives_restore_only_with_writes() {
        let st = seeded();
        let mut ov = Overlay::new();
        {
            let mut w = ov.writer(&st).unwrap();
            w.set_supply(PositionId(0), 0, 42).unwrap();
        }
        let snap = st.snapshot();
        let restored = snap.into_store(cfg(32)).unwrap();
        ov.writer(&restored).unwrap();
        let mut dead = Overlay::new();
        {
            let mut w = dead.writer(&st).unwrap();
            w.set_supply(PositionId(1), 0, 7).unwrap();
        }
        let mut st2 = snap.into_store(cfg(32)).unwrap();
        st2.set_supply(PositionId(1), 0, 1).unwrap();
        assert!(dead.writer(&st2).is_err());
    }

    /// GUIDE 02 §7b: leak at startup; readers swap.
    #[test]
    fn shared_leak_and_publish() {
        let shared = Shared::leak(StoreSnapshot::empty());
        let st = seeded();
        shared.publish(st.snapshot());
        let loaded = shared.snapshot.load();
        assert_eq!(loaded.len(), st.len());
        assert_eq!(loaded.tip, st.tip());
    }

    /// GUIDE 02 acceptance: 50k-position snapshot load + restore < 1 s.
    /// Oracle: wall clock of the load path only (populate is not recovery).
    #[test]
    fn cold_start_50k_under_one_second() {
        let mut st = StateStore::new(cfg(50_000));
        let m = MarketId(0);
        st.push_market(m, row(0, 1)).unwrap();
        st.reserve_positions(m, 50_000).unwrap();
        for u in 0..50_000u32 {
            let mut b = [0u8; 20];
            b[..4].copy_from_slice(&u.to_be_bytes());
            let p = st
                .intern(&PositionKey {
                    protocol: ProtocolId(1),
                    market: m,
                    user: Address::from(b),
                })
                .unwrap();
            st.set_supply(p, 0, u128::from(u) + 1).unwrap();
        }
        let path = tmp();
        st.snapshot().write_to(&path).unwrap();
        let t_read = Instant::now();
        let bytes = std::fs::read(&path).unwrap();
        let read_dt = t_read.elapsed();
        let t_crc = Instant::now();
        let hdr = bytes.get(..HEADER).unwrap();
        let prefix = hdr.get(..40).unwrap();
        let body = bytes.get(HEADER..).unwrap();
        let computed = !crc32_feed(crc32_feed(0xFFFF_FFFF, prefix), body);
        let crc_dt = t_crc.elapsed();
        let stored = u32::from_le_bytes(hdr.get(40..44).unwrap().try_into().unwrap());
        assert_eq!(computed, stored, "stage CRC must match the on-disk header");
        let t_decode = Instant::now();
        let loaded = decode(&bytes).unwrap();
        let decode_dt = t_decode.elapsed();
        let t_store = Instant::now();
        let restored = loaded.into_store(cfg(50_000)).unwrap();
        let store_dt = t_store.elapsed();
        let total = read_dt + decode_dt + store_dt;
        eprintln!(
            "cold start 50k: fs::read={read_dt:?} crc={crc_dt:?} decode={decode_dt:?} into_store={store_dt:?} total={total:?}"
        );
        assert_eq!(restored.len(), 50_000);
        assert_eq!(restored.supply(PositionId(49_999), 0).unwrap(), 50_000);
        assert_eq!(restored.floor(), restored.tip());
        // Debug copies + HashMap fill of 50k is multi-second; the budget is
        // a release measurement (GUIDE 02). Always print; enforce in release.
        if !cfg!(debug_assertions) {
            assert!(
                total.as_millis() < 1_000,
                "cold start 50k took {total:?}, budget 1s"
            );
        }
    }
}
