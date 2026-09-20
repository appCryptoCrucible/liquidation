//! Governance timelock poller (GUIDE 08 §5, WP 08B).
//!
//! Discovers Aave/Spark executors and Aave V4 AccessManagers from the
//! **registry** (`addresses_provider` / `hub`) via on-chain views at a
//! pinned block — never a hand-written address list. Aave V3's ACL admin
//! stores the PayloadsController; V4 hubs expose `authority()`.
//!
//! A queued payload emits [`ScheduledParamChange`] only when the fork
//! block's timestamp is ≥ `queuedAt + delay` (the execution instant). The
//! crossing set is filled by `liq-engine::triggers` (08A `ThresholdIndex`);
//! this crate does not depend on the engine (forbid.txt).

use crate::{OracleError, Result};
use alloy_primitives::{Address, Bytes, U256};
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_rpc_types_eth::{BlockNumberOrTag, Filter, TransactionInput, TransactionRequest};
use alloy_sol_types::{sol, SolCall, SolEvent};
use liq_config::{Intern, OnChainId, Registry};
use liq_types::{LogFilter, LogSubscriber, MarketId, ProtocolId, ScheduledParamChange, TraceId};
use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Mutex;

/// Fork pin used by 08A/15A-2 fixtures; every live read in this module is
/// at this block unless the caller passes another.
pub const PIN_BLOCK: u64 = 26_018_679;

/// How far back from `getPayloadsCount` a poll inspects. Zero is invalid.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct GovernanceConfig {
    pub payload_lookback: u64,
}

impl GovernanceConfig {
    /// Refuse a zero lookback: that would skip every queued payload.
    pub fn new(payload_lookback: u64) -> Result<Self> {
        if payload_lookback == 0 {
            return Err(OracleError::ZeroPayloadLookback);
        }
        Ok(Self { payload_lookback })
    }
}

sol! {
    interface IPoolAddressesProvider {
        function getACLAdmin() external view returns (address);
    }
    interface IAccessManaged {
        function authority() external view returns (address);
    }
    interface IPayloadsController {
        function getPayloadsCount() external view returns (uint40);
        function getPayloadById(uint40 payloadId) external view returns (bytes);
        event PayloadQueued(uint40 payloadId);
        event PayloadExecuted(uint40 payloadId);
    }
    interface IAccessManager {
        function minSetback() external view returns (uint32);
        function getSchedule(bytes32 id) external view returns (uint48);
        event OperationScheduled(
            bytes32 indexed operationId,
            uint32 indexed nonce,
            uint48 schedule,
            address caller,
            address target,
            bytes data
        );
    }
}

/// Classified timelock discovered from a registry protocol row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timelock {
    pub protocol: ProtocolId,
    pub market: MarketId,
    pub executor: Address,
    pub kind: TimelockKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimelockKind {
    /// Aave governance-v3 PayloadsController behind the pool ACL admin.
    PayloadsController { controller: Address },
    /// OpenZeppelin AccessManager (`minSetback` live).
    AccessManager,
    /// ACL admin with no recognised queue ABI (Spark executor at the pin).
    Unclassified,
}

/// Decoded PayloadsController payload (static words only).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PayloadView {
    pub id: u64,
    pub queued_at: u64,
    pub executed_at: u64,
    pub cancelled_at: u64,
    pub delay: u64,
}

/// `true` when `block_ts` is at or past `queued_at + delay`. Overflow is
/// refuse, not wrap.
pub fn execution_due(queued_at: u64, delay: u64, block_ts: u64) -> Result<bool> {
    let due = queued_at
        .checked_add(delay)
        .ok_or_else(|| OracleError::Governance("queuedAt+delay overflow".into()))?;
    Ok(block_ts >= due)
}

/// Async (cold) poller. `Mutex` is only on the cursor, never on a hot path.
pub struct GovernancePoller {
    provider: RootProvider,
    cfg: GovernanceConfig,
    locks: Vec<Timelock>,
    cursor: Mutex<u64>,
}

