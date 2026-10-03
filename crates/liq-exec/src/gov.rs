//! Governance bundles: one liquidation per transaction, every one allowed to
//! revert, all in one `eth_sendBundle` for the first block the payload can
//! execute in.
//!
//! Each transaction's plan carries the governance action (an Aave payload
//! id or a Sky spell); the Executor applies it first, so whichever
//! transaction runs first applies the change, pays its gas, and the rest
//! skip it. An account whose owner changes it in the block before reverts
//! alone; the others still land. Nonces come from the allocator after a
//! chain resync for the target block, so a bundle that does not land leaves
//! no gap and ordinary jobs for the same block never reuse one.

use std::sync::atomic::{AtomicU64, Ordering};

use alloy_primitives::{Address, Bytes, B256};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use liq_types::{
    stage, Allow, AllowQuery, AssetId, FlashProvider, IntendedSubmission, MarketId, PositionKey,
    ProtocolId, RiskAllow, Stage, Submitter, TraceId, TriggerKind,
};

use crate::chain::{ChainClient, SimCall};
use crate::error::{ExecError, Result};
use crate::fee::FeeQuote;
use crate::inclusion::{Tracked, WatchCmd};
use crate::path::ExecPath;
use crate::submit::route;
use crate::template::{sign_call, CallSpec};

/// Sum of the gas limits in one governance bundle. Every transaction
/// reserves `executePayload` gas (it may be the one that runs it), and a
/// transaction whose limit exceeds the block's remaining gas makes the whole
/// bundle invalid rather than reverting. Half a 30M–36M block.
pub const GOV_BUNDLE_GAS: u64 = 15_000_000;

/// One account's liquidation, ready to sign.
#[derive(Clone, Debug)]
pub struct GovTx {
    pub trace: TraceId,
    /// The encoded plan, for the recorder.
    pub plan: Bytes,
    /// `Executor.execute(plan)` with the governance flag and payload id.
    pub calldata: Bytes,
    pub protocol: ProtocolId,
    pub market: MarketId,
    pub collateral: AssetId,
    pub debt: AssetId,
    pub flash: FlashProvider,
    pub position: PositionKey,
    pub auction_bps: u16,
}

/// The governance change a bundle applies.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GovAction {
    /// Aave `PayloadsController.executePayload(id)`.
    Payload { controller: Address, id: u64 },
    /// Sky `DssSpell.cast()`; Spark's parameters change through these.
    Spell(Address),
}

/// Every liquidation one governance action enables in a block it can
/// execute in, most valuable first.
#[derive(Clone, Debug)]
pub struct GovJob {
    pub action: GovAction,
    pub base_block: u64,
    pub target_block: u64,
    pub target_ts: u64,
    /// Gas the governance call used in simulation.
    pub exec_gas: u64,
    pub fee: FeeQuote,
    pub chain_id: u64,
    pub txs: Vec<GovTx>,
}

/// Hot → exec inbox for governance jobs. Bounded; full is counted.
pub struct GovInbox {
    tx: Sender<GovJob>,
    pub full: AtomicU64,
}

impl GovInbox {
    #[must_use]
    pub fn pair(cap: usize) -> (Self, Receiver<GovJob>) {
        let (tx, rx) = crossbeam_channel::bounded(cap);
        (
            Self {
                tx,
                full: AtomicU64::new(0),
            },
            rx,
        )
    }

    /// Hot thread. Never blocks.
    pub fn try_send(&self, job: GovJob) -> bool {
        match self.tx.try_send(job) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.full.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("gov_queue_full").increment(1);
                false
            }
        }
    }
}

/// The Executor the simulation calls. `code` is what the simulation places
/// while it is not deployed: the core's runtime at `executor` and its
/// modules' where the core delegatecalls them. Empty once deployed.
#[derive(Clone, Debug)]
pub struct GovTarget {
    pub executor: Address,
    pub code: Vec<(Address, Bytes)>,
}

/// What [`ExecPath::submit_gov`] did.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GovReceipt {
    /// Every transaction reverted in simulation.
    NoneSurvived,
    /// The risk gate refused every survivor.
    Denied,
    /// Signed and recorded; the live-send conjunction is not met.
    Recorded { txs: usize },
    /// Sent to the builders.
    Accepted { txs: usize },
}

