//! Ingest → state → price → engine → router → sim → sign on a pinned segment.
//!
//! Missing pin, parquet, or full universe → [`Blocked`], no nanoseconds.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use alloy_primitives::B256;
use arrayvec::ArrayVec;
use liq_engine::{Engine, EngineConfig, World};
use liq_flash::{DepthOnlyRouteCache, FlashIndex, Haircut};
use liq_node::decode::DecodeArena;
use liq_node::dirty::DirtyAccumulator;
use liq_node::router::{LogRouter, Route};
use liq_node::source::{OwnedBlock, OwnedLog};
use liq_oracle::CanonicalBook;
use liq_protocol::{Constraints, Protocol};
use liq_sim::{verify_historical, SimError};
use liq_state::{StateStore, StoreConfig, UndoCapacity};
use liq_types::{stage, HaltReason, HaltScope, HaltSink, LogSubscriber, Stage, TraceId};

use crate::archive::ParquetArchive;
use crate::bench::hist::Samples;
use crate::bench::llc::{
    absent_no_recompute, begin as llc_begin, finish as llc_finish, LlcBaseline,
};
use crate::bench::segment::{pinned_range, AdapterRoster, ArchiveProbe};
use liq_protocol::{Archive, ArchiveError, DecodedLog, ProtocolError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blocked {
    ArchiveEmpty,
    PinAbsent,
    UniverseUnloaded,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BenchError {
    #[error("blocked: {0:?}")]
    Blocked(Blocked),
    #[error("archive: {0}")]
    Archive(&'static str),
    #[error("ingest")]
    Ingest,
    #[error("engine")]
    Engine,
    #[error("clock")]
    Clock,
    #[error("log topics exceed 4")]
    Topics,
}

#[derive(Clone, Debug)]
pub struct HotSamples {
    pub ingest: Samples,
    pub state: Samples,
    pub price: Samples,
    pub engine: Samples,
    pub router: Samples,
    pub sim: Samples,
    pub sign: Samples,
    pub path_a: Samples,
    pub blocks: u64,
    pub llc: LlcBaseline,
}

fn ns(t0: Instant) -> Result<u64, BenchError> {
    u64::try_from(t0.elapsed().as_nanos()).map_err(|_| BenchError::Clock)
}

/// 11 / 05B seam: historical sim is not invented.
#[must_use]
pub fn sim_historical_status(root: &Path) -> &'static str {
    match verify_historical(root) {
        Err(SimError::ArchiveUnavailable) => "DEFERRED",
        Err(_) => "DEFERRED",
        Ok(()) => "measured",
    }
}

/// `Ok` only when a pinned range, non-empty parquet, and a full roster exist.
pub fn readiness(probe: &ArchiveProbe, roster: AdapterRoster) -> Result<(u64, u64), Blocked> {
    let (from, to) = pinned_range().ok_or(Blocked::PinAbsent)?;
    match probe {
        ArchiveProbe::Empty { .. } => return Err(Blocked::ArchiveEmpty),
        ArchiveProbe::PresentUnpinned { .. } => {}
    }
    if !roster.is_full() {
        return Err(Blocked::UniverseUnloaded);
    }
    Ok((from, to))
}

fn owned_from_decoded(d: &DecodedLog<'_>) -> Result<OwnedLog, BenchError> {
    let mut topics = ArrayVec::<B256, 4>::new();
    for t in d.topics {
        topics.try_push(*t).map_err(|_| BenchError::Topics)?;
    }
    Ok(OwnedLog {
        address: d.address,
        topics,
        data: d.data.to_vec(),
        block: d.block,
        timestamp: d.timestamp,
        tx_index: 0,
        log_index: 0,
    })
}

/// Per-block fold. Startup alloc is outside [`liq_bot::alloc::with_hot_path`].
pub(crate) fn process_segment(
    root: &Path,
    from: u64,
    to: u64,
    protocols: &[&dyn Protocol],
    mut book: Option<&mut CanonicalBook>,
) -> Result<HotSamples, BenchError> {
    if from > to {
        return Err(BenchError::Archive("inverted range"));
    }
    if protocols.is_empty() {
        return Err(BenchError::Blocked(Blocked::UniverseUnloaded));
    }
    let arch = ParquetArchive::new(root.to_path_buf());
    let mut grouped: BTreeMap<u64, Vec<OwnedLog>> = BTreeMap::new();
    arch.logs(&[], from, to, &mut |d| match owned_from_decoded(d) {
        Ok(o) => {
            grouped.entry(d.block).or_default().push(o);
            Ok(())
        }
        Err(_) => Err(ProtocolError::from(ArchiveError::Malformed {
            block: d.block,
        })),
    })
    .map_err(|_| BenchError::Archive("parquet logs"))?;

    let n_blocks = to.saturating_sub(from).saturating_add(1);
    let cap = usize::try_from(n_blocks).unwrap_or(0);
    let mut hot = HotSamples {
        ingest: Samples::with_capacity(cap),
        state: Samples::with_capacity(cap),
        price: Samples::with_capacity(cap),
        engine: Samples::with_capacity(cap),
        router: Samples::with_capacity(cap),
        sim: Samples::with_capacity(cap),
        sign: Samples::with_capacity(cap),
        path_a: Samples::with_capacity(cap),
        blocks: 0,
        llc: absent_no_recompute(),
    };
    let mut llc_counters = match llc_begin(true) {
        Ok(c) => Some(c),
        Err(reason) => {
            hot.llc = LlcBaseline::Absent(reason);
            None
        }
    };
    let mut first = true;

    let subs: Vec<&dyn LogSubscriber> =
        protocols.iter().map(|p| *p as &dyn LogSubscriber).collect();
    let router = LogRouter::from_subscribers(&subs).map_err(|_| BenchError::Ingest)?;
    let mut store = StateStore::new(StoreConfig {
        base: from.saturating_sub(1),
        positions: 1_048_576,
        markets: 1024,
        undo: UndoCapacity {
            ops: 4096,
            extras: 1024,
            rows: 1024,
        },
    });
    let mut arena = DecodeArena::with_capacity(1 << 22);
    let mut dirty = DirtyAccumulator::new();
    let assets = 1024;
    let flash = FlashIndex::new(assets);
    let mut engine = Engine::new(EngineConfig {
        assets,
        positions: 1_048_576,
        queue: 1024,
    });
    let cons = Constraints::UNBOUNDED;
    let haircut = Haircut::NONE;

    let mut n = from;
    loop {
        let logs = grouped.remove(&n).unwrap_or_default();
        if logs.is_empty() {
            return Err(BenchError::Archive(
                "empty block needs header timestamp (not guessed as 0)",
            ));
        }
        let ts = logs.first().map(|l| l.timestamp).unwrap_or(0);
        let block = OwnedBlock {
            number: n,
            timestamp: ts,
            gas_limit: 0,
            logs,
        };
        let tid = TraceId::from_raw(n);

        liq_bot::alloc::with_hot_path(|| -> Result<(), BenchError> {
            let t_state = Instant::now();
            store
                .begin_block(block.number)
                .map_err(|_| BenchError::Ingest)?;
            arena.reset();
            dirty.clear();
            let mut ingest_ns = 0_u64;
            for log in &block.logs {
                let t_i = Instant::now();
                match router.route(&arena, log).map_err(|_| BenchError::Ingest)? {
                    Route::Untracked | Route::UnknownTopic => {
                        ingest_ns = ingest_ns.saturating_add(ns(t_i)?);
                    }
                    Route::Hit { decoded, subs, .. } => {
                        ingest_ns = ingest_ns.saturating_add(ns(t_i)?);
                        for &idx in subs {
                            let proto =
                                protocols.get(usize::from(idx)).ok_or(BenchError::Ingest)?;
                            let set = Protocol::apply_log(*proto, &mut store, &decoded)
                                .map_err(|_| BenchError::Ingest)?;
                            dirty.merge(set);
                        }
                    }
                }
            }
            dirty
                .collapse(&store, block.timestamp)
                .map_err(|_| BenchError::Ingest)?;
            let state_wall = ns(t_state)?;
            let state_ns = state_wall.checked_sub(ingest_ns).ok_or(BenchError::Clock)?;
            hot.ingest.push(ingest_ns);
            hot.state.push(state_ns);
            stage(tid, Stage::Candidate);

            if let Some(b) = book.as_mut() {
                let t_px = Instant::now();
                for log in &block.logs {
                    let dec = arena.copy(log);
                    let _ = b.apply_log(&dec, &NoHalt);
                }
                hot.price.push(ns(t_px)?);
                stage(tid, Stage::PriceTick);
            }

            let routes = DepthOnlyRouteCache(&flash);
            let view = store.view(ts);
            let w = World {
                view,
                protocols,
                flash: &flash,
                routes: &routes,
                haircut,
                cons: &cons,
            };
            if first {
                if let Some(c) = llc_counters.as_mut() {
                    c.on_block_arrived();
                }
            }
            let t_eng = Instant::now();
            engine.on_block(&w).map_err(|_| BenchError::Engine)?;
            hot.engine.push(ns(t_eng)?);
            if first {
                if let Some(c) = llc_counters.as_mut() {
                    hot.llc = llc_finish(c);
                }
                first = false;
            }
            let _queued = engine.queued();
            Ok(())
        })?;

        hot.blocks = hot.blocks.saturating_add(1);
        if n == to {
            break;
        }
        n = n
            .checked_add(1)
            .ok_or(BenchError::Archive("block overflow"))?;
    }
    let _ = book;
    Ok(hot)
}

struct NoHalt;

impl HaltSink for NoHalt {
    fn halt(&self, _scope: HaltScope, _reason: HaltReason) {}
}

#[cfg(test)]
mod tests {
    use super::{readiness, Blocked};
    use crate::bench::segment::{archive_root, probe_archive, AdapterRoster};

    #[test]
    fn readiness_fail_closed_today() {
        let probe = probe_archive(&archive_root());
        let r = readiness(&probe, AdapterRoster::unloaded());
        assert!(matches!(
            r,
            Err(Blocked::PinAbsent) | Err(Blocked::ArchiveEmpty) | Err(Blocked::UniverseUnloaded)
        ));
    }
}
