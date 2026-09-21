//! The one submit path: sign → [`IntendedSubmission`] → record → optional POST.
//!
//! Live HTTP is held ∧ [`SubmitEnabled`] ∧ nonce resync, loaded at send
//! time (held/resync default false). Hot thread hands off with
//! [`ExecInbox::try_send`] — never blocks, never `block_on`.

use crate::builders::{BuilderSet, PLANNED_EXECUTOR};
use crate::error::{ExecError, Result};
use crate::fee::FeeQuote;
use crate::inclusion::{spawn_watch, Tracked, WatchCmd};
use crate::nonce::{AllocatedNonce, NonceAllocator, NonceMode};
use crate::submit::{
    bid_policy, refund_percent, route, BuilderBundle, LiveSendBits, MevShare, SubmitEnabled,
};
use crate::template::{sign_call, CallSpec, PrecomputedSigner, SignedTx};
use alloy_primitives::{Address, Bytes, B256, U256};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use liq_oracle::mevshare::{SearcherKey, SendBundle};
use liq_types::{
    stage, Allow, AllowQuery, AssetId, FlashProvider, IntendedSubmission, MarketId, PositionKey,
    ProtocolId, RiskAllow, Stage, SubmitReceipt, Submitter, TraceId, TriggerKind, Venue,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Mirror `liq_obs::net_rtt::build_pooled_client` (16D). Same numbers; this
/// client is async `reqwest::Client` (13A submit path).
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const HTTP_POOL_IDLE: Duration = Duration::from_secs(90);
const HTTP_TCP_KEEPALIVE: Duration = Duration::from_secs(10);
const HTTP_POOL_MAX_IDLE_PER_HOST: usize = 8;

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
    /// Process lease held bit. Default false (unbound path cannot POST).
    pub lease_held: Arc<AtomicBool>,
    /// Chain-nonce resync bit (H4). Default false; 17A never stores true.
    pub nonce_resync: Arc<AtomicBool>,
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
        live: LiveSendBits,
    ) -> Result<Self> {
        if signers.len() != nonces.len() {
            return Err(ExecError::Config(
                "signer pool must match nonce slots".into(),
            ));
        }
        for (i, signer) in signers.iter().enumerate() {
            let key = nonces.address(i)?;
            if signer.address() != key {
                tracing::error!(
                    slot = i,
                    signer = ?signer.address(),
                    nonce_key = ?key,
                    "ExecPath::new: signer address does not match nonce key"
                );
                return Err(ExecError::Signer("slot signer/key mismatch".into()));
            }
        }
        let http = build_submit_http()?;
        let relay = builders.mevshare_relay;
        Ok(Self {
            recorder,
            gate,
            submit_enabled,
            lease_held: live.held,
            nonce_resync: live.nonce_resync,
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
        // Before record / toggle: a job live would reject must not be
        // Shadow-Recorded (13A residual; 17A wiring).
        require_venue_inputs(job, &routed.venue)?;

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
                self.mark_nonce_dropped(&allocated);
                return Ok(SubmitReceipt::Denied);
            }
            Allow::Yes => {}
        }

        // D49: gate sits on the consumption point. Acquire loads — not a
        // snapshot from bind. Unbound defaults (both false) cannot POST.
        if !self.submit_enabled.get()
            || !self.lease_held.load(Ordering::Acquire)
            || !self.nonce_resync.load(Ordering::Acquire)
        {
            self.track(&allocated, &signed, job);
            return Ok(SubmitReceipt::Recorded);
        }

        if let Err(e) = self
            .send_signed(job, &routed.venue, &signed, &routed.policy)
            .await
        {
            self.mark_nonce_dropped(&allocated);
            return Err(e);
        }
        stage(job.trace, Stage::VenueAck);
        self.track(&allocated, &signed, job);
        Ok(SubmitReceipt::Accepted)
    }

    /// POST to establish the idle socket. **Not** a live bundle. Name is
    /// `warm_http` so 16D `thirteen_a_http_pool_seam` can see the prewarm.
    pub async fn warm_http(&self, url: &str) -> Result<()> {
        crate::builders::reject_public_rpc(url)?;
        let resp = self
            .http
            .post(url)
            .header("content-type", "application/json")
            .body("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_chainId\",\"params\":[]}")
            .send()
            .await
            .map_err(|e| {
                tracing::error!(error = %e, url, "13A warm_http / prewarm failed");
                ExecError::Http(e.to_string())
            })?;
        resp.bytes().await.map_err(|e| {
            tracing::error!(error = %e, url, "13A warm_http body read failed");
            ExecError::Http(e.to_string())
        })?;
        Ok(())
    }

    /// Prewarm every curated builder plus the MEV-Share relay. Failures are
    /// logged; the caller decides whether the path stays bound.
    pub async fn prewarm(&self) -> Result<()> {
        let mut last_err: Option<ExecError> = None;
        for b in &self.builders.set.builders {
            if let Err(e) = self.warm_http(b.endpoint).await {
                tracing::error!(endpoint = b.endpoint, err = %e, "prewarm builder failed");
                last_err = Some(e);
            }
        }
        if let Err(e) = self.warm_http(self.mevshare.relay).await {
            tracing::error!(err = %e, "prewarm mevshare failed");
            last_err = Some(e);
        }
        match last_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
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

    /// Allocate-mode only. Logs [`crate::nonce::GapFill`]; does not POST it (H4).
    fn mark_nonce_dropped(&self, allocated: &AllocatedNonce) {
        if self.nonce_mode != NonceMode::Allocate {
            return;
        }
        match self.nonces.mark_dropped(allocated.slot, allocated.nonce) {
            Ok(fill) => {
                tracing::error!(
                    slot = fill.slot,
                    nonce = fill.nonce,
                    from = ?fill.from,
                    to = ?fill.to,
                    value = %fill.value,
                    "gap-fill prepared after deny/send-fail; not POSTed (H4)"
                );
                metrics::counter!("nonce_gap_fill").increment(1);
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    slot = allocated.slot,
                    nonce = allocated.nonce,
                    "mark_dropped failed — nonce hole"
                );
            }
        }
    }

    #[must_use]
    pub fn denied_count(&self) -> u64 {
        self.denied.load(Ordering::Relaxed)
    }
}

