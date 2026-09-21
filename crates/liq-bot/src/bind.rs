//! WP 17E: bind observed config into the 17D drain.
//!
//! Adapters come from committed TOML + each crate's `Config` / `new`.
//! `LiveRegistryUnasserted` / `EmptyInterned` / `LiveFeesUnasserted` (and
//! Fluid `LiveFactoryUnasserted`) omit that crate — no invented intern.
//! Header gas, wrap gas, WETH, and fee windows are observed or absent.

use std::fs;
use std::path::Path;

use alloy_primitives::{address, Address, U256};
use liq_config::{AaveV3Toml, Intern};
use liq_engine::Candidate;
use liq_exec::fee::FeeQuote;
use liq_flash::Haircut;
use liq_node::LogHandler;
use liq_plan::ValidateCtx;
use liq_protocol::{
    DecodedLog, DirtySet, ExecutorAdapter, FeedId, PositionExtraRepr, Protocol, ProtocolError,
    StateWriter,
};
use liq_router::{GasOracle, TailPins};
use liq_types::{AssetId, FlashProvider, LogFilter, LogSubscriber, MarketId, ProtocolId};

use crate::assemble_view::ProcessAssembleView;

/// Canonical mainnet WETH9 — lookup key into the committed registry, not a
/// fabricated token.
pub const REGISTRY_WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

/// A constructed adapter that passed `new`. Omitted crates are not stored.
pub enum BoundProtocol {
    AaveV3(liq_adapters_aave_v3::AaveV3),
    EulerV2(liq_adapters_euler_v2::EulerV2),
    SiloV2(liq_adapters_silo_v2::SiloV2),
}

impl BoundProtocol {
    #[must_use]
    pub fn as_dyn(&self) -> &dyn Protocol {
        match self {
            Self::AaveV3(p) => p,
            Self::EulerV2(p) => p,
            Self::SiloV2(p) => p,
        }
    }

    #[must_use]
    pub fn id(&self) -> ProtocolId {
        self.as_dyn().id()
    }

    /// Token addresses from adapter config (registry intern first, then these).
    pub fn token_addrs(&self) -> Vec<Address> {
        match self {
            Self::AaveV3(p) => p.config().assets.iter().map(|a| a.underlying).collect(),
            Self::EulerV2(p) => p.config().assets.iter().map(|a| a.underlying).collect(),
            Self::SiloV2(p) => p.config().assets.iter().map(|a| a.underlying).collect(),
        }
    }

    /// TailPins from adapter market/borrower fields already on the position.
    /// Quote-derived fields stay unset for [`ProcessAssembleView::apply_quote_derived`].
    /// Missing required pin → `None` (no zero tail).
    #[must_use]
    pub fn pins_from_candidate(
        &self,
        c: &Candidate,
        extra: Option<&PositionExtraRepr>,
    ) -> Option<TailPins> {
        let _ = extra;
        match self {
            Self::AaveV3(p) => pins_aave_v3(p.config(), c),
            Self::EulerV2(p) => pins_euler(p.config(), c),
            Self::SiloV2(p) => pins_silo(p.config(), c),
        }
    }
}

impl LogSubscriber for BoundProtocol {
    fn subscriptions(&self) -> Vec<LogFilter> {
        self.as_dyn().subscriptions()
    }
}

/// Same leaked adapter the router indexed. Calls [`Protocol::apply_log`]
/// (store + dirty already in apply).
pub struct AdapterHandler(pub &'static BoundProtocol);

impl LogSubscriber for AdapterHandler {
    fn subscriptions(&self) -> Vec<LogFilter> {
        self.0.subscriptions()
    }
}

impl LogHandler for AdapterHandler {
    fn apply_log(
        &self,
        st: &mut dyn StateWriter,
        log: &DecodedLog<'_>,
    ) -> core::result::Result<DirtySet, ProtocolError> {
        Protocol::apply_log(self.0.as_dyn(), st, log)
    }
}

/// Leak the load once. Ingest and drain share this slice (same intern).
#[must_use]
pub fn leak_protocols(load: ProtocolLoad) -> &'static [BoundProtocol] {
    if load.protocols.is_empty() {
        tracing::error!(
            "empty protocol list after TOML load — ingest subscribers empty; protocol ids empty"
        );
    }
    Box::leak(load.protocols.into_boxed_slice())
}

