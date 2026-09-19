//! WAL: append-only mutation log with off-thread fsync (GUIDE 02 §7).
//!
//! The hot thread only [`Wal::append`]s into a bounded `crossbeam` channel
//! (`send` blocks when full — backpressure, never drop). A dedicated writer
//! thread encodes, writes, and `sync_all`s. Fsync cadence is each
//! `BeginBlock` / `Snapshot` / `UnwindTo` plus shutdown: the lag is at most
//! the channel bound plus the open block. Crash recovery loads the snapshot
//! (`snapshot::StoreSnapshot::load`) and replays the frames after its
//! `Snapshot{block}` marker (log position, not a mutable block heuristic).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread::{self, JoinHandle};

use alloy_primitives::Address;
use bytemuck::Pod;
use crossbeam_channel::{bounded, Receiver, Sender};
use liq_protocol::{BlockNum, MarketRow, MarketSlot, PositionExtraRepr, StateWriter};
use liq_types::{MarketId, PositionId, PositionKey, ProtocolId};

use crate::error::StateError;
use crate::snapshot::{crc32, SnapshotError, StoreSnapshot};
use crate::store::{StateStore, StoreConfig};

const MAGIC: [u8; 8] = *b"LIQWAL01";
const MAX_FRAME: usize = 512;

/// [`read_frame`] refuses a frame longer than [`MAX_FRAME`] while
/// [`write_frame`] does not check, so a record that outgrew the cap would be
/// written and then be unreadable. The two widest encodings are pinned here:
/// widening `MarketRow` or `PositionExtraRepr` past the cap fails the build
/// instead of producing a WAL that cannot be replayed.
const _: () = {
    // tag + MarketId + slot + row
    assert!(1 + 4 + 2 + core::mem::size_of::<MarketRow>() <= MAX_FRAME);
    // tag + PositionId + extra
    assert!(1 + 4 + core::mem::size_of::<PositionExtraRepr>() <= MAX_FRAME);
};

/// One WAL frame. Replay applies these through [`StateWriter`] (plus
/// `begin_block` / `unwind_to`) in order, skipping anything at or before
/// the snapshot tip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalRecord {
    BeginBlock {
        block: BlockNum,
    },
    Intern {
        key: PositionKey,
    },
    SetSupply {
        pos: PositionId,
        slot: u16,
        shares: u128,
    },
    SetDebt {
        pos: PositionId,
        slot: u16,
        shares: u128,
    },
    SetExtra {
        pos: PositionId,
        extra: PositionExtraRepr,
    },
    SetMarket {
        at: MarketSlot,
        row: MarketRow,
    },
    PushMarket {
        market: MarketId,
        row: MarketRow,
    },
    /// Checkpoint: recovery ignores frames at or before `block`.
    Snapshot {
        block: BlockNum,
    },
    UnwindTo {
        target: BlockNum,
    },
}

enum WalCmd {
    Record(WalRecord),
    Shutdown,
}

/// Off-thread WAL writer. `append` never fsyncs.
pub struct Wal {
    tx: Sender<WalCmd>,
    thread: Option<JoinHandle<Result<(), WalError>>>,
}

/// WAL I/O or protocol failure. Off the hot path.
#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("WAL magic mismatch")]
    Magic,
    #[error("WAL CRC mismatch")]
    Crc,
    #[error("WAL truncated in the middle of a durable frame")]
    Corrupt,
    #[error("WAL channel closed (writer thread died)")]
    Closed,
    #[error("WAL channel bound must be ≥ 1")]
    ZeroBound,
    #[error("unknown WAL record tag {0}")]
    Tag(u8),
}

