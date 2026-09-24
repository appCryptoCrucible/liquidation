//! Startup seed of UniV3 pool state. Without it a V3 pool only learns its
//! price and in-range liquidity from its next `Swap` log, and its tick map
//! only from new `Mint`/`Burn`s — so after a restart every exit is sized
//! against empty or partial pools.
//!
//! Every read is pinned to one block and batched through Multicall3:
//! `slot0` + `liquidity` for each pool, then the initialized ticks in a
//! window around the current tick via Uniswap's TickLens. The window covers
//! at least ±[`WINDOW_TICKS`] ticks (≈ ±30 % price), which bounds the exit
//! size the solver will quote — beyond it the solver refuses rather than
//! guessing liquidity. Logs keep the state current from here on.
//!
//! V2 pairs are seeded from `getReserves` (every later `Sync` is absolute).
//! Curve pools cannot be folded from logs, so every pool log marks one
//! stale and [`spawn_curve_reseed`] re-reads it off the hot path, pinned to
//! one block; a read older than the log that made it stale is refused.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{address, Address, Bytes, U256};
use alloy_sol_types::{sol, SolCall};
use liq_config::rpc::{ChainRpc, HttpRpc};
use liq_router::{PoolBook, PoolId, PoolState, Tick};
use parking_lot::RwLock;

sol! {
    struct Call3 { address target; bool allowFailure; bytes callData; }
    struct Result3 { bool success; bytes returnData; }
    function aggregate3(Call3[] calls) returns (Result3[] returnData);

    function slot0() returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked);
    function liquidity() returns (uint128);

    struct PopulatedTick { int24 tick; int128 liquidityNet; uint128 liquidityGross; }
    function getPopulatedTicksInWord(address pool, int16 tickBitmapIndex) returns (PopulatedTick[] populatedTicks);

    function getReserves() returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);

    function balances(uint256 i) returns (uint256);
    function A() returns (uint256);
    function A_precise() returns (uint256);
    function fee() returns (uint256);
}

/// Multicall3, same address on every EVM chain.
const MULTICALL3: Address = address!("0xcA11bde05977b3631167028862bE2a173976CA11");
/// Uniswap V3 TickLens (mainnet).
const TICK_LENS: Address = address!("0xbfd8137f7d1516D3ea5cA83523914859ec47F573");
/// ln(1.3) / ln(1.0001) ≈ 2624 ticks ≈ ±30 % price.
const WINDOW_TICKS: i32 = 2_624;
const BATCH: usize = 150;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SeedStats {
    pub pools: usize,
    pub seeded: usize,
    pub failed: usize,
}

/// Bitmap words covering `WINDOW_TICKS` either side of `tick`.
fn words(tick: i32, spacing: i32) -> Option<(i16, i16)> {
    if spacing <= 0 {
        return None;
    }
    let compressed = tick.div_euclid(spacing);
    let word = compressed.div_euclid(256);
    let per_word = spacing.checked_mul(256)?;
    let k = WINDOW_TICKS.div_euclid(per_word).checked_add(1)?;
    let lo = i16::try_from(word.checked_sub(k)?).ok()?;
    let hi = i16::try_from(word.checked_add(k)?).ok()?;
    Some((lo, hi))
}

async fn aggregate(rpc: &HttpRpc, calls: Vec<Call3>, block: u64) -> Option<Vec<Result3>> {
    let data = Bytes::from(aggregate3Call { calls }.abi_encode());
    let raw = rpc.call_at(MULTICALL3, data, block).await.ok()?;
    aggregate3Call::abi_decode_returns(&raw).ok()
}