/// `HotSpawn.protocols` / `register_exex` ids. Empty load → empty box.
#[must_use]
pub fn protocol_ids(protocols: &[BoundProtocol]) -> Box<[ProtocolId]> {
    protocols.iter().map(BoundProtocol::id).collect()
}

/// Router subscribers: the leaked adapters themselves, not a second copy.
#[must_use]
pub fn subscriber_refs(protocols: &[BoundProtocol]) -> Vec<&dyn LogSubscriber> {
    protocols.iter().map(|p| p as &dyn LogSubscriber).collect()
}

/// Handlers in subscriber order. Matching index calls that adapter's apply.
#[must_use]
pub fn ingest_handlers(protocols: &'static [BoundProtocol]) -> Vec<Box<dyn LogHandler + Send>> {
    protocols
        .iter()
        .map(|p| Box::new(AdapterHandler(p)) as Box<dyn LogHandler + Send>)
        .collect()
}

/// Result of walking `config/protocols/*.toml`.
#[derive(Default)]
pub struct ProtocolLoad {
    pub protocols: Vec<BoundProtocol>,
    pub omitted: Vec<(&'static str, String)>,
}

/// Load each committed protocol file. Missing file or refused `new` → omit, log.
#[must_use]
pub fn load_protocols(config_dir: &Path, intern: &Intern) -> ProtocolLoad {
    let mut out = ProtocolLoad::default();
    let proto_dir = config_dir.join("protocols");
    for name in [
        "aave-v3.toml",
        "aave-v4.toml",
        "morpho-blue.toml",
    ] {
        let path = proto_dir.join(name);
        if !path.is_file() {
            tracing::error!(file = name, "protocol toml absent — adapter omitted (no invented config)");
            out.omitted.push((name, "toml absent".into()));
        }
    }
    push_spark(&proto_dir, intern, &mut out);
    push_euler(&proto_dir, intern, &mut out);
    push_silo(&proto_dir, &mut out);
    push_liquity(&proto_dir, &mut out);
    push_fluid(&proto_dir, &mut out);
    push_gearbox(&proto_dir, &mut out);
    push_compound(&proto_dir, intern, &mut out);
    if out.protocols.is_empty() {
        tracing::error!("empty protocol list after TOML load — drain stays a no-op");
    }
    out
}

fn omit(out: &mut ProtocolLoad, name: &'static str, why: impl std::fmt::Display) {
    let why = why.to_string();
    tracing::error!(adapter = name, reason = %why, "adapter omitted — no invented intern/registry/fees");
    out.omitted.push((name, why));
}

fn read_toml(dir: &Path, name: &str) -> Option<String> {
    let path = dir.join(name);
    match fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!(file = name, error = %e, "protocol toml unreadable — adapter omitted");
            None
        }
    }
}

fn push_spark(dir: &Path, intern: &Intern, out: &mut ProtocolLoad) {
    let path = dir.join("spark.toml");
    let toml = match AaveV3Toml::from_path(&path) {
        Ok(t) => t,
        Err(e) => {
            omit(out, "spark", e);
            return;
        }
    };
    if let Some(want) = intern.protocol("spark") {
        if want != ProtocolId(toml.protocol) {
            omit(
                out,
                "spark",
                format!("toml protocol {} != intern {want:?}", toml.protocol),
            );
            return;
        }
    }
    let cfg = spark_to_config(&toml);
    match liq_adapters_aave_v3::AaveV3::new(cfg) {
        Ok(p) => out.protocols.push(BoundProtocol::AaveV3(p)),
        Err(e) => omit(out, "spark", e),
    }
}

