//! WP 05F — GUIDE 05 Step 0 lite validation smoke.
//!
//! Bounded universe over a public RPC (`LIQ_RPC_URL` only; no default
//! endpoint). `LIQ_LITE_SPOKE` is required and must be an admitted Aave V4
//! spoke. Polls via [`liq_watch::source::RpcPoll`] — an independent copy of
//! 03A's `eth_getLogs` poller. This module does **not** call 03A's
//! [`liq_node::source::RpcPoll`] or `LogRouter`. W's copy is used because
//! [`WatchDecoder::decode_log`] needs `block_hash` / `tx_hash`. Fold is
//! 04A [`Protocol::apply_log`] on those logs converted to
//! [`liq_protocol::DecodedLog`].
//!
//! **Allowed bound (public RPC cannot backfill from deploy).** A single
//! view snapshot at `from - 1` may seed the [`JournalStore`] (hub + spoke
//! market rows, and positions for the in-window universe). **Every**
//! in-window log matching [`AaveV4::subscriptions`] then goes through
//! `apply_log`. That is the detector: misdecoded events, missing paths, and
//! `apply_log` bugs fail closed. The snapshot is not the detector.
//!
//! Intra-block oracle: subscribed logs are folded in
//! `(block, tx_index, log_index)` order. `AnswerUpdated` updates the
//! [`PriceVector`] and `Protocol::health` runs **before** a later
//! same-block `LiquidationCall` is treated as a miss. Evaluating health
//! only at `N-1` or tip for match decisions is forbidden.
//!
//! `pinned_through` is [`Registry::generated_at_block`] — the C2 pin at
//! which protocol rows were committed. `docs/coverage/aave-v4.md` pins
//! bytecode to `40232a0a` and has no separate block in the header; that
//! registry pin is the HaltSignal threshold.
//!
//! W [`WatchDecoder`] remains the liquidation ground truth. Bidirectional
//! match: in-universe unexplained miss → [`LiteError::UnexplainedMiss`],
//! no invented [`crate::recall::MissClass`].
//!
//! **Clears nothing.** The report is a smoke table, not the Recall gate.
//! Engine `DeclineReason` is unmeasured: [`MatchTable::flagged_and_declined`]
//! stays empty (not [`HealthState::Blocked`]).
//!
//! Other honest bounds: empty blocks (no subscribed logs) are not a health
//! tick; universe is Supply/Borrow/Repay in-window only; one spoke; no
//! recall %.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use alloy_primitives::{address, Address, Bytes, I256, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::{BlockNumberOrTag, TransactionInput, TransactionRequest};
use alloy_sol_types::{sol, SolCall, SolEvent};
use liq_adapters_aave_v4::events::spoke;
use liq_adapters_aave_v4::layout::{
    HubAsset, HubRow, Reserve, ReserveCfg, SpokeFlags, SpokeMeta, UserExtra, UserReserve,
    META_ASSET, UNMAPPED_ASSET,
};
use liq_adapters_aave_v4::math::{split, P8_TO_RAY};
use liq_adapters_aave_v4::{AaveV4, AssetConfig, Config, HubConfig, SourcePin, SpokeConfig};
use liq_config::{Intern, OnChainId, ProtocolEntry, Registry};
use liq_protocol::conformance::JournalStore;
use liq_protocol::{
    DecodedLog, DirtySet, HealthState, MarketFlags, MarketRow, PositionExtraRepr, Protocol,
    ProtocolError, StateWriter,
};
use liq_types::{
    AssetId, LogFilter, LogSubscriber, MarketId, PositionId, PositionKey, Price, PriceVector,
    ProtocolId, Ray, SourceKind,
};
use liq_watch::abi::chainlink;
use liq_watch::decode::WatchDecoder;
use liq_watch::source::{LogSource, OwnedBlock, OwnedLog, Poll, RpcPoll};
use liq_watch::types::DecodedLiquidation;
use thiserror::Error;

/// Canonical Multicall3 (same address `liq-config` batches boot asserts with).
const MULTICALL3: Address = address!("0xcA11bde05977b3631167028862bE2a173976CA11");
const BATCH: usize = 32;
/// GUIDE 05 §0: "few thousand blocks". Same magnitude as 03A's page size.
pub const DEFAULT_WINDOW_BLOCKS: u64 = 2_000;
const PAGE_BLOCKS: u64 = 2_000;

sol! {
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct Result {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls) external payable returns (Result[] memory returnData);
    }

    struct SpokeReserve {
        address underlying;
        address hub;
        uint16 assetId;
        uint8 decimals;
        uint24 collateralRisk;
        uint8 flags;
        uint32 dynamicConfigKey;
    }
    struct SpokeReserveConfig {
        uint24 collateralRisk;
        bool paused;
        bool frozen;
        bool borrowable;
        bool receiveSharesEnabled;
    }
    struct SpokeDynConfig {
        uint16 collateralFactor;
        uint32 maxLiquidationBonus;
        uint16 liquidationFee;
    }
    struct SpokeLiqConfig {
        uint128 targetHealthFactor;
        uint64 healthFactorForMaxBonus;
        uint16 liquidationBonusFactor;
    }
    struct SpokeUserPosition {
        uint120 drawnShares;
        uint120 premiumShares;
        int200 premiumOffsetRay;
        uint120 suppliedShares;
        uint32 dynamicConfigKey;
    }
    interface ISpokeViews {
        function ORACLE() external view returns (address);
        function getReserveCount() external view returns (uint256);
        function getReserve(uint256 reserveId) external view returns (SpokeReserve memory);
        function getReserveConfig(uint256 reserveId) external view returns (SpokeReserveConfig memory);
        function getDynamicReserveConfig(uint256 reserveId, uint32 dynamicConfigKey)
            external view returns (SpokeDynConfig memory);
        function getLiquidationConfig() external view returns (SpokeLiqConfig memory);
        function getUserReserveStatus(uint256 reserveId, address user) external view returns (bool, bool);
        function getUserPosition(uint256 reserveId, address user)
            external view returns (SpokeUserPosition memory);
        function getUserLastRiskPremium(address user) external view returns (uint256);
    }

    struct HubAssetView {
        uint120 liquidity;
        uint120 realizedFees;
        uint8 decimals;
        uint120 addedShares;
        uint120 swept;
        int200 premiumOffsetRay;
        uint120 drawnShares;
        uint120 premiumShares;
        uint16 liquidityFee;
        uint120 drawnIndex;
        uint96 drawnRate;
        uint40 lastUpdateTimestamp;
        address underlying;
        address irStrategy;
        address reinvestmentController;
        address feeReceiver;
        uint200 deficitRay;
    }
    struct HubSpokeView {
        uint120 drawnShares;
        uint120 premiumShares;
        int200 premiumOffsetRay;
        uint120 addedShares;
        uint40 addCap;
        uint40 drawCap;
        uint24 riskPremiumThreshold;
        bool active;
        bool halted;
        uint200 deficitRay;
    }
    interface IHubViews {
        function getAssetCount() external view returns (uint256);
        function getAsset(uint256 assetId) external view returns (HubAssetView memory);
        function getSpoke(uint256 assetId, address spoke) external view returns (HubSpokeView memory);
    }

    interface IOracleViews {
        function decimals() external view returns (uint8);
        function getReservePrice(uint256 reserveId) external view returns (uint256);
        function getReserveSource(uint256 reserveId) external view returns (address);
    }
}