impl GovernancePoller {
    /// Walk admitted Aave V3 / Spark / Aave V4-hub rows and resolve their
    /// timelocks at `block`.
    pub async fn from_registry(
        reg: &Registry,
        intern: &Intern,
        rpc_url: &str,
        cfg: GovernanceConfig,
        block: u64,
    ) -> Result<Self> {
        let provider = connect(rpc_url)?;
        let mut locks = Vec::new();
        let mut seen = BTreeSet::new();
        for entry in reg.protocols.values() {
            let Some(row) = classify_row(entry) else {
                continue;
            };
            let (protocol, market) = intern_ids(intern, &entry.family, entry.market)?;
            let exec = match row {
                Row::Provider(p) => acl_admin(&provider, p, block).await?,
                Row::Hub(h) => authority(&provider, h, block).await?,
            };
            if exec.is_zero() {
                return Err(OracleError::Governance(format!(
                    "zero timelock for family {}",
                    entry.family
                )));
            }
            if !seen.insert((exec, protocol, market)) {
                continue;
            }
            let kind = classify_kind(&provider, exec, block).await?;
            locks.push(Timelock {
                protocol,
                market,
                executor: exec,
                kind,
            });
        }
        if locks.is_empty() {
            return Err(OracleError::Governance(
                "registry produced no aave/spark/v4-hub timelocks".into(),
            ));
        }
        Ok(Self {
            provider,
            cfg,
            locks,
            cursor: Mutex::new(0),
        })
    }

    #[must_use]
    pub fn timelocks(&self) -> &[Timelock] {
        &self.locks
    }

    /// PayloadsController `getPayloadsCount` at `block`.
    pub async fn payloads_count(&self, controller: Address, block: u64) -> Result<u64> {
        let raw = call(
            &self.provider,
            controller,
            IPayloadsController::getPayloadsCountCall {}
                .abi_encode()
                .into(),
            block,
        )
        .await?;
        let n = IPayloadsController::getPayloadsCountCall::abi_decode_returns(&raw)
            .map_err(|e| OracleError::Governance(e.to_string()))?;
        Ok(n.to::<u64>())
    }

    /// One payload's static fields. Empty crossing — engine attaches it.
    pub async fn payload(&self, controller: Address, id: u64, block: u64) -> Result<PayloadView> {
        let id40 = u40(id)?;
        let data = IPayloadsController::getPayloadByIdCall { payloadId: id40 }.abi_encode();
        let raw = call(&self.provider, controller, data.into(), block).await?;
        decode_payload(id, &raw)
    }

    /// Open payloads in the lookback window whose timelock has matured at
    /// `block_ts`. `execution_block` is `block` (the first observed mature
    /// fork block — not an assumed seconds/12 conversion).
    pub async fn poll_matured(
        &self,
        block: u64,
        block_ts: u64,
    ) -> Result<Vec<ScheduledParamChange>> {
        let mut out = Vec::new();
        let mut high = 0u64;
        for lock in &self.locks {
            let TimelockKind::PayloadsController { controller } = lock.kind else {
                continue;
            };
            let count = self.payloads_count(controller, block).await?;
            let start = count.saturating_sub(self.cfg.payload_lookback);
            let cursor = {
                let g = self
                    .cursor
                    .lock()
                    .map_err(|_| OracleError::Governance("poller cursor poisoned".into()))?;
                *g
            };
            let mut i = cursor.max(start);
            while i < count {
                let p = self.payload(controller, i, block).await?;
                i = i.saturating_add(1);
                if p.queued_at == 0 || p.executed_at != 0 || p.cancelled_at != 0 {
                    continue;
                }
                if !execution_due(p.queued_at, p.delay, block_ts)? {
                    continue;
                }
                out.push(ScheduledParamChange {
                    protocol: lock.protocol,
                    market: lock.market,
                    execution_block: block,
                    crossing: Vec::new(),
                    trace: TraceId::from_raw(p.id),
                });
            }
            high = high.max(count);
        }
        if high > 0 {
            let mut g = self
                .cursor
                .lock()
                .map_err(|_| OracleError::Governance("poller cursor poisoned".into()))?;
            if high > *g {
                *g = high;
            }
        }
        Ok(out)
    }

