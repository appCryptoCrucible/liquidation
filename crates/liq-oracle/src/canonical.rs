//! Canonical prices: `AnswerUpdated` → [`PriceVector`], staleness at
//! `heartbeat × 1.5` (GUIDE 06 §3, §7b, GUIDE 14).

use crate::feeds::{asset_source_updated_topic0, FeedSet};
use crate::{OracleError, Result};
use alloy_primitives::{Address, I256, U256};
use alloy_sol_types::{sol, SolCall, SolEvent};
use liq_config::rpc::ChainRpc;
use liq_config::Intern;
use liq_protocol::DecodedLog;
use liq_types::fixed::Ray;
use liq_types::{AssetId, HaltReason, HaltScope, HaltSink, Price, PriceVector, SourceKind};
use smallvec::SmallVec;
use std::collections::BTreeMap;

sol! {
    interface IAggregator {
        event AnswerUpdated(int256 indexed current, uint256 indexed roundId, uint256 updatedAt);
        function latestRoundData()
            external
            view
            returns (
                uint80 roundId,
                int256 answer,
                uint256 startedAt,
                uint256 updatedAt,
                uint80 answeredInRound
            );
        function decimals() external view returns (uint8);
    }
    interface IAaveOracle {
        event AssetSourceUpdated(address indexed asset, address indexed source);
    }
}

/// `AnswerUpdated` topic0. Same hash as 03A would use if it decoded this event.
pub const ANSWER_UPDATED_TOPIC0: alloy_primitives::B256 =
    IAggregator::AnswerUpdated::SIGNATURE_HASH;

/// Seconds after `last_update` at which the asset is stale: `heartbeat + heartbeat/2`
/// (`heartbeat × 1.5`, integer). GUIDE 14 trips on `now − last >` this value.
#[must_use]
pub fn stale_after(heartbeat_secs: u32) -> u64 {
    let hb = u64::from(heartbeat_secs);
    match hb.checked_div(2) {
        Some(half) => hb.saturating_add(half),
        None => hb,
    }
}

/// Aggregator `answer` at `decimals` → [`Ray`] (1e27). Fail closed on
/// non-positive or a scale that does not fit.
pub fn answer_to_ray(answer: I256, decimals: u8) -> Result<Ray> {
    if !answer.is_positive() {
        return Err(OracleError::NonPositiveAnswer);
    }
    let mag = answer.unsigned_abs();
    let Some(exp) = 27u32.checked_sub(u32::from(decimals)) else {
        return Err(OracleError::ScaleOverflow { decimals });
    };
    let factor = ten_pow(exp, decimals)?;
    let raw = mag
        .checked_mul(factor)
        .ok_or(OracleError::ScaleOverflow { decimals })?;
    Ok(Ray::from_raw(raw))
}

fn ten_pow(exp: u32, decimals: u8) -> Result<U256> {
    let mut factor = U256::from(1u64);
    let ten = U256::from(10u64);
    let mut i = 0u32;
    while i < exp {
        factor = factor
            .checked_mul(ten)
            .ok_or(OracleError::ScaleOverflow { decimals })?;
        i = i
            .checked_add(1)
            .ok_or(OracleError::ScaleOverflow { decimals })?;
    }
    Ok(factor)
}

/// Working canonical book. Single writer (oracle thread).
pub struct CanonicalBook {
    feeds: FeedSet,
    by_agg: BTreeMap<Address, SmallVec<[u16; 2]>>,
    /// Per [`AssetId`]: first feed index, heartbeat, last `updatedAt`.
    by_asset: Vec<AssetSlot>,
    vector: PriceVector,
}

struct AssetSlot {
    heartbeat_secs: u32,
    last_ts: u64,
}

