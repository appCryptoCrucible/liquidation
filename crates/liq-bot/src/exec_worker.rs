//! Non-hot exec worker: recv [`ExecJob`] and call `submit_path`, and recv
//! [`GovJob`] and call `submit_gov`.
//!
//! Lives off the hot thread. `block_on` is here only — never on ingest.
//! With a chain client, every job first resyncs the operator nonce for its
//! target block, and a job marked `rpc_verify` is simulated against the
//! node before it is signed.

use std::sync::Arc;
use std::thread::{Builder, JoinHandle};

use alloy_primitives::Address;
use crossbeam_channel::Receiver;
use liq_exec::chain::{ChainClient, SimCall};
use liq_exec::gov::{GovJob, GovTarget};
use liq_exec::path::{ExecJob, ExecPath};
use liq_obs::ShadowRecorder;
use liq_risk::RiskGate;

type Path = ExecPath<ShadowRecorder, &'static RiskGate>;

/// The node the worker reads: nonces, simulations, governance bundles.
pub struct ChainExec {
    pub chain: ChainClient,
    /// Governance bundles from the hot thread. `None`: not planned.
    pub gov_rx: Option<Receiver<GovJob>>,
    /// `venues.executor` once deployed. `None`: simulate the compiled
    /// Executor at the placeholder address the transactions are signed to,
    /// with its modules where it delegatecalls them.
    pub deployed: Option<Address>,
    /// `PROFIT_SINK`, a constructor argument of the simulated Executor.
    pub profit_sink: Option<Address>,
}

/// Recv jobs and run the submit paths. Full inboxes are counted on try_send.
pub fn spawn_exec_worker(
    rx: Receiver<ExecJob>,
    chain: Option<ChainExec>,
    path: Arc<Path>,
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
        let gov_rx = chain
            .as_ref()
            .and_then(|c| c.gov_rx.clone())
            .unwrap_or_else(crossbeam_channel::never);
        let mut target: Option<GovTarget> = None;
        loop {
            crossbeam_channel::select! {
                recv(rx) -> msg => {
                    let Ok(mut job) = msg else { break };
                    if !rt.block_on(prepare(&mut job, chain.as_ref(), &path, &mut target)) {
                        continue;
                    }
                    if let Err(e) = rt.block_on(path.submit_path(&job)) {
                        tracing::error!(error = %e, trace = job.trace.raw(), "submit_path failed");
                    }
                }
                recv(gov_rx) -> msg => {
                    let (Ok(job), Some(c)) = (msg, chain.as_ref()) else { continue };
                    if target.is_none() {
                        target = sim_target(c, &path);
                    }
                    let Some(t) = target.as_ref() else {
                        tracing::error!(action = ?job.action, "no Executor to simulate — governance bundle not sent");
                        continue;
                    };
                    match rt.block_on(path.submit_gov(&job, &c.chain, t)) {
                        Ok(r) => tracing::info!(action = ?job.action, block = job.target_block, receipt = ?r, "governance bundle"),
                        Err(e) => tracing::error!(error = %e, action = ?job.action, "submit_gov failed"),
                    }
                }
            }
        }
    })
}

/// Resync the nonce for the job's block and, when asked, verify it against
/// the node and size its gas from the simulation. `false`: do not send.
async fn prepare(
    job: &mut ExecJob,
    chain: Option<&ChainExec>,
    path: &Path,
    target: &mut Option<GovTarget>,
) -> bool {
    let Some(c) = chain else {
        if job.rpc_verify {
            tracing::error!(
                trace = job.trace.raw(),
                "no chain client to verify against — not sent"
            );
            return false;
        }
        return true;
    };
    if let Err(e) = path.sync_nonce(&c.chain, job.target_block).await {
        tracing::error!(error = %e, trace = job.trace.raw(), "nonce resync failed — not sent");
        return false;
    }
    if !job.rpc_verify {
        return true;
    }
    let Some(base) = job.target_block.checked_sub(1) else {
        return false;
    };
    if target.is_none() {
        *target = sim_target(c, path);
    }
    let Some(t) = target.as_ref() else {
        tracing::error!(
            trace = job.trace.raw(),
            "no Executor to simulate — not sent"
        );
        return false;
    };
    // The key that signs this job, on its own slot.
    let Some(operator) = path.signers.get(job.slot).map(|s| s.address()) else {
        return false;
    };
    match verify(job, &c.chain, t, operator, base).await {
        Some(gas) => {
            job.gas_limit = gas;
            true
        }
        None => false,
    }
}

/// Simulate `job` alone on `base` as the next block. Its gas plus a fifth,
/// or `None` when it reverts or the node refuses.
async fn verify(
    job: &ExecJob,
    chain: &ChainClient,
    t: &GovTarget,
    operator: Address,
    base: u64,
) -> Option<u64> {
    let ts = chain.block_timestamp(base).await.ok()?.checked_add(12)?;
    let price = job.fee.next_base_fee.checked_add(job.fee.priority_wei)?;
    let call = SimCall {
        from: operator,
        to: t.executor,
        data: job.calldata.clone(),
        gas: None,
        gas_price: Some(price),
    };
    let r = match chain
        .simulate(base, job.target_block, ts, &t.code, &[call])
        .await
    {
        Ok(mut r) => r.pop()?,
        Err(e) => {
            tracing::error!(error = %e, trace = job.trace.raw(), "job simulation failed — not sent");
            return None;
        }
    };
    if !r.success {
        tracing::info!(trace = job.trace.raw(), reason = %r.return_data, "job reverts in simulation — not sent");
        return None;
    }
    r.gas_used.checked_mul(6)?.checked_div(5)
}

