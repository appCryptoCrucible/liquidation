//! The bot's state on disk: built once from the node's retained receipts,
//! then kept current by periodic snapshots.
//!
//! * **First start** ([`build_first_snapshot`]): no snapshot yet. Every
//!   protocol adapter's logs are replayed from [`BotConfig::backfill_from`]
//!   to the node's *finalized* block through the same fold as live blocks
//!   (`liq_node::backfill`), then written as the snapshot. Nothing else runs
//!   meanwhile, so no partial state is ever planned or sent from.
//! * **Running** ([`SnapshotWriter`]): every
//!   [`BotConfig::snapshot_every_blocks`] the hot thread copies the store and
//!   a background thread writes it.
//! * **Any start**: the snapshot's block ([`SnapshotHead`]) is handed to Reth
//!   as the ExEx head; Reth re-executes the blocks after it and delivers them
//!   before live ones.
//!
//! * **Bindings changed** ([`bindings_fingerprint`]): the head records a hash
//!   of every bound subscription. A start whose adapters bind to a different
//!   set (a new Gearbox manager, a new adapter, a changed event list) cannot
//!   use the snapshot: ids may have moved and the new contracts have no
//!   history in it. Startup refuses it
//!   ([`crate::startup::StartupError::StaleSnapshot`]) and the caller rebuilds.
//!
//! Files: `snapshot.bin` (the store), `snapshot.head` (its block number,
//! hash and bindings fingerprint), `wal.log` (kept empty: Reth's replay
//! replaces it). Each is written to a temporary file and renamed into place.
//!
//! [`BotConfig::backfill_from`]: liq_config::BotConfig::backfill_from
//! [`BotConfig::snapshot_every_blocks`]: liq_config::BotConfig::snapshot_every_blocks

use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};

use alloy_primitives::B256;
use liq_node::{ApplyCtx, DecodeArena, DirtyAccumulator, LogHandler, LogRouter, SnapshotSink};
use liq_state::{StateStore, StoreConfig, StoreSnapshot, UndoCapacity};
use liq_types::{LogFilter, LogSubscriber};
use thiserror::Error;

use crate::bind::{
    ingest_handlers, leak_protocols, load_protocols, subscriber_refs, BoundProtocol,
};
use crate::lease::{write_empty_wal, StatePaths};
use crate::live_rpc::LiveRpc;

/// The block a snapshot holds the state after, and what it was bound to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SnapshotHead {
    pub number: u64,
    pub hash: B256,
    /// [`bindings_fingerprint`] of the adapters that folded it.
    pub bindings: B256,
}

/// Bump when a fold changes what a stored row means without changing any
/// subscription, so every snapshot written before it is rebuilt.
pub const STATE_EPOCH: u32 = 1;

/// Hash of [`STATE_EPOCH`] and every `(protocol, address, topic0)` the bound
/// adapters subscribe to, sorted. Equal fingerprints mean the same contracts
/// in the same discovery order, so the same MarketIds.
#[must_use]
pub fn bindings_fingerprint(protocols: &[BoundProtocol]) -> B256 {
    let mut rows: Vec<(u16, alloy_primitives::Address, B256)> = protocols
        .iter()
        .flat_map(|p| {
            let id = p.id().0;
            p.subscriptions()
                .into_iter()
                .map(move |f| (id, f.address, f.topic0))
        })
        .collect();
    rows.sort_unstable();
    rows.dedup();
    let mut buf = Vec::with_capacity(rows.len().saturating_mul(54).saturating_add(4));
    buf.extend_from_slice(&STATE_EPOCH.to_be_bytes());
    for (id, a, t) in &rows {
        buf.extend_from_slice(&id.to_be_bytes());
        buf.extend_from_slice(a.as_slice());
        buf.extend_from_slice(t.as_slice());
    }
    alloy_primitives::keccak256(&buf)
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("state build config: {0}")]
    Config(String),
    #[error("state build rpc: {0}")]
    Rpc(String),
    #[error("state build omitted {0} adapter(s): {1}")]
    Omitted(usize, String),
    #[error("state replay: {0}")]
    Replay(String),
    #[error("state file {path}: {cause}")]
    Io { path: PathBuf, cause: String },
}

