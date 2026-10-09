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

    // `feeProtocol` is a uint8 on Uniswap and SushiSwap and a uint32 on PancakeSwap (two
    // packed 16-bit fees, 209,718,400 by default). It is not read here; the wider type is
    // the one that matches all three ABIs (alloy would also truncate a uint8 silently).
    function slot0() returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint32 feeProtocol, bool unlocked);
    function liquidity() returns (uint128);

    struct PopulatedTick { int24 tick; int128 liquidityNet; uint128 liquidityGross; }
    function getPopulatedTicksInWord(address pool, int16 tickBitmapIndex) returns (PopulatedTick[] populatedTicks);
    /// Uniswap V4 `StateView` (v4-periphery `lens/StateView.sol`).
    function getSlot0(bytes32 poolId) returns (uint160 sqrtPriceX96, int24 tick, uint24 protocolFee, uint24 lpFee);
    function getLiquidity(bytes32 poolId) returns (uint128 liquidity);
    function getTickBitmap(bytes32 poolId, int16 tick) returns (uint256 tickBitmap);
    function getTickLiquidity(bytes32 poolId, int24 tick) returns (uint128 liquidityGross, int128 liquidityNet);

    function getReserves() returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);

    function balances(uint256 i) returns (uint256);
    function A() returns (uint256);
    function A_precise() returns (uint256);
    function fee() returns (uint256);
    function stored_rates() returns (uint256[]);
    function D() returns (uint256);
    function getCurrentBlockTimestamp() returns (uint256 timestamp);
    function gamma() returns (uint256);
    function mid_fee() returns (uint256);
    function out_fee() returns (uint256);
    function fee_gamma() returns (uint256);
    function future_A_gamma_time() returns (uint256);
    function price_scale() returns (uint256);
    function price_scale(uint256 k) returns (uint256);
    function offpeg_fee_multiplier() returns (uint256);
    function previewRedeem(uint256 shares) returns (uint256);
    function exchangeRate() returns (uint256);
    function pyIndexStored() returns (uint256);
    function previewRedeem(address tokenOut, uint256 amountSharesToRedeem) returns (uint256);
    function totalSupply() returns (uint256);
    /// Euler EVK `cash()`: what `redeem` can pay out.
    function cash() returns (uint256);
    struct MarketState {
        int256 totalPt;
        int256 totalSy;
        int256 totalLp;
        address treasury;
        int256 scalarRoot;
        uint256 expiry;
        uint256 lnFeeRateRoot;
        uint256 reserveFeePercent;
        uint256 lastLnImpliedRate;
    }
    function readState(address router) returns (MarketState market);
}

/// Multicall3, same address on every EVM chain.
const MULTICALL3: Address = address!("0xcA11bde05977b3631167028862bE2a173976CA11");
/// Uniswap V3 TickLens (mainnet).
const TICK_LENS: Address = address!("0xbfd8137f7d1516D3ea5cA83523914859ec47F573");
/// Uniswap V4 `StateView` (v4 deployments, mainnet): the PoolManager's pool
/// state by pool id.
const V4_STATE_VIEW: Address = address!("0x7fFE42C4a5DEeA5b0feC41C94C136Cf115597227");
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

/// The tick range the bitmap words [`words`] reads cover: every tick of
/// words `lo..=hi`.
fn window_ticks(tick: i32, spacing: i32) -> Option<(i32, i32)> {
    let (lo, hi) = words(tick, spacing)?;
    let lo_tick = i32::from(lo).checked_mul(256)?.checked_mul(spacing)?;
    let hi_tick = i32::from(hi)
        .checked_mul(256)?
        .checked_add(255)?
        .checked_mul(spacing)?;
    Some((lo_tick, hi_tick))
}

pub(crate) async fn aggregate(
    rpc: &HttpRpc,
    calls: Vec<Call3>,
    block: u64,
) -> Option<Vec<Result3>> {
    let data = Bytes::from(aggregate3Call { calls }.abi_encode());
    let raw = rpc.call_at(MULTICALL3, data, block).await.ok()?;
    aggregate3Call::abi_decode_returns(&raw).ok()
}

/// [`aggregate`], splitting a batch the node refuses as a whole in halves
/// until each part runs, or is one call that fails on its own (`None`).
///
/// A TickLens batch is refused for gas: one deep pool's window of words costs
/// millions (USDC/WETH 0.05 %: 1,114 populated ticks, 12.7M gas), and 150
/// words across a few such pools pass the node's 50M `eth_call` cap. Refused
/// whole, every pool in the batch went unseeded, shallow ones with it, and
/// was routed around: LINK/WETH 0.3 % at block 26,106,490.
async fn aggregate_split(rpc: &HttpRpc, calls: Vec<Call3>, block: u64) -> Vec<Option<Result3>> {
    split_aggregate(calls, |part| aggregate(rpc, part, block)).await
}

/// [`aggregate_split`] over any batch runner.
/// Times one call the node refused on its own is sent again.
const SINGLE_CALL_RETRIES: u64 = 3;

async fn split_aggregate<F, Fut>(calls: Vec<Call3>, mut run: F) -> Vec<Option<Result3>>
where
    F: FnMut(Vec<Call3>) -> Fut,
    Fut: std::future::Future<Output = Option<Vec<Result3>>>,
{
    let mut out: Vec<Option<Result3>> = (0..calls.len()).map(|_| None).collect();
    let mut work: Vec<(usize, Vec<Call3>)> = vec![(0, calls)];
    while let Some((at, part)) = work.pop() {
        let n = part.len();
        let mut got = run(part.clone()).await;
        // One call refused on its own is a transport failure (Multicall3
        // returns a revert inside its result): a throttled endpoint, not
        // the call. Retried before the call is given up, since one lost
        // tick read leaves a whole pool unseeded.
        let mut retry = 0u64;
        while got.is_none() && n == 1 && retry < SINGLE_CALL_RETRIES {
            retry = retry.saturating_add(1);
            tokio::time::sleep(Duration::from_millis(250u64.saturating_mul(retry))).await;
            got = run(part.clone()).await;
        }
        match got {
            Some(res) if res.len() == n => {
                for (i, r) in res.into_iter().enumerate() {
                    if let Some(slot) = out.get_mut(at.saturating_add(i)) {
                        *slot = Some(r);
                    }
                }
            }
            _ if n > 1 => {
                let half = n / 2;
                let mut lo = part;
                let hi = lo.split_off(half);
                work.push((at.saturating_add(half), hi));
                work.push((at, lo));
            }
            _ => {}
        }
    }
    out
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
            // A V4 pool has no contract of its own ([`seed_v4`]).
            PoolState::V3(s) if s.v4.is_none() => Some((i, p.address, s.tick_spacing)),
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
        let res = aggregate_split(rpc, calls, block).await;
        for (i, &(k, _)) in chunk.iter().enumerate() {
            let decoded = res
                .get(i)
                .and_then(Option::as_ref)
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
        s.window = window_ticks(*tick, s.tick_spacing);
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

/// Seed every Uniswap V4 pool in `book` through `StateView`: price, tick and
/// fees from `getSlot0`, `getLiquidity`, then the tick bitmap words around
/// the price and each set tick's `getTickLiquidity`. The swap fee is
/// `calculateSwapFee(protocolFee, lpFee)` of the dearer direction.
pub async fn seed_v4(book: &mut PoolBook, rpc: &HttpRpc) -> SeedStats {
    let mut stats = SeedStats::default();
    let block = match rpc.block_number().await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "V4 seed skipped — head unavailable");
            return stats;
        }
    };
    let v4: Vec<(usize, alloy_primitives::B256, i32)> = book
        .pools()
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match &p.state {
            PoolState::V3(s) => s.v4.map(|k| (i, k.id, s.tick_spacing)),
            _ => None,
        })
        .collect();
    stats.pools = v4.len();
    if v4.is_empty() {
        return stats;
    }
    let mut calls = Vec::with_capacity(v4.len().saturating_mul(2));
    for &(_, id, _) in &v4 {
        calls.push(call(
            V4_STATE_VIEW,
            getSlot0Call { poolId: id }.abi_encode(),
        ));
        calls.push(call(
            V4_STATE_VIEW,
            getLiquidityCall { poolId: id }.abi_encode(),
        ));
    }
    let res = aggregate_split(rpc, calls, block).await;
    // (sqrt, tick, swap fee, liquidity)
    let mut heads: Vec<Option<(U256, i32, u32, u128)>> = vec![None; v4.len()];
    for (k, head) in heads.iter_mut().enumerate() {
        let get = |i: usize| res.get(i).and_then(Option::as_ref).filter(|r| r.success);
        let (Some(s0), Some(l)) = (
            get(k.saturating_mul(2)),
            get(k.saturating_mul(2).saturating_add(1)),
        ) else {
            continue;
        };
        let (Ok(s0), Ok(l)) = (
            getSlot0Call::abi_decode_returns(&s0.returnData),
            getLiquidityCall::abi_decode_returns(&l.returnData),
        ) else {
            continue;
        };
        let sqrt = U256::from(s0.sqrtPriceX96);
        if sqrt.is_zero() {
            continue;
        }
        let proto = s0.protocolFee.to::<u32>();
        let lp = s0.lpFee.to::<u32>();
        let fee = liq_router::v4_swap_fee(proto & 0xfff, lp)
            .max(liq_router::v4_swap_fee(proto >> 12, lp));
        *head = Some((sqrt, s0.tick.as_i32(), fee, l));
    }

    // Tick bitmap words in the window, then each set tick's liquidity.
    let mut words_jobs: Vec<(usize, i16)> = Vec::new();
    for (k, (&(_, _, spacing), head)) in v4.iter().zip(&heads).enumerate() {
        let Some((_, tick, _, _)) = head else {
            continue;
        };
        let Some((lo, hi)) = words(*tick, spacing) else {
            continue;
        };
        for w in lo..=hi {
            words_jobs.push((k, w));
        }
    }
    let calls = words_jobs
        .iter()
        .filter_map(|&(k, w)| {
            v4.get(k).map(|&(_, id, _)| {
                call(
                    V4_STATE_VIEW,
                    getTickBitmapCall {
                        poolId: id,
                        tick: w,
                    }
                    .abi_encode(),
                )
            })
        })
        .collect();
    let bitmaps = aggregate_split(rpc, calls, block).await;
    let mut tick_ok: Vec<bool> = vec![true; v4.len()];
    let mut tick_jobs: Vec<(usize, i32)> = Vec::new();
    for (i, &(k, w)) in words_jobs.iter().enumerate() {
        let word = bitmaps
            .get(i)
            .and_then(Option::as_ref)
            .filter(|r| r.success)
            .and_then(|r| getTickBitmapCall::abi_decode_returns(&r.returnData).ok());
        let Some(word) = word else {
            if let Some(ok) = tick_ok.get_mut(k) {
                *ok = false;
            }
            continue;
        };
        let Some(&(_, _, spacing)) = v4.get(k) else {
            continue;
        };
        for bit in 0..256usize {
            if !word.bit(bit) {
                continue;
            }
            let tick = i32::from(w)
                .checked_mul(256)
                .and_then(|c| c.checked_add(i32::try_from(bit).ok()?))
                .and_then(|c| c.checked_mul(spacing));
            if let Some(t) = tick {
                tick_jobs.push((k, t));
            }
        }
    }
    let calls = tick_jobs
        .iter()
        .filter_map(|&(k, t)| {
            let tick = alloy_primitives::aliases::I24::try_from(t).ok()?;
            v4.get(k).map(|&(_, id, _)| {
                call(
                    V4_STATE_VIEW,
                    getTickLiquidityCall { poolId: id, tick }.abi_encode(),
                )
            })
        })
        .collect();
    let liqs = aggregate_split(rpc, calls, block).await;
    let mut ticks: Vec<Vec<Tick>> = vec![Vec::new(); v4.len()];
    for (i, &(k, t)) in tick_jobs.iter().enumerate() {
        let got = liqs
            .get(i)
            .and_then(Option::as_ref)
            .filter(|r| r.success)
            .and_then(|r| getTickLiquidityCall::abi_decode_returns(&r.returnData).ok());
        match got {
            Some(r) => {
                if let Some(v) = ticks.get_mut(k) {
                    v.push(Tick {
                        tick: t,
                        net: r.liquidityNet,
                        gross: r.liquidityGross,
                    });
                }
            }
            None => {
                if let Some(ok) = tick_ok.get_mut(k) {
                    *ok = false;
                }
            }
        }
    }

    for (k, &(pool_i, id, _)) in v4.iter().enumerate() {
        let (Some(Some((sqrt, tick, fee, liq))), Some(true)) = (heads.get(k), tick_ok.get(k))
        else {
            // The stage that failed: a pool with no head was never
            // initialized or its slot0/liquidity read was refused; one with
            // a head lost a tick-bitmap word or a tick's liquidity read.
            let stage = if heads.get(k).is_some_and(Option::is_some) {
                "tick reads"
            } else {
                "slot0/liquidity (or not initialized)"
            };
            tracing::warn!(pool = %id, block, stage, "V4 pool not seeded — not routed");
            stats.failed = stats.failed.saturating_add(1);
            continue;
        };
        let Some(pool) = pool_mut(book, pool_i) else {
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
        s.fee_pips = *fee;
        s.liquidity = *liq;
        s.ticks = t;
        s.window = window_ticks(*tick, s.tick_spacing);
        stats.seeded = stats.seeded.saturating_add(1);
        tracing::debug!(pool = %id, ticks = s.ticks.len(), "V4 pool seeded");
    }
    tracing::info!(
        pools = stats.pools,
        seeded = stats.seeded,
        failed = stats.failed,
        block,
        "UniV4 pool state seeded"
    );
    stats
}