fn spark_to_config(t: &AaveV3Toml) -> liq_adapters_aave_v3::Config {
    liq_adapters_aave_v3::Config {
        protocol: ProtocolId(t.protocol),
        pools: t
            .pools
            .iter()
            .map(|p| liq_adapters_aave_v3::PoolConfig {
                address: p.address,
                market: MarketId(p.market),
                oracle: p.oracle,
                provider: p.provider,
                configurator: p.configurator,
                sentinel: p.sentinel,
                sequencer_oracle: p.sequencer_oracle,
            })
            .collect(),
        assets: t
            .assets
            .iter()
            .map(|a| liq_adapters_aave_v3::AssetConfig {
                underlying: a.underlying,
                asset: AssetId(a.asset),
                feed: FeedId(a.feed),
                siloed: a.siloed,
                isolated: a.isolated,
                debt_ceiling: a.debt_ceiling,
                decimals: a.decimals,
            })
            .collect(),
        price_sources: t
            .price_sources
            .iter()
            .map(|s| liq_adapters_aave_v3::SourcePin {
                pool: s.pool,
                underlying: s.underlying,
                source: s.source,
            })
            .collect(),
        liquidation: liq_adapters_aave_v3::LiquidationParams {
            close_factor_bps: t.liquidation.close_factor_bps,
            close_factor_hf_wad: t.liquidation.close_factor_hf_wad,
            min_base_max_close: t.liquidation.min_base_max_close,
            oracle_decimals: t.liquidation.oracle_decimals,
        },
        pinned_through: t.pinned_through,
    }
}

fn push_euler(dir: &Path, intern: &Intern, out: &mut ProtocolLoad) {
    let Some(raw) = read_toml(dir, "euler-v2.toml") else {
        omit(out, "euler-v2", "toml absent");
        return;
    };
    let mut cfg = match liq_adapters_euler_v2::Config::from_toml(&raw) {
        Ok(c) => c,
        Err(e) => {
            omit(out, "euler-v2", e);
            return;
        }
    };
    if let Err(e) = cfg.bind_from_intern(intern) {
        omit(out, "euler-v2", e);
        return;
    }
    match liq_adapters_euler_v2::EulerV2::new(cfg) {
        Ok(p) => out.protocols.push(BoundProtocol::EulerV2(p)),
        Err(e) => omit(out, "euler-v2", e),
    }
}

fn push_silo(dir: &Path, out: &mut ProtocolLoad) {
    let Some(raw) = read_toml(dir, "silo-v2.toml") else {
        omit(out, "silo-v2", "toml absent");
        return;
    };
    let cfg = match silo_from_toml(&raw) {
        Ok(c) => c,
        Err(e) => {
            omit(out, "silo-v2", e);
            return;
        }
    };
    match liq_adapters_silo_v2::SiloV2::new(cfg) {
        Ok(p) => out.protocols.push(BoundProtocol::SiloV2(p)),
        Err(e) => omit(out, "silo-v2", e),
    }
}

fn silo_from_toml(raw: &str) -> Result<liq_adapters_silo_v2::Config, String> {
    let f: SiloToml = toml::from_str(raw).map_err(|e| e.to_string())?;
    let mut factories = Vec::with_capacity(f.factories.len());
    for a in f.factories {
        factories.push(parse_addr(&a)?);
    }
    let mut pairs = Vec::with_capacity(f.pairs.len());
    for p in f.pairs {
        pairs.push(liq_adapters_silo_v2::PairConfig {
            silo_config: parse_addr(&p.silo_config)?,
            hook_receiver: parse_addr(&p.hook_receiver)?,
            market: MarketId(p.market),
            silo0: silo_side(p.silo0)?,
            silo1: silo_side(p.silo1)?,
        });
    }
    let mut assets = Vec::with_capacity(f.assets.len());
    for a in f.assets {
        assets.push(liq_adapters_silo_v2::AssetConfig {
            underlying: parse_addr(&a.underlying)?,
            asset: AssetId(a.asset),
            feed: FeedId(a.feed),
            decimals: a.decimals,
        });
    }
    Ok(liq_adapters_silo_v2::Config {
        protocol: ProtocolId(f.protocol),
        factories,
        pairs,
        assets,
        pinned_through: f.pinned_through,
    })
}

