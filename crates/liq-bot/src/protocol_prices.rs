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

/// One decoded protocol price. `usd` is false for ratio protocols
/// (Morpho, Silo, Fluid, Liquity): their prices reconstruct a health
/// ratio and are not dollars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuotedPrice {
    pub protocol: ProtocolId,
    pub market: MarketId,
    pub asset: AssetId,
    pub price: Ray,
    pub usd: bool,
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
                    let usd = quotes_in_usd(p);
                    batch
                        .entries
                        .extend(decoded.iter().map(|&(asset, price)| QuotedPrice {
                            protocol: proto.id(),
                            market: read.market,
                            asset,
                            price,
                            usd,
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
    applied: u64,
}

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

    fn quote(p: ProtocolId, m: MarketId, a: AssetId, price: Ray, usd: bool) -> QuotedPrice {
        QuotedPrice {
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
}