    /// AccessManager `minSetback` at `block` (seconds, from chain).
    pub async fn min_setback(&self, manager: Address, block: u64) -> Result<u32> {
        let raw = call(
            &self.provider,
            manager,
            IAccessManager::minSetbackCall {}.abi_encode().into(),
            block,
        )
        .await?;
        IAccessManager::minSetbackCall::abi_decode_returns(&raw)
            .map_err(|e| OracleError::Governance(e.to_string()))
    }
}

impl LogSubscriber for GovernancePoller {
    fn subscriptions(&self) -> Vec<LogFilter> {
        let q = IPayloadsController::PayloadQueued::SIGNATURE_HASH;
        let e = IPayloadsController::PayloadExecuted::SIGNATURE_HASH;
        let s = IAccessManager::OperationScheduled::SIGNATURE_HASH;
        let mut out = Vec::new();
        for lock in &self.locks {
            match lock.kind {
                TimelockKind::PayloadsController { controller } => {
                    out.push(LogFilter {
                        address: controller,
                        topic0: q,
                    });
                    out.push(LogFilter {
                        address: controller,
                        topic0: e,
                    });
                }
                TimelockKind::AccessManager => {
                    out.push(LogFilter {
                        address: lock.executor,
                        topic0: s,
                    });
                }
                TimelockKind::Unclassified => {}
            }
        }
        out
    }
}

enum Row {
    Provider(Address),
    Hub(Address),
}

fn classify_row(entry: &liq_config::ProtocolEntry) -> Option<Row> {
    match entry.family.as_str() {
        "aave-v3" | "spark" => extra_addr(&entry.extra, "addresses_provider")
            .ok()
            .map(Row::Provider),
        "aave-v4" => {
            let kind = entry.extra.get("kind").and_then(|v| v.as_str());
            if kind != Some("hub") {
                return None;
            }
            extra_addr(&entry.extra, "hub").ok().map(Row::Hub)
        }
        _ => None,
    }
}

fn extra_addr(extra: &serde_json::Map<String, serde_json::Value>, key: &str) -> Result<Address> {
    let s = extra
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| OracleError::Governance(format!("missing extra.{key}")))?;
    Address::from_str(s).map_err(|e| OracleError::Governance(e.to_string()))
}

fn intern_ids(intern: &Intern, family: &str, market: OnChainId) -> Result<(ProtocolId, MarketId)> {
    let protocol = intern
        .protocol(family)
        .ok_or_else(|| OracleError::InternProtocol(family.into()))?;
    intern
        .markets()
        .iter()
        .find(|m| m.protocol == protocol && m.key == market)
        .map(|m| (protocol, m.id))
        .ok_or(OracleError::InternMarket {
            market: format!("{market:?}"),
        })
}

async fn classify_kind(provider: &RootProvider, exec: Address, block: u64) -> Result<TimelockKind> {
    if let Ok(n) = call(
        provider,
        exec,
        IPayloadsController::getPayloadsCountCall {}
            .abi_encode()
            .into(),
        block,
    )
    .await
    {
        if IPayloadsController::getPayloadsCountCall::abi_decode_returns(&n).is_ok() {
            return Ok(TimelockKind::PayloadsController { controller: exec });
        }
    }
    let slot0 = storage_addr(provider, exec, block).await?;
    if !slot0.is_zero() {
        if let Ok(raw) = call(
            provider,
            slot0,
            IPayloadsController::getPayloadsCountCall {}
                .abi_encode()
                .into(),
            block,
        )
        .await
        {
            if IPayloadsController::getPayloadsCountCall::abi_decode_returns(&raw).is_ok() {
                return Ok(TimelockKind::PayloadsController { controller: slot0 });
            }
        }
    }
    if let Ok(raw) = call(
        provider,
        exec,
        IAccessManager::minSetbackCall {}.abi_encode().into(),
        block,
    )
    .await
    {
        if IAccessManager::minSetbackCall::abi_decode_returns(&raw).is_ok() {
            return Ok(TimelockKind::AccessManager);
        }
    }
    Ok(TimelockKind::Unclassified)
}