/// Gas limit for one transaction: its simulated use plus 20 %, plus room for
/// `executePayload` in case an earlier transaction in the bundle reverts.
pub fn gov_gas_limit(sim_gas: u64, exec_gas: u64) -> Result<u64> {
    sim_gas
        .checked_mul(6)
        .and_then(|g| g.checked_div(5))
        .and_then(|g| g.checked_add(exec_gas))
        .ok_or(ExecError::FeeOverflow)
}

/// Transactions that succeeded in simulation, in order, with their limits,
/// cut where the running sum would pass [`GOV_BUNDLE_GAS`]. The first
/// survivor is the one that ran `executePayload` in the simulation, so its
/// measured gas already includes it; each later one reserves it again.
pub fn gov_survivors(sims: &[(bool, u64)], exec_gas: u64) -> Result<Vec<(usize, u64)>> {
    let mut out: Vec<(usize, u64)> = Vec::new();
    let mut total = 0u64;
    for (i, &(ok, used)) in sims.iter().enumerate() {
        if !ok {
            continue;
        }
        let room = if out.is_empty() { 0 } else { exec_gas };
        let limit = gov_gas_limit(used, room)?;
        let next = total.checked_add(limit).ok_or(ExecError::FeeOverflow)?;
        if next > GOV_BUNDLE_GAS {
            break;
        }
        total = next;
        out.push((i, limit));
    }
    Ok(out)
}