fn io(path: &Path, e: impl std::fmt::Display) -> BuildError {
    BuildError::Io {
        path: path.to_path_buf(),
        cause: e.to_string(),
    }
}

/// `snapshot.head` beside `snapshot.bin`.
#[must_use]
pub fn head_path(paths: &StatePaths) -> PathBuf {
    paths.snapshot.with_extension("head")
}

/// Read the head record: `<number> <hash> <bindings>`.
pub fn read_head(paths: &StatePaths) -> Result<SnapshotHead, BuildError> {
    let p = head_path(paths);
    let raw = std::fs::read_to_string(&p).map_err(|e| io(&p, e))?;
    let mut it = raw.split_whitespace();
    let (Some(n), Some(h), Some(b), None) = (it.next(), it.next(), it.next(), it.next()) else {
        return Err(io(&p, "expected `<number> <hash> <bindings>`"));
    };
    Ok(SnapshotHead {
        number: n.parse().map_err(|e| io(&p, e))?,
        hash: h.parse().map_err(|e| io(&p, e))?,
        bindings: b.parse().map_err(|e| io(&p, e))?,
    })
}

/// Remove the head record so the next start rebuilds. The snapshot itself
/// is overwritten by the rebuild.
pub fn discard_head(paths: &StatePaths) -> Result<(), BuildError> {
    let p = head_path(paths);
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(&p, e)),
    }
}

fn write_atomic(
    path: &Path,
    bytes_or: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), BuildError> {
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("new")
    ));
    bytes_or(&tmp).map_err(|e| io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| io(path, e))
}

/// Write `snap` as the state at `head`: the snapshot, then its head record
/// (a reader that finds a head finds its snapshot), and an empty WAL if none
/// exists. `snap.tip` must be `head.number`.
pub fn write_snapshot(
    paths: &StatePaths,
    snap: &StoreSnapshot,
    head: SnapshotHead,
) -> Result<(), BuildError> {
    if snap.tip != head.number {
        return Err(io(
            &paths.snapshot,
            format!("snapshot tip {} is not head {}", snap.tip, head.number),
        ));
    }
    if let Some(dir) = paths.snapshot.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    }
    write_atomic(&paths.snapshot, |tmp| {
        snap.write_to(tmp).map_err(|e| e.to_string())
    })?;
    let hp = head_path(paths);
    write_atomic(&hp, |tmp| {
        std::fs::write(
            tmp,
            format!("{} {:#x} {:#x}\n", head.number, head.hash, head.bindings),
        )
        .map_err(|e| e.to_string())
    })?;
    if !paths.wal.exists() {
        write_empty_wal(&paths.wal).map_err(|e| io(&paths.wal, e))?;
    }
    Ok(())
}

struct FileSink<'a> {
    paths: &'a StatePaths,
    head: SnapshotHead,
}

impl SnapshotSink for FileSink<'_> {
    fn persist(&mut self, store: &StateStore, at: u64) -> liq_node::Result<()> {
        if at != self.head.number {
            tracing::error!(
                at,
                head = self.head.number,
                "replay ended off the finalized block"
            );
            return Err(liq_node::IngestError::BlockGap {
                tip: at,
                got: self.head.number,
            });
        }
        write_snapshot(self.paths, &store.snapshot(), self.head).map_err(|e| {
            tracing::error!(error = %e, "state snapshot write failed");
            liq_node::IngestError::SourceUnavailable
        })
    }
}

/// Store sizing for the replay. Capacity only; it grows as positions load.
fn replay_store(base: u64) -> StoreConfig {
    StoreConfig {
        base,
        positions: 1 << 16,
        markets: 1 << 12,
        undo: UndoCapacity {
            ops: 4096,
            extras: 512,
            rows: 512,
        },
    }
}

