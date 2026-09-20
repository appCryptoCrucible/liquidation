//! WP 05F — GUIDE 05 Step 0 lite validation smoke.
//!
//! Bounded universe over a public RPC (`LIQ_RPC_URL` only; no default
//! endpoint). Polls via [`liq_watch::source::RpcPoll`] — the same
//! `eth_getLogs` fold as 03A's [`liq_node::source::RpcPoll`] (explicitly
//! named for 05F). W's copy is used because [`WatchDecoder::decode_log`]
//! needs `block_hash` / `tx_hash`, which 03A's [`liq_node::source::OwnedLog`]
//! does not carry. No third poller.
//!
//! Detector: Aave V4 [`Protocol::health`] on position state read from the
//! spoke/hub/oracle views at the evaluation block (chain, not a guess) plus
//! [`WatchDecoder`] for actual liquidations. Bidirectional match. Every
//! in-universe "liquidated, never flagged" is [`LiteError::UnexplainedMiss`]
//! — this module does not invent a [`crate::recall::MissClass`].
//!
//! **Clears nothing.** The report is a smoke table, not the Recall gate.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::{BlockNumberOrTag, TransactionInput, TransactionRequest};
use alloy_sol_types::{sol, SolCall, SolEvent};
use liq_adapters_aave_v4::events::spoke;
use liq_adapters_aave_v4::layout::{
    HubAsset, Reserve, ReserveCfg, SpokeMeta, UserExtra, UserReserve, META_ASSET, UNMAPPED_ASSET,
};
use liq_adapters_aave_v4::math::{split, P8_TO_RAY};
use liq_adapters_aave_v4::{AaveV4, Config, HubConfig, SpokeConfig};
use liq_config::{Intern, OnChainId, ProtocolEntry, Registry};
use liq_protocol::{
    AssetMask, BlockReason, FeedId, Health, HealthState, MarketFlags, MarketRow, PositionExtraRepr,
    Protocol, ProtocolError,
};
use liq_types::{
    AssetId, LogFilter, MarketId, PositionId, PositionKey, Price, PriceVector, ProtocolId, Ray,
    SourceKind,
};
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
    match std::env::var("LIQ_RPC_URL") {
        Ok(s) if !s.is_empty() => Ok(s),
        _ => {
            tracing::error!("LIQ_RPC_URL unset; lite validation refused");
            Err(LiteError::NoRpc)
        }
    }
}

/// Bounded window. `LIQ_LITE_WINDOW` if set must parse as `> 0`; otherwise
/// [`DEFAULT_WINDOW_BLOCKS`].
pub fn window_blocks() -> Result<u64, LiteError> {
    match std::env::var("LIQ_LITE_WINDOW") {
        Err(_) => Ok(DEFAULT_WINDOW_BLOCKS),
        Ok(s) => {
            let n: u64 = s.parse().map_err(|_| LiteError::BadWindow)?;
            if n == 0 {
                Err(LiteError::BadWindow)
            } else {
                Ok(n)
            }
        }
    }
}

fn connect_http(url: &str) -> Result<impl Provider + Clone, LiteError> {
    let parsed = url.parse().map_err(|e| LiteError::Rpc(format!("{e}")))?;
    Ok(ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(parsed))
}

/// One HF < 1 observation from [`Protocol::health`].
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MatchTable {
    pub flagged_and_liquidated: Vec<(Flag, LiqHit)>,
    pub liquidated_never_flagged: Vec<(LiqHit, UnflaggedCause)>,
    pub flagged_nobody_liquidated: Vec<Flag>,
    pub flagged_and_declined: Vec<(Flag, BlockReason)>,
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

struct MarketSnap {
    rows: Vec<MarketRow>,
    px: PriceVector,
    timestamp: u64,
}

struct HealthCtx<'a, P> {
    provider: &'a P,
    adapter: &'a AaveV4,
    intern: &'a Intern,
    sel: &'a SpokeSel,
    oracle: Address,
}

/// Run the smoke against `root/registry/registry.json`.
pub async fn run(root: &Path) -> Result<LiteReport, LiteError> {
    let url = rpc_url()?;
    let window = window_blocks()?;
    let provider = connect_http(&url)?;
    run_on(provider, root, window).await
}