/// Fail-closed lite errors. Unset RPC is never a PASS.
#[derive(Debug, Error)]
pub enum LiteError {
    #[error("LIQ_RPC_URL unset")]
    NoRpc,
    #[error("LIQ_LITE_SPOKE unset")]
    NoSpokePin,
    #[error("LIQ_LITE_SPOKE {0:#x} is not an admitted aave-v4 spoke")]
    BadSpoke(Address),
    #[error("window includes genesis; from-1 snapshot is undefined")]
    GenesisWindow,
    #[error("rpc: {0}")]
    Rpc(String),
    #[error("config: {0}")]
    Config(String),
    #[error("watch: {0}")]
    Watch(String),
    #[error("protocol: {0}")]
    Protocol(ProtocolError),
    #[error("no admitted aave-v4 spoke in the registry")]
    NoMarket,
    #[error("LIQ_LITE_WINDOW must be a positive integer")]
    BadWindow,
    #[error("eth_call empty or reverted: {0}")]
    Call(String),
    #[error("in-universe liquidation of {user:#x} at block {block} was never flagged")]
    UnexplainedMiss { user: Address, block: u64 },
}

impl From<ProtocolError> for LiteError {
    fn from(e: ProtocolError) -> Self {
        Self::Protocol(e)
    }
}

/// Env URL only. Empty and unset are the same refusal.
#[must_use = "unset RPC must be surfaced, never dropped"]
pub fn rpc_url() -> Result<String, LiteError> {
    rpc_url_from(std::env::var("LIQ_RPC_URL").ok().as_deref())
}

/// Parse an RPC URL. Tests call this instead of mutating process-global env.
pub fn rpc_url_from(raw: Option<&str>) -> Result<String, LiteError> {
    match raw {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        _ => {
            tracing::error!("LIQ_RPC_URL unset; lite validation refused");
            Err(LiteError::NoRpc)
        }
    }
}

/// Bounded window. `LIQ_LITE_WINDOW` if set must parse as `> 0`; otherwise
/// [`DEFAULT_WINDOW_BLOCKS`].
pub fn window_blocks() -> Result<u64, LiteError> {
    window_blocks_from(std::env::var("LIQ_LITE_WINDOW").ok().as_deref())
}

/// Parse a window. `None` → default. Tests call this instead of mutating env.
pub fn window_blocks_from(raw: Option<&str>) -> Result<u64, LiteError> {
    match raw {
        None => Ok(DEFAULT_WINDOW_BLOCKS),
        Some(s) => {
            let n: u64 = s.parse().map_err(|_| LiteError::BadWindow)?;
            if n == 0 {
                Err(LiteError::BadWindow)
            } else {
                Ok(n)
            }
        }
    }
}

/// Required spoke pin from `LIQ_LITE_SPOKE`.
pub fn spoke_pin() -> Result<Address, LiteError> {
    spoke_pin_from(std::env::var("LIQ_LITE_SPOKE").ok().as_deref())
}

/// Parse the spoke pin. Unset/empty → [`LiteError::NoSpokePin`].
pub fn spoke_pin_from(raw: Option<&str>) -> Result<Address, LiteError> {
    match raw {
        Some(s) if !s.is_empty() => s
            .parse()
            .map_err(|_| LiteError::Config("LIQ_LITE_SPOKE is not an address".into())),
        _ => Err(LiteError::NoSpokePin),
    }
}

/// `(from, to, snap_at)` for `head` and `window`. `snap_at = from - 1`.
pub fn window_bounds(head: u64, window: u64) -> Result<(u64, u64, u64), LiteError> {
    let span = window.saturating_sub(1);
    let from = head.checked_sub(span).ok_or(LiteError::GenesisWindow)?;
    let snap = from.checked_sub(1).ok_or(LiteError::GenesisWindow)?;
    Ok((from, head, snap))
}

fn connect_http(url: &str) -> Result<impl Provider + Clone, LiteError> {
    let parsed = url.parse().map_err(|e| LiteError::Rpc(format!("{e}")))?;
    Ok(ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(parsed))
}

/// One HF < 1 observation from [`Protocol::health`] on the folded store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flag {
    pub user: Address,
    pub block: u64,
    pub hf: Ray,
    pub state: HealthState,
}

/// One W-decoded liquidation on the chosen market.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiqHit {
    pub user: Address,
    pub block: u64,
    pub tx_index: u32,
}

/// Why a liquidation was not a detector miss. Not [`crate::recall::MissClass`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnflaggedCause {
    /// User never appeared in Borrow / Supply / Repay in the window.
    OutsideUniverse,
}

/// GUIDE 05 §0 bidirectional table. No recall percentage.
///
/// `flagged_and_declined` is engine `DeclineReason` (unmeasured here). It is
/// never filled from [`HealthState::Blocked`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MatchTable {
    pub flagged_and_liquidated: Vec<(Flag, LiqHit)>,
    pub liquidated_never_flagged: Vec<(LiqHit, UnflaggedCause)>,
    pub flagged_nobody_liquidated: Vec<Flag>,
    pub flagged_and_declined: Vec<Flag>,
}

/// Smoke report. Intentionally has no rate / gate field.
#[derive(Clone, Debug)]
pub struct LiteReport {
    pub instance: String,
    pub spoke: Address,
    pub from: u64,
    pub to: u64,
    pub universe: usize,
    pub flags: Vec<Flag>,
    pub liquidations: usize,
    pub matches: MatchTable,
}

struct SpokeSel {
    instance: String,
    spoke: Address,
    protocol: ProtocolId,
    market: MarketId,
    hubs: Vec<Address>,
}

/// Run the smoke against `root/registry/registry.json`.
pub async fn run(root: &Path) -> Result<LiteReport, LiteError> {
    let url = rpc_url()?;
    let window = window_blocks()?;
    let spoke = spoke_pin()?;
    let provider = connect_http(&url)?;
    run_on(provider, root, window, spoke).await
}

async fn run_on<P: Provider + Clone>(
    provider: P,
    root: &Path,
    window: u64,
    spoke: Address,
) -> Result<LiteReport, LiteError> {
    let reg = Registry::from_path(&root.join("registry/registry.json"))
        .map_err(|e| LiteError::Config(e.to_string()))?;
    let intern = Intern::from_registry(&reg).map_err(|e| LiteError::Config(e.to_string()))?;
    let sel = select_spoke(&reg, &intern, spoke)?;
    let decoder = WatchDecoder::from_registry(&reg, intern.clone())
        .map_err(|e| LiteError::Watch(e.to_string()))?;

    let head = provider
        .get_block_number()
        .await
        .map_err(|e| LiteError::Rpc(e.to_string()))?;
    let (from, to, snap_at) = window_bounds(head, window)?;

    let oracle = call_addr(
        &provider,
        sel.spoke,
        Bytes::from(ISpokeViews::ORACLECall {}.abi_encode()),
        snap_at,
    )
    .await?;
    if oracle == Address::ZERO {
        return Err(LiteError::Config("spoke ORACLE() is zero".into()));
    }

    let snap_ts = header_ts(&provider, snap_at).await?;
    let (cfg, mut st, mut px) =
        snapshot_config_store(&provider, &intern, &sel, oracle, snap_at, snap_ts, &reg).await?;
    let adapter = AaveV4::new(cfg).map_err(|e| LiteError::Config(e.to_string()))?;

    let mut filters = adapter.subscriptions();
    let ans = chainlink::AnswerUpdated::SIGNATURE_HASH;
    for pin in &adapter.config().price_sources {
        let rec = feed_rec(&intern, pin.source).ok_or_else(|| {
            LiteError::Config(format!(
                "SourcePin {:#x} reserve {} not interned",
                pin.source, pin.reserve_id
            ))
        })?;
        filters.push(LogFilter {
            address: rec.aggregator,
            topic0: ans,
        });
    }

    let blocks = poll_all(provider.clone(), &filters, from, to).await?;
    let (universe, liqs) = universe_and_liqs(&decoder, &blocks)?;

    let mut ids = BTreeMap::new();
    seed_universe_positions(&provider, &sel, &mut st, &universe, snap_at, &mut ids).await?;

    let mut flags = Vec::new();
    let mut seen_flags = BTreeSet::new();
    fold_blocks(
        &mut Fold {
            adapter: &adapter,
            intern: &intern,
            st: &mut st,
            px: &mut px,
            universe: &universe,
            ids: &mut ids,
            flags: &mut flags,
            seen: &mut seen_flags,
        },
        &blocks,
    )?;

    for f in &flags {
        tracing::error!(
            user = %f.user,
            block = f.block,
            hf = %f.hf.raw(),
            "05F HF<1"
        );
    }

    let matches = match_table(&flags, &liqs, &universe)?;
    Ok(LiteReport {
        instance: sel.instance,
        spoke: sel.spoke,
        from,
        to,
        universe: universe.len(),
        flags,
        liquidations: liqs.len(),
        matches,
    })
}

