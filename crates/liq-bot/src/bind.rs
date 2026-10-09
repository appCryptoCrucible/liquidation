//! WP 17E: bind observed config into the 17D drain.
//!
//! Adapters come from committed TOML + each crate's `Config` / `new`.
//! `LiveRegistryUnasserted` / `EmptyInterned` / `LiveFeesUnasserted` (and
//! Fluid `LiveFactoryUnasserted`) omit that crate — no invented intern.
//! Header gas, wrap gas, WETH, and fee windows are observed or absent.

use std::fs;
use std::path::Path;

use alloy_primitives::{address, Address, B256, U256};
use liq_config::{AaveV3Toml, AaveV4Toml, Intern, MorphoBlueToml};
use liq_engine::Candidate;
use liq_exec::fee::FeeQuote;
use liq_flash::Haircut;
use liq_node::LogHandler;
use liq_plan::ValidateCtx;
use liq_protocol::{
    BlockNum, DecodedLog, DirtySet, ExecutorAdapter, FeedId, MarketRow, Protocol, ProtocolError,
    StateWriter,
};
use liq_router::{GasOracle, TailPins};
use liq_state::StateView;
use liq_types::{AssetId, HaltSink, LogFilter, LogSubscriber, MarketId, ProtocolId};

use crate::index::BoundIndex;
use crate::live_rpc::LiveRpc;

use crate::assemble_view::ProcessAssembleView;

/// Canonical mainnet WETH9 — lookup key into the committed registry, not a
/// fabricated token.
pub const REGISTRY_WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

/// A constructed adapter that passed `new`. Omitted crates are not stored.
pub enum BoundProtocol {
    AaveV3(liq_adapters_aave_v3::AaveV3),
    AaveV4(liq_adapters_aave_v4::AaveV4),
    MorphoBlue(liq_adapters_morpho_blue::MorphoBlue),
    EulerV2(liq_adapters_euler_v2::EulerV2),
    SiloV2(liq_adapters_silo_v2::SiloV2),
    LiquityV2(liq_adapters_liquity_v2::LiquityV2),
    Fluid(liq_adapters_fluid::Fluid),
    Gearbox(liq_adapters_gearbox::GearboxV3),
    CompoundV2(liq_adapters_compound_v2::CompoundV2),
}

impl BoundProtocol {
    #[must_use]
    pub fn as_dyn(&self) -> &dyn Protocol {
        match self {
            Self::AaveV3(p) => p,
            Self::AaveV4(p) => p,
            Self::MorphoBlue(p) => p,
            Self::EulerV2(p) => p,
            Self::SiloV2(p) => p,
            Self::LiquityV2(p) => p,
            Self::Fluid(p) => p,
            Self::Gearbox(p) => p,
            Self::CompoundV2(p) => p,
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
            Self::AaveV4(p) => p.config().assets.iter().map(|a| a.underlying).collect(),
            Self::MorphoBlue(p) => p.config().assets.iter().map(|a| a.underlying).collect(),
            Self::EulerV2(p) => p.config().assets.iter().map(|a| a.underlying).collect(),
            Self::SiloV2(p) => p.config().assets.iter().map(|a| a.underlying).collect(),
            Self::LiquityV2(p) => {
                let cfg = p.config();
                let mut v = vec![cfg.bold.underlying, cfg.weth.underlying];
                v.extend(cfg.branches.iter().map(|b| b.coll_token));
                v
            }
            Self::Fluid(p) => p.config().assets.iter().map(|a| a.underlying).collect(),
            Self::Gearbox(p) => p.config().assets.iter().map(|a| a.underlying).collect(),
            Self::CompoundV2(p) => p.config().assets.iter().map(|a| a.underlying).collect(),
        }
    }

    /// TailPins from adapter market/borrower fields already on the position.
    /// Quote-derived fields stay unset for [`ProcessAssembleView::apply_quote_derived`].
    /// Missing required pin → `None` (no zero tail).
    ///
    /// `view` is the store view at the candidate's block — Liquity reads the
    /// position's [`liq_protocol::PositionExtraRepr`] from it (the trove id); Aave V4 and
    /// Morpho Blue read the position's *market* rows from it (reserve slots,
    /// the Morpho `Id`), since neither is a static config pin: Aave V4's
    /// reserve id is the row's own slot index, and Morpho assigns `Id`s
    /// on-chain at `CreateMarket`, not from a deployment pin. Fluid's
    /// T1/T2-T4 split and Gearbox's facade are market/config properties;
    /// Compound's collateral cToken and `is_cether` come off the quote's own
    /// [`liq_protocol::SlotRef::Contract`] legs (P6: a global `AssetId`
    /// cannot tell cWBTC from cWBTC2, but the quote that named the leg
    /// already can).
    #[must_use]
    pub fn pins_from_candidate(
        &self,
        c: &Candidate,
        view: Option<&StateView<'_>>,
    ) -> Option<TailPins> {
        match self {
            Self::AaveV3(p) => pins_aave_v3(p.config(), c),
            Self::AaveV4(p) => pins_aave_v4(p.config(), c, view),
            Self::MorphoBlue(p) => pins_morpho(p.config(), c, view),
            Self::EulerV2(p) => pins_euler(p.config(), c),
            Self::SiloV2(p) => pins_silo(p.config(), c),
            Self::LiquityV2(p) => pins_liquity(p.config(), c, view),
            Self::Fluid(p) => pins_fluid(p.config(), c, view),
            Self::Gearbox(p) => pins_gearbox(p.config(), c),
            Self::CompoundV2(p) => pins_compound(p.config(), c),
        }
    }
}

impl BoundProtocol {
    /// Plan-validation pins for the leg `pins` describes. `validate` refuses
    /// an unpinned Aave V4 reserve, Morpho market or Liquity trove, so each
    /// one the drain is about to encode is pinned from the same on-chain
    /// state the adapter keeps: V4 reserve rows, the Morpho loan row (its
    /// `MarketParams` must still hash to the market id — `validate` checks),
    /// the Liquity trove. Compound pins are static ([`compound_validate_pins`]).
    pub fn validate_pins(
        &self,
        c: &Candidate,
        pins: &TailPins,
        view: Option<&StateView<'_>>,
        token: &dyn Fn(AssetId) -> Option<Address>,
        out: &mut ValidateCtx,
    ) {
        let (Some(repay), Some(seize)) = (
            c.quote.repay_options.get(usize::from(c.legs.repay)),
            c.quote.seize_options.get(usize::from(c.legs.seize)),
        ) else {
            return;
        };
        match self {
            Self::AaveV4(_) => {
                let ids = [
                    (pins.aave_v4_collateral_reserve_id, seize.asset),
                    (pins.aave_v4_debt_reserve_id, repay.asset),
                ];
                for (id, asset) in ids {
                    if let (Some(reserve_id), Some(underlying)) = (id, token(asset)) {
                        out.add_v4(liq_plan::V4ReservePin {
                            spoke: pins.market,
                            reserve_id,
                            underlying,
                        });
                    }
                }
            }
            Self::MorphoBlue(_) => {
                let (Some(id), Some(loan_token), Some(collateral_token)) = (
                    pins.morpho_market_id,
                    token(repay.asset),
                    token(seize.asset),
                ) else {
                    return;
                };
                let Some(loan) = view
                    .and_then(|v| v.markets(c.quote.key.market).ok())
                    .and_then(|rows| {
                        rows.get(usize::from(liq_adapters_morpho_blue::layout::LOAN_SLOT))
                    })
                    .and_then(|row| {
                        row.body::<liq_adapters_morpho_blue::layout::LoanRow>()
                            .ok()
                            .copied()
                    })
                else {
                    return;
                };
                out.add_morpho(liq_plan::MorphoMarketPin {
                    id,
                    morpho: pins.market,
                    loan_token,
                    collateral_token,
                    oracle: Address::from(loan.oracle),
                    irm: Address::from(loan.irm),
                    lltv: U256::from(loan.lltv),
                });
            }
            Self::LiquityV2(_) => {
                if let Some(trove_id) = pins.liquity_trove_id {
                    out.add_liquity(liq_plan::LiquityTrovePin {
                        trove_manager: pins.market,
                        trove_id,
                        borrower: pins.borrower,
                    });
                }
            }
            Self::AaveV3(_)
            | Self::EulerV2(_)
            | Self::SiloV2(_)
            | Self::Fluid(_)
            | Self::Gearbox(_)
            | Self::CompoundV2(_) => {}
        }
    }
}