fn connect(rpc_url: &str) -> Result<RootProvider> {
    if rpc_url.is_empty() {
        return Err(OracleError::Governance("empty rpc_url".into()));
    }
    let url = rpc_url
        .parse()
        .map_err(|e| OracleError::Governance(format!("invalid rpc_url: {e}")))?;
    Ok(ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(url))
}

async fn call(provider: &RootProvider, to: Address, data: Bytes, block: u64) -> Result<Bytes> {
    let tx = TransactionRequest {
        to: Some(to.into()),
        input: TransactionInput::new(data),
        ..Default::default()
    };
    let out = provider
        .call(tx)
        .number(block)
        .await
        .map_err(|e| OracleError::Governance(e.to_string()))?;
    if out.is_empty() {
        return Err(OracleError::Governance(format!("empty eth_call {to:#x}")));
    }
    Ok(out)
}

async fn acl_admin(provider: &RootProvider, to: Address, block: u64) -> Result<Address> {
    let raw = call(
        provider,
        to,
        IPoolAddressesProvider::getACLAdminCall {}
            .abi_encode()
            .into(),
        block,
    )
    .await?;
    IPoolAddressesProvider::getACLAdminCall::abi_decode_returns(&raw)
        .map_err(|e| OracleError::Governance(e.to_string()))
}

async fn authority(provider: &RootProvider, to: Address, block: u64) -> Result<Address> {
    let raw = call(
        provider,
        to,
        IAccessManaged::authorityCall {}.abi_encode().into(),
        block,
    )
    .await?;
    IAccessManaged::authorityCall::abi_decode_returns(&raw)
        .map_err(|e| OracleError::Governance(e.to_string()))
}

async fn storage_addr(provider: &RootProvider, to: Address, block: u64) -> Result<Address> {
    let slot = provider
        .get_storage_at(to, U256::ZERO)
        .number(block)
        .await
        .map_err(|e| OracleError::Governance(e.to_string()))?;
    let bytes = slot.to_be_bytes::<32>();
    let tail = bytes
        .get(12..32)
        .ok_or_else(|| OracleError::Governance("storage word short".into()))?;
    let out: [u8; 20] = tail
        .try_into()
        .map_err(|_| OracleError::Governance("storage address width".into()))?;
    Ok(Address::from(out))
}

fn u40(id: u64) -> Result<alloy_primitives::aliases::U40> {
    alloy_primitives::aliases::U40::try_from(id)
        .map_err(|_| OracleError::Governance(format!("payload id {id} > u40")))
}

fn decode_payload(id: u64, raw: &[u8]) -> Result<PayloadView> {
    // ABI: offset word + creator, access, extra, created, queued, executed,
    // cancelled, expiration, delay, grace, actions…
    let queued_at = word_u64(raw, id, 5)?;
    let executed_at = word_u64(raw, id, 6)?;
    let cancelled_at = word_u64(raw, id, 7)?;
    let delay = word_u64(raw, id, 9)?;
    Ok(PayloadView {
        id,
        queued_at,
        executed_at,
        cancelled_at,
        delay,
    })
}

fn word_u64(raw: &[u8], id: u64, i: usize) -> Result<u64> {
    let start = i.saturating_mul(32);
    let end = start.saturating_add(32);
    let w = raw
        .get(start..end)
        .ok_or(OracleError::PayloadTruncated { id, len: raw.len() })?;
    let v = U256::from_be_slice(w);
    u64::try_from(v).map_err(|_| OracleError::Governance(format!("word {i} of payload {id} > u64")))
}