impl CanonicalBook {
    /// Size the vector from `intern` (index = [`AssetId`]). Slots without a
    /// feed stay `ts = 0` and are not a price.
    pub fn new(feeds: FeedSet, intern: &Intern) -> Result<Self> {
        let n = intern.assets().len();
        let mut vector = Vec::with_capacity(n);
        for a in intern.assets() {
            vector.push(Price {
                asset: a.id,
                price: Ray::ZERO,
                source: SourceKind::Canonical,
                block: 0,
                ts: 0,
            });
        }
        let mut by_asset: Vec<Option<AssetSlot>> = Vec::with_capacity(n);
        for _ in 0..n {
            by_asset.push(None);
        }
        let mut by_agg: BTreeMap<Address, SmallVec<[u16; 2]>> = BTreeMap::new();
        for (i, f) in feeds.specs.iter().enumerate() {
            let idx = u16::try_from(i).map_err(|_| OracleError::Load("too many feeds".into()))?;
            for a in &f.watch {
                let slot = by_agg.entry(*a).or_default();
                if !slot.contains(&idx) {
                    slot.push(idx);
                }
            }
            let ai = usize::from(f.asset.0);
            let Some(slot) = by_asset.get_mut(ai) else {
                return Err(OracleError::InternAsset {
                    asset: f.spec.asset,
                });
            };
            match slot {
                Some(existing) => {
                    if existing.heartbeat_secs != f.spec.heartbeat_secs {
                        return Err(OracleError::Load(format!(
                            "heartbeat disagree for asset {:#x}",
                            f.spec.asset
                        )));
                    }
                }
                None => {
                    *slot = Some(AssetSlot {
                        heartbeat_secs: f.spec.heartbeat_secs,
                        last_ts: 0,
                    });
                }
            }
        }
        Ok(Self {
            feeds,
            by_agg,
            by_asset: flatten_slots(by_asset),
            vector: PriceVector(vector),
        })
    }

    #[must_use]
    pub fn vector(&self) -> &PriceVector {
        &self.vector
    }

    #[must_use]
    pub fn feeds(&self) -> &FeedSet {
        &self.feeds
    }

    /// Fold one routed log. `true` when the published vector changed.
    pub fn apply_log(&mut self, log: &DecodedLog<'_>, sink: Option<&dyn HaltSink>) -> Result<bool> {
        let Some(t0) = log.topics.first() else {
            return Ok(false);
        };
        if *t0 == ANSWER_UPDATED_TOPIC0 {
            return self.apply_answer(log);
        }
        if *t0 == asset_source_updated_topic0() {
            return self.apply_source_swap(log, sink);
        }
        Ok(false)
    }

    fn apply_answer(&mut self, log: &DecodedLog<'_>) -> Result<bool> {
        let ev = IAggregator::AnswerUpdated::decode_raw_log(log.topics.iter().copied(), log.data)
            .map_err(|_| OracleError::BadAnswerUpdated)?;
        let ts = u64::try_from(ev.updatedAt).map_err(|_| OracleError::BadAnswerUpdated)?;
        if ts == 0 {
            return Err(OracleError::BadAnswerUpdated);
        }
        let Some(idxs) = self.by_agg.get(&log.address) else {
            return Ok(false);
        };
        let idxs = idxs.clone();
        let mut changed = false;
        for idx in idxs {
            let Some(feed) = self.feeds.specs.get(usize::from(idx)) else {
                return Err(OracleError::Load("feed index".into()));
            };
            let price = answer_to_ray(ev.current, feed.spec.decimals)?;
            let asset = feed.asset;
            self.write_price(asset, price, log.block, ts)?;
            changed = true;
        }
        Ok(changed)
    }

    fn apply_source_swap(
        &mut self,
        log: &DecodedLog<'_>,
        sink: Option<&dyn HaltSink>,
    ) -> Result<bool> {
        let ev =
            IAaveOracle::AssetSourceUpdated::decode_raw_log(log.topics.iter().copied(), log.data)
                .map_err(|_| OracleError::BadAnswerUpdated)?;
        for f in &self.feeds.specs {
            if f.spec.asset != ev.asset {
                continue;
            }
            if ev.source != f.spec.proxy {
                if let Some(s) = sink {
                    s.halt(HaltScope::Asset(f.asset), HaltReason::ProxyUpgrade);
                }
                return Err(OracleError::SourceMigrated {
                    asset: ev.asset,
                    expected: f.spec.proxy,
                    found: ev.source,
                });
            }
        }
        Ok(false)
    }

    fn write_price(&mut self, asset: AssetId, price: Ray, block: u64, ts: u64) -> Result<()> {
        let i = usize::from(asset.0);
        let slot = self.vector.0.get_mut(i).ok_or(OracleError::InternAsset {
            asset: Address::ZERO,
        })?;
        slot.asset = asset;
        slot.price = price;
        slot.source = SourceKind::Canonical;
        slot.block = block;
        slot.ts = ts;
        if let Some(a) = self.by_asset.get_mut(i) {
            a.last_ts = ts;
        }
        Ok(())
    }