/// First start: replay every adapter's logs from `backfill_from` to the
/// node's finalized block and write the snapshot there. The node's own RPC
/// (`rpc_url`) serves the logs from its retained receipts.
///
/// The replay runs on its own thread and runtime (`liq-bot-replay`): the
/// fold's handler tables are not `Sync`, and a replay of this length does
/// not belong on the node's async workers. The returned future only waits
/// for that thread, so it is `Send` (the ExEx spawns it on Reth's runtime).
pub async fn build_first_snapshot(
    config_dir: &Path,
    paths: &StatePaths,
) -> Result<SnapshotHead, BuildError> {
    let dir = config_dir.to_path_buf();
    let owned = paths.clone();
    let worker = std::thread::Builder::new()
        .name("liq-bot-replay".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| BuildError::Replay(format!("replay runtime: {e}")))?;
            rt.block_on(replay_to_snapshot(&dir, &owned))
        })
        .map_err(|e| BuildError::Replay(format!("replay thread: {e}")))?;
    tokio::task::spawn_blocking(move || worker.join())
        .await
        .map_err(|e| BuildError::Replay(format!("replay join: {e}")))?
        .map_err(|_| BuildError::Replay("state replay thread panicked".into()))?
}

async fn replay_to_snapshot(
    config_dir: &Path,
    paths: &StatePaths,
) -> Result<SnapshotHead, BuildError> {
    let loaded = liq_config::boot(config_dir)
        .await
        .map_err(|e| BuildError::Config(e.to_string()))?;
    let url = loaded.config.rpc_url.clone();
    let from = loaded.config.backfill_from;
    if from == 0 {
        return Err(BuildError::Config(
            "node.toml backfill_from is 0 — set the earliest protocol deployment block".into(),
        ));
    }
    let http =
        liq_config::rpc::HttpRpc::connect(&url).map_err(|e| BuildError::Rpc(e.to_string()))?;
    let bind_block = liq_config::rpc::ChainRpc::block_number(&http)
        .await
        .map_err(|e| BuildError::Rpc(e.to_string()))?;
    let live = LiveRpc::new(http);
    let mut load = load_protocols(config_dir, &loaded.intern, Some((&live, bind_block)));
    crate::bind::retry_live_omitted(
        config_dir,
        &loaded.intern,
        Some((&live, bind_block)),
        &mut load,
        3,
    );
    if !load.omitted.is_empty() {
        let names: Vec<String> = load
            .omitted
            .iter()
            .map(|(n, why)| format!("{n}: {why}"))
            .collect();
        return Err(BuildError::Omitted(load.omitted.len(), names.join("; ")));
    }
    let protocols = leak_protocols(load);
    let bindings = bindings_fingerprint(protocols);

    let provider = alloy_provider::ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(
            url.parse()
                .map_err(|e| BuildError::Rpc(format!("invalid rpc_url: {e}")))?,
        );
    wait_until_synced(&provider).await?;
    let finalized = alloy_provider::Provider::get_block_by_number(
        &provider,
        alloy_rpc_types_eth::BlockNumberOrTag::Finalized,
    )
    .await
    .map_err(|e| BuildError::Rpc(e.to_string()))?
    .ok_or_else(|| BuildError::Rpc("node has no finalized block yet".into()))?;
    let head = SnapshotHead {
        number: finalized.header.number,
        hash: finalized.header.hash,
        bindings,
    };
    if head.number < from {
        return Err(BuildError::Config(format!(
            "finalized block {} is below backfill_from {from}",
            head.number
        )));
    }

    let subs = subscriber_refs(protocols);
    let filters: Vec<LogFilter> = protocols
        .iter()
        .flat_map(LogSubscriber::subscriptions)
        .collect();
    let router =
        LogRouter::from_subscribers(&subs).map_err(|e| BuildError::Replay(e.to_string()))?;
    let handlers = ingest_handlers(protocols);
    let refs: Vec<&dyn LogHandler> = handlers
        .iter()
        .map(|h| h.as_ref() as &dyn LogHandler)
        .collect();
    let base = from
        .checked_sub(1)
        .ok_or_else(|| BuildError::Config("backfill_from is 0".into()))?;
    let mut store = StateStore::new(replay_store(base));
    let mut arena = DecodeArena::with_capacity(1 << 20);
    let mut dirty = DirtyAccumulator::new();
    let mut ctx = ApplyCtx {
        store: &mut store,
        router: &router,
        handlers: &refs,
        arena: &mut arena,
        dirty: &mut dirty,
    };
    tracing::info!(
        from,
        to = head.number,
        addresses = filters.len(),
        protocols = protocols.len(),
        "state replay from the node's receipts — the liquidation loop starts after it"
    );
    let mut poll = liq_node::poller(provider, &filters, from, head.number).replay_only();
    let mut sink = FileSink { paths, head };
    liq_node::backfill(&mut poll, &mut ctx, from, head.number, &mut sink)
        .await
        .map_err(|e| BuildError::Replay(e.to_string()))?;
    tracing::info!(
        block = head.number,
        positions = store.len(),
        "state replay done — snapshot written"
    );
    Ok(head)
}