/// The deployed Executor, or the compiled system: the core's runtime at the
/// address transactions are signed to and its modules' where it
/// delegatecalls them, built here from the forge artifacts (the
/// constructors read no chain state).
fn sim_target(c: &ChainExec, path: &Path) -> Option<GovTarget> {
    if let Some(executor) = c.deployed {
        return Some(GovTarget {
            executor,
            code: Vec::new(),
        });
    }
    let operator = path.signers.first()?.address();
    // No second key: the compiled Executor gets the first one twice.
    let backrun = path.signers.get(1).map_or(operator, |s| s.address());
    let Some(sink) = c.profit_sink else {
        tracing::error!("PROFIT_SINK unset — the undeployed Executor cannot be simulated");
        return None;
    };
    let spec = liq_sim::ExecutorSpec::mainnet(operator, backrun, sink);
    match liq_sim::executor_stack(&spec) {
        Ok(stack) => Some(GovTarget {
            executor: path.executor,
            code: stack.runtimes(path.executor).to_vec(),
        }),
        Err(e) => {
            tracing::error!(error = %e, "Executor artifacts unreadable — run forge build");
            None
        }
    }
}

#[cfg(test)]
mod verify_tests;

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, keccak256, Address, Bytes};
    use alloy_sol_types::SolCall;
    use liq_exec::chain::{ChainClient, SimCall};
    use liq_exec::executor::{IExecutor, IExecutorModule};

    /// The compiled Executor and its modules, built here and placed by the
    /// node's state override at a real block, answer as the system they
    /// were built for: the core at the placeholder address the transactions
    /// are signed to, wired to its modules where they were placed.
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "needs MAINNET_RPC_URL and forge build output"]
    async fn simulated_executor_has_our_immutables() {
        let Some(url) = std::env::var("MAINNET_RPC_URL")
            .ok()
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        let chain = ChainClient::new(&url).unwrap();
        let operator = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let sink = address!("11fa49084B4D63b156a4C8238291A562019bA49d");
        let backrun = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
        let spec = liq_sim::ExecutorSpec::mainnet(operator, backrun, sink);
        let code = liq_sim::executor_stack(&spec)
            .unwrap()
            .runtimes(liq_sim::PLANNED_EXECUTOR);
        assert!(code.iter().all(|(_, c)| c.len() > 1_000));
        let base = 26_019_517u64;
        let ask = |to: Address, data: Vec<u8>| SimCall {
            from: operator,
            to,
            data: Bytes::from(data),
            gas: None,
            gas_price: None,
        };
        let core = |data: Vec<u8>| ask(liq_sim::PLANNED_EXECUTOR, data);
        let out = chain
            .simulate(
                base,
                base + 1,
                1_789_916_831,
                &code,
                &[
                    core(IExecutor::OPERATORCall {}.abi_encode()),
                    core(IExecutor::BACKRUN_OPERATORCall {}.abi_encode()),
                    core(IExecutor::PROFIT_SINKCall {}.abi_encode()),
                    core(IExecutor::LIQUIDATION_MODULECall {}.abi_encode()),
                    core(IExecutor::SWAP_MODULECall {}.abi_encode()),
                    ask(
                        liq_sim::PLANNED_LIQUIDATION_MODULE,
                        IExecutorModule::MODULE_IDCall {}.abi_encode(),
                    ),
                    ask(
                        liq_sim::PLANNED_SWAP_MODULE,
                        IExecutorModule::MODULE_IDCall {}.abi_encode(),
                    ),
                ],
            )
            .await
            .unwrap();
        assert!(out.iter().all(|r| r.success));
        assert_eq!(
            IExecutor::OPERATORCall::abi_decode_returns(&out[0].return_data).unwrap(),
            operator
        );
        assert_eq!(
            IExecutor::BACKRUN_OPERATORCall::abi_decode_returns(&out[1].return_data).unwrap(),
            backrun
        );
        assert_eq!(
            IExecutor::PROFIT_SINKCall::abi_decode_returns(&out[2].return_data).unwrap(),
            sink
        );
        assert_eq!(
            IExecutor::LIQUIDATION_MODULECall::abi_decode_returns(&out[3].return_data).unwrap(),
            liq_sim::PLANNED_LIQUIDATION_MODULE
        );
        assert_eq!(
            IExecutor::SWAP_MODULECall::abi_decode_returns(&out[4].return_data).unwrap(),
            liq_sim::PLANNED_SWAP_MODULE
        );
        assert_eq!(
            IExecutorModule::MODULE_IDCall::abi_decode_returns(&out[5].return_data).unwrap(),
            keccak256("liq-executor/liquidation-module/v1")
        );
        assert_eq!(
            IExecutorModule::MODULE_IDCall::abi_decode_returns(&out[6].return_data).unwrap(),
            keccak256("liq-executor/swap-module/v1")
        );
    }
}