fn silo_side(s: SiloSideToml) -> Result<liq_adapters_silo_v2::SideConfig, String> {
    Ok(liq_adapters_silo_v2::SideConfig {
        silo: parse_addr(&s.silo)?,
        token: parse_addr(&s.token)?,
        protected_share: parse_addr(&s.protected_share)?,
        debt_share: parse_addr(&s.debt_share)?,
        solvency_oracle: parse_addr(&s.solvency_oracle)?,
        lt: s.lt,
        liquidation_fee: s.liquidation_fee,
        liquidation_target_ltv: s.liquidation_target_ltv,
    })
}

fn parse_addr(s: &str) -> Result<Address, String> {
    s.parse().map_err(|e| format!("address {s}: {e}"))
}

#[derive(serde::Deserialize)]
struct SiloToml {
    protocol: u16,
    pinned_through: u64,
    factories: Vec<String>,
    pairs: Vec<SiloPairToml>,
    assets: Vec<SiloAssetToml>,
}

#[derive(serde::Deserialize)]
struct SiloPairToml {
    silo_config: String,
    hook_receiver: String,
    market: u32,
    silo0: SiloSideToml,
    silo1: SiloSideToml,
}

#[derive(serde::Deserialize)]
struct SiloSideToml {
    silo: String,
    token: String,
    protected_share: String,
    debt_share: String,
    solvency_oracle: String,
    lt: u128,
    liquidation_fee: u128,
    liquidation_target_ltv: u128,
}

#[derive(serde::Deserialize)]
struct SiloAssetToml {
    underlying: String,
    asset: u16,
    feed: u16,
    decimals: u8,
}

fn push_liquity(dir: &Path, out: &mut ProtocolLoad) {
    let Some(raw) = read_toml(dir, "liquity-v2.toml") else {
        omit(out, "liquity-v2", "toml absent");
        return;
    };
    let cfg = match liq_adapters_liquity_v2::Config::from_toml(&raw) {
        Ok(c) => c,
        Err(e) => {
            omit(out, "liquity-v2", e);
            return;
        }
    };
    match liq_adapters_liquity_v2::LiquityV2::new(cfg) {
        Ok(_) => tracing::error!("liquity-v2 constructed without live registry — refuse to keep"),
        Err(e) => omit(out, "liquity-v2", e),
    }
}

fn push_fluid(dir: &Path, out: &mut ProtocolLoad) {
    let Some(raw) = read_toml(dir, "fluid.toml") else {
        omit(out, "fluid", "toml absent");
        return;
    };
    let cfg = match liq_adapters_fluid::Config::from_toml(&raw) {
        Ok(c) => c,
        Err(e) => {
            omit(out, "fluid", e);
            return;
        }
    };
    match liq_adapters_fluid::Fluid::new(cfg) {
        Ok(_) => tracing::error!("fluid constructed without live factory — refuse to keep"),
        Err(e) => omit(out, "fluid", e),
    }
}

fn push_gearbox(dir: &Path, out: &mut ProtocolLoad) {
    let Some(raw) = read_toml(dir, "gearbox.toml") else {
        omit(out, "gearbox", "toml absent");
        return;
    };
    let cfg = match liq_adapters_gearbox::Config::from_toml(&raw) {
        Ok(c) => c,
        Err(e) => {
            omit(out, "gearbox", e);
            return;
        }
    };
    match liq_adapters_gearbox::GearboxV3::new(cfg) {
        Ok(_) => tracing::error!("gearbox constructed without live fees — refuse to keep"),
        Err(e) => omit(out, "gearbox", e),
    }
}

fn push_compound(dir: &Path, intern: &Intern, out: &mut ProtocolLoad) {
    let Some(raw) = read_toml(dir, "compound-v2.toml") else {
        omit(out, "compound-v2", "toml absent");
        return;
    };
    let mut cfg = match liq_adapters_compound_v2::Config::from_toml(&raw) {
        Ok(c) => c,
        Err(e) => {
            omit(out, "compound-v2", e);
            return;
        }
    };
    if let Err(e) = cfg.bind_from_intern(intern) {
        omit(out, "compound-v2", e);
        return;
    }
    match liq_adapters_compound_v2::CompoundV2::new(cfg) {
        Ok(_) => tracing::error!("compound-v2 constructed without live registry — refuse to keep"),
        Err(e) => omit(out, "compound-v2", e),
    }
}