/// Pure match. In-universe unexplained misses are [`LiteError::UnexplainedMiss`].
pub fn match_table(
    flags: &[Flag],
    liqs: &[LiqHit],
    universe: &BTreeSet<Address>,
) -> Result<MatchTable, LiteError> {
    let mut table = MatchTable::default();
    let mut matched_users = BTreeSet::new();
    for liq in liqs {
        if !universe.contains(&liq.user) {
            table
                .liquidated_never_flagged
                .push((liq.clone(), UnflaggedCause::OutsideUniverse));
            continue;
        }
        let Some(flag) = earliest_flag(flags, liq.user).filter(|f| f.block <= liq.block) else {
            return Err(LiteError::UnexplainedMiss {
                user: liq.user,
                block: liq.block,
            });
        };
        matched_users.insert(liq.user);
        table
            .flagged_and_liquidated
            .push((flag.clone(), liq.clone()));
    }
    let mut seen = BTreeSet::new();
    for flag in flags {
        if matched_users.contains(&flag.user) || !seen.insert(flag.user) {
            continue;
        }
        table.flagged_nobody_liquidated.push(flag.clone());
    }
    Ok(table)
}

fn earliest_flag(flags: &[Flag], user: Address) -> Option<&Flag> {
    flags
        .iter()
        .filter(|f| f.user == user)
        .min_by_key(|f| f.block)
}

/// Production pin: same predicate as `apply.rs` `cfg.pinned_source(spoke, r)`.
fn pinned_source(cfg: &Config, spoke: Address, reserve_id: u16) -> Option<Address> {
    cfg.price_sources
        .iter()
        .find(|p| p.spoke == spoke && p.reserve_id == reserve_id)
        .map(|p| p.source)
}

fn feed_rec(intern: &Intern, source: Address) -> Option<&liq_config::FeedRec> {
    if let Some(id) = intern.feed(source) {
        return intern.feeds().get(usize::from(id.0));
    }
    intern.feeds().iter().find(|f| f.aggregator == source)
}

fn select_spoke(reg: &Registry, intern: &Intern, want: Address) -> Result<SpokeSel, LiteError> {
    let protocol = intern.protocol("aave-v4").ok_or(LiteError::NoMarket)?;
    for (key, entry) in &reg.protocols {
        if entry.family != "aave-v4" || !entry.admitted {
            continue;
        }
        if extra_str(entry, "kind") != Some("spoke") {
            continue;
        }
        let OnChainId::Addr(spoke) = entry.market else {
            continue;
        };
        if spoke != want {
            continue;
        }
        let market = intern
            .markets()
            .iter()
            .find(|m| m.protocol == protocol && m.key == entry.market)
            .map(|m| m.id)
            .ok_or(LiteError::NoMarket)?;
        let hubs = extra_hubs(entry)?;
        if hubs.is_empty() {
            return Err(LiteError::Config("spoke has no hub".into()));
        }
        return Ok(SpokeSel {
            instance: key.clone(),
            spoke,
            protocol,
            market,
            hubs,
        });
    }
    Err(LiteError::BadSpoke(want))
}

fn extra_str<'a>(e: &'a ProtocolEntry, k: &str) -> Option<&'a str> {
    e.extra.get(k).and_then(|v| v.as_str())
}

fn extra_hubs(entry: &ProtocolEntry) -> Result<Vec<Address>, LiteError> {
    let mut out = Vec::new();
    if let Some(s) = extra_str(entry, "hub") {
        out.push(
            s.parse()
                .map_err(|_| LiteError::Config("hub not an address".into()))?,
        );
    }
    if let Some(arr) = entry.extra.get("hubs").and_then(|v| v.as_array()) {
        for v in arr {
            let s = v
                .as_str()
                .ok_or_else(|| LiteError::Config("hubs[] not a string".into()))?;
            let a: Address = s
                .parse()
                .map_err(|_| LiteError::Config("hubs[] not an address".into()))?;
            if !out.contains(&a) {
                out.push(a);
            }
        }
    }
    Ok(out)
}

async fn poll_all<P: Provider>(
    provider: P,
    filters: &[LogFilter],
    from: u64,
    to: u64,
) -> Result<Vec<OwnedBlock>, LiteError> {
    let mut poll = RpcPoll::new(provider, filters, from, Some(to), PAGE_BLOCKS);
    let mut buf = OwnedBlock::default();
    let mut out = Vec::new();
    loop {
        match poll
            .fetch_page()
            .await
            .map_err(|e| LiteError::Watch(e.to_string()))?
        {
            Poll::Exhausted => break,
            Poll::Idle => {
                if poll.cursor() > to {
                    break;
                }
            }
            Poll::Ready => {
                while matches!(
                    LogSource::poll_block(&mut poll, &mut buf)
                        .map_err(|e| LiteError::Watch(e.to_string()))?,
                    Poll::Ready
                ) {
                    out.push(core::mem::take(&mut buf));
                }
            }
        }
    }
    Ok(out)
}

fn universe_and_liqs(
    decoder: &WatchDecoder,
    blocks: &[OwnedBlock],
) -> Result<(BTreeSet<Address>, Vec<LiqHit>), LiteError> {
    let mut universe = BTreeSet::new();
    let mut liqs = Vec::new();
    let t_supply = spoke::Supply::SIGNATURE_HASH;
    let t_borrow = spoke::Borrow::SIGNATURE_HASH;
    let t_repay = spoke::Repay::SIGNATURE_HASH;
    for buf in blocks {
        let vol = decoder.coverage_from_oracle_logs(&buf.logs, u32::MAX).1;
        for log in &buf.logs {
            let Some(t0) = log.topics.first().copied() else {
                return Err(LiteError::Watch("malformed log".into()));
            };
            if t0 == t_supply {
                universe.insert(decode_user::<spoke::Supply>(log)?);
            } else if t0 == t_borrow {
                universe.insert(decode_user::<spoke::Borrow>(log)?);
            } else if t0 == t_repay {
                universe.insert(decode_user::<spoke::Repay>(log)?);
            }
            let (trig, _) = decoder.coverage_from_oracle_logs(&buf.logs, log.tx_index);
            match decoder.decode_log(log, trig, vol) {
                Ok(Some(ev)) => liqs.push(liq_hit(&ev)),
                Ok(None) => {}
                Err(e) => return Err(LiteError::Watch(e.to_string())),
            }
        }
    }
    Ok((universe, liqs))
}

fn decode_user<E: SolEvent + UserField>(log: &OwnedLog) -> Result<Address, LiteError> {
    let ev = E::decode_raw_log(log.topics.iter().copied(), &log.data)
        .map_err(|_| LiteError::Watch("abi".into()))?;
    Ok(ev.user())
}

trait UserField {
    fn user(&self) -> Address;
}
impl UserField for spoke::Supply {
    fn user(&self) -> Address {
        self.user
    }
}
impl UserField for spoke::Borrow {
    fn user(&self) -> Address {
        self.user
    }
}
impl UserField for spoke::Repay {
    fn user(&self) -> Address {
        self.user
    }
}

fn liq_hit(ev: &DecodedLiquidation) -> LiqHit {
    LiqHit {
        user: ev.user,
        block: ev.block,
        tx_index: ev.tx_index,
    }
}

