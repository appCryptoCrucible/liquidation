//! Protocol-reported prices, read off the hot path (GUIDE 06; decision
//! 2026-09-25: read each protocol's own price getter every block).
//!
//! * The hot thread collects every adapter's [`PriceRead`]s (at startup and
//!   every [`READS_REFRESH_BLOCKS`]) and publishes them to the reader.
//! * The reader thread, once per new head, multicalls all of them pinned to
//!   that block, decodes each answer with its adapter, and publishes the
//!   batch — latest wins, so a slow block never queues.
//! * The hot thread folds the newest batch into [`ProtocolPriceBook`], the
//!   engine's per-market overlay, and hands the engine the moves.
//!
//! Chainlink / derived feeds stay the canonical vector: the early-warning
//! path and the valuation of gas and exits. This book decides health.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use liq_config::rpc::{ChainRpc, HttpRpc};
use liq_engine::{ProtocolPriceMove, ProtocolPrices};
use liq_protocol::{MarketRows, PriceRead};
use liq_state::StateView;
use liq_types::{AssetId, MarketId, ProtocolId, Ray};

use crate::bind::BoundProtocol;
use crate::pool_seed::{aggregate, call};

/// How often the hot thread rebuilds the read set (markets can appear).
pub const READS_REFRESH_BLOCKS: u64 = 300;
/// How often the reader checks for a new head.
const POLL: Duration = Duration::from_millis(200);
/// Reads per multicall.
const BATCH: usize = 300;

/// Every adapter's reads: `(index into the bound protocols, read)`.
pub type ReadSet = Vec<(usize, PriceRead)>;

/// One decoded protocol price. `usd` is false for protocols whose getter is
/// not a dollar price (Morpho, Silo, Liquity); they never size.
///
/// `scale` is set on ratio reads (Morpho `price()`): the read's numeraire
/// asset and the price the decode gave it (Morpho's loan at `10^36`-scale).
/// Fluid publishes no price: its health is the vault's own liquidation,
/// read every block as state (`state_reads`). [`to_usd`] restates the pair in USD with the numeraire's own USD
/// price, which keeps the ratio health uses and makes values dollars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuotedPrice {
    pub protocol: ProtocolId,
    pub market: MarketId,
    pub asset: AssetId,
    pub price: Ray,
    pub usd: bool,
    pub scale: Option<(AssetId, Ray)>,
}

/// The numeraire of a ratio read's decoded pair: Morpho publishes the loan
/// first; an Euler vault in a token unit of account publishes the unit
/// first. `None` for reads that quote dollars.
fn ratio_numeraire(
    p: &BoundProtocol,
    read: &PriceRead,
    decoded: &[(AssetId, Ray)],
) -> Option<(AssetId, Ray)> {
    match p {
        BoundProtocol::MorphoBlue(_) => decoded.first().copied(),
        BoundProtocol::EulerV2(_) if read.tag == liq_adapters_euler_v2::RATIO_READ_TAG => {
            decoded.first().copied()
        }
        // Aave V2: ETH prices, WETH first at one RAY.
        BoundProtocol::AaveV3(_) if read.tag & liq_adapters_aave_v3::ETH_QUOTED_READ != 0 => {
            decoded.first().copied()
        }
        // Silo: both sides in the pair's oracle quote unit (often a
        // virtual asset with no price of its own), the numeraire first.
        BoundProtocol::SiloV2(_) => decoded.first().copied(),
        _ => None,
    }
}

/// Ratio prices restated in USD: each price times the numeraire's USD price
/// over the price the decode gave the numeraire (so the numeraire becomes
/// exactly its USD price and the pair's ratio is kept, to one unit of the
/// USD-scaled collateral price). When the numeraire has no USD price, another
/// asset of the same read that has one anchors it instead (only the ratio is
/// the protocol's: a Silo pair quoted in a virtual unit, a Morpho market
/// whose loan token has no feed). A read none of whose assets has a USD
/// price is left out, not given one: its market stays on the canonical
/// vector.
pub fn to_usd(
    entries: &[QuotedPrice],
    usd_of: impl Fn(AssetId) -> Option<Ray>,
) -> Vec<QuotedPrice> {
    let usd = |a: AssetId| usd_of(a).filter(|p| !p.raw().is_zero());
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let Some((num, published)) = e.scale else {
            out.push(*e);
            continue;
        };
        let anchor = usd(num).map(|u| (u, published)).or_else(|| {
            entries
                .iter()
                .filter(|s| s.protocol == e.protocol && s.market == e.market && s.scale == e.scale)
                .find_map(|s| usd(s.asset).map(|u| (u, s.price)))
        });
        let Some((usd, published)) = anchor else {
            tracing::debug!(
                market = e.market.0,
                asset = num.0,
                "no asset of the ratio read has a USD price — market not overlaid"
            );
            continue;
        };
        if published.raw().is_zero() {
            continue;
        }
        let Ok(price) = liq_types::fixed::mul_div(
            e.price.raw(),
            usd.raw(),
            published.raw(),
            liq_types::fixed::Rounding::Down,
        ) else {
            continue;
        };
        if price.is_zero() {
            continue;
        }
        out.push(QuotedPrice {
            price: Ray::from_raw(price),
            ..*e
        });
    }
    out
}

/// One block's decoded protocol prices.
#[derive(Debug, Default)]
pub struct PriceBatch {
    pub block: u64,
    pub entries: Vec<QuotedPrice>,
    /// Reads whose call or decode failed this block (their prices keep
    /// the last good value).
    pub failed: usize,
}

/// Shared between the hot thread and the reader.
pub struct ReaderShared {
    pub reads: ArcSwap<ReadSet>,
    pub latest: ArcSwap<Option<Arc<PriceBatch>>>,
}

impl ReaderShared {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            reads: ArcSwap::from_pointee(Vec::new()),
            latest: ArcSwap::from_pointee(None),
        })
    }
}

/// [`StateView`] as the adapters' [`MarketRows`].
pub struct ViewRows<'a>(pub StateView<'a>);

impl MarketRows for ViewRows<'_> {
    fn rows(&self, market: MarketId) -> Option<&[liq_protocol::MarketRow]> {
        self.0.markets(market).ok()
    }
}

/// Collect every adapter's reads against current state.
#[must_use]
pub fn collect_reads(protocols: &[BoundProtocol], view: StateView<'_>) -> ReadSet {
    let rows = ViewRows(view);
    let mut out = Vec::new();
    for (i, p) in protocols.iter().enumerate() {
        for r in p.as_dyn().price_reads(&rows) {
            out.push((i, r));
        }
    }
    out
}