fn build_submit_http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .pool_idle_timeout(HTTP_POOL_IDLE)
        .pool_max_idle_per_host(HTTP_POOL_MAX_IDLE_PER_HOST)
        .tcp_keepalive(HTTP_TCP_KEEPALIVE)
        .tcp_nodelay(true)
        .no_proxy()
        .build()
        .map_err(|e| ExecError::Http(e.to_string()))
}

/// Venue-required fields. Runs before the recorder and the submit toggle.
pub fn require_venue_inputs(job: &ExecJob, venue: &Venue) -> Result<()> {
    match job.trigger {
        TriggerKind::SvrAuction => {
            if job.hint_hash.is_none() {
                tracing::error!(trace = job.trace.raw(), "MissingHintHash before toggle");
                return Err(ExecError::MissingHintHash);
            }
        }
        TriggerKind::InterestDrift | TriggerKind::Stale | TriggerKind::ParamChange => {}
        TriggerKind::OraclePredicted => return Err(ExecError::PredictedNotSubmittable),
        TriggerKind::OraclePublic
        | TriggerKind::OraclePullHeld
        | TriggerKind::PoolStateChange
        | TriggerKind::UserAction
        | TriggerKind::DerivedRate => {
            if matches!(venue, Venue::BuilderBundle { .. }) {
                match &job.backrun_tx {
                    Some(b) if !b.is_empty() => {}
                    _ => {
                        tracing::error!(trace = job.trace.raw(), "MissingBackrunTx before toggle");
                        return Err(ExecError::MissingBackrunTx);
                    }
                }
            }
        }
    }
    Ok(())
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
    use crate::nonce::{NonceAllocator, NonceMode};
    use crate::submit::SubmitEnabled;
    use crate::template::PrecomputedSigner;
    use alloy_primitives::{address, b256};
    use liq_oracle::mevshare::SearcherKey;
    use std::sync::Arc;

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

    #[test]
    fn new_refuses_signer_nonce_key_mismatch() {
        let signer = Arc::new(
            PrecomputedSigner::from_secret(b256!(
                "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
            ))
            .unwrap(),
        );
        let other = address!("0x1111111111111111111111111111111111111111");
        let nonces = NonceAllocator::from_addresses(vec![other]).unwrap();
        let identity = SearcherKey::from_secret(b256!(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
        ))
        .unwrap();
        let builders = crate::builders::BuilderSet::from_parts(
            vec![crate::builders::BuilderEndpoint {
                id: liq_types::BuilderId(1),
                name: "t",
                endpoint: crate::builders::leak_str("http://127.0.0.1:1/".into()),
            }],
            crate::builders::leak_str("http://127.0.0.1:2/".into()),
        )
        .unwrap();
        let err = match ExecPath::new(
            CaptureRecorder::default(),
            AllowAll,
            Arc::new(SubmitEnabled::new(false)),
            NonceMode::Allocate,
            nonces,
            vec![signer],
            builders,
            identity,
            LiveSendBits::closed(),
        ) {
            Err(e) => e,
            Ok(_) => panic!("expected signer error"),
        };
        assert!(matches!(err, ExecError::Signer(_)));
    }

    #[test]
    fn svr_without_hint_fails_before_record() {
        let job = dummy_job(TriggerKind::SvrAuction);
        let builders = crate::builders::BuilderSet::from_parts(
            vec![crate::builders::BuilderEndpoint {
                id: liq_types::BuilderId(1),
                name: "t",
                endpoint: crate::builders::leak_str("http://127.0.0.1:1/".into()),
            }],
            crate::builders::leak_str("http://127.0.0.1:2/".into()),
        )
        .unwrap();
        let routed = crate::submit::route(job.trigger, &builders, 9_000, 10, 2).unwrap();
        let err = require_venue_inputs(&job, &routed.venue).unwrap_err();
        assert!(matches!(err, ExecError::MissingHintHash));
    }

    #[test]
    fn oracle_public_without_backrun_fails_before_record() {
        let job = dummy_job(TriggerKind::OraclePublic);
        let builders = crate::builders::BuilderSet::from_parts(
            vec![crate::builders::BuilderEndpoint {
                id: liq_types::BuilderId(1),
                name: "t",
                endpoint: crate::builders::leak_str("http://127.0.0.1:1/".into()),
            }],
            crate::builders::leak_str("http://127.0.0.1:2/".into()),
        )
        .unwrap();
        let routed = crate::submit::route(job.trigger, &builders, 9_000, 10, 2).unwrap();
        let err = require_venue_inputs(&job, &routed.venue).unwrap_err();
        assert!(matches!(err, ExecError::MissingBackrunTx));
    }

    #[test]
    fn submit_http_source_declares_keepalive_and_prewarm() {
        let src = include_str!("path.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        assert!(prod.contains("tcp_keepalive"));
        assert!(prod.contains("pool_idle_timeout"));
        assert!(prod.contains("tcp_nodelay"));
        assert!(prod.contains("fn warm_http"));
        assert!(prod.contains("fn prewarm"));
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