fn decoded<'a>(log: &'a OwnedLog) -> DecodedLog<'a> {
    DecodedLog {
        address: log.address,
        topics: log.topics.as_slice(),
        data: log.data.as_slice(),
        block: log.block,
        timestamp: log.timestamp,
    }
}

struct Fold<'a> {
    adapter: &'a AaveV4,
    intern: &'a Intern,
    st: &'a mut JournalStore,
    px: &'a mut PriceVector,
    universe: &'a BTreeSet<Address>,
    ids: &'a mut BTreeMap<Address, PositionId>,
    flags: &'a mut Vec<Flag>,
    seen: &'a mut BTreeSet<(Address, u64)>,
}

fn fold_blocks(f: &mut Fold<'_>, blocks: &[OwnedBlock]) -> Result<(), LiteError> {
    let ans = chainlink::AnswerUpdated::SIGNATURE_HASH;
    for buf in blocks {
        scan_health(f, buf.number, buf.timestamp)?;
        for log in &buf.logs {
            let t0 = log
                .topics
                .first()
                .copied()
                .ok_or_else(|| LiteError::Watch("malformed log".into()))?;
            if t0 == ans {
                apply_answer(f.intern, f.adapter.config(), f.px, log)?;
                scan_health(f, log.block, log.timestamp)?;
                continue;
            }
            if f.adapter
                .config()
                .hubs
                .iter()
                .any(|h| h.address == log.address)
                || f.adapter
                    .config()
                    .spokes
                    .iter()
                    .any(|s| s.address == log.address || s.oracle == log.address)
            {
                let dlog = decoded(log);
                match f.adapter.apply_log(f.st, &dlog) {
                    Ok(dirty) => {
                        note_dirty_ids(f.st, f.universe, f.ids, &dirty)?;
                        if !matches!(dirty, DirtySet::None) {
                            scan_health(f, log.block, log.timestamp)?;
                        }
                    }
                    Err(ProtocolError::HaltSignal) => {
                        tracing::error!(
                            address = %log.address,
                            block = log.block,
                            "05F HaltSignal"
                        );
                        return Err(LiteError::Protocol(ProtocolError::HaltSignal));
                    }
                    Err(e) => return Err(LiteError::Protocol(e)),
                }
            }
        }
    }
    Ok(())
}

fn note_dirty_ids(
    st: &JournalStore,
    universe: &BTreeSet<Address>,
    ids: &mut BTreeMap<Address, PositionId>,
    dirty: &DirtySet,
) -> Result<(), LiteError> {
    let DirtySet::Positions(ps) = dirty else {
        return Ok(());
    };
    for id in ps {
        let key = st.position_key(*id)?;
        if universe.contains(&key.user) {
            ids.insert(key.user, *id);
        }
    }
    Ok(())
}

fn apply_answer(
    intern: &Intern,
    cfg: &Config,
    px: &mut PriceVector,
    log: &OwnedLog,
) -> Result<(), LiteError> {
    let rec = feed_rec(intern, log.address).ok_or_else(|| {
        LiteError::Config(format!(
            "AnswerUpdated aggregator {:#x} not interned",
            log.address
        ))
    })?;
    let ev = chainlink::AnswerUpdated::decode_raw_log(log.topics.iter().copied(), &log.data)
        .map_err(|_| LiteError::Watch("AnswerUpdated".into()))?;
    let ray = answer_to_ray(ev.current, rec.decimals)?;
    for a in &cfg.assets {
        if a.feed != rec.id {
            continue;
        }
        let slot =
            px.0.get_mut(usize::from(a.asset.0))
                .ok_or_else(|| LiteError::Config("price vector short".into()))?;
        if slot.asset != a.asset {
            return Err(LiteError::Config("price vector asset mismatch".into()));
        }
        slot.price = ray;
        slot.block = log.block;
        slot.ts = log.timestamp;
        slot.source = SourceKind::Canonical;
    }
    Ok(())
}

fn answer_to_ray(answer: I256, decimals: u8) -> Result<Ray, LiteError> {
    if !answer.is_positive() {
        return Err(LiteError::Config("non-positive AnswerUpdated".into()));
    }
    let mag = answer.unsigned_abs();
    let Some(exp) = 27u32.checked_sub(u32::from(decimals)) else {
        return Err(LiteError::Config("oracle decimals > 27".into()));
    };
    let factor = pow10(exp)?;
    let raw = mag
        .checked_mul(factor)
        .ok_or_else(|| LiteError::Config("ray scale overflow".into()))?;
    Ok(Ray::from_raw(raw))
}

fn pow10(exp: u32) -> Result<U256, LiteError> {
    let mut v = U256::from(1u8);
    let ten = U256::from(10u8);
    let mut i = 0u32;
    while i < exp {
        v = v
            .checked_mul(ten)
            .ok_or_else(|| LiteError::Config("pow10 overflow".into()))?;
        i = i.saturating_add(1);
    }
    Ok(v)
}

fn scan_health(f: &mut Fold<'_>, block: u64, ts: u64) -> Result<(), LiteError> {
    for user in f.universe {
        let Some(&id) = f.ids.get(user) else {
            continue;
        };
        let pos = f.st.view(id, ts)?;
        match f.adapter.health(pos, f.px) {
            Ok(h) if h.hf < Ray::ONE => {
                if f.seen.insert((*user, block)) {
                    f.flags.push(Flag {
                        user: *user,
                        block,
                        hf: h.hf,
                        state: h.state,
                    });
                }
            }
            Ok(_) => {}
            Err(ProtocolError::MissingPrice(a)) => {
                return Err(LiteError::Protocol(ProtocolError::MissingPrice(a)))
            }
            Err(e) => return Err(LiteError::Protocol(e)),
        }
    }
    Ok(())
}

async fn snapshot_config_store<P: Provider>(
    provider: &P,
    intern: &Intern,
    sel: &SpokeSel,
    oracle: Address,
    block: u64,
    ts: u64,
    reg: &Registry,
) -> Result<(Config, JournalStore, PriceVector), LiteError> {
    let dec = call_u8(
        provider,
        oracle,
        Bytes::from(IOracleViews::decimalsCall {}.abi_encode()),
        block,
    )
    .await?;
    if dec != 8 {
        return Err(LiteError::Config(format!(
            "oracle decimals {dec} != 8; refusing to rescale"
        )));
    }
    let n_raw = call_u256(
        provider,
        sel.spoke,
        Bytes::from(ISpokeViews::getReserveCountCall {}.abi_encode()),
        block,
    )
    .await?;
    let n = usize_of(n_raw)?;
    if n >= usize::from(liq_protocol::AssetMask::MAX_SLOTS) {
        return Err(LiteError::Config("reserve count exceeds AssetMask".into()));
    }
    if n == 0 {
        return Err(LiteError::Config("spoke has no reserves".into()));
    }

    let mut hubs = Vec::new();
    for h in &sel.hubs {
        let market = intern
            .markets()
            .iter()
            .find(|m| m.protocol == sel.protocol && m.key == OnChainId::Addr(*h))
            .map(|m| m.id)
            .ok_or_else(|| LiteError::Config(format!("hub {h:#x} not interned")))?;
        hubs.push(HubConfig {
            address: *h,
            market,
        });
    }

    let mut assets_map: BTreeMap<Address, AssetConfig> = BTreeMap::new();
    let mut pins = Vec::new();
    let mut sources: Vec<Address> = Vec::new();
    let mut prices: Vec<(AssetId, U256)> = Vec::new();

    for rid in 0..n {
        let reserve_id = U256::from(u64_of_usize(rid)?);
        let r = ISpokeViews::getReserveCall::abi_decode_returns(
            &eth_call(
                provider,
                sel.spoke,
                Bytes::from(
                    ISpokeViews::getReserveCall {
                        reserveId: reserve_id,
                    }
                    .abi_encode(),
                ),
                block,
            )
            .await?,
        )
        .map_err(|e| LiteError::Call(e.to_string()))?;
        let src = call_addr(
            provider,
            oracle,
            Bytes::from(
                IOracleViews::getReserveSourceCall {
                    reserveId: reserve_id,
                }
                .abi_encode(),
            ),
            block,
        )
        .await?;
        sources.push(src);
        let rid_u16 = u16::try_from(rid).map_err(|_| LiteError::Call("reserve id".into()))?;
        if let Some(rec) = feed_rec(intern, src) {
            if let Some(asset) = intern.asset(r.underlying) {
                pins.push(SourcePin {
                    spoke: sel.spoke,
                    reserve_id: rid_u16,
                    source: src,
                });
                assets_map.entry(r.underlying).or_insert(AssetConfig {
                    underlying: r.underlying,
                    asset,
                    feed: rec.id,
                });
                let p8 = call_u256(
                    provider,
                    oracle,
                    Bytes::from(
                        IOracleViews::getReservePriceCall {
                            reserveId: reserve_id,
                        }
                        .abi_encode(),
                    ),
                    block,
                )
                .await?;
                if p8.is_zero() {
                    return Err(LiteError::Protocol(ProtocolError::MissingPrice(asset)));
                }
                prices.push((asset, p8));
            }
        }
        if !sel.hubs.contains(&r.hub) {
            return Err(LiteError::Config(format!(
                "reserve {rid} hub {:#x} not in registry spoke hubs",
                r.hub
            )));
        }
    }

    if assets_map.is_empty() {
        return Err(LiteError::Config(
            "no interned assets for spoke reserves".into(),
        ));
    }
    if pins.is_empty() {
        return Err(LiteError::Config(
            "no interned SourcePin for spoke (registry.oracles has no matching source)".into(),
        ));
    }

    let cfg = Config {
        protocol: sel.protocol,
        hubs,
        spokes: vec![SpokeConfig {
            address: sel.spoke,
            market: sel.market,
            oracle,
        }],
        assets: assets_map.into_values().collect(),
        price_sources: pins,
        pinned_through: reg.generated_at_block,
    };

    let mut st = JournalStore::new();
    seed_hubs(provider, intern, sel, &cfg, &mut st, block).await?;
    seed_spoke(provider, intern, sel, &cfg, &sources, &mut st, block).await?;

    let px = initial_px(intern, &prices, block, ts)?;
    Ok((cfg, st, px))
}