/// Read and decode `reads` at `block`.
pub async fn read_block(
    protocols: &[BoundProtocol],
    rpc: &HttpRpc,
    reads: &ReadSet,
    block: u64,
) -> PriceBatch {
    let mut batch = PriceBatch {
        block,
        ..PriceBatch::default()
    };
    let mut decoded = Vec::new();
    for chunk in reads.chunks(BATCH) {
        let calls = chunk
            .iter()
            .map(|(_, r)| call(r.target, r.calldata.to_vec()))
            .collect();
        let Some(res) = aggregate(rpc, calls, block).await else {
            batch.failed = batch.failed.saturating_add(chunk.len());
            continue;
        };
        for ((pi, read), row) in chunk.iter().zip(res) {
            let Some(p) = protocols.get(*pi) else {
                continue;
            };
            if !row.success {
                batch.failed = batch.failed.saturating_add(1);
                continue;
            }
            decoded.clear();
            let proto = p.as_dyn();
            match proto.decode_prices(read, &row.returnData, &mut decoded) {
                Ok(()) => {
                    let scale = ratio_numeraire(p, read, &decoded);
                    // A ratio read restated in USD is never another
                    // protocol's USD source.
                    let usd = quotes_in_usd(p) && scale.is_none();
                    batch
                        .entries
                        .extend(decoded.iter().map(|&(asset, price)| QuotedPrice {
                            protocol: proto.id(),
                            market: read.market,
                            asset,
                            price,
                            usd,
                            scale,
                        }));
                }
                Err(e) => {
                    batch.failed = batch.failed.saturating_add(1);
                    tracing::debug!(error = %e, protocol = proto.id().0, market = read.market.0, "price decode failed");
                }
            }
        }
    }
    batch
}

/// The reader thread: one batch per new head, published to `shared.latest`.
pub fn spawn_reader(
    protocols: &'static [BoundProtocol],
    rpc_url: String,
    shared: Arc<ReaderShared>,
    stop: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>, std::io::Error> {
    std::thread::Builder::new()
        .name("liq-bot-prices".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "price reader runtime refused — protocols priced from canonical feeds only");
                    return;
                }
            };
            let rpc = match HttpRpc::connect(&rpc_url) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "price reader RPC connect failed — protocols priced from canonical feeds only");
                    return;
                }
            };
            let mut last = 0u64;
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(POLL);
                let reads = shared.reads.load_full();
                if reads.is_empty() {
                    continue;
                }
                let head = match rt.block_on(rpc.block_number()) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(error = %e, "price reader: head unavailable");
                        continue;
                    }
                };
                if head <= last {
                    continue;
                }
                let batch = rt.block_on(read_block(protocols, &rpc, &reads, head));
                if batch.failed != 0 {
                    tracing::warn!(block = head, failed = batch.failed, reads = reads.len(), "protocol price reads failed");
                }
                last = head;
                shared.latest.store(Arc::new(Some(Arc::new(batch))));
            }
        })
}

/// The engine's per-market overlay. Hot-thread owned.
#[derive(Debug, Default)]
pub struct ProtocolPriceBook {
    map: HashMap<(ProtocolId, MarketId), Vec<(AssetId, Ray)>>,
    /// First USD-denominated source for each asset. A later protocol does
    /// not replace it. Ratio protocols never enter.
    usd_key: HashMap<AssetId, (ProtocolId, MarketId)>,
    /// Every market whose own oracle prices each asset (USD or restated in
    /// USD), in the order first seen: [`Self::usd_or_markets`].
    markets_of: HashMap<AssetId, Vec<(ProtocolId, MarketId)>>,
    applied: u64,
}

/// Markets [`ProtocolPriceBook::usd_or_markets`] takes the median of: enough
/// to outvote a few odd oracles without allocating on the hot thread.
const MEDIAN_MARKETS: usize = 15;

impl ProtocolPrices for ProtocolPriceBook {
    fn patch(&self, protocol: ProtocolId, market: MarketId) -> &[(AssetId, Ray)] {
        self.map.get(&(protocol, market)).map_or(&[], Vec::as_slice)
    }
}

impl ProtocolPriceBook {
    /// Block of the last batch applied (`0` = none yet).
    #[must_use]
    pub const fn applied(&self) -> u64 {
        self.applied
    }

    /// Fold a batch newer than the last one applied. Fills `moves` with
    /// every changed price; returns `true` when a `(protocol, market,
    /// asset)` got its first price (no thresholds exist for it yet, so the
    /// caller resyncs instead of sweeping).
    pub fn apply(&mut self, batch: &PriceBatch, moves: &mut Vec<ProtocolPriceMove>) -> bool {
        moves.clear();
        if batch.block <= self.applied {
            return false;
        }
        self.applied = batch.block;
        let mut first = false;
        for e in &batch.entries {
            let slot = self.map.entry((e.protocol, e.market)).or_default();
            match slot.iter_mut().find(|(a, _)| *a == e.asset) {
                Some((_, old)) => {
                    if *old != e.price {
                        moves.push(ProtocolPriceMove {
                            protocol: e.protocol,
                            market: e.market,
                            asset: e.asset,
                            old: *old,
                            new: e.price,
                        });
                        *old = e.price;
                    }
                }
                None => {
                    slot.push((e.asset, e.price));
                    first = true;
                    self.markets_of
                        .entry(e.asset)
                        .or_default()
                        .push((e.protocol, e.market));
                }
            }
            if e.usd && !self.usd_key.contains_key(&e.asset) {
                self.usd_key.insert(e.asset, (e.protocol, e.market));
            }
        }
        first
    }

    /// USD per whole token from the first Aave / Spark / Compound / Euler /
    /// Gearbox price that named this asset. Ratio-protocol prices are absent.
    #[must_use]
    pub fn usd(&self, asset: AssetId) -> Option<Ray> {
        let (protocol, market) = self.usd_key.get(&asset)?;
        self.map
            .get(&(*protocol, *market))?
            .iter()
            .find(|(a, _)| *a == asset)
            .map(|(_, p)| *p)
    }

    /// [`Self::usd`], or the USD price `batch` itself carries when the book
    /// has none yet. On the first batch after start a ratio read (Morpho)
    /// and the getter that prices its numeraire arrive together; restating
    /// against the book alone dropped the ratio read until a later block.
    #[must_use]
    pub fn usd_or_batch(&self, asset: AssetId, batch: &PriceBatch) -> Option<Ray> {
        self.usd(asset).or_else(|| {
            batch
                .entries
                .iter()
                .find(|e| e.usd && e.scale.is_none() && e.asset == asset)
                .map(|e| e.price)
        })
    }

    /// [`Self::usd`], or the price `(protocol, market)`'s own oracle gives
    /// `asset`, restated in USD. A collateral only its own market prices (a
    /// Morpho LP token: no canonical feed, no USD getter) is sized at the
    /// price the protocol seizes it at.
    #[must_use]
    pub fn usd_or_market(
        &self,
        asset: AssetId,
        protocol: ProtocolId,
        market: MarketId,
    ) -> Option<Ray> {
        self.usd(asset).or_else(|| {
            self.patch(protocol, market)
                .iter()
                .find(|(a, _)| *a == asset)
                .map(|(_, p)| *p)
        })
    }

