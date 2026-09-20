//! Per-(protocol, market, asset) feed registry (GUIDE 06 §2).
//!
//! TOML under `config/feeds/`. Every aggregator address is the registry's
//! `oracles[proxy].aggregator`. A TOML row that disagrees, or a resolved
//! registry oracle with no TOML row, refuses to start.

use crate::{OracleError, Result};
use alloy_primitives::Address;
use figment::providers::{Format, Toml};
use figment::Figment;
use liq_config::rpc::ChainRpc;
use liq_config::validate::Validate;
use liq_config::{Intern, OnChainId, OracleEntry, Registry};
use liq_protocol::FeedId;
use liq_types::{AssetId, LogFilter, LogSubscriber, MarketId, ProtocolId};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

/// Chainlink push vs SVR (GUIDE 06 §2 `mechanism`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mechanism {
    ChainlinkSvr,
    ChainlinkPush,
}

/// One TOML `[[feed]]` row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedSpec {
    pub protocol: String,
    pub market: OnChainId,
    pub asset: Address,
    pub mechanism: Mechanism,
    /// `getSourceOfAsset` / protocol-facing source. Registry `oracles` key.
    pub proxy: Address,
    #[serde(default)]
    pub svr_aggregator: Option<Address>,
    #[serde(default)]
    pub standard_aggregator: Option<Address>,
    pub decimals: u8,
    pub heartbeat_secs: u32,
    pub deviation_bps: u16,
    #[serde(default)]
    pub sources: Vec<String>,
}

impl FeedSpec {
    /// Aggregators this row watches. Registry-backed only; zero is invalid.
    pub fn watch(&self) -> Result<SmallVec<[Address; 2]>> {
        let mut out = SmallVec::new();
        for a in [self.standard_aggregator, self.svr_aggregator]
            .into_iter()
            .flatten()
        {
            if a.is_zero() {
                return Err(OracleError::ZeroAggregator { aggregator: a });
            }
            if !out.contains(&a) {
                out.push(a);
            }
        }
        if out.is_empty() {
            return Err(OracleError::NoAggregator { proxy: self.proxy });
        }
        Ok(out)
    }

    fn configured_aggregator(&self) -> Result<Address> {
        match self.mechanism {
            Mechanism::ChainlinkPush => self
                .standard_aggregator
                .ok_or(OracleError::NoAggregator { proxy: self.proxy }),
            Mechanism::ChainlinkSvr => self
                .svr_aggregator
                .ok_or(OracleError::NoAggregator { proxy: self.proxy }),
        }
    }
}

/// Loaded `config/feeds/*.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeedsConfig {
    pub feeds: Vec<FeedSpec>,
}

/// Registry oracle that could not become a feed. Logged; not an invented row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedFailure {
    pub proxy: Address,
    pub pair: String,
    pub reason: String,
}

/// One registry oracle that resolved to an asset + the markets that list it.
#[derive(Clone, Debug)]
pub struct RegistryOracle {
    pub proxy: Address,
    pub entry: OracleEntry,
    pub family: String,
    pub asset: Address,
    pub markets: Vec<OnChainId>,
}

#[derive(Deserialize)]
struct FeedsFile {
    #[serde(default)]
    feed: Vec<FeedSpec>,
}

/// Load every `*.toml` under `dir` via figment (00D). Missing dir or a file
/// with no `[[feed]]` is a load error.
pub fn load(dir: &Path) -> Result<FeedsConfig> {
    if !dir.is_dir() {
        return Err(OracleError::Load(format!(
            "feeds dir missing: {}",
            dir.display()
        )));
    }
    let mut feeds = Vec::new();
    let mut found = false;
    let rd = std::fs::read_dir(dir).map_err(|e| OracleError::Load(e.to_string()))?;
    let mut paths: Vec<_> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    paths.sort();
    for path in paths {
        found = true;
        let file: FeedsFile = Figment::new()
            .merge(Toml::file(&path))
            .extract()
            .map_err(|e| OracleError::Load(format!("{}: {e}", path.display())))?;
        if file.feed.is_empty() {
            return Err(OracleError::Load(format!(
                "{} has no [[feed]] rows",
                path.display()
            )));
        }
        feeds.extend(file.feed);
    }
    if !found {
        return Err(OracleError::Load(format!("no *.toml in {}", dir.display())));
    }
    Ok(FeedsConfig { feeds })
}

impl FeedsConfig {
    pub fn load(dir: &Path) -> Result<Self> {
        load(dir)
    }