/// Intern registry tokens in intern order. Drift → empty view (no shifted ids).
#[must_use]
pub fn intern_view(intern: &Intern) -> ProcessAssembleView {
    let mut v = ProcessAssembleView::empty();
    for rec in intern.assets() {
        if rec.address.is_zero() {
            tracing::error!(asset = rec.id.0, "registry token is zero — intern discarded");
            return ProcessAssembleView::empty();
        }
        match v.intern_token(rec.address) {
            Ok(id) if id == rec.id => {}
            Ok(id) => {
                tracing::error!(
                    expected = rec.id.0,
                    got = id.0,
                    "intern id mismatch — view discarded"
                );
                return ProcessAssembleView::empty();
            }
            Err(e) => {
                tracing::error!(error = %e, "registry intern refused — view discarded");
                return ProcessAssembleView::empty();
            }
        }
    }
    v
}

/// Intern adapter-config underlyings not already in the registry table.
pub fn intern_adapter_tokens(view: &mut ProcessAssembleView, protocols: &[BoundProtocol]) {
    for p in protocols {
        for addr in p.token_addrs() {
            if addr.is_zero() {
                tracing::error!("adapter token is zero — skipped (no invented intern)");
                continue;
            }
            if let Err(e) = view.intern_token(addr) {
                tracing::error!(error = %e, token = %addr, "adapter token intern refused");
            }
        }
    }
}

/// WETH address from the committed intern. Zero / missing → None.
#[must_use]
pub fn registry_weth(intern: &Intern) -> Option<Address> {
    let id = intern.asset(REGISTRY_WETH)?;
    let rec = intern.asset_rec(id)?;
    if rec.address.is_zero() {
        tracing::error!("registry WETH is the zero address — refused");
        return None;
    }
    Some(rec.address)
}

/// 10C `gas_overhead` mapped onto [`FlashProvider`] discriminants.
/// Missing key → `0` (that provider unusable). Never invents 30M.
#[must_use]
pub fn load_wrap_gas(flash_gas: &Path) -> [u64; 5] {
    let mut wrap = [0u64; 5];
    let raw = match fs::read_to_string(flash_gas) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "flash-gas.toml unreadable — wrap_gas stays 0");
            return wrap;
        }
    };
    let parsed: Result<FlashGasToml, _> = toml::from_str(&raw);
    let file = match parsed {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, "flash-gas.toml malformed — wrap_gas stays 0");
            return wrap;
        }
    };
    set_wrap(&mut wrap, FlashProvider::Aave, file.gas_overhead.aave_v3);
    set_wrap(&mut wrap, FlashProvider::UniV3, file.gas_overhead.univ3);
    set_wrap(&mut wrap, FlashProvider::UniV4, file.gas_overhead.univ4);
    set_wrap(&mut wrap, FlashProvider::Morpho, file.gas_overhead.morpho);
    set_wrap(&mut wrap, FlashProvider::SkyDss, file.gas_overhead.sky_dss);
    wrap
}

fn set_wrap(wrap: &mut [u64; 5], p: FlashProvider, v: Option<u64>) {
    let Some(gas) = v else {
        tracing::error!(provider = ?p, "10C snapshot missing — provider unusable (no guessed gas)");
        return;
    };
    if gas == 0 {
        tracing::error!(provider = ?p, "10C snapshot is zero — provider unusable");
        return;
    }
    if let Some(slot) = wrap.get_mut(p as usize) {
        *slot = gas;
    }
}

#[derive(serde::Deserialize)]
struct FlashGasToml {
    gas_overhead: GasOverheadToml,
}

#[derive(serde::Deserialize)]
struct GasOverheadToml {
    aave_v3: Option<u64>,
    univ3: Option<u64>,
    univ4: Option<u64>,
    morpho: Option<u64>,
    sky_dss: Option<u64>,
}

/// Observed parts that can become [`crate::drain::SelectReady`] when header
/// gas is on the committed block. `gas_failed` stays 0: 10C has wrap
/// snapshots only, no failed-path measurement.
#[derive(Clone, Debug)]
pub struct SelectBind {
    pub wrap_gas: [u64; 5],
    pub weth: Address,
    pub validate: ValidateCtx,
    pub haircut: Haircut,
    pub gas_failed: u64,
}