async fn run_on<P: Provider + Clone>(
    provider: P,
    root: &Path,
    window: u64,
) -> Result<LiteReport, LiteError> {
    let reg = Registry::from_path(&root.join("registry/registry.json"))
        .map_err(|e| LiteError::Config(e.to_string()))?;
    let intern = Intern::from_registry(&reg).map_err(|e| LiteError::Config(e.to_string()))?;
    let sel = select_spoke(&reg, &intern)?;
    let decoder = WatchDecoder::from_registry(&reg, intern.clone())
        .map_err(|e| LiteError::Watch(e.to_string()))?;

    let head = provider
        .get_block_number()
        .await
        .map_err(|e| LiteError::Rpc(e.to_string()))?;
    let span = window.saturating_sub(1);
    let from = head.saturating_sub(span);
    let to = head;

    let filters = universe_filters(sel.spoke);
    let mut poll = RpcPoll::new(provider.clone(), &filters, from, Some(to), PAGE_BLOCKS);
    let mut buf = OwnedBlock::default();
    let mut universe = BTreeSet::new();
    let mut liqs = Vec::new();
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
                    absorb_block(&decoder, &buf, &mut universe, &mut liqs)?;
                }
            }
        }
    }

    let oracle = call_addr(
        &provider,
        sel.spoke,
        Bytes::from(ISpokeViews::ORACLECall {}.abi_encode()),
        to,
    )
    .await?;
    if oracle == Address::ZERO {
        return Err(LiteError::Config("spoke ORACLE() is zero".into()));
    }
    let adapter = adapter_for(&sel, &intern, oracle)?;
    let ctx = HealthCtx {
        provider: &provider,
        adapter: &adapter,
        intern: &intern,
        sel: &sel,
        oracle,
    };

    let mut flags = Vec::new();
    for liq in &liqs {
        if !universe.contains(&liq.user) {
            continue;
        }
        let n1 = liq.block.checked_sub(1).ok_or(LiteError::UnexplainedMiss {
            user: liq.user,
            block: liq.block,
        })?;
        push_health(&ctx, liq.user, n1, &mut flags).await?;
    }
    for user in &universe {
        push_health(&ctx, *user, to, &mut flags).await?;
    }

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
        match flag.state {
            HealthState::Blocked { reason } => {
                table.flagged_and_declined.push((flag.clone(), reason));
            }
            _ => table.flagged_nobody_liquidated.push(flag.clone()),
        }
    }
    Ok(table)
}

fn earliest_flag(flags: &[Flag], user: Address) -> Option<&Flag> {
    flags
        .iter()
        .filter(|f| f.user == user)
        .min_by_key(|f| f.block)
}

fn universe_filters(spoke: Address) -> Vec<LogFilter> {
    [
        spoke::Supply::SIGNATURE_HASH,
        spoke::Borrow::SIGNATURE_HASH,
        spoke::Repay::SIGNATURE_HASH,
        spoke::LiquidationCall::SIGNATURE_HASH,
    ]
    .into_iter()
    .map(|topic0| LogFilter {
        address: spoke,
        topic0,
    })
    .collect()
}