pub(crate) fn call(target: Address, data: Vec<u8>) -> Call3 {
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

/// `getReserves` of each pair at `block`; `None` for a failed read.
pub(crate) async fn read_v2_reserves(
    rpc: &HttpRpc,
    pairs: &[Address],
    block: u64,
) -> Vec<Option<(U256, U256)>> {
    let mut out = Vec::with_capacity(pairs.len());
    for chunk in pairs.chunks(BATCH) {
        let calls = chunk
            .iter()
            .map(|&a| call(a, getReservesCall {}.abi_encode()))
            .collect();
        let res = aggregate(rpc, calls, block).await;
        for k in 0..chunk.len() {
            out.push(
                res.as_ref()
                    .and_then(|r| r.get(k))
                    .filter(|r| r.success)
                    .and_then(|r| getReservesCall::abi_decode_returns(&r.returnData).ok())
                    .map(|r| (U256::from(r.reserve0), U256::from(r.reserve1))),
            );
        }
    }
    out
}

/// One Curve pool read at a block: `balances`, `A` as stored, its
/// `A_PRECISION`, `fee` (1e10), and for NG `stored_rates()` and
/// `offpeg_fee_multiplier()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurveRead {
    pub balances: Vec<U256>,
    pub a: U256,
    pub a_precision: U256,
    pub fee: U256,
    pub ng: Option<(Vec<U256>, U256)>,
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
    pools: &[(Address, usize, bool)],
    block: u64,
) -> Vec<Option<CurveRead>> {
    let mut out = Vec::with_capacity(pools.len());
    // n balances + A_precise + A + fee (+ stored_rates, offpeg for NG) per
    // pool: at most 9 calls each.
    for chunk in pools.chunks(BATCH / 10) {
        let mut calls = Vec::new();
        for &(addr, n, ng) in chunk {
            for i in 0..n {
                calls.push(call(addr, balancesCall { i: U256::from(i) }.abi_encode()));
            }
            calls.push(call(addr, A_preciseCall {}.abi_encode()));
            calls.push(call(addr, ACall {}.abi_encode()));
            calls.push(call(addr, feeCall {}.abi_encode()));
            if ng {
                calls.push(call(addr, stored_ratesCall {}.abi_encode()));
                calls.push(call(addr, offpeg_fee_multiplierCall {}.abi_encode()));
            }
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
        for &(_, n, ng) in chunk {
            let balances: Option<Vec<U256>> = (0..n).map(|i| row(at.saturating_add(i))).collect();
            let precise = row(at.saturating_add(n));
            let plain = row(at.saturating_add(n).saturating_add(1));
            let fee = row(at.saturating_add(n).saturating_add(2));
            at = at.saturating_add(n).saturating_add(3);
            let ng_read = if ng {
                let rates = res
                    .as_ref()
                    .and_then(|r| r.get(at))
                    .filter(|r| r.success)
                    .and_then(|r| stored_ratesCall::abi_decode_returns(&r.returnData).ok())
                    .filter(|v| v.len() == n);
                let offpeg = row(at.saturating_add(1));
                at = at.saturating_add(2);
                match (rates, offpeg) {
                    (Some(r), Some(o)) => Some(Some((r, o))),
                    _ => None,
                }
            } else {
                Some(None)
            };
            out.push(match (balances, curve_amp(precise, plain), fee, ng_read) {
                (Some(balances), Some((a, a_precision)), Some(fee), Some(ng)) => Some(CurveRead {
                    balances,
                    a,
                    a_precision,
                    fee,
                    ng,
                }),
                _ => None,
            });
        }
    }
    out
}

/// Every Curve pool (`only_stale` → the stale ones, plus NG pools whose
/// rates move without a log and were last read before the head) as
/// `(book index, address, n_coins, ng)`.
fn curve_targets(
    book: &PoolBook,
    only_stale: bool,
    head: u64,
) -> Vec<(usize, Address, usize, bool)> {
    book.pools()
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match &p.state {
            PoolState::Curve(c)
                if !only_stale || c.stale || (c.dynamic_rates && c.read_block < head) =>
            {
                Some((i, p.address, c.rates.len(), c.ng))
            }
            _ => None,
        })
        .collect()
}

/// Read the targets at the head and apply what is still current. Returns
/// `(read, applied)`.
async fn refresh_curve(
    book: &RwLock<PoolBook>,
    rpc: &HttpRpc,
    targets: &[(usize, Address, usize, bool)],
    block: u64,
) -> (usize, usize) {
    let query: Vec<(Address, usize, bool)> =
        targets.iter().map(|&(_, a, n, ng)| (a, n, ng)).collect();
    let reads = read_curve(rpc, &query, block).await;
    let read = reads.iter().filter(|r| r.is_some()).count();
    let mut applied = 0usize;
    let mut w = book.write();
    for (&(i, addr, _, _), r) in targets.iter().zip(reads) {
        let Some(r) = r else {
            tracing::warn!(pool = %addr, block, "curve read failed — stays stale");
            continue;
        };
        let Ok(id) = u32::try_from(i).map(PoolId) else {
            continue;
        };
        let ng =
            r.ng.as_ref()
                .map(|(rates, offpeg)| (rates.as_slice(), *offpeg));
        match w.reseed_curve(id, &r.balances, r.a, r.a_precision, r.fee, ng, block) {
            Ok(true) => applied = applied.saturating_add(1),
            Ok(false) => {}
            Err(e) => tracing::error!(pool = %addr, error = ?e, "curve reseed refused"),
        }
    }
    (read, applied)
}

/// Seed every Curve pool in `book` at the current head.
pub async fn seed_curve(book: &mut PoolBook, rpc: &HttpRpc) -> SeedStats {
    let block = match rpc.block_number().await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "curve seed: head unavailable — pools stay stale");
            return SeedStats::default();
        }
    };
    let targets = curve_targets(book, false, block);
    let lock = RwLock::new(std::mem::replace(
        book,
        PoolBook::new(std::collections::HashMap::new(), None, 0),
    ));
    let (read, applied) = refresh_curve(&lock, rpc, &targets, block).await;
    let crypto = crypto_targets(&lock.read(), block);
    let (c_read, c_applied) = refresh_crypto(&lock, rpc, &crypto, block).await;
    tracing::info!(
        pools = crypto.len(),
        read = c_read,
        seeded = c_applied,
        "Curve crypto pool state seeded"
    );
    let balancers = balancer_targets(&lock.read(), block);
    let (b_read, b_applied) = refresh_balancer(&lock, rpc, &balancers, block).await;
    tracing::info!(
        pools = balancers.len(),
        read = b_read,
        seeded = b_applied,
        "Balancer pool state seeded"
    );
    let fluids = fluid_targets(&lock.read(), block);
    let (f_read, f_applied) = refresh_fluid(&lock, rpc, &fluids, block).await;
    tracing::info!(
        pools = fluids.len(),
        read = f_read,
        seeded = f_applied,
        "Fluid DEX pool state seeded"
    );
    let unwraps = unwrap_targets(&lock.read(), block);
    let (u_read, u_applied) = refresh_unwraps(&lock, rpc, &unwraps, block).await;
    tracing::info!(
        wrappers = unwraps.len(),
        read = u_read,
        seeded = u_applied,
        "unwrap rates seeded"
    );
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

/// Read crypto pools at `block` (`(address, n_coins)`). `None` for a pool
/// whose read failed — it stays stale. `ts` is the block's timestamp, for
/// the ramp check.
async fn read_crypto(
    rpc: &HttpRpc,
    pools: &[(Address, usize)],
    block: u64,
    ts: u64,
) -> Vec<Option<liq_router::CryptoRead>> {
    let mut out = Vec::with_capacity(pools.len());
    // n balances + 7 views + 1 or 2 price scales: at most 12 calls each.
    for chunk in pools.chunks(BATCH / 12) {
        let mut calls = Vec::new();
        for &(addr, n) in chunk {
            for i in 0..n {
                calls.push(call(addr, balancesCall { i: U256::from(i) }.abi_encode()));
            }
            calls.push(call(addr, DCall {}.abi_encode()));
            calls.push(call(addr, ACall {}.abi_encode()));
            calls.push(call(addr, gammaCall {}.abi_encode()));
            calls.push(call(addr, mid_feeCall {}.abi_encode()));
            calls.push(call(addr, out_feeCall {}.abi_encode()));
            calls.push(call(addr, fee_gammaCall {}.abi_encode()));
            calls.push(call(addr, future_A_gamma_timeCall {}.abi_encode()));
            if n == 2 {
                calls.push(call(addr, price_scale_0Call {}.abi_encode()));
            } else {
                for k in 0..n.saturating_sub(1) {
                    calls.push(call(
                        addr,
                        price_scale_1Call { k: U256::from(k) }.abi_encode(),
                    ));
                }
            }
        }
        let res = aggregate(rpc, calls, block).await;
        let row = |k: usize| {
            res.as_ref()
                .and_then(|r| r.get(k))
                .filter(|r| r.success)
                .and_then(|r| balancesCall::abi_decode_returns(&r.returnData).ok())
        };
        let mut at = 0usize;
        for &(_, n) in chunk {
            let balances: Option<Vec<U256>> = (0..n).map(|i| row(at.saturating_add(i))).collect();
            let v = |k: usize| row(at.saturating_add(n).saturating_add(k));
            let (d, a, g, mid, outf, fg, fut) = (v(0), v(1), v(2), v(3), v(4), v(5), v(6));
            let n_ps = if n == 2 { 1 } else { n.saturating_sub(1) };
            let ps: Option<Vec<U256>> = (0..n_ps).map(|k| v(7usize.saturating_add(k))).collect();
            at = at.saturating_add(n).saturating_add(7).saturating_add(n_ps);
            out.push(match (balances, d, a, g, mid, outf, fg, fut, ps) {
                (
                    Some(balances),
                    Some(d),
                    Some(ann),
                    Some(gamma),
                    Some(mid_fee),
                    Some(out_fee),
                    Some(fee_gamma),
                    Some(fut),
                    Some(price_scale),
                ) => Some(liq_router::CryptoRead {
                    balances,
                    price_scale,
                    d,
                    ann,
                    gamma,
                    mid_fee,
                    out_fee,
                    fee_gamma,
                    ramping: fut > U256::from(ts),
                    tweak: None,
                }),
                _ => None,
            });
        }
    }
    out
}

/// One crypto pool to re-read.
#[derive(Clone, Copy, Debug)]
struct CryptoTarget {
    index: usize,
    address: Address,
    n_coins: usize,
    /// Its swaps are followed: the `tweak_price` state is read too.
    followed: bool,
    /// Where `cached_price_oracle` was found last time, if ever.
    oracle_slot: Option<U256>,
}

/// Every crypto pool: `D` and `price_scale` move with each trade and the
/// pool's own `tweak_price`, so each is re-read once per block (and
/// whenever a log marks it stale).
fn crypto_targets(book: &PoolBook, head: u64) -> Vec<CryptoTarget> {
    book.pools()
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match &p.state {
            PoolState::Crypto(c) if c.stale || c.read_block < head => Some(CryptoTarget {
                index: i,
                address: p.address,
                n_coins: c.balances.len(),
                followed: c.kind == liq_router::CryptoKind::TwoStable,
                oracle_slot: c.tweak.as_ref().map(|t| t.oracle_slot),
            }),
            _ => None,
        })
        .collect()
}

sol! {
    function last_prices() external view returns (uint256);
    function last_timestamp() external view returns (uint256);
    function packed_rebalancing_params() external view returns (uint256);
    function donation_shares() external view returns (uint256);
    function donation_duration() external view returns (uint256);
    function last_donation_release_ts() external view returns (uint256);
    function donation_protection_expiry_ts() external view returns (uint256);
    function donation_protection_period() external view returns (uint256);
    function virtual_price() external view returns (uint256);
    function xcp_profit() external view returns (uint256);
    function lp_xcp_profit() external view returns (uint256);
    function reserved_profit_fraction() external view returns (uint256);
    function admin_fee() external view returns (uint256);
    function POLICY() external view returns (address);
    function version() external view returns (string);
}

/// Seconds between blocks: the plan executes one block after the read.
const BLOCK_SECS: u64 = 12;