    /// Config aggregator/decimals/mechanism must equal the registry. Every
    /// resolved registry oracle must have a TOML row. Fail closed.
    pub fn assert_against_registry(&self, reg: &Registry) -> Result<()> {
        let (resolved, failures) = resolve_registry(reg);
        for f in &failures {
            tracing::error!(
                proxy = %f.proxy,
                pair = %f.pair,
                reason = %f.reason,
                "registry oracle did not resolve to a feed"
            );
        }
        let mut want: HashSet<(String, OnChainId, Address, Address)> = HashSet::new();
        for r in &resolved {
            for m in &r.markets {
                want.insert((r.family.clone(), *m, r.asset, r.proxy));
            }
        }
        for spec in &self.feeds {
            if spec.heartbeat_secs == 0 {
                return Err(OracleError::ZeroHeartbeat { proxy: spec.proxy });
            }
            if spec.deviation_bps == 0 {
                return Err(OracleError::ZeroDeviation { proxy: spec.proxy });
            }
            let entry = reg
                .oracles
                .get(&spec.proxy)
                .ok_or(OracleError::ProxyNotInRegistry { proxy: spec.proxy })?;
            let found = spec.configured_aggregator()?;
            if found != entry.aggregator {
                return Err(OracleError::AggregatorMismatch {
                    proxy: spec.proxy,
                    expected: entry.aggregator,
                    found,
                });
            }
            if spec.decimals != entry.decimals {
                return Err(OracleError::DecimalsMismatch {
                    proxy: spec.proxy,
                    expected: entry.decimals,
                    found: spec.decimals,
                });
            }
            let svr = matches!(spec.mechanism, Mechanism::ChainlinkSvr);
            if svr != entry.svr {
                return Err(OracleError::Load(format!(
                    "mechanism/svr disagree for proxy {:#x}",
                    spec.proxy
                )));
            }
            let _ = spec.watch()?;
            if !want.remove(&(spec.protocol.clone(), spec.market, spec.asset, spec.proxy)) {
                return Err(OracleError::Load(format!(
                    "TOML feed ({}, {:?}, {:#x}) is not a resolved registry oracle",
                    spec.protocol, spec.market, spec.asset
                )));
            }
        }
        if let Some((fam, _m, _a, proxy)) = want.iter().next() {
            let pair = reg
                .oracles
                .get(proxy)
                .map(|e| e.pair.clone())
                .unwrap_or_default();
            return Err(OracleError::MissingToml {
                proxy: *proxy,
                pair: format!("{fam}/{pair}"),
            });
        }
        Ok(())
    }
}

impl Validate for FeedsConfig {
    async fn validate<R: ChainRpc + Sync>(&self, _rpc: &R) -> liq_config::Result<()> {
        if self.feeds.is_empty() {
            return Err(liq_config::ConfigError::Load(
                "feeds config is empty".into(),
            ));
        }
        Ok(())
    }
}

/// Walk `registry.oracles`. Pair `family/0xprefix` → unique token + markets
/// that list the proxy. Unresolved rows are failures, never guessed assets.
#[must_use]
pub fn resolve_registry(reg: &Registry) -> (Vec<RegistryOracle>, Vec<FeedFailure>) {
    let mut ok = Vec::new();
    let mut fail = Vec::new();
    for (proxy, entry) in &reg.oracles {
        match resolve_one(reg, *proxy, entry) {
            Ok(r) => ok.push(r),
            Err(reason) => {
                tracing::error!(proxy = %proxy, pair = %entry.pair, %reason, "oracle resolve failed");
                fail.push(FeedFailure {
                    proxy: *proxy,
                    pair: entry.pair.clone(),
                    reason,
                });
            }
        }
    }
    (ok, fail)
}

fn resolve_one(
    reg: &Registry,
    proxy: Address,
    entry: &OracleEntry,
) -> Result<RegistryOracle, String> {
    let (family, prefix) = split_pair(&entry.pair)
        .ok_or_else(|| format!("pair {} is not family/0xprefix", entry.pair))?;
    let mut assets = Vec::new();
    for addr in reg.tokens.keys() {
        let hex = format!("{addr:#x}");
        if hex.starts_with(prefix) {
            assets.push(*addr);
        }
    }
    let asset = match assets.as_slice() {
        [one] => *one,
        [] => {
            return Err(format!(
                "pair prefix {prefix} does not resolve to a registry token"
            ))
        }
        _ => {
            return Err(format!(
                "pair prefix {prefix} matches {} tokens",
                assets.len()
            ))
        }
    };
    let mut markets = Vec::new();
    for proto in reg.protocols.values() {
        if proto.family != family {
            continue;
        }
        if proto.oracle_adapters.contains(&proxy) {
            markets.push(proto.market);
        }
    }
    if markets.is_empty() {
        return Err("proxy is not listed on any protocol market".into());
    }
    Ok(RegistryOracle {
        proxy,
        entry: entry.clone(),
        family: family.to_string(),
        asset,
        markets,
    })
}

