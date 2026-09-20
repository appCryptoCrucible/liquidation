//! ExEx registration + `liq-node-hot` spawn seam (WP 03B, GUIDE 03 §1).
//!
//! A2 (Reth in-process) is deferred (D60). 16A pinning is deferred: this
//! process names the thread [`HOT_THREAD_NAME`] and leaves `pin_to_core` to
//! [`crate::threads`] once `cores.toml` matches live `shared_cpu_list`.
//! 17A binds the forwarder to `ExExContext`.

use std::thread::{Builder, JoinHandle};

use liq_node::{
    pin_deferred, spawn_hot, split_exex, split_mempool, ConsistentHeight, ExExForwarder, HotHandle,
    HotIngress, HotSpawn, MempoolProducer, HOT_THREAD_NAME,
};
use liq_types::PendingTx;
use rtrb::Consumer;

/// Rings the Reth ExEx future (17A) and the hot thread share.
pub struct ExExInstall {
    pub forwarder: ExExForwarder,
    pub ingress: HotIngress,
    pub mempool: MempoolProducer,
    pub mempool_rx: Consumer<PendingTx>,
    pub height: std::sync::Arc<ConsistentHeight>,
}

/// Split ExEx + mempool rings. Does not spawn; [`install_hot`] does.
#[must_use]
pub fn prepare() -> ExExInstall {
    let (forwarder, ingress) = split_exex();
    let (mempool, mempool_rx) = split_mempool();
    let height = std::sync::Arc::new(ConsistentHeight::new(liq_node::NumHash {
        number: 0,
        hash: alloy_primitives::B256::ZERO,
    }));
    ExExInstall {
        forwarder,
        ingress,
        mempool,
        mempool_rx,
        height,
    }
}

/// Named `liq-node-hot`. Pin is [`pin_deferred`] until 16A wires
/// [`crate::threads::pin_to_core`].
pub fn install_hot(
    store: liq_state::StateStore,
    router: liq_node::LogRouter,
    handlers: Vec<Box<dyn liq_node::LogHandler + Send>>,
    ingress: HotIngress,
    sink: &'static dyn liq_types::HaltSink,
    protocols: Box<[liq_types::ProtocolId]>,
    height: std::sync::Arc<ConsistentHeight>,
) -> liq_node::Result<HotHandle> {
    spawn_hot(HotSpawn {
        store,
        router,
        handlers,
        ingress,
        sink,
        protocols,
        height,
        pin: pin_deferred,
    })
}

/// Spawn a named hot thread without the full ingest graph (tests / 16A).
pub fn spawn_named<F>(f: F) -> std::io::Result<JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    Builder::new().name(HOT_THREAD_NAME.into()).spawn(f)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{prepare, spawn_named, HOT_THREAD_NAME};

    #[test]
    fn prepare_splits_rings() {
        let mut inst = prepare();
        assert!(inst.forwarder.take_finished().is_none());
        assert_eq!(inst.mempool.dropped(), 0);
    }

    #[test]
    fn named_thread_is_liq_node_hot() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let h = spawn_named(move || {
            tx.send(std::thread::current().name().map(str::to_owned))
                .unwrap();
        })
        .unwrap();
        let name = rx.recv().unwrap();
        h.join().unwrap();
        assert_eq!(name.as_deref(), Some(HOT_THREAD_NAME));
    }
}