fn absorb_block(
    decoder: &WatchDecoder,
    buf: &OwnedBlock,
    universe: &mut BTreeSet<Address>,
    liqs: &mut Vec<LiqHit>,
) -> Result<(), LiteError> {
    let t_supply = spoke::Supply::SIGNATURE_HASH;
    let t_borrow = spoke::Borrow::SIGNATURE_HASH;
    let t_repay = spoke::Repay::SIGNATURE_HASH;
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
    Ok(())
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

fn select_spoke(reg: &Registry, intern: &Intern) -> Result<SpokeSel, LiteError> {
    let protocol = intern.protocol("aave-v4").ok_or(LiteError::NoMarket)?;
    let pin = std::env::var("LIQ_LITE_SPOKE").ok();
    let pin_addr = match pin.as_deref() {
        None => None,
        Some(s) => Some(
            s.parse::<Address>()
                .map_err(|_| LiteError::Config("LIQ_LITE_SPOKE is not an address".into()))?,
        ),
    };
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
        if let Some(want) = pin_addr {
            if spoke != want {
                continue;
            }
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
    Err(LiteError::NoMarket)
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

fn adapter_for(sel: &SpokeSel, intern: &Intern, oracle: Address) -> Result<AaveV4, LiteError> {
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
    AaveV4::new(Config {
        protocol: sel.protocol,
        hubs,
        spokes: vec![SpokeConfig {
            address: sel.spoke,
            market: sel.market,
            oracle,
        }],
        assets: Vec::new(),
        price_sources: Vec::new(),
        pinned_through: 0,
    })
    .map_err(|e| LiteError::Config(e.to_string()))
}

async fn push_health<P: Provider>(
    ctx: &HealthCtx<'_, P>,
    user: Address,
    block: u64,
    flags: &mut Vec<Flag>,
) -> Result<(), LiteError> {
    let snap = hydrate_market(ctx.provider, ctx.intern, ctx.sel, ctx.oracle, block).await?;
    let health = health_at(ctx.provider, ctx.adapter, ctx.sel, &snap, user, block).await?;
    if health.hf < Ray::ONE {
        flags.push(Flag {
            user,
            block,
            hf: health.hf,
            state: health.state,
        });
    }
    Ok(())
}

async fn hydrate_market<P: Provider>(
    provider: &P,
    intern: &Intern,
    sel: &SpokeSel,
    oracle: Address,
    block: u64,
) -> Result<MarketSnap, LiteError> {
    let ts = header_ts(provider, block).await?;
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
    if n >= usize::from(AssetMask::MAX_SLOTS) {
        return Err(LiteError::Config("reserve count exceeds AssetMask".into()));
    }
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

    let mut meta = MarketRow::blank(META_ASSET, 0);
    let mut priced = 0u128;
    *meta.body_mut::<SpokeMeta>()? = SpokeMeta {
        target_hf: u128_of(U256::from(liq.targetHealthFactor))?,
        hf_for_max_bonus: liq.healthFactorForMaxBonus,
        bonus_factor: liq.liquidationBonusFactor,
        _pad: [0; 6],
        priced: 0,
    };
    let mut rows = vec![meta];
    let mut prices: BTreeMap<u16, (AssetId, U256)> = BTreeMap::new();

    for rid in 0..n {
        let reserve_id = U256::from(rid);
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
        if !sel.hubs.contains(&r.hub) {
            return Err(LiteError::Config(format!(
                "reserve {rid} hub {:#x} not in registry spoke hubs",
                r.hub
            )));
        }
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

        let interned = intern.asset(r.underlying);
        let is_priced = src != Address::ZERO && interned.is_some() && !p8.is_zero();
        if is_priced {
            let bit = 1u128
                .checked_shl(u32_from_usize(rid)?)
                .ok_or_else(|| LiteError::Config("priced mask".into()))?;
            priced |= bit;
        }
        let asset = interned.unwrap_or(UNMAPPED_ASSET);
        if is_priced {
            prices.insert(asset.0, (asset, p8));
        }
        let (plo, phi) = split(U256::from(asset_view.premiumOffsetRay.into_raw()));
        let (dlo, dhi) = split(U256::from(asset_view.deficitRay));
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
        row.price_feed = intern.feed(src).unwrap_or(FeedId(0));
        row.hub_slot = r.assetId;
        row.hub_market = intern
            .markets()
            .iter()
            .find(|m| m.key == OnChainId::Addr(r.hub))
            .map(|m| m.id.0)
            .unwrap_or(MarketRow::NO_HUB);
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
            hub: HubAsset {
                drawn_index: u128_of(U256::from(asset_view.drawnIndex))?,
                drawn_rate: u128_of(U256::from(asset_view.drawnRate))?,
                drawn_shares: u128_of(U256::from(asset_view.drawnShares))?,
                premium_shares: u128_of(U256::from(asset_view.premiumShares))?,
                premium_offset_lo: plo,
                premium_offset_hi: phi,
                liquidity: u128_of(U256::from(asset_view.liquidity))?,
                swept: u128_of(U256::from(asset_view.swept))?,
                realized_fees: u128_of(U256::from(asset_view.realizedFees))?,
                added_shares: u128_of(U256::from(asset_view.addedShares))?,
                deficit_ray_lo: dlo,
                deficit_ray_hi: dhi,
                liquidity_fee: asset_view.liquidityFee,
                _pad: [0; 14],
            },
            cfg: cfg_body,
        };
        row.flags = {
            let mut f = MarketFlags::NONE;
            if cfg_r.paused {
                f = MarketFlags(f.0 | MarketFlags::PAUSED.0);
            }
            if cfg_r.frozen {
                f = MarketFlags(f.0 | MarketFlags::FROZEN.0);
            }
            if !is_priced {
                f = MarketFlags(f.0 | MarketFlags::UNPRICED.0);
            }
            f
        };
        rows.push(row);
    }
    rows.get_mut(0)
        .ok_or_else(|| LiteError::Config("meta row".into()))?
        .body_mut::<SpokeMeta>()?
        .priced = priced;

    let px = price_vector(&prices, block, ts)?;
    Ok(MarketSnap {
        rows,
        px,
        timestamp: ts,
    })
}

async fn health_at<P: Provider>(
    provider: &P,
    adapter: &AaveV4,
    sel: &SpokeSel,
    snap: &MarketSnap,
    user: Address,
    block: u64,
) -> Result<Health, LiteError> {
    let n = snap.rows.len().saturating_sub(1);
    let mut calls = Vec::new();
    for rid in 0..n {
        calls.push((
            sel.spoke,
            Bytes::from(
                ISpokeViews::getUserReserveStatusCall {
                    reserveId: U256::from(rid),
                    user,
                }
                .abi_encode(),
            ),
        ));
        calls.push((
            sel.spoke,
            Bytes::from(
                ISpokeViews::getUserPositionCall {
                    reserveId: U256::from(rid),
                    user,
                }
                .abi_encode(),
            ),
        ));
    }
    calls.push((
        sel.spoke,
        Bytes::from(ISpokeViews::getUserLastRiskPremiumCall { user }.abi_encode()),
    ));
    let raws = aggregate3(provider, &calls, block).await?;
    let rp_raw = raws
        .last()
        .ok_or_else(|| LiteError::Call("risk premium".into()))?;
    let rp = ISpokeViews::getUserLastRiskPremiumCall::abi_decode_returns(rp_raw)
        .map_err(|e| LiteError::Call(e.to_string()))?;
    let rp_u32 = u32_of(rp)?;

    let mut config = AssetMask::EMPTY;
    let mut supply = vec![0u128; snap.rows.len()];
    let mut debt = vec![0u128; snap.rows.len()];
    let mut slot_extra = vec![PositionExtraRepr::ZERO; snap.rows.len()];
    let mut dyn_calls: Vec<(u16, u32, Bytes)> = Vec::new();

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
        let pos = ISpokeViews::getUserPositionCall::abi_decode_returns(pos_raw)
            .map_err(|e| LiteError::Call(e.to_string()))?;
        let slot = u16::try_from(rid.checked_add(1).ok_or(LiteError::Call("slot".into()))?)
            .map_err(|_| LiteError::Call("slot".into()))?;
        let sup = u128_of(U256::from(pos.suppliedShares))?;
        let drw = u128_of(U256::from(pos.drawnShares))?;
        if sup == 0 && drw == 0 {
            continue;
        }
        config = config
            .with(slot)
            .ok_or_else(|| LiteError::Config("slot mask".into()))?;
        *supply
            .get_mut(usize::from(slot))
            .ok_or_else(|| LiteError::Call("supply slot".into()))? = sup;
        *debt
            .get_mut(usize::from(slot))
            .ok_or_else(|| LiteError::Call("debt slot".into()))? = drw;
        let (lo, hi) = split(U256::from(pos.premiumOffsetRay.into_raw()));
        let cell = slot_extra
            .get_mut(usize::from(slot))
            .ok_or_else(|| LiteError::Call("extra slot".into()))?;
        *cell.view_mut::<UserReserve>()? = UserReserve {
            premium_shares: u128_of(U256::from(pos.premiumShares))?,
            premium_offset_lo: lo,
            premium_offset_hi: hi,
            collateral_factor: 0,
            liquidation_fee: 0,
            max_liquidation_bonus: 0,
            dyn_key: pos.dynamicConfigKey,
            flags: if as_coll {
                UserReserve::USING_AS_COLLATERAL
            } else {
                0
            },
            _pad: [0; 3],
        };
        dyn_calls.push((
            slot,
            pos.dynamicConfigKey,
            Bytes::from(
                ISpokeViews::getDynamicReserveConfigCall {
                    reserveId: U256::from(rid),
                    dynamicConfigKey: pos.dynamicConfigKey,
                }
                .abi_encode(),
            ),
        ));
    }

    if !dyn_calls.is_empty() {
        let batch: Vec<(Address, Bytes)> = dyn_calls
            .iter()
            .map(|(_, _, b)| (sel.spoke, b.clone()))
            .collect();
        let dyn_raws = aggregate3(provider, &batch, block).await?;
        for (i, (slot, _, _)) in dyn_calls.iter().enumerate() {
            let raw = dyn_raws
                .get(i)
                .ok_or_else(|| LiteError::Call("dyn cfg".into()))?;
            let d = ISpokeViews::getDynamicReserveConfigCall::abi_decode_returns(raw)
                .map_err(|e| LiteError::Call(e.to_string()))?;
            let cell = slot_extra
                .get_mut(usize::from(*slot))
                .ok_or_else(|| LiteError::Call("dyn slot".into()))?;
            let u: &mut UserReserve = cell.view_mut()?;
            u.collateral_factor = d.collateralFactor;
            u.liquidation_fee = d.liquidationFee;
            u.max_liquidation_bonus = d.maxLiquidationBonus;
        }
    }

    let mut extra = PositionExtraRepr::ZERO;
    extra.view_mut::<UserExtra>()?.risk_premium = rp_u32;
    let key = PositionKey {
        protocol: sel.protocol,
        market: sel.market,
        user,
    };
    let pos = liq_protocol::PositionRef {
        id: PositionId(0),
        key: &key,
        config,
        supply: &supply,
        debt: &debt,
        extra: &extra,
        slot_extra: &slot_extra,
        markets: &snap.rows,
        timestamp: snap.timestamp,
    };
    Ok(adapter.health(pos, &snap.px)?)
}

fn price_vector(
    prices: &BTreeMap<u16, (AssetId, U256)>,
    block: u64,
    ts: u64,
) -> Result<PriceVector, LiteError> {
    let max = prices.keys().copied().max().unwrap_or(0);
    let len = usize::from(max)
        .checked_add(1)
        .ok_or_else(|| LiteError::Config("price vector".into()))?;
    let mut v = Vec::with_capacity(len);
    for i in 0..len {
        let id = AssetId(u16::try_from(i).map_err(|_| LiteError::Config("asset id".into()))?);
        let (asset, p8) = prices.get(&id.0).copied().unwrap_or((id, U256::ZERO));
        let raw = p8
            .checked_mul(P8_TO_RAY)
            .ok_or(ProtocolError::Fixed(liq_types::fixed::FixedError::Overflow))?;
        v.push(Price {
            asset,
            price: Ray::from_raw(raw),
            source: SourceKind::Canonical,
            block,
            ts,
        });
    }
    Ok(PriceVector(v))
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
fn u32_from_usize(v: usize) -> Result<u32, LiteError> {
    u32::try_from(v).map_err(|_| LiteError::Call("u32 overflow".into()))
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
    use liq_protocol::HealthState;

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

    #[test]
    fn rpc_url_unset_is_err_never_pass() {
        let prev = std::env::var("LIQ_RPC_URL").ok();
        std::env::remove_var("LIQ_RPC_URL");
        let err = rpc_url().unwrap_err();
        assert!(matches!(err, LiteError::NoRpc));
        match prev {
            Some(v) => std::env::set_var("LIQ_RPC_URL", v),
            None => std::env::remove_var("LIQ_RPC_URL"),
        }
    }

    #[test]
    fn empty_rpc_url_is_err() {
        let prev = std::env::var("LIQ_RPC_URL").ok();
        std::env::set_var("LIQ_RPC_URL", "");
        assert!(matches!(rpc_url(), Err(LiteError::NoRpc)));
        match prev {
            Some(v) => std::env::set_var("LIQ_RPC_URL", v),
            None => std::env::remove_var("LIQ_RPC_URL"),
        }
    }

    #[test]
    fn window_default_and_bad() {
        let prev = std::env::var("LIQ_LITE_WINDOW").ok();
        std::env::remove_var("LIQ_LITE_WINDOW");
        assert_eq!(window_blocks().unwrap(), DEFAULT_WINDOW_BLOCKS);
        std::env::set_var("LIQ_LITE_WINDOW", "0");
        assert!(matches!(window_blocks(), Err(LiteError::BadWindow)));
        std::env::set_var("LIQ_LITE_WINDOW", "nope");
        assert!(matches!(window_blocks(), Err(LiteError::BadWindow)));
        match prev {
            Some(v) => std::env::set_var("LIQ_LITE_WINDOW", v),
            None => std::env::remove_var("LIQ_LITE_WINDOW"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_without_rpc_never_fake_pass() {
        let prev = std::env::var("LIQ_RPC_URL").ok();
        std::env::remove_var("LIQ_RPC_URL");
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let err = run(&root).await.unwrap_err();
        assert!(matches!(err, LiteError::NoRpc), "{err}");
        match prev {
            Some(v) => std::env::set_var("LIQ_RPC_URL", v),
            None => std::env::remove_var("LIQ_RPC_URL"),
        }
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
    fn flagged_nobody_and_declined() {
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
        assert_eq!(t.flagged_nobody_liquidated.len(), 1);
        assert_eq!(t.flagged_nobody_liquidated[0].user, a);
        assert_eq!(t.flagged_and_declined.len(), 1);
        assert_eq!(t.flagged_and_declined[0].1, BlockReason::Paused);
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
    /// set runs the real smoke; without it the ignore harness does not fake PASS.
    #[ignore]
    #[tokio::test(flavor = "current_thread")]
    async fn live_smoke_requires_rpc() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let report = run(&root).await.expect("LIQ_RPC_URL set and RPC reachable");
        assert!(report.to >= report.from);
        let _ = report.matches;
    }
}