fn initial_px(
    intern: &Intern,
    prices: &[(AssetId, U256)],
    block: u64,
    ts: u64,
) -> Result<PriceVector, LiteError> {
    let n = intern.asset_id_capacity();
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let id = AssetId(u16::try_from(i).map_err(|_| LiteError::Config("asset id".into()))?);
        v.push(Price {
            asset: id,
            price: Ray::ZERO,
            source: SourceKind::Canonical,
            block,
            ts,
        });
    }
    for (asset, p8) in prices {
        let raw = p8
            .checked_mul(P8_TO_RAY)
            .ok_or(ProtocolError::Fixed(liq_types::fixed::FixedError::Overflow))?;
        let slot = v
            .get_mut(usize::from(asset.0))
            .ok_or_else(|| LiteError::Config("price vector short".into()))?;
        if slot.asset != *asset {
            return Err(LiteError::Config("price vector asset mismatch".into()));
        }
        slot.price = Ray::from_raw(raw);
    }
    Ok(PriceVector(v))
}

async fn seed_hubs<P: Provider>(
    provider: &P,
    intern: &Intern,
    sel: &SpokeSel,
    cfg: &Config,
    st: &mut JournalStore,
    block: u64,
) -> Result<(), LiteError> {
    for h in &cfg.hubs {
        let n_raw = call_u256(
            provider,
            h.address,
            Bytes::from(IHubViews::getAssetCountCall {}.abi_encode()),
            block,
        )
        .await?;
        let n = usize_of(n_raw)?;
        for i in 0..n {
            let asset_id = U256::from(u64_of_usize(i)?);
            let view = IHubViews::getAssetCall::abi_decode_returns(
                &eth_call(
                    provider,
                    h.address,
                    Bytes::from(IHubViews::getAssetCall { assetId: asset_id }.abi_encode()),
                    block,
                )
                .await?,
            )
            .map_err(|e| LiteError::Call(e.to_string()))?;
            let spoke_view = IHubViews::getSpokeCall::abi_decode_returns(
                &eth_call(
                    provider,
                    h.address,
                    Bytes::from(
                        IHubViews::getSpokeCall {
                            assetId: asset_id,
                            spoke: sel.spoke,
                        }
                        .abi_encode(),
                    ),
                    block,
                )
                .await?,
            )
            .map_err(|e| LiteError::Call(e.to_string()))?;
            let interned = intern.asset(view.underlying);
            let ac = interned.and_then(|id| cfg.assets.iter().find(|a| a.asset == id));
            let asset = ac.map(|a| a.asset).unwrap_or(UNMAPPED_ASSET);
            let mut row = MarketRow::blank(asset, view.decimals);
            if let Some(a) = ac {
                row.price_feed = a.feed;
            }
            if interned.is_none() {
                row.flags = MarketFlags::UNPRICED;
            }
            let last = u32::try_from(view.lastUpdateTimestamp.to::<u64>())
                .map_err(|_| LiteError::Call("lastUpdateTimestamp".into()))?;
            row.last_update = last;
            let hub_asset = hub_asset_of(&view)?;
            let mut flags = SpokeFlags([0u8; SpokeFlags::MAX_SPOKES]);
            let mut bits = 0u8;
            if spoke_view.active {
                bits |= SpokeFlags::ACTIVE;
            }
            if spoke_view.halted {
                bits |= SpokeFlags::HALTED;
            }
            *flags
                .0
                .get_mut(0)
                .ok_or_else(|| LiteError::Config("spoke flags".into()))? = bits;
            *row.body_mut::<HubRow>()? = HubRow {
                asset: hub_asset,
                spokes: flags,
            };
            st.push_market(h.market, row)?;
        }
    }
    Ok(())
}

fn hub_asset_of(view: &HubAssetView) -> Result<HubAsset, LiteError> {
    let (plo, phi) = split(U256::from(view.premiumOffsetRay.into_raw()));
    let (dlo, dhi) = split(U256::from(view.deficitRay));
    Ok(HubAsset {
        drawn_index: u128_of(U256::from(view.drawnIndex))?,
        drawn_rate: u128_of(U256::from(view.drawnRate))?,
        drawn_shares: u128_of(U256::from(view.drawnShares))?,
        premium_shares: u128_of(U256::from(view.premiumShares))?,
        premium_offset_lo: plo,
        premium_offset_hi: phi,
        liquidity: u128_of(U256::from(view.liquidity))?,
        swept: u128_of(U256::from(view.swept))?,
        realized_fees: u128_of(U256::from(view.realizedFees))?,
        added_shares: u128_of(U256::from(view.addedShares))?,
        deficit_ray_lo: dlo,
        deficit_ray_hi: dhi,
        liquidity_fee: view.liquidityFee,
        _pad: [0; 14],
    })
}