/// The `tweak_price` state of each followed pool at `block`, for the
/// plan's execution a block later. `None` for a pool whose reads failed,
/// whose version is not one the model ports, or that has a policy
/// contract steering its rebalance: it then goes stale after one swap.
async fn read_tweak(
    rpc: &HttpRpc,
    pools: &[CryptoTarget],
    block: u64,
    ts: u64,
) -> Vec<Option<liq_router::TweakState>> {
    const VIEWS: usize = 16;
    let mut out = Vec::with_capacity(pools.len());
    let mut calls = Vec::with_capacity(pools.len().saturating_mul(VIEWS));
    for t in pools {
        let a = t.address;
        calls.push(call(a, last_pricesCall {}.abi_encode()));
        calls.push(call(a, last_timestampCall {}.abi_encode()));
        calls.push(call(a, packed_rebalancing_paramsCall {}.abi_encode()));
        calls.push(call(a, totalSupplyCall {}.abi_encode()));
        calls.push(call(a, donation_sharesCall {}.abi_encode()));
        calls.push(call(a, donation_durationCall {}.abi_encode()));
        calls.push(call(a, last_donation_release_tsCall {}.abi_encode()));
        calls.push(call(a, donation_protection_expiry_tsCall {}.abi_encode()));
        calls.push(call(a, donation_protection_periodCall {}.abi_encode()));
        calls.push(call(a, virtual_priceCall {}.abi_encode()));
        calls.push(call(a, xcp_profitCall {}.abi_encode()));
        calls.push(call(a, lp_xcp_profitCall {}.abi_encode()));
        calls.push(call(a, reserved_profit_fractionCall {}.abi_encode()));
        calls.push(call(a, admin_feeCall {}.abi_encode()));
        calls.push(call(a, POLICYCall {}.abi_encode()));
        calls.push(call(a, versionCall {}.abi_encode()));
    }
    let res = aggregate_split(rpc, calls, block).await;
    for (k, t) in pools.iter().enumerate() {
        let at = k.saturating_mul(VIEWS);
        let word = |o: usize| {
            res.get(at.saturating_add(o))
                .and_then(|r| r.as_ref())
                .filter(|r| r.success)
                .and_then(|r| last_pricesCall::abi_decode_returns(&r.returnData).ok())
        };
        let raw = |o: usize| {
            res.get(at.saturating_add(o))
                .and_then(|r| r.as_ref())
                .filter(|r| r.success)
                .map(|r| r.returnData.clone())
        };
        let version = raw(15).and_then(|d| versionCall::abi_decode_returns(&d).ok());
        let v3 = match version.as_deref() {
            Some("v3.0.0") => true,
            Some("v2.1.0d") => false,
            _ => {
                tracing::warn!(pool = %t.address, ?version, "crypto pool version not followed");
                out.push(None);
                continue;
            }
        };
        let policy = raw(14).and_then(|d| POLICYCall::abi_decode_returns(&d).ok());
        if v3 && policy.is_none_or(|p| !p.is_zero()) {
            tracing::warn!(pool = %t.address, ?policy, "crypto pool has a rebalance policy: not followed");
            out.push(None);
            continue;
        }
        let fields = [
            word(0),
            word(1),
            word(2),
            word(3),
            word(4),
            word(5),
            word(6),
            word(7),
            word(8),
            word(9),
            word(10),
            word(13),
        ];
        let [Some(last_prices), Some(last_timestamp), Some(params), Some(total_supply), Some(donation_shares), Some(donation_duration), Some(last_donation_release_ts), Some(donation_protection_expiry_ts), Some(donation_protection_period), Some(virtual_price), Some(xcp_profit), Some(admin_fee)] =
            fields
        else {
            tracing::warn!(pool = %t.address, "crypto tweak_price state unreadable");
            out.push(None);
            continue;
        };
        let (lp_xcp_profit, reserved) = if v3 {
            match (word(11), word(12)) {
                (Some(a), Some(b)) => (a, b),
                _ => {
                    out.push(None);
                    continue;
                }
            }
        } else {
            (U256::ZERO, U256::ZERO)
        };
        // `cached_price_oracle` has no getter: the storage slot before
        // `last_prices`', found once by matching that value.
        let mut slot = None;
        let candidates: Vec<U256> = match t.oracle_slot {
            Some(s) => vec![s.saturating_add(U256::ONE)],
            None => (1u64..12).map(U256::from).collect(),
        };
        for cand in candidates {
            match rpc.storage_at(t.address, cand, block).await {
                Ok(v) if v == last_prices => {
                    slot = Some(cand.saturating_sub(U256::ONE));
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(pool = %t.address, error = %e, "storage read failed");
                    break;
                }
            }
        }
        let Some(oracle_slot) = slot else {
            tracing::warn!(pool = %t.address, "cached_price_oracle slot not found");
            out.push(None);
            continue;
        };
        let Ok(price_oracle) = rpc.storage_at(t.address, oracle_slot, block).await else {
            out.push(None);
            continue;
        };
        out.push(Some(liq_router::TweakState {
            v3,
            price_oracle,
            last_prices,
            last_timestamp,
            packed_rebalancing_params: params,
            total_supply,
            donation_shares,
            donation_duration,
            last_donation_release_ts,
            donation_protection_expiry_ts,
            donation_protection_period,
            virtual_price,
            xcp_profit,
            lp_xcp_profit,
            reserved_profit_fraction: reserved,
            admin_fee,
            exec_ts: U256::from(ts.saturating_add(BLOCK_SECS)),
            oracle_slot,
        }));
    }
    out
}

/// Read and apply the crypto targets at `block`. Returns `(read, applied)`.
async fn refresh_crypto(
    book: &RwLock<PoolBook>,
    rpc: &HttpRpc,
    targets: &[CryptoTarget],
    block: u64,
) -> (usize, usize) {
    if targets.is_empty() {
        return (0, 0);
    }
    // The block's timestamp, from Multicall3 at the same pinned block.
    let ts_call = vec![call(
        MULTICALL3,
        getCurrentBlockTimestampCall {}.abi_encode(),
    )];
    let ts = aggregate(rpc, ts_call, block)
        .await
        .and_then(|r| r.into_iter().next())
        .filter(|r| r.success)
        .and_then(|r| getCurrentBlockTimestampCall::abi_decode_returns(&r.returnData).ok())
        .and_then(|t| u64::try_from(t).ok());
    let Some(ts) = ts else {
        tracing::warn!(block, "crypto reseed: block timestamp unavailable");
        return (0, 0);
    };
    let query: Vec<(Address, usize)> = targets.iter().map(|t| (t.address, t.n_coins)).collect();
    let mut reads = read_crypto(rpc, &query, block, ts).await;
    // The followed pools' `tweak_price` state, beside their swap state.
    let followed: Vec<CryptoTarget> = targets.iter().filter(|t| t.followed).copied().collect();
    if !followed.is_empty() {
        let tweaks = read_tweak(rpc, &followed, block, ts).await;
        let mut by_addr: std::collections::HashMap<Address, Option<liq_router::TweakState>> =
            followed.iter().map(|t| t.address).zip(tweaks).collect();
        for (t, r) in targets.iter().zip(reads.iter_mut()) {
            if let (true, Some(r)) = (t.followed, r.as_mut()) {
                r.tweak = by_addr.remove(&t.address).flatten();
            }
        }
    }
    let read = reads.iter().filter(|r| r.is_some()).count();
    let mut applied = 0usize;
    let mut w = book.write();
    for (t, r) in targets.iter().zip(reads) {
        let (i, addr) = (t.index, t.address);
        let Some(r) = r else {
            tracing::debug!(pool = %addr, block, "crypto read failed — stays stale");
            continue;
        };
        let Ok(id) = u32::try_from(i).map(PoolId) else {
            continue;
        };
        match w.reseed_crypto(id, &r, block) {
            Ok(true) => applied = applied.saturating_add(1),
            Ok(false) => {}
            Err(e) => tracing::error!(pool = %addr, error = ?e, "crypto reseed refused"),
        }
    }
    (read, applied)
}

sol! {
    function getPoolTokens(bytes32 poolId) external view returns (address[] tokens, uint256[] balances, uint256 lastChangeBlock);
    function getNormalizedWeights() external view returns (uint256[]);
    function getSwapFeePercentage() external view returns (uint256);
    function getPausedState() external view returns (bool paused, uint256 pauseWindowEndTime, uint256 bufferPeriodEndTime);
}

/// One Balancer pool to re-read: `(book index, pool, pool id, tokens)`.
type BalancerTarget = (usize, Address, alloy_primitives::B256, Vec<Address>);

/// Every Balancer pool whose state was last read before `head`, or that a
/// Vault log made stale.
fn balancer_targets(book: &PoolBook, head: u64) -> Vec<BalancerTarget> {
    book.pools()
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match &p.state {
            PoolState::Balancer(b) if b.stale || b.read_block < head => {
                Some((i, p.address, b.pool_id, b.tokens.to_vec()))
            }
            _ => None,
        })
        .collect()
}

/// Read Balancer pools at `block`: the Vault's balances for the pool id, the
/// pool's weights and fee, and whether it is paused. `None` for a pool whose
/// reads failed or whose Vault tokens are not the entry's.
async fn read_balancer(
    rpc: &HttpRpc,
    pools: &[BalancerTarget],
    block: u64,
) -> Vec<Option<liq_router::BalancerRead>> {
    const PER: usize = 4;
    let vault = liq_router::solver::BALANCER_VAULT;
    let mut calls = Vec::with_capacity(pools.len().saturating_mul(PER));
    for (_, addr, id, _) in pools {
        calls.push(call(vault, getPoolTokensCall { poolId: *id }.abi_encode()));
        calls.push(call(*addr, getNormalizedWeightsCall {}.abi_encode()));
        calls.push(call(*addr, getSwapFeePercentageCall {}.abi_encode()));
        calls.push(call(*addr, getPausedStateCall {}.abi_encode()));
    }
    let res = aggregate_split(rpc, calls, block).await;
    let row = |k: usize| {
        res.get(k)
            .and_then(|r| r.as_ref())
            .filter(|r| r.success)
            .map(|r| r.returnData.clone())
    };
    pools
        .iter()
        .enumerate()
        .map(|(n, (_, _, _, tokens))| {
            let at = n.saturating_mul(PER);
            let pt = row(at).and_then(|d| getPoolTokensCall::abi_decode_returns(&d).ok())?;
            if pt.tokens != *tokens {
                return None;
            }
            let weights = row(at.saturating_add(1))
                .and_then(|d| getNormalizedWeightsCall::abi_decode_returns(&d).ok())?;
            let swap_fee = row(at.saturating_add(2))
                .and_then(|d| getSwapFeePercentageCall::abi_decode_returns(&d).ok())?;
            let paused = row(at.saturating_add(3))
                .and_then(|d| getPausedStateCall::abi_decode_returns(&d).ok())?;
            Some(liq_router::BalancerRead {
                balances: pt.balances,
                weights,
                swap_fee,
                paused: paused.paused,
            })
        })
        .collect()
}

/// Read and apply the Balancer targets at `block`. Returns `(read, applied)`.
async fn refresh_balancer(
    book: &RwLock<PoolBook>,
    rpc: &HttpRpc,
    targets: &[BalancerTarget],
    block: u64,
) -> (usize, usize) {
    if targets.is_empty() {
        return (0, 0);
    }
    let reads = read_balancer(rpc, targets, block).await;
    let read = reads.iter().filter(|r| r.is_some()).count();
    let mut applied = 0usize;
    let mut w = book.write();
    for ((i, addr, _, _), r) in targets.iter().zip(reads) {
        let Some(r) = r else {
            tracing::warn!(pool = %addr, block, "balancer read failed — stays stale");
            continue;
        };
        let Ok(id) = u32::try_from(*i).map(PoolId) else {
            continue;
        };
        match w.reseed_balancer(id, &r, block) {
            Ok(true) => applied = applied.saturating_add(1),
            Ok(false) => {}
            Err(e) => tracing::error!(pool = %addr, error = ?e, "balancer reseed refused"),
        }
    }
    (read, applied)
}

sol! {
    /// A Fluid DEX pool (and the Liquidity proxy) exposes any storage word.
    function readFromStorage(bytes32 slot) external view returns (uint256 result);
    function constantsView() external view returns (uint256 dexId, address liquidity, address factory, address shift, address admin, address colOperations, address debtOperations, address perfectOperationsAndOracle, address deployerContract, address token0, address token1, bytes32 supplyToken0Slot, bytes32 borrowToken0Slot, bytes32 supplyToken1Slot, bytes32 borrowToken1Slot, bytes32 exchangePriceToken0Slot, bytes32 exchangePriceToken1Slot, uint256 oracleMapping);
    function constantsView2() external view returns (uint256 token0Num, uint256 token0Den, uint256 token1Num, uint256 token1Den);
    function centerPrice() external view returns (uint256);
    function balanceOf(address owner) external view returns (uint256);
    function getEthBalance(address owner) external view returns (uint256);
}

/// One Fluid DEX pool to re-read: `(book index, pool, on-chain tokens,
/// constants known, deployer, hook id from the last read)`.
#[derive(Clone, Debug)]
struct FluidTarget {
    index: usize,
    address: Address,
    tokens: [Address; 2],
    constants_known: bool,
    deployer: Address,
}

/// Every Fluid pool whose state was last read before `head`, or that a
/// Liquidity log made stale.
fn fluid_targets(book: &PoolBook, head: u64) -> Vec<FluidTarget> {
    book.pools()
        .iter()
        .enumerate()
        .filter_map(|(i, p)| match &p.state {
            PoolState::Fluid(f) if f.stale || f.read_block < head => {
                let tokens = [*f.tokens.first()?, *f.tokens.get(1)?];
                Some(FluidTarget {
                    index: i,
                    address: p.address,
                    tokens,
                    constants_known: !f.deployer.is_zero(),
                    deployer: f.deployer,
                })
            }
            _ => None,
        })
        .collect()
}

