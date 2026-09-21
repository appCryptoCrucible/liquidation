//! Non-hot exec worker: recv [`ExecJob`] and call `submit_path`.
//!
//! Lives off the hot thread. `block_on` is here only — never on ingest.

use std::sync::Arc;
use std::thread::{Builder, JoinHandle};

use crossbeam_channel::Receiver;
use liq_exec::path::{ExecJob, ExecPath};
use liq_obs::ShadowRecorder;
use liq_risk::RiskGate;

/// Recv jobs and run the one submit path. Full inbox is counted on try_send.
pub fn spawn_exec_worker(
    rx: Receiver<ExecJob>,
    path: Arc<ExecPath<ShadowRecorder, &'static RiskGate>>,
) -> std::io::Result<JoinHandle<()>> {
    Builder::new().name("liq-bot-exec".into()).spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!(error = %e, "exec worker runtime refused");
                return;
            }
        };
        while let Ok(job) = rx.recv() {
            if let Err(e) = rt.block_on(path.submit_path(&job)) {
                tracing::error!(
                    error = %e,
                    trace = job.trace.raw(),
                    "submit_path failed"
                );
            }
        }
    })
}
