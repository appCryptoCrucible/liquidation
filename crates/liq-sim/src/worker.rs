//! Thread-per-core sim workers. Pinning is WP 16A (`pin_to_core` deferred).
//! Each worker owns revm + CacheDB; SPSC `rtrb` both ways (GUIDE 11 Step 4c).

use crate::verify::{verify, Bundle};
use crate::warm::Simulator;
use crate::{SimError, SimId, SimOutcome};
use revm::context::BlockEnv;
use revm::database_interface::DatabaseRef;
use rtrb::{Consumer, Producer, RingBuffer};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::thread::{Builder, JoinHandle};

const RING: usize = 64;

/// Request on the worker inbox.
#[derive(Clone, Debug)]
pub struct SimRequest {
    pub id: SimId,
    pub bundle: Bundle,
    pub block: BlockEnv,
}

/// Reply on the worker outbox.
#[derive(Clone, Debug)]
pub struct SimReply {
    pub id: SimId,
    pub outcome: Result<SimOutcome, SimError>,
}

pub struct SimWorker<P: DatabaseRef<Error = SimError> + Send + Sync + 'static> {
    sim: Simulator<P>,
    inbox: Consumer<SimRequest>,
    outbox: Producer<SimReply>,
}

impl<P: DatabaseRef<Error = SimError> + Send + Sync + 'static> SimWorker<P> {
    fn run(mut self) {
        loop {
            match self.inbox.pop() {
                Ok(req) => {
                    let outcome = catch_unwind(AssertUnwindSafe(|| {
                        verify(&mut self.sim, &req.bundle, req.block.clone())
                    }))
                    .unwrap_or_else(|_| Err(SimError::WorkerPanic));
                    if self
                        .outbox
                        .push(SimReply {
                            id: req.id,
                            outcome,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(rtrb::PopError::Empty) => {
                    if self.inbox.is_abandoned() {
                        break;
                    }
                    std::thread::yield_now();
                }
            }
        }
    }
}

/// Dispatcher-side ends of the SPSC rings plus join handles.
pub struct SimPool {
    pub inboxes: Vec<Producer<SimRequest>>,
    pub outboxes: Vec<Consumer<SimReply>>,
    handles: Vec<JoinHandle<()>>,
}

impl SimPool {
    /// Submit to worker `i`. Does not block.
    pub fn try_send(&mut self, i: usize, req: SimRequest) -> Result<(), SimError> {
        let p = self
            .inboxes
            .get_mut(i)
            .ok_or(SimError::Malformed("worker index"))?;
        p.push(req).map_err(|_| SimError::Malformed("inbox full"))
    }

    pub fn try_recv(&mut self, i: usize) -> Result<Option<SimReply>, SimError> {
        let c = self
            .outboxes
            .get_mut(i)
            .ok_or(SimError::Malformed("worker index"))?;
        match c.pop() {
            Ok(r) => Ok(Some(r)),
            Err(rtrb::PopError::Empty) => Ok(None),
        }
    }
}

impl Drop for SimPool {
    fn drop(&mut self) {
        self.inboxes.clear();
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

/// Spawn `n` workers. Threads are named `liq-sim-worker-{i}`. Core pinning
/// is deferred to 16A.
pub fn spawn_workers<P>(sims: Vec<Simulator<P>>) -> Result<SimPool, SimError>
where
    P: DatabaseRef<Error = SimError> + Send + Sync + 'static,
{
    let mut inboxes = Vec::with_capacity(sims.len());
    let mut outboxes = Vec::with_capacity(sims.len());
    let mut handles = Vec::with_capacity(sims.len());
    for (i, sim) in sims.into_iter().enumerate() {
        let (in_p, in_c) = RingBuffer::<SimRequest>::new(RING);
        let (out_p, out_c) = RingBuffer::<SimReply>::new(RING);
        let worker = SimWorker {
            sim,
            inbox: in_c,
            outbox: out_p,
        };
        let name = format!("liq-sim-worker-{i}");
        let h = Builder::new()
            .name(name)
            .spawn(move || worker.run())
            .map_err(|_| SimError::Malformed("spawn failed"))?;
        inboxes.push(in_p);
        outboxes.push(out_c);
        handles.push(h);
    }
    Ok(SimPool {
        inboxes,
        outboxes,
        handles,
    })
}
