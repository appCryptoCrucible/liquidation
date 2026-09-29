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
//! Files: `snapshot.bin` (the store), `snapshot.head` (its block number and
//! hash), `wal.log` (kept empty: Reth's replay replaces it). Each is written
//! to a temporary file and renamed into place.
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

use crate::bind::{ingest_handlers, leak_protocols, load_protocols, subscriber_refs};
use crate::lease::{write_empty_wal, StatePaths};
use crate::live_rpc::LiveRpc;

/// The block a snapshot holds the state after.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SnapshotHead {
    pub number: u64,
    pub hash: B256,
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

/// Read the head record: `<number> <hash>`.
pub fn read_head(paths: &StatePaths) -> Result<SnapshotHead, BuildError> {
    let p = head_path(paths);
    let raw = std::fs::read_to_string(&p).map_err(|e| io(&p, e))?;
    let mut it = raw.split_whitespace();
    let (Some(n), Some(h), None) = (it.next(), it.next(), it.next()) else {
        return Err(io(&p, "expected `<number> <hash>`"));
    };
    Ok(SnapshotHead {
        number: n.parse().map_err(|e| io(&p, e))?,
        hash: h.parse().map_err(|e| io(&p, e))?,
    })
}

fn write_atomic(path: &Path, bytes_or: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), BuildError> {
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
pub fn write_snapshot(paths: &StatePaths, snap: &StoreSnapshot, head: SnapshotHead) -> Result<(), BuildError> {
    if snap.tip != head.number {
        return Err(io(
            &paths.snapshot,
            format!("snapshot tip {} is not head {}", snap.tip, head.number),
        ));
    }
    if let Some(dir) = paths.snapshot.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    }
    write_atomic(&paths.snapshot, |tmp| snap.write_to(tmp).map_err(|e| e.to_string()))?;
    let hp = head_path(paths);
    write_atomic(&hp, |tmp| {
        std::fs::write(tmp, format!("{} {:#x}\n", head.number, head.hash)).map_err(|e| e.to_string())
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
            tracing::error!(at, head = self.head.number, "replay ended off the finalized block");
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
pub async fn build_first_snapshot(config_dir: &Path, paths: &StatePaths) -> Result<SnapshotHead, BuildError> {
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
    let http = liq_config::rpc::HttpRpc::connect(&url).map_err(|e| BuildError::Rpc(e.to_string()))?;
    let bind_block = liq_config::rpc::ChainRpc::block_number(&http)
        .await
        .map_err(|e| BuildError::Rpc(e.to_string()))?;
    let live = LiveRpc::new(http);
    let load = load_protocols(config_dir, &loaded.intern, Some((&live, bind_block)));
    if !load.omitted.is_empty() {
        let names: Vec<String> = load.omitted.iter().map(|(n, why)| format!("{n}: {why}")).collect();
        return Err(BuildError::Omitted(load.omitted.len(), names.join("; ")));
    }
    let protocols = leak_protocols(load);

    let provider = alloy_provider::ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(url.parse().map_err(|e| BuildError::Rpc(format!("invalid rpc_url: {e}")))?);
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
    };
    if head.number < from {
        return Err(BuildError::Config(format!(
            "finalized block {} is below backfill_from {from}",
            head.number
        )));
    }

    let subs = subscriber_refs(protocols);
    let filters: Vec<LogFilter> = protocols.iter().flat_map(LogSubscriber::subscriptions).collect();
    let router = LogRouter::from_subscribers(&subs).map_err(|e| BuildError::Replay(e.to_string()))?;
    let handlers = ingest_handlers(protocols);
    let refs: Vec<&dyn LogHandler> = handlers.iter().map(|h| h.as_ref() as &dyn LogHandler).collect();
    let base = from.checked_sub(1).ok_or_else(|| BuildError::Config("backfill_from is 0".into()))?;
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
}

impl SnapshotWriter {
    /// Spawn the writer thread for `paths`.
    pub fn spawn(paths: StatePaths, every: u64) -> std::io::Result<Self> {
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
        })
    }

    /// Called after each consistent block with the store at `head`.
    pub fn after_block(&mut self, store: &StateStore, head: SnapshotHead) {
        if self.last != 0 && head.number < self.last.saturating_add(self.every) {
            return;
        }
        match self.tx.try_send((store.snapshot(), head)) {
            Ok(()) => self.last = head.number,
            Err(TrySendError::Full(_)) => {
                tracing::warn!(block = head.number, "previous snapshot still writing — skipped");
            }
            Err(TrySendError::Disconnected(_)) => {
                tracing::error!("snapshot writer thread is gone — state is no longer persisted");
            }
        }
    }
}
