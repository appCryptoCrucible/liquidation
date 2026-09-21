//! Bind 13A [`ExecPath`]: [`RiskGate`] as [`RiskAllow`], [`ShadowRecorder`]
//! as the recorder. `submit_enabled` stays default false. Live HTTP also
//! requires the lease atomics (resync ABSENT — 17A never stores true).

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

#[cfg(not(test))]
use alloy_primitives::b256;
use alloy_primitives::B256;
use liq_exec::builders::{load_builders, BuilderSet};
use liq_exec::nonce::{NonceAllocator, NonceMode};
use liq_exec::path::ExecPath;
use liq_exec::submit::{LiveSendBits, SubmitEnabled};
use liq_exec::template::PrecomputedSigner;
use liq_obs::ShadowRecorder;
use liq_oracle::mevshare::SearcherKey;
use liq_risk::RiskGate;

use crate::lease::SubmitLease;
use crate::shared::Shared;

/// Well-known Anvil account-0 key. Production parse refuses it.
#[cfg(not(test))]
const ANVIL_DEV_SECRET: B256 =
    b256!("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

/// 32-byte operator + identity secrets. Never invented; never committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessSecrets {
    pub operator: B256,
    pub identity: B256,
}

impl ProcessSecrets {
    /// Hex from `LIQ_OPERATOR_SECRET` / `LIQ_IDENTITY_SECRET`. Missing or
    /// malformed → `None` (process still starts; ExecPath stays unbound).
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let operator = match std::env::var("LIQ_OPERATOR_SECRET") {
            Ok(s) if !s.trim().is_empty() => s,
            _ => {
                tracing::error!("LIQ_OPERATOR_SECRET missing — ExecPath unbound");
                return None;
            }
        };
        let identity = match std::env::var("LIQ_IDENTITY_SECRET") {
            Ok(s) if !s.trim().is_empty() => s,
            _ => {
                tracing::error!("LIQ_IDENTITY_SECRET missing — ExecPath unbound");
                return None;
            }
        };
        Self::from_hex(&operator, &identity)
    }

    /// Parse two 32-byte hex secrets. Zero / Anvil-outside-test / bad hex → None.
    #[must_use]
    pub fn from_hex(operator: &str, identity: &str) -> Option<Self> {
        Some(Self {
            operator: parse_secret(operator)?,
            identity: parse_secret(identity)?,
        })
    }
}

fn parse_secret(raw: &str) -> Option<B256> {
    let t = raw.trim();
    let secret = match B256::from_str(t) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "secret hex refused");
            return None;
        }
    };
    if secret.is_zero() {
        tracing::error!("secret is the zero key — refused");
        return None;
    }
    refuse_anvil(secret)
}

#[allow(clippy::needless_return)]
fn refuse_anvil(secret: B256) -> Option<B256> {
    #[cfg(test)]
    {
        return Some(secret);
    }
    #[cfg(not(test))]
    {
        if secret == ANVIL_DEV_SECRET {
            tracing::error!("anvil well-known key refused outside test");
            return None;
        }
        Some(secret)
    }
}

/// Production bind. Signer↔nonce check lives in [`ExecPath::new`].
/// Attaches the process lease atomics — an unused helper is not a gate.
#[allow(clippy::too_many_arguments)]
pub fn bind(
    recorder: ShadowRecorder,
    gate: &'static RiskGate,
    submit_enabled: Arc<SubmitEnabled>,
    nonces: NonceAllocator,
    signers: Vec<Arc<PrecomputedSigner>>,
    builders: BuilderSet,
    identity: SearcherKey,
    lease: &SubmitLease,
) -> liq_exec::error::Result<ExecPath<ShadowRecorder, &'static RiskGate>> {
    ExecPath::new(
        recorder,
        gate,
        submit_enabled,
        NonceMode::Allocate,
        nonces,
        signers,
        builders,
        identity,
        LiveSendBits {
            held: lease.held_flag(),
            nonce_resync: lease.nonce_resync_flag(),
        },
    )
}