impl<R, G> ExecPath<R, G>
where
    R: Submitter,
    R::Error: core::fmt::Display,
    G: RiskAllow,
{
    /// Simulate, sign with consecutive chain nonces, record, gate, send.
    pub async fn submit_gov(
        &self,
        job: &GovJob,
        chain: &ChainClient,
        target: &GovTarget,
    ) -> Result<GovReceipt> {
        if job.chain_id != 1 {
            return Err(ExecError::ChainId(job.chain_id));
        }
        if job.txs.is_empty() {
            return Err(ExecError::EmptyBundle);
        }
        if job.base_block.checked_add(1) != Some(job.target_block) {
            return Err(ExecError::StaleFee {
                parent: job.base_block,
                target: job.target_block,
            });
        }
        let signer = self.signers.first().ok_or(ExecError::BadSlot(0))?;
        let operator = signer.address();
        let bound = job.fee.bind(job.target_block, job.target_block)?;
        // The Executor charges the governance call at `tx.gasprice`, so the
        // simulation must see the price the transactions will pay.
        let price = job
            .fee
            .next_base_fee
            .checked_add(job.fee.priority_wei)
            .ok_or(ExecError::FeeOverflow)?;

        let calls: Vec<SimCall> = job
            .txs
            .iter()
            .map(|t| SimCall {
                from: operator,
                to: target.executor,
                data: t.calldata.clone(),
                gas: None,
                gas_price: Some(price),
            })
            .collect();
        let sims = chain
            .simulate(
                job.base_block,
                job.target_block,
                job.target_ts,
                &target.code,
                &calls,
            )
            .await?;
        let outcome: Vec<(bool, u64)> = sims.iter().map(|s| (s.success, s.gas_used)).collect();
        let keep = gov_survivors(&outcome, job.exec_gas)?;
        if keep.is_empty() {
            tracing::info!(
                action = ?job.action,
                txs = job.txs.len(),
                "governance bundle: every transaction reverted in simulation"
            );
            return Ok(GovReceipt::NoneSurvived);
        }

        self.sync_nonce(chain, job.target_block).await?;
        let mut nonce = self.nonces.next_of(0)?;
        let mut raws: Vec<Bytes> = Vec::with_capacity(keep.len());
        let mut hashes: Vec<B256> = Vec::with_capacity(keep.len());
        let mut tracked: Vec<Tracked> = Vec::with_capacity(keep.len());
        for (i, gas_limit) in keep {
            let t = job.txs.get(i).ok_or(ExecError::EmptyBundle)?;
            let routed = route(
                TriggerKind::ParamChange,
                &self.builders.set,
                t.auction_bps,
                job.fee.priority_wei,
                job.fee.modest_priority_wei,
            )?;
            let q = AllowQuery {
                protocol: t.protocol,
                market: t.market,
                collateral: t.collateral,
                debt: t.debt,
                flash: t.flash,
                trigger: TriggerKind::ParamChange,
                operator_key: operator,
            };
            if let Allow::Denied { scope, reason } = self.gate.allow(t.trace, &q) {
                self.denied.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("risk_denied").increment(1);
                tracing::info!(
                    trace = t.trace.raw(),
                    ?scope,
                    ?reason,
                    "risk deny; left out of the governance bundle"
                );
                continue;
            }
            let signed = sign_call(
                signer,
                CallSpec {
                    chain_id: job.chain_id,
                    nonce,
                    to: target.executor,
                    input: t.calldata.clone(),
                    gas_limit,
                    fees: &bound,
                    priority: routed.policy.priority_wei,
                },
            )?;
            stage(t.trace, Stage::Signed);
            self.recorder
                .submit(&IntendedSubmission {
                    plan: t.plan.clone(),
                    bid: routed.intended_bid,
                    venue: routed.venue,
                    deadline: job.target_block,
                    trace: t.trace,
                })
                .map_err(|e| ExecError::Record(e.to_string()))?;
            nonce = nonce.checked_add(1).ok_or(ExecError::NonceOverflow)?;
            hashes.push(signed.hash);
            raws.push(signed.raw);
            tracked.push(Tracked {
                trace: t.trace,
                tx_hash: signed.hash,
                position: t.position,
                operator,
                min_block: job.target_block,
                max_block: job.target_block,
            });
        }
        if raws.is_empty() {
            return Ok(GovReceipt::Denied);
        }
        let n = raws.len();
        // Consume exactly the nonces signed above.
        if self.nonce_mode == crate::nonce::NonceMode::Allocate {
            self.nonces
                .allocate_run(0, u64::try_from(n).map_err(|_| ExecError::NonceOverflow)?)?;
        }
        // No deployed Executor: the placeholder has no code on mainnet, so
        // the bundle is recorded like a closed send gate (see `submit_path`).
        let undeployed = self.executor == crate::builders::PLANNED_EXECUTOR
            || target.executor == crate::builders::PLANNED_EXECUTOR;
        if !self.submit_enabled.get()
            || !self.lease_held.load(Ordering::Acquire)
            || !self.nonce_resync.load(Ordering::Acquire)
            || undeployed
        {
            self.track_all(tracked);
            return Ok(GovReceipt::Recorded { txs: n });
        }
        self.builders
            .send_reverting(&self.http, &self.identity, job.target_block, &raws, &hashes)
            .await?;
        self.track_all(tracked);
        tracing::info!(
            action = ?job.action,
            block = job.target_block,
            txs = n,
            "governance bundle sent"
        );
        Ok(GovReceipt::Accepted { txs: n })
    }

    fn track_all(&self, tracked: Vec<Tracked>) {
        let Some(tx) = &self.watch_tx else {
            return;
        };
        for t in tracked {
            let trace = t.trace;
            if tx.try_send(WatchCmd::Track(t)).is_err() {
                tracing::error!(trace = trace.raw(), "inclusion watch channel full");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gas_limit_is_sim_plus_a_fifth_plus_exec_room() {
        assert_eq!(
            gov_gas_limit(500_000, 2_436_297).unwrap(),
            600_000 + 2_436_297
        );
        assert!(gov_gas_limit(u64::MAX, 1).is_err());
    }

    #[test]
    fn survivors_skip_reverts_keep_order_and_respect_the_budget() {
        let exec = 2_436_297u64;
        // tx0 ran executePayload (its own gas includes it); tx1 reverted;
        // tx2 and tx3 skipped the payload.
        let sims = [
            (true, 2_900_000),
            (false, 60_000),
            (true, 480_000),
            (true, 510_000),
        ];
        let got = gov_survivors(&sims, exec).unwrap();
        assert_eq!(
            got,
            vec![(0, 3_480_000), (2, 576_000 + exec), (3, 612_000 + exec),],
            "the executor of the payload does not reserve it twice"
        );
        let total: u64 = got.iter().map(|(_, l)| *l).sum();
        assert!(total <= GOV_BUNDLE_GAS);

        // A 7.5M payload (the one executed in block 26019517) leaves room
        // for the transaction that runs it and nothing after it.
        let big = [(true, 8_000_000), (true, 500_000)];
        let got = gov_survivors(&big, 7_477_682).unwrap();
        assert_eq!(got, vec![(0, 9_600_000)]);

        // The executor reverted: the next survivor runs the payload in
        // simulation, and is first, so it does not reserve it again.
        let got = gov_survivors(&[(false, 3_000_000), (true, 2_900_000)], exec).unwrap();
        assert_eq!(got, vec![(1, 3_480_000)]);
    }
}