/// Seed every V3 pool in `book` at the current head. Pools whose reads fail
/// stay unseeded (logs fill them in over time) and are counted.
pub async fn seed_v3(book: &mut PoolBook, rpc: &HttpRpc) -> SeedStats {
    let mut stats = SeedStats::default();
    let block = match rpc.block_number().await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "V3 seed skipped — head unavailable");
            return stats;
        }
    };
    let v3: Vec<(usize, Address, i32)> = book
        .pools()
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match &p.state {
            PoolState::V3(s) => Some((i, p.address, s.tick_spacing)),
            _ => None,
        })
        .collect();
    stats.pools = v3.len();

    // Phase 1: slot0 + liquidity.
    let mut heads: Vec<Option<(U256, i32, u128)>> = vec![None; v3.len()];
    for (chunk_i, chunk) in v3.chunks(BATCH / 2).enumerate() {
        let mut calls = Vec::with_capacity(chunk.len().saturating_mul(2));
        for &(_, addr, _) in chunk {
            calls.push(Call3 {
                target: addr,
                allowFailure: true,
                callData: slot0Call {}.abi_encode().into(),
            });
            calls.push(Call3 {
                target: addr,
                allowFailure: true,
                callData: liquidityCall {}.abi_encode().into(),
            });
        }
        let Some(res) = aggregate(rpc, calls, block).await else {
            tracing::error!(chunk = chunk_i, "V3 seed: slot0/liquidity batch failed");
            continue;
        };
        for (j, pair) in res.chunks(2).enumerate() {
            let [s0, liq] = pair else { continue };
            if !s0.success || !liq.success {
                continue;
            }
            let (Ok(s), Ok(l)) = (
                slot0Call::abi_decode_returns(&s0.returnData),
                liquidityCall::abi_decode_returns(&liq.returnData),
            ) else {
                continue;
            };
            if let Some(slot) = heads.get_mut(chunk_i.saturating_mul(BATCH / 2).saturating_add(j)) {
                *slot = Some((U256::from(s.sqrtPriceX96), s.tick.as_i32(), l));
            }
        }
    }

    // Phase 2: populated ticks in the window.
    let mut jobs: Vec<(usize, i16)> = Vec::new();
    for (k, (&(_, _, spacing), head)) in v3.iter().zip(&heads).enumerate() {
        let Some((_, tick, _)) = head else { continue };
        let Some((lo, hi)) = words(*tick, spacing) else {
            continue;
        };
        for w in lo..=hi {
            jobs.push((k, w));
        }
    }
    let mut ticks: Vec<Vec<Tick>> = vec![Vec::new(); v3.len()];
    let mut tick_ok: Vec<bool> = vec![true; v3.len()];
    for chunk in jobs.chunks(BATCH) {
        let calls = chunk
            .iter()
            .filter_map(|&(k, w)| {
                v3.get(k).map(|&(_, addr, _)| Call3 {
                    target: TICK_LENS,
                    allowFailure: true,
                    callData: getPopulatedTicksInWordCall {
                        pool: addr,
                        tickBitmapIndex: w,
                    }
                    .abi_encode()
                    .into(),
                })
            })
            .collect();
        let res = aggregate(rpc, calls, block).await;
        for (i, &(k, _)) in chunk.iter().enumerate() {
            let decoded = res
                .as_ref()
                .and_then(|r| r.get(i))
                .filter(|r| r.success)
                .and_then(|r| getPopulatedTicksInWordCall::abi_decode_returns(&r.returnData).ok());
            match decoded {
                Some(list) => {
                    if let Some(v) = ticks.get_mut(k) {
                        v.extend(list.into_iter().map(|t| Tick {
                            tick: t.tick.as_i32(),
                            net: t.liquidityNet,
                            gross: t.liquidityGross,
                        }));
                    }
                }
                None => {
                    if let Some(ok) = tick_ok.get_mut(k) {
                        *ok = false;
                    }
                }
            }
        }
    }

    for (k, &(pool_i, addr, _)) in v3.iter().enumerate() {
        let (Some(Some((sqrt, tick, liq))), Some(true)) = (heads.get(k), tick_ok.get(k)) else {
            stats.failed = stats.failed.saturating_add(1);
            continue;
        };
        let Some(pool) = u32::try_from(pool_i)
            .ok()
            .and_then(|i| book.get_mut(liq_router::PoolId(i)))
        else {
            stats.failed = stats.failed.saturating_add(1);
            continue;
        };
        let PoolState::V3(s) = &mut pool.state else {
            continue;
        };
        let mut t = std::mem::take(ticks.get_mut(k).unwrap_or(&mut Vec::new()));
        t.sort_unstable_by_key(|x| x.tick);
        t.dedup_by_key(|x| x.tick);
        s.sqrt_price_x96 = *sqrt;
        s.tick = *tick;
        s.liquidity = *liq;
        s.ticks = t;
        stats.seeded = stats.seeded.saturating_add(1);
        tracing::debug!(pool = %addr, ticks = s.ticks.len(), "V3 pool seeded");
    }
    tracing::info!(
        pools = stats.pools,
        seeded = stats.seeded,
        failed = stats.failed,
        block,
        "UniV3 pool state seeded"
    );
    stats
}