/// Keep `Filter` reachable so log-subscription tests compile against the same
/// alloy types the poller will use for `OperationScheduled` pages.
#[allow(dead_code)]
fn queued_filter(controller: Address, from: u64, to: u64) -> Filter {
    Filter::new()
        .address(controller)
        .event_signature(IPayloadsController::PayloadQueued::SIGNATURE_HASH)
        .from_block(BlockNumberOrTag::Number(from))
        .to_block(BlockNumberOrTag::Number(to))
}

#[cfg(test)]
mod tests {
    use super::*;
    use liq_config::{Intern, Registry};
    use std::path::PathBuf;
    use std::time::Duration;

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    fn rpc() -> String {
        std::env::var("LIQ_RPC_URL")
            .or_else(|_| std::env::var("MAINNET_RPC_URL"))
            .unwrap_or_else(|_| "https://eth.drpc.org".into())
    }

    fn committed() -> (Registry, Intern) {
        let reg = Registry::from_path(&root().join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        (reg, intern)
    }

    /// Oracle: `queuedAt + delay` is the maturity; pin ts is before payload
    /// 469's due time; due+1 is after. Negative: overflow refuses.
    #[test]
    fn execution_due_is_exact_and_overflow_is_refused() {
        let queued = 1_789_830_371u64;
        let delay = 86_400u64;
        assert!(!execution_due(queued, delay, 1_789_906_763).unwrap());
        assert!(execution_due(queued, delay, queued.checked_add(delay).unwrap()).unwrap());
        assert!(execution_due(u64::MAX, 1, u64::MAX).is_err());
    }

    #[test]
    fn lookback_zero_is_refused() {
        assert!(matches!(
            GovernanceConfig::new(0),
            Err(OracleError::ZeroPayloadLookback)
        ));
    }

    /// Oracle: registry-derived ACL admin + PayloadsController at PIN_BLOCK.
    /// Count 471 and payload 469 queued/not executed with delay 86400 are
    /// `cast call` observations, not guesses. Negative: a mutated lookback
    /// of 0 cannot construct.
    #[tokio::test(flavor = "current_thread")]
    async fn pin_discovers_aave_payloads_controller_and_open_payload_469() {
        let (reg, intern) = committed();
        let poller = tokio::time::timeout(
            Duration::from_secs(90),
            GovernancePoller::from_registry(
                &reg,
                &intern,
                &rpc(),
                GovernanceConfig::new(64).unwrap(),
                PIN_BLOCK,
            ),
        )
        .await
        .expect("rpc timed out — fail closed")
        .unwrap();
        let aave = poller
            .timelocks()
            .iter()
            .find(|t| matches!(t.kind, TimelockKind::PayloadsController { .. }))
            .expect("aave-v3 payloads controller");
        let TimelockKind::PayloadsController { controller } = aave.kind else {
            panic!("kind");
        };
        let n = poller.payloads_count(controller, PIN_BLOCK).await.unwrap();
        assert_eq!(n, 471, "cast getPayloadsCount @ {PIN_BLOCK}");
        let p = poller.payload(controller, 469, PIN_BLOCK).await.unwrap();
        assert_eq!(p.queued_at, 1_789_830_371);
        assert_eq!(p.executed_at, 0);
        assert_eq!(p.cancelled_at, 0);
        assert_eq!(p.delay, 86_400);
        assert!(!execution_due(p.queued_at, p.delay, 1_789_906_763).unwrap());
        let v4 = poller
            .timelocks()
            .iter()
            .filter(|t| matches!(t.kind, TimelockKind::AccessManager))
            .count();
        assert!(v4 >= 1, "at least one V4 hub AccessManager");
        let spark = poller
            .timelocks()
            .iter()
            .any(|t| matches!(t.kind, TimelockKind::Unclassified));
        assert!(spark, "spark ACL admin is unclassified at this pin");
        let subs = poller.subscriptions();
        assert!(subs.iter().any(|s| s.address == controller));
        let _ = queued_filter(controller, PIN_BLOCK, PIN_BLOCK);
    }
}