/// Build [`SelectBind`] from 10C wrap + registry WETH. Zero WETH → None.
#[must_use]
pub fn select_bind(wrap_gas: [u64; 5], weth: Address) -> Option<SelectBind> {
    if weth.is_zero() {
        tracing::error!("WETH is zero — SelectReady stays None");
        return None;
    }
    Some(SelectBind {
        wrap_gas,
        weth,
        validate: ValidateCtx {
            weth,
            v4_underlying: Vec::new(),
            morpho: Vec::new(),
            compound: Vec::new(),
            liquity: Vec::new(),
        },
        haircut: Haircut::NONE,
        gas_failed: 0,
    })
}

/// Fee only from a real 12A-2 window. Empty window / missing base → None.
#[must_use]
pub fn fee_from_oracle(oracle: &GasOracle, parent_block: u64) -> Option<FeeQuote> {
    let next_base_fee = match oracle.base_fee_wei() {
        Some(b) if b != 0 => b,
        Some(_) => {
            tracing::error!("gas oracle base fee is zero — fee stays None");
            return None;
        }
        None => {
            tracing::error!("gas oracle has no base fee — fee stays None");
            return None;
        }
    };
    let priority = match oracle.priority_percentile(50) {
        Ok(p) if p != 0 => u128::from(p),
        Ok(_) => {
            tracing::error!("priority percentile is zero — fee stays None");
            return None;
        }
        Err(e) => {
            tracing::error!(error = %e, "priority window empty — fee stays None");
            return None;
        }
    };
    let modest = match oracle.priority_percentile(1) {
        Ok(p) if p != 0 => u128::from(p),
        Ok(_) => {
            tracing::error!("modest priority is zero — fee stays None");
            return None;
        }
        Err(e) => {
            tracing::error!(error = %e, "modest priority window empty — fee stays None");
            return None;
        }
    };
    Some(FeeQuote {
        parent_block,
        next_base_fee,
        priority_wei: priority,
        modest_priority_wei: modest,
    })
}

fn pins_aave_v3(cfg: &liq_adapters_aave_v3::Config, c: &Candidate) -> Option<TailPins> {
    let pool = cfg.pools.iter().find(|p| p.market == c.quote.key.market)?;
    if pool.address.is_zero() || c.quote.key.user.is_zero() {
        tracing::error!("aave market/borrower zero — skip (no zero tail)");
        return None;
    }
    Some(base_pins(
        ExecutorAdapter::AaveV3,
        pool.address,
        c.quote.key.user,
    ))
}

fn pins_euler(cfg: &liq_adapters_euler_v2::Config, c: &Candidate) -> Option<TailPins> {
    let vault = cfg
        .interned
        .iter()
        .find(|(_, id)| *id == c.quote.key.market)
        .map(|(a, _)| *a)?;
    if vault.is_zero() || c.quote.key.user.is_zero() {
        tracing::error!("euler vault/borrower zero — skip (no zero tail)");
        return None;
    }
    Some(base_pins(ExecutorAdapter::EulerV2, vault, c.quote.key.user))
}

fn pins_silo(cfg: &liq_adapters_silo_v2::Config, c: &Candidate) -> Option<TailPins> {
    let pair = cfg.pairs.iter().find(|p| p.market == c.quote.key.market)?;
    if pair.hook_receiver.is_zero() || c.quote.key.user.is_zero() {
        tracing::error!("silo hook/borrower zero — skip (no zero tail)");
        return None;
    }
    Some(base_pins(
        ExecutorAdapter::SiloV2,
        pair.hook_receiver,
        c.quote.key.user,
    ))
}

fn base_pins(adapter: ExecutorAdapter, market: Address, borrower: Address) -> TailPins {
    TailPins {
        adapter,
        market,
        borrower,
        protocol_pull: None,
        euler_min_yield: None,
        liquity_trove_id: None,
        fluid_t1: None,
        fluid_col_per_unit_debt: None,
        gearbox_min_seized: None,
        gearbox_full_multicall: false,
        compound_ctoken_collateral: None,
        compound_is_cether: None,
    }
}