/// Snapshot + WAL recovery failure.
#[derive(Debug, thiserror::Error)]
pub enum RecoverError {
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    #[error(transparent)]
    Wal(#[from] WalError),
    #[error(transparent)]
    Protocol(#[from] liq_protocol::ProtocolError),
    #[error(transparent)]
    State(#[from] StateError),
}

impl Wal {
    /// Open (or create) `path`, spawn the fsync thread. `bound` is the
    /// maximum queued records; `append` blocks at capacity.
    pub fn open(path: &Path, bound: usize) -> Result<Self, WalError> {
        if bound == 0 {
            return Err(WalError::ZeroBound);
        }
        let (tx, rx) = bounded(bound);
        let path = path.to_path_buf();
        let thread = thread::Builder::new()
            .name("liq-wal".into())
            .spawn(move || writer_loop(path, rx))
            .map_err(WalError::Io)?;
        Ok(Self {
            tx,
            thread: Some(thread),
        })
    }

    /// Enqueue `rec`. Blocks if the channel is full (backpressure). Never
    /// drops. Never fsyncs.
    pub fn append(&self, rec: WalRecord) -> Result<(), WalError> {
        self.tx
            .send(WalCmd::Record(rec))
            .map_err(|_| WalError::Closed)
    }

    /// Flush remaining records, fsync, join the writer. Prefer this over
    /// drop when the error must surface.
    pub fn shutdown(mut self) -> Result<(), WalError> {
        let _ = self.tx.send(WalCmd::Shutdown);
        match self.thread.take() {
            Some(h) => h.join().unwrap_or(Err(WalError::Closed)),
            None => Ok(()),
        }
    }
}

impl Drop for Wal {
    fn drop(&mut self) {
        let _ = self.tx.send(WalCmd::Shutdown);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

fn writer_loop(path: std::path::PathBuf, rx: Receiver<WalCmd>) -> Result<(), WalError> {
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        file.write_all(&MAGIC)?;
        file.sync_all()?;
    } else {
        let mut mag = [0u8; 8];
        file.read_exact(&mut mag)?;
        if mag != MAGIC {
            return Err(WalError::Magic);
        }
        file.seek(SeekFrom::End(0))?;
    }
    let mut pending = false;
    loop {
        match rx.recv() {
            Ok(WalCmd::Record(rec)) => {
                write_frame(&mut file, &rec)?;
                pending = true;
                if matches!(
                    rec,
                    WalRecord::BeginBlock { .. }
                        | WalRecord::Snapshot { .. }
                        | WalRecord::UnwindTo { .. }
                ) {
                    file.sync_all()?;
                    pending = false;
                }
            }
            Ok(WalCmd::Shutdown) | Err(_) => {
                if pending {
                    file.sync_all()?;
                }
                return Ok(());
            }
        }
    }
}

fn write_frame(file: &mut File, rec: &WalRecord) -> Result<(), WalError> {
    let payload = encode(rec);
    let crc = crc32(&payload);
    let len = u32::try_from(payload.len()).map_err(|_| WalError::Corrupt)?;
    file.write_all(&len.to_le_bytes())?;
    file.write_all(&crc.to_le_bytes())?;
    file.write_all(&payload)?;
    Ok(())
}

const TAG_BEGIN: u8 = 1;
const TAG_INTERN: u8 = 2;
const TAG_SUPPLY: u8 = 3;
const TAG_DEBT: u8 = 4;
const TAG_EXTRA: u8 = 5;
const TAG_MARKET: u8 = 6;
const TAG_PUSH: u8 = 7;
const TAG_SNAP: u8 = 8;
const TAG_UNWIND: u8 = 9;

fn encode(rec: &WalRecord) -> Vec<u8> {
    let mut b = Vec::with_capacity(160);
    match rec {
        WalRecord::BeginBlock { block } => {
            b.push(TAG_BEGIN);
            b.extend_from_slice(&block.to_le_bytes());
        }
        WalRecord::Intern { key } => {
            b.push(TAG_INTERN);
            b.extend_from_slice(&key.protocol.0.to_le_bytes());
            b.extend_from_slice(&key.market.0.to_le_bytes());
            b.extend_from_slice(key.user.as_slice());
        }
        WalRecord::SetSupply { pos, slot, shares } => {
            b.push(TAG_SUPPLY);
            put_cell(&mut b, *pos, *slot, *shares);
        }
        WalRecord::SetDebt { pos, slot, shares } => {
            b.push(TAG_DEBT);
            put_cell(&mut b, *pos, *slot, *shares);
        }
        WalRecord::SetExtra { pos, extra } => {
            b.push(TAG_EXTRA);
            b.extend_from_slice(&pos.0.to_le_bytes());
            b.extend_from_slice(bytemuck::bytes_of(extra));
        }
        WalRecord::SetMarket { at, row } => {
            b.push(TAG_MARKET);
            b.extend_from_slice(&at.market.0.to_le_bytes());
            b.extend_from_slice(&at.slot.to_le_bytes());
            b.extend_from_slice(bytemuck::bytes_of(row));
        }
        WalRecord::PushMarket { market, row } => {
            b.push(TAG_PUSH);
            b.extend_from_slice(&market.0.to_le_bytes());
            b.extend_from_slice(bytemuck::bytes_of(row));
        }
        WalRecord::Snapshot { block } => {
            b.push(TAG_SNAP);
            b.extend_from_slice(&block.to_le_bytes());
        }
        WalRecord::UnwindTo { target } => {
            b.push(TAG_UNWIND);
            b.extend_from_slice(&target.to_le_bytes());
        }
    }
    b
}

fn put_cell(b: &mut Vec<u8>, pos: PositionId, slot: u16, shares: u128) {
    b.extend_from_slice(&pos.0.to_le_bytes());
    b.extend_from_slice(&slot.to_le_bytes());
    b.extend_from_slice(&shares.to_le_bytes());
}

/// Replay every complete frame. A torn tail (short last frame) is discarded,
/// not an error — that is a crash during the last write.
pub fn replay(path: &Path) -> Result<Vec<WalRecord>, WalError> {
    let mut file = File::open(path)?;
    let mut mag = [0u8; 8];
    match file.read_exact(&mut mag) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Err(WalError::Magic),
        Err(e) => return Err(WalError::Io(e)),
    }
    if mag != MAGIC {
        return Err(WalError::Magic);
    }
    let mut out = Vec::new();
    loop {
        match read_frame(&mut file)? {
            None => return Ok(out),
            Some(rec) => out.push(rec),
        }
    }
}

fn read_frame(file: &mut File) -> Result<Option<WalRecord>, WalError> {
    let mut hdr = [0u8; 8];
    match file.read_exact(&mut hdr) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(WalError::Io(e)),
    }
    let len_b: [u8; 4] = hdr
        .get(..4)
        .ok_or(WalError::Corrupt)?
        .try_into()
        .map_err(|_| WalError::Corrupt)?;
    let crc_b: [u8; 4] = hdr
        .get(4..8)
        .ok_or(WalError::Corrupt)?
        .try_into()
        .map_err(|_| WalError::Corrupt)?;
    let len = u32::from_le_bytes(len_b) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(WalError::Corrupt);
    }
    let expect_crc = u32::from_le_bytes(crc_b);
    let mut payload = vec![0u8; len];
    match file.read_exact(&mut payload) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(WalError::Io(e)),
    }
    if crc32(&payload) != expect_crc {
        return Err(WalError::Crc);
    }
    decode(&payload).map(Some)
}