fn split_pair(pair: &str) -> Option<(&str, &str)> {
    let (fam, prefix) = pair.split_once('/')?;
    if fam.is_empty() || !prefix.starts_with("0x") || prefix.len() < 4 {
        return None;
    }
    Some((fam, prefix))
}

/// Interned, validated feed set. Implements [`LogSubscriber`] for every
/// aggregator `AnswerUpdated` and each protocol `price_oracle` source-swap.
#[derive(Clone, Debug)]
pub struct FeedSet {
    pub specs: Vec<ResolvedFeed>,
    aggregators: Vec<Address>,
    source_oracles: Vec<Address>,
}

/// One interned feed row.
#[derive(Clone, Debug)]
pub struct ResolvedFeed {
    pub spec: FeedSpec,
    pub id: FeedId,
    pub asset: AssetId,
    pub protocol: ProtocolId,
    pub market: MarketId,
    pub watch: SmallVec<[Address; 2]>,
}

impl FeedSet {
    /// Bind TOML + intern after [`FeedsConfig::assert_against_registry`].
    pub fn bind(cfg: &FeedsConfig, intern: &Intern, reg: &Registry) -> Result<Self> {
        cfg.assert_against_registry(reg)?;
        let mut specs = Vec::with_capacity(cfg.feeds.len());
        let mut aggs = BTreeSet::new();
        let mut families = BTreeSet::new();
        for spec in &cfg.feeds {
            let watch = spec.watch()?;
            for a in &watch {
                aggs.insert(*a);
            }
            families.insert(spec.protocol.as_str());
            let asset = intern
                .asset(spec.asset)
                .ok_or(OracleError::InternAsset { asset: spec.asset })?;
            let protocol = intern
                .protocol(&spec.protocol)
                .ok_or_else(|| OracleError::InternProtocol(spec.protocol.clone()))?;
            let market = intern
                .markets()
                .iter()
                .find(|m| m.protocol == protocol && m.key == spec.market)
                .map(|m| m.id)
                .ok_or_else(|| OracleError::InternMarket {
                    market: format!("{:?}", spec.market),
                })?;
            let id = intern
                .feed(spec.proxy)
                .ok_or(OracleError::InternFeed { proxy: spec.proxy })?;
            specs.push(ResolvedFeed {
                spec: spec.clone(),
                id,
                asset,
                protocol,
                market,
                watch,
            });
        }
        let source_oracles = price_oracles(reg, &families);
        Ok(Self {
            specs,
            aggregators: aggs.into_iter().collect(),
            source_oracles,
        })
    }
}

fn price_oracles(reg: &Registry, families: &BTreeSet<&str>) -> Vec<Address> {
    let mut out = BTreeSet::new();
    for proto in reg.protocols.values() {
        if !families.contains(proto.family.as_str()) {
            continue;
        }
        if let Some(v) = proto.extra.get("price_oracle") {
            if let Some(s) = v.as_str() {
                if let Ok(a) = s.parse::<Address>() {
                    if !a.is_zero() {
                        out.insert(a);
                    }
                }
            }
        }
    }
    out.into_iter().collect()
}

alloy_sol_types::sol! {
    interface IAggregator {
        event AnswerUpdated(int256 indexed current, uint256 indexed roundId, uint256 updatedAt);
    }
    interface IAaveOracle {
        event AssetSourceUpdated(address indexed asset, address indexed source);
        function getSourceOfAsset(address asset) external view returns (address);
    }
}

/// `AnswerUpdated(int256,uint256,uint256)` topic0.
pub fn answer_updated_topic0() -> alloy_primitives::B256 {
    use alloy_sol_types::SolEvent;
    IAggregator::AnswerUpdated::SIGNATURE_HASH
}

/// V3 `AssetSourceUpdated` topic0 (03C carry-forward: fail closed on swap).
pub fn asset_source_updated_topic0() -> alloy_primitives::B256 {
    use alloy_sol_types::SolEvent;
    IAaveOracle::AssetSourceUpdated::SIGNATURE_HASH
}