async fn seed_spoke<P: Provider>(
    provider: &P,
    intern: &Intern,
    sel: &SpokeSel,
    cfg: &Config,
    sources: &[Address],
    st: &mut JournalStore,
    block: u64,
) -> Result<(), LiteError> {
    let liq = ISpokeViews::getLiquidationConfigCall::abi_decode_returns(
        &eth_call(
            provider,
            sel.spoke,
            Bytes::from(ISpokeViews::getLiquidationConfigCall {}.abi_encode()),
            block,
        )
        .await?,
    )
    .map_err(|e| LiteError::Call(e.to_string()))?;
    let mut priced = 0u128;
    let mut meta = MarketRow::blank(META_ASSET, 0);
    meta.flags = MarketFlags::UNPRICED;
    *meta.body_mut::<SpokeMeta>()? = SpokeMeta {
        target_hf: u128_of(U256::from(liq.targetHealthFactor))?,
        hf_for_max_bonus: liq.healthFactorForMaxBonus,
        bonus_factor: liq.liquidationBonusFactor,
        _pad: [0; 6],
        priced: 0,
    };
    st.push_market(sel.market, meta)?;

    for rid in 0..sources.len() {
        let reserve_id = U256::from(u64_of_usize(rid)?);
        let r = ISpokeViews::getReserveCall::abi_decode_returns(
            &eth_call(
                provider,
                sel.spoke,
                Bytes::from(
                    ISpokeViews::getReserveCall {
                        reserveId: reserve_id,
                    }
                    .abi_encode(),
                ),
                block,
            )
            .await?,
        )
        .map_err(|e| LiteError::Call(e.to_string()))?;
        if !sel.hubs.contains(&r.hub) {
            return Err(LiteError::Config(format!(
                "reserve {rid} hub {:#x} not in registry spoke hubs",
                r.hub
            )));
        }
        let cfg_r = ISpokeViews::getReserveConfigCall::abi_decode_returns(
            &eth_call(
                provider,
                sel.spoke,
                Bytes::from(
                    ISpokeViews::getReserveConfigCall {
                        reserveId: reserve_id,
                    }
                    .abi_encode(),
                ),
                block,
            )
            .await?,
        )
        .map_err(|e| LiteError::Call(e.to_string()))?;
        let dyn_c = ISpokeViews::getDynamicReserveConfigCall::abi_decode_returns(
            &eth_call(
                provider,
                sel.spoke,
                Bytes::from(
                    ISpokeViews::getDynamicReserveConfigCall {
                        reserveId: reserve_id,
                        dynamicConfigKey: r.dynamicConfigKey,
                    }
                    .abi_encode(),
                ),
                block,
            )
            .await?,
        )
        .map_err(|e| LiteError::Call(e.to_string()))?;
        let src = *sources
            .get(rid)
            .ok_or_else(|| LiteError::Config("source len".into()))?;
        let asset_view = IHubViews::getAssetCall::abi_decode_returns(
            &eth_call(
                provider,
                r.hub,
                Bytes::from(
                    IHubViews::getAssetCall {
                        assetId: U256::from(r.assetId),
                    }
                    .abi_encode(),
                ),
                block,
            )
            .await?,
        )
        .map_err(|e| LiteError::Call(e.to_string()))?;
        let spoke_view = IHubViews::getSpokeCall::abi_decode_returns(
            &eth_call(
                provider,
                r.hub,
                Bytes::from(
                    IHubViews::getSpokeCall {
                        assetId: U256::from(r.assetId),
                        spoke: sel.spoke,
                    }
                    .abi_encode(),
                ),
                block,
            )
            .await?,
        )
        .map_err(|e| LiteError::Call(e.to_string()))?;

        let rid_u16 = u16::try_from(rid).map_err(|_| LiteError::Call("reserve id".into()))?;
        let is_priced = pinned_source(cfg, sel.spoke, rid_u16) == Some(src);
        let interned = intern.asset(r.underlying);
        let unmapped = interned.is_none();
        if is_priced && !unmapped {
            let bit = 1u128
                .checked_shl(u32::from(rid_u16))
                .ok_or_else(|| LiteError::Config("priced mask".into()))?;
            priced |= bit;
        }
        let ac = interned.and_then(|id| cfg.assets.iter().find(|a| a.asset == id));
        let asset = ac.map(|a| a.asset).unwrap_or(UNMAPPED_ASSET);
        let mut cfg_flags = 0u8;
        if cfg_r.paused {
            cfg_flags |= ReserveCfg::PAUSED;
        }
        if cfg_r.frozen {
            cfg_flags |= ReserveCfg::FROZEN;
        }
        if cfg_r.borrowable {
            cfg_flags |= ReserveCfg::BORROWABLE;
        }
        if cfg_r.receiveSharesEnabled {
            cfg_flags |= ReserveCfg::RECEIVE_SHARES;
        }
        if spoke_view.active {
            cfg_flags |= ReserveCfg::SPOKE_ACTIVE;
        }
        if spoke_view.halted {
            cfg_flags |= ReserveCfg::SPOKE_HALTED;
        }
        let last = u32::try_from(asset_view.lastUpdateTimestamp.to::<u64>())
            .map_err(|_| LiteError::Call("lastUpdateTimestamp".into()))?;
        let mut row = MarketRow::blank(asset, r.decimals);
        if let Some(a) = ac {
            row.price_feed = a.feed;
        }
        row.hub_slot = r.assetId;
        row.hub_market = intern
            .markets()
            .iter()
            .find(|m| m.key == OnChainId::Addr(r.hub))
            .map(|m| m.id.0)
            .ok_or_else(|| LiteError::Config(format!("hub {:#x} not interned", r.hub)))?;
        row.last_update = last;
        let cfg_body = ReserveCfg {
            collateral_factor: dyn_c.collateralFactor,
            liquidation_fee: dyn_c.liquidationFee,
            max_liquidation_bonus: dyn_c.maxLiquidationBonus,
            dyn_key: r.dynamicConfigKey,
            collateral_risk: r.collateralRisk.to::<u32>(),
            flags: cfg_flags,
            _pad: [0; 15],
        };
        *row.body_mut::<Reserve>()? = Reserve {
            hub: hub_asset_of(&asset_view)?,
            cfg: cfg_body,
        };
        let mut f = MarketFlags::NONE;
        if cfg_r.paused {
            f = MarketFlags(f.0 | MarketFlags::PAUSED.0);
        }
        if cfg_r.frozen {
            f = MarketFlags(f.0 | MarketFlags::FROZEN.0);
        }
        if !is_priced || unmapped {
            f = MarketFlags(f.0 | MarketFlags::UNPRICED.0);
        }
        row.flags = f;
        st.push_market(sel.market, row)?;
    }

    let at = liq_protocol::MarketSlot {
        market: sel.market,
        slot: 0,
    };
    let mut meta_row = *st.market(at)?;
    meta_row.body_mut::<SpokeMeta>()?.priced = priced;
    st.set_market(at, meta_row)?;
    Ok(())
}

