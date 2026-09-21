//! The one submit path: sign → [`IntendedSubmission`] → record → optional POST.
//!
//! Live HTTP is [`SubmitEnabled`] (default false). Hot thread hands off with
//! [`ExecInbox::try_send`] — never blocks, never `block_on`.

use crate::builders::{BuilderSet, PLANNED_EXECUTOR};
use crate::error::{ExecError, Result};
use crate::fee::FeeQuote;
use crate::inclusion::{spawn_watch, Tracked, WatchCmd};
use crate::nonce::{AllocatedNonce, NonceAllocator, NonceMode};
use crate::submit::{bid_policy, refund_percent, route, BuilderBundle, MevShare, SubmitEnabled};
use crate::template::{sign_call, CallSpec, PrecomputedSigner, SignedTx};
use alloy_primitives::{Address, Bytes, B256, U256};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use liq_oracle::mevshare::{SearcherKey, SendBundle};
use liq_types::{
    stage, Allow, AllowQuery, AssetId, FlashProvider, IntendedSubmission, MarketId, PositionKey,
    ProtocolId, RiskAllow, Stage, SubmitReceipt, Submitter, TraceId, TriggerKind, Venue,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Job built off the hot path (or moved onto the exec task). All fee and
/// bid fields are required inputs — nothing is defaulted.
#[derive(Clone, Debug)]
pub struct ExecJob {
    pub trace: TraceId,
    pub plan: Bytes,
    pub trigger: TriggerKind,
    pub protocol: ProtocolId,
    pub market: MarketId,
    pub collateral: AssetId,
    pub debt: AssetId,
    pub flash: FlashProvider,
    pub operator_key: Address,
    pub position: PositionKey,
    pub hint_hash: Option<B256>,
    /// Raw parent tx for a backrun bundle. Required for ordered triggers.
    pub backrun_tx: Option<Bytes>,
    pub target_block: u64,
    pub max_block: u64,
    pub fee: FeeQuote,
    /// Bid-channel auction bps. Forced to 0 for InterestDrift / Stale.
    pub auction_bps: u16,
    pub refund_address: Address,
    pub calldata: Bytes,
    pub gas_limit: u64,
    pub chain_id: u64,
    pub slot: usize,
}

/// Hot → exec inbox. Bounded; full is counted.
pub struct ExecInbox {
    tx: Sender<ExecJob>,
    pub full: AtomicU64,
}

impl ExecInbox {
    #[must_use]
    pub fn pair(cap: usize) -> (Self, Receiver<ExecJob>) {
        let (tx, rx) = crossbeam_channel::bounded(cap);
        (
            Self {
                tx,
                full: AtomicU64::new(0),
            },
            rx,
        )
    }

    /// Hot thread. Never blocks. Never `block_on`.
    pub fn try_send(&self, job: ExecJob) -> bool {
        match self.tx.try_send(job) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.full.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("submit_queue_full").increment(1);
                false
            }
        }
    }

    #[must_use]
    pub fn full_count(&self) -> u64 {
        self.full.load(Ordering::Relaxed)
    }
}

/// The one path. Generic over the 09A recorder and the 14A gate.
pub struct ExecPath<R, G> {
    pub recorder: R,
    pub gate: G,
    pub submit_enabled: Arc<SubmitEnabled>,
    pub nonce_mode: NonceMode,
    pub nonces: NonceAllocator,
    pub signers: Box<[Arc<PrecomputedSigner>]>,
    pub mevshare: MevShare,
    pub builders: BuilderBundle,
    pub identity: SearcherKey,
    pub http: reqwest::Client,
    pub denied: AtomicU64,
    pub watch_tx: Option<Sender<WatchCmd>>,
}