/// `LiquiditySlotsLink` mapping slots.
const LIQ_EXCHANGE_PRICES_SLOT: u64 = 5;
const LIQ_RATE_DATA_SLOT: u64 = 6;
const LIQ_TOTAL_AMOUNTS_SLOT: u64 = 7;
const LIQ_USER_SUPPLY_SLOT: u64 = 8;
const LIQ_USER_BORROW_SLOT: u64 = 9;
const LIQ_CONFIGS2_SLOT: u64 = 11;

/// `keccak256(abi.encode(key, slot))`, a mapping's storage slot.
fn mapping_slot(key: Address, slot: alloy_primitives::B256) -> alloy_primitives::B256 {
    let mut b = [0u8; 64];
    b[12..32].copy_from_slice(key.as_slice());
    b[32..].copy_from_slice(slot.as_slice());
    alloy_primitives::keccak256(b)
}

fn slot_const(n: u64) -> alloy_primitives::B256 {
    alloy_primitives::B256::from(U256::from(n))
}

/// The Liquidity layer's words for `token` as `user`:
/// `[exchange prices & config, totals, configs2, rate data, supply, borrow]`.
fn liquidity_slots(token: Address, user: Address) -> [alloy_primitives::B256; 6] {
    let double = |slot: u64| mapping_slot(token, mapping_slot(user, slot_const(slot)));
    [
        mapping_slot(token, slot_const(LIQ_EXCHANGE_PRICES_SLOT)),
        mapping_slot(token, slot_const(LIQ_TOTAL_AMOUNTS_SLOT)),
        mapping_slot(token, slot_const(LIQ_CONFIGS2_SLOT)),
        mapping_slot(token, slot_const(LIQ_RATE_DATA_SLOT)),
        double(LIQ_USER_SUPPLY_SLOT),
        double(LIQ_USER_BORROW_SLOT),
    ]
}

fn word_of(data: &[u8]) -> Option<U256> {
    (data.len() == 32).then(|| U256::from_be_slice(data))
}

/// Read Fluid pools at `block`, executing at `ts + BLOCK_SECS`: the pool's
/// two variable words, the center price hook's answer, both tokens' words in
/// the Liquidity layer, the layer's balances and, the first time, the pool's
/// constants. `None` for a pool with an unreadable part.
async fn read_fluid(
    rpc: &HttpRpc,
    pools: &[FluidTarget],
    block: u64,
    ts: u64,
) -> Vec<Option<liq_router::FluidRead>> {
    use liq_router::solver::FLUID_LIQUIDITY as LIQ;
    // Round one, per pool: [constantsView, constantsView2]? then dexVariables
    // (slot 0), dexVariables2 (slot 1), then per token the six Liquidity
    // words and the layer's balance.
    let mut calls = Vec::new();
    let mut starts = Vec::with_capacity(pools.len());
    for p in pools {
        starts.push(calls.len());
        if !p.constants_known {
            calls.push(call(p.address, constantsViewCall {}.abi_encode()));
            calls.push(call(p.address, constantsView2Call {}.abi_encode()));
        }
        for slot in [0u64, 1] {
            calls.push(call(
                p.address,
                readFromStorageCall {
                    slot: slot_const(slot),
                }
                .abi_encode(),
            ));
        }
        for t in p.tokens {
            for slot in liquidity_slots(t, p.address) {
                calls.push(call(LIQ, readFromStorageCall { slot }.abi_encode()));
            }
            if t == crate::index::FLUID_NATIVE {
                calls.push(call(
                    MULTICALL3,
                    getEthBalanceCall { owner: LIQ }.abi_encode(),
                ));
            } else {
                calls.push(call(t, balanceOfCall { owner: LIQ }.abi_encode()));
            }
        }
    }
    let res = aggregate_split(rpc, calls, block).await;
    let row = |k: usize| {
        res.get(k)
            .and_then(|r| r.as_ref())
            .filter(|r| r.success)
            .map(|r| r.returnData.clone())
    };
    struct Round1 {
        constants: Option<([U256; 4], Address)>,
        dex_vars: U256,
        dex_vars2: U256,
        tokens: [liq_router::LiqToken; 2],
        deployer: Address,
    }
    let mut round1: Vec<Option<Round1>> = Vec::with_capacity(pools.len());
    for (n, p) in pools.iter().enumerate() {
        let mut at = starts.get(n).copied().unwrap_or(0);
        let mut next = || {
            let k = at;
            at = at.saturating_add(1);
            row(k)
        };
        let parsed = (|| {
            let mut constants = None;
            let mut deployer = p.deployer;
            if !p.constants_known {
                let cv = constantsViewCall::abi_decode_returns(&next()?).ok()?;
                let cv2 = constantsView2Call::abi_decode_returns(&next()?).ok()?;
                // The on-chain tokens must be the entry's.
                if cv.token0 != p.tokens[0] || cv.token1 != p.tokens[1] {
                    return None;
                }
                deployer = cv.deployerContract;
                constants = Some((
                    [cv2.token0Num, cv2.token0Den, cv2.token1Num, cv2.token1Den],
                    deployer,
                ));
            }
            let dex_vars = word_of(&next()?)?;
            let dex_vars2 = word_of(&next()?)?;
            let mut tokens = [liq_router::LiqToken::default(); 2];
            for t in &mut tokens {
                let mut w = [U256::ZERO; 6];
                for x in &mut w {
                    *x = word_of(&next()?)?;
                }
                let balance = word_of(&next()?)?;
                *t = liq_router::LiqToken {
                    ep_cfg: w[0],
                    totals: w[1],
                    configs2: w[2],
                    rate_data: w[3],
                    supply: w[4],
                    borrow: w[5],
                    balance,
                };
            }
            Some(Round1 {
                constants,
                dex_vars,
                dex_vars2,
                tokens,
                deployer,
            })
        })();
        round1.push(parsed);
    }
    // Round two: the center price hooks (`dexVariables2` bits 112..141 name
    // the hook's CREATE nonce from the pool's deployer contract).
    let mut hook_calls = Vec::new();
    let mut hook_of: Vec<Option<usize>> = vec![None; pools.len()];
    for (slot, r) in hook_of.iter_mut().zip(&round1) {
        let Some(r) = r else { continue };
        let id: U256 = r.dex_vars2.wrapping_shr(112) & U256::from((1u64 << 30) - 1);
        if id.is_zero() {
            continue;
        }
        let Ok(nonce) = u64::try_from(id) else {
            continue;
        };
        *slot = Some(hook_calls.len());
        hook_calls.push(call(
            r.deployer.create(nonce),
            centerPriceCall {}.abi_encode(),
        ));
    }
    let hooks = if hook_calls.is_empty() {
        Vec::new()
    } else {
        aggregate_split(rpc, hook_calls, block).await
    };
    round1
        .into_iter()
        .enumerate()
        .map(|(n, r)| {
            let r = r?;
            let center_ext = match hook_of.get(n).copied().flatten() {
                None => None,
                Some(k) => {
                    let hook = hooks
                        .get(k)
                        .and_then(|h| h.as_ref())
                        .filter(|h| h.success)
                        .and_then(|h| word_of(&h.returnData));
                    // A pool whose hook cannot be read is not followed.
                    Some(hook?)
                }
            };
            Some(liq_router::FluidRead {
                dex_vars: r.dex_vars,
                dex_vars2: r.dex_vars2,
                center_ext,
                tokens: r.tokens,
                exec_ts: ts.saturating_add(BLOCK_SECS),
                constants: r.constants,
            })
        })
        .collect()
}

/// Read and apply the Fluid targets at `block`. Returns `(read, applied)`.
async fn refresh_fluid(
    book: &RwLock<PoolBook>,
    rpc: &HttpRpc,
    targets: &[FluidTarget],
    block: u64,
) -> (usize, usize) {
    if targets.is_empty() {
        return (0, 0);
    }
    // The block's timestamp, from Multicall3 at the same pinned block.
    let ts_call = vec![call(
        MULTICALL3,
        getCurrentBlockTimestampCall {}.abi_encode(),
    )];
    let ts = aggregate(rpc, ts_call, block)
        .await
        .and_then(|r| r.into_iter().next())
        .filter(|r| r.success)
        .and_then(|r| getCurrentBlockTimestampCall::abi_decode_returns(&r.returnData).ok())
        .and_then(|t| u64::try_from(t).ok());
    let Some(ts) = ts else {
        tracing::warn!(block, "fluid reseed: block timestamp unavailable");
        return (0, 0);
    };
    let reads = read_fluid(rpc, targets, block, ts).await;
    let read = reads.iter().filter(|r| r.is_some()).count();
    let mut applied = 0usize;
    let mut w = book.write();
    for (t, r) in targets.iter().zip(reads) {
        let Some(r) = r else {
            tracing::warn!(pool = %t.address, block, "fluid read failed — stays stale");
            continue;
        };
        let Ok(id) = u32::try_from(t.index).map(PoolId) else {
            continue;
        };
        match w.reseed_fluid(id, &r, block) {
            Ok(true) => applied = applied.saturating_add(1),
            Ok(false) => {}
            Err(e) => tracing::error!(pool = %t.address, error = ?e, "fluid reseed refused"),
        }
    }
    (read, applied)
}

/// One wrapper to re-read.
#[derive(Clone, Copy, Debug)]
struct UnwrapTarget {
    wrapper: liq_types::AssetId,
    token: Address,
    into: Address,
    scale: U256,
    kind: liq_router::UnwrapKind,
    cash_capped: bool,
}

/// Wrappers whose rate was last read before `head`. Vault and SY rates move
/// every block (interest), without a log.
fn unwrap_targets(book: &PoolBook, head: u64) -> Vec<UnwrapTarget> {
    book.unwraps()
        .filter(|u| u.read_block < head)
        .map(|u| UnwrapTarget {
            wrapper: u.wrapper,
            token: u.wrapper_token,
            into: u.into_token,
            scale: u.scale,
            kind: u.kind,
            cash_capped: u.cash_capped,
        })
        .collect()
}

/// `cash()` of each cash-capped vault (`None` for every other target, and
/// for a failed read).
async fn read_unwrap_caps(
    rpc: &HttpRpc,
    targets: &[UnwrapTarget],
    block: u64,
) -> Vec<Option<U256>> {
    let at: Vec<usize> = (0..targets.len())
        .filter(|&i| targets.get(i).is_some_and(|t| t.cash_capped))
        .collect();
    let mut out = vec![None; targets.len()];
    if at.is_empty() {
        return out;
    }
    let calls = at
        .iter()
        .filter_map(|&i| targets.get(i))
        .map(|t| call(t.token, cashCall {}.abi_encode()))
        .collect();
    for (&i, w) in at.iter().zip(read_words(rpc, calls, block).await) {
        if let Some(slot) = out.get_mut(i) {
            *slot = w;
        }
    }
    out
}

/// Rows of one `aggregate` as `uint256`s (`None` per failed row).
async fn read_words(rpc: &HttpRpc, calls: Vec<Call3>, block: u64) -> Vec<Option<U256>> {
    let n = calls.len();
    let mut out = Vec::with_capacity(n);
    for chunk in calls.chunks(BATCH) {
        let res = aggregate(rpc, chunk.to_vec(), block).await;
        for k in 0..chunk.len() {
            out.push(
                res.as_ref()
                    .and_then(|r| r.get(k))
                    .filter(|r| r.success)
                    .and_then(|r| exchangeRateCall::abi_decode_returns(&r.returnData).ok()),
            );
        }
    }
    out
}

/// One word per wrapper at `block` — what its [`liq_router::UnwrapRate`]
/// is made of:
/// - Curve LP: the LP's `totalSupply()` (the pool's balances are the
///   book's own, kept by the Curve reseed);
/// - ERC-4626: `previewRedeem(scale)`;
/// - expired Pendle PT: the YT pays `scale · 1e18 / max(SY.exchangeRate(),
///   pyIndexStored)` SY (`PendleYieldToken._redeemPY`, `SYUtils.assetToSy`),
///   and the SY `previewRedeem(into, ·)` of that.
async fn read_unwrap_rates(
    rpc: &HttpRpc,
    targets: &[UnwrapTarget],
    block: u64,
) -> Vec<Option<U256>> {
    let mut first = Vec::new();
    for t in targets {
        match t.kind {
            liq_router::UnwrapKind::Erc4626 => {
                first.push(call(
                    t.token,
                    previewRedeem_0Call { shares: t.scale }.abi_encode(),
                ));
            }
            liq_router::UnwrapKind::CurveLp { .. } => {
                first.push(call(t.token, totalSupplyCall {}.abi_encode()));
            }
            liq_router::UnwrapKind::PendlePt { yt, sy } => {
                first.push(call(sy, exchangeRateCall {}.abi_encode()));
                first.push(call(yt, pyIndexStoredCall {}.abi_encode()));
            }
            // A market is a snapshot, read by `read_pendle_markets`.
            liq_router::UnwrapKind::PendleMarket { .. } => {}
        }
    }
    let words = read_words(rpc, first, block).await;
    let mut out: Vec<Option<U256>> = Vec::with_capacity(targets.len());
    let mut second = Vec::new();
    let mut pending = Vec::new();
    let mut at = 0usize;
    let one = U256::from(1_000_000_000_000_000_000u64);
    for (i, t) in targets.iter().enumerate() {
        match t.kind {
            liq_router::UnwrapKind::Erc4626 | liq_router::UnwrapKind::CurveLp { .. } => {
                out.push(words.get(at).copied().flatten());
                at = at.saturating_add(1);
            }
            liq_router::UnwrapKind::PendlePt { sy, .. } => {
                let rate = words.get(at).copied().flatten();
                let stored = words.get(at.saturating_add(1)).copied().flatten();
                at = at.saturating_add(2);
                out.push(None);
                let sy_out = match (rate, stored) {
                    (Some(r), Some(s)) => {
                        let index = r.max(s);
                        t.scale.checked_mul(one).and_then(|v| v.checked_div(index))
                    }
                    _ => None,
                };
                if let Some(shares) = sy_out.filter(|v| !v.is_zero()) {
                    second.push(call(
                        sy,
                        previewRedeem_1Call {
                            tokenOut: t.into,
                            amountSharesToRedeem: shares,
                        }
                        .abi_encode(),
                    ));
                    pending.push(i);
                }
            }
            liq_router::UnwrapKind::PendleMarket { .. } => out.push(None),
        }
    }
    if !second.is_empty() {
        let words = read_words(rpc, second, block).await;
        for (i, w) in pending.into_iter().zip(words) {
            if let Some(slot) = out.get_mut(i) {
                *slot = w;
            }
        }
    }
    out
}