fn decode(p: &[u8]) -> Result<WalRecord, WalError> {
    let tag = *p.first().ok_or(WalError::Corrupt)?;
    let rest = p.get(1..).ok_or(WalError::Corrupt)?;
    match tag {
        TAG_BEGIN => Ok(WalRecord::BeginBlock {
            block: u64_le(rest, 0)?,
        }),
        TAG_INTERN => {
            let protocol = u16_le(rest, 0)?;
            let market = u32_le(rest, 2)?;
            let user = rest.get(6..26).ok_or(WalError::Corrupt)?;
            let mut a = [0u8; 20];
            a.copy_from_slice(user);
            Ok(WalRecord::Intern {
                key: PositionKey {
                    protocol: ProtocolId(protocol),
                    market: MarketId(market),
                    user: Address::from(a),
                },
            })
        }
        TAG_SUPPLY => {
            cell(rest).map(|(pos, slot, shares)| WalRecord::SetSupply { pos, slot, shares })
        }
        TAG_DEBT => cell(rest).map(|(pos, slot, shares)| WalRecord::SetDebt { pos, slot, shares }),
        TAG_EXTRA => {
            let pos = PositionId(u32_le(rest, 0)?);
            let extra = pod_at::<PositionExtraRepr>(rest, 4)?;
            Ok(WalRecord::SetExtra { pos, extra })
        }
        TAG_MARKET => {
            let market = MarketId(u32_le(rest, 0)?);
            let slot = u16_le(rest, 4)?;
            let row = pod_at::<MarketRow>(rest, 6)?;
            Ok(WalRecord::SetMarket {
                at: MarketSlot { market, slot },
                row,
            })
        }
        TAG_PUSH => {
            let market = MarketId(u32_le(rest, 0)?);
            let row = pod_at::<MarketRow>(rest, 4)?;
            Ok(WalRecord::PushMarket { market, row })
        }
        TAG_SNAP => Ok(WalRecord::Snapshot {
            block: u64_le(rest, 0)?,
        }),
        TAG_UNWIND => Ok(WalRecord::UnwindTo {
            target: u64_le(rest, 0)?,
        }),
        t => Err(WalError::Tag(t)),
    }
}