    /// Asset-scoped staleness. `now` is the canonical block timestamp.
    pub fn check_staleness(&self, now: u64, sink: &dyn HaltSink) {
        for (i, slot) in self.by_asset.iter().enumerate() {
            if slot.heartbeat_secs == 0 {
                continue;
            }
            let elapsed = now.saturating_sub(slot.last_ts);
            if elapsed > stale_after(slot.heartbeat_secs) {
                let id = AssetId(u16::try_from(i).unwrap_or(u16::MAX));
                sink.halt(HaltScope::Asset(id), HaltReason::OracleStale);
            }
        }
    }

    /// Seed from `latestRoundData` on each watched aggregator. Live chain.
    pub async fn seed<R: ChainRpc + Sync>(&mut self, rpc: &R) -> Result<()> {
        let mut seen: BTreeMap<Address, (I256, u64)> = BTreeMap::new();
        for f in &self.feeds.specs {
            for agg in &f.watch {
                if seen.contains_key(agg) {
                    continue;
                }
                let data =
                    alloy_primitives::Bytes::from(IAggregator::latestRoundDataCall {}.abi_encode());
                let raw = rpc.call(*agg, data).await?;
                let out = IAggregator::latestRoundDataCall::abi_decode_returns_validate(&raw)
                    .map_err(|_| OracleError::Load(format!("latestRoundData at {agg:#x}")))?;
                let ts = u64::try_from(out.updatedAt).map_err(|_| OracleError::BadAnswerUpdated)?;
                if ts == 0 {
                    return Err(OracleError::BadAnswerUpdated);
                }
                seen.insert(*agg, (out.answer, ts));
            }
        }
        let specs: Vec<(SmallVec<[Address; 2]>, u8, AssetId)> = self
            .feeds
            .specs
            .iter()
            .map(|f| (f.watch.clone(), f.spec.decimals, f.asset))
            .collect();
        for (watch, decimals, asset) in specs {
            let Some(agg) = watch.first() else {
                continue;
            };
            let Some((answer, ts)) = seen.get(agg).copied() else {
                continue;
            };
            let price = answer_to_ray(answer, decimals)?;
            self.write_price(asset, price, 0, ts)?;
        }
        Ok(())
    }
}

fn flatten_slots(src: Vec<Option<AssetSlot>>) -> Vec<AssetSlot> {
    src.into_iter()
        .map(|s| {
            s.unwrap_or(AssetSlot {
                heartbeat_secs: 0,
                last_ts: 0,
            })
        })
        .collect()
}