/// Each live-PT market at `block` as a [`liq_router::pendle::MarketSnapshot`]
/// quoted for the next block (`timestamp + 12`): `readState(0)` (no router
/// fee override applies to the Executor), the index `YT.pyIndexCurrent()`
/// would return (`max(SY.exchangeRate(), pyIndexStored)`), and
/// `SY.previewRedeem(into, 1000 PT-units)`. `None` for any other target or a
/// failed read.
async fn read_pendle_markets(
    rpc: &HttpRpc,
    targets: &[UnwrapTarget],
    block: u64,
) -> Vec<Option<liq_router::pendle::MarketSnapshot>> {
    let mut calls = vec![call(
        MULTICALL3,
        getCurrentBlockTimestampCall {}.abi_encode(),
    )];
    for t in targets {
        if let liq_router::UnwrapKind::PendleMarket { market, yt, sy } = t.kind {
            let sy_scale = t.scale.saturating_mul(U256::from(1_000u64));
            calls.push(call(
                market,
                readStateCall {
                    router: Address::ZERO,
                }
                .abi_encode(),
            ));
            calls.push(call(sy, exchangeRateCall {}.abi_encode()));
            calls.push(call(yt, pyIndexStoredCall {}.abi_encode()));
            calls.push(call(
                sy,
                previewRedeem_1Call {
                    tokenOut: t.into,
                    amountSharesToRedeem: sy_scale,
                }
                .abi_encode(),
            ));
        }
    }
    let mut out = Vec::with_capacity(targets.len());
    if calls.len() == 1 {
        out.resize(targets.len(), None);
        return out;
    }
    let mut rows = Vec::with_capacity(calls.len());
    for chunk in calls.chunks(BATCH) {
        let res = aggregate(rpc, chunk.to_vec(), block).await;
        for k in 0..chunk.len() {
            rows.push(
                res.as_ref()
                    .and_then(|r| r.get(k))
                    .filter(|r| r.success)
                    .map(|r| r.returnData.clone()),
            );
        }
    }
    let ts = rows
        .first()
        .cloned()
        .flatten()
        .and_then(|d| getCurrentBlockTimestampCall::abi_decode_returns(&d).ok())
        .and_then(|t| u64::try_from(t).ok());
    let word = |k: usize| {
        rows.get(k)
            .cloned()
            .flatten()
            .and_then(|d| exchangeRateCall::abi_decode_returns(&d).ok())
    };
    let mut at = 1usize;
    for t in targets {
        let liq_router::UnwrapKind::PendleMarket { .. } = t.kind else {
            out.push(None);
            continue;
        };
        let state = rows
            .get(at)
            .cloned()
            .flatten()
            .and_then(|d| readStateCall::abi_decode_returns(&d).ok());
        let (rate, stored, out_per) = (
            word(at.saturating_add(1)),
            word(at.saturating_add(2)),
            word(at.saturating_add(3)),
        );
        at = at.saturating_add(4);
        let snap = match (ts, state, rate, stored, out_per) {
            (Some(ts), Some(m), Some(rate), Some(stored), Some(out_per)) => u64::try_from(m.expiry)
                .ok()
                .map(|expiry| liq_router::pendle::MarketSnapshot {
                    total_pt: m.totalPt,
                    total_sy: m.totalSy,
                    scalar_root: m.scalarRoot,
                    expiry,
                    ln_fee_rate_root: m.lnFeeRateRoot,
                    reserve_fee_percent: m.reserveFeePercent,
                    last_ln_implied_rate: m.lastLnImpliedRate,
                    index: rate.max(stored),
                    quote_ts: ts.saturating_add(12),
                    out_per_sy_scale: out_per,
                    sy_scale: t.scale.saturating_mul(U256::from(1_000u64)),
                }),
            _ => None,
        };
        out.push(snap);
    }
    out
}

/// The market is expired at the time its snapshot is quoted for (the next
/// block): `PendleMarketV6` refuses every trade once `expiry <= now`, and
/// the YT's post-expiry redeem opens at the same moment.
fn market_expired(s: &liq_router::pendle::MarketSnapshot) -> bool {
    s.expiry <= s.quote_ts
}

