//! Venue submitters: [`MevShare`] (06B wrap) and [`BuilderBundle`] (`JoinSet`).
//!
//! Live HTTP is gated at send time by [`SubmitEnabled`] ∧ lease held ∧
//! nonce resync (held/resync default **false**). The 06B
//! `MevShareSubmitter::submit` still returns `LiveSendIs13A`; this module
//! is the type that POSTs when the three-way conjunction is true.

use crate::builders::BuilderSet;
use crate::error::{ExecError, Result};
use alloy_primitives::{Address, Bytes, U256};
use liq_oracle::mevshare::{
    post_signed, sign_body, MevShareSubmitter, SearcherKey, SendBundle, SignedRelayRequest,
};
use liq_types::{IntendedSubmission, SubmitReceipt, Submitter, TriggerKind, Venue};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::task::JoinSet;

/// Hot-reloadable live-send flag (H4 flip; 17A owns SIGHUP later).
#[derive(Debug)]
pub struct SubmitEnabled {
    flag: AtomicBool,
}

impl SubmitEnabled {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            flag: AtomicBool::new(enabled),
        }
    }

    pub fn set(&self, enabled: bool) {
        self.flag.store(enabled, Ordering::Release);
    }

    #[must_use]
    pub fn get(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}

impl Default for SubmitEnabled {
    fn default() -> Self {
        Self::new(false)
    }
}

/// Lease bits read at POST (Acquire). Both default false so an unbound
/// [`crate::path::ExecPath`] cannot live-send. `liq-bot` `bind` attaches the
/// process lease atomics; this crate does not depend on `liq-bot`.
#[derive(Clone, Debug)]
pub struct LiveSendBits {
    pub held: Arc<AtomicBool>,
    pub nonce_resync: Arc<AtomicBool>,
}

impl LiveSendBits {
    #[must_use]
    pub fn closed() -> Self {
        Self {
            held: Arc::new(AtomicBool::new(false)),
            nonce_resync: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Default for LiveSendBits {
    fn default() -> Self {
        Self::closed()
    }
}

fn searcher_key_addr(key: &SearcherKey) -> Address {
    key.address()
}

/// 13A MEV-Share sender. Signing is 06B (`sign_send_bundle`); POST is
/// 06B `post_signed`.
pub struct MevShare {
    inner: MevShareSubmitter,
    pub relay: &'static str,
}

impl MevShare {
    #[must_use]
    pub fn new(relay: &'static str, key: SearcherKey) -> Self {
        Self {
            inner: MevShareSubmitter::new(relay, key),
            relay,
        }
    }

    pub fn sign(&self, bundle: &SendBundle) -> Result<SignedRelayRequest> {
        self.inner
            .sign_send_bundle(bundle)
            .map_err(|e| ExecError::Identity(e.to_string()))
    }

    /// POST an already-signed request. Caller gates at [`crate::path::ExecPath::submit_path`].
    pub async fn send(
        &self,
        client: &reqwest::Client,
        req: &SignedRelayRequest,
    ) -> Result<serde_json::Value> {
        post_signed(client, req)
            .await
            .map_err(|e| ExecError::Http(e.to_string()))
    }
}

impl Submitter for MevShare {
    type Error = ExecError;

    fn submit(&self, submission: &IntendedSubmission) -> Result<SubmitReceipt> {
        let Venue::MevShare { relay } = submission.venue else {
            return Err(ExecError::WrongVenueMevShare);
        };
        if relay != self.relay {
            return Err(ExecError::RelayMismatch);
        }
        Ok(SubmitReceipt::Recorded)
    }
}

/// Curated `eth_sendBundle` fan-out. `JoinSet`, not sequential awaits.
pub struct BuilderBundle {
    pub set: BuilderSet,
}

impl BuilderBundle {
    #[must_use]
    pub fn new(set: BuilderSet) -> Self {
        Self { set }
    }

    pub fn first_venue(&self) -> Result<Venue> {
        let b = self.set.builders.first().ok_or(ExecError::EmptyBuilders)?;
        Ok(Venue::BuilderBundle {
            endpoint: b.endpoint,
            builder: b.id,
        })
    }

    /// Sign one `eth_sendBundle` body with the identity key; fan-out the
    /// same signed bytes to every curated builder.
    pub async fn send(
        &self,
        client: &reqwest::Client,
        identity: &SearcherKey,
        block: u64,
        txs: &[Bytes],
    ) -> Result<FanoutReport> {
        let body = rpc_eth_send_bundle(block, txs)?;
        let header = sign_body(identity, &body).map_err(|e| ExecError::Identity(e.to_string()))?;
        let mut set = JoinSet::new();
        for b in &self.set.builders {
            let client = client.clone();
            let req = SignedRelayRequest {
                relay: b.endpoint,
                body: body.clone(),
                signature_header: header.clone(),
            };
            let id = b.id;
            set.spawn(async move {
                let r = post_signed(&client, &req).await;
                (id, r)
            });
        }
        let mut ok = 0u16;
        let mut fail = 0u16;
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((id, Ok(_))) => {
                    ok = ok.saturating_add(1);
                    tracing::info!(builder = id.0, "eth_sendBundle accepted");
                }
                Ok((id, Err(e))) => {
                    fail = fail.saturating_add(1);
                    tracing::error!(builder = id.0, err = %e, "eth_sendBundle failed");
                }
                Err(e) => {
                    fail = fail.saturating_add(1);
                    tracing::error!(err = %e, "builder task join failed");
                }
            }
        }
        if ok == 0 {
            return Err(ExecError::AllBuildersUnreachable);
        }
        Ok(FanoutReport { ok, fail })
    }
}

/// How many builders accepted / failed. No fallback if `ok == 0`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FanoutReport {
    pub ok: u16,
    pub fail: u16,
}

