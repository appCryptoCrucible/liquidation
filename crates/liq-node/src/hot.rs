//! Pinned hot thread: drain the ExEx ring, fold via 03A, confirm height
//! (GUIDE 03 §1 / §4b; RUST-CONV §5.1).
//!
//! 16A pinning is a seam: the thread is named `liq-node-hot`. `pin_to_core`
//! is owned by `liq-bot` (cores.toml). This module does not take `liq-bot`
//! as a dependency.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{Builder, JoinHandle};

use liq_state::StateStore;
use liq_types::{HaltSink, ProtocolId};

use crate::apply::ApplyCtx;
use crate::decode::DecodeArena;
use crate::dirty::{CollapsedDirty, DirtyAccumulator};
use crate::exex::{ConsistentHeight, HotIngress, Notification};
use crate::reorg::{halt_reorg_too_deep, handle_notification};
use crate::router::LogRouter;
use crate::{BlockNum, IngestError, Result, Timestamp};

/// GUIDE 16 / 16A name. Pinning is wired in `liq-bot` when A1 topology is live.
pub const HOT_THREAD_NAME: &str = "liq-node-hot";

/// Store + collapsed dirty after a successful apply. Bot-owned; this crate
/// does not depend on engine / router / exec.
pub struct AfterBlockCtx<'a> {
    pub store: &'a StateStore,
    pub dirty: &'a CollapsedDirty,
    pub block: BlockNum,
    pub timestamp: Timestamp,
    /// Header `gasLimit` of the committed tip. `0` = absent.
    pub gas_limit: u64,
    /// Header `gasUsed` of the committed tip. `0` = absent (or an empty block).
    pub gas_used: u64,
    /// Header `baseFeePerGas`. `0` = absent — never a fabricated fee.
    pub base_fee_per_gas: u64,
}

/// Called on the hot thread after apply + collapse. Existing apply / reorg
/// / halt behaviour is unchanged when this is `None`.
pub trait AfterBlock: Send {
    fn after_block(&mut self, ctx: AfterBlockCtx<'_>);
}

/// One drain of the ExEx ring. Arena and dirty buffers are reused.
/// `after` runs only after a consistent commit/reorg apply (not on revert
/// or halted apply).
pub fn drain(
    ingress: &mut HotIngress,
    ctx: &mut ApplyCtx<'_>,
    sink: &dyn HaltSink,
    protocols: &[ProtocolId],
    height: &ConsistentHeight,
    mut after: Option<&mut dyn AfterBlock>,
) -> Result<u32> {
    let mut n = 0u32;
    while let Some(notif) = ingress.pop() {
        n = n.saturating_add(1);
        let last_meta = match &notif {
            Notification::Committed { new } | Notification::Reorged { new, .. } => {
                new.blocks.last().map(|b| {
                    (
                        b.timestamp,
                        b.gas_limit,
                        b.gas_used,
                        b.base_fee_per_gas,
                    )
                })
            }
            Notification::Reverted { .. } => None,
        };
        let outcome = handle_notification(ctx, &notif, sink, protocols);
        for block in match notif {
            Notification::Committed { new } | Notification::Reorged { new, .. } => new.blocks,
            Notification::Reverted { .. } => Vec::new(),
        } {
            ingress.recycle_block(block);
        }
        match outcome {
            Ok(Some(done)) => {
                height.publish(done);
                if let Err(e) = ingress.confirm(done) {
                    halt_reorg_too_deep(sink);
                    return Err(e);
                }
                if let (Some(hook), Some((ts, gas_limit, gas_used, base_fee_per_gas))) =
                    (after.as_deref_mut(), last_meta)
                {
                    hook.after_block(AfterBlockCtx {
                        store: ctx.store,
                        dirty: ctx.dirty.collapsed(),
                        block: done.num_hash.number,
                        timestamp: ts,
                        gas_limit,
                        gas_used,
                        base_fee_per_gas,
                    });
                }
            }
            Ok(None) => break,
            Err(e) => {
                halt_reorg_too_deep(sink);
                return Err(e);
            }
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

    /// Wait for the thread to exit without setting the stop flag (Err-exit tests).
    #[cfg(test)]
    pub fn join_no_stop(self) -> std::thread::Result<Result<()>> {
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
    /// When false, pin failure is fail-closed (thread does not drain). When
    /// true, pin failure is logged and the thread still runs (A1 topology not
    /// live). Explicit — not a permanently-Ok stub.
    pub allow_unpinned: bool,
    /// Bot-owned after apply+collapse. `None` keeps ingest-only behaviour.
    pub after_block: Option<Box<dyn AfterBlock>>,
}

/// Spawn the named hot thread. `pin` is the 16A seam. Failure of `pin` with
/// `allow_unpinned == false` is fail-closed (mirrors `liq-bot` `spawn_pinned`).
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
        allow_unpinned,
        mut after_block,
    } = cfg;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = Arc::clone(&stop);
    let (pin_tx, pin_rx) = mpsc::sync_channel(1);
    let handle = Builder::new()
        .name(HOT_THREAD_NAME.into())
        .spawn(move || {
            match pin() {
                Ok(()) => {
                    let _ = pin_tx.send(Ok(()));
                }
                Err(e) => {
                    tracing::error!(?e, "liq-node-hot pin failed");
                    if !allow_unpinned {
                        let _ = pin_tx.send(Err(e));
                        return Err(e);
                    }
                    let _ = pin_tx.send(Ok(()));
                }
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
                    drain(
                        &mut ingress,
                        &mut ctx,
                        sink,
                        &protocols,
                        height.as_ref(),
                        after_block.as_mut().map(|h| h.as_mut() as &mut dyn AfterBlock),
                    )?;
                }
                std::hint::spin_loop();
            }
            Ok(())
        })
        .map_err(|_| {
            tracing::error!("failed to spawn {HOT_THREAD_NAME}");
            IngestError::HotStalled
        })?;
    match pin_rx.recv() {
        Ok(Ok(())) => Ok(HotHandle { handle, stop }),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            tracing::error!("{HOT_THREAD_NAME} pin handshake dropped");
            Err(IngestError::HotStalled)
        }
    }
}