/// Read `previewRedeem(scale)` for the targets at `block` and record it.
/// A failed or zero read leaves the old rate, which ages out of routing
/// only by being old — the conversion's haircut covers a few blocks' drift,
/// and the simulation before any send checks the real redeem.
async fn refresh_unwraps(
    book: &RwLock<PoolBook>,
    rpc: &HttpRpc,
    targets: &[UnwrapTarget],
    block: u64,
) -> (usize, usize) {
    let reads = read_unwrap_rates(rpc, targets, block).await;
    let markets = read_pendle_markets(rpc, targets, block).await;
    let caps = read_unwrap_caps(rpc, targets, block).await;
    let rates: Vec<Option<liq_router::UnwrapRate>> = targets
        .iter()
        .zip(reads)
        .zip(markets)
        .zip(caps)
        .map(|(((t, word), snap), cap)| match t.kind {
            liq_router::UnwrapKind::PendleMarket { .. } => snap.map(liq_router::UnwrapRate::Pendle),
            liq_router::UnwrapKind::CurveLp { .. } => word
                .filter(|v| !v.is_zero())
                .map(|total_supply| liq_router::UnwrapRate::CurveLp { total_supply }),
            // A cash-capped vault whose cash did not read is not routed:
            // uncapped, it would be quoted past what it can pay.
            liq_router::UnwrapKind::Erc4626 if t.cash_capped && cap.is_none() => None,
            liq_router::UnwrapKind::Erc4626 | liq_router::UnwrapKind::PendlePt { .. } => word
                .filter(|v| !v.is_zero())
                .map(|assets_per_scale| liq_router::UnwrapRate::Linear {
                    assets_per_scale,
                    max_into: cap,
                }),
        })
        .collect();
    let read = rates.iter().filter(|r| r.is_some()).count();
    let mut applied = 0usize;
    let mut w = book.write();
    for (t, r) in targets.iter().zip(rates) {
        let Some(rate) = r else {
            tracing::debug!(token = %t.token, block, "unwrap rate read failed");
            continue;
        };
        // A live PT whose market is expired by the next block: from then on
        // the market refuses to trade, and the PT redeems through its YT.
        if let liq_router::UnwrapRate::Pendle(s) = rate {
            if market_expired(&s) {
                if w.expire_pendle_market(t.wrapper) {
                    tracing::info!(
                        token = %t.token,
                        expiry = s.expiry,
                        block,
                        "Pendle PT reached expiry: market sale → post-expiry redeem"
                    );
                    applied = applied.saturating_add(1);
                }
                continue;
            }
        }
        if w.set_unwrap_rate(t.wrapper, rate, block) {
            applied = applied.saturating_add(1);
        }
    }
    (read, applied)
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
                let head = match rt.block_on(rpc.block_number()) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(error = %e, "curve reseed: head unavailable");
                        continue;
                    }
                };
                let unwraps = unwrap_targets(&book.read(), head);
                if !unwraps.is_empty() {
                    let (read, applied) =
                        rt.block_on(refresh_unwraps(&book, &rpc, &unwraps, head));
                    tracing::debug!(wrappers = unwraps.len(), read, applied, "unwrap rates");
                }
                let balancers = balancer_targets(&book.read(), head);
                if !balancers.is_empty() {
                    let (read, applied) =
                        rt.block_on(refresh_balancer(&book, &rpc, &balancers, head));
                    tracing::debug!(pools = balancers.len(), read, applied, "balancer reseed");
                }
                let fluids = fluid_targets(&book.read(), head);
                if !fluids.is_empty() {
                    let (read, applied) = rt.block_on(refresh_fluid(&book, &rpc, &fluids, head));
                    tracing::debug!(pools = fluids.len(), read, applied, "fluid reseed");
                }
                let crypto = crypto_targets(&book.read(), head);
                if !crypto.is_empty() {
                    let (read, applied) = rt.block_on(refresh_crypto(&book, &rpc, &crypto, head));
                    tracing::debug!(pools = crypto.len(), read, applied, "crypto reseed");
                }
                let targets = curve_targets(&book.read(), true, head);
                if targets.is_empty() {
                    continue;
                }
                let (read, applied) = rt.block_on(refresh_curve(&book, &rpc, &targets, head));
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

    /// A node that refuses every batch of more than three calls (its gas
    /// cap) still answers each call, in place: the batch is split until the
    /// parts run. A call refused on its own stays `None`.
    #[tokio::test]
    async fn a_batch_refused_whole_is_split_until_each_part_runs() {
        let calls: Vec<Call3> = (0..10u8)
            .map(|i| Call3 {
                target: Address::repeat_byte(i),
                allowFailure: true,
                callData: vec![i].into(),
            })
            .collect();
        let mut sizes = Vec::new();
        let out = split_aggregate(calls, |part| {
            sizes.push(part.len());
            let res: Vec<Result3> = part
                .iter()
                .map(|c| Result3 {
                    success: true,
                    returnData: c.callData.clone(),
                })
                .collect();
            let fits = part.len() <= 3;
            async move { fits.then_some(res) }
        })
        .await;
        for (i, r) in out.iter().enumerate() {
            let want = [u8::try_from(i).unwrap()];
            assert_eq!(r.as_ref().unwrap().returnData.as_ref(), &want[..]);
        }
        assert_eq!(sizes[0], 10, "the whole batch first");
        assert!(
            sizes.iter().filter(|&&n| n <= 3).sum::<usize>() == 10,
            "{sizes:?}"
        );

        let one = vec![Call3 {
            target: Address::ZERO,
            allowFailure: true,
            callData: Bytes::new(),
        }];
        let out = split_aggregate(one.clone(), |_| async { None }).await;
        assert!(out[0].is_none(), "refused every time: given up");

        // Refused twice, then answered: retried, kept.
        let mut tries = 0u32;
        let out = split_aggregate(one, |part| {
            tries += 1;
            let ok = tries > 2;
            let res: Vec<Result3> = part
                .iter()
                .map(|_| Result3 {
                    success: true,
                    returnData: Bytes::from_static(&[7]),
                })
                .collect();
            async move { ok.then_some(res) }
        })
        .await;
        assert_eq!(out[0].as_ref().unwrap().returnData.as_ref(), &[7u8][..]);
        assert_eq!(tries, 3);
    }

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
                factory: 0,
                sqrt_price_x96: U256::ZERO,
                tick: 0,
                liquidity: 0,
                fee_pips: 500,
                tick_spacing: 10,
                ticks: Vec::new(),
                v4: None,
                window: None,
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

    /// Live: the Balancer weighted pools of the candidate registry
    /// (`LIQ_REGISTRY_FILE`, `data/review/2026-10-08-dex-registry.candidate.json`
    /// by default), built as the index builds them ([`crate::index::build_pool`]),
    /// read by [`refresh_balancer`] and quoted both ways at several sizes:
    /// every exact-input answer must equal `BalancerQueries.querySwap` at the
    /// same block, and the exact-output cost of buying that answer back must
    /// be what the Vault asks, and within the output token's rounding of the
    /// input that earned it.
    /// Oracle: Balancer's own query contract runs the Vault's `swap` and
    /// reverts the state.
    #[tokio::test]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn balancer_pools_quote_exactly_what_the_vault_does() {
        use liq_config::{Intern, PoolVenue, Registry};
        sol! {
            struct QSingleSwap { bytes32 poolId; uint8 kind; address assetIn; address assetOut; uint256 amount; bytes userData; }
            struct QFunds { address sender; bool fromInternalBalance; address recipient; bool toInternalBalance; }
            function querySwap(QSingleSwap singleSwap, QFunds funds) returns (uint256);
        }
        const QUERIES: Address = address!("E39B5e3B6D74016b2F6A9673D7d7493B6DF549d5");
        let url = std::env::var("MAINNET_RPC_URL").expect("MAINNET_RPC_URL");
        let rpc = HttpRpc::connect(&url).unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = std::env::var("LIQ_REGISTRY_FILE").map_or_else(
            |_| root.join("data/review/2026-10-08-dex-registry.candidate.json"),
            std::path::PathBuf::from,
        );
        let reg = Registry::from_path(&path).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let hops = crate::gas_model::HopGas {
            balancer: 1,
            univ3: 1,
            ..Default::default()
        };
        let mut assets = std::collections::HashMap::new();
        for rec in intern.assets() {
            assets.insert(rec.address, rec.id);
        }
        let mut book = PoolBook::new(assets, None, 100_000);
        for (addr, entry) in &reg.pools {
            if entry.venue == PoolVenue::Balancer {
                let pool = crate::index::build_pool(&intern, &reg, hops, *addr, entry).unwrap();
                book.add(pool).unwrap();
            }
        }
        let n = book.pools().len();
        assert!(n >= 4, "the reviewed Balancer pools: {n}");
        let block = rpc.block_number().await.unwrap();
        let lock = RwLock::new(book);
        let targets = balancer_targets(&lock.read(), block);
        assert_eq!(targets.len(), n, "every pool starts unread");
        let (read, applied) = refresh_balancer(&lock, &rpc, &targets, block).await;
        assert_eq!((read, applied), (n, n), "every pool reads and applies");
        let book = lock.into_inner();
        let mut checked = 0usize;
        let mut refused = 0usize;
        for pool in book.pools() {
            assert!(pool.is_live(), "{:#x} live after its read", pool.address);
            let PoolState::Balancer(st) = &pool.state else {
                panic!()
            };
            for (i, j) in [(0u8, 1u8), (1, 0)] {
                let (t_in, t_out) = (pool.tokens[usize::from(i)], pool.tokens[usize::from(j)]);
                for (num, den) in [
                    (1u64, 10_000_000u64),
                    (1, 10_000),
                    (1, 100),
                    (10, 100),
                    (29, 100),
                ] {
                    let dx = (st.balances[usize::from(i)] * U256::from(num) / U256::from(den))
                        .max(U256::ONE);
                    let ask = |kind: u8, amount: U256| {
                        let data: alloy_primitives::Bytes = querySwapCall {
                            singleSwap: QSingleSwap {
                                poolId: st.pool_id,
                                kind,
                                assetIn: t_in,
                                assetOut: t_out,
                                amount,
                                userData: alloy_primitives::Bytes::new(),
                            },
                            funds: QFunds {
                                sender: Address::repeat_byte(1),
                                fromInternalBalance: false,
                                recipient: Address::repeat_byte(1),
                                toInternalBalance: false,
                            },
                        }
                        .abi_encode()
                        .into();
                        let rpc = &rpc;
                        async move {
                            rpc.call_at(QUERIES, data, block)
                                .await
                                .ok()
                                .and_then(|raw| querySwapCall::abi_decode_returns(&raw).ok())
                        }
                    };
                    let want = ask(0, dx).await;
                    let got = pool.quote_exact_in(i, j, dx).ok();
                    assert_eq!(got, want, "{:#x} {i}->{j} dx {dx}", pool.address);
                    match want {
                        Some(out) => {
                            checked += 1;
                            // Buying `out` back costs what the Vault says, and
                            // at least the input that earned it.
                            let cost = ask(1, out).await;
                            let ours = st.dx(i, j, out).ok();
                            assert_eq!(ours, cost, "{:#x} {i}->{j} buy {out}", pool.address);
                            // The output was rounded down to the output token's
                            // own precision (relative `1/out`), and `powUp`
                            // pads the power by an absolute 1e-14 whatever the
                            // trade's size, which moves a small trade's cost by
                            // `balance · 1e-14` each way: a sanity bound, the
                            // exact answers being compared above.
                            if let Some(c) = cost {
                                let balance_in = st.balances[usize::from(i)];
                                let slack = dx * U256::from(2u64) / out.max(U256::ONE)
                                    + balance_in / U256::from(10_000_000_000_000u64)
                                        * U256::from(4u64)
                                    + U256::from(2u64);
                                assert!(
                                    c.abs_diff(dx) <= slack,
                                    "buy-back {c} vs sold {dx} (slack {slack})"
                                );
                            }
                        }
                        None => refused += 1,
                    }
                }
            }
        }
        eprintln!("{checked} Balancer quotes equal the Vault's at block {block} ({refused} refused beyond the ratio)");
        assert!(
            checked >= 32,
            "every pool, both ways, several sizes: {checked}"
        );
    }

    /// Live: the Fluid DEX pools of the candidate registry, seeded by the
    /// production read ([`refresh_fluid`]: both variable words, the center
    /// price hooks, the Liquidity layer's words and balances, the constants)
    /// and quoted at the block's own timestamp: every size the pool's own
    /// `swapIn` estimate answers must equal it to the wei. The estimate
    /// (`to = ADDRESS_DEAD`) reverts `FluidDexSwapResult(amountOut)` after the
    /// DEX math and before the Liquidity checks; it runs inside Multicall3 so
    /// the revert data comes back. The pools that take native ETH in are
    /// measured out of ETH only (a plain multicall carries no value); the
    /// recorded real executions cover the rest.
    /// Oracle: the deployed pools.
    #[tokio::test]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn fluid_pools_quote_exactly_what_the_pool_does() {
        use liq_config::{Intern, PoolVenue, Registry};
        sol! {
            function swapIn(bool swap0to1, uint256 amountIn, uint256 amountOutMin, address to) external payable returns (uint256);
        }
        const DEAD: Address = address!("000000000000000000000000000000000000dEaD");
        // `FluidDexSwapResult(uint256)`
        let result_sel = &alloy_primitives::keccak256("FluidDexSwapResult(uint256)")[..4];
        let url = std::env::var("MAINNET_RPC_URL").expect("MAINNET_RPC_URL");
        let rpc = HttpRpc::connect(&url).unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = std::env::var("LIQ_REGISTRY_FILE").map_or_else(
            |_| root.join("data/review/2026-10-08-dex-registry.candidate.json"),
            std::path::PathBuf::from,
        );
        let reg = Registry::from_path(&path).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let hops = crate::gas_model::HopGas {
            fluid: 1,
            univ3: 1,
            ..Default::default()
        };
        let mut assets = std::collections::HashMap::new();
        for rec in intern.assets() {
            assets.insert(rec.address, rec.id);
        }
        let mut book = PoolBook::new(assets, None, 100_000);
        for (addr, entry) in &reg.pools {
            if entry.venue == PoolVenue::Fluid {
                let pool = crate::index::build_pool(&intern, &reg, hops, *addr, entry).unwrap();
                book.add(pool).unwrap();
            }
        }
        let n = book.pools().len();
        assert!(n >= 30, "the candidate Fluid pools: {n}");
        let block = rpc.block_number().await.unwrap();
        let lock = RwLock::new(book);
        let targets = fluid_targets(&lock.read(), block);
        assert_eq!(targets.len(), n, "every pool starts unread");
        let (read, applied) = refresh_fluid(&lock, &rpc, &targets, block).await;
        assert_eq!((read, applied), (n, n), "every pool reads and applies");
        let book = lock.into_inner();
        // The estimate runs at the block's own timestamp.
        let ts = aggregate(
            &rpc,
            vec![call(
                MULTICALL3,
                getCurrentBlockTimestampCall {}.abi_encode(),
            )],
            block,
        )
        .await
        .and_then(|r| r.into_iter().next())
        .and_then(|r| getCurrentBlockTimestampCall::abi_decode_returns(&r.returnData).ok())
        .and_then(|t| u64::try_from(t).ok())
        .unwrap();
        let (mut checked, mut model_only_refusals, mut estimate_refusals) =
            (0usize, 0usize, 0usize);
        for pool in book.pools() {
            let PoolState::Fluid(st) = &pool.state else {
                panic!()
            };
            assert!(pool.is_live(), "{:#x} live after its read", pool.address);
            let mut st = st.clone();
            st.exec_ts = ts;
            for (i, j) in [(0u8, 1u8), (1, 0)] {
                if st.native[usize::from(i)] {
                    continue;
                }
                let scale = st.liq[usize::from(i)].balance.max(U256::from(1_000_000u64));
                for bps in [1u64, 10, 100, 500, 2_000] {
                    let dx =
                        (scale * U256::from(bps) / U256::from(10_000u64)).max(U256::from(1_000u64));
                    let zfo = i == 0;
                    let data = swapInCall {
                        swap0to1: zfo,
                        amountIn: dx,
                        amountOutMin: U256::ZERO,
                        to: DEAD,
                    }
                    .abi_encode();
                    // A throttled endpoint refuses some calls outright;
                    // retry before reading the answer (the estimate's revert
                    // is inside the multicall's own result, not a refusal).
                    let mut r = None;
                    for attempt in 0..8u64 {
                        r = aggregate(&rpc, vec![call(pool.address, data.clone())], block)
                            .await
                            .and_then(|r| r.into_iter().next());
                        if r.is_some() {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(
                            500u64.saturating_mul(attempt.saturating_add(1)),
                        ))
                        .await;
                    }
                    let r = r.expect("the node answered the estimate");
                    let want = (!r.success
                        && r.returnData.len() == 36
                        && r.returnData[..4] == *result_sel)
                        .then(|| U256::from_be_slice(&r.returnData[4..]));
                    let got = st.dy(i, j, dx).ok();
                    match (got, want) {
                        (Some(g), Some(w)) => {
                            assert_eq!(g, w, "{:#x} {i}->{j} dx {dx}", pool.address);
                            checked += 1;
                        }
                        (None, Some(_)) => model_only_refusals += 1, // a Liquidity limit
                        (Some(g), None) => panic!(
                            "{:#x} {i}->{j} dx {dx}: model answers {g}, the pool refuses",
                            pool.address
                        ),
                        (None, None) => estimate_refusals += 1,
                    }
                }
            }
        }
        eprintln!(
            "{checked} Fluid quotes equal the pools' at block {block} ({model_only_refusals} model-only refusals, {estimate_refusals} refused by both)"
        );
        assert!(
            checked >= 150,
            "every pool, both ways, several sizes: {checked}"
        );
        assert!(
            model_only_refusals * 10 <= checked,
            "model refuses what the estimate answers too often: {model_only_refusals}"
        );
    }

    /// Live: pools of SushiSwap V3 and PancakeSwap V3, found through each
    /// factory's `getPool`, seeded by [`seed_v3`] with the factory id the
    /// index gives them; the router's exact-input quote in each direction at
    /// three sizes must equal that fork's own `QuoterV2.quoteExactInputSingle`
    /// at the same block. Oracle: the quoter runs the pool's own `swap` and
    /// reverts with its result, so this covers the slot0 read, the tick reads through Uniswap's
    /// `TickLens`, each fork's tick spacing (Pancake's 2500 tier is 50) and
    /// the swap math, with Pancake's `lmPool` set on some pools.
    #[tokio::test]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn v3_fork_pools_quote_exactly_what_their_own_quoter_does() {
        use liq_router::{Pool, V3State};
        sol! {
            function getPool(address a, address b, uint24 fee) returns (address pool);
            struct QuoteParams { address tokenIn; address tokenOut; uint256 amountIn; uint24 fee; uint160 sqrtPriceLimitX96; }
            function quoteExactInputSingle(QuoteParams params) returns (uint256 amountOut, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed, uint256 gasEstimate);
        }
        let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let usdc = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let usdt = address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");
        let wbtc = address!("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
        let tokens = [weth, usdc, usdt, wbtc];
        // (name, factory id, factory, QuoterV2, fees)
        let forks: [(&str, u8, Address, Address, [u32; 4]); 2] = [
            (
                "sushi",
                liq_plan::V3_FACTORY_SUSHI,
                address!("0xbACEB8eC6b9355Dfc0269C18bac9d6E2Bdc29C4F"),
                address!("0x64e8802FE490fa7cc61d3463958199161Bb608A7"),
                [100, 500, 3000, 10_000],
            ),
            (
                "pancake",
                liq_plan::V3_FACTORY_PANCAKE,
                address!("0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865"),
                address!("0xB048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997"),
                [100, 500, 2500, 10_000],
            ),
        ];
        let url = std::env::var("MAINNET_RPC_URL").expect("MAINNET_RPC_URL");
        let rpc = HttpRpc::connect(&url).unwrap();
        let block = rpc.block_number().await.unwrap();
        let mut assets = std::collections::HashMap::new();
        for (k, t) in tokens.iter().enumerate() {
            assets.insert(*t, liq_types::AssetId(u16::try_from(k).unwrap()));
        }
        let mut book = PoolBook::new(assets.clone(), None, 100_000);
        // (fork name, quoter, fee) per book pool.
        let mut meta: Vec<(&str, Address, u32)> = Vec::new();
        for (name, fid, factory, quoter, fees) in forks {
            for (x, ta) in tokens.iter().enumerate() {
                for tb in tokens.iter().skip(x + 1) {
                    let (t0, t1) = if ta < tb { (*ta, *tb) } else { (*tb, *ta) };
                    for fee in fees {
                        let data: alloy_primitives::Bytes = getPoolCall {
                            a: t0,
                            b: t1,
                            fee: alloy_primitives::aliases::U24::from(fee),
                        }
                        .abi_encode()
                        .into();
                        let Ok(raw) = rpc.call_at(factory, data, block).await else {
                            continue;
                        };
                        let pool = getPoolCall::abi_decode_returns(&raw).unwrap();
                        if pool.is_zero() {
                            continue;
                        }
                        let spacing = match fee {
                            100 => 1,
                            500 => 10,
                            2500 => 50,
                            3000 => 60,
                            _ => 200,
                        };
                        book.add(Pool {
                            address: pool,
                            assets: smallvec::SmallVec::from_slice(&[assets[&t0], assets[&t1]]),
                            tokens: smallvec::SmallVec::from_slice(&[t0, t1]),
                            hop_gas: 100_000,
                            state: PoolState::V3(V3State {
                                factory: fid,
                                sqrt_price_x96: U256::ZERO,
                                tick: 0,
                                liquidity: 0,
                                fee_pips: fee,
                                tick_spacing: spacing,
                                ticks: Vec::new(),
                                v4: None,
                                window: None,
                            }),
                        })
                        .unwrap();
                        meta.push((name, quoter, fee));
                    }
                }
            }
        }
        let n = book.pools().len();
        assert!(n >= 12, "pools of both forks: {n}");
        let st = seed_v3(&mut book, &rpc).await;
        assert_eq!(st.pools, n);
        assert_eq!(st.failed, 0, "every fork pool seeds: {st:?}");
        let mut checked = 0usize;
        let mut refused = 0usize;
        let mut per_fork = std::collections::BTreeMap::<&str, usize>::new();
        for (pool, (name, quoter, fee)) in book.pools().iter().zip(&meta) {
            if !pool.is_live() {
                continue;
            }
            let PoolState::V3(s) = &pool.state else {
                unreachable!()
            };
            // Sizes from the pool's own scale: a ten-thousandth, a hundredth
            // and a tenth of its input-side virtual reserve at the current
            // price, from `liquidity` and the price.
            let l = U256::from(s.liquidity);
            let q96: U256 = U256::from(1u8) << 96usize;
            for (i, j) in [(0u8, 1u8), (1, 0)] {
                let token_in = pool.tokens[usize::from(i)];
                let token_out = pool.tokens[usize::from(j)];
                let reserve: U256 = if i == 0 {
                    l * q96 / s.sqrt_price_x96
                } else {
                    l * s.sqrt_price_x96 / q96
                };
                for div in [10_000u64, 100, 10] {
                    let dx = reserve / U256::from(div);
                    if dx.is_zero() {
                        continue;
                    }
                    let data: alloy_primitives::Bytes = quoteExactInputSingleCall {
                        params: QuoteParams {
                            tokenIn: token_in,
                            tokenOut: token_out,
                            amountIn: dx,
                            fee: alloy_primitives::aliases::U24::from(*fee),
                            sqrtPriceLimitX96: alloy_primitives::aliases::U160::ZERO,
                        },
                    }
                    .abi_encode()
                    .into();
                    // The quoter reverts past the pool's liquidity.
                    let Ok(raw) = rpc.call_at(*quoter, data, block).await else {
                        continue;
                    };
                    let quoted = quoteExactInputSingleCall::abi_decode_returns(&raw).unwrap();
                    let want = quoted.amountOut;
                    // Past the seeded tick window the router refuses rather
                    // than guess. The quoter says where the swap ends: a
                    // swap that ends a spacing or more inside the window
                    // crosses only ticks the seed read, and must be quoted.
                    let after = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(
                        alloy_primitives::U256::from(quoted.sqrtPriceX96After),
                    )
                    .unwrap();
                    let inside = s.window.is_none_or(|(lo, hi)| {
                        after >= lo + s.tick_spacing && after <= hi - s.tick_spacing
                    });
                    let Ok(got) = pool.quote_exact_in(i, j, dx) else {
                        assert!(
                            !inside,
                            "{name} {} {i}->{j} dx {dx}: refused, but the swap ends at tick {after} inside the window {:?}",
                            pool.address, s.window
                        );
                        refused += 1;
                        continue;
                    };
                    assert_eq!(
                        got, want,
                        "{name} {} fee {fee} {i}->{j} dx {dx}",
                        pool.address
                    );
                    checked += 1;
                    *per_fork.entry(name).or_default() += 1;
                }
            }
        }
        eprintln!(
            "{checked} fork quotes equal their quoters' at block {block}: {per_fork:?} ({refused} beyond the window)"
        );
        assert!(
            per_fork.get("sushi").copied().unwrap_or(0) >= 10
                && per_fork.get("pancake").copied().unwrap_or(0) >= 10,
            "both forks checked: {per_fork:?}"
        );
    }

    /// Live: two committed V4 pools, LAC/USDC (1 %, the only pool LAC has)
    /// and native ETH/USDC (0.05 %), seeded by [`seed_v4`] and built as the
    /// index builds them; the router's exact-input quote in each direction
    /// at three sizes must equal Uniswap's `V4Quoter.quoteExactInputSingle`
    /// at the same block. Oracle: the quoter runs the PoolManager's own
    /// `swap` and reverts with its result.
    #[tokio::test]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn v4_pools_quote_exactly_what_the_v4_quoter_does() {
        use liq_config::{Intern, Registry};
        use liq_router::{Pool, V3State, V4Key};
        sol! {
            struct QPoolKey { address currency0; address currency1; uint24 fee; int24 tickSpacing; address hooks; }
            struct QuoteExactSingleParams { QPoolKey poolKey; bool zeroForOne; uint128 exactAmount; bytes hookData; }
            function quoteExactInputSingle(QuoteExactSingleParams params) returns (uint256 amountOut, uint256 gasEstimate);
        }
        const V4_QUOTER: Address = address!("0x52f0e24d1c21c8a0cb1e5a5dd6198556bd9e1203");
        let url = std::env::var("MAINNET_RPC_URL").expect("MAINNET_RPC_URL");
        let rpc = HttpRpc::connect(&url).unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let mut assets = std::collections::HashMap::new();
        let mut book_pools = Vec::new();
        for key in [
            address!("0xda7843c94d974e0697e1768fb9c8b695f986a45b"),
            address!("0xaab21826d33ca12bb9f565d8496e8fda8a82ca27"),
        ] {
            let e = reg.pools.get(&key).expect("committed univ4 pool");
            let (a0, a1) = (
                intern.asset(e.token0).unwrap(),
                intern.asset(e.token1).unwrap(),
            );
            assets.insert(e.token0, a0);
            assets.insert(e.token1, a1);
            book_pools.push(Pool {
                address: key,
                assets: smallvec::SmallVec::from_slice(&[a0, a1]),
                tokens: smallvec::SmallVec::from_slice(&[e.token0, e.token1]),
                hop_gas: 86_332,
                state: PoolState::V3(V3State {
                    factory: 0,
                    sqrt_price_x96: U256::ZERO,
                    tick: 0,
                    liquidity: 0,
                    fee_pips: e.fee,
                    tick_spacing: e.tick_spacing.unwrap(),
                    ticks: Vec::new(),
                    v4: Some(V4Key {
                        currency0: e.v4_currency0(),
                        currency1: e.token1,
                        fee: e.fee,
                        tick_spacing: e.tick_spacing.unwrap(),
                        hooks: e.hooks.unwrap(),
                        id: e.v4_id.unwrap(),
                    }),
                    window: None,
                }),
            });
        }
        let mut book = PoolBook::new(assets, None, 100_000);
        for p in book_pools {
            book.add(p).unwrap();
        }
        let st = seed_v4(&mut book, &rpc).await;
        assert_eq!((st.pools, st.seeded, st.failed), (2, 2, 0));
        let block = rpc.block_number().await.unwrap();
        let mut checked = 0;
        for pool in book.pools() {
            let PoolState::V3(s) = &pool.state else {
                panic!()
            };
            let k = s.v4.unwrap();
            for (i, j) in [(0u8, 1u8), (1, 0)] {
                // A thousandth, a hundredth and a tenth of one side's
                // virtual reserve at the current price.
                let unit = if i == 0 {
                    U256::from(10u64).pow(U256::from(18u64))
                } else {
                    U256::from(1_000_000u64)
                };
                for mult in [1u64, 100, 10_000] {
                    let dx = unit * U256::from(mult);
                    let data: alloy_primitives::Bytes = quoteExactInputSingleCall {
                        params: QuoteExactSingleParams {
                            poolKey: QPoolKey {
                                currency0: k.currency0,
                                currency1: k.currency1,
                                fee: alloy_primitives::aliases::U24::from(k.fee),
                                tickSpacing: alloy_primitives::aliases::I24::try_from(
                                    k.tick_spacing,
                                )
                                .unwrap(),
                                hooks: k.hooks,
                            },
                            zeroForOne: i == 0,
                            exactAmount: u128::try_from(dx).unwrap(),
                            hookData: alloy_primitives::Bytes::new(),
                        },
                    }
                    .abi_encode()
                    .into();
                    let Ok(raw) = rpc.call_at(V4_QUOTER, data, block).await else {
                        continue; // beyond the pool's liquidity: the quoter reverts
                    };
                    let want = quoteExactInputSingleCall::abi_decode_returns(&raw)
                        .unwrap()
                        .amountOut;
                    // Past the seeded tick window the router refuses
                    // rather than guess; only the largest size may get there.
                    let Ok(got) = pool.quote_exact_in(i, j, dx) else {
                        assert_eq!(
                            mult, 10_000,
                            "{} {i}->{j} dx {dx}: refused inside the window",
                            k.id
                        );
                        continue;
                    };
                    assert_eq!(got, want, "{} {i}->{j} dx {dx}", k.id);
                    checked += 1;
                }
            }
        }
        eprintln!("{checked} V4 quotes equal the V4Quoter's at block {block}");
        assert!(checked >= 6, "both pools quoted both ways");
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
                ng: false,
                d_once: false,
                offpeg_fee_multiplier: U256::ZERO,
                dynamic_rates: false,
                read_block: 0,
                handler: 0,
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
        let block = rpc.block_number().await.unwrap();
        let targets = curve_targets(&book, false, block);
        let query: Vec<(Address, usize, bool)> =
            targets.iter().map(|&(_, a, n, ng)| (a, n, ng)).collect();
        let reads = read_curve(&rpc, &query, block).await;
        let mut checked = 0usize;
        let mut wrong = Vec::new();
        for (&(i, addr, n, _), r) in targets.iter().zip(reads) {
            let r = r.unwrap_or_else(|| panic!("curve read failed for {addr}"));
            let id = PoolId(u32::try_from(i).unwrap());
            let ng =
                r.ng.as_ref()
                    .map(|(rates, offpeg)| (rates.as_slice(), *offpeg));
            assert!(book
                .reseed_curve(id, &r.balances, r.a, r.a_precision, r.fee, ng, block)
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
                    // A throttled endpoint answers some calls with an error;
                    // retry before calling it a revert.
                    let data: alloy_primitives::Bytes = data.into();
                    let mut raw = rpc.call_at(addr, data.clone(), block).await;
                    for _ in 0..5 {
                        if raw.is_ok() {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                        raw = rpc.call_at(addr, data.clone(), block).await;
                    }
                    let raw = raw.unwrap();
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

        // Crypto pools: seeded by the same reseed code, quoted by the exact
        // port, against the pool's `get_dy(uint256,uint256,uint256)` at the
        // same block.
        mod crypto_view {
            alloy_sol_types::sol! {
                function get_dy(uint256 i, uint256 j, uint256 dx) returns (uint256);
            }
        }
        let lock = RwLock::new(book);
        let crypto = crypto_targets(&lock.read(), block);
        let (mut c_read, mut c_applied) = refresh_crypto(&lock, &rpc, &crypto, block).await;
        // A throttled endpoint refuses whole Multicall3 batches: the pools
        // still unread are read again, a few times, before that counts.
        for _ in 0..5 {
            if c_read >= crypto.len() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let again = crypto_targets(&lock.read(), block);
            let (r, a) = refresh_crypto(&lock, &rpc, &again, block).await;
            c_read += r;
            c_applied += a;
        }
        assert_eq!(c_read, crypto.len(), "every crypto pool reads");
        let mut book = lock.into_inner();
        let mut c_checked = 0usize;
        let mut c_followed = 0usize;
        let mut c_wrong = Vec::new();
        for t in &crypto {
            let (i, addr, n) = (t.index, t.address, t.n_coins);
            let pool = book.get(PoolId(u32::try_from(i).unwrap())).unwrap();
            if !pool.is_live() {
                continue; // ramping at this block
            }
            let PoolState::Crypto(st) = &pool.state else {
                unreachable!()
            };
            // A followed pool carries its `tweak_price` state, read whole,
            // with the cached oracle near the oracle the view reports.
            if t.followed {
                let tw = st
                    .tweak
                    .as_ref()
                    .unwrap_or_else(|| panic!("{addr}: tweak state"));
                assert!(!tw.total_supply.is_zero() && !tw.virtual_price.is_zero());
                assert!(tw.last_timestamp <= tw.exec_ts && !tw.oracle_slot.is_zero());
                let ps = st.price_scale[0];
                let near = tw.price_oracle > ps / U256::from(2u64)
                    && tw.price_oracle < ps * U256::from(2u64);
                assert!(near, "{addr}: oracle {} vs scale {ps}", tw.price_oracle);
                c_followed += 1;
            }
            for ci in 0..n {
                for cj in 0..n {
                    if ci == cj {
                        continue;
                    }
                    let dx = (st.balances[ci] / U256::from(1000u64)).max(U256::ONE);
                    let data: alloy_primitives::Bytes = crypto_view::get_dyCall {
                        i: U256::from(ci),
                        j: U256::from(cj),
                        dx,
                    }
                    .abi_encode()
                    .into();
                    let mut raw = rpc.call_at(addr, data.clone(), block).await;
                    for _ in 0..5 {
                        if raw.is_ok() {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                        raw = rpc.call_at(addr, data.clone(), block).await;
                    }
                    let want = raw
                        .ok()
                        .and_then(|r| crypto_view::get_dyCall::abi_decode_returns(&r).ok());
                    let got = pool
                        .quote_exact_in(u8::try_from(ci).unwrap(), u8::try_from(cj).unwrap(), dx)
                        .ok();
                    c_checked += 1;
                    if got != want {
                        c_wrong.push(format!("{addr} {ci}->{cj}: solver {got:?} chain {want:?}"));
                    }
                }
            }
        }
        eprintln!(
            "{} crypto pools ({c_applied} seeded, {c_followed} followed), {c_checked} quotes checked",
            crypto.len()
        );
        assert!(
            c_followed >= 2,
            "the two_stable pools read their tweak_price state: {c_followed}"
        );
        assert!(
            c_wrong.is_empty(),
            "crypto solver != get_dy:\n{}",
            c_wrong.join("\n")
        );
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

    /// Live check (`MAINNET_RPC_URL`): every committed unwrap reads a rate at
    /// the head, and the linear conversion of one whole unit is at or under
    /// the chain's own answer for that size, within 2 ppm.
    #[tokio::test]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn committed_unwraps_read_and_convert_conservatively() {
        use liq_config::{Intern, Registry};
        let url = std::env::var("MAINNET_RPC_URL").expect("MAINNET_RPC_URL");
        let rpc = HttpRpc::connect(&url).unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let book = crate::index::load_index(&root.join("config"), &intern, &reg).book;
        let block = rpc.block_number().await.unwrap();
        let lock = RwLock::new(book);
        let targets = unwrap_targets(&lock.read(), block);
        let (read, applied) = refresh_unwraps(&lock, &rpc, &targets, block).await;
        eprintln!(
            "{} unwraps, {read} read, {applied} applied at {block}",
            targets.len()
        );
        assert_eq!(read, targets.len(), "every unwrap reads");
        // The chain's answer for one whole unit (scale / 1000).
        let unit: Vec<UnwrapTarget> = targets
            .iter()
            .map(|t| UnwrapTarget {
                scale: t.scale / U256::from(1_000u64),
                ..*t
            })
            .collect();
        let want = read_unwrap_rates(&rpc, &unit, block).await;
        let book = lock.into_inner();
        let mut wrong = Vec::new();
        for (t, w) in unit.iter().zip(want) {
            // Curve LPs and Pendle markets are exact, checked against the
            // chain in their own tests (live withdrawals, recorded sales).
            if matches!(
                t.kind,
                liq_router::UnwrapKind::CurveLp { .. }
                    | liq_router::UnwrapKind::PendleMarket { .. }
            ) {
                continue;
            }
            let u = book.unwrap_of(t.wrapper).unwrap();
            let got = u.convert(t.scale, &book).unwrap();
            let w = w.unwrap();
            let slack = w / U256::from(500_000u64) + U256::from(2u64);
            if got > w || w - got > slack {
                wrong.push(format!("{:#x}: convert {got} chain {w}", t.token));
            }
        }
        assert!(
            wrong.is_empty(),
            "unwrap conversion off:\n{}",
            wrong.join("\n")
        );
    }

    /// A market is expired for the block its snapshot quotes: the switch to
    /// the redeem happens once the next block is at or past expiry, and not
    /// a block earlier.
    #[test]
    fn market_expires_at_the_quoted_block() {
        let mut s = liq_router::pendle::MarketSnapshot {
            total_pt: alloy_primitives::I256::ONE,
            total_sy: alloy_primitives::I256::ONE,
            scalar_root: alloy_primitives::I256::ONE,
            expiry: 1_000,
            ln_fee_rate_root: U256::ZERO,
            reserve_fee_percent: U256::ZERO,
            last_ln_implied_rate: U256::ZERO,
            index: U256::ONE,
            quote_ts: 999,
            out_per_sy_scale: U256::ONE,
            sy_scale: U256::ONE,
        };
        assert!(!market_expired(&s));
        s.quote_ts = 1_000;
        assert!(market_expired(&s));
    }

    /// Live check (`MAINNET_RPC_URL`): a PT registered as a live-market PT
    /// whose market has expired (PT-sUSDE-26DEC2024) is switched by the
    /// reseed to the post-expiry redeem, then read and quoted as one.
    #[tokio::test]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn expired_market_pt_switches_to_the_redeem_live() {
        use liq_config::{Intern, Registry};
        let pt = address!("0xee9085fc268f6727d5d4293dbabccf901ffdcc29");
        let susde = address!("0x9d39a5de30e57443bff2a8307a4256c8797a3497");
        let url = std::env::var("MAINNET_RPC_URL").expect("MAINNET_RPC_URL");
        let rpc = HttpRpc::connect(&url).unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let mut book = crate::index::load_index(&root.join("config"), &intern, &reg).book;
        let wrapper = intern.asset(pt).unwrap();
        book.add_unwrap(liq_router::Unwrap {
            kind: liq_router::UnwrapKind::PendleMarket {
                market: address!("0xa0ab94debb3cc9a7ea77f3205ba4ab23276fed08"),
                yt: address!("0xbe05538f48d76504953c5d1068898c6642937427"),
                sy: address!("0xd288755556c235afffb6316702719c32bd8706e8"),
            },
            wrapper,
            wrapper_token: pt,
            into: intern.asset(susde).unwrap(),
            into_token: susde,
            rate: liq_router::UnwrapRate::Unread,
            scale: U256::from(1_000_000_000_000_000_000u64),
            read_block: 0,
            gas: 445_782,
            expiry_gas: 141_280,
            cash_capped: false,
        });
        let block = rpc.block_number().await.unwrap();
        let lock = RwLock::new(book);
        let first: Vec<_> = unwrap_targets(&lock.read(), block)
            .into_iter()
            .filter(|t| t.wrapper == wrapper)
            .collect();
        let (_, applied) = refresh_unwraps(&lock, &rpc, &first, block).await;
        assert_eq!(applied, 1, "the expired market switched");
        let kind = lock.read().unwrap_of(wrapper).unwrap().kind;
        assert!(
            matches!(kind, liq_router::UnwrapKind::PendlePt { .. }),
            "{kind:?}"
        );
        let second: Vec<_> = unwrap_targets(&lock.read(), block)
            .into_iter()
            .filter(|t| t.wrapper == wrapper)
            .collect();
        let (read, applied) = refresh_unwraps(&lock, &rpc, &second, block).await;
        assert_eq!((read, applied), (1, 1), "the redeem rate reads");
        let book = lock.into_inner();
        let u = book.unwrap_of(wrapper).unwrap();
        assert!(matches!(u.rate, liq_router::UnwrapRate::Linear { .. }));
        assert_eq!(u.gas, 141_280);
        let one = U256::from(1_000_000_000_000_000_000u64);
        let out = u.convert(one, &book).unwrap();
        // One expired PT redeems for one USDe of sUSDe: less than 1 sUSDe.
        assert!(!out.is_zero() && out < one, "{out}");
        eprintln!("PT-sUSDE-26DEC2024: 1 PT → {out} sUSDe via the redeem at {block}");
    }

    /// Live check (`MAINNET_RPC_URL`): every committed live-PT market reads
    /// as a snapshot, and one PT sells through it (the port itself is
    /// checked against recorded sales in `liq-router`'s `pendle_vectors`).
    #[tokio::test]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn committed_pendle_markets_read_and_quote() {
        use liq_config::{Intern, Registry};
        let url = std::env::var("MAINNET_RPC_URL").expect("MAINNET_RPC_URL");
        let rpc = HttpRpc::connect(&url).unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let book = crate::index::load_index(&root.join("config"), &intern, &reg).book;
        let block = rpc.block_number().await.unwrap();
        let lock = RwLock::new(book);
        let targets: Vec<_> = unwrap_targets(&lock.read(), block)
            .into_iter()
            .filter(|t| matches!(t.kind, liq_router::UnwrapKind::PendleMarket { .. }))
            .collect();
        assert!(!targets.is_empty(), "registry has live-PT markets");
        let (read, _) = refresh_unwraps(&lock, &rpc, &targets, block).await;
        assert_eq!(read, targets.len(), "every market reads");
        let book = lock.into_inner();
        for t in &targets {
            let u = book.unwrap_of(t.wrapper).unwrap();
            let out = u.convert(u.scale, &book).unwrap();
            assert!(!out.is_zero(), "{:#x}: one PT sells for nothing", t.token);
        }
        eprintln!("{} markets read and quoted at {block}", targets.len());
    }

    /// Live check (`MAINNET_RPC_URL`): every committed Curve LP unwrap,
    /// quoted on its pool as the reseed reads it, equals the pool's own
    /// `calc_withdraw_one_coin` to the wei at the same block — for one LP
    /// and for a tenth of the supply.
    #[tokio::test]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn committed_curve_lp_unwraps_quote_exactly() {
        use liq_config::{Intern, Registry};
        sol! {
            function calc_withdraw_one_coin(uint256 burn, int128 i) returns (uint256);
        }
        let url = std::env::var("MAINNET_RPC_URL").expect("MAINNET_RPC_URL");
        let rpc = HttpRpc::connect(&url).unwrap();
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let intern = Intern::from_registry(&reg).unwrap();
        let mut book = crate::index::load_index(&root.join("config"), &intern, &reg).book;
        let block = rpc.block_number().await.unwrap();
        let lps: Vec<liq_router::Unwrap> = book
            .unwraps()
            .filter(|u| matches!(u.kind, liq_router::UnwrapKind::CurveLp { .. }))
            .cloned()
            .collect();
        assert!(!lps.is_empty(), "registry has Curve LP unwraps");
        let targets: Vec<_> = curve_targets(&book, false, block)
            .into_iter()
            .filter(|t| lps.iter().any(|u| u.wrapper_token == t.1))
            .collect();
        let query: Vec<(Address, usize, bool)> =
            targets.iter().map(|&(_, a, n, ng)| (a, n, ng)).collect();
        for (&(i, addr, _, _), r) in targets.iter().zip(read_curve(&rpc, &query, block).await) {
            let r = r.unwrap_or_else(|| panic!("curve read failed for {addr}"));
            let ng =
                r.ng.as_ref()
                    .map(|(rates, offpeg)| (rates.as_slice(), *offpeg));
            let id = PoolId(u32::try_from(i).unwrap());
            assert!(book
                .reseed_curve(id, &r.balances, r.a, r.a_precision, r.fee, ng, block)
                .unwrap());
        }
        let lock = RwLock::new(book);
        let ut: Vec<_> = unwrap_targets(&lock.read(), block)
            .into_iter()
            .filter(|t| matches!(t.kind, liq_router::UnwrapKind::CurveLp { .. }))
            .collect();
        let (read, _) = refresh_unwraps(&lock, &rpc, &ut, block).await;
        assert_eq!(read, ut.len(), "every LP supply reads");
        let book = lock.into_inner();
        let mut checked = 0usize;
        let mut wrong = Vec::new();
        for u in &lps {
            let liq_router::UnwrapKind::CurveLp { i } = u.kind else {
                continue;
            };
            let u = book.unwrap_of(u.wrapper).unwrap();
            let liq_router::UnwrapRate::CurveLp { total_supply } = u.rate else {
                panic!("{:#x} unread", u.wrapper_token);
            };
            for amt in [
                u.scale * U256::from(1_000u64),
                total_supply / U256::from(10u64),
            ] {
                let data: alloy_primitives::Bytes = calc_withdraw_one_coinCall {
                    burn: amt,
                    i: i128::from(i),
                }
                .abi_encode()
                .into();
                let mut raw = rpc.call_at(u.wrapper_token, data.clone(), block).await;
                for _ in 0..5 {
                    if raw.is_ok() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                    raw = rpc.call_at(u.wrapper_token, data.clone(), block).await;
                }
                let want = calc_withdraw_one_coinCall::abi_decode_returns(&raw.unwrap()).unwrap();
                let got = u.convert(amt, &book).ok();
                checked += 1;
                if got != Some(want) {
                    wrong.push(format!(
                        "{:#x} {amt}: port {got:?} chain {want}",
                        u.wrapper_token
                    ));
                }
            }
        }
        eprintln!(
            "{} LPs, {checked} withdrawals checked at {block}",
            lps.len()
        );
        assert!(wrong.is_empty(), "LP withdrawal off:\n{}", wrong.join("\n"));
    }
}
