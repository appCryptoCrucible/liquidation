//! Pinned hot thread: drain the ExEx ring, fold via 03A, confirm height
//! (GUIDE 03 §1 / §4b; RUST-CONV §5.1).
//!
//! 16A pinning is a seam: the thread is named `liq-node-hot`. `pin_to_core`
//! is owned by `liq-bot` (cores.toml). This module does not take `liq-bot`
//! as a dependency.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{Builder, JoinHandle};

use liq_state::StateStore;
use liq_types::{HaltSink, ProtocolId};

use crate::apply::ApplyCtx;
use crate::decode::DecodeArena;
use crate::dirty::DirtyAccumulator;
use crate::exex::{ConsistentHeight, HotIngress};
use crate::reorg::handle_notification;
use crate::router::LogRouter;
use crate::{IngestError, Result};

/// GUIDE 16 / 16A name. Pinning is wired in `liq-bot` when A1 topology is live.
pub const HOT_THREAD_NAME: &str = "liq-node-hot";

/// One drain of the ExEx ring. Arena and dirty buffers are reused.
pub fn drain(
    ingress: &mut HotIngress,
    ctx: &mut ApplyCtx<'_>,
    sink: &dyn HaltSink,
    protocols: &[ProtocolId],
    height: &ConsistentHeight,
) -> Result<u32> {
    let mut n = 0u32;
    while let Some(notif) = ingress.pop() {
        n = n.saturating_add(1);
        if let Some(done) = handle_notification(ctx, &notif, sink, protocols)? {
            height.publish(done);
            ingress.confirm(done)?;
        }
        for block in match notif {
            crate::exex::Notification::Committed { new }
            | crate::exex::Notification::Reorged { new, .. } => new.blocks,
            crate::exex::Notification::Reverted { .. } => Vec::new(),
        } {
            ingress.recycle_block(block);
        }
    }
    Ok(n)
}

/// Owned join handle. Not `Arc<Mutex<Thread>>`.
pub struct HotHandle {
    handle: JoinHandle<Result<()>>,
    stop: Arc<AtomicBool>,
}

impl HotHandle {
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn join(self) -> std::thread::Result<Result<()>> {
        self.request_stop();
        self.handle.join()
    }
}

/// Inputs to [`spawn`]. Grouped so the call stays under clippy's arity cap.
pub struct HotSpawn {
    pub store: StateStore,
    pub router: LogRouter,
    pub handlers: Vec<Box<dyn crate::apply::LogHandler + Send>>,
    pub ingress: HotIngress,
    pub sink: &'static dyn HaltSink,
    pub protocols: Box<[ProtocolId]>,
    pub height: Arc<ConsistentHeight>,
    pub pin: fn() -> core::result::Result<(), IngestError>,
}

/// Spawn the named hot thread. `pin` is the 16A seam (`Ok` = already pinned
/// or pin skipped). Failure of `pin` is logged and the thread still runs —
/// A1 topology is not live on this builder's box (D60).
pub fn spawn(cfg: HotSpawn) -> Result<HotHandle> {
    let HotSpawn {
        mut store,
        router,
        handlers,
        mut ingress,
        sink,
        protocols,
        height,
        pin,
    } = cfg;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = Arc::clone(&stop);
    let handle = Builder::new()
        .name(HOT_THREAD_NAME.into())
        .spawn(move || {
            if let Err(e) = pin() {
                tracing::error!(?e, "liq-node-hot pin seam failed; running unpinned (16A)");
            }
            let mut arena = DecodeArena::with_capacity(1 << 20);
            let mut dirty = DirtyAccumulator::new();
            let refs: Vec<&dyn crate::apply::LogHandler> =
                handlers.iter().map(|h| h.as_ref() as _).collect();
            loop {
                if stop_t.load(Ordering::Acquire) {
                    break;
                }
                {
                    let mut ctx = ApplyCtx {
                        store: &mut store,
                        router: &router,
                        handlers: &refs,
                        arena: &mut arena,
                        dirty: &mut dirty,
                    };
                    drain(&mut ingress, &mut ctx, sink, &protocols, height.as_ref())?;
                }
                std::hint::spin_loop();
            }
            Ok(())
        })
        .map_err(|_| {
            tracing::error!("failed to spawn {HOT_THREAD_NAME}");
            IngestError::HotStalled
        })?;
    Ok(HotHandle { handle, stop })
}