impl<R, G> ExecPath<R, G>
where
    R: Submitter,
    R::Error: core::fmt::Display,
    G: RiskAllow,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        recorder: R,
        gate: G,
        submit_enabled: Arc<SubmitEnabled>,
        nonce_mode: NonceMode,
        nonces: NonceAllocator,
        signers: Vec<Arc<PrecomputedSigner>>,
        builders: BuilderSet,
        identity: SearcherKey,
    ) -> Result<Self> {
        if signers.len() != nonces.len() {
            return Err(ExecError::Config(
                "signer pool must match nonce slots".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .map_err(|e| ExecError::Http(e.to_string()))?;
        let relay = builders.mevshare_relay;
        Ok(Self {
            recorder,
            gate,
            submit_enabled,
            nonce_mode,
            nonces,
            signers: signers.into_boxed_slice(),
            mevshare: MevShare::new(relay, identity.clone()),
            builders: BuilderBundle::new(builders),
            identity,
            http,
            denied: AtomicU64::new(0),
            watch_tx: None,
        })
    }

    /// Attach an inclusion-watch command sender (try_send, counted).
    pub fn with_watch(mut self, tx: Sender<WatchCmd>) -> Self {
        self.watch_tx = Some(tx);
        self
    }

    /// Sign, record, gate, optional POST. This is the only send path.
    pub async fn submit_path(&self, job: &ExecJob) -> Result<SubmitReceipt> {
        if job.chain_id != 1 {
            return Err(ExecError::ChainId(job.chain_id));
        }
        if job.gas_limit == 0 {
            return Err(ExecError::ZeroGasLimit);
        }
        let bound = job.fee.bind(job.target_block, job.max_block)?;
        let routed = route(
            job.trigger,
            &self.builders.set,
            job.auction_bps,
            job.fee.priority_wei,
            job.fee.modest_priority_wei,
        )?;
        if matches!(job.trigger, TriggerKind::SvrAuction) {
            let span = job
                .max_block
                .checked_sub(job.target_block)
                .ok_or(ExecError::FeeOverflow)?;
            if !(2..=3).contains(&span) {
                return Err(ExecError::BadMevShareSpan(span));
            }
        }

        let allocated = match self.nonce_mode {
            NonceMode::Allocate => self.nonces.allocate(job.slot)?,
            NonceMode::DryRun => self.nonces.dry_run(job.slot)?,
        };
        let signer = self
            .signers
            .get(job.slot)
            .ok_or(ExecError::BadSlot(job.slot))?;
        if signer.address() != allocated.address && self.nonce_mode == NonceMode::Allocate {
            tracing::error!(
                slot = job.slot,
                "signer address does not match nonce key; fail closed"
            );
            return Err(ExecError::Signer("slot signer/key mismatch".into()));
        }

        let signed = sign_call(
            signer,
            CallSpec {
                chain_id: job.chain_id,
                nonce: allocated.nonce,
                to: PLANNED_EXECUTOR,
                input: job.calldata.clone(),
                gas_limit: job.gas_limit,
                fees: &bound,
                priority: routed.policy.priority_wei,
            },
        )?;
        stage(job.trace, Stage::Signed);

        let intended = IntendedSubmission {
            plan: job.plan.clone(),
            bid: routed.intended_bid,
            venue: routed.venue,
            deadline: job.max_block,
            trace: job.trace,
        };
        self.recorder
            .submit(&intended)
            .map_err(|e| ExecError::Record(e.to_string()))?;

        let q = AllowQuery {
            protocol: job.protocol,
            market: job.market,
            collateral: job.collateral,
            debt: job.debt,
            flash: job.flash,
            trigger: job.trigger,
            operator_key: job.operator_key,
        };
        match self.gate.allow(job.trace, &q) {
            Allow::Denied { scope, reason } => {
                self.denied.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("risk_denied").increment(1);
                tracing::info!(
                    trace = job.trace.raw(),
                    ?scope,
                    ?reason,
                    "risk deny; HTTP not sent"
                );
                return Ok(SubmitReceipt::Denied);
            }
            Allow::Yes => {}
        }

        if !self.submit_enabled.get() {
            self.track(&allocated, &signed, job);
            return Ok(SubmitReceipt::Recorded);
        }

        self.send_signed(job, &routed.venue, &signed, &routed.policy)
            .await?;
        stage(job.trace, Stage::VenueAck);
        self.track(&allocated, &signed, job);
        Ok(SubmitReceipt::Accepted)
    }

    async fn send_signed(
        &self,
        job: &ExecJob,
        venue: &Venue,
        signed: &SignedTx,
        policy: &crate::submit::BidPolicy,
    ) -> Result<()> {
        match venue {
            Venue::MevShare { .. } => {
                let hint = job.hint_hash.ok_or(ExecError::MissingHintHash)?;
                let pct = refund_percent(policy.auction_bps)?;
                let bundle = SendBundle {
                    block: job.target_block,
                    max_block: job.max_block,
                    hint_hash: hint,
                    signed_liquidation: signed.raw.clone(),
                    refund_address: job.refund_address,
                    refund_percent: pct,
                };
                let req = self.mevshare.sign(&bundle)?;
                let _ = self.mevshare.send(&self.http, &req).await?;
            }
            Venue::BuilderBundle { .. } => {
                let txs = bundle_txs(job, signed)?;
                let _ = self
                    .builders
                    .send(&self.http, &self.identity, job.target_block, &txs)
                    .await?;
            }
        }
        Ok(())
    }

    fn track(&self, allocated: &AllocatedNonce, signed: &SignedTx, job: &ExecJob) {
        let Some(tx) = &self.watch_tx else {
            return;
        };
        let cmd = WatchCmd::Track(Tracked {
            trace: job.trace,
            tx_hash: signed.hash,
            position: job.position,
            operator: allocated.address,
            min_block: job.target_block,
            max_block: job.max_block,
        });
        if tx.try_send(cmd).is_err() {
            tracing::error!(trace = job.trace.raw(), "inclusion watch channel full");
        }
    }

    #[must_use]
    pub fn denied_count(&self) -> u64 {
        self.denied.load(Ordering::Relaxed)
    }
}

fn bundle_txs(job: &ExecJob, signed: &SignedTx) -> Result<Vec<Bytes>> {
    match job.trigger {
        TriggerKind::InterestDrift | TriggerKind::Stale | TriggerKind::ParamChange => {
            Ok(vec![signed.raw.clone()])
        }
        TriggerKind::SvrAuction => Err(ExecError::WrongVenueBuilder),
        TriggerKind::OraclePredicted => Err(ExecError::PredictedNotSubmittable),
        TriggerKind::OraclePublic
        | TriggerKind::OraclePullHeld
        | TriggerKind::PoolStateChange
        | TriggerKind::UserAction
        | TriggerKind::DerivedRate => {
            let parent = job.backrun_tx.clone().ok_or(ExecError::MissingBackrunTx)?;
            if parent.is_empty() {
                return Err(ExecError::MissingBackrunTx);
            }
            Ok(vec![parent, signed.raw.clone()])
        }
    }
}

/// Channels + thread for [`start_inclusion_watch`].
pub struct InclusionIo {
    pub cmds: Sender<WatchCmd>,
    pub outcomes: Receiver<(TraceId, crate::inclusion::Terminal)>,
    pub thread: std::thread::JoinHandle<()>,
    pub outcome_full: Arc<AtomicU64>,
}

/// Start the inclusion watcher thread.
pub fn start_inclusion_watch(outcome_cap: usize) -> Result<InclusionIo> {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let (out_tx, out_rx) = crossbeam_channel::bounded(outcome_cap);
    let full = Arc::new(AtomicU64::new(0));
    let thread = spawn_watch(cmd_rx, out_tx, Arc::clone(&full))?;
    Ok(InclusionIo {
        cmds: cmd_tx,
        outcomes: out_rx,
        thread,
        outcome_full: full,
    })
}

/// Test / wiring recorder that captures the intended row.
pub struct CaptureRecorder {
    pub rows: parking_lot::Mutex<Vec<IntendedSubmission>>,
}

impl Default for CaptureRecorder {
    fn default() -> Self {
        Self {
            rows: parking_lot::Mutex::new(Vec::new()),
        }
    }
}

impl Submitter for CaptureRecorder {
    type Error = ExecError;

    fn submit(&self, submission: &IntendedSubmission) -> Result<SubmitReceipt> {
        self.rows.lock().push(submission.clone());
        Ok(SubmitReceipt::Shadow)
    }
}

/// Gate that always allows.
pub struct AllowAll;

impl RiskAllow for AllowAll {
    fn allow(&self, _trace: TraceId, _q: &AllowQuery) -> Allow {
        Allow::Yes
    }
}

/// Gate that always denies.
pub struct DenyAll {
    pub reason: liq_types::HaltReason,
}

impl RiskAllow for DenyAll {
    fn allow(&self, _trace: TraceId, _q: &AllowQuery) -> Allow {
        Allow::Denied {
            scope: liq_types::HaltScope::Global,
            reason: self.reason,
        }
    }
}

/// Silence unused import of bid_policy in this module (used by tests / route).
#[allow(dead_code)]
fn _policy_check(k: TriggerKind) -> Result<U256> {
    bid_policy(k, 0, 1, 1).map(|p| U256::from(p.priority_wei))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use crate::fee::FeeQuote;
    use alloy_primitives::address;

    fn quote(parent: u64) -> FeeQuote {
        FeeQuote {
            parent_block: parent,
            next_base_fee: 1_000,
            priority_wei: 10,
            modest_priority_wei: 2,
        }
    }

    #[test]
    fn try_send_counts_full_never_blocks() {
        let (inbox, rx) = ExecInbox::pair(1);
        let job = dummy_job(TriggerKind::InterestDrift);
        assert!(inbox.try_send(job.clone()));
        assert!(!inbox.try_send(job));
        assert_eq!(inbox.full_count(), 1);
        drop(rx);
    }

    fn dummy_job(kind: TriggerKind) -> ExecJob {
        ExecJob {
            trace: TraceId::from_raw(1),
            plan: Bytes::from_static(&[0x01]),
            trigger: kind,
            protocol: ProtocolId(1),
            market: MarketId(1),
            collateral: AssetId(0),
            debt: AssetId(1),
            flash: FlashProvider::Aave,
            operator_key: address!("0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"),
            position: PositionKey {
                protocol: ProtocolId(1),
                market: MarketId(1),
                user: Address::ZERO,
            },
            hint_hash: None,
            backrun_tx: None,
            target_block: 101,
            max_block: 101,
            fee: quote(100),
            auction_bps: 9_000,
            refund_address: Address::ZERO,
            calldata: Bytes::from_static(&[0xab]),
            gas_limit: 200_000,
            chain_id: 1,
            slot: 0,
        }
    }
}