/// Family name → protocol id: the registry intern first, then adapters whose
/// id is config-defined rather than registry-interned (Fluid, Gearbox).
#[must_use]
pub fn resolve_family(
    intern: &Intern,
    adapters: &[BoundProtocol],
    family: &str,
) -> Option<ProtocolId> {
    intern.protocol(family).or_else(|| {
        adapters.iter().find_map(|p| match (p, family) {
            (BoundProtocol::Fluid(_), "fluid")
            | (BoundProtocol::Gearbox(_), "gearbox")
            | (BoundProtocol::LiquityV2(_), "liquity-v2") => Some(p.id()),
            (BoundProtocol::AaveV3(a), "aave-v2")
                if a.config().liquidation.version == liq_adapters_aave_v3::AaveVersion::V2 =>
            {
                Some(p.id())
            }
            _ => None,
        })
    })
}

/// Every `(debt cToken, collateral cToken)` pair within one Comptroller,
/// from config. `is_cether` is the *debt* cToken's config pin
/// (`underlying == 0`) — which `liquidateBorrow` the Executor calls.
#[must_use]
pub fn compound_validate_pins(protocols: &[BoundProtocol]) -> Vec<liq_plan::CompoundMarketPin> {
    let mut out = Vec::new();
    for p in protocols {
        let BoundProtocol::CompoundV2(c) = p else {
            continue;
        };
        for fork in &c.config().forks {
            for debt in &fork.ctokens {
                for coll in &fork.ctokens {
                    out.push(liq_plan::CompoundMarketPin {
                        debt_ctoken: debt.ctoken,
                        ctoken_collateral: coll.ctoken,
                        is_cether: u8::from(debt.underlying.is_zero()),
                    });
                }
            }
        }
    }
    out
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

/// Protocol adapters, then flash, book, feeds, derived. Empty groups absent.
#[must_use]
pub fn router_subscribers<'a>(
    protocols: &'a [BoundProtocol],
    index: &'a BoundIndex,
) -> Vec<&'a dyn LogSubscriber> {
    let mut out = subscriber_refs(protocols);
    out.extend(index.subscribers());
    out
}

/// Handlers matching [`router_subscribers`] order. Protocol ids stay adapters.
#[must_use]
pub fn router_handlers(
    protocols: &'static [BoundProtocol],
    index: &'static BoundIndex,
    sink: &'static dyn HaltSink,
) -> Vec<Box<dyn LogHandler + Send>> {
    let mut out = ingest_handlers(protocols);
    out.extend(index.handlers(sink));
    out
}