async fn seed_universe_positions<P: Provider>(
    provider: &P,
    sel: &SpokeSel,
    st: &mut JournalStore,
    universe: &BTreeSet<Address>,
    block: u64,
    ids: &mut BTreeMap<Address, PositionId>,
) -> Result<(), LiteError> {
    let n = st.markets(sel.market)?.len().saturating_sub(1);
    for user in universe {
        let key = PositionKey {
            protocol: sel.protocol,
            market: sel.market,
            user: *user,
        };
        let pos = st.intern(&key)?;
        ids.insert(*user, pos);
        let mut extra = PositionExtraRepr::ZERO;
        extra.view_mut::<UserExtra>()?.risk_premium = u32_of(
            ISpokeViews::getUserLastRiskPremiumCall::abi_decode_returns(
                &eth_call(
                    provider,
                    sel.spoke,
                    Bytes::from(
                        ISpokeViews::getUserLastRiskPremiumCall { user: *user }.abi_encode(),
                    ),
                    block,
                )
                .await?,
            )
            .map_err(|e| LiteError::Call(e.to_string()))?,
        )?;
        st.set_extra(pos, extra)?;

        let mut calls = Vec::new();
        for rid in 0..n {
            calls.push((
                sel.spoke,
                Bytes::from(
                    ISpokeViews::getUserReserveStatusCall {
                        reserveId: U256::from(u64_of_usize(rid)?),
                        user: *user,
                    }
                    .abi_encode(),
                ),
            ));
            calls.push((
                sel.spoke,
                Bytes::from(
                    ISpokeViews::getUserPositionCall {
                        reserveId: U256::from(u64_of_usize(rid)?),
                        user: *user,
                    }
                    .abi_encode(),
                ),
            ));
        }
        let raws = aggregate3(provider, &calls, block).await?;
        let mut dyn_calls: Vec<(u16, Bytes)> = Vec::new();
        for rid in 0..n {
            let status_i = rid.checked_mul(2).ok_or(LiteError::Call("idx".into()))?;
            let pos_i = status_i
                .checked_add(1)
                .ok_or(LiteError::Call("idx".into()))?;
            let status_raw = raws
                .get(status_i)
                .ok_or_else(|| LiteError::Call("status".into()))?;
            let pos_raw = raws
                .get(pos_i)
                .ok_or_else(|| LiteError::Call("position".into()))?;
            let status = ISpokeViews::getUserReserveStatusCall::abi_decode_returns(status_raw)
                .map_err(|e| LiteError::Call(e.to_string()))?;
            let as_coll = status._0;
            let upos = ISpokeViews::getUserPositionCall::abi_decode_returns(pos_raw)
                .map_err(|e| LiteError::Call(e.to_string()))?;
            let slot = u16::try_from(rid.checked_add(1).ok_or(LiteError::Call("slot".into()))?)
                .map_err(|_| LiteError::Call("slot".into()))?;
            let sup = u128_of(U256::from(upos.suppliedShares))?;
            let drw = u128_of(U256::from(upos.drawnShares))?;
            if sup != 0 {
                st.set_supply(pos, slot, sup)?;
            }
            if drw != 0 {
                st.set_debt(pos, slot, drw)?;
            }
            let (lo, hi) = split(U256::from(upos.premiumOffsetRay.into_raw()));
            let mut cell = PositionExtraRepr::ZERO;
            *cell.view_mut::<UserReserve>()? = UserReserve {
                premium_shares: u128_of(U256::from(upos.premiumShares))?,
                premium_offset_lo: lo,
                premium_offset_hi: hi,
                collateral_factor: 0,
                liquidation_fee: 0,
                max_liquidation_bonus: 0,
                dyn_key: upos.dynamicConfigKey,
                flags: if as_coll {
                    UserReserve::USING_AS_COLLATERAL
                } else {
                    0
                },
                _pad: [0; 3],
            };
            st.set_slot_extra(pos, slot, cell)?;
            if sup != 0 || drw != 0 {
                dyn_calls.push((
                    slot,
                    Bytes::from(
                        ISpokeViews::getDynamicReserveConfigCall {
                            reserveId: U256::from(u64_of_usize(rid)?),
                            dynamicConfigKey: upos.dynamicConfigKey,
                        }
                        .abi_encode(),
                    ),
                ));
            }
        }
        if !dyn_calls.is_empty() {
            let batch: Vec<(Address, Bytes)> = dyn_calls
                .iter()
                .map(|(_, b)| (sel.spoke, b.clone()))
                .collect();
            let dyn_raws = aggregate3(provider, &batch, block).await?;
            for (i, (slot, _)) in dyn_calls.iter().enumerate() {
                let raw = dyn_raws
                    .get(i)
                    .ok_or_else(|| LiteError::Call("dyn cfg".into()))?;
                let d = ISpokeViews::getDynamicReserveConfigCall::abi_decode_returns(raw)
                    .map_err(|e| LiteError::Call(e.to_string()))?;
                let mut cell = *st.slot_extra(pos, *slot)?;
                let u: &mut UserReserve = cell.view_mut()?;
                u.collateral_factor = d.collateralFactor;
                u.liquidation_fee = d.liquidationFee;
                u.max_liquidation_bonus = d.maxLiquidationBonus;
                st.set_slot_extra(pos, *slot, cell)?;
            }
        }
    }
    Ok(())
}

async fn header_ts<P: Provider>(provider: &P, block: u64) -> Result<u64, LiteError> {
    let b = provider
        .get_block_by_number(BlockNumberOrTag::Number(block))
        .await
        .map_err(|e| LiteError::Rpc(e.to_string()))?
        .ok_or_else(|| LiteError::Rpc(format!("missing header {block}")))?;
    Ok(b.header.timestamp)
}

async fn eth_call<P: Provider>(
    provider: &P,
    to: Address,
    data: Bytes,
    block: u64,
) -> Result<Bytes, LiteError> {
    let tx = TransactionRequest {
        to: Some(to.into()),
        input: TransactionInput::new(data),
        ..Default::default()
    };
    let out = provider
        .call(tx)
        .number(block)
        .await
        .map_err(|e| LiteError::Rpc(e.to_string()))?;
    if out.is_empty() {
        return Err(LiteError::Call(format!("{to:#x} empty")));
    }
    Ok(out)
}

async fn call_u256<P: Provider>(
    provider: &P,
    to: Address,
    data: Bytes,
    block: u64,
) -> Result<U256, LiteError> {
    let raw = eth_call(provider, to, data, block).await?;
    if raw.len() < 32 {
        return Err(LiteError::Call("short u256".into()));
    }
    Ok(U256::from_be_slice(
        raw.get(..32).ok_or(LiteError::Call("u256".into()))?,
    ))
}

async fn call_u8<P: Provider>(
    provider: &P,
    to: Address,
    data: Bytes,
    block: u64,
) -> Result<u8, LiteError> {
    u8_of(call_u256(provider, to, data, block).await?)
}

async fn call_addr<P: Provider>(
    provider: &P,
    to: Address,
    data: Bytes,
    block: u64,
) -> Result<Address, LiteError> {
    let raw = eth_call(provider, to, data, block).await?;
    if raw.len() < 32 {
        return Err(LiteError::Call("short address".into()));
    }
    Ok(Address::from_slice(
        raw.get(12..32).ok_or(LiteError::Call("address".into()))?,
    ))
}

async fn aggregate3<P: Provider>(
    provider: &P,
    calls: &[(Address, Bytes)],
    block: u64,
) -> Result<Vec<Bytes>, LiteError> {
    if calls.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(calls.len());
    let mut offset = 0usize;
    while offset < calls.len() {
        let end = offset
            .checked_add(BATCH)
            .unwrap_or(calls.len())
            .min(calls.len());
        let slice = calls
            .get(offset..end)
            .ok_or_else(|| LiteError::Call("batch slice".into()))?;
        let inner: Vec<IMulticall3::Call3> = slice
            .iter()
            .map(|(t, d)| IMulticall3::Call3 {
                target: *t,
                allowFailure: true,
                callData: d.clone(),
            })
            .collect();
        let raw = eth_call(
            provider,
            MULTICALL3,
            Bytes::from(IMulticall3::aggregate3Call { calls: inner }.abi_encode()),
            block,
        )
        .await?;
        let rows = IMulticall3::aggregate3Call::abi_decode_returns(&raw)
            .map_err(|e| LiteError::Call(e.to_string()))?;
        if rows.len() != slice.len() {
            return Err(LiteError::Call("multicall length".into()));
        }
        for (i, row) in rows.iter().enumerate() {
            if !row.success {
                let (t, _) = slice
                    .get(i)
                    .ok_or_else(|| LiteError::Call("multicall idx".into()))?;
                return Err(LiteError::Call(format!("inner revert {t:#x}")));
            }
            out.push(row.returnData.clone());
        }
        offset = end;
    }
    Ok(out)
}

