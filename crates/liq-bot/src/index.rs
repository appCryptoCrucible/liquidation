//! WP 17G: flash / PoolBook / oracle `LogSubscriber`s on the hot router.
//!
//! Addresses from the committed registry and `config/` only. Missing → omit,
//! log. Depths, ticks, sqrt_price, and answers stay zero until logs fold.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use alloy_primitives::{Address, U256};
use liq_config::{AaveV3Toml, Intern, OnChainId, PoolVenue, ProtocolEntry, Registry};
use liq_flash::{
    AavePool, FlashSource, HeldAsset, MorphoBlue, SkyDssFlash, UniV3Pool, UniV4PoolManager,
};
use liq_node::LogHandler;
use liq_oracle::{CanonicalBook, DerivedBook, FeedSet, FeedsConfig};
use liq_protocol::{DecodedLog, DirtySet, ProtocolError};
use liq_router::{
    CryptoKind, CryptoState, CurveState, Pool, PoolBook, PoolState, V2State, V3State,
};
use liq_types::{FlashProvider, HaltSink, LogFilter, LogSubscriber};
use parking_lot::{Mutex, RwLock};
use smallvec::SmallVec;

/// Canonical DAI — lookup key into the committed intern, not a fabricated token.
const REGISTRY_DAI: Address =
    alloy_primitives::address!("0x6B175474E89094C44Da98b954EedeAC495271d0F");

/// The V3 factories' `feeAmountTickSpacing`: Uniswap and SushiSwap enable
/// 100, 500, 3000 and 10000; PancakeSwap 100, 500, 2500 and 10000 (read
/// from each factory on chain, 2026-10-08). Fee is committed on each
/// `PoolEntry`; unknown fees are omitted (no guessed spacing).
fn univ3_tick_spacing(fee: u32) -> Option<i32> {
    match fee {
        100 => Some(1),
        500 => Some(10),
        2500 => Some(50),
        3000 => Some(60),
        10_000 => Some(200),
        _ => None,
    }
}

/// Uniswap V3 factory.
const UNIV3_FACTORY: Address =
    alloy_primitives::address!("0x1F98431c8aD98523631AE4a59f267346ea31F984");
/// SushiSwap V3 factory: Uniswap's code, its own pools (`getPool` and the
/// CREATE2 derivation from Uniswap's init hash agree on 11 of 11 live pools).
const SUSHI_V3_FACTORY: Address =
    alloy_primitives::address!("0xbACEB8eC6b9355Dfc0269C18bac9d6E2Bdc29C4F");
/// PancakeSwap V3 factory. Its pools are deployed by the `PoolDeployer`
/// the Executor derives them from.
const PANCAKE_V3_FACTORY: Address =
    alloy_primitives::address!("0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865");

/// Executor factory id for a V3 pool's committed factory; `None` = a factory
/// the Executor cannot verify (the pool is omitted, as an unknown V2 fork
/// is).
fn v3_factory_id(factory: Address) -> Option<u8> {
    if factory == UNIV3_FACTORY {
        Some(liq_plan::V3_FACTORY_UNISWAP)
    } else if factory == SUSHI_V3_FACTORY {
        Some(liq_plan::V3_FACTORY_SUSHI)
    } else if factory == PANCAKE_V3_FACTORY {
        Some(liq_plan::V3_FACTORY_PANCAKE)
    } else {
        None
    }
}