/// Compound pin: missing cToken / `is_cether` → None (no zero tail).
#[must_use]
pub fn pins_compound_required(
    market: Address,
    borrower: Address,
    ctoken_collateral: Option<Address>,
    is_cether: Option<bool>,
) -> Option<TailPins> {
    let ctoken = ctoken_collateral.filter(|a| !a.is_zero())?;
    let is_cether = is_cether?;
    if market.is_zero() || borrower.is_zero() {
        return None;
    }
    let mut p = base_pins(ExecutorAdapter::CompoundV2, market, borrower);
    p.compound_ctoken_collateral = Some(ctoken);
    p.compound_is_cether = Some(is_cether);
    Some(p)
}

/// Liquity pin: missing / zero troveId → None.
#[must_use]
pub fn pins_liquity_required(
    market: Address,
    borrower: Address,
    trove_id: Option<U256>,
) -> Option<TailPins> {
    let trove_id = trove_id.filter(|t| !t.is_zero())?;
    if market.is_zero() || borrower.is_zero() {
        return None;
    }
    let mut p = base_pins(ExecutorAdapter::LiquityV2, market, borrower);
    p.liquity_trove_id = Some(trove_id);
    Some(p)
}

/// Fluid pin: missing `fluid_t1` → None. T2–T4 stay unpinned (`Some(false)`
/// is not invented here).
#[must_use]
pub fn pins_fluid_required(
    market: Address,
    borrower: Address,
    fluid_t1: Option<bool>,
) -> Option<TailPins> {
    let fluid_t1 = fluid_t1?;
    if !fluid_t1 {
        tracing::error!("fluid T2–T4 unwired — skip (no invented MultiCall/T2)");
        return None;
    }
    if market.is_zero() || borrower.is_zero() {
        return None;
    }
    let mut p = base_pins(ExecutorAdapter::Fluid, market, borrower);
    p.fluid_t1 = Some(true);
    Some(p)
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
    use liq_config::{Intern, Registry};
    use liq_router::{AssembleView, GasOracle};

    fn root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    #[test]
    fn adapter_toml_load_constructs_and_omits_unasserted() {
        let intern =
            Intern::from_registry(&Registry::from_path(&root().join("registry/registry.json")).unwrap())
                .unwrap();
        let load = load_protocols(&root().join("config"), &intern);
        assert!(
            !load.protocols.is_empty(),
            "at least one real TOML adapter must construct: omitted={:?}",
            load.omitted
        );
        assert!(
            load.omitted.iter().any(|(n, w)| {
                *n == "liquity-v2" && w.contains("not asserted")
            }),
            "LiveRegistryUnasserted crate must be omitted: {:?}",
            load.omitted
        );
        assert!(
            load.omitted.iter().any(|(n, w)| {
                *n == "gearbox" && w.contains("fees() was not asserted")
            }),
            "LiveFeesUnasserted crate must be omitted: {:?}",
            load.omitted
        );
        assert!(load.omitted.iter().any(|(n, _)| *n == "compound-v2"));
        assert!(
            load.protocols
                .iter()
                .any(|p| matches!(p, BoundProtocol::AaveV3(_) | BoundProtocol::EulerV2(_) | BoundProtocol::SiloV2(_)))
        );
    }

    #[test]
    fn missing_tail_pin_field_skips_no_zero_tail() {
        let m = address!("0x0000000000000000000000000000000000000051");
        let b = address!("0x00000000000000000000000000000000000000b1");
        assert!(
            pins_compound_required(m, b, None, Some(false)).is_none(),
            "missing cToken must not become a zero tail"
        );
        assert!(
            pins_compound_required(m, b, Some(Address::ZERO), Some(true)).is_none()
        );
        assert!(pins_liquity_required(m, b, None).is_none());
        assert!(pins_liquity_required(m, b, Some(U256::ZERO)).is_none());
        assert!(pins_fluid_required(m, b, None).is_none());
        assert!(
            pins_fluid_required(m, b, Some(false)).is_none(),
            "T2–T4 must not assemble"
        );
        assert!(pins_fluid_required(m, b, Some(true)).is_some());
    }

    #[test]
    fn empty_fee_window_is_none() {
        let o = GasOracle::with_priority_cap(4).unwrap();
        assert!(fee_from_oracle(&o, 0).is_none());
    }

    #[test]
    fn wrap_gas_matches_10c_toml() {
        let w = load_wrap_gas(&root().join("config/flash-gas.toml"));
        assert_eq!(w[FlashProvider::Aave as usize], 366_332);
        assert_eq!(w[FlashProvider::UniV3 as usize], 355_632);
        assert_eq!(w[FlashProvider::UniV4 as usize], 460_032);
        assert_eq!(w[FlashProvider::Morpho as usize], 370_435);
        assert_eq!(w[FlashProvider::SkyDss as usize], 384_134);
        assert!(!w.contains(&30_000_000));
    }

    #[test]
    fn intern_view_matches_registry_weth() {
        let intern =
            Intern::from_registry(&Registry::from_path(&root().join("registry/registry.json")).unwrap())
                .unwrap();
        let view = intern_view(&intern);
        let weth = registry_weth(&intern).unwrap();
        let id = intern.asset(weth).unwrap();
        assert_eq!(view.token(id), Some(weth));
    }

    #[test]
    fn zero_weth_select_bind_none() {
        assert!(select_bind([366_332, 355_632, 460_032, 370_435, 384_134], Address::ZERO).is_none());
    }

    #[test]
    fn loaded_adapters_are_nonempty_subscribers_and_ids_match() {
        let intern =
            Intern::from_registry(&Registry::from_path(&root().join("registry/registry.json")).unwrap())
                .unwrap();
        let load = load_protocols(&root().join("config"), &intern);
        assert!(
            load.protocols.iter().any(|p| matches!(p, BoundProtocol::AaveV3(_))),
            "spark must construct: omitted={:?}",
            load.omitted
        );
        assert!(
            load.protocols.iter().any(|p| matches!(p, BoundProtocol::EulerV2(_))),
            "euler must construct: omitted={:?}",
            load.omitted
        );
        assert!(
            load.protocols.iter().any(|p| matches!(p, BoundProtocol::SiloV2(_))),
            "silo must construct: omitted={:?}",
            load.omitted
        );
        let leaked = leak_protocols(load);
        let subs = subscriber_refs(leaked);
        assert!(!subs.is_empty(), "from_subscribers must be nonempty");
        assert!(
            leaked.iter().all(|p| !p.subscriptions().is_empty()),
            "bound adapters must advertise filters"
        );
        liq_node::LogRouter::from_subscribers(&subs).unwrap();
        let ids = protocol_ids(leaked);
        assert_eq!(ids.len(), leaked.len());
        for (id, p) in ids.iter().zip(leaked.iter()) {
            assert_eq!(*id, p.id());
        }
        let handlers = ingest_handlers(leaked);
        assert_eq!(handlers.len(), leaked.len());
    }

    #[test]
    fn empty_load_subscribers_and_ids_empty() {
        let intern =
            Intern::from_registry(&Registry::from_path(&root().join("registry/registry.json")).unwrap())
                .unwrap();
        let missing = root().join("config/protocols/.17f-empty-omitted");
        let load = load_protocols(&missing, &intern);
        assert!(
            load.protocols.is_empty(),
            "omitted dir must not invent adapters: {:?}",
            load.protocols.iter().map(BoundProtocol::id).collect::<Vec<_>>()
        );
        let leaked = leak_protocols(load);
        assert!(leaked.is_empty());
        let subs = subscriber_refs(leaked);
        assert!(subs.is_empty());
        liq_node::LogRouter::from_subscribers(&subs).unwrap();
        let ids = protocol_ids(leaked);
        assert!(ids.is_empty());
        assert!(ingest_handlers(leaked).is_empty());
    }

    #[test]
    fn production_bind_source_has_no_invented_bid_or_30m() {
        let src = include_str!("bind.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        assert!(!prod.contains("BidConfig::new"));
        assert!(!prod.contains("30_000_000") && !prod.contains("30000000"));
        assert!(!prod.contains("gas_failed: 50_000"));
    }
}