/// Process bind. Missing secrets / builders / recorder → `None`, log, no
/// invented signer. Resync is not stored true.
pub fn bind_from_config(
    builders_path: &Path,
    shadow_dir: &Path,
    shared: &Shared,
    secrets: Option<ProcessSecrets>,
) -> Option<ExecPath<ShadowRecorder, &'static RiskGate>> {
    let Some(secrets) = secrets else {
        tracing::error!("process secrets absent — ExecPath unbound (ingest/lease continue)");
        return None;
    };
    let builders = match load_builders(builders_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, path = %builders_path.display(), "builders load refused — ExecPath unbound");
            return None;
        }
    };
    if let Err(e) = std::fs::create_dir_all(shadow_dir) {
        tracing::error!(error = %e, path = %shadow_dir.display(), "shadow dir refused — ExecPath unbound");
        return None;
    }
    let recorder = match ShadowRecorder::open(shadow_dir) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "shadow recorder refused — ExecPath unbound");
            return None;
        }
    };
    try_bind_process(
        recorder,
        shared.risk,
        Arc::clone(&shared.submit_enabled),
        builders,
        secrets,
        shared.lease,
    )
}

/// Signers from supplied secrets, then [`bind`]. No Anvil fallback.
pub fn try_bind_process(
    recorder: ShadowRecorder,
    gate: &'static RiskGate,
    submit_enabled: Arc<SubmitEnabled>,
    builders: BuilderSet,
    secrets: ProcessSecrets,
    lease: &SubmitLease,
) -> Option<ExecPath<ShadowRecorder, &'static RiskGate>> {
    let signer = match PrecomputedSigner::from_secret(secrets.operator) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!(error = %e, "operator signer refused — ExecPath unbound");
            return None;
        }
    };
    let identity = match SearcherKey::from_secret(secrets.identity) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(error = %e, "identity key refused — ExecPath unbound");
            return None;
        }
    };
    let nonces = match NonceAllocator::from_addresses(vec![signer.address()]) {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(error = %e, "nonce allocator refused — ExecPath unbound");
            return None;
        }
    };
    match bind(
        recorder,
        gate,
        submit_enabled,
        nonces,
        vec![signer],
        builders,
        identity,
        lease,
    ) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::error!(error = %e, "exec_bind::bind refused — ExecPath unbound");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256, Address, Bytes, B256};
    use liq_config::BotConfig;
    use liq_exec::builders::{leak_str, BuilderEndpoint};
    use liq_exec::fee::FeeQuote;
    use liq_exec::path::ExecJob;
    use liq_types::{
        AssetId, BuilderId, FlashProvider, MarketId, PositionKey, ProtocolId, SubmitReceipt,
        TraceId, TriggerKind,
    };
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const SECRET: B256 =
        b256!("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
    const OPERATOR: Address = address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    static DIRS: AtomicU64 = AtomicU64::new(0);

    fn job() -> ExecJob {
        ExecJob {
            trace: TraceId::from_raw(7),
            plan: Bytes::from_static(&[0xab]),
            trigger: TriggerKind::SvrAuction,
            protocol: ProtocolId(1),
            market: MarketId(1),
            collateral: AssetId(0),
            debt: AssetId(1),
            flash: FlashProvider::Aave,
            operator_key: OPERATOR,
            position: PositionKey {
                protocol: ProtocolId(1),
                market: MarketId(1),
                user: address!("0x1111111111111111111111111111111111111111"),
            },
            hint_hash: Some(b256!(
                "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            )),
            backrun_tx: None,
            target_block: 101,
            max_block: 103,
            fee: FeeQuote {
                parent_block: 100,
                next_base_fee: 1_000,
                priority_wei: 10,
                modest_priority_wei: 2,
            },
            auction_bps: 9_000,
            refund_address: OPERATOR,
            calldata: Bytes::from_static(&[0x11]),
            gas_limit: 200_000,
            chain_id: 1,
            slot: 0,
        }
    }

    fn shadow_dir() -> std::path::PathBuf {
        let n = DIRS.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("liq-17a-bind-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn bind_path(
        flag: Arc<SubmitEnabled>,
        lease: &SubmitLease,
        builders: BuilderSet,
    ) -> ExecPath<ShadowRecorder, &'static RiskGate> {
        let recorder = ShadowRecorder::open(shadow_dir()).unwrap();
        let gate: &'static RiskGate = Box::leak(Box::new(RiskGate::new()));
        let signer = Arc::new(PrecomputedSigner::from_secret(SECRET).unwrap());
        let nonces = NonceAllocator::from_addresses(vec![signer.address()]).unwrap();
        let identity = SearcherKey::from_secret(SECRET).unwrap();
        bind(
            recorder,
            gate,
            flag,
            nonces,
            vec![signer],
            builders,
            identity,
            lease,
        )
        .unwrap()
    }

    fn builders_for(url: &str) -> BuilderSet {
        let relay = leak_str(url.to_owned());
        BuilderSet::from_parts(
            vec![BuilderEndpoint {
                id: BuilderId(1),
                name: "mock",
                endpoint: relay,
            }],
            relay,
        )
        .unwrap()
    }

    async fn spawn_mock() -> (String, Arc<AtomicU64>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicU64::new(0));
        let hits_t = Arc::clone(&hits);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let hits_t = Arc::clone(&hits_t);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16_384];
                    let mut got = Vec::new();
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                got.extend_from_slice(buf.get(..n).unwrap_or(&[]));
                                if got.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    hits_t.fetch_add(1, Ordering::Relaxed);
                    let payload = br#"{"jsonrpc":"2.0","id":1,"result":{"bundleHash":"0x00"}}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let mut out = resp.into_bytes();
                    out.extend_from_slice(payload);
                    let _ = sock.write_all(&out).await;
                });
            }
        });
        (format!("http://{addr}/"), hits)
    }

    /// Old path POSTed on `submit_enabled` alone. Must be Recorded + 0 HTTP.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enabled_held_resync_false_zero_http() {
        let (url, hits) = spawn_mock().await;
        let flag = Arc::new(SubmitEnabled::new(true));
        let lease = SubmitLease::granted_shadow();
        assert!(lease.held());
        assert!(!lease.nonce_resync());
        let path = bind_path(flag, &lease, builders_for(&url));
        let rec = path.submit_path(&job()).await.unwrap();
        assert_eq!(rec, SubmitReceipt::Recorded);
        assert_eq!(hits.load(Ordering::Relaxed), 0);
    }

    /// Test-only resync true; held false. Old path POSTed. Must be 0 HTTP.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enabled_unheld_resync_true_zero_http() {
        let (url, hits) = spawn_mock().await;
        let flag = Arc::new(SubmitEnabled::new(true));
        let lease = SubmitLease::refused();
        lease.nonce_resync_flag().store(true, Ordering::Release);
        assert!(!lease.held());
        assert!(lease.nonce_resync());
        let path = bind_path(flag, &lease, builders_for(&url));
        let rec = path.submit_path(&job()).await.unwrap();
        assert_eq!(rec, SubmitReceipt::Recorded);
        assert_eq!(hits.load(Ordering::Relaxed), 0);
    }

    /// `submit_enabled` false; test-only held+resync true. Must be 0 HTTP.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabled_held_resync_true_zero_http() {
        let (url, hits) = spawn_mock().await;
        let flag = Arc::new(SubmitEnabled::new(false));
        let lease = SubmitLease::granted_shadow();
        lease.nonce_resync_flag().store(true, Ordering::Release);
        let path = bind_path(flag, &lease, builders_for(&url));
        let rec = path.submit_path(&job()).await.unwrap();
        assert_eq!(rec, SubmitReceipt::Recorded);
        assert_eq!(hits.load(Ordering::Relaxed), 0);
    }

    /// Test-only force of all three. Production has no such force.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn all_three_true_posts_accepted() {
        let (url, hits) = spawn_mock().await;
        let flag = Arc::new(SubmitEnabled::new(true));
        let lease = SubmitLease::granted_shadow();
        lease.nonce_resync_flag().store(true, Ordering::Release);
        let path = bind_path(flag, &lease, builders_for(&url));
        let rec = path.submit_path(&job()).await.unwrap();
        assert_eq!(rec, SubmitReceipt::Accepted);
        assert!(hits.load(Ordering::Relaxed) >= 1);
    }

    /// Hot-reload `submit_enabled` is not enough. Conjunction is read at send
    /// (lease atomics, not a bind-time bool). The f313737 drill that POSTed
    /// while a helper said closed is gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hot_reload_toggle_posts_only_when_conjunction_true() {
        let (url, hits) = spawn_mock().await;
        let flag = Arc::new(SubmitEnabled::new(false));
        let lease = SubmitLease::granted_shadow();
        let path = bind_path(Arc::clone(&flag), &lease, builders_for(&url));

        let rec = path.submit_path(&job()).await.unwrap();
        assert_eq!(rec, SubmitReceipt::Recorded);
        assert_eq!(hits.load(Ordering::Relaxed), 0);

        flag.set(true);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let rec = path.submit_path(&job()).await.unwrap();
        assert_eq!(rec, SubmitReceipt::Recorded);
        assert_eq!(hits.load(Ordering::Relaxed), 0);

        lease.nonce_resync_flag().store(true, Ordering::Release);
        let rec = path.submit_path(&job()).await.unwrap();
        assert_eq!(rec, SubmitReceipt::Accepted);
        assert!(hits.load(Ordering::Relaxed) >= 1);

        lease.held_flag().store(false, Ordering::Release);
        let before = hits.load(Ordering::Relaxed);
        let rec = path.submit_path(&job()).await.unwrap();
        assert_eq!(rec, SubmitReceipt::Recorded);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(hits.load(Ordering::Relaxed), before);

        lease.held_flag().store(true, Ordering::Release);
        flag.set(false);
        let rec = path.submit_path(&job()).await.unwrap();
        assert_eq!(rec, SubmitReceipt::Recorded);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(hits.load(Ordering::Relaxed), before);
    }

    fn test_secrets() -> ProcessSecrets {
        ProcessSecrets::from_hex(
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .expect("test hex")
    }

    /// Process bind with test secrets: same path as `run()`, exec Some,
    /// resync false, submit_enabled false → Recorded + 0 HTTP.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn process_bind_with_test_secrets_recorded_zero_http() {
        let (url, hits) = spawn_mock().await;
        let recorder = ShadowRecorder::open(shadow_dir()).unwrap();
        let gate: &'static RiskGate = Box::leak(Box::new(RiskGate::new()));
        let flag = Arc::new(SubmitEnabled::new(false));
        let lease = SubmitLease::granted_shadow();
        assert!(!lease.nonce_resync());
        assert!(!flag.get());
        let path = try_bind_process(
            recorder,
            gate,
            flag,
            builders_for(&url),
            test_secrets(),
            &lease,
        )
        .expect("bound");
        assert!(!path.nonce_resync.load(Ordering::Acquire));
        assert!(!path.submit_enabled.get());
        let rec = path.submit_path(&job()).await.unwrap();
        assert_eq!(rec, SubmitReceipt::Recorded);
        assert_eq!(hits.load(Ordering::Relaxed), 0);
    }

    fn bind_test_cfg() -> BotConfig {
        BotConfig {
            chain_id: 1,
            registry_path: std::path::PathBuf::from("registry/registry.json"),
            rpc_url: String::new(),
            risk: liq_config::RiskConfig::default(),
            venues: liq_config::VenuesConfig::default(),
            submit_enabled: false,
        }
    }

    #[test]
    fn process_bind_no_secrets_unbound_no_invented_key() {
        let shared = crate::startup::leak_process_shared(&bind_test_cfg(), SubmitLease::refused());
        let shadow = shadow_dir();
        let builders = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/builders.toml");
        let exec = bind_from_config(&builders, &shadow, shared, None);
        assert!(exec.is_none(), "missing secrets must not invent a signer");
        assert!(ProcessSecrets::from_hex("not-hex", "not-hex").is_none());
        assert!(ProcessSecrets::from_hex(
            "0x0000000000000000000000000000000000000000000000000000000000000000",
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
        )
        .is_none());
    }

    #[test]
    fn bind_from_config_with_test_secrets_is_some() {
        let shared =
            crate::startup::leak_process_shared(&bind_test_cfg(), SubmitLease::granted_shadow());
        assert!(!shared.lease.nonce_resync());
        assert!(!shared.submit_enabled.get());
        let shadow = shadow_dir();
        let builders = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/builders.toml");
        let exec = bind_from_config(&builders, &shadow, shared, Some(test_secrets()));
        assert!(exec.is_some(), "old unbound process would leave exec None");
        let path = exec.unwrap();
        assert!(!path.nonce_resync.load(Ordering::Acquire));
        assert!(!path.submit_enabled.get());
    }
}