impl Submitter for BuilderBundle {
    type Error = ExecError;

    fn submit(&self, submission: &IntendedSubmission) -> Result<SubmitReceipt> {
        let Venue::BuilderBundle { endpoint, .. } = submission.venue else {
            return Err(ExecError::WrongVenueBuilder);
        };
        let known = self.set.builders.iter().any(|b| b.endpoint == endpoint);
        if !known {
            return Err(ExecError::RelayMismatch);
        }
        Ok(SubmitReceipt::Recorded)
    }
}

/// `eth_sendBundle` JSON-RPC UTF-8 body. No `revertingTxHashes`.
pub fn rpc_eth_send_bundle(block: u64, txs: &[Bytes]) -> Result<Vec<u8>> {
    if txs.is_empty() {
        return Err(ExecError::EmptyBundle);
    }
    let hex_txs: Vec<String> = txs.iter().map(|t| format!("{t:#x}")).collect();
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "eth_sendBundle",
        "params": [{
            "txs": hex_txs,
            "blockNumber": format!("0x{block:x}"),
        }],
    });
    serde_json::to_vec(&payload).map_err(|e| ExecError::Serde(e.to_string()))
}

/// Bid policy after [`TriggerKind`] routing. Auction bps is forced to 0
/// for `InterestDrift` and `Stale`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BidPolicy {
    pub auction_bps: u16,
    pub priority_wei: u128,
    pub modest: bool,
    pub one_tx: bool,
}

/// Route + bid for a trigger. `auction_bps` is the bid-channel value;
/// InterestDrift / Stale zero it.
pub fn bid_policy(
    kind: TriggerKind,
    auction_bps: u16,
    priority_wei: u128,
    modest_priority_wei: u128,
) -> Result<BidPolicy> {
    if auction_bps > 10_000 {
        return Err(ExecError::AuctionBpsRange(auction_bps));
    }
    match kind {
        TriggerKind::InterestDrift | TriggerKind::Stale => {
            if modest_priority_wei == 0 {
                return Err(ExecError::MissingModestPriority);
            }
            Ok(BidPolicy {
                auction_bps: 0,
                priority_wei: modest_priority_wei,
                modest: true,
                one_tx: true,
            })
        }
        TriggerKind::OraclePredicted => Err(ExecError::PredictedNotSubmittable),
        TriggerKind::SvrAuction => {
            if priority_wei == 0 {
                return Err(ExecError::MissingPriority);
            }
            Ok(BidPolicy {
                auction_bps,
                priority_wei,
                modest: false,
                one_tx: false,
            })
        }
        _ => {
            if priority_wei == 0 {
                return Err(ExecError::MissingPriority);
            }
            Ok(BidPolicy {
                auction_bps,
                priority_wei,
                modest: false,
                one_tx: false,
            })
        }
    }
}

/// RefundConfig percent: `auction_bps / 100`. Must be exact.
pub fn refund_percent(auction_bps: u16) -> Result<u8> {
    if auction_bps > 10_000 {
        return Err(ExecError::AuctionBpsRange(auction_bps));
    }
    let rem = auction_bps
        .checked_rem(100)
        .ok_or(ExecError::AuctionBpsNotPercent(auction_bps))?;
    if rem != 0 {
        return Err(ExecError::AuctionBpsNotPercent(auction_bps));
    }
    let pct = auction_bps
        .checked_div(100)
        .ok_or(ExecError::AuctionBpsNotPercent(auction_bps))?;
    u8::try_from(pct).map_err(|_| ExecError::AuctionBpsRange(auction_bps))
}

/// Venue + intended bid after routing.
#[derive(Clone, Debug)]
pub struct Routed {
    pub venue: Venue,
    pub intended_bid: U256,
    pub policy: BidPolicy,
}