fn omit(out: &mut Vec<(&'static str, String)>, name: &'static str, why: impl std::fmt::Display) {
    let why = why.to_string();
    tracing::error!(group = name, reason = %why, "index subscriber omitted — no invented address/depth/price");
    out.push((name, why));
}

fn extra_addr(p: &ProtocolEntry, key: &str) -> Option<Address> {
    let v = p.extra.get(key)?;
    let s = v.as_str()?;
    let a = s.parse::<Address>().ok()?;
    if a.is_zero() {
        None
    } else {
        Some(a)
    }
}

fn first_extra(reg: &Registry, keys: &[&str]) -> Option<Address> {
    for p in reg.protocols.values() {
        for k in keys {
            if let Some(a) = extra_addr(p, k) {
                return Some(a);
            }
        }
    }
    None
}

fn flash_map_addr(reg: &Registry, kinds: &[&str]) -> Option<Address> {
    for addr in reg.flash_sources.keys().copied() {
        if addr.is_zero() {
            continue;
        }
        let Some(kind) = reg.flash_source_kind(addr) else {
            tracing::error!(address = %addr, "flash_sources row has no kind — omitted (no guessed arena)");
            continue;
        };
        if kinds.iter().any(|k| kind.eq_ignore_ascii_case(k)) {
            return Some(addr);
        }
    }
    None
}

fn intern_held(intern: &Intern) -> Vec<HeldAsset> {
    intern
        .assets()
        .iter()
        .filter(|a| !a.address.is_zero())
        .map(|a| HeldAsset {
            asset: a.id,
            token: a.address,
            balance: U256::ZERO,
        })
        .collect()
}

/// The pool's `PoolConfigurator` from the protocol TOML that binds it
/// (`aave-v3.toml` or `spark.toml`, generated from the provider's
/// `getPoolConfigurator()`). The registry does not carry it.
fn toml_configurator(config_dir: &Path, pool: Address) -> Option<Address> {
    ["protocols/aave-v3.toml", "protocols/spark.toml"]
        .iter()
        .filter_map(|f| AaveV3Toml::from_path(&config_dir.join(f)).ok())
        .find_map(|toml| {
            toml.pools
                .iter()
                .find(|p| p.address == pool)
                .map(|p| p.configurator)
        })
        .filter(|a| !a.is_zero())
}

fn nonzero_filters(filters: Vec<LogFilter>) -> Vec<LogFilter> {
    filters
        .into_iter()
        .filter(|f| !f.address.is_zero())
        .collect()
}

fn univ3_factory(reg: &Registry) -> Option<Address> {
    let mut found = None;
    for p in reg.pools.values() {
        // A fork's pools are Executor-verified by their own factory id; this
        // agreement is about Uniswap's.
        if p.venue != PoolVenue::Univ3
            || p.factory.is_zero()
            || v3_factory_id(p.factory).is_some_and(|f| f != liq_plan::V3_FACTORY_UNISWAP)
        {
            continue;
        }
        match found {
            None => found = Some(p.factory),
            Some(f) if f != p.factory => {
                tracing::error!("univ3 factory disagree — PoolCreated discovery omitted");
                return None;
            }
            Some(_) => {}
        }
    }
    found
}

/// Shared flash-source roster. Hot thread is the only writer.
pub type FlashSources = Arc<Mutex<Vec<Box<dyn FlashSource>>>>;

/// One flash source as a router subscriber / handler. Writer is the hot thread.
#[derive(Clone)]
pub struct FlashHandler {
    sources: FlashSources,
    idx: usize,
}

impl LogSubscriber for FlashHandler {
    fn subscriptions(&self) -> Vec<LogFilter> {
        let g = self.sources.lock();
        g.get(self.idx)
            .map(|s| nonzero_filters(s.subscriptions()))
            .unwrap_or_default()
    }
}

impl LogHandler for FlashHandler {
    fn apply_log(
        &self,
        _st: &mut dyn liq_protocol::StateWriter,
        log: &DecodedLog<'_>,
    ) -> core::result::Result<DirtySet, ProtocolError> {
        let mut g = self.sources.lock();
        if let Some(s) = g.get_mut(self.idx) {
            s.apply_log(log);
        } else {
            tracing::error!(idx = self.idx, "flash handler idx missing — log dropped");
        }
        Ok(DirtySet::None)
    }
}

/// Shared `PoolBook` subscriber. Single writer on the hot thread.
#[derive(Clone)]
pub struct BookHandler {
    book: Arc<RwLock<PoolBook>>,
    /// A pool the book discovers from a log (Uniswap V3 `PoolCreated`) needs
    /// its own logs routed: ask the hot thread to rebuild its router.
    resubscribe: Arc<liq_node::Resubscribe>,
}

impl LogSubscriber for BookHandler {
    fn subscriptions(&self) -> Vec<LogFilter> {
        nonzero_filters(self.book.read().subscriptions())
    }
}

impl LogHandler for BookHandler {
    fn apply_log(
        &self,
        _st: &mut dyn liq_protocol::StateWriter,
        log: &DecodedLog<'_>,
    ) -> core::result::Result<DirtySet, ProtocolError> {
        let mut book = self.book.write();
        let before = book.discovered();
        book.apply_log(log);
        if book.discovered() != before {
            self.resubscribe.request();
        }
        Ok(DirtySet::None)
    }
}

#[derive(Clone)]
pub struct FeedSub {
    book: Arc<Mutex<CanonicalBook>>,
}

impl LogSubscriber for FeedSub {
    fn subscriptions(&self) -> Vec<LogFilter> {
        nonzero_filters(self.book.lock().feeds().subscriptions())
    }
}

pub struct FeedHandler {
    book: Arc<Mutex<CanonicalBook>>,
    sink: &'static dyn HaltSink,
}

impl LogSubscriber for FeedHandler {
    fn subscriptions(&self) -> Vec<LogFilter> {
        nonzero_filters(self.book.lock().feeds().subscriptions())
    }
}

impl LogHandler for FeedHandler {
    fn apply_log(
        &self,
        _st: &mut dyn liq_protocol::StateWriter,
        log: &DecodedLog<'_>,
    ) -> core::result::Result<DirtySet, ProtocolError> {
        match self.book.lock().apply_log(log, self.sink) {
            Ok(_) => Ok(DirtySet::None),
            Err(liq_oracle::OracleError::SourceMigrated { .. }) => Err(ProtocolError::HaltSignal),
            Err(liq_oracle::OracleError::BadAnswerUpdated)
            | Err(liq_oracle::OracleError::NonPositiveAnswer) => Err(ProtocolError::MalformedLog),
            Err(e) => {
                tracing::error!(error = %e, "canonical apply_log refused");
                Err(ProtocolError::MalformedLog)
            }
        }
    }
}

#[derive(Clone)]
pub struct DerivedSub {
    book: Arc<Mutex<DerivedBook>>,
}

impl LogSubscriber for DerivedSub {
    fn subscriptions(&self) -> Vec<LogFilter> {
        nonzero_filters(self.book.lock().subscriptions())
    }
}

pub struct DerivedHandler {
    book: Arc<Mutex<DerivedBook>>,
    canonical: Option<Arc<Mutex<CanonicalBook>>>,
    sink: &'static dyn HaltSink,
}

impl LogSubscriber for DerivedHandler {
    fn subscriptions(&self) -> Vec<LogFilter> {
        nonzero_filters(self.book.lock().subscriptions())
    }
}

impl LogHandler for DerivedHandler {
    fn apply_log(
        &self,
        _st: &mut dyn liq_protocol::StateWriter,
        log: &DecodedLog<'_>,
    ) -> core::result::Result<DirtySet, ProtocolError> {
        let deps = match self.canonical.as_ref() {
            Some(c) => c.lock().vector().clone(),
            None => {
                tracing::error!("derived apply without canonical deps — omitted");
                return Ok(DirtySet::None);
            }
        };
        match self.book.lock().apply_log(log, &deps, self.sink) {
            Ok(_) => Ok(DirtySet::None),
            Err(liq_oracle::OracleError::SourceMigrated { .. }) => Err(ProtocolError::HaltSignal),
            Err(liq_oracle::OracleError::BadRateLog) => Err(ProtocolError::MalformedLog),
            Err(e) => {
                tracing::error!(error = %e, "derived apply_log refused");
                Err(ProtocolError::MalformedLog)
            }
        }
    }
}

/// Leaked process index. Drain + warm read the same book / sources.
pub struct BoundIndex {
    pub sources: FlashSources,
    pub book: Arc<RwLock<PoolBook>>,
    pub canonical: Option<Arc<Mutex<CanonicalBook>>>,
    pub derived: Option<Arc<Mutex<DerivedBook>>>,
    pub flash_assets: usize,
    /// Asks the hot thread to rebuild its router and the ExEx to forward the
    /// new address set when what the book follows changes at runtime.
    pub resubscribe: Arc<liq_node::Resubscribe>,
    flash_handlers: Vec<FlashHandler>,
    book_handler: Option<BookHandler>,
    feed_sub: Option<FeedSub>,
    derived_sub: Option<DerivedSub>,
    pub omitted: Vec<(&'static str, String)>,
}

impl BoundIndex {
    /// Flash then book then feeds then derived — concat after protocol adapters.
    #[must_use]
    pub fn subscribers(&self) -> Vec<&dyn LogSubscriber> {
        let mut out: Vec<&dyn LogSubscriber> = Vec::new();
        for h in &self.flash_handlers {
            out.push(h);
        }
        if let Some(h) = &self.book_handler {
            out.push(h);
        }
        if let Some(h) = &self.feed_sub {
            out.push(h);
        }
        if let Some(h) = &self.derived_sub {
            out.push(h);
        }
        out
    }

    /// Handlers in the same order as [`Self::subscribers`].
    #[must_use]
    pub fn handlers(&self, sink: &'static dyn HaltSink) -> Vec<Box<dyn LogHandler + Send>> {
        let mut out: Vec<Box<dyn LogHandler + Send>> = Vec::new();
        for h in &self.flash_handlers {
            out.push(Box::new(h.clone()));
        }
        if let Some(h) = &self.book_handler {
            out.push(Box::new(h.clone()));
        }
        if let Some(f) = &self.canonical {
            out.push(Box::new(FeedHandler {
                book: Arc::clone(f),
                sink,
            }));
        }
        if let Some(d) = &self.derived {
            out.push(Box::new(DerivedHandler {
                book: Arc::clone(d),
                canonical: self.canonical.clone(),
                sink,
            }));
        }
        out
    }

    #[must_use]
    pub fn subscriber_empty(&self) -> bool {
        self.flash_handlers.is_empty()
            && self
                .book_handler
                .as_ref()
                .is_none_or(|h| h.subscriptions().is_empty())
            && self.feed_sub.is_none()
            && self.derived_sub.is_none()
    }
}

/// Construct flash / book / feeds from registry + config. Missing → omit, log.
#[must_use]
pub fn load_index(config_dir: &Path, intern: &Intern, registry: &Registry) -> IndexLoad {
    let mut omitted = Vec::new();
    let model = crate::gas_model::GasModel::load(&config_dir.join("liq-gas.toml"));
    let wrap = model.as_ref().map_or_else(
        crate::bind::WrapGas::default,
        crate::gas_model::GasModel::select_wrap,
    );
    let hops = model.as_ref().map_or_else(Default::default, |m| m.hop);
    let sources = load_flash(
        config_dir,
        intern,
        registry,
        &wrap.by_provider,
        &mut omitted,
    );
    let book = load_book(intern, registry, hops, &mut omitted);
    let canonical = load_feeds(config_dir, intern, registry, &mut omitted);
    let derived = load_derived(&mut omitted);
    IndexLoad {
        sources,
        book,
        canonical,
        derived,
        omitted,
        flash_assets: intern.asset_id_capacity(),
    }
}

/// Pre-leak bundle.
pub struct IndexLoad {
    pub sources: Vec<Box<dyn FlashSource>>,
    pub book: PoolBook,
    pub canonical: Option<CanonicalBook>,
    pub derived: Option<DerivedBook>,
    pub omitted: Vec<(&'static str, String)>,
    pub flash_assets: usize,
}

impl IndexLoad {
    fn into_bound(self) -> BoundIndex {
        let sources = Arc::new(Mutex::new(self.sources));
        let n = sources.lock().len();
        let mut flash_handlers = Vec::with_capacity(n);
        for idx in 0..n {
            flash_handlers.push(FlashHandler {
                sources: Arc::clone(&sources),
                idx,
            });
        }
        let book = Arc::new(RwLock::new(self.book));
        let resubscribe = Arc::new(liq_node::Resubscribe::new());
        // Always present: pools the registry watcher or V3 discovery adds at
        // runtime subscribe through it even when the book starts empty.
        let book_handler = Some(BookHandler {
            book: Arc::clone(&book),
            resubscribe: Arc::clone(&resubscribe),
        });
        let canonical = self.canonical.map(|b| Arc::new(Mutex::new(b)));
        let feed_sub = canonical.as_ref().and_then(|b| {
            let subs = b.lock().feeds().subscriptions();
            if subs.iter().any(|f| !f.address.is_zero()) {
                Some(FeedSub {
                    book: Arc::clone(b),
                })
            } else {
                None
            }
        });
        let derived = self.derived.map(|b| Arc::new(Mutex::new(b)));
        let derived_sub = derived.as_ref().and_then(|b| {
            let subs = b.lock().subscriptions();
            if subs.iter().any(|f| !f.address.is_zero()) {
                Some(DerivedSub {
                    book: Arc::clone(b),
                })
            } else {
                None
            }
        });
        BoundIndex {
            sources,
            book,
            canonical,
            derived,
            flash_assets: self.flash_assets,
            resubscribe,
            flash_handlers,
            book_handler,
            feed_sub,
            derived_sub,
            omitted: self.omitted,
        }
    }
}

/// Leak once. Drain, warm, and ingest share this object.
#[must_use]
pub fn leak_index(load: IndexLoad) -> &'static BoundIndex {
    if load.sources.is_empty()
        && load.book.pools().is_empty()
        && load.canonical.is_none()
        && load.derived.is_none()
    {
        tracing::error!("empty index load — flash/book/feeds absent from router");
    }
    Box::leak(Box::new(load.into_bound()))
}

fn wrap_of(wrap: &[u64; 7], p: FlashProvider) -> u64 {
    wrap.get(p as usize).copied().unwrap_or(0)
}

fn load_flash(
    config_dir: &Path,
    intern: &Intern,
    registry: &Registry,
    wrap: &[u64; 7],
    omitted: &mut Vec<(&'static str, String)>,
) -> Vec<Box<dyn FlashSource>> {
    let mut sources: Vec<Box<dyn FlashSource>> = Vec::new();
    let mut saw_v4_proto = false;
    for proto in registry.protocols.values() {
        if proto.family == "aave-v4" && !saw_v4_proto {
            omit(
                omitted,
                "aave-v4",
                "no V3-style flash pool address in registry/config",
            );
            saw_v4_proto = true;
        }
        if proto.family != "aave-v3" && proto.family != "spark" {
            continue;
        }
        let pool = match proto.market {
            OnChainId::Addr(a) if !a.is_zero() => a,
            _ => {
                omit(
                    omitted,
                    "aave-pool",
                    format!("{} market is not a pool address", proto.family),
                );
                continue;
            }
        };
        let configurator =
            extra_addr(proto, "configurator").or_else(|| toml_configurator(config_dir, pool));
        if configurator.is_none() {
            tracing::error!(
                family = proto.family.as_str(),
                pool = %pool,
                "aave configurator missing — premium/reserve-flag logs unsubscribed"
            );
        }
        // Reserves, premium, flags and balances come from chain at startup
        // (`flash_seed`); until then the pool funds nothing.
        sources.push(Box::new(
            AavePool::unseeded(
                pool,
                configurator.unwrap_or(Address::ZERO),
                &intern_held(intern),
            )
            .with_overhead(wrap_of(wrap, FlashProvider::Aave)),
        ));
    }

    for (addr, entry) in &registry.pools {
        if entry.venue != PoolVenue::Univ3 {
            continue;
        }
        if addr.is_zero() || entry.token0.is_zero() || entry.token1.is_zero() {
            omit(omitted, "univ3", format!("zero address on pool {addr:#x}"));
            continue;
        }
        let Some(a0) = intern.asset(entry.token0) else {
            omit(
                omitted,
                "univ3",
                format!("token0 {:#x} not interned", entry.token0),
            );
            continue;
        };
        let Some(a1) = intern.asset(entry.token1) else {
            omit(
                omitted,
                "univ3",
                format!("token1 {:#x} not interned", entry.token1),
            );
            continue;
        };
        sources.push(Box::new(
            UniV3Pool::new(
                *addr,
                entry.token0,
                entry.token1,
                a0,
                a1,
                entry.fee,
                U256::ZERO,
                U256::ZERO,
            )
            .with_overhead(wrap_of(wrap, FlashProvider::UniV3)),
        ));
    }

    match flash_map_addr(registry, &["univ4", "pool_manager"])
        .or_else(|| first_extra(registry, &["pool_manager"]))
    {
        Some(pm) => {
            let held = intern_held(intern);
            sources.push(Box::new(
                UniV4PoolManager::new(pm, &held).with_overhead(wrap_of(wrap, FlashProvider::UniV4)),
            ));
        }
        None => omit(
            omitted,
            "univ4",
            "PoolManager address missing from registry/config",
        ),
    }

    match flash_map_addr(registry, &["morpho", "singleton"])
        .or_else(|| first_extra(registry, &["morpho", "singleton"]))
    {
        Some(m) => {
            let held = intern_held(intern);
            sources.push(Box::new(
                MorphoBlue::new(m, &held).with_overhead(wrap_of(wrap, FlashProvider::Morpho)),
            ));
        }
        None => omit(
            omitted,
            "morpho",
            "Morpho singleton missing from registry/config",
        ),
    }

    let sky = flash_map_addr(registry, &["sky", "dss_flash", "mcd_flash"])
        .or_else(|| first_extra(registry, &["dss_flash", "mcd_flash"]));
    let end = flash_map_addr(registry, &["mcd_end", "end"])
        .or_else(|| first_extra(registry, &["mcd_end", "end"]));
    let dai = intern.asset(REGISTRY_DAI);
    match (sky, end, dai) {
        (Some(flash), Some(end), Some(dai)) => {
            sources.push(Box::new(
                SkyDssFlash::new(flash, end, dai, U256::ZERO, U256::ZERO, false)
                    .with_overhead(wrap_of(wrap, FlashProvider::SkyDss)),
            ));
        }
        _ => omit(
            omitted,
            "sky-dss",
            "MCD_FLASH / MCD_END / DAI missing from registry/config",
        ),
    }

    if sources.is_empty() {
        tracing::error!("no flash sources constructed — FlashIndex stays empty");
    }
    sources
}

/// Uniswap V2 factory (pairs verified by the Executor's CREATE2 check).
const UNIV2_FACTORY: Address =
    alloy_primitives::address!("0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f");
/// SushiSwap V2 factory.
const SUSHI_FACTORY: Address =
    alloy_primitives::address!("0xC0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac");

/// Executor factory id for a V2 pair's committed factory; `None` = a fork
/// the Executor cannot verify (omitted).
fn v2_factory_id(factory: Address) -> Option<u8> {
    if factory == UNIV2_FACTORY {
        Some(liq_plan::V2_FACTORY_UNISWAP)
    } else if factory == SUSHI_FACTORY {
        Some(liq_plan::V2_FACTORY_SUSHI)
    } else {
        None
    }
}

/// Curve plain-pool `RATES[i]` = `10^(36 − decimals)`.
fn curve_rate(decimals: u8) -> Option<U256> {
    let exp = 36u64.checked_sub(u64::from(decimals))?;
    Some(U256::from(10u64).pow(U256::from(exp)))
}

/// Registry pools → the routable [`PoolBook`]. State starts empty (not
/// live): V3/V2 are seeded at startup and folded from logs; Curve starts
/// stale and is read by the Curve reseed thread.
fn load_book(
    intern: &Intern,
    registry: &Registry,
    hops: crate::gas_model::HopGas,
    omitted: &mut Vec<(&'static str, String)>,
) -> PoolBook {
    if hops.univ3 == 0
        || hops.univ2 == 0
        || hops.curve == 0
        || hops.curve_ng == 0
        || hops.curve_crypto == 0
        || hops.pancake_v3 == 0
        || hops.balancer == 0
        || hops.fluid == 0
        || hops.unwrap_4626 == 0
        || hops.pendle_pt == 0
        || hops.curve_lp == 0
        || hops.pendle_market == 0
    {
        tracing::error!(
            ?hops,
            "swap hop gas unmeasured for a venue — priced at 0 until liq-gas.toml loads"
        );
    }
    let mut assets = HashMap::new();
    for rec in intern.assets() {
        if rec.address.is_zero() {
            tracing::error!(
                asset = rec.id.0,
                "intern token is zero — skipped in book assets"
            );
            continue;
        }
        assets.insert(rec.address, rec.id);
    }
    // No pool joins the book while the bot runs: Uniswap V3 `PoolCreated`
    // discovery is off (decision 2026-10-07). New pools are proposed by the
    // daily refresh for review and admitted to the registry by hand, then
    // take effect at a restart. The factory is still checked for agreement.
    let _ = univ3_factory(registry);
    let mut book = PoolBook::new(assets, None, hops.univ3);
    // Exits may route through WETH: the deep pools are against it, and a
    // collateral's own pool into its debt can be thin, or missing.
    match intern.asset(crate::bind::REGISTRY_WETH) {
        Some(weth) => book.set_hub(weth),
        None => tracing::error!("WETH not interned — exits route direct only"),
    }
    for (addr, entry) in &registry.pools {
        match build_pool(intern, registry, hops, *addr, entry) {
            Ok(pool) => {
                if let Err(e) = book.add(pool) {
                    tracing::error!(error = ?e, pool = %addr, "pool address seed refused");
                }
            }
            Err(why) => omit(omitted, "book", why),
        }
    }
    if book.pools().is_empty() {
        tracing::error!("PoolBook has no seeded addresses — empty publish until logs");
    }
    add_unwraps(&mut book, intern, registry, hops, omitted);
    book
}

/// The sentinel the Fluid pools and the Liquidity layer name native ETH by.
pub(crate) const FLUID_NATIVE: Address =
    alloy_primitives::address!("0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");

/// One registry pool as an unseeded book pool (V3 / V2 zero state, Curve
/// and crypto stale until read). `Err` names why it cannot be routed.
pub(crate) fn build_pool(
    intern: &Intern,
    registry: &Registry,
    hops: crate::gas_model::HopGas,
    addr: Address,
    entry: &liq_config::PoolEntry,
) -> Result<Pool, String> {
    let tokens: SmallVec<[Address; liq_router::MAX_COINS]> = match entry.venue {
        PoolVenue::Curve
        | PoolVenue::CurveNg
        | PoolVenue::CurveCrypto
        | PoolVenue::Balancer
        | PoolVenue::Fluid => entry.coins.iter().copied().collect(),
        PoolVenue::Univ3 | PoolVenue::Univ4 | PoolVenue::Univ2 => {
            SmallVec::from_slice(&[entry.token0, entry.token1])
        }
    };
    if addr.is_zero()
        || tokens.len() < 2
        || tokens.len() > liq_router::MAX_COINS
        || tokens.iter().any(|t| t.is_zero())
    {
        return Err(format!("zero address on pool {addr:#x}"));
    }
    // A V4 ETH/WETH pool holds WETH on both sides to the plan: a self-loop,
    // not a swap (and its book address is not a contract to flash from).
    if tokens
        .iter()
        .enumerate()
        .any(|(i, t)| tokens.iter().skip(i.saturating_add(1)).any(|u| u == t))
    {
        return Err(format!("pool {addr:#x} holds one token twice"));
    }
    let mut ids = SmallVec::new();
    let mut rates = SmallVec::new();
    for t in &tokens {
        let Some(id) = intern.asset(*t) else {
            break;
        };
        ids.push(id);
        if entry.venue.is_curve() {
            // NG: the precision multiplier until the first read replaces
            // it with `stored_rates()` (the pool starts stale).
            match registry.tokens.get(t).and_then(|e| curve_rate(e.decimals)) {
                Some(r) => rates.push(r),
                None => break,
            }
        }
    }
    if ids.len() != tokens.len() || (entry.venue.is_curve() && rates.len() != tokens.len()) {
        return Err(format!(
            "pool {addr:#x} has a coin not interned / no decimals"
        ));
    }
    // Every Curve venue: the MetaRegistry handler that holds the pool. Its
    // legs carry the index, so a pool without one cannot be encoded.
    let curve_handler = || {
        entry
            .curve_handler
            .ok_or_else(|| format!("curve pool {addr:#x}: no MetaRegistry handler index"))
    };
    let (hop_gas, state) = match entry.venue {
        PoolVenue::Univ3 => {
            let Some(spacing) = univ3_tick_spacing(entry.fee) else {
                return Err(format!("unknown univ3 fee {} on {addr:#x}", entry.fee));
            };
            let Some(factory) = v3_factory_id(entry.factory) else {
                return Err(format!(
                    "v3 pool {addr:#x} factory {:#x} not verifiable",
                    entry.factory
                ));
            };
            (
                // Pancake's pool calls its `lmPool` in every swap (measured
                // in `liq-gas.toml`); Sushi's swap is Uniswap's.
                if factory == liq_plan::V3_FACTORY_PANCAKE {
                    hops.pancake_v3
                } else {
                    hops.univ3
                },
                PoolState::V3(V3State {
                    factory,
                    sqrt_price_x96: U256::ZERO,
                    tick: 0,
                    liquidity: 0,
                    fee_pips: entry.fee,
                    tick_spacing: spacing,
                    ticks: Vec::new(),
                    v4: None,
                    window: None,
                }),
            )
        }
        PoolVenue::Univ4 => {
            let (Some(tick_spacing), Some(hooks), Some(id)) =
                (entry.tick_spacing, entry.hooks, entry.v4_id)
            else {
                return Err(format!(
                    "univ4 pool {addr:#x}: no tick spacing, hooks or id"
                ));
            };
            if entry.v4_key_id() != Some(id) || liq_router::V4Key::book_address_of(id).0 != *addr {
                return Err(format!("univ4 pool {addr:#x}: key does not hash to its id"));
            }
            (
                hops.univ4,
                PoolState::V3(V3State {
                    factory: 0,
                    sqrt_price_x96: U256::ZERO,
                    tick: 0,
                    liquidity: 0,
                    // The swap fee is read with the slot0 (protocol fee).
                    fee_pips: entry.fee,
                    tick_spacing,
                    ticks: Vec::new(),
                    v4: Some(liq_router::V4Key {
                        currency0: entry.v4_currency0(),
                        currency1: entry.token1,
                        fee: entry.fee,
                        tick_spacing,
                        hooks,
                        id,
                    }),
                    window: None,
                }),
            )
        }
        PoolVenue::Univ2 => {
            let Some(factory) = v2_factory_id(entry.factory) else {
                return Err(format!(
                    "v2 pair {addr:#x} factory {:#x} not verifiable",
                    entry.factory
                ));
            };
            (
                hops.univ2,
                PoolState::V2(V2State {
                    reserve0: U256::ZERO,
                    reserve1: U256::ZERO,
                    factory,
                }),
            )
        }
        PoolVenue::Curve => (
            hops.curve,
            PoolState::Curve(CurveState {
                balances: tokens.iter().map(|_| U256::ZERO).collect(),
                rates,
                a: U256::ZERO,
                a_precision: U256::from(1u64),
                fee: U256::from(entry.fee),
                stale: true,
                stale_block: 0,
                ng: false,
                d_once: entry.curve_d_once,
                offpeg_fee_multiplier: U256::ZERO,
                dynamic_rates: false,
                read_block: 0,
                handler: curve_handler()?,
            }),
        ),
        PoolVenue::CurveCrypto => {
            let kind = match entry.crypto_kind {
                Some(liq_config::CryptoKind::TwoV1) => CryptoKind::TwoV1,
                Some(liq_config::CryptoKind::TwoV200) => CryptoKind::TwoV200,
                Some(liq_config::CryptoKind::TwoV210) => CryptoKind::TwoV210,
                Some(liq_config::CryptoKind::TwoStable) => CryptoKind::TwoStable,
                Some(liq_config::CryptoKind::Tri) => CryptoKind::Tri,
                None => return Err(format!("curve crypto {addr:#x}: no crypto_kind")),
            };
            // `10^(18 − decimals)`; `rates` holds `10^(36 − decimals)`.
            let precisions = rates
                .iter()
                .map(|r| r.checked_div(U256::from(10u64).pow(U256::from(18u64))))
                .collect::<Option<SmallVec<[U256; liq_router::MAX_COINS]>>>();
            let Some(precisions) = precisions else {
                return Err(format!("curve crypto {addr:#x}: decimals"));
            };
            (
                hops.curve_crypto,
                PoolState::Crypto(CryptoState {
                    kind,
                    balances: tokens.iter().map(|_| U256::ZERO).collect(),
                    precisions,
                    price_scale: tokens.iter().skip(1).map(|_| U256::ZERO).collect(),
                    d: U256::ZERO,
                    ann: U256::ZERO,
                    gamma: U256::ZERO,
                    mid_fee: U256::ZERO,
                    out_fee: U256::ZERO,
                    fee_gamma: U256::ZERO,
                    stale: true,
                    stale_block: 0,
                    read_block: 0,
                    handler: curve_handler()?,
                    tweak: None,
                }),
            )
        }
        PoolVenue::Balancer => {
            let (Some(pool_id), Some(kind)) = (entry.pool_id, entry.balancer_kind) else {
                return Err(format!("balancer {addr:#x}: no pool id or kind"));
            };
            // The entry's key is the pool id's first 20 bytes.
            let keyed = pool_id
                .first_chunk::<20>()
                .is_some_and(|head| Address::from(*head) == addr);
            if tokens.len() != 2 || !keyed {
                return Err(format!(
                    "balancer {addr:#x}: not a two-token pool keyed by its id"
                ));
            }
            // `10^(18 − decimals)`: a Balancer pool scales every token to 18.
            let scaling = tokens
                .iter()
                .map(|t| {
                    registry
                        .tokens
                        .get(t)
                        .and_then(|e| 18u8.checked_sub(e.decimals))
                        .map(|d| U256::from(10u64).pow(U256::from(d)))
                })
                .collect::<Option<SmallVec<[U256; liq_router::MAX_COINS]>>>();
            let Some(scaling) = scaling else {
                return Err(format!(
                    "balancer {addr:#x}: a token has no decimals or more than 18"
                ));
            };
            (
                hops.balancer,
                PoolState::Balancer(liq_router::BalancerState {
                    pool_id,
                    tokens: tokens.clone(),
                    balances: tokens.iter().map(|_| U256::ZERO).collect(),
                    weights: tokens.iter().map(|_| U256::ZERO).collect(),
                    scaling,
                    swap_fee: U256::ZERO,
                    fast_pow: kind == liq_config::BalancerKind::WeightedV4,
                    stale: true,
                    stale_block: 0,
                    read_block: 0,
                }),
            )
        }
        PoolVenue::Fluid => {
            if tokens.len() != 2 {
                return Err(format!("fluid {addr:#x}: not a two-token pool"));
            }
            // Native ETH is a coin named WETH to the plan; the pool and the
            // Liquidity layer's logs name it by the 0xEeee… sentinel.
            let native_side = |t: &Address| entry.native && *t == crate::bind::REGISTRY_WETH;
            let on_chain: SmallVec<[Address; liq_router::MAX_COINS]> = tokens
                .iter()
                .map(|t| if native_side(t) { FLUID_NATIVE } else { *t })
                .collect();
            let mut native = [false; 2];
            for (n, t) in native.iter_mut().zip(&tokens) {
                *n = native_side(t);
            }
            (
                hops.fluid,
                PoolState::Fluid(liq_router::FluidState {
                    prec: [U256::ZERO; 4],
                    native,
                    deployer: Address::ZERO,
                    tokens: on_chain,
                    dex_vars: U256::ZERO,
                    dex_vars2: U256::ZERO,
                    center_ext: None,
                    liq: [liq_router::LiqToken::default(); 2],
                    exec_ts: 0,
                    stale: true,
                    stale_block: 0,
                    read_block: 0,
                }),
            )
        }
        PoolVenue::CurveNg => {
            // Rebasing coins (type 2) change balances without a pool
            // log; the reseed would quote a stale balance. Left out.
            if entry.asset_types.len() != tokens.len() || entry.asset_types.contains(&2) {
                return Err(format!(
                    "curve NG {addr:#x}: asset types missing or rebasing"
                ));
            }
            (
                hops.curve_ng,
                PoolState::Curve(CurveState {
                    balances: tokens.iter().map(|_| U256::ZERO).collect(),
                    rates,
                    a: U256::ZERO,
                    a_precision: U256::from(100u64),
                    fee: U256::from(entry.fee),
                    stale: true,
                    stale_block: 0,
                    ng: true,
                    d_once: true,
                    offpeg_fee_multiplier: U256::ZERO,
                    dynamic_rates: entry.asset_types.iter().any(|t| *t == 1 || *t == 3),
                    read_block: 0,
                    handler: curve_handler()?,
                }),
            )
        }
    };
    Ok(Pool {
        address: addr,
        assets: ids,
        tokens,
        hop_gas,
        state,
    })
}

/// Registry wrappers → the book's unwraps. The rate starts unread (not
/// routed) until the reseed thread reads it.
fn add_unwraps(
    book: &mut PoolBook,
    intern: &Intern,
    registry: &Registry,
    hops: crate::gas_model::HopGas,
    omitted: &mut Vec<(&'static str, String)>,
) {
    for (addr, entry) in &registry.tokens {
        match build_unwrap(intern, registry, hops, *addr, entry) {
            Ok(Some(u)) => book.add_unwrap(u),
            Ok(None) => {}
            Err(why) => omit(omitted, "book", why),
        }
    }
}

/// A registry token's unwrap as an unread book unwrap; `None` when the
/// token has none. `Err` names why it cannot be routed.
pub(crate) fn build_unwrap(
    intern: &Intern,
    registry: &Registry,
    hops: crate::gas_model::HopGas,
    addr: Address,
    entry: &liq_config::TokenEntry,
) -> Result<Option<liq_router::Unwrap>, String> {
    let Some(u) = entry.unwrap else {
        return Ok(None);
    };
    let (Some(wrapper), Some(into)) = (intern.asset(addr), intern.asset(u.into)) else {
        return Err(format!(
            "unwrap {addr:#x} → {:#x}: token not interned",
            u.into
        ));
    };
    // A thousand whole shares: the linear rate keeps three more digits
    // than one share would.
    let Some(scale) = U256::from(10u64).checked_pow(U256::from(entry.decimals.saturating_add(3)))
    else {
        return Err(format!("unwrap {addr:#x}: decimals"));
    };
    let kind = match (u.kind, u.yt, u.sy) {
        (liq_config::UnwrapKind::Erc4626, _, _) => liq_router::UnwrapKind::Erc4626,
        (liq_config::UnwrapKind::CurveLp, _, _) => {
            // The LP is its own pool; `into` is one of its coins.
            let coin = registry
                .pools
                .get(&addr)
                .filter(|p| p.venue == PoolVenue::CurveNg)
                .and_then(|p| p.coins.iter().position(|c| *c == u.into))
                .and_then(|i| u8::try_from(i).ok());
            let Some(i) = coin else {
                return Err(format!(
                    "curve LP {addr:#x}: not an NG registry pool holding {:#x}",
                    u.into
                ));
            };
            liq_router::UnwrapKind::CurveLp { i }
        }
        (liq_config::UnwrapKind::PendlePt, Some(yt), Some(sy)) => {
            liq_router::UnwrapKind::PendlePt { yt, sy }
        }
        (liq_config::UnwrapKind::PendlePt, _, _) => {
            return Err(format!("pendle PT {addr:#x}: no yt/sy"));
        }
        (liq_config::UnwrapKind::PendleMarket, Some(yt), Some(sy)) if u.market.is_some() => {
            liq_router::UnwrapKind::PendleMarket {
                market: u.market.unwrap_or_default(),
                yt,
                sy,
            }
        }
        (liq_config::UnwrapKind::PendleMarket, _, _) => {
            return Err(format!("pendle market PT {addr:#x}: no market/sy"));
        }
    };
    Ok(Some(liq_router::Unwrap {
        kind,
        wrapper,
        wrapper_token: addr,
        into,
        into_token: u.into,
        rate: liq_router::UnwrapRate::Unread,
        // A Curve LP's scale is only the step its marginal is taken
        // over: a thousandth of a token, not a thousand.
        scale: match kind {
            liq_router::UnwrapKind::CurveLp { .. } => scale
                .checked_div(U256::from(1_000_000u64))
                .unwrap_or_default(),
            // One whole PT: the market's marginal (a smaller sale can
            // pay a zero LP fee, which the market refuses).
            liq_router::UnwrapKind::PendleMarket { .. } => {
                scale.checked_div(U256::from(1_000u64)).unwrap_or_default()
            }
            _ => scale,
        },
        read_block: 0,
        cash_capped: u.cash_capped && kind == liq_router::UnwrapKind::Erc4626,
        gas: match kind {
            liq_router::UnwrapKind::Erc4626 => hops.unwrap_4626,
            liq_router::UnwrapKind::PendlePt { .. } => hops.pendle_pt,
            liq_router::UnwrapKind::CurveLp { .. } => hops.curve_lp,
            liq_router::UnwrapKind::PendleMarket { .. } => hops.pendle_market,
        },
        // At expiry a market PT switches to the post-expiry redeem.
        expiry_gas: match kind {
            liq_router::UnwrapKind::PendleMarket { .. } => hops.pendle_pt,
            _ => 0,
        },
    }))
}

fn load_feeds(
    config_dir: &Path,
    intern: &Intern,
    registry: &Registry,
    omitted: &mut Vec<(&'static str, String)>,
) -> Option<CanonicalBook> {
    let pin = config_dir
        .parent()
        .map(|p| p.join("registry/feeds-mainnet.PIN"));
    let Some(pin) = pin else {
        omit(
            omitted,
            "feeds",
            "config dir has no parent — PIN unresolved",
        );
        return None;
    };
    match std::fs::read_to_string(&pin) {
        Ok(txt) if txt.contains("feeds-mainnet.json") => {}
        Ok(_) => {
            omit(omitted, "feeds", "PIN does not name feeds-mainnet.json");
            return None;
        }
        Err(e) => {
            omit(omitted, "feeds", format!("PIN unreadable: {e}"));
            return None;
        }
    }
    let feeds_dir = config_dir.join("feeds");
    let cfg = match FeedsConfig::load(&feeds_dir) {
        Ok(c) => c,
        Err(e) => {
            omit(omitted, "feeds", e);
            return None;
        }
    };
    let (set, failures) = FeedSet::bind_available(&cfg, intern, registry);
    for f in &failures {
        tracing::error!(
            proxy = %f.proxy,
            pair = %f.pair,
            reason = %f.reason,
            "feed row omitted — no invented answer"
        );
    }
    if !set.subscriptions().is_empty() {
        match CanonicalBook::new(set, intern, registry) {
            Ok((book, unfed)) => {
                for id in unfed {
                    tracing::error!(
                        asset = id.0,
                        "listed asset has no feed — price() stays None"
                    );
                }
                return Some(book);
            }
            Err(e) => {
                omit(omitted, "feeds", e);
                return None;
            }
        }
    }
    omit(omitted, "feeds", "no committed feed rows bound");
    None
}

fn load_derived(omitted: &mut Vec<(&'static str, String)>) -> Option<DerivedBook> {
    omit(
        omitted,
        "derived",
        "no committed derived-spec config — DerivedBook omitted",
    );
    None
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
    use alloy_primitives::Address;
    use liq_config::{Intern, Registry};
    use std::collections::HashSet;

    fn root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    fn committed() -> (Intern, Registry) {
        let reg = Registry::from_path(&root().join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        (intern, reg)
    }

    fn committed_addrs(intern: &Intern, reg: &Registry, config_dir: &Path) -> HashSet<Address> {
        let mut a = HashSet::new();
        for t in intern.assets() {
            a.insert(t.address);
        }
        a.extend(reg.pools.keys().copied());
        for p in reg.pools.values() {
            a.insert(p.factory);
            a.insert(p.token0);
            a.insert(p.token1);
        }
        a.extend(reg.oracles.keys().copied());
        for e in reg.oracles.values() {
            a.insert(e.aggregator);
        }
        for p in reg.protocols.values() {
            if let OnChainId::Addr(addr) = p.market {
                a.insert(addr);
            }
            for v in p.extra.values() {
                if let Some(s) = v.as_str() {
                    if let Ok(addr) = s.parse::<Address>() {
                        a.insert(addr);
                    }
                }
            }
        }
        a.extend(reg.flash_sources.keys().copied());
        for f in ["protocols/aave-v3.toml", "protocols/spark.toml"] {
            if let Ok(t) = AaveV3Toml::from_path(&config_dir.join(f)) {
                for p in t.pools {
                    a.insert(p.address);
                    a.insert(p.configurator);
                    a.insert(p.oracle);
                    a.insert(p.provider);
                }
            }
        }
        a
    }

    #[test]
    fn loaded_flash_book_feed_addrs_are_registry_or_config() {
        let (intern, reg) = committed();
        let cfg = root().join("config");
        let load = load_index(&cfg, &intern, &reg);
        // The registry's flash-source rows bind the singletons
        // (`registry_binds_the_flash_singletons`); none is omitted.
        for group in ["univ4", "morpho", "sky-dss"] {
            assert!(
                !load.omitted.iter().any(|(n, _)| *n == group),
                "{group} omitted: {:?}",
                load.omitted
            );
        }
        assert!(
            load.omitted.iter().any(|(n, _)| *n == "derived"),
            "derived specs are not committed: {:?}",
            load.omitted
        );
        assert!(
            !load.sources.is_empty(),
            "aave-v3/spark + C1 univ3 must construct: omitted={:?}",
            load.omitted
        );
        assert!(
            !load.book.pools().is_empty(),
            "C1 univ3 list must seed book addresses"
        );
        assert!(
            load.canonical.is_some(),
            "config/feeds + PIN must bind: omitted={:?}",
            load.omitted
        );
        let leaked = leak_index(load);
        assert!(!leaked.flash_handlers.is_empty());
        assert!(leaked.book_handler.is_some());
        assert!(leaked.feed_sub.is_some());
        let subs = leaked.subscribers();
        assert!(!subs.is_empty());
        let router = liq_node::LogRouter::from_subscribers(&subs).unwrap();
        let allowed = committed_addrs(&intern, &reg, &cfg);
        let mut saw_aave = false;
        let mut saw_pool = false;
        let mut saw_feed = false;
        for s in &subs {
            for f in s.subscriptions() {
                assert!(
                    allowed.contains(&f.address),
                    "hardcoded extra {:#x}",
                    f.address
                );
                assert!(
                    router.tracks(f.address),
                    "subscribed {:#x} missing from router filter",
                    f.address
                );
                if reg.protocols.values().any(|p| {
                    matches!(p.market, OnChainId::Addr(a) if a == f.address)
                        && (p.family == "aave-v3" || p.family == "spark")
                }) {
                    saw_aave = true;
                }
                if reg.pools.contains_key(&f.address) {
                    saw_pool = true;
                }
                if intern
                    .feeds()
                    .iter()
                    .any(|r| r.aggregator == f.address || r.proxy == f.address)
                {
                    saw_feed = true;
                }
            }
        }
        assert!(saw_aave, "aave/spark pool must be in the filter");
        assert!(saw_pool, "C1 univ3 pool must be in the filter");
        assert!(saw_feed, "committed feed aggregator must be in the filter");
    }

    #[test]
    fn missing_feeds_omits_group_process_starts() {
        let (intern, reg) = committed();
        let missing = root().join("config/protocols/.17g-missing-feeds");
        let load = load_index(&missing, &intern, &reg);
        assert!(
            load.canonical.is_none(),
            "missing feeds dir must omit feeds: {:?}",
            load.omitted
        );
        assert!(load.omitted.iter().any(|(n, _)| *n == "feeds"));
        let leaked = leak_index(load);
        assert!(leaked.feed_sub.is_none());
        let subs = leaked.subscribers();
        liq_node::LogRouter::from_subscribers(&subs).unwrap();
    }

    #[test]
    fn empty_index_router_starts() {
        let intern = Intern::from_registry(
            &Registry::from_slice(
                br#"{
            "chain_id": 1,
            "generated_at_block": 0,
            "tokens": {},
            "protocols": {},
            "oracles": {},
            "pools": {},
            "flash_sources": {},
            "routers": {}
        }"#,
            )
            .unwrap(),
        )
        .unwrap();
        let reg = Registry::from_slice(
            br#"{
            "chain_id": 1,
            "generated_at_block": 0,
            "tokens": {},
            "protocols": {},
            "oracles": {},
            "pools": {},
            "flash_sources": {},
            "routers": {}
        }"#,
        )
        .unwrap();
        let load = load_index(Path::new("/no/such/17g-config"), &intern, &reg);
        assert!(load.sources.is_empty());
        assert!(load.book.pools().is_empty());
        assert!(load.canonical.is_none());
        let leaked = leak_index(load);
        assert!(leaked.subscriber_empty());
        liq_node::LogRouter::from_subscribers(&leaked.subscribers()).unwrap();
    }

    #[test]
    fn production_index_source_has_no_invented_bid_or_30m() {
        let src = include_str!("index.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        assert!(!prod.contains("BidConfig::new"));
        assert!(!prod.contains("30_000_000") && !prod.contains("30000000"));
        assert!(!prod.contains("gas_failed: 50_000"));
        assert!(!prod.contains("SelectReady"));
    }

    /// Oracle: `getPoolConfigurator()` on each pool's addresses provider,
    /// read on chain (2026-10-04) — the same values `aave-v3.toml` and
    /// `spark.toml` carry. Every committed Aave/Spark flash pool subscribes
    /// to its configurator's premium and reserve-flag logs. The registry
    /// carries no configurator; before the TOML lookup covered `aave-v3.toml`
    /// only Spark's pool had one, and the three Aave V3 pools followed none.
    #[test]
    fn every_aave_flash_pool_follows_its_configurator() {
        use alloy_primitives::{address, b256};
        let (intern, reg) = committed();
        let load = load_index(&root().join("config"), &intern, &reg);
        // `FlashloanPremiumTotalUpdated(uint128,uint128)`.
        let premium = b256!("71aba182c9d0529b516de7a78bed74d49c207ef7e152f52f7ea5d8730138f643");
        for (pool, configurator) in [
            (
                address!("0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2"),
                address!("0x64b761d848206f447fe2dd461b0c635ec39ebb27"),
            ),
            (
                address!("0x4e033931ad43597d96d6bcc25c280717730b58b1"),
                address!("0x342631c6cefc9cfbf97b2fe4aa242a236e1fd517"),
            ),
            (
                address!("0x0aa97c284e98396202b6a04024f5e2c65026f3c0"),
                address!("0x8438f4d29d895d75c86bdc25360c25ef0607e65d"),
            ),
            (
                address!("0xc13e21b648a5ee794902342038ff3adab66be987"),
                address!("0x542dba469bde58faee189ffb60c6b49ce60e0738"),
            ),
        ] {
            let s = load
                .sources
                .iter()
                .find(|s| s.provider() == FlashProvider::Aave && s.source() == pool)
                .unwrap_or_else(|| panic!("{pool:#x} not bound"));
            assert!(
                s.subscriptions()
                    .iter()
                    .any(|f| f.address == configurator && f.topic0 == premium),
                "{pool:#x} does not follow {configurator:#x}"
            );
        }
    }

    /// The registry's flash-source rows bind the singletons the index needs:
    /// Morpho, the V4 PoolManager and Sky's DssFlash with its End. Before
    /// the rows existed all three were omitted and only Aave and V3 pools
    /// could fund a liquidation.
    #[test]
    fn registry_binds_the_flash_singletons() {
        let (intern, reg) = committed();
        let load = load_index(&root().join("config"), &intern, &reg);
        for p in [
            FlashProvider::Morpho,
            FlashProvider::UniV4,
            FlashProvider::SkyDss,
        ] {
            assert_eq!(
                load.sources.iter().filter(|s| s.provider() == p).count(),
                1,
                "{p:?}"
            );
        }
        for group in ["morpho", "univ4", "sky-dss"] {
            assert!(
                !load.omitted.iter().any(|(g, _)| *g == group),
                "{group} omitted"
            );
        }
    }

    #[test]
    fn empty_book_still_empty_until_logs() {
        let (intern, reg) = committed();
        let load = load_index(&root().join("config"), &intern, &reg);
        let mut venues = [0usize; 4];
        for p in load.book.pools() {
            assert!(!p.is_live(), "{:#x} live before seed/logs", p.address);
            match &p.state {
                PoolState::V3(s) => {
                    venues[0] += 1;
                    assert_eq!(s.sqrt_price_x96, U256::ZERO);
                    assert_eq!(s.liquidity, 0);
                    assert!(s.ticks.is_empty());
                }
                PoolState::V2(s) => {
                    venues[1] += 1;
                    assert!(s.reserve0.is_zero() && s.reserve1.is_zero());
                }
                PoolState::Curve(s) => {
                    venues[2] += 1;
                    assert!(s.stale, "curve starts stale until read");
                    assert_eq!(s.rates.len(), p.tokens.len());
                }
                PoolState::Crypto(s) => {
                    venues[3] += 1;
                    assert!(s.stale, "crypto starts stale until read");
                    assert_eq!(s.precisions.len(), p.tokens.len());
                }
                // None are admitted until reviewed; any that is starts stale.
                PoolState::Balancer(s) => {
                    assert!(s.stale, "balancer starts stale until read");
                    assert_eq!(s.scaling.len(), p.tokens.len());
                }
                PoolState::Fluid(s) => {
                    assert!(s.stale, "fluid starts stale until read");
                    assert_eq!(s.tokens.len(), p.tokens.len());
                }
            }
        }
        assert!(
            venues.iter().all(|&n| n > 0),
            "every venue loads: {venues:?}"
        );
    }

    /// Every registry unwrap reaches the book, unread (not routed) until the
    /// reseed thread reads its rate, and each unwraps into a token that has
    /// pools of its own.
    #[test]
    fn registry_unwraps_load_unread() {
        let (intern, reg) = committed();
        let load = load_index(&root().join("config"), &intern, &reg);
        let want = reg.tokens.values().filter(|t| t.unwrap.is_some()).count();
        assert!(want > 0, "registry has unwraps");
        let got: Vec<_> = load.book.unwraps().collect();
        assert_eq!(got.len(), want, "every registry unwrap is in the book");
        for u in got {
            assert!(
                !u.is_live(),
                "{:#x} routed before its rate is read",
                u.wrapper_token
            );
            assert!(
                load.book.pairs().any(|(a, _)| a == u.into),
                "{:#x} unwraps into {:#x}, which has no pool",
                u.wrapper_token,
                u.into_token
            );
            assert!(u.gas > 0, "unwrap gas measured");
        }
    }
}