    /// [`Self::usd`], or the median of what the markets pricing `asset`
    /// say (each its own oracle, restated in USD; the first
    /// [`MEDIAN_MARKETS`] seen; the lower middle of an even count). For a
    /// token no canonical feed or USD getter prices, where no single market
    /// is the one in question: gas and bands in that token. Permissionless
    /// markets name any oracle, so no one market decides.
    #[must_use]
    pub fn usd_or_markets(&self, asset: AssetId) -> Option<Ray> {
        if let Some(p) = self.usd(asset) {
            return Some(p);
        }
        let mut seen: smallvec::SmallVec<[Ray; MEDIAN_MARKETS]> = smallvec::SmallVec::new();
        for (protocol, market) in self.markets_of.get(&asset)?.iter().take(MEDIAN_MARKETS) {
            if let Some((_, p)) = self
                .patch(*protocol, *market)
                .iter()
                .find(|(a, p)| *a == asset && !p.raw().is_zero())
            {
                seen.push(*p);
            }
        }
        seen.sort_unstable();
        let mid = seen.len().checked_sub(1)? / 2;
        seen.get(mid).copied()
    }
}

/// Per protocol, what the first applied batch priced: markets with an
/// oracle read, how many the book prices, and the rest by reason. A market
/// with no read at all (no oracle pinned) is not in `reads` and so not
/// counted here; the adapters log those at bind.
#[derive(Debug, PartialEq, Eq)]
pub struct PriceCoverage {
    pub protocol: ProtocolId,
    pub read: usize,
    pub priced: usize,
    /// The read failed or decoded no price.
    pub failed: Vec<MarketId>,
    /// Decoded, but a ratio read none of whose assets has a USD price.
    pub no_anchor: Vec<MarketId>,
}

/// [`PriceCoverage`] of `book` after `raw` (the batch as read, before
/// restating) was applied, over `reads`.
#[must_use]
pub fn coverage(
    book: &ProtocolPriceBook,
    reads: &ReadSet,
    raw: &PriceBatch,
    protocols: &[BoundProtocol],
) -> Vec<PriceCoverage> {
    let mut out: Vec<PriceCoverage> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (pi, r) in reads {
        let Some(p) = protocols.get(*pi) else {
            continue;
        };
        let id = p.as_dyn().id();
        if !seen.insert((id, r.market)) {
            continue;
        }
        let i = match out.iter().position(|c| c.protocol == id) {
            Some(i) => i,
            None => {
                out.push(PriceCoverage {
                    protocol: id,
                    read: 0,
                    priced: 0,
                    failed: Vec::new(),
                    no_anchor: Vec::new(),
                });
                out.len().saturating_sub(1)
            }
        };
        let Some(c) = out.get_mut(i) else { continue };
        c.read = c.read.saturating_add(1);
        if !book.patch(id, r.market).is_empty() {
            c.priced = c.priced.saturating_add(1);
        } else if raw
            .entries
            .iter()
            .any(|e| e.protocol == id && e.market == r.market)
        {
            c.no_anchor.push(r.market);
        } else {
            c.failed.push(r.market);
        }
    }
    out
}

/// Log [`coverage`]: one line per protocol, with the first unpriced
/// markets by id.
pub fn log_coverage(cov: &[PriceCoverage], block: u64) {
    const SHOWN: usize = 10;
    for c in cov {
        let ids = |v: &[MarketId]| v.iter().take(SHOWN).map(|m| m.0).collect::<Vec<_>>();
        let unpriced = c.failed.len().saturating_add(c.no_anchor.len());
        if unpriced == 0 {
            tracing::info!(
                protocol = c.protocol.0,
                block,
                read = c.read,
                priced = c.priced,
                "protocol prices: every market with an oracle read is priced"
            );
        } else {
            tracing::warn!(
                protocol = c.protocol.0,
                block,
                read = c.read,
                priced = c.priced,
                read_failed = c.failed.len(),
                no_usd_anchor = c.no_anchor.len(),
                first_failed = ?ids(&c.failed),
                first_no_anchor = ?ids(&c.no_anchor),
                "protocol prices: markets unpriced after the first batch"
            );
        }
    }
}