fn cell(rest: &[u8]) -> Result<(PositionId, u16, u128), WalError> {
    Ok((
        PositionId(u32_le(rest, 0)?),
        u16_le(rest, 4)?,
        u128_le(rest, 6)?,
    ))
}

fn u16_le(b: &[u8], at: usize) -> Result<u16, WalError> {
    let s = b.get(at..at.saturating_add(2)).ok_or(WalError::Corrupt)?;
    let a: [u8; 2] = s.try_into().map_err(|_| WalError::Corrupt)?;
    Ok(u16::from_le_bytes(a))
}

fn u32_le(b: &[u8], at: usize) -> Result<u32, WalError> {
    let s = b.get(at..at.saturating_add(4)).ok_or(WalError::Corrupt)?;
    let a: [u8; 4] = s.try_into().map_err(|_| WalError::Corrupt)?;
    Ok(u32::from_le_bytes(a))
}

fn u64_le(b: &[u8], at: usize) -> Result<u64, WalError> {
    let s = b.get(at..at.saturating_add(8)).ok_or(WalError::Corrupt)?;
    let a: [u8; 8] = s.try_into().map_err(|_| WalError::Corrupt)?;
    Ok(u64::from_le_bytes(a))
}

fn u128_le(b: &[u8], at: usize) -> Result<u128, WalError> {
    let s = b.get(at..at.saturating_add(16)).ok_or(WalError::Corrupt)?;
    let a: [u8; 16] = s.try_into().map_err(|_| WalError::Corrupt)?;
    Ok(u128::from_le_bytes(a))
}

fn pod_at<T: Pod>(b: &[u8], at: usize) -> Result<T, WalError> {
    let n = core::mem::size_of::<T>();
    let s = b.get(at..at.saturating_add(n)).ok_or(WalError::Corrupt)?;
    Ok(bytemuck::pod_read_unaligned(s))
}

/// Apply one record to `store`. `Snapshot` is a marker and a no-op on the
/// store itself.
pub fn apply(store: &mut StateStore, rec: &WalRecord) -> Result<(), RecoverError> {
    match rec {
        WalRecord::BeginBlock { block } => store.begin_block(*block)?,
        WalRecord::Intern { key } => {
            store.intern(key)?;
        }
        WalRecord::SetSupply { pos, slot, shares } => store.set_supply(*pos, *slot, *shares)?,
        WalRecord::SetDebt { pos, slot, shares } => store.set_debt(*pos, *slot, *shares)?,
        WalRecord::SetExtra { pos, extra } => store.set_extra(*pos, *extra)?,
        WalRecord::SetMarket { at, row } => store.set_market(*at, *row)?,
        WalRecord::PushMarket { market, row } => {
            store.push_market(*market, *row)?;
        }
        WalRecord::Snapshot { .. } => {}
        WalRecord::UnwindTo { target } => store.unwind_to(*target)?,
    }
    Ok(())
}

/// Load `snapshot_path`, then replay WAL frames after the `Snapshot{tip}`
/// marker. An unwind below the restored floor surfaces [`crate::error::StateError::ReorgTooDeep`].
pub fn recover(
    snapshot_path: &Path,
    wal_path: &Path,
    capacity: StoreConfig,
) -> Result<StateStore, RecoverError> {
    let snap = StoreSnapshot::load(snapshot_path)?;
    let tip = snap.tip;
    let mut store = snap.into_store(capacity)?;
    replay_into(&mut store, wal_path, tip)?;
    Ok(store)
}

