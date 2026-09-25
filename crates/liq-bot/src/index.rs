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
use liq_router::{CurveState, Pool, PoolBook, PoolState, V2State, V3State};
use liq_types::{FlashProvider, HaltSink, LogFilter, LogSubscriber};
use parking_lot::{Mutex, RwLock};
use smallvec::SmallVec;

/// Canonical DAI — lookup key into the committed intern, not a fabricated token.
const REGISTRY_DAI: Address =
    alloy_primitives::address!("0x6B175474E89094C44Da98b954EedeAC495271d0F");

/// Uniswap V3 factory `enableFeeAmount` mapping. Fee is committed on each
/// `PoolEntry`; unknown fees are omitted (no guessed spacing).
fn univ3_tick_spacing(fee: u32) -> Option<i32> {
    match fee {
        100 => Some(1),
        500 => Some(10),
        3000 => Some(60),
        10_000 => Some(200),
        _ => None,
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

fn spark_configurator(config_dir: &Path, pool: Address) -> Option<Address> {
    let path = config_dir.join("protocols/spark.toml");
    let toml = AaveV3Toml::from_path(&path).ok()?;
    toml.pools
        .iter()
        .find(|p| p.address == pool)
        .map(|p| p.configurator)
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
        if p.venue != PoolVenue::Univ3 || p.factory.is_zero() {
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
        self.book.write().apply_log(log);
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
            && self.book_handler.is_none()
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
        let book_handler = {
            let subs = book.read().subscriptions();
            if subs.iter().any(|f| !f.address.is_zero()) {
                Some(BookHandler {
                    book: Arc::clone(&book),
                })
            } else {
                None
            }
        };
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

fn wrap_of(wrap: &[u64; 5], p: FlashProvider) -> u64 {
    wrap.get(p as usize).copied().unwrap_or(0)
}

fn load_flash(
    config_dir: &Path,
    intern: &Intern,
    registry: &Registry,
    wrap: &[u64; 5],
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
            extra_addr(proto, "configurator").or_else(|| spark_configurator(config_dir, pool));
        if configurator.is_none() {
            tracing::error!(
                family = proto.family.as_str(),
                pool = %pool,
                "aave configurator missing — premium/reserve-flag logs unsubscribed"
            );
        }
        sources.push(Box::new(
            AavePool::new(pool, configurator.unwrap_or(Address::ZERO), 0, &[])
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
    if hops.univ3 == 0 || hops.univ2 == 0 || hops.curve == 0 {
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
    let factory = univ3_factory(registry);
    let mut book = PoolBook::new(assets, factory, hops.univ3);
    for (addr, entry) in &registry.pools {
        let tokens: SmallVec<[Address; liq_router::MAX_COINS]> = match entry.venue {
            PoolVenue::Curve => entry.coins.iter().copied().collect(),
            PoolVenue::Univ3 | PoolVenue::Univ2 => {
                SmallVec::from_slice(&[entry.token0, entry.token1])
            }
        };
        if addr.is_zero()
            || tokens.len() < 2
            || tokens.len() > liq_router::MAX_COINS
            || tokens.iter().any(|t| t.is_zero())
        {
            omit(omitted, "book", format!("zero address on pool {addr:#x}"));
            continue;
        }
        let mut ids = SmallVec::new();
        let mut rates = SmallVec::new();
        for t in &tokens {
            let Some(id) = intern.asset(*t) else {
                break;
            };
            ids.push(id);
            if entry.venue == PoolVenue::Curve {
                match registry.tokens.get(t).and_then(|e| curve_rate(e.decimals)) {
                    Some(r) => rates.push(r),
                    None => break,
                }
            }
        }
        if ids.len() != tokens.len()
            || (entry.venue == PoolVenue::Curve && rates.len() != tokens.len())
        {
            omit(
                omitted,
                "book",
                format!("pool {addr:#x} has a coin not interned / no decimals"),
            );
            continue;
        }
        let (hop_gas, state) = match entry.venue {
            PoolVenue::Univ3 => {
                let Some(spacing) = univ3_tick_spacing(entry.fee) else {
                    omit(
                        omitted,
                        "book",
                        format!("unknown univ3 fee {} on {addr:#x}", entry.fee),
                    );
                    continue;
                };
                (
                    hops.univ3,
                    PoolState::V3(V3State {
                        sqrt_price_x96: U256::ZERO,
                        tick: 0,
                        liquidity: 0,
                        fee_pips: entry.fee,
                        tick_spacing: spacing,
                        ticks: Vec::new(),
                    }),
                )
            }
            PoolVenue::Univ2 => {
                let Some(factory) = v2_factory_id(entry.factory) else {
                    omit(
                        omitted,
                        "book",
                        format!(
                            "v2 pair {addr:#x} factory {:#x} not verifiable",
                            entry.factory
                        ),
                    );
                    continue;
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
                }),
            ),
        };
        let pool = Pool {
            address: *addr,
            assets: ids,
            tokens,
            hop_gas,
            state,
        };
        if let Err(e) = book.add(pool) {
            tracing::error!(error = ?e, pool = %addr, "pool address seed refused");
        }
    }
    if book.pools().is_empty() {
        tracing::error!("PoolBook has no seeded addresses — empty publish until logs");
    }
    book
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
        if let Ok(t) = AaveV3Toml::from_path(&config_dir.join("protocols/spark.toml")) {
            for p in t.pools {
                a.insert(p.address);
                a.insert(p.configurator);
                a.insert(p.oracle);
                a.insert(p.provider);
            }
        }
        a
    }

    #[test]
    fn loaded_flash_book_feed_addrs_are_registry_or_config() {
        let (intern, reg) = committed();
        let cfg = root().join("config");
        let load = load_index(&cfg, &intern, &reg);
        assert!(
            load.omitted.iter().any(|(n, _)| *n == "univ4"),
            "univ4 must omit without a committed PoolManager: {:?}",
            load.omitted
        );
        assert!(load.omitted.iter().any(|(n, _)| *n == "morpho"));
        assert!(load.omitted.iter().any(|(n, _)| *n == "sky-dss"));
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

    #[test]
    fn empty_book_still_empty_until_logs() {
        let (intern, reg) = committed();
        let load = load_index(&root().join("config"), &intern, &reg);
        let mut venues = [0usize; 3];
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
            }
        }
        assert!(
            venues.iter().all(|&n| n > 0),
            "every venue loads: {venues:?}"
        );
    }
}