pub fn route(
    kind: TriggerKind,
    builders: &BuilderSet,
    auction_bps: u16,
    priority_wei: u128,
    modest_priority_wei: u128,
) -> Result<Routed> {
    let policy = bid_policy(kind, auction_bps, priority_wei, modest_priority_wei)?;
    let venue = match kind {
        TriggerKind::SvrAuction => Venue::MevShare {
            relay: builders.mevshare_relay,
        },
        _ => {
            let b = builders.builders.first().ok_or(ExecError::EmptyBuilders)?;
            Venue::BuilderBundle {
                endpoint: b.endpoint,
                builder: b.id,
            }
        }
    };
    let intended_bid = if policy.modest {
        U256::from(policy.priority_wei)
    } else {
        U256::from(policy.auction_bps)
    };
    Ok(Routed {
        venue,
        intended_bid,
        policy,
    })
}

/// Shared HTTP client + identity key for both venues.
pub struct HttpCtx {
    pub client: reqwest::Client,
    pub identity: SearcherKey,
    pub enabled: Arc<SubmitEnabled>,
}

impl HttpCtx {
    pub fn new(identity: SearcherKey, enabled: Arc<SubmitEnabled>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .map_err(|e| ExecError::Http(e.to_string()))?;
        Ok(Self {
            client,
            identity,
            enabled,
        })
    }
}

// Silence unused helper in non-test builds if identity address is unused.
#[allow(dead_code)]
fn _identity_addr(ctx: &HttpCtx) -> Address {
    searcher_key_addr(&ctx.identity)
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
    use crate::builders::{leak_str, BuilderEndpoint};
    use alloy_primitives::b256;
    use liq_types::BuilderId;

    fn builders() -> BuilderSet {
        BuilderSet::from_parts(
            vec![BuilderEndpoint {
                id: BuilderId(1),
                name: "mock-a",
                endpoint: leak_str("http://127.0.0.1:1/".into()),
            }],
            leak_str("http://127.0.0.1:2/".into()),
        )
        .unwrap()
    }

    #[test]
    fn interest_drift_never_carries_auction_bid() {
        let r = route(TriggerKind::InterestDrift, &builders(), 9_000, 50, 2).unwrap();
        assert_eq!(r.policy.auction_bps, 0);
        assert!(r.policy.modest);
        assert!(r.policy.one_tx);
        assert_eq!(r.intended_bid, U256::from(2u64));
        assert_ne!(r.intended_bid, U256::from(9_000u64));
        match r.venue {
            Venue::BuilderBundle { .. } => {}
            Venue::MevShare { .. } => panic!("InterestDrift must not route to MevShare"),
        }
        let svr = route(TriggerKind::SvrAuction, &builders(), 9_000, 50, 2).unwrap();
        assert_eq!(svr.policy.auction_bps, 9_000);
        assert_eq!(svr.intended_bid, U256::from(9_000u64));
        match svr.venue {
            Venue::MevShare { .. } => {}
            Venue::BuilderBundle { .. } => panic!("SvrAuction must route to MevShare"),
        }
    }

    #[test]
    fn stale_is_one_tx_modest() {
        let r = route(TriggerKind::Stale, &builders(), 8_000, 50, 3).unwrap();
        assert_eq!(r.policy.auction_bps, 0);
        assert!(r.policy.one_tx);
        assert_eq!(refund_percent(0).unwrap(), 0);
        assert_eq!(refund_percent(9_000).unwrap(), 90);
        assert!(refund_percent(9_050).is_err());
    }

    #[test]
    fn submit_enabled_defaults_false() {
        let f = SubmitEnabled::default();
        assert!(!f.get());
        f.set(true);
        assert!(f.get());
        f.set(false);
        assert!(!f.get());
    }

    #[test]
    fn live_send_bits_default_closed() {
        let b = LiveSendBits::default();
        assert!(!b.held.load(Ordering::Acquire));
        assert!(!b.nonce_resync.load(Ordering::Acquire));
        let c = LiveSendBits::closed();
        assert!(!c.held.load(Ordering::Acquire));
        assert!(!c.nonce_resync.load(Ordering::Acquire));
    }

    #[test]
    fn eth_send_bundle_body_is_one_tx_atomic() {
        let tx = Bytes::from(vec![0x02, 0xf8]);
        let body = rpc_eth_send_bundle(26_000_000, &[tx]).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["method"], "eth_sendBundle");
        assert!(v["params"][0]["revertingTxHashes"].is_null());
        assert_eq!(v["params"][0]["txs"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn submitter_impls_validate_venue() {
        let key = SearcherKey::from_secret(b256!(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
        ))
        .unwrap();
        let set = builders();
        let relay = set.mevshare_relay;
        let ms = MevShare::new(relay, key);
        let intended = IntendedSubmission {
            plan: Bytes::new(),
            bid: U256::from(1u64),
            venue: Venue::MevShare { relay },
            deadline: 1,
            trace: liq_types::TraceId::from_raw(1),
        };
        assert_eq!(ms.submit(&intended).unwrap(), SubmitReceipt::Recorded);
        let bb = BuilderBundle::new(set);
        let mut other = intended.clone();
        other.venue = Venue::BuilderBundle {
            endpoint: "http://127.0.0.1:9/",
            builder: BuilderId(1),
        };
        assert!(bb.submit(&other).is_err());
    }
}