fn call(target: Address, data: Vec<u8>) -> Call3 {
    Call3 {
        target,
        allowFailure: true,
        callData: data.into(),
    }
}

fn pool_mut(book: &mut PoolBook, i: usize) -> Option<&mut liq_router::Pool> {
    u32::try_from(i).ok().and_then(|i| book.get_mut(PoolId(i)))
}

/// Seed every V2 pair in `book` from `getReserves` at the current head.
pub async fn seed_v2(book: &mut PoolBook, rpc: &HttpRpc) -> SeedStats {
    let mut stats = SeedStats::default();
    let block = match rpc.block_number().await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "V2 seed skipped — head unavailable");
            return stats;
        }
    };
    let v2: Vec<(usize, Address)> = book
        .pools()
        .iter()
        .enumerate()
        .filter(|(_, p)| matches!(p.state, PoolState::V2(_)))
        .map(|(i, p)| (i, p.address))
        .collect();
    stats.pools = v2.len();
    for chunk in v2.chunks(BATCH) {
        let calls = chunk
            .iter()
            .map(|&(_, a)| call(a, getReservesCall {}.abi_encode()))
            .collect();
        let res = aggregate(rpc, calls, block).await;
        for (k, &(pool_i, addr)) in chunk.iter().enumerate() {
            let got = res
                .as_ref()
                .and_then(|r| r.get(k))
                .filter(|r| r.success)
                .and_then(|r| getReservesCall::abi_decode_returns(&r.returnData).ok());
            let (Some(r), Some(pool)) = (got, pool_mut(book, pool_i)) else {
                stats.failed = stats.failed.saturating_add(1);
                continue;
            };
            if let PoolState::V2(s) = &mut pool.state {
                s.reserve0 = U256::from(r.reserve0);
                s.reserve1 = U256::from(r.reserve1);
                stats.seeded = stats.seeded.saturating_add(1);
                tracing::debug!(pool = %addr, "V2 pair seeded");
            }
        }
    }
    tracing::info!(
        pools = stats.pools,
        seeded = stats.seeded,
        failed = stats.failed,
        block,
        "UniV2/Sushi pair reserves seeded"
    );
    stats
}

/// One Curve pool read at a block: `balances`, `A` as stored, its
/// `A_PRECISION`, `fee` (1e10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurveRead {
    pub balances: Vec<U256>,
    pub a: U256,
    pub a_precision: U256,
    pub fee: U256,
}

/// `A_precise()` answering means `A_PRECISION = 100` and it is the stored
/// value; otherwise (3pool-era pools) `A()` is stored as-is.
fn curve_amp(precise: Option<U256>, plain: Option<U256>) -> Option<(U256, U256)> {
    match (precise, plain) {
        (Some(p), _) if !p.is_zero() => Some((p, U256::from(100u64))),
        (_, Some(a)) if !a.is_zero() => Some((a, U256::from(1u64))),
        _ => None,
    }
}

/// Read `pools` (`(address, n_coins)`) at `block`. `None` for a pool whose
/// read failed or was incomplete — it stays stale.
async fn read_curve(
    rpc: &HttpRpc,
    pools: &[(Address, usize)],
    block: u64,
) -> Vec<Option<CurveRead>> {
    let mut out = Vec::with_capacity(pools.len());
    // n balances + A_precise + A + fee per pool: at most 7 calls each.
    for chunk in pools.chunks(BATCH / 8) {
        let mut calls = Vec::new();
        for &(addr, n) in chunk {
            for i in 0..n {
                calls.push(call(addr, balancesCall { i: U256::from(i) }.abi_encode()));
            }
            calls.push(call(addr, A_preciseCall {}.abi_encode()));
            calls.push(call(addr, ACall {}.abi_encode()));
            calls.push(call(addr, feeCall {}.abi_encode()));
        }
        let res = aggregate(rpc, calls, block).await;
        // Every read here returns one `uint256`.
        let row = |k: usize| {
            res.as_ref()
                .and_then(|r| r.get(k))
                .filter(|r| r.success)
                .and_then(|r| balancesCall::abi_decode_returns(&r.returnData).ok())
        };
        let mut at = 0usize;
        for &(_, n) in chunk {
            let balances: Option<Vec<U256>> = (0..n).map(|i| row(at.saturating_add(i))).collect();
            let precise = row(at.saturating_add(n));
            let plain = row(at.saturating_add(n).saturating_add(1));
            let fee = row(at.saturating_add(n).saturating_add(2));
            at = at.saturating_add(n).saturating_add(3);
            out.push(match (balances, curve_amp(precise, plain), fee) {
                (Some(balances), Some((a, a_precision)), Some(fee)) => Some(CurveRead {
                    balances,
                    a,
                    a_precision,
                    fee,
                }),
                _ => None,
            });
        }
    }
    out
}