/// A syncing node answers `eth_getLogs` for blocks it does not have yet
/// with nothing — which would build state with those logs missing. Wait
/// until it reports synced.
async fn wait_until_synced<P: alloy_provider::Provider>(provider: &P) -> Result<(), BuildError> {
    let mut logged = false;
    loop {
        match provider.syncing().await {
            Ok(alloy_rpc_types_eth::SyncStatus::None) => return Ok(()),
            Ok(status) => {
                if !logged {
                    tracing::info!(?status, "node still syncing — state replay waits for it");
                    logged = true;
                }
            }
            Err(e) => return Err(BuildError::Rpc(e.to_string())),
        }
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

/// Writes snapshots off the hot thread. The hot thread hands over a copy
/// every `every` blocks; a write still running when the next is due is
/// skipped, not queued.
pub struct SnapshotWriter {
    tx: SyncSender<(StoreSnapshot, SnapshotHead)>,
    every: u64,
    last: u64,
    bindings: B256,
}

impl SnapshotWriter {
    /// Spawn the writer thread for `paths`. `bindings` is this process's
    /// [`bindings_fingerprint`], recorded in every head it writes.
    pub fn spawn(paths: StatePaths, every: u64, bindings: B256) -> std::io::Result<Self> {
        let (tx, rx) = sync_channel::<(StoreSnapshot, SnapshotHead)>(1);
        std::thread::Builder::new()
            .name("liq-bot-snapshot".into())
            .spawn(move || {
                while let Ok((snap, head)) = rx.recv() {
                    match write_snapshot(&paths, &snap, head) {
                        Ok(()) => tracing::info!(block = head.number, "state snapshot written"),
                        Err(e) => tracing::error!(error = %e, block = head.number, "state snapshot write failed"),
                    }
                }
            })?;
        Ok(Self {
            tx,
            every: every.max(1),
            last: 0,
            bindings,
        })
    }

    /// Called after each consistent block with the store at `number`/`hash`.
    pub fn after_block(&mut self, store: &StateStore, number: u64, hash: B256) {
        let head = SnapshotHead {
            number,
            hash,
            bindings: self.bindings,
        };
        if self.last != 0 && head.number < self.last.saturating_add(self.every) {
            return;
        }
        match self.tx.try_send((store.snapshot(), head)) {
            Ok(()) => self.last = head.number,
            Err(TrySendError::Full(_)) => {
                tracing::warn!(
                    block = head.number,
                    "previous snapshot still writing — skipped"
                );
            }
            Err(TrySendError::Disconnected(_)) => {
                tracing::error!("snapshot writer thread is gone — state is no longer persisted");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static N: AtomicU64 = AtomicU64::new(0);

    fn paths() -> StatePaths {
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("liq-state-build-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        StatePaths {
            snapshot: d.join("data").join("snapshot.bin"),
            wal: d.join("data").join("wal.log"),
        }
    }

    fn store_at(block: u64) -> StateStore {
        StateStore::new(replay_store(block))
    }

    fn is_send<T: Send>(_: &T) {}

    /// The ExEx spawns these on Reth's runtime, which needs `Send`. The
    /// `reth` binary builds only on Linux; this check runs everywhere.
    #[test]
    fn exex_futures_are_send() {
        let dir = Path::new("config");
        let p = paths();
        is_send(&build_first_snapshot(dir, &p));
        is_send(&crate::startup::run(dir, dir, &p, false));
        is_send(&crate::startup::run_on_built_state(dir, dir, &p, false));
        fn started_is_send<T: Send>() {}
        started_is_send::<crate::startup::Started>();
        // The ExEx loop's own reads: `wait_for_rpc` and the catch-up check.
        let rpc = liq_config::rpc::HttpRpc::connect("http://127.0.0.1:8545").unwrap();
        is_send(&liq_config::rpc::ChainRpc::chain_id(&rpc));
        is_send(&liq_config::rpc::ChainRpc::block_number(&rpc));
    }

    #[test]
    fn snapshot_and_head_round_trip() {
        let p = paths();
        let head = SnapshotHead {
            number: 21_000_000,
            hash: B256::repeat_byte(0xab),
            bindings: B256::repeat_byte(0xcd),
        };
        write_snapshot(&p, &store_at(head.number).snapshot(), head).unwrap();
        assert_eq!(read_head(&p).unwrap(), head);
        assert_eq!(StoreSnapshot::load(&p.snapshot).unwrap().tip, head.number);
        assert!(
            p.wal.exists(),
            "an empty WAL is written beside a first snapshot"
        );
    }

    #[test]
    fn snapshot_off_its_head_is_refused() {
        let p = paths();
        let head = SnapshotHead {
            number: 10,
            hash: B256::ZERO,
            bindings: B256::ZERO,
        };
        assert!(write_snapshot(&p, &store_at(9).snapshot(), head).is_err());
        assert!(!head_path(&p).exists());
    }

    /// The offline-bound adapters (no live RPC): the same set hashes the
    /// same, and dropping one adapter changes the hash.
    #[test]
    fn fingerprint_follows_the_bound_set() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let intern = liq_config::Intern::from_registry(
            &liq_config::Registry::from_path(&root.join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        let load = load_protocols(&root.join("config"), &intern, None);
        let all = load.protocols.as_slice();
        assert!(all.len() >= 2, "offline bind yields several adapters");
        let fp = bindings_fingerprint(all);
        let again = load_protocols(&root.join("config"), &intern, None);
        assert_eq!(fp, bindings_fingerprint(&again.protocols));
        assert_ne!(fp, bindings_fingerprint(all.get(1..).unwrap()));
        assert_ne!(fp, bindings_fingerprint(&[]));
    }

    #[test]
    fn discarded_head_is_gone_and_discarding_twice_is_fine() {
        let p = paths();
        let head = SnapshotHead {
            number: 5,
            hash: B256::ZERO,
            bindings: B256::ZERO,
        };
        write_snapshot(&p, &store_at(5).snapshot(), head).unwrap();
        discard_head(&p).unwrap();
        assert!(read_head(&p).is_err());
        discard_head(&p).unwrap();
    }

    #[test]
    fn malformed_head_is_an_error() {
        let p = paths();
        let hp = head_path(&p);
        std::fs::create_dir_all(hp.parent().unwrap()).unwrap();
        let h = format!("{:#x}", B256::ZERO);
        for bad in [
            String::new(),
            "12".into(),
            format!("12 {h}"),
            format!("12 {h} {h} extra"),
            format!("x {h} {h}"),
        ] {
            std::fs::write(&hp, &bad).unwrap();
            assert!(read_head(&p).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn writer_waits_the_interval_between_snapshots() {
        let p = paths();
        let fp = B256::repeat_byte(7);
        let mut w = SnapshotWriter::spawn(p.clone(), 100, fp).unwrap();
        let h = B256::repeat_byte(1);
        w.after_block(&store_at(1_000), 1_000, h);
        assert_eq!(w.last, 1_000);
        w.after_block(&store_at(1_050), 1_050, h);
        assert_eq!(w.last, 1_000, "inside the interval: no snapshot");
        // Let the first write finish so the channel has room.
        for _ in 0..200 {
            if read_head(&p).is_ok_and(|h| h.number == 1_000 && h.bindings == fp) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        w.after_block(&store_at(1_100), 1_100, h);
        assert_eq!(w.last, 1_100);
    }
}