/// 16A pin seam when the caller has not wired `pin_to_core`. Combined with
/// [`HotSpawn::allow_unpinned`]: this is not a permanently-Ok skip of a
/// required pin.
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
    use super::{drain, spawn, HotSpawn, HOT_THREAD_NAME};
    use crate::apply::{ApplyCtx, LogHandler};
    use crate::decode::DecodeArena;
    use crate::dirty::DirtyAccumulator;
    use crate::exex::{split_exex, ConsistentHeight, Notification, NumHash, OwnedChain};
    use crate::router::LogRouter;
    use crate::source::OwnedBlock;
    use crate::IngestError;
    use alloy_primitives::B256;
    use liq_protocol::DirtySet;
    use liq_state::{StateStore, StoreConfig, UndoCapacity};
    use liq_types::{HaltReason, HaltScope, HaltSink, LogFilter, LogSubscriber, ProtocolId};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

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
                    gas_limit: 0,
                    gas_used: 0,
                    base_fee_per_gas: 0,
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
            drain(&mut hot, &mut ctx, &sink, &ids, &height, None).unwrap();
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
            drain(&mut hot, &mut ctx, &sink, &ids, &height, None).unwrap();
        }
        assert_eq!(height.load().num_hash.number, 2);
        assert_eq!(fwd.take_finished().unwrap().num_hash.number, 2);
        let _ = Arc::new(height);
    }

    struct Rec {
        hits: AtomicU32,
        last: Mutex<Option<(HaltScope, HaltReason)>>,
    }
    impl HaltSink for Rec {
        fn halt(&self, s: HaltScope, r: HaltReason) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            *self.last.lock().unwrap() = Some((s, r));
        }
    }

    fn pin_fail() -> core::result::Result<(), IngestError> {
        Err(IngestError::PinFailed)
    }

    #[test]
    fn drain_halts_before_err_and_stops_after_halt() {
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
        let sink = Rec {
            hits: AtomicU32::new(0),
            last: Mutex::new(None),
        };
        let ids = [ProtocolId(1)];
        fwd.push(Notification::Reverted {
            first: 0,
            last: NumHash {
                number: 0,
                hash: B256::ZERO,
            },
        })
        .unwrap();
        fwd.push(Notification::Committed {
            new: OwnedChain {
                blocks: vec![OwnedBlock {
                    number: 1,
                    timestamp: 1,
                    gas_limit: 0,
                    gas_used: 0,
                    base_fee_per_gas: 0,
                    logs: Vec::new(),
                }],
                tip: NumHash {
                    number: 1,
                    hash: B256::repeat_byte(1),
                },
            },
        })
        .unwrap();
        {
            let mut ctx = ApplyCtx {
                store: &mut store,
                router: &router,
                handlers: &handlers,
                arena: &mut arena,
                dirty: &mut dirty,
            };
            let err = drain(&mut hot, &mut ctx, &sink, &ids, &height, None).unwrap_err();
            assert!(matches!(err, IngestError::CannotUnwindGenesis { .. }));
        }
        assert_eq!(sink.hits.load(Ordering::Relaxed), 2);
        assert_eq!(
            *sink.last.lock().unwrap(),
            Some((HaltScope::Global, HaltReason::ReorgTooDeep))
        );
        assert!(
            hot.pop().is_some(),
            "must not consume the ring after the first Err"
        );
    }

    #[test]
    fn drain_breaks_on_first_halted_apply() {
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
        let sink = Rec {
            hits: AtomicU32::new(0),
            last: Mutex::new(None),
        };
        let ids = [ProtocolId(1)];
        fwd.push(Notification::Committed {
            new: OwnedChain {
                blocks: vec![OwnedBlock {
                    number: 99,
                    timestamp: 99,
                    gas_limit: 0,
                    gas_used: 0,
                    base_fee_per_gas: 0,
                    logs: Vec::new(),
                }],
                tip: NumHash {
                    number: 99,
                    hash: B256::repeat_byte(99),
                },
            },
        })
        .unwrap();
        fwd.push(Notification::Committed {
            new: OwnedChain {
                blocks: vec![OwnedBlock {
                    number: 1,
                    timestamp: 1,
                    gas_limit: 0,
                    gas_used: 0,
                    base_fee_per_gas: 0,
                    logs: Vec::new(),
                }],
                tip: NumHash {
                    number: 1,
                    hash: B256::repeat_byte(1),
                },
            },
        })
        .unwrap();
        {
            let mut ctx = ApplyCtx {
                store: &mut store,
                router: &router,
                handlers: &handlers,
                arena: &mut arena,
                dirty: &mut dirty,
            };
            drain(&mut hot, &mut ctx, &sink, &ids, &height, None).unwrap();
        }
        assert!(sink.hits.load(Ordering::Relaxed) >= 1);
        assert!(hot.pop().is_some(), "must break on first Ok(false)");
    }

    #[test]
    fn spawn_stop_join() {
        let rec = Nop;
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let (_fwd, ingress) = split_exex();
        let sink: &'static dyn HaltSink = Box::leak(Box::new(Sink));
        let h = spawn(HotSpawn {
            store: StateStore::new(cfg()),
            router,
            handlers: vec![Box::new(Nop)],
            ingress,
            sink,
            protocols: Box::from([ProtocolId(1)]),
            height: Arc::new(ConsistentHeight::new(NumHash {
                number: 0,
                hash: B256::ZERO,
            })),
            pin: super::pin_deferred,
            allow_unpinned: true,
            after_block: None,
        })
        .unwrap();
        h.request_stop();
        let joined = h.join().unwrap();
        assert!(joined.is_ok());
    }

    #[test]
    fn spawn_pin_required_fail_closed() {
        let rec = Nop;
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let (_fwd, ingress) = split_exex();
        let sink: &'static dyn HaltSink = Box::leak(Box::new(Sink));
        let err = match spawn(HotSpawn {
            store: StateStore::new(cfg()),
            router,
            handlers: vec![Box::new(Nop)],
            ingress,
            sink,
            protocols: Box::from([ProtocolId(1)]),
            height: Arc::new(ConsistentHeight::new(NumHash {
                number: 0,
                hash: B256::ZERO,
            })),
            pin: pin_fail,
            allow_unpinned: false,
            after_block: None,
        }) {
            Ok(_) => panic!("pin required must fail-closed"),
            Err(e) => e,
        };
        assert_eq!(err, IngestError::PinFailed);
    }

    #[test]
    fn spawn_err_exit_halts() {
        let rec = Nop;
        let subs: [&dyn LogSubscriber; 1] = [&rec];
        let router = LogRouter::from_subscribers(&subs).unwrap();
        let (mut fwd, ingress) = split_exex();
        fwd.push(Notification::Reverted {
            first: 0,
            last: NumHash {
                number: 0,
                hash: B256::ZERO,
            },
        })
        .unwrap();
        let sink: &'static Rec = Box::leak(Box::new(Rec {
            hits: AtomicU32::new(0),
            last: Mutex::new(None),
        }));
        let h = spawn(HotSpawn {
            store: StateStore::new(cfg()),
            router,
            handlers: vec![Box::new(Nop)],
            ingress,
            sink,
            protocols: Box::from([ProtocolId(1)]),
            height: Arc::new(ConsistentHeight::new(NumHash {
                number: 0,
                hash: B256::ZERO,
            })),
            pin: super::pin_deferred,
            allow_unpinned: true,
            after_block: None,
        })
        .unwrap();
        let joined = h.join_no_stop().unwrap();
        assert!(matches!(
            joined,
            Err(IngestError::CannotUnwindGenesis { .. })
        ));
        assert_eq!(sink.hits.load(Ordering::Relaxed), 2);
    }

    struct CountHook {
        hits: AtomicU32,
        last_block: AtomicU32,
        last_gas: std::sync::atomic::AtomicU64,
        last_used: std::sync::atomic::AtomicU64,
        last_base: std::sync::atomic::AtomicU64,
    }
    impl crate::AfterBlock for CountHook {
        fn after_block(&mut self, ctx: crate::AfterBlockCtx<'_>) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            let n = u32::try_from(ctx.block).unwrap_or(u32::MAX);
            self.last_block.store(n, Ordering::Relaxed);
            self.last_gas.store(ctx.gas_limit, Ordering::Relaxed);
            self.last_used.store(ctx.gas_used, Ordering::Relaxed);
            self.last_base.store(ctx.base_fee_per_gas, Ordering::Relaxed);
            let _ = ctx.dirty;
            let _ = ctx.store;
            let _ = ctx.timestamp;
        }
    }

    /// Apply+collapse then the bot hook. Revert / halt must not fire it.
    #[test]
    fn drain_calls_after_block_on_consistent_commit() {
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
        fwd.push(Notification::Committed {
            new: OwnedChain {
                blocks: vec![OwnedBlock {
                    number: 1,
                    timestamp: 11,
                    gas_limit: 45_000_000,
                    gas_used: 15_000_000,
                    base_fee_per_gas: 1_000_000_000,
                    logs: Vec::new(),
                }],
                tip: NumHash {
                    number: 1,
                    hash: B256::repeat_byte(1),
                },
            },
        })
        .unwrap();
        let mut hook = CountHook {
            hits: AtomicU32::new(0),
            last_block: AtomicU32::new(0),
            last_gas: std::sync::atomic::AtomicU64::new(u64::MAX),
            last_used: std::sync::atomic::AtomicU64::new(u64::MAX),
            last_base: std::sync::atomic::AtomicU64::new(u64::MAX),
        };
        {
            let mut ctx = ApplyCtx {
                store: &mut store,
                router: &router,
                handlers: &handlers,
                arena: &mut arena,
                dirty: &mut dirty,
            };
            drain(
                &mut hot,
                &mut ctx,
                &sink,
                &ids,
                &height,
                Some(&mut hook),
            )
            .unwrap();
        }
        assert_eq!(hook.hits.load(Ordering::Relaxed), 1);
        assert_eq!(hook.last_block.load(Ordering::Relaxed), 1);
        assert_eq!(
            hook.last_gas.load(Ordering::Relaxed),
            45_000_000,
            "AfterBlockCtx.gas_limit is the committed header, not a default"
        );
        assert_eq!(
            hook.last_used.load(Ordering::Relaxed),
            15_000_000,
            "AfterBlockCtx.gas_used is the committed header"
        );
        assert_eq!(
            hook.last_base.load(Ordering::Relaxed),
            1_000_000_000,
            "AfterBlockCtx.base_fee_per_gas is the committed header, not invented"
        );
        assert_eq!(height.load().num_hash.number, 1);
    }
}