/// Every Curve pool (`only_stale` → just the stale ones) as
/// `(book index, address, n_coins)`.
fn curve_targets(book: &PoolBook, only_stale: bool) -> Vec<(usize, Address, usize)> {
    book.pools()
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match &p.state {
            PoolState::Curve(c) if !only_stale || c.stale => Some((i, p.address, c.rates.len())),
            _ => None,
        })
        .collect()
}

/// Read the targets at the head and apply what is still current. Returns
/// `(read, applied)`.
async fn refresh_curve(
    book: &RwLock<PoolBook>,
    rpc: &HttpRpc,
    targets: &[(usize, Address, usize)],
) -> (usize, usize) {
    let block = match rpc.block_number().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "curve reseed: head unavailable");
            return (0, 0);
        }
    };
    let query: Vec<(Address, usize)> = targets.iter().map(|&(_, a, n)| (a, n)).collect();
    let reads = read_curve(rpc, &query, block).await;
    let read = reads.iter().filter(|r| r.is_some()).count();
    let mut applied = 0usize;
    let mut w = book.write();
    for (&(i, addr, _), r) in targets.iter().zip(reads) {
        let Some(r) = r else {
            tracing::debug!(pool = %addr, block, "curve read failed — stays stale");
            continue;
        };
        let Ok(id) = u32::try_from(i).map(PoolId) else {
            continue;
        };
        match w.reseed_curve(id, &r.balances, r.a, r.a_precision, r.fee, block) {
            Ok(true) => applied = applied.saturating_add(1),
            Ok(false) => {}
            Err(e) => tracing::error!(pool = %addr, error = ?e, "curve reseed refused"),
        }
    }
    (read, applied)
}

/// Seed every Curve pool in `book` at the current head.
pub async fn seed_curve(book: &mut PoolBook, rpc: &HttpRpc) -> SeedStats {
    let targets = curve_targets(book, false);
    let lock = RwLock::new(std::mem::replace(
        book,
        PoolBook::new(std::collections::HashMap::new(), None, 0),
    ));
    let (read, applied) = refresh_curve(&lock, rpc, &targets).await;
    *book = lock.into_inner();
    let stats = SeedStats {
        pools: targets.len(),
        seeded: applied,
        failed: targets.len().saturating_sub(read),
    };
    tracing::info!(
        pools = stats.pools,
        seeded = stats.seeded,
        failed = stats.failed,
        "Curve pool state seeded"
    );
    stats
}

/// How often the Curve reseed thread looks for stale pools.
const CURVE_POLL: Duration = Duration::from_millis(500);