/// Result of walking `config/protocols/*.toml`.
#[derive(Default)]
pub struct ProtocolLoad {
    pub protocols: Vec<BoundProtocol>,
    pub omitted: Vec<(&'static str, String)>,
}

/// Load each committed protocol file. Missing file or refused `new` → omit, log.
///
/// `live`: a connected [`LiveRpc`] plus the block its callers already pinned
/// the rest of boot against, for the four adapters whose `Config::new`
/// refuses without a live-registry assertion (Liquity, Fluid, Gearbox,
/// Compound V2 — see `push_liquity` et al.). `None` omits all four with a
/// named reason instead of silently never registering them; tests that
/// exercise only the offline TOML/shape paths pass `None` on purpose.
#[must_use]
pub fn load_protocols(
    config_dir: &Path,
    intern: &Intern,
    live: Option<(&LiveRpc, BlockNum)>,
) -> ProtocolLoad {
    let mut out = ProtocolLoad::default();
    let proto_dir = config_dir.join("protocols");
    for name in ["aave-v3.toml", "aave-v4.toml", "morpho-blue.toml"] {
        let path = proto_dir.join(name);
        if !path.is_file() {
            tracing::error!(
                file = name,
                "protocol toml absent — adapter omitted (no invented config)"
            );
            out.omitted.push((name, "toml absent".into()));
        }
    }
    push_spark(&proto_dir, intern, &mut out);
    push_aave_v3(&proto_dir, intern, &mut out);
    push_aave_v2(&proto_dir, &mut out);
    push_aave_v4(&proto_dir, intern, &mut out);
    push_morpho(&proto_dir, intern, &mut out);
    push_euler(&proto_dir, intern, &mut out);
    push_silo(&proto_dir, &mut out);
    push_liquity(&proto_dir, live, &mut out);
    push_fluid(&proto_dir, intern, live, &mut out);
    push_gearbox(&proto_dir, intern, live, &mut out);
    push_compound(&proto_dir, intern, live, &mut out);
    if out.protocols.is_empty() {
        tracing::error!("empty protocol list after TOML load — drain stays a no-op");
    }
    out
}

/// Rebind the live-bound adapters (Liquity, Fluid, Gearbox, Compound V2)
/// that `load` omitted, up to `attempts` times. Their binds make hundreds of
/// `eth_call`s and one failure omits the adapter. Offline omissions (a bad
/// TOML) are kept as they are.
pub fn retry_live_omitted(
    config_dir: &Path,
    intern: &Intern,
    live: Option<(&LiveRpc, BlockNum)>,
    load: &mut ProtocolLoad,
    attempts: u32,
) {
    let proto_dir = config_dir.join("protocols");
    for attempt in 1..=attempts {
        if !load
            .omitted
            .iter()
            .any(|(n, _)| matches!(*n, "liquity-v2" | "fluid" | "gearbox" | "compound-v2"))
        {
            return;
        }
        tracing::warn!(attempt, omitted = ?load.omitted, "rebinding live-bound adapters");
        let mut retry = ProtocolLoad::default();
        for (name, why) in std::mem::take(&mut load.omitted) {
            match name {
                "liquity-v2" => push_liquity(&proto_dir, live, &mut retry),
                "fluid" => push_fluid(&proto_dir, intern, live, &mut retry),
                "gearbox" => push_gearbox(&proto_dir, intern, live, &mut retry),
                "compound-v2" => push_compound(&proto_dir, intern, live, &mut retry),
                other => retry.omitted.push((other, why)),
            }
        }
        load.protocols.append(&mut retry.protocols);
        load.omitted = retry.omitted;
    }
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
    let cfg = match spark_to_config(&toml) {
        Ok(c) => c,
        Err(e) => {
            omit(out, "spark", e);
            return;
        }
    };
    match liq_adapters_aave_v3::AaveV3::new(cfg) {
        Ok(p) => out.protocols.push(BoundProtocol::AaveV3(p)),
        Err(e) => omit(out, "spark", e),
    }
}

/// Aave V2's protocol id: config-defined, after the registry-interned ids
/// and Liquity (9), Fluid (10) and Gearbox (11). V2 is not a registry
/// family: adding one would renumber every interned protocol and market.
pub const AAVE_V2_PROTOCOL: ProtocolId = ProtocolId(12);

/// Aave V2 (`aave-v2.toml`): the Aave V3 adapter running its V2 version.
fn push_aave_v2(dir: &Path, out: &mut ProtocolLoad) {
    let path = dir.join("aave-v2.toml");
    if !path.is_file() {
        omit(out, "aave-v2", "toml absent");
        return;
    }
    let toml = match AaveV3Toml::from_path(&path) {
        Ok(t) => t,
        Err(e) => {
            omit(out, "aave-v2", e);
            return;
        }
    };
    if ProtocolId(toml.protocol) != AAVE_V2_PROTOCOL {
        omit(
            out,
            "aave-v2",
            format!("toml protocol {} != {}", toml.protocol, AAVE_V2_PROTOCOL.0),
        );
        return;
    }
    let cfg = match spark_to_config(&toml) {
        Ok(c) if c.liquidation.version == liq_adapters_aave_v3::AaveVersion::V2 => c,
        Ok(_) => {
            omit(out, "aave-v2", "aave-v2.toml is not `version = \"v2\"`");
            return;
        }
        Err(e) => {
            omit(out, "aave-v2", e);
            return;
        }
    };
    match liq_adapters_aave_v3::AaveV3::new(cfg) {
        Ok(p) => out.protocols.push(BoundProtocol::AaveV3(p)),
        Err(e) => omit(out, "aave-v2", e),
    }
}

fn push_aave_v3(dir: &Path, intern: &Intern, out: &mut ProtocolLoad) {
    let path = dir.join("aave-v3.toml");
    if !path.is_file() {
        omit(out, "aave-v3", "toml absent");
        return;
    }
    let toml = match AaveV3Toml::from_path(&path) {
        Ok(t) => t,
        Err(e) => {
            omit(out, "aave-v3", e);
            return;
        }
    };
    if let Some(want) = intern.protocol("aave-v3") {
        if want != ProtocolId(toml.protocol) {
            omit(
                out,
                "aave-v3",
                format!("toml protocol {} != intern {want:?}", toml.protocol),
            );
            return;
        }
    }
    let cfg = match spark_to_config(&toml) {
        Ok(c) => c,
        Err(e) => {
            omit(out, "aave-v3", e);
            return;
        }
    };
    match liq_adapters_aave_v3::AaveV3::new(cfg) {
        Ok(p) => out.protocols.push(BoundProtocol::AaveV3(p)),
        Err(e) => omit(out, "aave-v3", e),
    }
}

fn push_aave_v4(dir: &Path, intern: &Intern, out: &mut ProtocolLoad) {
    let path = dir.join("aave-v4.toml");
    if !path.is_file() {
        omit(out, "aave-v4", "toml absent");
        return;
    }
    let toml = match AaveV4Toml::from_path(&path) {
        Ok(t) => t,
        Err(e) => {
            omit(out, "aave-v4", e);
            return;
        }
    };
    if let Some(want) = intern.protocol("aave-v4") {
        if want != ProtocolId(toml.protocol) {
            omit(
                out,
                "aave-v4",
                format!("toml protocol {} != intern {want:?}", toml.protocol),
            );
            return;
        }
    }
    let cfg = aave_v4_to_config(&toml);
    match liq_adapters_aave_v4::AaveV4::new(cfg) {
        Ok(p) => out.protocols.push(BoundProtocol::AaveV4(p)),
        Err(e) => omit(out, "aave-v4", e),
    }
}

fn aave_v4_to_config(t: &AaveV4Toml) -> liq_adapters_aave_v4::Config {
    liq_adapters_aave_v4::Config {
        protocol: ProtocolId(t.protocol),
        hubs: t
            .hubs
            .iter()
            .map(|h| liq_adapters_aave_v4::HubConfig {
                address: h.address,
                market: MarketId(h.market),
            })
            .collect(),
        spokes: t
            .spokes
            .iter()
            .map(|s| liq_adapters_aave_v4::SpokeConfig {
                address: s.address,
                market: MarketId(s.market),
                oracle: s.oracle,
            })
            .collect(),
        assets: t
            .assets
            .iter()
            .map(|a| liq_adapters_aave_v4::AssetConfig {
                underlying: a.underlying,
                asset: AssetId(a.asset),
                feed: FeedId(a.feed),
            })
            .collect(),
        price_sources: t
            .price_sources
            .iter()
            .map(|s| liq_adapters_aave_v4::SourcePin {
                spoke: s.spoke,
                reserve_id: s.reserve_id,
                source: s.source,
            })
            .collect(),
        pinned_through: t.pinned_through,
    }
}

fn push_morpho(dir: &Path, intern: &Intern, out: &mut ProtocolLoad) {
    let path = dir.join("morpho-blue.toml");
    if !path.is_file() {
        omit(out, "morpho-blue", "toml absent");
        return;
    }
    let toml = match MorphoBlueToml::from_path(&path) {
        Ok(t) => t,
        Err(e) => {
            omit(out, "morpho-blue", e);
            return;
        }
    };
    if let Some(want) = intern.protocol("morpho-blue") {
        if want != ProtocolId(toml.protocol) {
            omit(
                out,
                "morpho-blue",
                format!("toml protocol {} != intern {want:?}", toml.protocol),
            );
            return;
        }
    }
    let cfg = morpho_to_config(&toml);
    match liq_adapters_morpho_blue::MorphoBlue::new(cfg) {
        Ok(p) => out.protocols.push(BoundProtocol::MorphoBlue(p)),
        Err(e) => omit(out, "morpho-blue", e),
    }
}

fn morpho_to_config(t: &MorphoBlueToml) -> liq_adapters_morpho_blue::Config {
    liq_adapters_morpho_blue::Config {
        protocol: ProtocolId(t.protocol),
        morpho: t.morpho,
        catalog: MarketId(t.catalog),
        first_market: MarketId(t.first_market),
        assets: t
            .assets
            .iter()
            .map(|a| liq_adapters_morpho_blue::AssetConfig {
                underlying: a.underlying,
                asset: AssetId(a.asset),
                feed: FeedId(a.feed),
                decimals: a.decimals,
            })
            .collect(),
        price_sources: t
            .price_sources
            .iter()
            .map(|s| liq_adapters_morpho_blue::SourcePin {
                oracle: s.oracle,
                collateral: AssetId(s.collateral),
                loan: AssetId(s.loan),
            })
            .collect(),
        pinned_through: t.pinned_through,
    }
}

fn spark_to_config(t: &AaveV3Toml) -> Result<liq_adapters_aave_v3::Config, String> {
    let balance_model = match t.liquidation.balance_model.as_str() {
        "token-math-35" => liq_adapters_aave_v3::BalanceModel::TokenMath35,
        "wad-ray-half-up" => liq_adapters_aave_v3::BalanceModel::WadRayHalfUp,
        other => return Err(format!("unknown balance_model {other}")),
    };
    let close_factor_scope = match t.liquidation.close_factor_scope.as_str() {
        "position-base" => liq_adapters_aave_v3::CloseFactorScope::PositionBase,
        "reserve-debt" => liq_adapters_aave_v3::CloseFactorScope::ReserveDebt,
        other => return Err(format!("unknown close_factor_scope {other}")),
    };
    let version = match t.liquidation.version.as_str() {
        "v3" => liq_adapters_aave_v3::AaveVersion::V3,
        "v2" => liq_adapters_aave_v3::AaveVersion::V2,
        other => return Err(format!("unknown aave version {other}")),
    };
    Ok(liq_adapters_aave_v3::Config {
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
                tokens: p.tokens.clone(),
                grace_sentinel: p.grace_sentinel,
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
            balance_model,
            close_factor_scope,
            version,
        },
        pinned_through: t.pinned_through,
    })
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

pub(crate) fn push_silo(dir: &Path, out: &mut ProtocolLoad) {
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
        interest_fee: s.interest_fee,
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
    interest_fee: u128,
}

#[derive(serde::Deserialize)]
struct SiloAssetToml {
    underlying: String,
    asset: u16,
    feed: u16,
    decimals: u8,
}

pub(crate) fn push_liquity(dir: &Path, live: Option<(&LiveRpc, BlockNum)>, out: &mut ProtocolLoad) {
    let Some(raw) = read_toml(dir, "liquity-v2.toml") else {
        omit(out, "liquity-v2", "toml absent");
        return;
    };
    let mut cfg = match liq_adapters_liquity_v2::Config::from_toml(&raw) {
        Ok(c) => c,
        Err(e) => {
            omit(out, "liquity-v2", e);
            return;
        }
    };
    // T13 L2/L2-follow-up. `new` cannot return `Ok` without
    // `live_registry_asserted`, which `from_toml` never sets — the live
    // AddressesRegistry read is what `assert_live_registry` performs, here,
    // against the same block the rest of boot observed. No provider (tests,
    // or a boot that could not reach the RPC) omits with a name distinct
    // from a real on-chain mismatch.
    let Some((rpc, block)) = live else {
        omit(out, "liquity-v2", "no live RPC at bind time");
        return;
    };
    if let Err(e) = cfg.assert_live_registry(rpc, block) {
        omit(out, "liquity-v2", e);
        return;
    }
    match liq_adapters_liquity_v2::LiquityV2::new(cfg) {
        Ok(p) => out.protocols.push(BoundProtocol::LiquityV2(p)),
        Err(e) => omit(out, "liquity-v2", e),
    }
}

pub(crate) fn push_fluid(
    dir: &Path,
    intern: &Intern,
    live: Option<(&LiveRpc, BlockNum)>,
    out: &mut ProtocolLoad,
) {
    let Some(raw) = read_toml(dir, "fluid.toml") else {
        omit(out, "fluid", "toml absent");
        return;
    };
    let mut cfg = match liq_adapters_fluid::Config::from_toml(&raw) {
        Ok(c) => c,
        Err(e) => {
            omit(out, "fluid", e);
            return;
        }
    };
    let Some((rpc, block)) = live else {
        omit(out, "fluid", "no live RPC at bind time");
        return;
    };
    if let Err(e) = cfg.bind_live(rpc, block, &|a| intern.asset(a)) {
        omit(out, "fluid", e);
        return;
    }
    match liq_adapters_fluid::Fluid::new(cfg) {
        Ok(p) => out.protocols.push(BoundProtocol::Fluid(p)),
        Err(e) => omit(out, "fluid", e),
    }
}

pub(crate) fn push_gearbox(
    dir: &Path,
    intern: &Intern,
    live: Option<(&LiveRpc, BlockNum)>,
    out: &mut ProtocolLoad,
) {
    let Some(raw) = read_toml(dir, "gearbox.toml") else {
        omit(out, "gearbox", "toml absent");
        return;
    };
    let mut cfg = match liq_adapters_gearbox::Config::from_toml(&raw) {
        Ok(c) => c,
        Err(e) => {
            omit(out, "gearbox", e);
            return;
        }
    };
    let Some((rpc, block)) = live else {
        omit(out, "gearbox", "no live RPC at bind time");
        return;
    };
    if let Err(e) = cfg.assert_live_registry(rpc, block) {
        omit(out, "gearbox", e);
        return;
    }
    for (manager, why) in &cfg.skipped {
        tracing::warn!(%manager, reason = %why, "gearbox manager skipped");
    }
    if let Err(e) = cfg.bind_assets_from_intern(intern) {
        omit(out, "gearbox", e);
        return;
    }
    let unmapped = cfg
        .managers
        .iter()
        .flat_map(|m| &m.tokens)
        .filter(|t| t.asset == liq_adapters_gearbox::UNMAPPED_ASSET)
        .count();
    if unmapped != 0 {
        tracing::warn!(
            unmapped,
            "gearbox collateral tokens not in the registry — accounts holding them fail closed"
        );
    }
    match liq_adapters_gearbox::GearboxV3::new(cfg) {
        Ok(p) => out.protocols.push(BoundProtocol::Gearbox(p)),
        Err(e) => omit(out, "gearbox", e),
    }
}

fn push_compound(
    dir: &Path,
    intern: &Intern,
    live: Option<(&LiveRpc, BlockNum)>,
    out: &mut ProtocolLoad,
) {
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
    let Some((rpc, block)) = live else {
        omit(out, "compound-v2", "no live RPC at bind time");
        return;
    };
    if let Err(e) = cfg.assert_live_registry(rpc, block) {
        omit(out, "compound-v2", e);
        return;
    }
    match liq_adapters_compound_v2::CompoundV2::new(cfg) {
        Ok(p) => out.protocols.push(BoundProtocol::CompoundV2(p)),
        Err(e) => omit(out, "compound-v2", e),
    }
}

/// Intern registry tokens in intern order. Drift → empty view (no shifted ids).
#[must_use]
pub fn intern_view(intern: &Intern) -> ProcessAssembleView {
    let mut v = ProcessAssembleView::empty();
    for rec in intern.assets() {
        if rec.address.is_zero() {
            tracing::error!(
                asset = rec.id.0,
                "registry token is zero — intern discarded"
            );
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

/// Per-provider wrap gas as `select` charges it (`liq-gas.toml` `[wrap]` +
/// `[tx].base`, see [`crate::gas_model::GasModel::select_wrap`]).
/// `by_provider` is indexed by [`liq_types::FlashProvider`]; `aave_v4` is
/// the V3-flash + V4-adapter figure.
#[derive(Copy, Clone, Debug, Default)]
pub struct WrapGas {
    pub by_provider: [u64; 7],
    pub aave_v4: u64,
}

/// Observed parts that can become [`crate::drain::SelectReady`] when header
/// gas is on the committed block. `gas_failed` stays 0: 10C has wrap
/// snapshots only, no failed-path measurement.
#[derive(Clone, Debug)]
pub struct SelectBind {
    pub wrap_gas: [u64; 7],
    pub wrap_aave_v4: u64,
    pub aave_v4: Option<ProtocolId>,
    pub weth: Address,
    pub validate: ValidateCtx,
    pub haircut: Haircut,
    pub gas_failed: u64,
}

/// Build [`SelectBind`] from 10C wrap + registry WETH. Zero WETH → None.
#[must_use]
pub fn select_bind(
    wrap: WrapGas,
    weth: Address,
    aave_v4: Option<ProtocolId>,
) -> Option<SelectBind> {
    if weth.is_zero() {
        tracing::error!("WETH is zero — SelectReady stays None");
        return None;
    }
    Some(SelectBind {
        wrap_gas: wrap.by_provider,
        wrap_aave_v4: wrap.aave_v4,
        aave_v4,
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

/// Committed `config/bid.toml` only. Missing file, refused cell, or a
/// missing Aave family id → None.
#[must_use]
pub fn load_bid_config(path: &Path, intern: &Intern) -> Option<liq_router::BidSchedule> {
    if !path.is_file() {
        tracing::error!("bid.toml absent — SelectReady stays None (D11 unset)");
        return None;
    }
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "bid.toml unreadable — SelectReady stays None");
            return None;
        }
    };
    let parsed: Result<BidToml, _> = toml::from_str(&raw);
    let file = match parsed {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, "bid.toml malformed — SelectReady stays None");
            return None;
        }
    };
    let size_cut_wei = match file.size_cut_wei.parse::<u128>() {
        Ok(w) if w != 0 => U256::from(w),
        _ => {
            tracing::error!("bid.toml size_cut_wei refused — SelectReady stays None");
            return None;
        }
    };
    let Some(aave_v3) = intern.protocol("aave-v3") else {
        tracing::error!("intern missing aave-v3 — SelectReady stays None");
        return None;
    };
    let Some(aave_v4) = intern.protocol("aave-v4") else {
        tracing::error!("intern missing aave-v4 — SelectReady stays None");
        return None;
    };
    if aave_v3 == aave_v4 {
        tracing::error!("aave-v3 and aave-v4 intern to the same id — SelectReady stays None");
        return None;
    }
    let cell = |s: &BidSection| {
        liq_router::BidConfig::try_from_fields(
            s.beta_cap_bps,
            s.learning_target_bps,
            s.jitter_lo_bps,
            s.jitter_hi_bps,
        )
    };
    let (Some(aave_below), Some(aave_above), Some(other_below), Some(other_above)) = (
        cell(&file.aave.below),
        cell(&file.aave.above),
        cell(&file.other.below),
        cell(&file.other.above),
    ) else {
        tracing::error!("bid.toml cell refused by BidConfig — SelectReady stays None");
        return None;
    };
    Some(liq_router::BidSchedule {
        size_cut_wei,
        aave_v3,
        aave_v4,
        aave_below,
        aave_above,
        other_below,
        other_above,
    })
}

#[derive(serde::Deserialize)]
struct BidToml {
    size_cut_wei: String,
    aave: BidPair,
    other: BidPair,
}

#[derive(serde::Deserialize)]
struct BidPair {
    below: BidSection,
    above: BidSection,
}

#[derive(serde::Deserialize)]
struct BidSection {
    beta_cap_bps: u16,
    learning_target_bps: u16,
    jitter_lo_bps: i16,
    jitter_hi_bps: i16,
}

/// Base fee from the observed header. Priority is [`liq_router::PRIORITY_FEE_WEI`].
/// Missing base fee → None. An empty priority ring does not block the quote.
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
    Some(FeeQuote {
        parent_block,
        next_base_fee,
        priority_wei: liq_router::PRIORITY_FEE_WEI,
        modest_priority_wei: liq_router::PRIORITY_FEE_WEI,
    })
}

/// The reserve id of `asset` in a spoke's market rows: `slot - 1` (slot 0
/// is the spoke meta row, never a reserve).
fn aave_v4_reserve_id(rows: &[MarketRow], asset: AssetId) -> Option<u16> {
    let slot = rows.iter().position(|r| r.asset == asset)?;
    u16::try_from(slot.checked_sub(1)?).ok()
}

fn pins_aave_v4(
    cfg: &liq_adapters_aave_v4::Config,
    c: &Candidate,
    view: Option<&StateView<'_>>,
) -> Option<TailPins> {
    let spoke = cfg.spokes.iter().find(|s| s.market == c.quote.key.market)?;
    if spoke.address.is_zero() || c.quote.key.user.is_zero() {
        tracing::error!("aave-v4 spoke/borrower zero — skip (no zero tail)");
        return None;
    }
    let repay = c.quote.repay_options.get(usize::from(c.legs.repay))?;
    let seize = c.quote.seize_options.get(usize::from(c.legs.seize))?;
    let rows = view.and_then(|v| v.markets(c.quote.key.market).ok());
    let debt_reserve_id = rows.and_then(|r| aave_v4_reserve_id(r, repay.asset));
    let collateral_reserve_id = rows.and_then(|r| aave_v4_reserve_id(r, seize.asset));
    if debt_reserve_id.is_none() || collateral_reserve_id.is_none() {
        tracing::error!(
            pos = c.position.0,
            "aave-v4 reserve id lookup refused — skip (no zero tail)"
        );
        return None;
    }
    let mut p = base_pins(ExecutorAdapter::AaveV4, spoke.address, c.quote.key.user);
    p.aave_v4_debt_reserve_id = debt_reserve_id;
    p.aave_v4_collateral_reserve_id = collateral_reserve_id;
    Some(p)
}

fn pins_morpho(
    cfg: &liq_adapters_morpho_blue::Config,
    c: &Candidate,
    view: Option<&StateView<'_>>,
) -> Option<TailPins> {
    if cfg.morpho.is_zero() || c.quote.key.user.is_zero() {
        tracing::error!("morpho singleton/borrower zero — skip (no zero tail)");
        return None;
    }
    let market_id = view
        .and_then(|v| v.markets(c.quote.key.market).ok())
        .and_then(|rows| rows.get(usize::from(liq_adapters_morpho_blue::layout::LOAN_SLOT)))
        .and_then(|row| row.body::<liq_adapters_morpho_blue::layout::LoanRow>().ok())
        .map(|loan| B256::from(loan.morpho_id))
        .filter(|id| !id.is_zero());
    let Some(market_id) = market_id else {
        tracing::error!(
            pos = c.position.0,
            "morpho market_id lookup refused — skip (no zero tail)"
        );
        return None;
    };
    let mut p = base_pins(ExecutorAdapter::MorphoBlue, cfg.morpho, c.quote.key.user);
    p.morpho_market_id = Some(market_id);
    Some(p)
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

fn pins_liquity(
    cfg: &liq_adapters_liquity_v2::Config,
    c: &Candidate,
    view: Option<&StateView<'_>>,
) -> Option<TailPins> {
    let branch = cfg
        .branches
        .iter()
        .find(|b| b.market == c.quote.key.market)?;
    let trove_id = view
        .and_then(|v| v.position(c.position).ok())
        .and_then(|pos| {
            pos.extra
                .view::<liq_adapters_liquity_v2::layout::TroveExtra>()
                .ok()
        })
        .map(|t| U256::from_be_bytes(t.trove_id));
    let pins = pins_liquity_required(branch.trove_manager, c.quote.key.user, trove_id);
    if pins.is_none() {
        tracing::error!(
            pos = c.position.0,
            "liquity trove_manager/borrower/trove_id refused — skip (no zero tail)"
        );
    }
    pins
}

/// Fluid pins from the vault's row and position in `view`: the vault type,
/// the one-token choice each quoted leg names (its store slot), native
/// sides, the absorb choice, and the quoted liquidation in the vault's own
/// units (shares on a smart side) — what the tail's floors are computed from.
fn pins_fluid(
    cfg: &liq_adapters_fluid::Config,
    c: &Candidate,
    view: Option<&StateView<'_>>,
) -> Option<TailPins> {
    use liq_adapters_fluid::{VaultExtra, VaultRow, NATIVE_TOKEN, VAULT_T1, VAULT_T2, VAULT_T3};
    use liq_plan::{
        FLUID_ABSORB, FLUID_COL_TOKEN1, FLUID_DEBT_TOKEN1, FLUID_NATIVE_COL, FLUID_NATIVE_DEBT,
        FLUID_T1, FLUID_T2, FLUID_T3, FLUID_T4,
    };
    let pin = cfg.pin_by_market(c.quote.key.market)?;
    if pin.vault != c.quote.key.user {
        tracing::error!(pos = c.position.0, "fluid quote names another vault — skip");
        return None;
    }
    let view = view?;
    let rows = view.markets(c.quote.key.market).ok()?;
    let body: &VaultRow = rows.first()?.body().ok()?;
    let extra: VaultExtra = *view.position(c.position).ok()?.extra.view().ok()?;
    let repay = c.quote.repay_options.get(usize::from(c.legs.repay))?;
    let seize = c.quote.seize_options.get(usize::from(c.legs.seize))?;
    let debt_slot = repay.slot.slot()?;
    let col_slot = seize.slot.slot()?;
    let n_col = u16::from(body.n_col);
    let debt_i = debt_slot.checked_sub(n_col)?;
    if col_slot >= n_col || debt_i >= u16::from(body.n_debt) {
        return None;
    }
    let debt_token = if debt_i == 0 {
        pin.borrow0
    } else {
        pin.borrow1
    };
    let col_token = if col_slot == 0 {
        pin.supply0
    } else {
        pin.supply1
    };
    let kind = match pin.vault_type {
        VAULT_T1 => FLUID_T1,
        VAULT_T2 => FLUID_T2,
        VAULT_T3 => FLUID_T3,
        _ => FLUID_T4,
    };
    let mut flags = 0u8;
    if debt_i == 1 {
        flags |= FLUID_DEBT_TOKEN1;
    }
    if col_slot == 1 {
        flags |= FLUID_COL_TOKEN1;
    }
    if extra.flags & VaultExtra::ABSORB != 0 {
        flags |= FLUID_ABSORB;
    }
    if debt_token == NATIVE_TOKEN {
        flags |= FLUID_NATIVE_DEBT;
    }
    if col_token == NATIVE_TOKEN {
        flags |= FLUID_NATIVE_COL;
    }
    if extra.debt_units == 0 || extra.col_units == 0 {
        return None;
    }
    let mut p = base_pins(ExecutorAdapter::Fluid, pin.vault, pin.vault);
    p.fluid = Some(liq_router::FluidPins {
        kind,
        flags,
        debt_units: U256::from(extra.debt_units),
        col_units: U256::from(extra.col_units),
    });
    Some(p)
}

fn pins_gearbox(cfg: &liq_adapters_gearbox::Config, c: &Candidate) -> Option<TailPins> {
    let mgr = cfg
        .managers
        .iter()
        .find(|m| m.market == c.quote.key.market)?;
    if mgr.facade.is_zero() || c.quote.key.user.is_zero() {
        tracing::error!("gearbox facade/borrower zero — skip (no zero tail)");
        return None;
    }
    Some(base_pins(
        ExecutorAdapter::Gearbox,
        mgr.facade,
        c.quote.key.user,
    ))
}

fn pins_compound(cfg: &liq_adapters_compound_v2::Config, c: &Candidate) -> Option<TailPins> {
    let repay = c.quote.repay_options.get(usize::from(c.legs.repay))?;
    let seize = c.quote.seize_options.get(usize::from(c.legs.seize))?;
    let debt_ctoken = repay.slot.contract()?;
    let coll_ctoken = seize.slot.contract()?;
    let is_cether = cfg
        .forks
        .iter()
        .flat_map(|f| &f.ctokens)
        .find(|p| p.ctoken == debt_ctoken)
        .map(|p| p.underlying.is_zero());
    let pins = pins_compound_required(debt_ctoken, c.quote.key.user, Some(coll_ctoken), is_cether);
    if pins.is_none() {
        tracing::error!(
            pos = c.position.0,
            "compound cToken/borrower/is_cether refused — skip (no zero tail)"
        );
    }
    pins
}

fn base_pins(adapter: ExecutorAdapter, market: Address, borrower: Address) -> TailPins {
    TailPins {
        adapter,
        market,
        borrower,
        protocol_pull: None,
        euler_min_yield: None,
        euler_collateral_vault: None,
        liquity_trove_id: None,
        fluid: None,
        fluid_tail: None,
        gearbox_min_seized: None,
        gearbox_full: false,
        compound_ctoken_collateral: None,
        compound_is_cether: None,
        aave_v4_collateral_reserve_id: None,
        aave_v4_debt_reserve_id: None,
        morpho_market_id: None,
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
    use liq_protocol::PositionExtraRepr;
    use liq_router::{AssembleView, GasOracle, PRIORITY_FEE_WEI};

    fn root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    /// The committed protocol tomls bind the adapters that need no live
    /// RPC. Before aave-v3/aave-v4/morpho-blue.toml existed, production
    /// What production wires, read through the real loader from the
    /// committed TOMLs: Spark runs SparkLend's half-up `rayMul` and its
    /// per-reserve close factor and watches its stable debt tokens; Aave V3
    /// runs 3.5 `TokenMath` and the position-wide cap. Both subscribe to
    /// their aTokens' `BalanceTransfer`.
    #[test]
    fn spark_and_aave_v3_are_bound_with_their_own_models_and_tokens() {
        use liq_adapters_aave_v3::{BalanceModel, CloseFactorScope};
        let intern = Intern::from_registry(
            &Registry::from_path(&root().join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        let out = load_protocols(&root().join("config"), &intern, None);
        let spark_pool: alloy_primitives::Address = "0xc13e21b648a5ee794902342038ff3adab66be987"
            .parse()
            .unwrap();
        let spark_dai_stable: alloy_primitives::Address =
            "0xfe2b7a7f4cc0fb76f7fc1c6518d586f1e4559176"
                .parse()
                .unwrap();
        let mut seen = (0, 0);
        for p in &out.protocols {
            let BoundProtocol::AaveV3(a) = p else {
                continue;
            };
            let cfg = a.config();
            // Aave V2 runs on this adapter too (its own test below).
            if cfg.liquidation.version == liq_adapters_aave_v3::AaveVersion::V2 {
                continue;
            }
            if cfg.pools.iter().any(|pl| pl.address == spark_pool) {
                seen.0 += 1;
                assert_eq!(cfg.liquidation.balance_model, BalanceModel::WadRayHalfUp);
                assert_eq!(
                    cfg.liquidation.close_factor_scope,
                    CloseFactorScope::ReserveDebt
                );
                assert!(cfg.pools[0].tokens.contains(&spark_dai_stable));
                assert_eq!(cfg.pools[0].tokens.len(), 60);
            } else {
                seen.1 += 1;
                assert_eq!(cfg.liquidation.balance_model, BalanceModel::TokenMath35);
                assert_eq!(
                    cfg.liquidation.close_factor_scope,
                    CloseFactorScope::PositionBase
                );
                assert!(cfg.pools.iter().all(|pl| !pl.tokens.is_empty()));
            }
            use alloy_sol_types::SolEvent;
            let transfer = liq_adapters_aave_v3::events::token::BalanceTransfer::SIGNATURE_HASH;
            let subs = liq_types::LogSubscriber::subscriptions(a);
            for pl in &cfg.pools {
                for t in &pl.tokens {
                    assert!(subs.iter().any(|f| f.address == *t && f.topic0 == transfer));
                }
            }
        }
        assert_eq!(seen, (1, 1), "one Spark instance, one Aave V3 instance");
    }

    /// omitted all three ("toml absent").
    #[test]
    fn committed_tomls_bind_aave_and_morpho() {
        let intern = Intern::from_registry(
            &Registry::from_path(&root().join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        let out = load_protocols(&root().join("config"), &intern, None);
        for name in [
            "aave-v3",
            "aave-v2",
            "aave-v4",
            "morpho-blue",
            "spark",
            "euler-v2",
        ] {
            assert!(
                !out.omitted.iter().any(|(n, _)| *n == name),
                "{name} omitted: {:?}",
                out.omitted
            );
        }
        let bound = |f: fn(&BoundProtocol) -> bool| out.protocols.iter().filter(|p| f(p)).count();
        assert_eq!(
            bound(|p| matches!(p, BoundProtocol::AaveV3(_))),
            3,
            "Aave V3 + Spark + Aave V2"
        );
        // Aave V2: the V2 version, config-defined id 12, the pool's whole
        // reserve debt, its ETH oracle, its grace sentinel.
        let v2 = out
            .protocols
            .iter()
            .find_map(|p| match p {
                BoundProtocol::AaveV3(a)
                    if a.config().liquidation.version == liq_adapters_aave_v3::AaveVersion::V2 =>
                {
                    Some(a.config())
                }
                _ => None,
            })
            .expect("aave-v2 bound");
        assert_eq!(v2.protocol, AAVE_V2_PROTOCOL);
        assert_eq!(v2.liquidation.close_factor_bps, 10_000);
        assert_eq!(v2.liquidation.oracle_decimals, 18);
        assert_eq!(
            v2.liquidation.close_factor_scope,
            liq_adapters_aave_v3::CloseFactorScope::ReserveDebt
        );
        assert!(!v2.pools[0].grace_sentinel.is_zero());
        assert_eq!(
            resolve_family(&intern, &out.protocols, "aave-v2"),
            Some(AAVE_V2_PROTOCOL)
        );
        assert_eq!(bound(|p| matches!(p, BoundProtocol::AaveV4(_))), 1);
        assert_eq!(bound(|p| matches!(p, BoundProtocol::MorphoBlue(_))), 1);
        // Every registry vault, each priced through its own pinned oracle.
        let euler = out
            .protocols
            .iter()
            .find_map(|p| match p {
                BoundProtocol::EulerV2(e) => Some(e.config()),
                _ => None,
            })
            .expect("euler-v2 bound");
        assert_eq!(euler.vaults.len(), euler.interned.len());
        assert!(!euler.price_sources.is_empty());
    }

    /// Live: Gearbox v3.1 discovery through the address provider loads the
    /// real managers, including the ones holding debt, and maps their
    /// tokens from the committed intern.
    #[test]
    #[ignore = "needs MAINNET_RPC_URL"]
    fn gearbox_v31_discovery_loads_live_managers() {
        let url = std::env::var("MAINNET_RPC_URL").expect("MAINNET_RPC_URL");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let http = liq_config::rpc::HttpRpc::connect(&url).unwrap();
        let block = rt
            .block_on(liq_config::rpc::ChainRpc::block_number(&http))
            .unwrap();
        drop(rt);
        let rpc = LiveRpc::new(http);
        let intern = Intern::from_registry(
            &Registry::from_path(&root().join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        let mut out = ProtocolLoad::default();
        push_gearbox(
            &root().join("config/protocols"),
            &intern,
            Some((&rpc, block)),
            &mut out,
        );
        assert!(out.omitted.is_empty(), "omitted: {:?}", out.omitted);
        let Some(BoundProtocol::Gearbox(g)) = out.protocols.first() else {
            panic!("gearbox not bound");
        };
        let cfg = g.config();
        let mapped = cfg
            .managers
            .iter()
            .flat_map(|m| &m.tokens)
            .filter(|t| t.asset != liq_adapters_gearbox::UNMAPPED_ASSET)
            .count();
        eprintln!(
            "gearbox v3.1: {} registers, {} managers, {mapped} mapped tokens",
            cfg.registers.len(),
            cfg.managers.len()
        );
        assert!(cfg.managers.len() >= 60);
        // The WETH manager with ~749 WETH of debt at 26_048_000.
        assert!(cfg.managers.iter().any(|m| m.manager
            == alloy_primitives::address!("0x79c6c1ce5b12abcc3e407ce8c160ee1160250921")));
        assert!(mapped > 0);
    }

    #[test]
    fn adapter_toml_load_constructs_and_omits_unasserted() {
        let intern = Intern::from_registry(
            &Registry::from_path(&root().join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        // `live: None` — the offline path this test exercises never reaches
        // each adapter's own `LiveRegistryUnasserted`/`LiveFeesUnasserted`;
        // `push_liquity`/`push_fluid`/`push_gearbox`/`push_compound` now
        // short-circuit on "no live RPC at bind time" before ever calling
        // `assert_live_*` or `new`. A real RPC (`Some((rpc, block))`, wired
        // in `startup.rs` from `loaded.config.rpc_url`) is what makes these
        // four adapters construct in production.
        let load = load_protocols(&root().join("config"), &intern, None);
        assert!(
            !load.protocols.is_empty(),
            "at least one real TOML adapter must construct: omitted={:?}",
            load.omitted
        );
        for name in ["liquity-v2", "fluid", "gearbox", "compound-v2"] {
            assert!(
                load.omitted
                    .iter()
                    .any(|(n, w)| { *n == name && w.contains("no live RPC at bind time") }),
                "{name} must be omitted with no live RPC in the offline path: {:?}",
                load.omitted
            );
        }
        assert!(load.protocols.iter().any(|p| matches!(
            p,
            BoundProtocol::AaveV3(_) | BoundProtocol::EulerV2(_) | BoundProtocol::SiloV2(_)
        )));
    }

    #[test]
    fn missing_tail_pin_field_skips_no_zero_tail() {
        let m = address!("0x0000000000000000000000000000000000000051");
        let b = address!("0x00000000000000000000000000000000000000b1");
        assert!(
            pins_compound_required(m, b, None, Some(false)).is_none(),
            "missing cToken must not become a zero tail"
        );
        assert!(pins_compound_required(m, b, Some(Address::ZERO), Some(true)).is_none());
        assert!(pins_liquity_required(m, b, None).is_none());
        assert!(pins_liquity_required(m, b, Some(U256::ZERO)).is_none());
    }

    /// `pins_liquity` decodes `trove_id` with `U256::from_be_bytes`, matching
    /// `liq_adapters_liquity_v2::apply`'s `trove_id.to_be_bytes()` write —
    /// a mismatched endianness here would silently pin every Liquity leg to
    /// the wrong trove.
    #[test]
    fn liquity_trove_id_round_trips_big_endian() {
        let trove_id = U256::from(0x1234_5678_u64);
        let mut extra = PositionExtraRepr::ZERO;
        {
            let view = extra
                .view_mut::<liq_adapters_liquity_v2::layout::TroveExtra>()
                .unwrap();
            view.trove_id = trove_id.to_be_bytes();
        }
        let decoded = extra
            .view::<liq_adapters_liquity_v2::layout::TroveExtra>()
            .unwrap();
        assert_eq!(U256::from_be_bytes(decoded.trove_id), trove_id);
    }

    #[test]
    fn empty_fee_window_is_none() {
        let o = GasOracle::with_priority_cap(4).unwrap();
        assert!(fee_from_oracle(&o, 0).is_none());
    }

    #[test]
    fn observed_base_fee_quotes_one_gwei_without_priority_samples() {
        let mut o = GasOracle::with_priority_cap(4).unwrap();
        o.observe_parent(1_000_000_000, 15_000_000, 30_000_000, &[])
            .unwrap();
        let fee = fee_from_oracle(&o, 9).unwrap();
        assert_eq!(fee.priority_wei, PRIORITY_FEE_WEI);
        assert_eq!(fee.modest_priority_wei, PRIORITY_FEE_WEI);
        assert_eq!(PRIORITY_FEE_WEI, 1_000_000_000);
        assert_ne!(fee.next_base_fee, 0);
        assert_eq!(fee.parent_block, 9);
    }

    #[test]
    fn missing_bid_toml_is_none() {
        let intern = Intern::from_registry(
            &Registry::from_path(&root().join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        assert!(load_bid_config(std::path::Path::new("/no/such/bid.toml"), &intern).is_none());
    }

    #[test]
    fn committed_bid_toml_is_four_cells() {
        let intern = Intern::from_registry(
            &Registry::from_path(&root().join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        let s = load_bid_config(&root().join("config/bid.toml"), &intern).expect("bid.toml");
        assert_eq!(s.aave_v3, intern.protocol("aave-v3").unwrap());
        assert_eq!(s.aave_v4, intern.protocol("aave-v4").unwrap());
        assert_ne!(s.aave_v3, intern.protocol("spark").unwrap());
        assert_ne!(s.aave_v3, intern.protocol("morpho-blue").unwrap());
        assert_eq!(s.aave_below.beta_cap_bps, 9_950);
        assert_eq!(s.aave_above.beta_cap_bps, 9_950);
        assert_eq!(s.other_below.beta_cap_bps, 7_500);
        assert_eq!(s.other_above.beta_cap_bps, 7_500);
        assert_eq!(s.size_cut_wei, U256::from(3_000_000_000_000_000_000u128));
    }

    #[test]
    fn intern_view_matches_registry_weth() {
        let intern = Intern::from_registry(
            &Registry::from_path(&root().join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        let view = intern_view(&intern);
        let weth = registry_weth(&intern).unwrap();
        let id = intern.asset(weth).unwrap();
        assert_eq!(view.token(id), Some(weth));
    }

    #[test]
    fn zero_weth_select_bind_none() {
        assert!(select_bind(
            WrapGas {
                by_provider: [366_332, 355_632, 460_032, 370_435, 384_134, 0, 0],
                aave_v4: 496_704,
            },
            Address::ZERO,
            None,
        )
        .is_none());
    }

    #[test]
    fn loaded_adapters_are_nonempty_subscribers_and_ids_match() {
        let intern = Intern::from_registry(
            &Registry::from_path(&root().join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        let load = load_protocols(&root().join("config"), &intern, None);
        assert!(
            load.protocols
                .iter()
                .any(|p| matches!(p, BoundProtocol::AaveV3(_))),
            "spark must construct: omitted={:?}",
            load.omitted
        );
        assert!(
            load.protocols
                .iter()
                .any(|p| matches!(p, BoundProtocol::EulerV2(_))),
            "euler must construct: omitted={:?}",
            load.omitted
        );
        assert!(
            load.protocols
                .iter()
                .any(|p| matches!(p, BoundProtocol::SiloV2(_))),
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
        let intern = Intern::from_registry(
            &Registry::from_path(&root().join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        let missing = root().join("config/protocols/.17f-empty-omitted");
        let load = load_protocols(&missing, &intern, None);
        assert!(
            load.protocols.is_empty(),
            "omitted dir must not invent adapters: {:?}",
            load.protocols
                .iter()
                .map(BoundProtocol::id)
                .collect::<Vec<_>>()
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
    fn router_concat_keeps_protocol_ids_as_adapters() {
        let intern = Intern::from_registry(
            &Registry::from_path(&root().join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        let load = load_protocols(&root().join("config"), &intern, None);
        let leaked = leak_protocols(load);
        let index = crate::index::leak_index(crate::index::load_index(
            &root().join("config"),
            &intern,
            &Registry::from_path(&root().join("registry/registry.json")).unwrap(),
        ));
        let subs = router_subscribers(leaked, index);
        assert!(
            subs.len() > leaked.len(),
            "flash/book/feeds must concat after adapters"
        );
        let ids = protocol_ids(leaked);
        assert_eq!(ids.len(), leaked.len());
        for (id, p) in ids.iter().zip(leaked.iter()) {
            assert_eq!(*id, p.id());
        }
        let handlers = router_handlers(leaked, index, crate::shared::leak_risk());
        assert_eq!(handlers.len(), subs.len());
        liq_node::LogRouter::from_subscribers(&subs).unwrap();
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod prune_filter {
    use super::*;
    use liq_config::{Intern, Registry};

    /// Every address a bound adapter folds logs from must keep its receipts
    /// on the pruned node, or the first-start state replay cannot see its
    /// history. Binds everything as production does (live RPC for the
    /// adapters that read chain at bind) and writes, under `tools/d15/`:
    ///
    /// * `subscribed_addresses.txt` — every subscribed address and its protocol;
    /// * `receipts_log_filter.bot.toml` — the committed filter
    ///   (`d15_receipts_log_filter.complete.toml`) plus each subscribed address
    ///   it lacks, kept from genesis (`before = 0`).
    ///
    /// Run it right before provisioning the node: the reth `[prune]` config
    /// takes the generated file.
    #[test]
    #[ignore = "needs MAINNET_RPC_URL; writes tools/d15"]
    fn live_generate_the_prune_filter_from_the_adapters() {
        let url = std::env::var("MAINNET_RPC_URL").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let http = liq_config::rpc::HttpRpc::connect(&url).unwrap();
        let block = rt
            .block_on(liq_config::rpc::ChainRpc::block_number(&http))
            .unwrap();
        drop(rt);
        let rpc = LiveRpc::new(http);
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let intern = Intern::from_registry(
            &Registry::from_path(&root.join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        let live = Some((&rpc, block));
        let mut load = load_protocols(&root.join("config"), &intern, live);
        // A public endpoint drops the odd `eth_call` in a bind of hundreds.
        retry_live_omitted(&root.join("config"), &intern, live, &mut load, 5);
        assert!(load.omitted.is_empty(), "omitted: {:?}", load.omitted);
        let base =
            std::fs::read_to_string(root.join("tools/d15/d15_receipts_log_filter.complete.toml"))
                .unwrap();
        let lower = base.to_lowercase();
        let mut subscribed: Vec<(Address, ProtocolId)> = Vec::new();
        for p in &load.protocols {
            for f in p.subscriptions() {
                if !subscribed.iter().any(|(a, _)| *a == f.address) {
                    subscribed.push((f.address, p.id()));
                }
            }
        }
        let missing: Vec<&(Address, ProtocolId)> = subscribed
            .iter()
            .filter(|(a, _)| !lower.contains(&format!("\"{a:#x}\"")))
            .collect();
        let list: String = subscribed
            .iter()
            .map(|(a, p)| format!("{a:#x} {}\n", p.0))
            .collect();
        std::fs::write(root.join("tools/d15/subscribed_addresses.txt"), list).unwrap();
        let mut merged = format!(
            "# receipts_log_filter for the pruned node, generated by\n\
             # liq-bot bind::prune_filter::live_generate_the_prune_filter_from_the_adapters\n\
             # at block {block}: the committed D15 filter plus every address a bound\n\
             # adapter subscribes to ({} total, {} added), all kept from genesis.\n\n",
            subscribed.len(),
            missing.len()
        );
        merged.push_str(&base);
        if !merged.ends_with('\n') {
            merged.push('\n');
        }
        merged.push_str("\n# Added from the live adapter subscriptions.\n");
        for (a, p) in &missing {
            merged.push_str(&format!(
                "\"{a:#x}\" = {{ before = 0 }}  # protocol {}\n",
                p.0
            ));
        }
        std::fs::write(root.join("tools/d15/receipts_log_filter.bot.toml"), merged).unwrap();
        eprintln!(
            "block {block}: {} subscribed addresses, {} added to the filter",
            subscribed.len(),
            missing.len()
        );
    }

    /// Live: the generated Compound config binds against the chain, with
    /// the forks the frozen-market rule brought in. The chain is the
    /// oracle: a frozen pin survives the bind only while its
    /// `accrueInterest()` reverts there, and every pinned cToken's own
    /// `underlying()` and seize share are read.
    #[test]
    #[ignore = "needs MAINNET_RPC_URL"]
    fn live_compound_binds_frozen_and_copycat_forks() {
        use alloy_primitives::Address;
        let url = std::env::var("MAINNET_RPC_URL").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let rpc = liq_config::rpc::HttpRpc::connect(&url).unwrap();
        let block = rt
            .block_on(liq_config::rpc::ChainRpc::block_number(&rpc))
            .unwrap();
        let live = crate::live_rpc::LiveRpc::new(liq_config::rpc::HttpRpc::connect(&url).unwrap());
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let mut out = ProtocolLoad::default();
        push_compound(
            &root.join("config/protocols"),
            &intern,
            Some((&live, block)),
            &mut out,
        );
        assert!(out.omitted.is_empty(), "omitted: {:?}", out.omitted);
        let Some(BoundProtocol::CompoundV2(p)) = out.protocols.first() else {
            panic!("compound not bound");
        };
        let a = |s: &str| s.parse::<Address>().unwrap();
        let fork = |c: &str| p.config().forks.iter().find(|f| f.comptroller == a(c));
        let cream = fork("0x3d5bc3c8d13dcb8bf317092d84783c2697ae9258").expect("Cream v1 bound");
        let cr_cream = a("0x892b14321a4fcba80669ae30bd0cd99a7ecf6ac0");
        assert!(
            cream
                .ctokens
                .iter()
                .any(|c| c.ctoken == cr_cream && c.frozen),
            "crCREAM frozen"
        );
        assert!(cream.ctokens.len() >= 90);
        let inverse = fork("0x4dcf7407ae5c07f8681e1659f626e114a7667339").expect("Inverse bound");
        let xinv = a("0x1637e4e9941d55703a7a5e7807d6ada3f7dcd61b");
        assert!(
            inverse.ctokens.iter().all(|c| c.ctoken != xinv),
            "xINV is not Compound's code"
        );
        for c in [
            "0x606246e9ef6c70dcb6cee42136cd06d127e2b7c7",
            "0xf41ae30d269cd73a2dfaed3ee5496318ebd470b6",
        ] {
            assert!(fork(c).is_some(), "{c} bound");
        }
        // Copycats whose own close factor or incentive is zero: a
        // liquidation there repays or seizes nothing.
        for c in [
            "0x1457b6bebffbb4cd2b4442a063ecd945dc0d6b70",
            "0x7f9a61682e1aa0e45b3dd95860c17a1640d37c14",
            "0x1b72ee0ad58b25c8645728aa5bacfc4fe5f082dd",
            "0xaa86979f14545f86e4368764a99a86ba5fede477",
        ] {
            assert!(fork(c).is_none(), "{c} has nothing to liquidate");
        }
        // DeFiPie: pTokens behind `ProxyWithRegistry`, the five-field accrual.
        let pie = fork("0x36de5bbc618a04c9b471208ef52ee2b1f536e92d").expect("DeFiPie bound");
        assert_eq!(pie.ctokens.len(), 30);
        let frozen = p
            .config()
            .forks
            .iter()
            .flat_map(|f| &f.ctokens)
            .filter(|c| c.frozen)
            .count();
        eprintln!(
            "compound block {block}: {} forks, {} cTokens, {frozen} frozen",
            p.config().forks.len(),
            p.config()
                .forks
                .iter()
                .map(|f| f.ctokens.len())
                .sum::<usize>()
        );
    }
}