/// Encode `AnswerUpdated` (tests / fixtures).
#[cfg(test)]
pub fn encode_answer_updated(
    aggregator: Address,
    current: I256,
    round_id: U256,
    updated_at: U256,
    block: u64,
    timestamp: u64,
) -> liq_node::OwnedLog {
    let ev = IAggregator::AnswerUpdated {
        current,
        roundId: round_id,
        updatedAt: updated_at,
    };
    let topics = ev.encode_topics();
    let mut av = arrayvec::ArrayVec::<alloy_primitives::B256, 4>::new();
    for t in topics {
        av.push(t.into());
    }
    liq_node::OwnedLog {
        address: aggregator,
        topics: av,
        data: ev.encode_data(),
        block,
        timestamp,
        tx_index: 0,
        log_index: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{answer_to_ray, encode_answer_updated, stale_after, CanonicalBook};
    use crate::feeds::{FeedSet, FeedsConfig};
    use crate::publish::split;
    use crate::OracleError;
    use alloy_primitives::{address, Address, I256, U256};
    use liq_config::{Intern, Registry};
    use liq_node::{DecodeArena, LogRouter, Route};
    use liq_types::fixed::Ray;
    use liq_types::{AssetId, HaltReason, HaltScope, HaltSink, LogSubscriber, SourceKind};
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    const WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const WETH_AGG: Address = address!("0x7c7FdFCa295a787DED12Bb5c1A49A8d2Cc20E3f8");

    struct Rec(Mutex<Vec<(HaltScope, HaltReason)>>);
    impl HaltSink for Rec {
        fn halt(&self, scope: HaltScope, reason: HaltReason) {
            self.0.lock().unwrap().push((scope, reason));
        }
    }

    fn book() -> (CanonicalBook, Intern, AssetId) {
        let root = workspace_root();
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let feeds = FeedsConfig::load(&root.join("config/feeds")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let set = FeedSet::bind(&feeds, &intern, &reg).unwrap();
        let weth = intern.asset(WETH).unwrap();
        (CanonicalBook::new(set, &intern).unwrap(), intern, weth)
    }

    /// Oracle: 8-decimal Chainlink `1e8` is `Ray::ONE`. Math, not the book.
    #[test]
    fn eight_decimal_one_is_ray_one() {
        let ans = I256::try_from(100_000_000i64).unwrap();
        assert_eq!(answer_to_ray(ans, 8).unwrap(), Ray::ONE);
        assert!(matches!(
            answer_to_ray(I256::ZERO, 8),
            Err(OracleError::NonPositiveAnswer)
        ));
        assert!(matches!(
            answer_to_ray(I256::MINUS_ONE, 8),
            Err(OracleError::NonPositiveAnswer)
        ));
        assert!(matches!(
            answer_to_ray(ans, 28),
            Err(OracleError::ScaleOverflow { decimals: 28 })
        ));
    }

    /// Oracle: GUIDE 14 — trip when `now − last > heartbeat × 1.5`.
    #[test]
    fn staleness_trips_strictly_after_one_and_a_half_heartbeats() {
        assert_eq!(stale_after(3600), 5400);
        assert_eq!(stale_after(86400), 129_600);
        let (mut book, _, weth) = book();
        book.write_price(weth, Ray::ONE, 1, 1_000).unwrap();
        let rec = Rec(Mutex::new(Vec::new()));
        book.check_staleness(1_000 + 5400, &rec);
        let at_eq = rec.0.lock().unwrap().clone();
        assert!(
            !at_eq.iter().any(|(s, _)| *s == HaltScope::Asset(weth)),
            "oracle: GUIDE-14 `>` — equal to 1.5× is not stale for WETH; got {at_eq:?}"
        );
        book.check_staleness(1_000 + 5401, &rec);
        let hits = rec.0.lock().unwrap();
        assert!(
            hits.iter()
                .any(|(s, r)| { *s == HaltScope::Asset(weth) && *r == HaltReason::OracleStale }),
            "oracle: GUIDE-14 asset-scoped OracleStale; got {hits:?}"
        );
        assert!(
            !hits.iter().any(|(s, _)| matches!(s, HaltScope::Global)),
            "oracle: staleness is not global"
        );
    }

    /// Oracle: GUIDE 06 §3 — log → decode → vector, via 03A LogRouter.
    #[test]
    fn answer_updated_via_log_router_updates_price_vector() {
        let (mut book, _intern, weth) = book();
        let filters = book.feeds().subscriptions();
        struct Sub(Vec<liq_types::LogFilter>);
        impl LogSubscriber for Sub {
            fn subscriptions(&self) -> Vec<liq_types::LogFilter> {
                self.0.clone()
            }
        }
        let sub = Sub(filters);
        let router = LogRouter::from_subscribers(&[&sub as &dyn LogSubscriber]).unwrap();
        let ans = I256::try_from(200_000_000_000i64).unwrap(); // $2000 at 8 dec
        let log = encode_answer_updated(
            WETH_AGG,
            ans,
            U256::from(42u64),
            U256::from(1_700_000_000u64),
            18_000_000,
            1_700_000_012,
        );
        let arena = DecodeArena::with_capacity(4096);
        match router.route(&arena, &log).unwrap() {
            Route::Hit { decoded, .. } => {
                assert!(book.apply_log(&decoded, None).unwrap());
            }
            other => panic!("expected Hit, got {other:?}"),
        }
        let px = book.vector().0.get(usize::from(weth.0)).unwrap();
        assert_eq!(px.asset, weth);
        assert_eq!(px.price, answer_to_ray(ans, 8).unwrap());
        assert!(matches!(px.source, SourceKind::Canonical));
        assert_eq!(px.block, 18_000_000);
        assert_eq!(px.ts, 1_700_000_000);
        let (mut w, mut r) = split(book.vector());
        w.write(book.vector().clone());
        assert_eq!(
            r.read().0.get(usize::from(weth.0)).unwrap().ts,
            1_700_000_000
        );
    }
}