/// 16A pin placeholder: always Ok. `liq-bot::exex_install` replaces this.
pub fn pin_deferred() -> core::result::Result<(), IngestError> {
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::{drain, HOT_THREAD_NAME};
    use crate::apply::{ApplyCtx, LogHandler};
    use crate::decode::DecodeArena;
    use crate::dirty::DirtyAccumulator;
    use crate::exex::{split_exex, ConsistentHeight, Notification, NumHash, OwnedChain};
    use crate::router::LogRouter;
    use crate::source::OwnedBlock;
    use alloy_primitives::B256;
    use liq_protocol::DirtySet;
    use liq_state::{StateStore, StoreConfig, UndoCapacity};
    use liq_types::{HaltReason, HaltScope, HaltSink, LogFilter, LogSubscriber, ProtocolId};
    use std::sync::Arc;

    struct Nop;
    impl LogSubscriber for Nop {
        fn subscriptions(&self) -> Vec<LogFilter> {
            Vec::new()
        }
    }
    impl LogHandler for Nop {
        fn apply_log(
            &self,
            _st: &mut dyn liq_protocol::StateWriter,
            _log: &liq_protocol::DecodedLog<'_>,
        ) -> core::result::Result<DirtySet, liq_protocol::ProtocolError> {
            Ok(DirtySet::None)
        }
    }
    struct Sink;
    impl HaltSink for Sink {
        fn halt(&self, _s: HaltScope, _r: HaltReason) {}
    }

    fn cfg() -> StoreConfig {
        StoreConfig {
            base: 0,
            positions: 4,
            markets: 2,
            undo: UndoCapacity {
                ops: 8,
                extras: 2,
                rows: 2,
            },
        }
    }

    #[test]
    fn thread_name_is_liq_node_hot() {
        assert_eq!(HOT_THREAD_NAME, "liq-node-hot");
    }

    /// Two commits: FinishedHeight order matches apply order; height 2 is
    /// not visible until block 2 is consistent.
    #[test]
    fn drain_confirms_in_order() {
        let rec = Nop;
        let handlers: [&dyn LogHandler; 1] = [&rec];
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let mut store = StateStore::new(cfg());
        let mut arena = DecodeArena::with_capacity(64);
        let mut dirty = DirtyAccumulator::new();
        let (mut fwd, mut hot) = split_exex();
        let height = ConsistentHeight::new(NumHash {
            number: 0,
            hash: B256::ZERO,
        });
        let sink = Sink;
        let ids = [ProtocolId(1)];
        let mk = |n: u64| Notification::Committed {
            new: OwnedChain {
                blocks: vec![OwnedBlock {
                    number: n,
                    timestamp: n,
                    logs: Vec::new(),
                }],
                tip: NumHash {
                    number: n,
                    hash: B256::repeat_byte(n as u8),
                },
            },
        };
        fwd.push(mk(1)).unwrap();
        {
            let mut ctx = ApplyCtx {
                store: &mut store,
                router: &router,
                handlers: &handlers,
                arena: &mut arena,
                dirty: &mut dirty,
            };
            drain(&mut hot, &mut ctx, &sink, &ids, &height).unwrap();
        }
        assert_eq!(height.load().num_hash.number, 1);
        assert_eq!(fwd.take_finished().unwrap().num_hash.number, 1);
        assert!(fwd.take_finished().is_none());
        fwd.push(mk(2)).unwrap();
        {
            let mut ctx = ApplyCtx {
                store: &mut store,
                router: &router,
                handlers: &handlers,
                arena: &mut arena,
                dirty: &mut dirty,
            };
            drain(&mut hot, &mut ctx, &sink, &ids, &height).unwrap();
        }
        assert_eq!(height.load().num_hash.number, 2);
        assert_eq!(fwd.take_finished().unwrap().num_hash.number, 2);
        let _ = Arc::new(height);
    }
}