fn quotes_in_usd(p: &BoundProtocol) -> bool {
    matches!(
        p,
        BoundProtocol::AaveV3(_)
            | BoundProtocol::AaveV4(_)
            | BoundProtocol::EulerV2(_)
            | BoundProtocol::Gearbox(_)
            | BoundProtocol::CompoundV2(_)
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use liq_protocol::Protocol;

    fn ray(v: u64) -> Ray {
        Ray::from_raw(alloy_primitives::U256::from(v))
    }

    /// A token only markets price (no canonical feed, no USD getter) is
    /// priced at the median of those markets: one odd oracle among three
    /// does not move it; with two, the lower; a USD getter, once one
    /// exists, comes first. Oracle: the median by hand.
    #[test]
    fn a_market_only_token_takes_the_median_market() {
        let (p, a) = (ProtocolId(2), AssetId(9));
        let mut book = ProtocolPriceBook::default();
        let batch = |block: u64, entries: Vec<QuotedPrice>| PriceBatch {
            block,
            entries,
            failed: 0,
        };
        let mut moves = Vec::new();
        book.apply(
            &batch(
                1,
                vec![
                    quote(p, MarketId(1), a, ray(100), false),
                    quote(p, MarketId(2), a, ray(9_000), false),
                ],
            ),
            &mut moves,
        );
        assert_eq!(book.usd_or_markets(a), Some(ray(100)), "two: the lower");
        book.apply(
            &batch(2, vec![quote(p, MarketId(3), a, ray(102), false)]),
            &mut moves,
        );
        assert_eq!(
            book.usd_or_markets(a),
            Some(ray(102)),
            "the odd 9,000 outvoted"
        );
        assert_eq!(book.usd_or_markets(AssetId(10)), None);
        book.apply(
            &batch(
                3,
                vec![quote(ProtocolId(1), MarketId(7), a, ray(101), true)],
            ),
            &mut moves,
        );
        assert_eq!(book.usd_or_markets(a), Some(ray(101)), "a USD getter first");
    }

    fn quote(p: ProtocolId, m: MarketId, a: AssetId, price: Ray, usd: bool) -> QuotedPrice {
        QuotedPrice {
            scale: None,
            protocol: p,
            market: m,
            asset: a,
            price,
            usd,
        }
    }

    struct NoRows;
    impl MarketRows for NoRows {
        fn rows(&self, _: MarketId) -> Option<&[liq_protocol::MarketRow]> {
            None
        }
    }

    /// Live: the committed configs, bound as production binds them, read
    /// every Aave V3 / Spark reserve price at head, and WETH's equals the
    /// pool oracle's own `getAssetPrice(WETH)` scaled to RAY.
    #[test]
    #[ignore = "needs MAINNET_RPC_URL"]
    fn live_aave_reads_price_every_reserve() {
        use alloy_primitives::{address, U256};
        use alloy_sol_types::{sol, SolCall};
        use liq_config::{Intern, Registry};
        sol! { function getAssetPrice(address asset) returns (uint256); }
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let load = crate::bind::load_protocols(&root.join("config"), &intern, None);
        let protocols: &'static [BoundProtocol] = Box::leak(load.protocols.into_boxed_slice());
        let mut reads = Vec::new();
        for (i, p) in protocols.iter().enumerate() {
            for r in p.as_dyn().price_reads(&NoRows) {
                reads.push((i, r));
            }
        }
        assert!(
            reads.len() >= 4,
            "Core, Prime, EtherFi, Spark: {}",
            reads.len()
        );
        let url = std::env::var("MAINNET_RPC_URL").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let rpc = HttpRpc::connect(&url).unwrap();
        let block = rt.block_on(rpc.block_number()).unwrap();
        let batch = rt.block_on(read_block(protocols, &rpc, &reads, block));
        assert_eq!(batch.failed, 0);
        let aave = intern.protocol("aave-v3").unwrap();
        let spark = intern.protocol("spark").unwrap();
        let n_aave = batch.entries.iter().filter(|e| e.protocol == aave).count();
        let n_spark = batch.entries.iter().filter(|e| e.protocol == spark).count();
        eprintln!(
            "block {block}: {} prices ({n_aave} aave-v3, {n_spark} spark)",
            batch.entries.len()
        );
        assert!(
            n_aave >= 75,
            "every priced Aave V3 reserve across 3 pools: {n_aave}"
        );
        assert!(n_spark >= 15, "Spark reserves: {n_spark}");

        let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let core_oracle = address!("0x54586be62e3c3580375ae3723c145253060ca0c2");
        let raw = rt
            .block_on(rpc.call_at(
                core_oracle,
                getAssetPriceCall { asset: weth }.abi_encode().into(),
                block,
            ))
            .unwrap();
        let scale = U256::from(10u64).pow(U256::from(19u64));
        let want = getAssetPriceCall::abi_decode_returns(&raw)
            .unwrap()
            .checked_mul(scale)
            .unwrap();
        let weth_id = intern.asset(weth).unwrap();
        let core = intern
            .markets()
            .iter()
            .find(|m| {
                m.key
                    == liq_config::OnChainId::Addr(address!(
                        "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2"
                    ))
            })
            .unwrap()
            .id;
        let got = batch
            .entries
            .iter()
            .find(|e| e.protocol == aave && e.market == core && e.asset == weth_id)
            .unwrap()
            .price;
        assert_eq!(
            got.raw(),
            want,
            "WETH = Core oracle getAssetPrice, to the wei"
        );
    }

    #[test]
    fn book_reports_first_prices_then_moves_and_ignores_stale_batches() {
        let (p, m, a) = (ProtocolId(1), MarketId(7), AssetId(3));
        let mut book = ProtocolPriceBook::default();
        let mut moves = Vec::new();
        let b1 = PriceBatch {
            block: 10,
            entries: vec![quote(p, m, a, ray(100), true)],
            failed: 0,
        };
        assert!(book.apply(&b1, &mut moves), "first price → resync");
        assert!(moves.is_empty());
        assert_eq!(book.patch(p, m), &[(a, ray(100))]);
        assert!(book.patch(p, MarketId(8)).is_empty());

        let b2 = PriceBatch {
            block: 11,
            entries: vec![quote(p, m, a, ray(90), true)],
            failed: 0,
        };
        assert!(!book.apply(&b2, &mut moves));
        assert_eq!(
            moves,
            vec![ProtocolPriceMove {
                protocol: p,
                market: m,
                asset: a,
                old: ray(100),
                new: ray(90)
            }]
        );
        // Same block again, or an older one: not re-applied.
        assert!(!book.apply(&b2, &mut moves));
        assert!(moves.is_empty());
        assert_eq!(book.applied(), 11);
        // Unchanged price: no move.
        let b3 = PriceBatch {
            block: 12,
            entries: vec![quote(p, m, a, ray(90), true)],
            failed: 0,
        };
        assert!(!book.apply(&b3, &mut moves));
        assert!(moves.is_empty());
        assert_eq!(book.usd(a).map(|r| r.raw()), Some(ray(90).raw()));
    }

    #[test]
    fn ratio_protocol_price_is_not_a_usd_price() {
        let mut book = ProtocolPriceBook::default();
        let mut moves = Vec::new();
        let batch = PriceBatch {
            block: 1,
            entries: vec![quote(
                ProtocolId(9),
                MarketId(1),
                AssetId(4),
                ray(50),
                false,
            )],
            failed: 0,
        };
        assert!(book.apply(&batch, &mut moves));
        assert!(book.usd(AssetId(4)).is_none());
        assert_eq!(
            book.patch(ProtocolId(9), MarketId(1)),
            &[(AssetId(4), ray(50))]
        );
    }

    fn call_retry(
        rt: &tokio::runtime::Runtime,
        rpc: &HttpRpc,
        to: alloy_primitives::Address,
        data: alloy_primitives::Bytes,
        block: u64,
    ) -> Result<alloy_primitives::Bytes, liq_config::ConfigError> {
        let mut last = liq_config::ConfigError::RpcUnavailable {
            cause: "no attempt".into(),
        };
        for _ in 0..6 {
            match rt.block_on(rpc.call_at(to, data.clone(), block)) {
                Ok(b) => return Ok(b),
                Err(e @ liq_config::ConfigError::CallFailed { .. }) => return Err(e),
                Err(e) => {
                    last = e;
                    std::thread::sleep(std::time::Duration::from_millis(350));
                }
            }
        }
        Err(last)
    }

    fn live_http() -> (tokio::runtime::Runtime, HttpRpc, u64) {
        let url = std::env::var("MAINNET_RPC_URL").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let rpc = HttpRpc::connect(&url).unwrap();
        let block = rt.block_on(rpc.block_number()).unwrap();
        (rt, rpc, block)
    }

    /// Chain: `AaveOracle.getReservePrice` on the spoke's own oracle, same block.
    #[test]
    #[ignore = "needs MAINNET_RPC_URL"]
    fn live_aave_v4_reserve_prices_match_the_oracle() {
        use alloy_primitives::{address, U256};
        use alloy_sol_types::{sol, SolCall};
        use liq_config::{Intern, Registry};
        use liq_protocol::MarketRow;
        sol! {
            struct SpokeReserve {
                address underlying;
                address hub;
                uint16 assetId;
                uint8 decimals;
                uint24 collateralRisk;
                uint8 flags;
                uint32 dynamicConfigKey;
            }
            function getReserveCount() external view returns (uint256);
            function getReserve(uint256 reserveId) external view returns (SpokeReserve);
            function getReservePrice(uint256 reserveId) external view returns (uint256);
        }
        struct One<'a> {
            market: MarketId,
            rows: &'a [MarketRow],
        }
        impl MarketRows for One<'_> {
            fn rows(&self, m: MarketId) -> Option<&[MarketRow]> {
                if m == self.market {
                    Some(self.rows)
                } else {
                    None
                }
            }
        }
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let load = crate::bind::load_protocols(&root.join("config"), &intern, None);
        let protocols: &'static [BoundProtocol] = Box::leak(load.protocols.into_boxed_slice());
        let aave = protocols
            .iter()
            .find_map(|p| match p {
                BoundProtocol::AaveV4(a) => Some(a),
                _ => None,
            })
            .unwrap();
        let spoke_addr = address!("0xe1900480ac69f0b296841cd01cc37546d92f35cd");
        let spoke = aave
            .config()
            .spokes
            .iter()
            .find(|s| s.address == spoke_addr)
            .unwrap();
        let (rt, rpc, block) = live_http();
        let count_raw = rt
            .block_on(rpc.call_at(
                spoke.address,
                getReserveCountCall {}.abi_encode().into(),
                block,
            ))
            .unwrap();
        let count = getReserveCountCall::abi_decode_returns(&count_raw).unwrap();
        let n = usize::try_from(count).unwrap();
        assert!(n > 0, "spoke has no reserves");
        let mut rows = vec![MarketRow::blank(AssetId(u16::MAX), 0); n.saturating_add(1)];
        let mut expect = Vec::new();
        for i in 0..n {
            let raw = rt
                .block_on(
                    rpc.call_at(
                        spoke.address,
                        getReserveCall {
                            reserveId: U256::from(i),
                        }
                        .abi_encode()
                        .into(),
                        block,
                    ),
                )
                .unwrap();
            let reserve = getReserveCall::abi_decode_returns(&raw).unwrap();
            let Some(id) = intern.asset(reserve.underlying) else {
                continue;
            };
            if !aave.config().assets.iter().any(|a| a.asset == id) {
                continue;
            }
            rows[i.saturating_add(1)] = MarketRow::blank(id, reserve.decimals);
            expect.push((i, id));
        }
        assert!(!expect.is_empty(), "no interned reserve on the spoke");
        let stub = One {
            market: spoke.market,
            rows: &rows,
        };
        let idx = protocols
            .iter()
            .position(|p| matches!(p, BoundProtocol::AaveV4(_)))
            .unwrap();
        let reads: Vec<_> = aave
            .price_reads(&stub)
            .into_iter()
            .map(|r| (idx, r))
            .collect();
        let batch = rt.block_on(read_block(protocols, &rpc, &reads, block));
        assert_eq!(batch.failed, 0);
        let scale = U256::from(10u64).pow(U256::from(19u64));
        for (i, id) in expect {
            let raw = rt
                .block_on(
                    rpc.call_at(
                        spoke.oracle,
                        getReservePriceCall {
                            reserveId: U256::from(i),
                        }
                        .abi_encode()
                        .into(),
                        block,
                    ),
                )
                .unwrap();
            let p = getReservePriceCall::abi_decode_returns(&raw).unwrap();
            let got = batch
                .entries
                .iter()
                .find(|e| e.asset == id && e.market == spoke.market);
            if p.is_zero() {
                assert!(got.is_none(), "a zero oracle price must not be published");
                continue;
            }
            let want = p.checked_mul(scale).unwrap();
            assert_eq!(got.unwrap().price.raw(), want, "reserve {i}");
        }
    }

    /// Chain: Compound `getUnderlyingPrice(cUSDC)` at the same block. 6 decimals → mantissa / 1000.
    #[test]
    #[ignore = "needs MAINNET_RPC_URL"]
    fn live_compound_cusdc_matches_get_underlying_price() {
        use alloy_primitives::{address, U256};
        use alloy_sol_types::{sol, SolCall};
        use liq_config::{Intern, Registry};
        sol! { function getUnderlyingPrice(address cToken) external view returns (uint256); }
        let cusdc = address!("0x39AA39c021dfbaE8faC545936693aC917d5E7563");
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let (rt, rpc, block) = live_http();
        struct Retry<'a> {
            rt: &'a tokio::runtime::Runtime,
            rpc: &'a HttpRpc,
        }
        impl liq_adapters_compound_v2::RegistryRpc for Retry<'_> {
            fn eth_call(
                &self,
                to: alloy_primitives::Address,
                data: &[u8],
                block: u64,
            ) -> core::result::Result<alloy_primitives::Bytes, liq_adapters_compound_v2::ConfigError>
            {
                match call_retry(
                    self.rt,
                    self.rpc,
                    to,
                    alloy_primitives::Bytes::copy_from_slice(data),
                    block,
                ) {
                    Ok(b) => Ok(b),
                    Err(liq_config::ConfigError::CallFailed { .. }) => {
                        Err(liq_adapters_compound_v2::ConfigError::RegistryCall(to))
                    }
                    Err(e) => panic!("compound registry call {to} failed: {e}"),
                }
            }
        }
        let raw = std::fs::read_to_string(root.join("config/protocols/compound-v2.toml")).unwrap();
        let mut cfg = liq_adapters_compound_v2::Config::from_toml(&raw).unwrap();
        cfg.bind_from_intern(&intern).unwrap();
        cfg.assert_live_registry(&Retry { rt: &rt, rpc: &rpc }, block)
            .unwrap();
        let compound = liq_adapters_compound_v2::CompoundV2::new(cfg).unwrap();
        let protocols: &'static [BoundProtocol] =
            Box::leak(vec![BoundProtocol::CompoundV2(compound)].into_boxed_slice());
        let BoundProtocol::CompoundV2(compound) = &protocols[0] else {
            unreachable!("just bound");
        };
        let mut oracle = None;
        let mut asset = None;
        for fork in &compound.config().forks {
            for c in &fork.ctokens {
                if c.ctoken == cusdc {
                    oracle = Some(fork.oracle);
                    asset = Some(c.underlying);
                }
            }
        }
        let oracle = oracle.unwrap();
        let underlying = asset.unwrap();
        let idx = protocols
            .iter()
            .position(|p| matches!(p, BoundProtocol::CompoundV2(_)))
            .unwrap();
        let reads: Vec<_> = compound
            .price_reads(&NoRows)
            .into_iter()
            .filter(|r| {
                r.calldata
                    .as_ref()
                    .windows(20)
                    .any(|w| w == cusdc.as_slice())
            })
            .map(|r| (idx, r))
            .collect();
        assert_eq!(reads.len(), 1);
        let batch = rt.block_on(read_block(protocols, &rpc, &reads, block));
        assert_eq!(batch.failed, 0);
        let raw = call_retry(
            &rt,
            &rpc,
            oracle,
            getUnderlyingPriceCall { cToken: cusdc }.abi_encode().into(),
            block,
        )
        .unwrap();
        let m = getUnderlyingPriceCall::abi_decode_returns(&raw).unwrap();
        assert!(!m.is_zero(), "cUSDC underlying price is zero");
        let want = m.checked_div(U256::from(1000u64)).unwrap();
        let id = intern.asset(underlying).unwrap();
        let got = batch.entries.iter().find(|e| e.asset == id).unwrap();
        assert_eq!(got.price.raw(), want, "cUSDC ray = mantissa / 1000");
    }

    /// Chain: `IOracle.price()` for 20 pinned markets. The overlay must
    /// reconstruct that integer, including prices that are not multiples of 1e9.
    #[test]
    #[ignore = "needs MAINNET_RPC_URL"]
    fn live_morpho_overlay_matches_oracle_price() {
        use alloy_primitives::U256;
        use alloy_sol_types::{sol, SolCall};
        use liq_adapters_morpho_blue::health::oracle_price;
        use liq_adapters_morpho_blue::layout::LoanRow;
        use liq_config::{Intern, Registry};
        use liq_protocol::MarketRow;
        sol! { function price() external view returns (uint256); }
        struct Book {
            start: u32,
            rows: Vec<Vec<MarketRow>>,
        }
        impl MarketRows for Book {
            fn rows(&self, m: MarketId) -> Option<&[MarketRow]> {
                let i = m.0.checked_sub(self.start)?;
                self.rows.get(usize::try_from(i).ok()?).map(Vec::as_slice)
            }
        }
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let load = crate::bind::load_protocols(&root.join("config"), &intern, None);
        let protocols: &'static [BoundProtocol] = Box::leak(load.protocols.into_boxed_slice());
        let morpho = protocols
            .iter()
            .find_map(|p| match p {
                BoundProtocol::MorphoBlue(m) => Some(m),
                _ => None,
            })
            .unwrap();
        let cfg = morpho.config();
        let pins: Vec<_> = cfg.price_sources.iter().take(20).collect();
        assert_eq!(pins.len(), 20);
        let dec = |id: AssetId| cfg.assets.iter().find(|a| a.asset == id).unwrap().decimals;
        let mut rows = Vec::new();
        for pin in &pins {
            let mut loan = MarketRow::blank(pin.loan, dec(pin.loan));
            let coll = MarketRow::blank(pin.collateral, dec(pin.collateral));
            let body = loan.body_mut::<LoanRow>().unwrap();
            body.oracle.copy_from_slice(pin.oracle.as_slice());
            body.coll_decimals = dec(pin.collateral);
            body.flags = LoanRow::PRICED;
            rows.push(vec![loan, coll]);
        }
        let book = Book {
            start: cfg.first_market.0,
            rows,
        };
        let idx = protocols
            .iter()
            .position(|p| matches!(p, BoundProtocol::MorphoBlue(_)))
            .unwrap();
        let reads: Vec<_> = morpho
            .price_reads(&book)
            .into_iter()
            .map(|r| (idx, r))
            .collect();
        assert_eq!(reads.len(), 20);
        let (rt, rpc, block) = live_http();
        let batch = rt.block_on(read_block(protocols, &rpc, &reads, block));
        let mut matched = 0usize;
        let mut reverted = 0usize;
        for (i, pin) in pins.iter().enumerate() {
            let market = MarketId(
                cfg.first_market
                    .0
                    .checked_add(u32::try_from(i).unwrap())
                    .unwrap(),
            );
            let raw = call_retry(
                &rt,
                &rpc,
                pin.oracle,
                priceCall {}.abi_encode().into(),
                block,
            );
            let Ok(raw) = raw else {
                reverted = reverted.saturating_add(1);
                let present = batch.entries.iter().any(|e| {
                    e.market == market && (e.asset == pin.loan || e.asset == pin.collateral)
                });
                assert!(!present, "reverted oracle {} was published", pin.oracle);
                continue;
            };
            let chain = priceCall::abi_decode_returns(&raw).unwrap();
            let loan = batch
                .entries
                .iter()
                .find(|e| e.market == market && e.asset == pin.loan);
            let coll = batch
                .entries
                .iter()
                .find(|e| e.market == market && e.asset == pin.collateral);
            if chain.is_zero() {
                assert!(
                    loan.is_none() && coll.is_none(),
                    "zero price() must not be published"
                );
                continue;
            }
            let loan = loan.unwrap().price.raw();
            let coll = coll.unwrap().price.raw();
            let back = oracle_price(coll, loan, dec(pin.collateral), dec(pin.loan)).unwrap();
            assert_eq!(back, chain, "market {market:?} oracle {}", pin.oracle);
            let rem = chain.checked_rem(U256::from(1_000_000_000u64)).unwrap();
            if rem != U256::ZERO {
                assert_ne!(
                    loan,
                    liq_types::fixed::RAY,
                    "1 RAY cannot represent this price()"
                );
            }
            matched = matched.saturating_add(1);
        }
        assert!(
            matched >= 19,
            "matched {matched}, reverted {reverted}, failed {}",
            batch.failed
        );
        assert_eq!(
            batch.failed, reverted,
            "decode failures beyond reverted oracles"
        );
    }

    /// One protocol bound as production binds it, with a live RPC at `block`.
    fn bind_live(
        push: impl FnOnce(
            &std::path::Path,
            (&crate::live_rpc::LiveRpc, u64),
            &mut crate::bind::ProtocolLoad,
        ),
    ) -> (
        &'static [BoundProtocol],
        tokio::runtime::Runtime,
        HttpRpc,
        u64,
    ) {
        let (rt, rpc, block) = live_http();
        let url = std::env::var("MAINNET_RPC_URL").unwrap();
        let live = crate::live_rpc::LiveRpc::new(HttpRpc::connect(&url).unwrap());
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut out = crate::bind::ProtocolLoad::default();
        push(&root.join("config/protocols"), (&live, block), &mut out);
        assert!(out.omitted.is_empty(), "omitted: {:?}", out.omitted);
        let protocols: &'static [BoundProtocol] = Box::leak(out.protocols.into_boxed_slice());
        (protocols, rt, rpc, block)
    }

    /// Chain: each branch's `PriceFeed.fetchPrice()` at the same block,
    /// × 1e9; BOLD at exactly 1 RAY.
    #[test]
    #[ignore = "needs MAINNET_RPC_URL"]
    fn live_liquity_branches_match_fetch_price() {
        use alloy_primitives::U256;
        use alloy_sol_types::{sol, SolCall};
        sol! { function fetchPrice() external returns (uint256 price, bool newOracleFailureDetected); }
        let (protocols, rt, rpc, block) =
            bind_live(|dir, live, out| crate::bind::push_liquity(dir, Some(live), out));
        let BoundProtocol::LiquityV2(l) = &protocols[0] else {
            panic!("liquity not bound");
        };
        let reads: Vec<_> = l.price_reads(&NoRows).into_iter().map(|r| (0, r)).collect();
        assert_eq!(reads.len(), l.config().branches.len());
        let batch = rt.block_on(read_block(protocols, &rpc, &reads, block));
        assert_eq!(batch.failed, 0);
        for b in &l.config().branches {
            let raw = call_retry(
                &rt,
                &rpc,
                b.price_feed,
                fetchPriceCall {}.abi_encode().into(),
                block,
            )
            .unwrap();
            let want = fetchPriceCall::abi_decode_returns(&raw).unwrap().price;
            assert!(!want.is_zero(), "{} fetchPrice is zero", b.coll_symbol);
            let got = batch
                .entries
                .iter()
                .find(|e| e.market == b.market && e.asset == b.coll_asset)
                .unwrap();
            assert_eq!(
                got.price.raw(),
                want.checked_mul(U256::from(1_000_000_000u64)).unwrap(),
                "{} at block {block}",
                b.coll_symbol
            );
            let bold = batch
                .entries
                .iter()
                .find(|e| e.market == b.market && e.asset == l.config().bold.asset)
                .unwrap();
            assert_eq!(bold.price.raw(), liq_types::fixed::RAY);
        }
    }

    /// Chain: `PriceOracleV3.convertToUSD(10^decimals, token)` (the call
    /// `calcDebtAndCollateral` values collateral with) at the same block,
    /// for every mapped token of every manager. A token whose feed reverts
    /// must not be published.
    #[test]
    #[ignore = "needs MAINNET_RPC_URL"]
    fn live_gearbox_prices_match_convert_to_usd() {
        use alloy_primitives::U256;
        use alloy_sol_types::{sol, SolCall};
        sol! { function convertToUSD(uint256 amount, address token) external view returns (uint256); }
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let intern = liq_config::Intern::from_registry(
            &liq_config::Registry::from_path(&root.join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        let (protocols, rt, rpc, block) =
            bind_live(|dir, live, out| crate::bind::push_gearbox(dir, &intern, Some(live), out));
        let BoundProtocol::Gearbox(g) = &protocols[0] else {
            panic!("gearbox not bound");
        };
        let reads: Vec<_> = g.price_reads(&NoRows).into_iter().map(|r| (0, r)).collect();
        assert!(!reads.is_empty());
        let batch = rt.block_on(read_block(protocols, &rpc, &reads, block));
        // One chain call per (oracle, token): managers share 12 oracles.
        let mut chain: std::collections::HashMap<
            (alloy_primitives::Address, alloy_primitives::Address),
            Option<U256>,
        > = std::collections::HashMap::new();
        let (mut matched, mut reverted, mut zero) = (0usize, 0usize, 0usize);
        for m in &g.config().managers {
            for t in m
                .tokens
                .iter()
                .filter(|t| t.asset != liq_adapters_gearbox::UNMAPPED_ASSET && !t.token.is_zero())
            {
                let usd8 = *chain.entry((m.price_oracle, t.token)).or_insert_with(|| {
                    let unit = U256::from(10u64).pow(U256::from(t.decimals));
                    let data = convertToUSDCall {
                        amount: unit,
                        token: t.token,
                    }
                    .abi_encode()
                    .into();
                    match call_retry(&rt, &rpc, m.price_oracle, data, block) {
                        Ok(raw) => Some(convertToUSDCall::abi_decode_returns(&raw).unwrap()),
                        Err(liq_config::ConfigError::CallFailed { .. }) => None,
                        Err(e) => panic!("rpc error on {} {}: {e}", m.price_oracle, t.token),
                    }
                });
                let got = batch
                    .entries
                    .iter()
                    .find(|e| e.market == m.market && e.asset == t.asset);
                let Some(usd8) = usd8.filter(|p| !p.is_zero()) else {
                    if usd8.is_some() {
                        zero = zero.saturating_add(1);
                    } else {
                        reverted = reverted.saturating_add(1);
                    }
                    assert!(
                        got.is_none(),
                        "reverting or zero token {} was published",
                        t.token
                    );
                    continue;
                };
                let want = usd8
                    .checked_mul(U256::from(10u64).pow(U256::from(19u64)))
                    .unwrap();
                assert_eq!(
                    got.expect("priced token not published").price.raw(),
                    want,
                    "manager {} token {}",
                    m.manager,
                    t.token
                );
                matched = matched.saturating_add(1);
            }
        }
        eprintln!(
            "gearbox block {block}: {matched} prices matched, {reverted} reverted, {zero} zero, {} failed reads, {} distinct calls",
            batch.failed,
            chain.len()
        );
        assert!(matched > 0);
        assert_eq!(
            batch.failed, reverted,
            "failed reads beyond reverting feeds"
        );
    }

    /// Chain: admitted Euler vaults with the USD unit of account. The
    /// overlay's debt-asset price equals `getQuote(10^decimals, asset, USD)`
    /// on the vault's own oracle router, × 1e9, at the same block.
    #[test]
    #[ignore = "needs MAINNET_RPC_URL"]
    fn live_euler_debt_prices_match_get_quote() {
        use alloy_primitives::{Address, U256};
        use alloy_sol_types::{sol, SolCall};
        use liq_adapters_euler_v2::layout::VaultRow;
        use liq_config::{Intern, Registry};
        use liq_protocol::MarketRow;
        sol! {
            function asset() external view returns (address);
            function oracle() external view returns (address);
            function unitOfAccount() external view returns (address);
            function getQuote(uint256 inAmount, address base, address quote) external view returns (uint256);
        }
        struct Book(Vec<(MarketId, Vec<MarketRow>)>);
        impl MarketRows for Book {
            fn rows(&self, m: MarketId) -> Option<&[MarketRow]> {
                self.0
                    .iter()
                    .find(|(id, _)| *id == m)
                    .map(|(_, r)| r.as_slice())
            }
        }
        let usd = alloy_primitives::address!("0x0000000000000000000000000000000000000348");
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let load = crate::bind::load_protocols(&root.join("config"), &intern, None);
        let protocols: &'static [BoundProtocol] = Box::leak(load.protocols.into_boxed_slice());
        let idx = protocols
            .iter()
            .position(|p| matches!(p, BoundProtocol::EulerV2(_)))
            .unwrap();
        let BoundProtocol::EulerV2(e) = &protocols[idx] else {
            unreachable!("just found");
        };
        let cfg = e.config();
        let (rt, rpc, block) = live_http();
        let get = |to: Address, data: Vec<u8>| call_retry(&rt, &rpc, to, data.into(), block);
        let word = |b: alloy_primitives::Bytes| Address::from_slice(&b[12..32]);
        let mut book = Vec::new();
        let mut expect = Vec::new();
        for &(vault, market) in &cfg.interned {
            let (Ok(a), Ok(o), Ok(u)) = (
                get(vault, assetCall {}.abi_encode()),
                get(vault, oracleCall {}.abi_encode()),
                get(vault, unitOfAccountCall {}.abi_encode()),
            ) else {
                continue;
            };
            let (asset, oracle, unit) = (word(a), word(o), word(u));
            if unit != usd || oracle.is_zero() {
                continue;
            }
            let Some(ac) = cfg.assets.iter().find(|x| x.underlying == asset) else {
                continue;
            };
            let mut row = MarketRow::blank(ac.asset, ac.decimals);
            let v = row.body_mut::<VaultRow>().unwrap();
            v.vault.copy_from_slice(vault.as_slice());
            v.underlying.copy_from_slice(asset.as_slice());
            v.oracle.copy_from_slice(oracle.as_slice());
            v.unit_of_account.copy_from_slice(unit.as_slice());
            v.flags = VaultRow::PRICED;
            book.push((market, vec![row]));
            expect.push((market, ac.asset, asset, ac.decimals, oracle));
            if expect.len() >= 8 {
                break;
            }
        }
        assert!(
            !expect.is_empty(),
            "no admitted USD-unit vault with an interned asset"
        );
        let reads: Vec<_> = e
            .price_reads(&Book(book))
            .into_iter()
            .map(|r| (idx, r))
            .collect();
        let batch = rt.block_on(read_block(protocols, &rpc, &reads, block));
        let mut matched = 0usize;
        for (market, id, asset, dec, oracle) in expect {
            let got = batch
                .entries
                .iter()
                .find(|x| x.market == market && x.asset == id);
            let q = getQuoteCall {
                inAmount: U256::from(10u64).pow(U256::from(dec)),
                base: asset,
                quote: usd,
            };
            let Ok(raw) = get(oracle, q.abi_encode()) else {
                assert!(got.is_none(), "reverting quote for {asset} was published");
                continue;
            };
            let want = getQuoteCall::abi_decode_returns(&raw).unwrap();
            if want.is_zero() {
                assert!(got.is_none());
                continue;
            }
            let want = want.checked_mul(U256::from(1_000_000_000u64)).unwrap();
            assert_eq!(
                got.unwrap().price.raw(),
                want,
                "market {market:?} asset {asset}"
            );
            matched = matched.saturating_add(1);
        }
        eprintln!("euler block {block}: {matched} vault prices matched");
        assert!(matched > 0);
    }

    fn usd_ray(dollars: u64) -> Ray {
        Ray::from_raw(alloy_primitives::U256::from(dollars) * liq_types::fixed::RAY)
    }

    /// A ratio read whose numeraire has no USD price (a Silo pair quoted in
    /// a virtual unit) is anchored on its other asset: that asset becomes its
    /// own dollar price and the pair keeps the protocol's ratio.
    #[test]
    fn a_ratio_read_without_a_numeraire_price_anchors_on_its_other_asset() {
        let (unit, coll, m) = (AssetId(0), AssetId(1), MarketId(7001));
        let (p_unit, p_coll) = (
            Ray::from_raw(alloy_primitives::U256::from(999_871_000_000_000_000u128)),
            Ray::from_raw(alloy_primitives::U256::from(1_027_341_680_000_000_000u128)),
        );
        let q = |asset, p: Ray| QuotedPrice {
            scale: Some((unit, p_unit)),
            ..quote(ProtocolId(6), m, asset, p, false)
        };
        let out = to_usd(&[q(unit, p_unit), q(coll, p_coll)], |a| {
            (a == coll).then(|| usd_ray(100_000))
        });
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].price, usd_ray(100_000));
        // unit / coll in USD is the protocol's unit / coll, to one unit.
        let lhs = out[0].price.raw() * p_coll.raw();
        let rhs = p_unit.raw() * out[1].price.raw();
        let diff = if lhs > rhs { lhs - rhs } else { rhs - lhs };
        assert!(diff <= p_coll.raw(), "ratio moved by {diff}");
        // Neither asset priced: nothing overlaid.
        assert!(to_usd(&[q(unit, p_unit), q(coll, p_coll)], |_| None).is_empty());
    }

    /// Morpho: the pair the decode builds reproduces `price()` exactly; after
    /// restating in USD the loan is its dollar price and `oracle_price` still
    /// reproduces `price()` to within one unit of the collateral price, i.e.
    /// a relative error below 1e-24 — far under the WAD health factor.
    #[test]
    fn morpho_pair_restated_in_usd_keeps_the_oracle_price() {
        use liq_adapters_morpho_blue::health::{oracle_price, prices_matching_oracle};
        let (loan, coll, m) = (AssetId(0), AssetId(1), MarketId(5001));
        // wstETH (18) in WETH (18): price() = 1.2 * 1e36 plus odd wei.
        let price = alloy_primitives::U256::from(1_200_000_000_000_000_123u64)
            * alloy_primitives::U256::from(10u64).pow(alloy_primitives::U256::from(18u64));
        let (p_loan, p_coll) = prices_matching_oracle(price, 18, 18).unwrap();
        let scale = Some((loan, Ray::from_raw(p_loan)));
        let q = |asset, p| QuotedPrice {
            scale,
            ..quote(ProtocolId(9), m, asset, Ray::from_raw(p), false)
        };
        let out = to_usd(&[q(loan, p_loan), q(coll, p_coll)], |a| {
            (a == loan).then(|| usd_ray(3_000))
        });
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].price, usd_ray(3_000), "the loan is its dollar price");
        let back = oracle_price(out[1].price.raw(), out[0].price.raw(), 18, 18).unwrap();
        let diff = if back > price {
            back - price
        } else {
            price - back
        };
        assert!(
            diff * alloy_primitives::U256::from(10u64).pow(alloy_primitives::U256::from(24u64))
                < price
        );
        // Collateral in dollars: 1.2 * 3000.
        let usd = out[1].price.raw() / liq_types::fixed::RAY;
        assert_eq!(usd, alloy_primitives::U256::from(3_600u64));
    }

    /// The first batch after start carries a Morpho read and the Aave
    /// getter that prices its loan token. The book is still empty, so the
    /// numeraire's USD price comes from the batch itself; once applied, from
    /// the book. A ratio read is never a USD source. Oracle: the batch's own
    /// numbers.
    #[test]
    fn a_numeraire_priced_in_the_same_batch_is_used_before_the_book_has_it() {
        let (usdc, lp, m) = (AssetId(677), AssetId(129), MarketId(5001));
        let getter = quote(ProtocolId(0), MarketId(1), usdc, ray(1), true);
        let ratio = QuotedPrice {
            scale: Some((usdc, ray(1))),
            ..quote(ProtocolId(5), m, lp, ray(2), false)
        };
        let batch = PriceBatch {
            block: 7,
            entries: vec![getter, ratio],
            failed: 0,
        };
        let mut book = ProtocolPriceBook::default();
        assert_eq!(book.usd(usdc), None, "empty before the first batch");
        assert_eq!(book.usd_or_batch(usdc, &batch), Some(ray(1)));
        assert_eq!(
            book.usd_or_batch(lp, &batch),
            None,
            "a ratio read is not a USD price"
        );
        let mut moves = Vec::new();
        book.apply(&batch, &mut moves);
        assert_eq!(
            book.usd_or_batch(usdc, &PriceBatch::default()),
            Some(ray(1))
        );
    }

    /// A collateral with no USD source anywhere is sized at its own market's
    /// overlay price; with a USD source, that wins; another market's
    /// overlay is never borrowed. Oracle: the overlay entries applied.
    #[test]
    fn a_collateral_only_its_market_prices_is_sized_at_that_price() {
        let (lp, usdc, m, other) = (AssetId(129), AssetId(677), MarketId(5001), MarketId(5002));
        let morpho = ProtocolId(5);
        let mut book = ProtocolPriceBook::default();
        let mut moves = Vec::new();
        book.apply(
            &PriceBatch {
                block: 1,
                entries: vec![
                    quote(morpho, m, lp, ray(3), false),
                    quote(ProtocolId(0), MarketId(1), usdc, ray(1), true),
                ],
                failed: 0,
            },
            &mut moves,
        );
        assert_eq!(book.usd(lp), None);
        assert_eq!(book.usd_or_market(lp, morpho, m), Some(ray(3)));
        assert_eq!(book.usd_or_market(lp, morpho, other), None);
        assert_eq!(
            book.usd_or_market(usdc, morpho, m),
            Some(ray(1)),
            "a USD source wins"
        );
    }

    /// No USD price for the numeraire: the read is left out, not priced at 1.
    /// Dollar quotes pass through untouched.
    #[test]
    fn ratio_without_a_numeraire_price_is_left_out() {
        let (a, b, m) = (AssetId(0), AssetId(1), MarketId(1));
        let scale = Some((a, ray(10)));
        let ratio = |asset| QuotedPrice {
            scale,
            ..quote(ProtocolId(9), m, asset, ray(10), false)
        };
        let dollar = quote(ProtocolId(1), m, b, ray(7), true);
        let out = to_usd(&[ratio(a), ratio(b), dollar], |_| None);
        assert_eq!(out, vec![dollar]);
    }
}