fn u128_of(v: U256) -> Result<u128, LiteError> {
    u128::try_from(v).map_err(|_| LiteError::Call("u128 overflow".into()))
}
fn u32_of(v: U256) -> Result<u32, LiteError> {
    u32::try_from(v).map_err(|_| LiteError::Call("u32 overflow".into()))
}
fn u8_of(v: U256) -> Result<u8, LiteError> {
    u8::try_from(v).map_err(|_| LiteError::Call("u8 overflow".into()))
}
fn usize_of(v: U256) -> Result<usize, LiteError> {
    usize::try_from(v).map_err(|_| LiteError::Call("usize overflow".into()))
}
fn u64_of_usize(v: usize) -> Result<u64, LiteError> {
    u64::try_from(v).map_err(|_| LiteError::Call("u64 overflow".into()))
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
    use liq_protocol::{BlockReason, FeedId, HealthState};

    fn user(b: u8) -> Address {
        Address::repeat_byte(b)
    }

    fn flag(u: Address, block: u64, state: HealthState) -> Flag {
        Flag {
            user: u,
            block,
            hf: Ray::from_raw(U256::from(1u64)),
            state,
        }
    }

    fn liq(u: Address, block: u64) -> LiqHit {
        LiqHit {
            user: u,
            block,
            tx_index: 0,
        }
    }

    fn workspace_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn rpc_url_unset_is_err_never_pass() {
        assert!(matches!(rpc_url_from(None), Err(LiteError::NoRpc)));
        assert!(matches!(rpc_url_from(Some("")), Err(LiteError::NoRpc)));
        assert_eq!(
            rpc_url_from(Some("http://127.0.0.1")).unwrap(),
            "http://127.0.0.1"
        );
    }

    #[test]
    fn window_default_and_bad() {
        assert_eq!(window_blocks_from(None).unwrap(), DEFAULT_WINDOW_BLOCKS);
        assert!(matches!(
            window_blocks_from(Some("0")),
            Err(LiteError::BadWindow)
        ));
        assert!(matches!(
            window_blocks_from(Some("nope")),
            Err(LiteError::BadWindow)
        ));
        assert_eq!(window_blocks_from(Some("12")).unwrap(), 12);
    }

    #[test]
    fn spoke_pin_required() {
        assert!(matches!(spoke_pin_from(None), Err(LiteError::NoSpokePin)));
        assert!(matches!(
            spoke_pin_from(Some("")),
            Err(LiteError::NoSpokePin)
        ));
        assert!(matches!(
            spoke_pin_from(Some("nope")),
            Err(LiteError::Config(_))
        ));
    }

    #[test]
    fn genesis_window_is_not_unexplained_miss() {
        assert!(matches!(
            window_bounds(0, 2_000),
            Err(LiteError::GenesisWindow)
        ));
        assert!(matches!(
            window_bounds(5, 10),
            Err(LiteError::GenesisWindow)
        ));
        let (from, to, snap) = window_bounds(10_000, 2_000).unwrap();
        assert_eq!(to, 10_000);
        assert_eq!(from, 8_001);
        assert_eq!(snap, 8_000);
    }

    #[test]
    fn admitted_spoke_selects_and_unknown_is_err() {
        let root = workspace_root();
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let unknown = Address::repeat_byte(0x42);
        assert!(matches!(
            select_spoke(&reg, &intern, unknown),
            Err(LiteError::BadSpoke(a)) if a == unknown
        ));
        let want: Address = "0x2226749630775ee20230ad65214fb339087ef30d"
            .parse()
            .unwrap();
        let sel = select_spoke(&reg, &intern, want).unwrap();
        assert_eq!(sel.spoke, want);
        assert!(!sel.hubs.is_empty());
    }

    #[test]
    fn interned_feed_is_identity_not_feed_id_zero_fallback() {
        let root = workspace_root();
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        assert!(feed_rec(&intern, Address::repeat_byte(0xab)).is_none());
        let rec = intern.feeds().first().expect("committed oracles");
        assert_eq!(feed_rec(&intern, rec.proxy).map(|f| f.id), Some(rec.id));
        assert_eq!(
            feed_rec(&intern, rec.aggregator).map(|f| f.id),
            Some(rec.id)
        );
    }

    #[test]
    fn pinned_source_matches_apply_predicate() {
        let spoke = Address::repeat_byte(0x22);
        let src = Address::repeat_byte(0xb0);
        let cfg = Config {
            protocol: ProtocolId(0),
            hubs: vec![HubConfig {
                address: Address::repeat_byte(0x11),
                market: MarketId(1),
            }],
            spokes: vec![SpokeConfig {
                address: spoke,
                market: MarketId(2),
                oracle: Address::repeat_byte(0x33),
            }],
            assets: vec![AssetConfig {
                underlying: Address::repeat_byte(0xa0),
                asset: AssetId(1),
                feed: FeedId(3),
            }],
            price_sources: vec![SourcePin {
                spoke,
                reserve_id: 0,
                source: src,
            }],
            pinned_through: 1,
        };
        assert_eq!(pinned_source(&cfg, spoke, 0), Some(src));
        assert_eq!(pinned_source(&cfg, spoke, 1), None);
        assert_ne!(pinned_source(&cfg, spoke, 0), Some(Address::ZERO));
    }

    #[test]
    fn flagged_and_liquidated() {
        let u = user(0x11);
        let flags = vec![flag(u, 10, HealthState::Liquidatable)];
        let liqs = vec![liq(u, 12)];
        let mut uni = BTreeSet::new();
        uni.insert(u);
        let t = match_table(&flags, &liqs, &uni).unwrap();
        assert_eq!(t.flagged_and_liquidated.len(), 1);
        assert!(t.liquidated_never_flagged.is_empty());
        assert!(t.flagged_nobody_liquidated.is_empty());
        assert!(t.flagged_and_declined.is_empty());
    }

    #[test]
    fn outside_universe_is_explained_not_a_miss_class() {
        let u = user(0x22);
        let flags = vec![];
        let liqs = vec![liq(u, 5)];
        let uni = BTreeSet::new();
        let t = match_table(&flags, &liqs, &uni).unwrap();
        assert_eq!(t.liquidated_never_flagged.len(), 1);
        assert_eq!(
            t.liquidated_never_flagged[0].1,
            UnflaggedCause::OutsideUniverse
        );
    }

    #[test]
    fn in_universe_never_flagged_fails_closed() {
        let u = user(0x33);
        let mut uni = BTreeSet::new();
        uni.insert(u);
        let err = match_table(&[], &[liq(u, 9)], &uni).unwrap_err();
        assert!(matches!(
            err,
            LiteError::UnexplainedMiss { user, block: 9 } if user == u
        ));
    }

    #[test]
    fn flag_after_liq_is_not_detection() {
        let u = user(0x44);
        let flags = vec![flag(u, 20, HealthState::Liquidatable)];
        let mut uni = BTreeSet::new();
        uni.insert(u);
        let err = match_table(&flags, &[liq(u, 10)], &uni).unwrap_err();
        assert!(matches!(err, LiteError::UnexplainedMiss { .. }));
    }

    #[test]
    fn blocked_is_not_flagged_and_declined() {
        let a = user(0x55);
        let b = user(0x66);
        let flags = vec![
            flag(a, 1, HealthState::Liquidatable),
            flag(
                b,
                1,
                HealthState::Blocked {
                    reason: BlockReason::Paused,
                },
            ),
        ];
        let mut uni = BTreeSet::new();
        uni.insert(a);
        uni.insert(b);
        let t = match_table(&flags, &[], &uni).unwrap();
        assert_eq!(t.flagged_nobody_liquidated.len(), 2);
        assert!(t.flagged_and_declined.is_empty());
    }

    #[test]
    fn report_is_a_table_not_a_rate() {
        let t = MatchTable::default();
        assert!(t.flagged_and_liquidated.is_empty());
        assert!(t.liquidated_never_flagged.is_empty());
        assert!(t.flagged_nobody_liquidated.is_empty());
        assert!(t.flagged_and_declined.is_empty());
    }

    /// Live path. Unset RPC → this test is ignored in default CI.
    /// `cargo test -p liq-replay --lib lite -- --ignored` with `LIQ_RPC_URL`
    /// and `LIQ_LITE_SPOKE` set runs the real smoke; without it the ignore
    /// harness does not fake PASS.
    #[ignore]
    #[tokio::test(flavor = "current_thread")]
    async fn live_smoke_requires_rpc() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let report = run(&root)
            .await
            .expect("LIQ_RPC_URL and LIQ_LITE_SPOKE set and RPC reachable");
        assert!(report.to >= report.from);
        let _ = report.matches;
    }
}