/// Off-hot-path thread that re-reads every stale Curve pool (one
/// block-pinned Multicall3 read per poll) and applies it under a brief write
/// lock. A stale pool is not routed meanwhile — never quoted from a guess.
pub fn spawn_curve_reseed(
    book: Arc<RwLock<PoolBook>>,
    rpc_url: String,
    stop: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>, std::io::Error> {
    std::thread::Builder::new()
        .name("liq-bot-curve".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "curve reseed runtime refused — stale pools stay unrouted");
                    return;
                }
            };
            let rpc = match HttpRpc::connect(&rpc_url) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "curve reseed RPC connect failed — stale pools stay unrouted");
                    return;
                }
            };
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(CURVE_POLL);
                let targets = curve_targets(&book.read(), true);
                if targets.is_empty() {
                    continue;
                }
                let (read, applied) = rt.block_on(refresh_curve(&book, &rpc, &targets));
                tracing::debug!(stale = targets.len(), read, applied, "curve reseed");
            }
        })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    /// Live check against mainnet (`MAINNET_RPC_URL`): seed the USDC/WETH
    /// 0.05 % pool and quote a 10 WETH exit on it.
    #[tokio::test]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn seeds_live_usdc_weth_and_quotes_an_exit() {
        use liq_router::{solve_pair, GasTerms, Pool, SolveBudget, V3State};
        use liq_types::AssetId;
        let url = std::env::var("MAINNET_RPC_URL").expect("MAINNET_RPC_URL");
        let rpc = HttpRpc::connect(&url).unwrap();
        let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let (a_usdc, a_weth) = (AssetId(0), AssetId(1));
        let mut assets = std::collections::HashMap::new();
        assets.insert(usdc, a_usdc);
        assets.insert(weth, a_weth);
        let mut book = PoolBook::new(assets, None, 100_000);
        book.add(Pool {
            address: address!("0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            assets: smallvec::SmallVec::from_slice(&[a_usdc, a_weth]),
            tokens: smallvec::SmallVec::from_slice(&[usdc, weth]),
            hop_gas: 100_000,
            state: PoolState::V3(V3State {
                sqrt_price_x96: U256::ZERO,
                tick: 0,
                liquidity: 0,
                fee_pips: 500,
                tick_spacing: 10,
                ticks: Vec::new(),
            }),
        })
        .unwrap();
        let st = seed_v3(&mut book, &rpc).await;
        assert_eq!((st.pools, st.seeded, st.failed), (1, 1, 0));
        let PoolState::V3(s) = &book.pools()[0].state else {
            panic!()
        };
        assert!(
            s.liquidity > 0 && !s.ticks.is_empty(),
            "seeded state is live"
        );
        let gas = GasTerms {
            base_fee_wei: 1_000_000_000,
            priority_fee_wei: 0,
            out_per_eth: U256::from(3_000_000_000u64),
        };
        let ten_eth = U256::from(10u64) * U256::from(1_000_000_000_000_000_000u64);
        let q = solve_pair(
            &book,
            a_weth,
            a_usdc,
            ten_eth,
            &gas,
            &SolveBudget::default(),
        )
        .unwrap();
        assert!(q.amount_out > U256::ZERO, "exit quoted on the seeded pool");
        eprintln!(
            "10 WETH -> {} raw USDC over {} ticks",
            q.amount_out,
            s.ticks.len()
        );
    }

    /// Live check: seed Curve 3pool and the Uniswap V2 DAI/WETH pair, then
    /// quote a 100k DAI to USDC exit on 3pool.
    #[tokio::test]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn seeds_live_curve_3pool_and_v2_pair() {
        use liq_router::{CurveState, Pool, V2State};
        use liq_types::AssetId;
        let url = std::env::var("MAINNET_RPC_URL").expect("MAINNET_RPC_URL");
        let rpc = HttpRpc::connect(&url).unwrap();
        let dai = address!("0x6B175474E89094C44Da98b954EedeAC495271d0F");
        let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let usdt = address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");
        let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let mut assets = std::collections::HashMap::new();
        for (i, t) in [dai, usdc, usdt, weth].into_iter().enumerate() {
            assets.insert(t, AssetId(i as u16));
        }
        let mut book = PoolBook::new(assets, None, 100_000);
        let e = |d: u32| U256::from(10u64).pow(U256::from(36 - d));
        book.add(Pool {
            address: address!("0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"),
            assets: smallvec::SmallVec::from_slice(&[AssetId(0), AssetId(1), AssetId(2)]),
            tokens: smallvec::SmallVec::from_slice(&[dai, usdc, usdt]),
            hop_gas: 100_000,
            state: PoolState::Curve(CurveState {
                balances: smallvec::SmallVec::from_slice(&[U256::ZERO; 3]),
                rates: smallvec::SmallVec::from_slice(&[e(18), e(6), e(6)]),
                a: U256::ZERO,
                a_precision: U256::from(1u64),
                fee: U256::ZERO,
                stale: true,
                stale_block: 0,
            }),
        })
        .unwrap();
        book.add(Pool {
            address: address!("0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11"),
            assets: smallvec::SmallVec::from_slice(&[AssetId(0), AssetId(3)]),
            tokens: smallvec::SmallVec::from_slice(&[dai, weth]),
            hop_gas: 65_000,
            state: PoolState::V2(V2State {
                reserve0: U256::ZERO,
                reserve1: U256::ZERO,
                factory: 0,
            }),
        })
        .unwrap();
        assert_eq!(seed_curve(&mut book, &rpc).await.seeded, 1);
        assert_eq!(seed_v2(&mut book, &rpc).await.seeded, 1);
        assert!(book.pools().iter().all(liq_router::Pool::is_live));
        let dx = U256::from(100_000u64) * U256::from(10u64).pow(U256::from(18u64));
        let out = book.pools()[0].quote_exact_in(0, 1, dx).unwrap();
        eprintln!("100k DAI -> {out} raw USDC on 3pool");
        assert!(out > U256::from(99_000_000_000u64) && out < U256::from(100_500_000_000u64));
    }

    /// Live check over the committed registry: every Curve pool the book
    /// loads, seeded at one block, quotes what the pool's own `get_dy`
    /// returns at that block (1 % of the input balance, every ordered coin
    /// pair). The solver models `exchange`, which takes the fee before
    /// scaling to raw units, so it may sit exactly 1 wei under `get_dy` for
    /// a non-18-decimal output — never above. Every V2 pair seeds live.
    #[tokio::test]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn committed_exit_pools_quote_exactly_on_chain() {
        use liq_config::{Intern, Registry};
        sol! {
            function get_dy(int128 i, int128 j, uint256 dx) returns (uint256);
        }
        let url = std::env::var("MAINNET_RPC_URL").expect("MAINNET_RPC_URL");
        let rpc = HttpRpc::connect(&url).unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let mut book = crate::index::load_index(&root.join("config"), &intern, &reg).book;
        let targets = curve_targets(&book, false);
        let block = rpc.block_number().await.unwrap();
        let query: Vec<(Address, usize)> = targets.iter().map(|&(_, a, n)| (a, n)).collect();
        let reads = read_curve(&rpc, &query, block).await;
        let mut checked = 0usize;
        let mut wrong = Vec::new();
        for (&(i, addr, n), r) in targets.iter().zip(reads) {
            let r = r.unwrap_or_else(|| panic!("curve read failed for {addr}"));
            let id = PoolId(u32::try_from(i).unwrap());
            assert!(book
                .reseed_curve(id, &r.balances, r.a, r.a_precision, r.fee, block)
                .unwrap());
            let pool = book.get(id).unwrap();
            for ci in 0..n {
                for cj in 0..n {
                    if ci == cj {
                        continue;
                    }
                    let dx = r.balances[ci] / U256::from(100u64);
                    let data = get_dyCall {
                        i: i128::try_from(ci).unwrap(),
                        j: i128::try_from(cj).unwrap(),
                        dx,
                    }
                    .abi_encode();
                    let raw = rpc.call_at(addr, data.into(), block).await.unwrap();
                    let want = get_dyCall::abi_decode_returns(&raw).unwrap();
                    let got = pool
                        .quote_exact_in(u8::try_from(ci).unwrap(), u8::try_from(cj).unwrap(), dx)
                        .ok();
                    checked += 1;
                    let within = got.is_some_and(|g| g <= want && want - g <= U256::ONE);
                    if !within {
                        wrong.push(format!("{addr} {ci}->{cj}: solver {got:?} chain {want}"));
                    }
                }
            }
        }
        eprintln!(
            "{} curve pools, {checked} quotes checked at block {block}",
            targets.len()
        );
        assert!(wrong.is_empty(), "solver != get_dy:\n{}", wrong.join("\n"));
        let v2 = seed_v2(&mut book, &rpc).await;
        eprintln!("v2: {v2:?}");
        assert_eq!(v2.failed, 0);
    }

    #[test]
    fn curve_amp_prefers_a_precise() {
        let p = |v: u64| Some(U256::from(v));
        assert_eq!(
            curve_amp(p(200_000), p(2_000)),
            Some((U256::from(200_000u64), U256::from(100u64)))
        );
        assert_eq!(
            curve_amp(None, p(2_000)),
            Some((U256::from(2_000u64), U256::from(1u64)))
        );
        assert_eq!(curve_amp(None, None), None);
        assert_eq!(curve_amp(p(0), p(0)), None);
    }

    #[test]
    fn window_covers_thirty_percent_either_side() {
        // spacing 10: 2560 ticks per word → k = 2, words 75..=79 around 77.
        assert_eq!(words(197_502, 10), Some((75, 79)));
        // spacing 1: 256 ticks per word → k = 11.
        let (lo, hi) = words(0, 1).unwrap();
        assert_eq!((lo, hi), (-11, 11));
        assert!(i32::from(hi - lo + 1) * 256 >= 2 * WINDOW_TICKS);
        // Negative ticks floor, not truncate.
        assert_eq!(words(-1, 60), Some((-2, 0)));
        assert_eq!(words(0, 0), None);
    }
}