impl LogSubscriber for FeedSet {
    fn subscriptions(&self) -> Vec<LogFilter> {
        let t_ans = answer_updated_topic0();
        let t_src = asset_source_updated_topic0();
        let n = self
            .aggregators
            .len()
            .saturating_add(self.source_oracles.len());
        let mut out = Vec::with_capacity(n);
        for a in &self.aggregators {
            out.push(LogFilter {
                address: *a,
                topic0: t_ans,
            });
        }
        for o in &self.source_oracles {
            out.push(LogFilter {
                address: *o,
                topic0: t_src,
            });
        }
        out
    }
}

/// Re-read `getSourceOfAsset` for every feed that has a protocol `price_oracle`.
pub async fn assert_protocol_sources<R: ChainRpc + Sync>(
    cfg: &FeedsConfig,
    reg: &Registry,
    rpc: &R,
) -> Result<()> {
    use alloy_sol_types::SolCall;
    let mut oracle_by_market: HashMap<(String, OnChainId), Address> = HashMap::new();
    for proto in reg.protocols.values() {
        if let Some(v) = proto.extra.get("price_oracle") {
            if let Some(s) = v.as_str() {
                if let Ok(a) = s.parse::<Address>() {
                    oracle_by_market.insert((proto.family.clone(), proto.market), a);
                }
            }
        }
    }
    for spec in &cfg.feeds {
        let Some(oracle) = oracle_by_market.get(&(spec.protocol.clone(), spec.market)) else {
            continue;
        };
        let data = alloy_primitives::Bytes::from(
            IAaveOracle::getSourceOfAssetCall { asset: spec.asset }.abi_encode(),
        );
        let raw = rpc.call(*oracle, data).await?;
        let found = IAaveOracle::getSourceOfAssetCall::abi_decode_returns_validate(&raw)
            .map_err(|_| OracleError::Load(format!("getSourceOfAsset decode at {oracle:#x}")))?;
        if found != spec.proxy {
            return Err(OracleError::ProtocolOracleMismatch {
                asset: spec.asset,
                expected: spec.proxy,
                found,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{load, resolve_registry, FeedsConfig, Mechanism};
    use crate::OracleError;
    use alloy_primitives::{address, Address};
    use liq_config::{Intern, Registry};
    use liq_types::LogSubscriber;
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    fn committed() -> (Registry, FeedsConfig) {
        let root = workspace_root();
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let feeds = FeedsConfig::load(&root.join("config/feeds")).unwrap();
        (reg, feeds)
    }

    const WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const WETH_PROXY: Address = address!("0x5424384B256154046E9667dDfAaa5e550145215e");
    const WETH_AGG: Address = address!("0x7c7FdFCa295a787DED12Bb5c1A49A8d2Cc20E3f8");

    /// Oracle: committed registry + config/feeds. Negative: a TOML aggregator
    /// that is not the registry value must not bind.
    #[test]
    fn committed_feeds_match_registry() {
        let (reg, feeds) = committed();
        feeds.assert_against_registry(&reg).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let set = super::FeedSet::bind(&feeds, &intern, &reg).unwrap();
        assert_eq!(set.specs.len(), 10, "oracle: 9 assets, WETH × 2 markets");
        assert!(set.aggregators.contains(&WETH_AGG));
        let subs = set.subscriptions();
        assert!(
            subs.iter().any(|s| s.address == WETH_AGG),
            "oracle: subscribe to registry aggregator"
        );
        let weth_rows: Vec<_> = set.specs.iter().filter(|s| s.spec.asset == WETH).collect();
        assert_eq!(weth_rows.len(), 2);
        assert!(weth_rows.iter().all(|s| s.spec.proxy == WETH_PROXY));
        assert!(weth_rows
            .iter()
            .all(|s| s.spec.mechanism == Mechanism::ChainlinkPush));
    }

    /// Oracle: GUIDE 06 §2 + WP 06A-1 mutation. Flip WETH aggregator's
    /// configured address; boot must refuse. Registry is not mutated.
    #[test]
    fn corrupting_aggregator_fails_startup() {
        let (reg, mut feeds) = committed();
        let row = feeds
            .feeds
            .iter_mut()
            .find(|f| f.proxy == WETH_PROXY)
            .unwrap();
        let bogus = address!("0x1111111111111111111111111111111111111111");
        row.standard_aggregator = Some(bogus);
        let err = feeds.assert_against_registry(&reg).unwrap_err();
        match err {
            OracleError::AggregatorMismatch {
                proxy,
                expected,
                found,
            } => {
                assert_eq!(proxy, WETH_PROXY);
                assert_eq!(expected, WETH_AGG);
                assert_eq!(found, bogus);
            }
            other => panic!("expected AggregatorMismatch, got {other}"),
        }
    }

    /// Oracle: committed registry.oracles. Pair prefixes that do not hit a
    /// token are failures — we do not invent SNX/ENS/… addresses.
    #[test]
    fn unresolved_registry_oracles_are_failures_not_feeds() {
        let (reg, _) = committed();
        let (ok, fail) = resolve_registry(&reg);
        assert_eq!(reg.oracles.len(), ok.len().saturating_add(fail.len()));
        assert_eq!(ok.len(), 9, "oracle: 9 pair prefixes hit exactly one token");
        assert_eq!(fail.len(), 6);
        let pairs: Vec<_> = fail.iter().map(|f| f.pair.as_str()).collect();
        for p in [
            "aave-v3/0xc011a73e",
            "aave-v3/0xc1836021",
            "aave-v3/0x11111111",
            "aave-v3/0xaf5191b0",
            "aave-v3/0xdefa4e8a",
            "aave-v3/0x3432b6a6",
        ] {
            assert!(
                pairs.contains(&p),
                "oracle: committed pair {p} must be a failure; got {pairs:?}"
            );
        }
        for f in &fail {
            assert!(
                f.reason.contains("does not resolve to a registry token"),
                "{}",
                f.reason
            );
        }
        let meta: serde_json::Value = serde_json::from_slice(
            &std::fs::read(workspace_root().join("registry/registry.meta.json")).unwrap(),
        )
        .unwrap();
        let agg_fail = meta["failures"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|k| k.starts_with("oracle:aggregator:"))
            .count();
        assert_eq!(
            agg_fail, 226,
            "oracle: committed meta oracle:aggregator failures"
        );
    }

    /// Oracle: Aave V3 core `getSourceOfAsset(WETH)` on chain. Negative: a
    /// mutated proxy must not match.
    #[tokio::test(flavor = "current_thread")]
    async fn live_weth_source_matches_registry_proxy() {
        use super::assert_protocol_sources;
        use liq_config::HttpRpc;
        use std::time::Duration;
        let (reg, feeds) = committed();
        let only: Vec<_> = feeds
            .feeds
            .iter()
            .filter(|f| f.asset == WETH && f.proxy == WETH_PROXY)
            .cloned()
            .collect();
        assert!(!only.is_empty());
        let subset = FeedsConfig { feeds: only };
        let url = std::env::var("LIQ_RPC_URL")
            .unwrap_or_else(|_| "https://ethereum.publicnode.com".to_string());
        let rpc = HttpRpc::connect(&url).unwrap();
        tokio::time::timeout(
            Duration::from_secs(45),
            assert_protocol_sources(&subset, &reg, &rpc),
        )
        .await
        .expect("rpc timed out — fail closed")
        .unwrap();
        let mut bad = subset.clone();
        bad.feeds[0].proxy = address!("0x1111111111111111111111111111111111111111");
        let err = tokio::time::timeout(
            Duration::from_secs(45),
            assert_protocol_sources(&bad, &reg, &rpc),
        )
        .await
        .expect("rpc timed out — fail closed")
        .unwrap_err();
        assert!(
            matches!(err, OracleError::ProtocolOracleMismatch { .. }),
            "got {err}"
        );
    }

    /// Oracle: figment load. Negative: a typo'd key is deny_unknown_fields.
    #[test]
    #[allow(clippy::result_large_err)]
    fn unknown_feed_field_is_load_error() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "bad.toml",
                r#"
[[feed]]
protocol = "aave-v3"
market = "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2"
asset = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
mechanism = "chainlink-push"
proxy = "0x5424384b256154046e9667ddfaaa5e550145215e"
standard_aggregator = "0x7c7fdfca295a787ded12bb5c1a49a8d2cc20e3f8"
decimals = 8
heartbeat_secs = 3600
deviation_bps = 50
spoke = "invented"
"#,
            )?;
            let err = load(jail.directory()).unwrap_err();
            match err {
                OracleError::Load(msg) => {
                    assert!(
                        msg.contains("unknown field") && msg.contains("spoke"),
                        "{msg}"
                    );
                }
                other => panic!("expected Load, got {other}"),
            }
            Ok(())
        });
    }
}