/// Replay frames that appear after the `Snapshot{snap_tip}` marker.
///
/// The cut is log position, not a running `wal_block`. Frames before the
/// matching marker are already in the snapshot (including a prefix logged
/// before the first `BeginBlock`). Frames after it are applied, so an
/// `UnwindTo` below the restored floor (`floor == tip == snap_tip`, depth
/// 0) surfaces [`crate::error::StateError::ReorgTooDeep`] instead of being skipped.
/// A `BeginBlock` past `snap_tip` also opens the cut if the marker was lost.
pub fn replay_into(
    store: &mut StateStore,
    wal_path: &Path,
    snap_tip: BlockNum,
) -> Result<(), RecoverError> {
    let recs = replay(wal_path)?;
    let mut after_snap = false;
    for rec in &recs {
        match rec {
            WalRecord::Snapshot { block } if *block == snap_tip => {
                after_snap = true;
            }
            WalRecord::Snapshot { .. } => {}
            WalRecord::BeginBlock { block } => {
                if *block > snap_tip {
                    after_snap = true;
                }
                if after_snap {
                    apply(store, rec)?;
                }
            }
            _ if after_snap => apply(store, rec)?,
            _ => {}
        }
    }
    Ok(())
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
    use super::{recover, replay, RecoverError, Wal, WalError, WalRecord};
    use crate::error::StateError;
    use crate::store::{StateStore, StoreConfig};
    use crate::undo::UndoCapacity;
    use alloy_primitives::Address;
    use liq_protocol::{FeedId, MarketFlags, MarketRow, StateWriter};
    use liq_types::{AssetId, MarketId, PositionKey, ProtocolId, RayU128};
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    fn dir() -> std::path::PathBuf {
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("liq-02b-wal-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn row(asset: u16) -> MarketRow {
        MarketRow {
            supply_index: RayU128::from_raw(1),
            debt_index: RayU128::from_raw(1),
            supply_rate: RayU128::from_raw(0),
            debt_rate: RayU128::from_raw(0),
            dust_floor: 0,
            last_update: 0,
            target_hf: 0,
            hub_ref: u16::MAX,
            liq_threshold: 0,
            ltv: 0,
            price_feed: FeedId(asset),
            asset: AssetId(asset),
            max_liq_bonus: 0,
            hf_for_max_bonus: 0,
            liq_bonus_factor: 0,
            decimals: 18,
            flags: MarketFlags::NONE,
            _pad: [0; 22],
        }
    }

    fn cfg() -> StoreConfig {
        StoreConfig {
            base: 100,
            positions: 32,
            markets: 4,
            undo: UndoCapacity {
                ops: 64,
                extras: 8,
                rows: 8,
            },
        }
    }

    /// Oracle: identity `replay(log(apply(x))) == x` against the live store.
    /// Also: bound 2 does not drop 8 records (backpressure, not try_send).
    #[test]
    fn wal_replay_matches_live_and_does_not_drop() {
        let d = dir();
        let wal_path = d.join("w.log");
        let snap_path = d.join("s.snap");
        let wal = Wal::open(&wal_path, 2).unwrap();
        let mut st = StateStore::new(cfg());
        let m = MarketId(0);
        wal.append(WalRecord::PushMarket {
            market: m,
            row: row(0),
        })
        .unwrap();
        st.push_market(m, row(0)).unwrap();
        let k = PositionKey {
            protocol: ProtocolId(1),
            market: m,
            user: Address::repeat_byte(1),
        };
        wal.append(WalRecord::Intern { key: k }).unwrap();
        let p = st.intern(&k).unwrap();
        st.snapshot().write_to(&snap_path).unwrap();
        wal.append(WalRecord::Snapshot { block: st.tip() }).unwrap();
        wal.append(WalRecord::BeginBlock { block: 101 }).unwrap();
        st.begin_block(101).unwrap();
        for i in 0..8u128 {
            wal.append(WalRecord::SetSupply {
                pos: p,
                slot: 0,
                shares: i + 1,
            })
            .unwrap();
            st.set_supply(p, 0, i + 1).unwrap();
        }
        wal.shutdown().unwrap();
        let recs = replay(&wal_path).unwrap();
        assert_eq!(
            recs.iter()
                .filter(|r| matches!(r, WalRecord::SetSupply { .. }))
                .count(),
            8,
            "bound 2 must not drop"
        );
        let got = recover(&snap_path, &wal_path, cfg()).unwrap();
        assert_eq!(got.supply(p, 0).unwrap(), st.supply(p, 0).unwrap());
        assert_eq!(got.tip(), 101);
        assert_eq!(got.len(), st.len());
    }

    /// Oracle: a torn tail is dropped; a CRC failure with trailing bytes is
    /// refused. Independent of encode: we splice bytes on disk.
    #[test]
    fn torn_tail_dropped_mid_crc_refused() {
        let d = dir();
        let wal_path = d.join("w.log");
        let wal = Wal::open(&wal_path, 8).unwrap();
        wal.append(WalRecord::BeginBlock { block: 1 }).unwrap();
        wal.shutdown().unwrap();
        let mut bytes = std::fs::read(&wal_path).unwrap();
        bytes.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]); // length 1, no payload
        std::fs::write(&wal_path, &bytes).unwrap();
        let recs = replay(&wal_path).unwrap();
        assert_eq!(recs, vec![WalRecord::BeginBlock { block: 1 }]);

        let mut bad = std::fs::read(&wal_path).unwrap();
        // Flip a CRC byte of the first real frame (after magic).
        bad[12] ^= 0xff;
        bad.extend_from_slice(&[0u8; 16]);
        std::fs::write(&wal_path, &bad).unwrap();
        assert!(matches!(replay(&wal_path), Err(WalError::Crc)));
    }

    /// Recovery must not double-apply the snapshotted prefix.
    #[test]
    fn recover_skips_records_at_or_before_snapshot_tip() {
        let d = dir();
        let wal_path = d.join("w.log");
        let snap_path = d.join("s.snap");
        let wal = Wal::open(&wal_path, 32).unwrap();
        let mut st = StateStore::new(cfg());
        let m = MarketId(1);
        wal.append(WalRecord::PushMarket {
            market: m,
            row: row(3),
        })
        .unwrap();
        st.push_market(m, row(3)).unwrap();
        let k = PositionKey {
            protocol: ProtocolId(2),
            market: m,
            user: Address::repeat_byte(9),
        };
        wal.append(WalRecord::Intern { key: k }).unwrap();
        let p = st.intern(&k).unwrap();
        wal.append(WalRecord::SetSupply {
            pos: p,
            slot: 0,
            shares: 7,
        })
        .unwrap();
        st.set_supply(p, 0, 7).unwrap();
        st.snapshot().write_to(&snap_path).unwrap();
        wal.append(WalRecord::Snapshot { block: st.tip() }).unwrap();
        wal.append(WalRecord::BeginBlock { block: 101 }).unwrap();
        st.begin_block(101).unwrap();
        wal.append(WalRecord::SetSupply {
            pos: p,
            slot: 0,
            shares: 99,
        })
        .unwrap();
        st.set_supply(p, 0, 99).unwrap();
        wal.shutdown().unwrap();
        let got = recover(&snap_path, &wal_path, cfg()).unwrap();
        assert_eq!(got.supply(p, 0).unwrap(), 99);
        assert_eq!(got.supply(p, 0).unwrap(), st.supply(p, 0).unwrap());
    }

    #[test]
    fn zero_bound_is_refused() {
        let d = dir();
        assert!(matches!(
            Wal::open(&d.join("w.log"), 0),
            Err(WalError::ZeroBound)
        ));
    }

    /// Oracle: live store after unwind + canonical rewrite. Recovery must
    /// match (identity `recover(log(apply(x))) == x`). Mutation-red: skipping
    /// `UnwindTo` leaves the orphaned supply or fails `BeginBlock` with a gap.
    #[test]
    fn recover_replays_unwind_to_after_snapshot() {
        let d = dir();
        let wal_path = d.join("w.log");
        let snap_path = d.join("s.snap");
        let wal = Wal::open(&wal_path, 32).unwrap();
        let mut st = StateStore::new(cfg());
        let m = MarketId(0);
        wal.append(WalRecord::PushMarket {
            market: m,
            row: row(0),
        })
        .unwrap();
        st.push_market(m, row(0)).unwrap();
        let k = PositionKey {
            protocol: ProtocolId(1),
            market: m,
            user: Address::repeat_byte(1),
        };
        wal.append(WalRecord::Intern { key: k }).unwrap();
        let p = st.intern(&k).unwrap();
        wal.append(WalRecord::SetSupply {
            pos: p,
            slot: 0,
            shares: 1,
        })
        .unwrap();
        st.set_supply(p, 0, 1).unwrap();
        st.snapshot().write_to(&snap_path).unwrap();
        wal.append(WalRecord::Snapshot { block: st.tip() }).unwrap();

        wal.append(WalRecord::BeginBlock { block: 101 }).unwrap();
        st.begin_block(101).unwrap();
        wal.append(WalRecord::SetSupply {
            pos: p,
            slot: 0,
            shares: 22,
        })
        .unwrap();
        st.set_supply(p, 0, 22).unwrap();

        wal.append(WalRecord::UnwindTo { target: 100 }).unwrap();
        st.unwind_to(100).unwrap();

        wal.append(WalRecord::BeginBlock { block: 101 }).unwrap();
        st.begin_block(101).unwrap();
        wal.append(WalRecord::SetSupply {
            pos: p,
            slot: 0,
            shares: 99,
        })
        .unwrap();
        st.set_supply(p, 0, 99).unwrap();
        wal.shutdown().unwrap();

        let got = recover(&snap_path, &wal_path, cfg()).unwrap();
        assert_eq!(got.supply(p, 0).unwrap(), 99);
        assert_eq!(got.supply(p, 0).unwrap(), st.supply(p, 0).unwrap());
        assert_eq!(got.tip(), 101);
        assert_eq!(st.tip(), 101);
    }

    /// Reproduced silent-wrong-state: snapshot tip 102, then `UnwindTo{101}`
    /// and canonical 102'. Restored `floor == tip == 102`; the unwind is
    /// below the floor and must surface `ReorgTooDeep`, not `Ok` with the
    /// orphaned supply (GUIDE 02: health factors silently wrong after a reorg).
    #[test]
    fn recover_unwind_below_snapshot_floor_is_reorg_too_deep() {
        let d = dir();
        let wal_path = d.join("w.log");
        let snap_path = d.join("s.snap");
        let wal = Wal::open(&wal_path, 32).unwrap();
        let mut st = StateStore::new(cfg());
        let m = MarketId(0);
        wal.append(WalRecord::PushMarket {
            market: m,
            row: row(0),
        })
        .unwrap();
        st.push_market(m, row(0)).unwrap();
        let k = PositionKey {
            protocol: ProtocolId(1),
            market: m,
            user: Address::repeat_byte(2),
        };
        wal.append(WalRecord::Intern { key: k }).unwrap();
        let p = st.intern(&k).unwrap();
        wal.append(WalRecord::BeginBlock { block: 101 }).unwrap();
        st.begin_block(101).unwrap();
        wal.append(WalRecord::SetSupply {
            pos: p,
            slot: 0,
            shares: 7,
        })
        .unwrap();
        st.set_supply(p, 0, 7).unwrap();
        wal.append(WalRecord::BeginBlock { block: 102 }).unwrap();
        st.begin_block(102).unwrap();
        wal.append(WalRecord::SetSupply {
            pos: p,
            slot: 0,
            shares: 22,
        })
        .unwrap();
        st.set_supply(p, 0, 22).unwrap();
        st.snapshot().write_to(&snap_path).unwrap();
        wal.append(WalRecord::Snapshot { block: st.tip() }).unwrap();

        wal.append(WalRecord::UnwindTo { target: 101 }).unwrap();
        wal.append(WalRecord::BeginBlock { block: 102 }).unwrap();
        wal.append(WalRecord::SetSupply {
            pos: p,
            slot: 0,
            shares: 99,
        })
        .unwrap();
        wal.shutdown().unwrap();

        st.unwind_to(101).unwrap();
        st.begin_block(102).unwrap();
        st.set_supply(p, 0, 99).unwrap();
        assert_eq!(st.supply(p, 0).unwrap(), 99);

        let err = recover(&snap_path, &wal_path, cfg()).unwrap_err();
        assert!(
            matches!(err, RecoverError::State(StateError::ReorgTooDeep { .. })),
            "unwind below restored floor must fail loudly, got {err:?}"
        );
    }
}
