//! Pool models, exact quoting and the log-driven `PoolBook` (GUIDE 12 §3).
//!
//! Three venue families are quotable — Uniswap V2, Uniswap V3 and Curve
//! StableSwap (plain pools). **Uniswap V4 and Balancer are not variants of
//! [`Venue`]**: a pool of those kinds cannot be constructed, so it cannot be
//! routed (GUIDE 12 §3 "Do not route swaps through Uniswap V4",
//! `DEPENDENCIES.md` §0.4).
//!
//! Every quote is exact integer math replicating the contract:
//!
//! * V3 — the `swap()` loop of `UniswapV3Pool.sol` step for step: word-
//!   boundary stepping (`nextInitializedTickWithinOneWord`), then
//!   `SwapMath.computeSwapStep` / `SqrtPriceMath` / `TickMath` from
//!   `uniswap_v3_math` (the Solidity port both `amms-rs` and
//!   `Dex-Math-Core-rs` delegate to).
//! * V2 — `UniswapV2Library.getAmountOut`, `997 / 1000`.
//! * Curve — `StableSwap.get_dy`: `get_D` / `get_y` Newton with the pool's
//!   own convergence rule, `A_PRECISION` generalised (`1` for 3pool-era
//!   pools, `100` for later plain pools).
//!
//! State is folded from the log stream (`LogSubscriber`), never polled:
//! V3 `Initialize`/`Swap`/`Mint`/`Burn` and V2 `Sync` are exact folds.
//! Curve balances are **not** log-derivable without the pool's admin-fee
//! split, so any Curve log marks the pool stale (excluded from routing)
//! until the warm tier's off-hot-path reader re-seeds it (carry-forward).

use std::collections::HashMap;

use alloy_primitives::{Address, B256, I256, U256, U512};
use alloy_sol_types::{sol, SolEvent};
use liq_protocol::DecodedLog;
use liq_types::{AssetId, LogFilter, LogSubscriber};
use smallvec::SmallVec;
use uniswap_v3_math::{liquidity_math, swap_math, tick_math};

use crate::balancer::BalancerState;
use crate::crypto::CryptoState;
use crate::fluid::FluidState;

/// Why a quote or solve refused. Every variant is a *refusal*, never a
/// guess: the caller logs and skips.
#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    /// Coin indices do not name a swap this pool supports.
    #[error("leg does not exist on this pool")]
    BadLeg,
    /// Pool state is not quotable (uninitialised, un-folded, or stale).
    #[error("pool state stale or uninitialised")]
    StalePool,
    /// The pool(s) cannot absorb the requested size.
    #[error("insufficient liquidity for size")]
    InsufficientLiquidity,
    /// 256-bit arithmetic would overflow, or an iteration cap was hit.
    #[error("arithmetic overflow or non-convergence")]
    Math,
    /// An input the solve needs (price, gas terms, bucket ladder) is
    /// absent. Logged upstream; nothing is invented in its place.
    #[error("required input missing")]
    MissingInput,
}

/// `2^96`.
pub const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);
/// `2^192`.
pub const Q192: U256 = U256::from_limbs([0, 0, 0, 1]);
/// `1e6`: V3 fee denominator (pips).
pub const PIPS: u32 = 1_000_000;
/// `1e10`: Curve `FEE_DENOMINATOR`.
pub const CURVE_FEE_DENOM: U256 = U256::from_limbs([10_000_000_000, 0, 0, 0]);
/// `1e18`: Curve `PRECISION`.
pub const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
/// Most coins a Curve plain pool may hold here.
pub const MAX_COINS: usize = 4;

sol! {
    /// PancakeSwap V3's Swap event: Uniswap's, with the two protocol-fee
    /// amounts the pool took on the swap. Every other pool event has the
    /// same signature as Uniswap's (`PancakeV3Pool.sol`, read 2026-10-08).
    #[allow(clippy::too_many_arguments)] // the event's own nine fields
    interface IPancakeV3Pool {
        event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick, uint128 protocolFeesToken0, uint128 protocolFeesToken1);
    }

    interface IUniswapV3Pool {
        event Initialize(uint160 sqrtPriceX96, int24 tick);
        event Mint(address sender, address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
        event Burn(address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
        event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick);
    }
    interface IUniswapV3Factory {
        event PoolCreated(address indexed token0, address indexed token1, uint24 indexed fee, int24 tickSpacing, address pool);
    }
    interface IUniswapV2Pair {
        event Sync(uint112 reserve0, uint112 reserve1);
    }
    interface ICurvePool {
        event TokenExchange(address indexed buyer, int128 sold_id, uint256 tokens_sold, int128 bought_id, uint256 tokens_bought);
        event AddLiquidity2(address indexed provider, uint256[2] token_amounts, uint256[2] fees, uint256 invariant, uint256 token_supply);
        event AddLiquidity3(address indexed provider, uint256[3] token_amounts, uint256[3] fees, uint256 invariant, uint256 token_supply);
        event AddLiquidity4(address indexed provider, uint256[4] token_amounts, uint256[4] fees, uint256 invariant, uint256 token_supply);
        event RemoveLiquidity2(address indexed provider, uint256[2] token_amounts, uint256[2] fees, uint256 token_supply);
        event RemoveLiquidity3(address indexed provider, uint256[3] token_amounts, uint256[3] fees, uint256 token_supply);
        event RemoveLiquidity4(address indexed provider, uint256[4] token_amounts, uint256[4] fees, uint256 token_supply);
        event RemoveLiquidityOne(address indexed provider, uint256 token_amount, uint256 coin_amount);
        event RemoveLiquidityOneSupply(address indexed provider, uint256 token_amount, uint256 coin_amount, uint256 token_supply);
        event RemoveLiquidityImbalance2(address indexed provider, uint256[2] token_amounts, uint256[2] fees, uint256 invariant, uint256 token_supply);
        event RemoveLiquidityImbalance3(address indexed provider, uint256[3] token_amounts, uint256[3] fees, uint256 invariant, uint256 token_supply);
        event RemoveLiquidityImbalance4(address indexed provider, uint256[4] token_amounts, uint256[4] fees, uint256 invariant, uint256 token_supply);
        event RampA(uint256 old_A, uint256 new_A, uint256 initial_time, uint256 future_time);
        event StopRampA(uint256 A, uint256 t);
        event NewFee(uint256 fee, uint256 admin_fee);
        // StableSwap-NG (`DynArray` logs as `uint256[]`).
        event AddLiquidity(address indexed provider, uint256[] token_amounts, uint256[] fees, uint256 invariant, uint256 token_supply);
        event RemoveLiquidity(address indexed provider, uint256[] token_amounts, uint256[] fees, uint256 token_supply);
        event RemoveLiquidityImbalance(address indexed provider, uint256[] token_amounts, uint256[] fees, uint256 invariant, uint256 token_supply);
        event ApplyNewFee(uint256 fee, uint256 offpeg_fee_multiplier);
    }
}

/// `keccak256("RemoveLiquidityOne(address,int128,uint256,uint256,uint256)")`:
/// NG's event shares the plain name with a different signature, so the
/// `sol!` alias above hashes the wrong name.
const NG_REMOVE_LIQUIDITY_ONE: B256 =
    alloy_primitives::b256!("6f48129db1f37ccb9cc5dd7e119cb32750cabdf75b48375d730d26ce3659bbe1");

/// Curve topic0s that invalidate a plain pool's cached state. The event
/// *names* are the Vyper ones; the `sol!` aliases above only exist because
/// the array width is part of the signature.
const CURVE_STALE_TOPICS: [B256; 15] = [
    ICurvePool::TokenExchange::SIGNATURE_HASH,
    ICurvePool::AddLiquidity2::SIGNATURE_HASH,
    ICurvePool::AddLiquidity3::SIGNATURE_HASH,
    ICurvePool::AddLiquidity4::SIGNATURE_HASH,
    ICurvePool::RemoveLiquidity2::SIGNATURE_HASH,
    ICurvePool::RemoveLiquidity3::SIGNATURE_HASH,
    ICurvePool::RemoveLiquidity4::SIGNATURE_HASH,
    ICurvePool::RemoveLiquidityOne::SIGNATURE_HASH,
    ICurvePool::RemoveLiquidityOneSupply::SIGNATURE_HASH,
    ICurvePool::RemoveLiquidityImbalance2::SIGNATURE_HASH,
    ICurvePool::RemoveLiquidityImbalance3::SIGNATURE_HASH,
    ICurvePool::RemoveLiquidityImbalance4::SIGNATURE_HASH,
    ICurvePool::RampA::SIGNATURE_HASH,
    ICurvePool::StopRampA::SIGNATURE_HASH,
    ICurvePool::NewFee::SIGNATURE_HASH,
];

/// StableSwap-NG topic0s that invalidate the cached state.
const CURVE_NG_STALE_TOPICS: [B256; 8] = [
    ICurvePool::TokenExchange::SIGNATURE_HASH,
    ICurvePool::AddLiquidity::SIGNATURE_HASH,
    ICurvePool::RemoveLiquidity::SIGNATURE_HASH,
    NG_REMOVE_LIQUIDITY_ONE,
    ICurvePool::RemoveLiquidityImbalance::SIGNATURE_HASH,
    ICurvePool::RampA::SIGNATURE_HASH,
    ICurvePool::StopRampA::SIGNATURE_HASH,
    ICurvePool::ApplyNewFee::SIGNATURE_HASH,
];

impl CurveState {
    fn stale_topics(&self) -> &'static [B256] {
        if self.ng {
            &CURVE_NG_STALE_TOPICS
        } else {
            &CURVE_STALE_TOPICS
        }
    }
}

/// Curve crypto-pool events. `sol!` gives the 8-field v1 event an 8-argument
/// constructor, hence the module-level allow.
#[allow(clippy::too_many_arguments)]
mod crypto_events {
    alloy_sol_types::sol! {
        // Curve crypto pools. The names are the Vyper ones; one interface per
        // shape, because the array width and field count are in the signature.
        interface ICryptoNg {
            event TokenExchange(address indexed buyer, uint256 sold_id, uint256 tokens_sold, uint256 bought_id, uint256 tokens_bought, uint256 fee, uint256 packed_price_scale);
            event RemoveLiquidityOne(address indexed provider, uint256 token_amount, uint256 coin_index, uint256 coin_amount, uint256 approx_fee, uint256 packed_price_scale);
            event NewParameters(uint256 mid_fee, uint256 out_fee, uint256 fee_gamma, uint256 allowed_extra_profit, uint256 adjustment_step, uint256 ma_time);
            event RampAgamma(uint256 initial_A, uint256 future_A, uint256 initial_gamma, uint256 future_gamma, uint256 initial_time, uint256 future_time);
            event StopRampA(uint256 current_A, uint256 current_gamma, uint256 time);
            event ClaimAdminFee(address indexed admin, uint256 tokens);
            event CommitNewParameters(uint256 indexed deadline, uint256 mid_fee, uint256 out_fee, uint256 fee_gamma, uint256 allowed_extra_profit, uint256 adjustment_step, uint256 ma_time);
        }
        interface ICryptoTwo {
            event AddLiquidity(address indexed provider, uint256[2] token_amounts, uint256 fee, uint256 token_supply, uint256 packed_price_scale);
            event RemoveLiquidity(address indexed provider, uint256[2] token_amounts, uint256 token_supply);
            event ClaimAdminFee(address indexed admin, uint256[2] tokens);
            event NewParameters(uint256 mid_fee, uint256 out_fee, uint256 fee_gamma, uint256 allowed_extra_profit, uint256 adjustment_step, uint256 ma_time, uint256 xcp_ma_time);
        }
        interface ICryptoTri {
            event AddLiquidity(address indexed provider, uint256[3] token_amounts, uint256 fee, uint256 token_supply, uint256 packed_price_scale);
            event RemoveLiquidity(address indexed provider, uint256[3] token_amounts, uint256 token_supply);
        }
        interface ICryptoV1 {
            event TokenExchange(address indexed buyer, uint256 sold_id, uint256 tokens_sold, uint256 bought_id, uint256 tokens_bought);
            event AddLiquidity(address indexed provider, uint256[2] token_amounts, uint256 fee, uint256 token_supply);
            event RemoveLiquidityOne(address indexed provider, uint256 token_amount, uint256 coin_index, uint256 coin_amount);
            event CommitNewParameters(uint256 indexed deadline, uint256 admin_fee, uint256 mid_fee, uint256 out_fee, uint256 fee_gamma, uint256 allowed_extra_profit, uint256 adjustment_step, uint256 ma_half_time);
        }
    }
}
use crypto_events::{ICryptoNg, ICryptoTri, ICryptoTwo, ICryptoV1};

/// Crypto-pool topic0s that change the cached state (every variant: the
/// original `CurveCryptoSwap2`, twocrypto-ng, tricrypto-ng). The state is
/// also re-read every block, so a missed topic costs at most that block.
const CRYPTO_STALE_TOPICS: [B256; 16] = [
    ICryptoNg::TokenExchange::SIGNATURE_HASH,
    ICryptoNg::RemoveLiquidityOne::SIGNATURE_HASH,
    ICryptoNg::NewParameters::SIGNATURE_HASH,
    ICryptoNg::RampAgamma::SIGNATURE_HASH,
    ICryptoNg::StopRampA::SIGNATURE_HASH,
    ICryptoNg::ClaimAdminFee::SIGNATURE_HASH,
    ICryptoNg::CommitNewParameters::SIGNATURE_HASH,
    ICryptoTwo::AddLiquidity::SIGNATURE_HASH,
    ICryptoTwo::RemoveLiquidity::SIGNATURE_HASH,
    ICryptoTwo::ClaimAdminFee::SIGNATURE_HASH,
    ICryptoTwo::NewParameters::SIGNATURE_HASH,
    ICryptoTri::AddLiquidity::SIGNATURE_HASH,
    ICryptoTri::RemoveLiquidity::SIGNATURE_HASH,
    ICryptoV1::TokenExchange::SIGNATURE_HASH,
    ICryptoV1::AddLiquidity::SIGNATURE_HASH,
    ICryptoV1::RemoveLiquidityOne::SIGNATURE_HASH,
];

/// Swap venue family. **No `UniV4`** — by construction: a V4 pool is V3 state
/// with its key. Balancer V2 weighted pools are their own venue (swap venue
/// 11), as is Fluid DEX (12).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Venue {
    UniV2,
    UniV3,
    CurveStable,
    CurveCrypto,
    Balancer,
    Fluid,
}

/// Dense pool index into a [`PoolBook`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PoolId(pub u32);

/// One directed swap through one pool: coin `i` in, coin `j` out.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Leg {
    pub pool: PoolId,
    pub i: u8,
    pub j: u8,
}

/// One initialized V3 tick. `gross` decides initialization (bitmap flip
/// at zero); `net` is applied on crossing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Tick {
    pub tick: i32,
    pub net: i128,
    pub gross: u128,
}

/// Uniswap V3 pool state — exactly what `swap()` reads. A Uniswap V4 pool
/// is the same concentrated-liquidity math (v4-core `Pool.swap` runs V3's
/// `TickMath`, `SqrtPriceMath` and `SwapMath`), so it is this state with
/// [`Self::v4`] set: its key, for reads and for the swap leg (venue 9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V3State {
    /// Which V3 factory deployed the pool: [`V3_FACTORY_UNISWAP`] (0),
    /// [`V3_FACTORY_SUSHI`] or [`V3_FACTORY_PANCAKE`], the id a pool-direct
    /// leg names and the Executor derives the pool from. It picks the Swap
    /// event the pool's logs are folded from (Pancake's carries two more
    /// fields). Always 0 for a V4 pool.
    pub factory: u8,
    pub sqrt_price_x96: U256,
    pub tick: i32,
    pub liquidity: u128,
    /// V4: the swap fee, `calculateSwapFee(protocolFee, lpFee)`, the larger
    /// of the two directions' (a V4 protocol fee is set per direction).
    pub fee_pips: u32,
    pub tick_spacing: i32,
    /// Initialized ticks, ascending by `tick`.
    pub ticks: Vec<Tick>,
    /// A Uniswap V4 pool's key; `None` for a V3 pool.
    pub v4: Option<V4Key>,
    /// The inclusive tick range whose initialized ticks are known: what the
    /// seed read. The swap math stops at its edge (`InsufficientLiquidity`)
    /// instead of carrying the last liquidity into ticks it never read.
    /// `None`: the whole tick map is known (a pool followed since creation).
    pub window: Option<(i32, i32)>,
}

/// The tick a V3 step may run to: the next initialized tick, clamped to the
/// known window's edge. Returns `(tick, initialized, at_edge)`; `None` when
/// the price already stands outside the window (nothing beyond is known).
/// [`bounded_next_tick`] for the exact solver.
#[inline]
pub(crate) fn bounded_next_tick_pub(
    s: &V3State,
    tick: i32,
    zfo: bool,
) -> Option<(i32, bool, bool)> {
    bounded_next_tick(s, tick, zfo)
}

#[inline]
fn bounded_next_tick(s: &V3State, tick: i32, zfo: bool) -> Option<(i32, bool, bool)> {
    let (next, initialized) = next_tick_within_word(&s.ticks, tick, s.tick_spacing, zfo);
    let next = next.clamp(tick_math::MIN_TICK, tick_math::MAX_TICK);
    let Some((lo, hi)) = s.window else {
        return Some((next, initialized, false));
    };
    if tick < lo || tick > hi {
        return None;
    }
    if zfo && next < lo {
        return Some((lo, false, true));
    }
    if !zfo && next > hi {
        return Some((hi, false, true));
    }
    Some((next, initialized, false))
}

/// A Uniswap V4 pool's `PoolKey` and id (`keccak256(abi.encode(key))`).
/// `currency0 == 0` is native ETH; the pool's [`Pool::tokens`] name it WETH.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct V4Key {
    pub currency0: Address,
    pub currency1: Address,
    /// The key's `fee` (a static LP fee; a dynamic-fee flag would need a
    /// hook, which the venue refuses unless allowlisted).
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: Address,
    pub id: B256,
}

impl V4Key {
    /// The 66-byte venue-9 leg data: `currency0 ‖ currency1 ‖ fee (3) ‖
    /// tickSpacing (3) ‖ hooks`.
    #[must_use]
    pub fn leg_data(&self) -> Vec<u8> {
        let [_, f0, f1, f2] = self.fee.to_be_bytes();
        let [_, t0, t1, t2] = self.tick_spacing.to_be_bytes();
        let mut d = Vec::with_capacity(66);
        d.extend_from_slice(self.currency0.as_slice());
        d.extend_from_slice(self.currency1.as_slice());
        d.extend_from_slice(&[f0, f1, f2, t0, t1, t2]);
        d.extend_from_slice(self.hooks.as_slice());
        d
    }

    /// The pool id from the key: `keccak256(abi.encode(PoolKey))`. `None`
    /// when the fee or tick spacing does not fit its 24 bits.
    #[must_use]
    pub fn compute_id(&self) -> Option<B256> {
        use alloy_sol_types::SolValue;
        let fee = alloy_primitives::aliases::U24::try_from(self.fee).ok()?;
        let ts = alloy_primitives::aliases::I24::try_from(self.tick_spacing).ok()?;
        Some(alloy_primitives::keccak256(
            (self.currency0, self.currency1, fee, ts, self.hooks).abi_encode(),
        ))
    }

    /// The address a V4 pool has in a [`PoolBook`] (V4 pools share the
    /// PoolManager's): the low 20 bytes of its id.
    #[must_use]
    pub fn book_address(&self) -> Address {
        Self::book_address_of(self.id)
    }

    /// [`Self::book_address`] of a pool id.
    #[must_use]
    pub fn book_address_of(id: B256) -> Address {
        Address::from_word(id)
    }
}

/// `ProtocolFeeLibrary.calculateSwapFee`: the fee a V4 swap charges, in
/// pips, from one direction's protocol fee and the LP fee.
#[must_use]
pub fn v4_swap_fee(protocol_fee: u32, lp_fee: u32) -> u32 {
    let p = u64::from(protocol_fee);
    let l = u64::from(lp_fee);
    p.checked_add(l)
        .and_then(|s| s.checked_sub(p.checked_mul(l)?.checked_div(1_000_000)?))
        .and_then(|f| u32::try_from(f).ok())
        .unwrap_or(u32::MAX)
}

/// Uniswap V2 pair reserves (`uint112` each).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct V2State {
    pub reserve0: U256,
    pub reserve1: U256,
    /// Which factory deployed the pair (`liq_wire` `V2_FACTORY_*`): the
    /// Executor re-derives the pair address from it. Both are 0.30 %.
    pub factory: u8,
}

/// Curve StableSwap: a plain pool (`StableSwap*.vy`, `A_PRECISION` ∈
/// {1, 100}) or a StableSwap-NG pool (`CurveStableSwapNG.vy`, `ng`). Crypto
/// and meta pools are not representable — fail closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurveState {
    /// Raw on-chain `balances(i)` (NG: `stored_balances − admin_balances`,
    /// which is what its `balances(i)` returns).
    pub balances: SmallVec<[U256; MAX_COINS]>,
    /// Plain: `RATES[i]` = `1e18 · 10^(18 − decimals_i)`. NG: `stored_rates()`
    /// at the last read — the precision multiplier times any oracle or
    /// ERC-4626 rate, so it is re-read with the balances.
    pub rates: SmallVec<[U256; MAX_COINS]>,
    /// `A()` as stored (already scaled by `a_precision`).
    pub a: U256,
    /// `A_PRECISION`: `1` (3pool era) or `100`.
    pub a_precision: U256,
    /// `fee()` at `1e10`.
    pub fee: U256,
    /// Set by any pool log; cleared by [`CurveState::reseed`]. A stale pool
    /// is excluded from routing rather than quoted from a guess.
    pub stale: bool,
    /// Block of the newest log that set `stale`. A reseed read at an older
    /// block cannot clear it ([`CurveState::reseed_at`]).
    pub stale_block: u64,
    /// StableSwap-NG math: `get_D` divides `D_P` by `N^N` once, and the fee
    /// is `_dynamic_fee` with [`Self::offpeg_fee_multiplier`].
    pub ng: bool,
    /// `get_D` divides by `N^N` once (NG, and the crvUSD stableswap
    /// factory's plain pools); otherwise by `x·N` per coin.
    pub d_once: bool,
    /// NG `offpeg_fee_multiplier()` (1e10). At or below 1e10 the fee is
    /// flat. 0 on plain pools.
    pub offpeg_fee_multiplier: U256,
    /// NG pool with an oracle or ERC-4626 rate: `stored_rates()` moves
    /// without a pool log, so the reseed thread re-reads it every block.
    pub dynamic_rates: bool,
    /// Block of the last applied read.
    pub read_block: u64,
    /// Index of the Curve MetaRegistry handler that holds the pool
    /// (`get_registry(i)`). The Executor asks that one handler, so every
    /// leg through the pool carries this byte.
    pub handler: u8,
}

impl CurveState {
    /// Replace the cached state from an off-hot-path read at a block.
    pub fn reseed(&mut self, balances: &[U256], a: U256, fee: U256) -> Result<(), RouteError> {
        if balances.len() != self.rates.len() {
            return Err(RouteError::BadLeg);
        }
        self.balances = balances.iter().copied().collect();
        self.a = a;
        self.fee = fee;
        self.stale = false;
        Ok(())
    }

    /// [`CurveState::reseed`] from a read pinned at `block`. Refused (state
    /// untouched, returns `false`) when a pool log newer than `block` made
    /// the pool stale — that read already misses the trade.
    pub fn reseed_at(
        &mut self,
        balances: &[U256],
        a: U256,
        fee: U256,
        block: u64,
    ) -> Result<bool, RouteError> {
        if self.stale && self.stale_block > block {
            return Ok(false);
        }
        self.reseed(balances, a, fee)?;
        self.read_block = block;
        Ok(true)
    }

    /// NG: replace `stored_rates()` and `offpeg_fee_multiplier()` from the
    /// same read as the balances. Refused for a plain pool (its rates are
    /// fixed precision multipliers) or a length mismatch.
    pub fn set_ng_params(&mut self, rates: &[U256], offpeg: U256) -> Result<(), RouteError> {
        if !self.ng || rates.len() != self.rates.len() || rates.iter().any(|r| r.is_zero()) {
            return Err(RouteError::BadLeg);
        }
        self.rates = rates.iter().copied().collect();
        self.offpeg_fee_multiplier = offpeg;
        Ok(())
    }

    /// The fee `exchange` charges for moving `xp[i] → x`, `xp[j] → y`:
    /// flat on plain pools, NG `_dynamic_fee((xp_i + x)/2, (xp_j + y)/2)`.
    fn swap_fee(&self, xpi: U256, xpj: U256) -> Result<U256, RouteError> {
        if !self.ng {
            return Ok(self.fee);
        }
        ng_dynamic_fee(xpi, xpj, self.fee, self.offpeg_fee_multiplier)
    }
}

/// `CurveStableSwapNG._dynamic_fee`: `fee · m / ((m − 1e10)·4·xpi·xpj/(xpi+xpj)² + 1e10)`,
/// or `fee` when `m <= 1e10`.
pub fn ng_dynamic_fee(xpi: U256, xpj: U256, fee: U256, m: U256) -> Result<U256, RouteError> {
    if m <= CURVE_FEE_DENOM {
        return Ok(fee);
    }
    let sum = xpi.checked_add(xpj).ok_or(RouteError::Math)?;
    let xps2 = sum.checked_mul(sum).ok_or(RouteError::Math)?;
    if xps2.is_zero() {
        return Err(RouteError::InsufficientLiquidity);
    }
    let skew = m
        .checked_sub(CURVE_FEE_DENOM)
        .and_then(|v| v.checked_mul(U256::from(4u8)))
        .and_then(|v| v.checked_mul(xpi))
        .and_then(|v| v.checked_mul(xpj))
        .ok_or(RouteError::Math)?
        .checked_div(xps2)
        .ok_or(RouteError::Math)?;
    let den = skew.checked_add(CURVE_FEE_DENOM).ok_or(RouteError::Math)?;
    m.checked_mul(fee)
        .and_then(|v| v.checked_div(den))
        .ok_or(RouteError::Math)
}

/// Venue-specific state. Variants are inline (no `Box`): the exact tier
/// clones pools into scratch for K-collateral ordering and must not
/// allocate per variant there.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PoolState {
    V2(V2State),
    V3(V3State),
    Curve(CurveState),
    Crypto(CryptoState),
    Balancer(BalancerState),
    Fluid(FluidState),
}

/// One routable pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pool {
    pub address: Address,
    /// Coin index → global asset id.
    pub assets: SmallVec<[AssetId; MAX_COINS]>,
    /// Coin index → token contract (for discovery / `Transfer` filters).
    pub tokens: SmallVec<[Address; MAX_COINS]>,
    /// Gas one swap through this pool costs inside the executor
    /// (config, GUIDE 12 §4 "each additional pool costs 80–120k").
    pub hop_gas: u64,
    pub state: PoolState,
}

impl Pool {
    /// The pool's swap math. A Uniswap V4 pool is [`Venue::UniV3`] (V3
    /// state with its key); where a real V3 pool contract is needed (a
    /// flash swap, `token0()`), check [`Pool::is_v4`] too: a V4 pool's
    /// `address` is its id's low 20 bytes, not a contract.
    #[inline]
    #[must_use]
    pub fn venue(&self) -> Venue {
        match self.state {
            PoolState::V2(_) => Venue::UniV2,
            PoolState::V3(_) => Venue::UniV3,
            PoolState::Curve(_) => Venue::CurveStable,
            PoolState::Crypto(_) => Venue::CurveCrypto,
            PoolState::Balancer(_) => Venue::Balancer,
            PoolState::Fluid(_) => Venue::Fluid,
        }
    }

    /// A Uniswap V4 pool (held in the PoolManager, keyed by its id).
    #[inline]
    #[must_use]
    pub fn is_v4(&self) -> bool {
        matches!(&self.state, PoolState::V3(s) if s.v4.is_some())
    }

    /// A Uniswap V3 pool contract at `address`.
    #[inline]
    #[must_use]
    pub fn is_v3_contract(&self) -> bool {
        self.venue() == Venue::UniV3 && !self.is_v4()
    }

    /// Coin index of `asset`, if the pool holds it.
    #[inline]
    #[must_use]
    pub fn coin(&self, asset: AssetId) -> Option<u8> {
        self.assets
            .iter()
            .position(|&a| a == asset)
            .and_then(|p| u8::try_from(p).ok())
    }

    /// `true` when the pool can be quoted at all right now.
    #[inline]
    #[must_use]
    pub fn is_live(&self) -> bool {
        match &self.state {
            PoolState::V2(s) => !s.reserve0.is_zero() && !s.reserve1.is_zero(),
            PoolState::V3(s) => !s.sqrt_price_x96.is_zero(),
            PoolState::Curve(s) => !s.stale && s.balances.iter().all(|b| !b.is_zero()),
            PoolState::Crypto(s) => s.is_live(),
            PoolState::Balancer(s) => s.is_live(),
            PoolState::Fluid(s) => s.is_live(),
        }
    }

    /// Exact output for `amount_in` of coin `i` into coin `j`, as the
    /// contract would compute it against this state. Allocation-free.
    pub fn quote_exact_in(&self, i: u8, j: u8, amount_in: U256) -> Result<U256, RouteError> {
        self.state.quote(i, j, amount_in)
    }

    /// The input of coin `i` this pool absorbs selling into `j` before what
    /// it knows runs out: a V3 (or V4) pool's seeded tick window
    /// ([`v3_capacity_in`]); `None` for a pool with no such bound (V2 and
    /// Curve quote any size from their own reserves).
    #[must_use]
    pub fn capacity_in(&self, i: u8, j: u8) -> Option<U256> {
        match &self.state {
            PoolState::V3(s) => {
                let zfo = i < j;
                v3_capacity_in(s, zfo).ok()
            }
            // A weighted pool refuses past 30 % of its input balance.
            PoolState::Balancer(s) => s.capacity_in(i),
            // Half the imaginary reserves of the side taking the input.
            PoolState::Fluid(s) => s.capacity_in(i),
            _ => None,
        }
    }

    /// Exact output, **mutating** the state the way the swap would
    /// (GUIDE 12 §4c: subsequent legs see the displaced state).
    pub fn apply_exact_in(&mut self, i: u8, j: u8, amount_in: U256) -> Result<U256, RouteError> {
        self.state.apply(i, j, amount_in)
    }

    /// `ρ(0)`: `sqrt(marginal out-per-in at zero size)` in Q96. The
    /// ranking key for the greedy pool set (GUIDE 12 §4) and the start of
    /// the water-fill.
    pub fn rho_at_zero(&self, i: u8, j: u8) -> Result<U256, RouteError> {
        match &self.state {
            PoolState::V3(s) => {
                let zfo = zero_for_one(i, j)?;
                v3_rho(s.sqrt_price_x96, s.fee_pips, zfo)
            }
            PoolState::V2(s) => {
                let zfo = zero_for_one(i, j)?;
                let (rin, rout) = if zfo {
                    (s.reserve0, s.reserve1)
                } else {
                    (s.reserve1, s.reserve0)
                };
                // ρ² = 0.997 · rout / rin (raw units) → Q96 sqrt.
                let num = U512::from(rout)
                    .checked_mul(U512::from(Q192))
                    .and_then(|v| v.checked_mul(U512::from(997u64)))
                    .ok_or(RouteError::Math)?;
                let den = U512::from(rin)
                    .checked_mul(U512::from(1000u64))
                    .ok_or(RouteError::Math)?;
                if den.is_zero() {
                    return Err(RouteError::InsufficientLiquidity);
                }
                let q = num.checked_div(den).ok_or(RouteError::Math)?;
                narrow(q.root(2))
            }
            PoolState::Curve(s) => curve_rho(s, i, j, U256::ZERO),
            PoolState::Crypto(s) => s.rho(i, j, U256::ZERO),
            PoolState::Balancer(s) => s.rho(i, j, U256::ZERO),
            PoolState::Fluid(s) => s.rho(i, j, U256::ZERO),
        }
    }
}

impl PoolState {
    fn quote(&self, i: u8, j: u8, amount_in: U256) -> Result<U256, RouteError> {
        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }
        match self {
            PoolState::V3(s) => v3_swap(s, zero_for_one(i, j)?, amount_in).map(|r| r.out),
            PoolState::V2(s) => v2_swap(s, zero_for_one(i, j)?, amount_in).map(|r| r.0),
            PoolState::Curve(s) => curve_exchange(s, i, j, amount_in).map(|r| r.0),
            PoolState::Crypto(s) => s.dy(i, j, amount_in),
            PoolState::Balancer(s) => s.dy(i, j, amount_in),
            PoolState::Fluid(s) => s.dy(i, j, amount_in),
        }
    }

    fn apply(&mut self, i: u8, j: u8, amount_in: U256) -> Result<U256, RouteError> {
        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }
        match self {
            PoolState::V3(s) => {
                let r = v3_swap(s, zero_for_one(i, j)?, amount_in)?;
                s.sqrt_price_x96 = r.sqrt_price_x96;
                s.tick = r.tick;
                s.liquidity = r.liquidity;
                Ok(r.out)
            }
            PoolState::V2(s) => {
                let (out, next) = v2_swap(s, zero_for_one(i, j)?, amount_in)?;
                *s = next;
                Ok(out)
            }
            PoolState::Curve(s) => {
                let (out, bi, bj) = curve_exchange(s, i, j, amount_in)?;
                *s.balances
                    .get_mut(usize::from(i))
                    .ok_or(RouteError::BadLeg)? = bi;
                *s.balances
                    .get_mut(usize::from(j))
                    .ok_or(RouteError::BadLeg)? = bj;
                Ok(out)
            }
            // Followed exactly for the 2025 Twocrypto pools; any other goes
            // stale, its `D` and `price_scale` after the swap unknown.
            PoolState::Crypto(s) => s.apply(i, j, amount_in),
            // The Vault moves the pool's balances by exactly the swap.
            PoolState::Balancer(s) => s.apply(i, j, amount_in),
            // Both Liquidity positions and the pool's price variables move.
            PoolState::Fluid(s) => s.apply(i, j, amount_in),
        }
    }
}

/// Post-swap V3 slot0 fields plus the output.
#[derive(Copy, Clone, Debug)]
pub(crate) struct V3SwapResult {
    pub out: U256,
    pub sqrt_price_x96: U256,
    pub tick: i32,
    pub liquidity: u128,
}

#[inline]
fn zero_for_one(i: u8, j: u8) -> Result<bool, RouteError> {
    match (i, j) {
        (0, 1) => Ok(true),
        (1, 0) => Ok(false),
        _ => Err(RouteError::BadLeg),
    }
}

// ───────────────────────────── Uniswap V3 ─────────────────────────────

/// `TickBitmap.nextInitializedTickWithinOneWord` over a sorted tick list.
/// Returns `(next_tick, initialized)`; a non-initialized word edge is a
/// real stop in the contract's loop and therefore in ours (rounding is
/// per step).
#[must_use]
pub fn next_tick_within_word(ticks: &[Tick], tick: i32, spacing: i32, zfo: bool) -> (i32, bool) {
    if spacing <= 0 {
        return (tick, false);
    }
    let compressed = tick.div_euclid(spacing);
    if zfo {
        let bit = compressed.rem_euclid(256);
        let word_floor = compressed.saturating_sub(bit).saturating_mul(spacing);
        // Largest initialized tick with compressed index ≤ compressed.
        let bound = compressed.saturating_mul(spacing);
        let pos = ticks.partition_point(|t| t.tick <= bound);
        match pos.checked_sub(1).and_then(|p| ticks.get(p)) {
            Some(t) if t.tick >= word_floor => (t.tick, true),
            _ => (word_floor, false),
        }
    } else {
        let next = compressed.saturating_add(1);
        let bit = next.rem_euclid(256);
        let word_ceil = next
            .saturating_add(255i32.saturating_sub(bit))
            .saturating_mul(spacing);
        let bound = next.saturating_mul(spacing);
        let pos = ticks.partition_point(|t| t.tick < bound);
        match ticks.get(pos) {
            Some(t) if t.tick <= word_ceil => (t.tick, true),
            _ => (word_ceil, false),
        }
    }
}

/// `UniswapV3Pool.swap` exact-input loop against `s`. Pure: the caller
/// decides whether to commit the returned slot0.
pub(crate) fn v3_swap(s: &V3State, zfo: bool, amount_in: U256) -> Result<V3SwapResult, RouteError> {
    if s.sqrt_price_x96.is_zero() {
        return Err(RouteError::StalePool);
    }
    let limit = if zfo {
        tick_math::MIN_SQRT_RATIO
            .checked_add(U256::ONE)
            .ok_or(RouteError::Math)?
    } else {
        tick_math::MAX_SQRT_RATIO
            .checked_sub(U256::ONE)
            .ok_or(RouteError::Math)?
    };
    let mut remaining = I256::try_from(amount_in).map_err(|_| RouteError::Math)?;
    let mut out = U256::ZERO;
    let mut sqrt_p = s.sqrt_price_x96;
    let mut tick = s.tick;
    let mut liq = s.liquidity;
    while !remaining.is_zero() && sqrt_p != limit {
        let Some((next_tick, initialized, at_edge)) = bounded_next_tick(s, tick, zfo) else {
            return Err(RouteError::InsufficientLiquidity);
        };
        let sqrt_next =
            tick_math::get_sqrt_ratio_at_tick(next_tick).map_err(|_| RouteError::Math)?;
        let target = if (zfo && sqrt_next < limit) || (!zfo && sqrt_next > limit) {
            limit
        } else {
            sqrt_next
        };
        let step_start = sqrt_p;
        let (sqrt_after, step_in, step_out, fee) =
            swap_math::compute_swap_step(sqrt_p, target, liq, remaining, s.fee_pips)
                .map_err(|_| RouteError::Math)?;
        let consumed = step_in.checked_add(fee).ok_or(RouteError::Math)?;
        let consumed = I256::try_from(consumed).map_err(|_| RouteError::Math)?;
        remaining = remaining.checked_sub(consumed).ok_or(RouteError::Math)?;
        out = out.checked_add(step_out).ok_or(RouteError::Math)?;
        sqrt_p = sqrt_after;
        // At the known window's edge with input left: what lies beyond was
        // never read.
        if at_edge && sqrt_p == sqrt_next && !remaining.is_zero() {
            return Err(RouteError::InsufficientLiquidity);
        }
        if sqrt_p == sqrt_next {
            if initialized {
                let pos = s.ticks.partition_point(|t| t.tick < next_tick);
                let net = s.ticks.get(pos).map_or(0, |t| t.net);
                let net = if zfo {
                    net.checked_neg().ok_or(RouteError::Math)?
                } else {
                    net
                };
                liq = liquidity_math::add_delta(liq, net).map_err(|_| RouteError::Math)?;
            }
            tick = if zfo {
                next_tick.checked_sub(1).ok_or(RouteError::Math)?
            } else {
                next_tick
            };
        } else if sqrt_p != step_start {
            tick = tick_math::get_tick_at_sqrt_ratio(sqrt_p).map_err(|_| RouteError::Math)?;
        }
    }
    if !remaining.is_zero() {
        // Price limit reached with input left: the pool cannot absorb it.
        return Err(RouteError::InsufficientLiquidity);
    }
    Ok(V3SwapResult {
        out,
        sqrt_price_x96: sqrt_p,
        tick,
        liquidity: liq,
    })
}

/// The input a V3 pool absorbs before its known ticks run out (the seeded
/// window's edge) or its price limit: every step of [`v3_swap`] with
/// unbounded input, summed. A pool whose price already stands outside its
/// window absorbs nothing.
pub(crate) fn v3_capacity_in(s: &V3State, zfo: bool) -> Result<U256, RouteError> {
    if s.sqrt_price_x96.is_zero() {
        return Err(RouteError::StalePool);
    }
    let limit = if zfo {
        tick_math::MIN_SQRT_RATIO
            .checked_add(U256::ONE)
            .ok_or(RouteError::Math)?
    } else {
        tick_math::MAX_SQRT_RATIO
            .checked_sub(U256::ONE)
            .ok_or(RouteError::Math)?
    };
    let unbounded = I256::MAX;
    let mut consumed = U256::ZERO;
    let mut sqrt_p = s.sqrt_price_x96;
    let mut tick = s.tick;
    let mut liq = s.liquidity;
    while sqrt_p != limit {
        let Some((next_tick, initialized, at_edge)) = bounded_next_tick(s, tick, zfo) else {
            return Ok(U256::ZERO);
        };
        let sqrt_next =
            tick_math::get_sqrt_ratio_at_tick(next_tick).map_err(|_| RouteError::Math)?;
        let target = if (zfo && sqrt_next < limit) || (!zfo && sqrt_next > limit) {
            limit
        } else {
            sqrt_next
        };
        let (sqrt_after, step_in, _, fee) =
            swap_math::compute_swap_step(sqrt_p, target, liq, unbounded, s.fee_pips)
                .map_err(|_| RouteError::Math)?;
        consumed = consumed
            .checked_add(step_in)
            .and_then(|c| c.checked_add(fee))
            .ok_or(RouteError::Math)?;
        sqrt_p = sqrt_after;
        if at_edge && sqrt_p == sqrt_next {
            break;
        }
        if sqrt_p == sqrt_next {
            if initialized {
                let pos = s.ticks.partition_point(|t| t.tick < next_tick);
                let net = s.ticks.get(pos).map_or(0, |t| t.net);
                let net = if zfo {
                    net.checked_neg().ok_or(RouteError::Math)?
                } else {
                    net
                };
                liq = liquidity_math::add_delta(liq, net).map_err(|_| RouteError::Math)?;
            }
            tick = if zfo {
                next_tick.checked_sub(1).ok_or(RouteError::Math)?
            } else {
                next_tick
            };
        } else {
            break;
        }
    }
    Ok(consumed)
}

/// `ρ = sqrt((1 − f) · out/in)` in Q96 for a V3 price `sqrt_p`:
/// zero-for-one `out/in = P`, one-for-zero `out/in = 1/P`.
pub(crate) fn v3_rho(sqrt_p: U256, fee_pips: u32, zfo: bool) -> Result<U256, RouteError> {
    let r = if zfo {
        sqrt_p
    } else {
        // sqrt(1/P) in Q96 = 2^96 / (s / 2^96) = 2^192 / s, floor.
        Q192.checked_div(sqrt_p).ok_or(RouteError::Math)?
    };
    let keep = PIPS.checked_sub(fee_pips).ok_or(RouteError::Math)?;
    // ρ = r · sqrt(keep / 1e6) = sqrt(r² · keep / 1e6).
    let sq = U512::from(r)
        .checked_mul(U512::from(r))
        .and_then(|v| v.checked_mul(U512::from(keep)))
        .and_then(|v| v.checked_div(U512::from(PIPS)))
        .ok_or(RouteError::Math)?;
    narrow(sq.root(2))
}

// ───────────────────────────── Uniswap V2 ─────────────────────────────

/// `UniswapV2Library.getAmountOut`, plus the reserves `_update` leaves.
pub(crate) fn v2_swap(
    s: &V2State,
    zfo: bool,
    amount_in: U256,
) -> Result<(U256, V2State), RouteError> {
    let (rin, rout) = if zfo {
        (s.reserve0, s.reserve1)
    } else {
        (s.reserve1, s.reserve0)
    };
    if rin.is_zero() || rout.is_zero() {
        return Err(RouteError::InsufficientLiquidity);
    }
    let in_fee = amount_in
        .checked_mul(U256::from(997u64))
        .ok_or(RouteError::Math)?;
    let num = in_fee.checked_mul(rout).ok_or(RouteError::Math)?;
    let den = rin
        .checked_mul(U256::from(1000u64))
        .and_then(|v| v.checked_add(in_fee))
        .ok_or(RouteError::Math)?;
    let out = num.checked_div(den).ok_or(RouteError::Math)?;
    if out >= rout {
        return Err(RouteError::InsufficientLiquidity);
    }
    let new_in = rin.checked_add(amount_in).ok_or(RouteError::Math)?;
    let new_out = rout.checked_sub(out).ok_or(RouteError::Math)?;
    let next = if zfo {
        V2State {
            reserve0: new_in,
            reserve1: new_out,
            ..*s
        }
    } else {
        V2State {
            reserve0: new_out,
            reserve1: new_in,
            ..*s
        }
    };
    Ok((out, next))
}

// ───────────────────────────── Curve StableSwap ─────────────────────────────

fn n_coins(s: &CurveState) -> Result<U256, RouteError> {
    let n = s.balances.len();
    if !(2..=MAX_COINS).contains(&n) || s.rates.len() != n {
        return Err(RouteError::BadLeg);
    }
    Ok(U256::from(n))
}

/// `_xp()`: `balances[i] · rates[i] / 1e18`.
fn curve_xp(s: &CurveState) -> Result<SmallVec<[U256; MAX_COINS]>, RouteError> {
    s.balances
        .iter()
        .zip(&s.rates)
        .map(|(&b, &r)| {
            b.checked_mul(r)
                .and_then(|v| v.checked_div(WAD))
                .ok_or(RouteError::Math)
        })
        .collect()
}

/// `Ann = A · N_COINS` (A already carries `A_PRECISION`).
fn curve_ann(s: &CurveState, n: U256) -> Result<U256, RouteError> {
    s.a.checked_mul(n).ok_or(RouteError::Math)
}

/// `get_D(xp, amp)`: Curve's Newton on the invariant, its own stopping
/// rule (`|D − Dprev| ≤ 1`), 255-iteration cap → error, as the contract
/// would revert (newer pools) or return garbage (3pool — we refuse).
pub fn curve_get_d(s: &CurveState, xp: &[U256]) -> Result<U256, RouteError> {
    let n = n_coins(s)?;
    let ap = s.a_precision;
    let mut sum = U256::ZERO;
    for &x in xp {
        sum = sum.checked_add(x).ok_or(RouteError::Math)?;
    }
    if sum.is_zero() {
        return Ok(U256::ZERO);
    }
    let ann = curve_ann(s, n)?;
    let mut d = sum;
    let n_plus_1 = n.checked_add(U256::ONE).ok_or(RouteError::Math)?;
    // NG divides by `N^N` once after the product; plain pools by `x·N`
    // per coin. The integer results differ, so each follows its pool.
    let n_pow_n = n.checked_pow(n).ok_or(RouteError::Math)?;
    for _ in 0..255 {
        let mut d_p = d;
        for &x in xp {
            let den = if s.d_once {
                x
            } else {
                x.checked_mul(n).ok_or(RouteError::Math)?
            };
            if den.is_zero() {
                return Err(RouteError::InsufficientLiquidity);
            }
            d_p = d_p
                .checked_mul(d)
                .and_then(|v| v.checked_div(den))
                .ok_or(RouteError::Math)?;
        }
        if s.d_once {
            d_p = d_p.checked_div(n_pow_n).ok_or(RouteError::Math)?;
        }
        let d_prev = d;
        // D = (Ann·S/AP + D_P·n) · D / ((Ann − AP)·D/AP + (n+1)·D_P)
        let t1 = ann
            .checked_mul(sum)
            .and_then(|v| v.checked_div(ap))
            .and_then(|v| v.checked_add(d_p.checked_mul(n)?))
            .and_then(|v| v.checked_mul(d))
            .ok_or(RouteError::Math)?;
        let t2 = ann
            .checked_sub(ap)
            .and_then(|v| v.checked_mul(d))
            .and_then(|v| v.checked_div(ap))
            .and_then(|v| v.checked_add(n_plus_1.checked_mul(d_p)?))
            .ok_or(RouteError::Math)?;
        d = t1.checked_div(t2).ok_or(RouteError::Math)?;
        if d.abs_diff(d_prev) <= U256::ONE {
            return Ok(d);
        }
    }
    Err(RouteError::Math)
}

/// `get_y(i, j, x, xp)`: Newton for the `j` balance given the new `i`.
pub fn curve_get_y(
    s: &CurveState,
    i: usize,
    j: usize,
    x: U256,
    xp: &[U256],
) -> Result<U256, RouteError> {
    let n = n_coins(s)?;
    let ap = s.a_precision;
    let d = curve_get_d(s, xp)?;
    let ann = curve_ann(s, n)?;
    let mut c = d;
    let mut s_ = U256::ZERO;
    for (k, &xk) in xp.iter().enumerate() {
        let xk = if k == i {
            x
        } else if k != j {
            xk
        } else {
            continue;
        };
        s_ = s_.checked_add(xk).ok_or(RouteError::Math)?;
        let den = xk.checked_mul(n).ok_or(RouteError::Math)?;
        if den.is_zero() {
            return Err(RouteError::InsufficientLiquidity);
        }
        c = c
            .checked_mul(d)
            .and_then(|v| v.checked_div(den))
            .ok_or(RouteError::Math)?;
    }
    // c = c · D · AP / (Ann · n); b = S + D · AP / Ann
    let ann_n = ann.checked_mul(n).ok_or(RouteError::Math)?;
    c = c
        .checked_mul(d)
        .and_then(|v| v.checked_mul(ap))
        .and_then(|v| v.checked_div(ann_n))
        .ok_or(RouteError::Math)?;
    let b = d
        .checked_mul(ap)
        .and_then(|v| v.checked_div(ann))
        .and_then(|v| v.checked_add(s_))
        .ok_or(RouteError::Math)?;
    let mut y = d;
    for _ in 0..255 {
        let y_prev = y;
        // y = (y² + c) / (2y + b − D)
        let num = y
            .checked_mul(y)
            .and_then(|v| v.checked_add(c))
            .ok_or(RouteError::Math)?;
        let den = y
            .checked_mul(U256::from(2u64))
            .and_then(|v| v.checked_add(b))
            .and_then(|v| v.checked_sub(d))
            .ok_or(RouteError::Math)?;
        y = num.checked_div(den).ok_or(RouteError::Math)?;
        if y.abs_diff(y_prev) <= U256::ONE {
            return Ok(y);
        }
    }
    Err(RouteError::Math)
}

/// StableSwap-NG `get_y_D(A, i, xp, D)`: Newton for `xp[i]` when the pool's
/// invariant is `d` (the other balances fixed).
fn curve_get_y_d(s: &CurveState, i: usize, xp: &[U256], d: U256) -> Result<U256, RouteError> {
    let n = n_coins(s)?;
    let ap = s.a_precision;
    let ann = curve_ann(s, n)?;
    let mut c = d;
    let mut s_ = U256::ZERO;
    for (k, &xk) in xp.iter().enumerate() {
        if k == i {
            continue;
        }
        s_ = s_.checked_add(xk).ok_or(RouteError::Math)?;
        let den = xk.checked_mul(n).ok_or(RouteError::Math)?;
        if den.is_zero() {
            return Err(RouteError::InsufficientLiquidity);
        }
        c = c
            .checked_mul(d)
            .and_then(|v| v.checked_div(den))
            .ok_or(RouteError::Math)?;
    }
    let ann_n = ann.checked_mul(n).ok_or(RouteError::Math)?;
    c = c
        .checked_mul(d)
        .and_then(|v| v.checked_mul(ap))
        .and_then(|v| v.checked_div(ann_n))
        .ok_or(RouteError::Math)?;
    let b = d
        .checked_mul(ap)
        .and_then(|v| v.checked_div(ann))
        .and_then(|v| v.checked_add(s_))
        .ok_or(RouteError::Math)?;
    let mut y = d;
    for _ in 0..255 {
        let y_prev = y;
        let num = y
            .checked_mul(y)
            .and_then(|v| v.checked_add(c))
            .ok_or(RouteError::Math)?;
        let den = y
            .checked_mul(U256::from(2u64))
            .and_then(|v| v.checked_add(b))
            .and_then(|v| v.checked_sub(d))
            .ok_or(RouteError::Math)?;
        y = num.checked_div(den).ok_or(RouteError::Math)?;
        if y.abs_diff(y_prev) <= U256::ONE {
            return Ok(y);
        }
    }
    Err(RouteError::Math)
}

/// StableSwap-NG `_calc_withdraw_one_coin(burn, i).dy`: coin `i` (raw
/// units) paid by `remove_liquidity_one_coin(burn, i, …)` when the LP
/// supply is `total_supply` (`CurveStableSwapNG.vy` v7.0.0). Checked where
/// the pool's arithmetic reverts, `unsafe_*` where it does not check.
pub fn ng_withdraw_one_coin(
    s: &CurveState,
    i: u8,
    burn: U256,
    total_supply: U256,
) -> Result<U256, RouteError> {
    if s.stale {
        return Err(RouteError::StalePool);
    }
    if !s.ng {
        return Err(RouteError::BadLeg);
    }
    let n = n_coins(s)?;
    let n_us = s.balances.len();
    let i = usize::from(i);
    if i >= n_us {
        return Err(RouteError::BadLeg);
    }
    if burn.is_zero() || total_supply.is_zero() {
        return Err(RouteError::InsufficientLiquidity);
    }
    let xp = curve_xp(s)?;
    let d0 = curve_get_d(s, &xp)?;
    if d0.is_zero() {
        return Err(RouteError::InsufficientLiquidity);
    }
    let d1 = burn
        .checked_mul(d0)
        .and_then(|v| v.checked_div(total_supply))
        .and_then(|v| d0.checked_sub(v))
        .ok_or(RouteError::Math)?;
    let new_y = curve_get_y_d(s, i, &xp, d1)?;
    // base_fee = fee · N / (4 · (N − 1)); ys = (D0 + D1) / (2N)
    let base_fee = s
        .fee
        .checked_mul(n)
        .and_then(|v| v.checked_div(U256::from(4u8).checked_mul(n.checked_sub(U256::ONE)?)?))
        .ok_or(RouteError::Math)?;
    let ys = d0
        .checked_add(d1)
        .and_then(|v| v.checked_div(U256::from(2u8).checked_mul(n)?))
        .ok_or(RouteError::Math)?;
    let mut xp_reduced = xp.clone();
    for (j, (&xp_j, red)) in xp.iter().zip(xp_reduced.iter_mut()).enumerate() {
        let scaled = xp_j
            .checked_mul(d1)
            .and_then(|v| v.checked_div(d0))
            .ok_or(RouteError::Math)?;
        let (dx_expected, xavg) = if j == i {
            (
                scaled.checked_sub(new_y).ok_or(RouteError::Math)?,
                xp_j.checked_add(new_y)
                    .and_then(|v| v.checked_div(U256::from(2u8)))
                    .ok_or(RouteError::Math)?,
            )
        } else {
            (xp_j.checked_sub(scaled).ok_or(RouteError::Math)?, xp_j)
        };
        let fee = ng_dynamic_fee(xavg, ys, base_fee, s.offpeg_fee_multiplier)?;
        let cut = fee
            .checked_mul(dx_expected)
            .and_then(|v| v.checked_div(CURVE_FEE_DENOM))
            .ok_or(RouteError::Math)?;
        *red = xp_j.checked_sub(cut).ok_or(RouteError::Math)?;
    }
    let y = curve_get_y_d(s, i, &xp_reduced, d1)?;
    let dy = xp_reduced
        .get(i)
        .copied()
        .ok_or(RouteError::BadLeg)?
        .checked_sub(y)
        .and_then(|v| v.checked_sub(U256::ONE))
        .ok_or(RouteError::InsufficientLiquidity)?;
    let rate = s.rates.get(i).copied().ok_or(RouteError::BadLeg)?;
    dy.checked_mul(WAD)
        .and_then(|v| v.checked_div(rate))
        .ok_or(RouteError::Math)
}

/// Output of `exchange(i, j, dx)` — **not** `get_dy`: the two round
/// differently for non-18-decimal coins (`get_dy` scales to raw before the
/// fee, `exchange` takes the fee in `xp` units then scales). Execution
/// settles through `exchange`, so that is the wei-exact path. Returns
/// `(out_raw, dy_before_fee_raw, y)`.
fn curve_dy(
    s: &CurveState,
    i: usize,
    j: usize,
    dx: U256,
) -> Result<(U256, U256, U256), RouteError> {
    if s.stale {
        return Err(RouteError::StalePool);
    }
    let xp = curve_xp(s)?;
    let (&xi, &xj) = (
        xp.get(i).ok_or(RouteError::BadLeg)?,
        xp.get(j).ok_or(RouteError::BadLeg)?,
    );
    let (&ri, &rj) = (
        s.rates.get(i).ok_or(RouteError::BadLeg)?,
        s.rates.get(j).ok_or(RouteError::BadLeg)?,
    );
    let x = dx
        .checked_mul(ri)
        .and_then(|v| v.checked_div(WAD))
        .and_then(|v| v.checked_add(xi))
        .ok_or(RouteError::Math)?;
    let y = curve_get_y(s, i, j, x, &xp)?;
    // dy = xp[j] − y − 1 ; dy_fee = dy · fee / 1e10 ; out = (dy − dy_fee) · 1e18 / rates[j]
    let dy_xp = xj
        .checked_sub(y)
        .and_then(|v| v.checked_sub(U256::ONE))
        .ok_or(RouteError::InsufficientLiquidity)?;
    let half = |a: U256, b: U256| {
        a.checked_add(b)
            .and_then(|v| v.checked_div(U256::from(2u8)))
            .ok_or(RouteError::Math)
    };
    let fee = s.swap_fee(half(xi, x)?, half(xj, y)?)?;
    let fee_xp = dy_xp
        .checked_mul(fee)
        .and_then(|v| v.checked_div(CURVE_FEE_DENOM))
        .ok_or(RouteError::Math)?;
    let to_raw = |v: U256| {
        v.checked_mul(WAD)
            .and_then(|v| v.checked_div(rj))
            .ok_or(RouteError::Math)
    };
    let out = to_raw(dy_xp.checked_sub(fee_xp).ok_or(RouteError::Math)?)?;
    let dy_raw = to_raw(dy_xp)?;
    Ok((out, dy_raw, y))
}

/// `exchange(i, j, dx)`: quote, then the balance update the contract
/// makes. The admin-fee split of the fee is `admin_fee · fee / 1e10`;
/// plain pools we model all use `admin_fee = 50 %` **but that is a pool
/// parameter**, so we take the conservative side for our own displaced
/// state: the full fee leaves the pool (balances[j] −= dy_before_fee).
/// This understates the pool's post-swap depth for the *next* leg of a
/// sequential batch by at most `fee/2` of one leg's output — a
/// second-order, conservative error, bounded and documented.
///
/// Returns `(out, new_balance_i, new_balance_j)`.
fn curve_exchange(
    s: &CurveState,
    i: u8,
    j: u8,
    dx: U256,
) -> Result<(U256, U256, U256), RouteError> {
    let (i, j) = (usize::from(i), usize::from(j));
    if i == j {
        return Err(RouteError::BadLeg);
    }
    let (out, dy, _) = curve_dy(s, i, j, dx)?;
    let bi = s
        .balances
        .get(i)
        .ok_or(RouteError::BadLeg)?
        .checked_add(dx)
        .ok_or(RouteError::Math)?;
    let bj = s
        .balances
        .get(j)
        .ok_or(RouteError::BadLeg)?
        .checked_sub(dy)
        .ok_or(RouteError::InsufficientLiquidity)?;
    Ok((out, bi, bj))
}

/// `ρ` for a Curve pool after `dx` of coin `i` has been absorbed:
/// `sqrt((1 − fee) · |dy/dx|)` in Q96 raw units, from implicit
/// differentiation of the invariant
/// `Ann'·S + D = Ann'·D + D^{n+1}/(n^n·Πx)`:
/// `|dy/dx| = (Ann' + K/x_i) / (Ann' + K/x_j)`, `K = D^{n+1}/(n^n·Πx)`.
pub(crate) fn curve_rho(s: &CurveState, i: u8, j: u8, dx: U256) -> Result<U256, RouteError> {
    let (i, j) = (usize::from(i), usize::from(j));
    if i == j || s.stale {
        return Err(if i == j {
            RouteError::BadLeg
        } else {
            RouteError::StalePool
        });
    }
    let n = n_coins(s)?;
    let ap = s.a_precision;
    let mut xp = curve_xp(s)?;
    let (&ri, &rj) = (
        s.rates.get(i).ok_or(RouteError::BadLeg)?,
        s.rates.get(j).ok_or(RouteError::BadLeg)?,
    );
    if !dx.is_zero() {
        let xi = *xp.get(i).ok_or(RouteError::BadLeg)?;
        let xi_new = dx
            .checked_mul(ri)
            .and_then(|v| v.checked_div(WAD))
            .and_then(|v| v.checked_add(xi))
            .ok_or(RouteError::Math)?;
        let y = curve_get_y(s, i, j, xi_new, &xp)?;
        *xp.get_mut(i).ok_or(RouteError::BadLeg)? = xi_new;
        *xp.get_mut(j).ok_or(RouteError::BadLeg)? = y;
    }
    let d = curve_get_d(s, &xp)?;
    // ann' scaled by 1e18: Ann · 1e18 / AP.
    let ann18 = curve_ann(s, n)?
        .checked_mul(WAD)
        .and_then(|v| v.checked_div(ap))
        .ok_or(RouteError::Math)?;
    // k18 = 1e18 · D^{n+1} / (n^n · Πx), built as a product of ratios.
    let mut k18 = d.checked_mul(WAD).ok_or(RouteError::Math)?;
    for &x in &xp {
        let den = x.checked_mul(n).ok_or(RouteError::Math)?;
        if den.is_zero() {
            return Err(RouteError::InsufficientLiquidity);
        }
        k18 = mul_div_512(k18, d, den)?;
    }
    let term = |x: U256| -> Result<U256, RouteError> {
        if x.is_zero() {
            return Err(RouteError::InsufficientLiquidity);
        }
        k18.checked_div(x)
            .and_then(|v| v.checked_add(ann18))
            .ok_or(RouteError::Math)
    };
    let (&xi, &xj) = (
        xp.get(i).ok_or(RouteError::BadLeg)?,
        xp.get(j).ok_or(RouteError::BadLeg)?,
    );
    let (ti, tj) = (term(xi)?, term(xj)?);
    // m = (ti/tj) · (ri/rj) · (1e10 − fee)/1e10 ; ρ = sqrt(m · 2^192). NG's
    // fee for an infinitesimal trade here is the dynamic fee at this point.
    let fee = s.swap_fee(xi, xj)?;
    let keep = CURVE_FEE_DENOM.checked_sub(fee).ok_or(RouteError::Math)?;
    let num = U512::from(ti)
        .checked_mul(U512::from(ri))
        .and_then(|v| v.checked_mul(U512::from(keep)))
        .and_then(|v| v.checked_mul(U512::from(Q192)))
        .ok_or(RouteError::Math)?;
    let den = U512::from(tj)
        .checked_mul(U512::from(rj))
        .and_then(|v| v.checked_mul(U512::from(CURVE_FEE_DENOM)))
        .ok_or(RouteError::Math)?;
    if den.is_zero() {
        return Err(RouteError::Math);
    }
    let q = num.checked_div(den).ok_or(RouteError::Math)?;
    narrow(q.root(2))
}

/// `U512 → U256`, error when the high limbs are set.
#[inline]
pub(crate) fn narrow(q: U512) -> Result<U256, RouteError> {
    U256::checked_from_limbs_slice(q.as_limbs()).ok_or(RouteError::Math)
}

/// `a · b / d` through 512 bits, floor. Shared by the solver.
pub(crate) fn mul_div_512(a: U256, b: U256, d: U256) -> Result<U256, RouteError> {
    if d.is_zero() {
        return Err(RouteError::Math);
    }
    let p = U512::from(a)
        .checked_mul(U512::from(b))
        .ok_or(RouteError::Math)?;
    narrow(p.checked_div(U512::from(d)).ok_or(RouteError::Math)?)
}

// ───────────────────────────── PoolBook ─────────────────────────────

/// What a wrapper token unwraps into.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnwrapKind {
    /// ERC-4626 `redeem(shares, self, self)` into `asset()`.
    Erc4626,
    /// Expired Pendle PT: `redeemPY` on its YT for SY, then `SY.redeem`
    /// into the unwrapped token.
    PendlePt { yt: Address, sy: Address },
    /// Curve StableSwap-NG LP (the pool is its own LP token):
    /// `remove_liquidity_one_coin` into coin `i`. The pool is in the book;
    /// its state there is what the withdrawal is quoted on.
    CurveLp { i: u8 },
    /// Live Pendle PT: sold on its market (`swapExactPtForSy`), then
    /// `SY.redeem` into the unwrapped token.
    PendleMarket {
        market: Address,
        yt: Address,
        sy: Address,
    },
}

/// What an unwrap pays, as last read.
// One per wrapper, replaced every block: a boxed snapshot would allocate on
// each read, the size difference costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UnwrapRate {
    /// Never read: not routed.
    Unread,
    /// `into` paid for [`Unwrap::scale`] wrapper units (ERC-4626 vaults,
    /// expired PTs): linear in size (discovery checks it), up to `max_into`
    /// when the vault pays only from its cash ([`Unwrap::cash_capped`]).
    Linear {
        assets_per_scale: U256,
        max_into: Option<U256>,
    },
    /// Curve LP: the LP's total supply; the pool's balances are the book's.
    CurveLp { total_supply: U256 },
    /// Live Pendle PT: the market and SY as read ([`crate::pendle`]).
    Pendle(crate::pendle::MarketSnapshot),
}

/// A collateral the Executor unwraps before selling: the exit for `wrapper`
/// is `unwrap → into` and then the pools of `into`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unwrap {
    pub kind: UnwrapKind,
    pub wrapper: AssetId,
    pub wrapper_token: Address,
    pub into: AssetId,
    pub into_token: Address,
    /// The last read, at [`Self::read_block`].
    pub rate: UnwrapRate,
    /// Wrapper units a linear rate is read at (large, so it is precise);
    /// also the step of the marginal a curve's `ρ` is taken over.
    pub scale: U256,
    /// Block of the last rate read; 0 = never read (not routed).
    pub read_block: u64,
    /// Gas of the unwrap step inside the Executor.
    pub gas: u64,
    /// Live Pendle PT only: the gas of the post-expiry redeem (venue 6) it
    /// switches to at expiry ([`PoolBook::expire_pendle_market`]); 0 else.
    pub expiry_gas: u64,
    /// ERC-4626 only: the vault pays at most its `cash()`, read with the
    /// rate into [`UnwrapRate::Linear::max_into`].
    pub cash_capped: bool,
}

impl Unwrap {
    /// Read at least once, with a usable rate.
    #[must_use]
    pub fn is_live(&self) -> bool {
        if self.read_block == 0 || self.scale.is_zero() {
            return false;
        }
        match self.rate {
            UnwrapRate::Unread => false,
            UnwrapRate::Linear {
                assets_per_scale, ..
            } => !assets_per_scale.is_zero(),
            UnwrapRate::CurveLp { total_supply } => !total_supply.is_zero(),
            UnwrapRate::Pendle(s) => {
                s.total_pt > alloy_primitives::I256::ZERO && !s.sy_scale.is_zero()
            }
        }
    }

    /// What unwrapping `amount` pays. Linear: the rate less one part per
    /// million and one wei (the vault rounds down, and its rate moves a
    /// little between the read and inclusion), and `InsufficientLiquidity`
    /// above the vault's cash. Curve LP: the pool's own withdrawal math on
    /// the book's state of the pool, exactly.
    pub fn convert(&self, amount: U256, book: &PoolBook) -> Result<U256, RouteError> {
        if !self.is_live() {
            return Err(RouteError::StalePool);
        }
        match (self.rate, self.kind) {
            (
                UnwrapRate::Linear {
                    assets_per_scale,
                    max_into,
                },
                _,
            ) => {
                let raw = mul_div_512(amount, assets_per_scale, self.scale)?;
                if max_into.is_some_and(|cap| raw > cap) {
                    return Err(RouteError::InsufficientLiquidity);
                }
                let haircut = raw
                    .checked_div(U256::from(1_000_000u64))
                    .unwrap_or_default();
                Ok(raw.saturating_sub(haircut).saturating_sub(U256::ONE))
            }
            (UnwrapRate::CurveLp { total_supply }, UnwrapKind::CurveLp { i }) => {
                let pool = book
                    .by_address(self.wrapper_token)
                    .and_then(|id| book.get(id))
                    .ok_or(RouteError::BadLeg)?;
                let PoolState::Curve(s) = &pool.state else {
                    return Err(RouteError::BadLeg);
                };
                ng_withdraw_one_coin(s, i, amount, total_supply)
            }
            (UnwrapRate::Pendle(s), UnwrapKind::PendleMarket { .. }) => {
                crate::pendle::sell_pt_for_out(&s, amount)
            }
            _ => Err(RouteError::BadLeg),
        }
    }

    /// `into` per wrapper unit at zero size, as `(num, den)`: the linear
    /// rate, or a curve's marginal over one [`Self::scale`] step.
    fn marginal(&self, book: &PoolBook) -> Result<(U256, U256), RouteError> {
        match self.rate {
            UnwrapRate::Linear {
                assets_per_scale, ..
            } => Ok((assets_per_scale, self.scale)),
            _ => Ok((self.convert(self.scale, book)?, self.scale)),
        }
    }

    /// `ρ` of the unwrap alone (Q96 sqrt of its marginal rate).
    pub fn rho(&self, book: &PoolBook) -> Result<U256, RouteError> {
        let (num, den) = self.marginal(book)?;
        let q = U512::from(num)
            .checked_mul(U512::from(Q192))
            .and_then(|v| v.checked_div(U512::from(den)))
            .ok_or(RouteError::Math)?;
        narrow(q.root(2))
    }

    /// `ρ` of `inner ∘ unwrap` from `inner`'s `ρ`: `sqrt(ρ_inner² · rate)`.
    pub fn scale_rho(&self, rho_inner: U256, book: &PoolBook) -> Result<U256, RouteError> {
        let (num, den) = self.marginal(book)?;
        let q = U512::from(rho_inner)
            .checked_mul(U512::from(rho_inner))
            .and_then(|v| v.checked_mul(U512::from(num)))
            .and_then(|v| v.checked_div(U512::from(den)))
            .ok_or(RouteError::Math)?;
        narrow(q.root(2))
    }
}

/// Where a `(collateral, debt)` exit comes from.
#[derive(Debug)]
pub enum ExitSource<'a> {
    /// Pools holding both tokens.
    Direct(&'a [Leg]),
    /// Unwrap first, then these pools of the unwrapped asset (empty when the
    /// unwrapped asset is the debt itself).
    Unwrap(&'a Unwrap, &'a [Leg]),
}

/// A `(collateral, debt)` exit through the book's hub: the collateral (or
/// what it unwraps into, when it has no pool into the hub) sold into the
/// hub, then the hub sold into the debt.
#[derive(Debug)]
pub struct HubRoute<'a> {
    pub hub: AssetId,
    pub unwrap: Option<&'a Unwrap>,
    /// Pools selling the collateral (or what it unwraps into) for the hub.
    pub into_hub: &'a [Leg],
    /// Pools selling the hub for the debt.
    pub out_of_hub: &'a [Leg],
}

/// Every routable pool plus the `(asset_in, asset_out) → legs` index.
/// Written by the ingest thread (`apply_log`), cloned by the warm builder
/// per publish. Discovery: V3 `PoolCreated` at the factory adds a pool
/// whose both tokens are tracked assets; its own logs then build its
/// state exactly from `Initialize` onward (carry-forward: the 03A
/// `LogRouter` filter table is static — re-subscribe on `discovered()`).
#[derive(Clone, Debug)]
pub struct PoolBook {
    pools: Vec<Pool>,
    by_address: HashMap<Address, PoolId>,
    legs: HashMap<(AssetId, AssetId), SmallVec<[Leg; 8]>>,
    assets: HashMap<Address, AssetId>,
    /// Wrapper asset → how to unwrap it.
    unwraps: HashMap<AssetId, Unwrap>,
    /// V2 pairs added at runtime and not yet seeded → block of the newest
    /// `Sync` folded into them (0 = none). A seed read older than that is
    /// refused: the `Sync` already set the reserves exactly.
    pending_v2: HashMap<PoolId, u64>,
    v3_factory: Option<Address>,
    /// V4 pools by pool id (their logs all come from the PoolManager).
    by_v4_id: HashMap<B256, PoolId>,
    /// Balancer pools by pool id (their swaps and balance changes are logs
    /// of the Vault).
    by_balancer_id: HashMap<B256, PoolId>,
    /// Fluid DEX pools by the Liquidity-layer token they hold (their
    /// positions' totals, prices and balance change by `LogOperate`s of
    /// the one Liquidity contract).
    by_fluid_token: HashMap<Address, Vec<PoolId>>,
    /// Gas per V3 hop for discovered pools.
    v3_hop_gas: u64,
    /// The asset exits may also route through ([`Self::hub_route`]). WETH:
    /// its raw units are wei, so its gas price is the identity.
    hub: Option<AssetId>,
    /// The token graph exits may also route through: two-hop exits through
    /// any intermediate token it proposes ([`crate::graph::GraphRoutes`]).
    /// `None`: only the hub.
    graph: Option<std::sync::Arc<crate::graph::GraphRoutes>>,
    discovered: u64,
    generation: u64,
}

impl PoolBook {
    /// `assets`: tracked token → global id. `v3_factory`: subscribe to
    /// `PoolCreated` there (none → no discovery).
    #[must_use]
    pub fn new(
        assets: HashMap<Address, AssetId>,
        v3_factory: Option<Address>,
        v3_hop_gas: u64,
    ) -> Self {
        Self {
            pools: Vec::new(),
            by_address: HashMap::new(),
            legs: HashMap::new(),
            assets,
            unwraps: HashMap::new(),
            pending_v2: HashMap::new(),
            v3_factory,
            by_v4_id: HashMap::new(),
            by_balancer_id: HashMap::new(),
            by_fluid_token: HashMap::new(),
            v3_hop_gas,
            hub: None,
            graph: None,
            discovered: 0,
            generation: 0,
        }
    }

    /// Route exits through the intermediate tokens `graph` proposes, as
    /// well as directly and through the hub; `None` turns that off.
    pub fn set_graph(&mut self, graph: Option<std::sync::Arc<crate::graph::GraphRoutes>>) {
        self.graph = graph;
        self.generation = self.generation.wrapping_add(1);
    }

    #[inline]
    #[must_use]
    pub fn graph(&self) -> Option<&crate::graph::GraphRoutes> {
        self.graph.as_deref()
    }

    /// The asset id of a tracked token.
    #[inline]
    #[must_use]
    pub fn asset_id(&self, token: Address) -> Option<AssetId> {
        self.assets.get(&token).copied()
    }

    /// Route exits through `weth` as well as directly. Most collateral
    /// reaches a stablecoin through ETH (GUIDE 12 §4e), and the deep pools
    /// are against WETH: a collateral's direct pool into its debt can be
    /// thin, or missing.
    pub fn set_hub(&mut self, weth: AssetId) {
        self.hub = Some(weth);
        self.generation = self.generation.wrapping_add(1);
    }

    #[inline]
    #[must_use]
    pub fn hub(&self) -> Option<AssetId> {
        self.hub
    }

    /// Register a pool. Rejects duplicates and coin/asset width mismatch.
    pub fn add(&mut self, pool: Pool) -> Result<PoolId, RouteError> {
        if self.by_address.contains_key(&pool.address) || pool.assets.len() != pool.tokens.len() {
            return Err(RouteError::BadLeg);
        }
        let n = pool.assets.len();
        match &pool.state {
            PoolState::V2(_) | PoolState::V3(_) if n != 2 => return Err(RouteError::BadLeg),
            PoolState::Curve(c) if c.balances.len() != n || c.rates.len() != n => {
                return Err(RouteError::BadLeg)
            }
            PoolState::Crypto(c) if c.balances.len() != n || c.precisions.len() != n => {
                return Err(RouteError::BadLeg)
            }
            PoolState::Balancer(b)
                if n != 2
                    || b.balances.len() != n
                    || b.weights.len() != n
                    || b.scaling.len() != n =>
            {
                return Err(RouteError::BadLeg)
            }
            PoolState::Fluid(f) if n != 2 || f.tokens.len() != n => return Err(RouteError::BadLeg),
            _ => {}
        }
        if let PoolState::Balancer(b) = &pool.state {
            if self.by_balancer_id.contains_key(&b.pool_id) {
                return Err(RouteError::BadLeg);
            }
        }
        let id = PoolId(u32::try_from(self.pools.len()).map_err(|_| RouteError::Math)?);
        if let PoolState::V3(V3State { v4: Some(k), .. }) = &pool.state {
            if self.by_v4_id.contains_key(&k.id) {
                return Err(RouteError::BadLeg);
            }
            self.by_v4_id.insert(k.id, id);
        }
        for (i, &a) in pool.assets.iter().enumerate() {
            for (j, &b) in pool.assets.iter().enumerate() {
                if i == j {
                    continue;
                }
                let (Ok(i), Ok(j)) = (u8::try_from(i), u8::try_from(j)) else {
                    return Err(RouteError::BadLeg);
                };
                self.legs
                    .entry((a, b))
                    .or_default()
                    .push(Leg { pool: id, i, j });
            }
        }
        if let PoolState::Balancer(b) = &pool.state {
            self.by_balancer_id.insert(b.pool_id, id);
        }
        if let PoolState::Fluid(f) = &pool.state {
            for t in &f.tokens {
                self.by_fluid_token.entry(*t).or_default().push(id);
            }
        }
        self.by_address.insert(pool.address, id);
        self.pools.push(pool);
        self.generation = self.generation.wrapping_add(1);
        Ok(id)
    }

    #[inline]
    #[must_use]
    pub fn get(&self, id: PoolId) -> Option<&Pool> {
        self.pools.get(usize::try_from(id.0).ok()?)
    }

    #[inline]
    pub fn get_mut(&mut self, id: PoolId) -> Option<&mut Pool> {
        self.pools.get_mut(usize::try_from(id.0).ok()?)
    }

    #[inline]
    #[must_use]
    pub fn by_address(&self, a: Address) -> Option<PoolId> {
        self.by_address.get(&a).copied()
    }

    #[inline]
    #[must_use]
    pub fn pools(&self) -> &[Pool] {
        &self.pools
    }

    /// Directed legs quoting `asset_in → asset_out`. Empty when none.
    #[inline]
    #[must_use]
    pub fn legs(&self, asset_in: AssetId, asset_out: AssetId) -> &[Leg] {
        self.legs
            .get(&(asset_in, asset_out))
            .map_or(&[], SmallVec::as_slice)
    }

    /// Every `(in, out)` pair with at least one leg, plus each unwrapped
    /// wrapper's pairs (`(wrapper, into)` and `(wrapper, out)` for each
    /// `(into, out)`) that have no pool of their own.
    pub fn pairs(&self) -> impl Iterator<Item = (AssetId, AssetId)> + '_ {
        let direct = self.legs.keys().copied();
        let via = self.unwraps.values().flat_map(move |u| {
            core::iter::once((u.wrapper, u.into))
                .chain(
                    self.legs
                        .keys()
                        .filter(move |(a, _)| *a == u.into)
                        .map(move |(_, d)| (u.wrapper, *d)),
                )
                .filter(move |p| !self.legs.contains_key(p))
        });
        direct.chain(via)
    }

    /// Add a V2 pair while running: unseeded (zero reserves, not live) until
    /// [`Self::seed_pending_v2`] applies a read, or a `Sync` folds in.
    pub fn add_pending_v2(&mut self, pool: Pool) -> Result<PoolId, RouteError> {
        if !matches!(pool.state, PoolState::V2(_)) {
            return Err(RouteError::BadLeg);
        }
        let id = self.add(pool)?;
        self.pending_v2.insert(id, 0);
        Ok(id)
    }

    /// Seed a runtime-added V2 pair from `getReserves` read at `block`.
    /// Refused (`false`, and the pair is no longer pending) when a `Sync` at
    /// or after `block` already set its reserves.
    pub fn seed_pending_v2(&mut self, id: PoolId, r0: U256, r1: U256, block: u64) -> bool {
        let Some(last_sync) = self.pending_v2.remove(&id) else {
            return false;
        };
        if last_sync >= block {
            return false;
        }
        let Some(PoolState::V2(s)) = self.get_mut(id).map(|p| &mut p.state) else {
            return false;
        };
        s.reserve0 = r0;
        s.reserve1 = r1;
        self.generation = self.generation.wrapping_add(1);
        true
    }

    /// Runtime-added V2 pairs still waiting for their seed.
    pub fn pending_v2(&self) -> impl Iterator<Item = PoolId> + '_ {
        self.pending_v2.keys().copied()
    }

    /// Force a Curve / crypto pool to be re-read at or after `block` (a pool
    /// added at runtime: logs before its subscription were not seen).
    pub fn mark_stale(&mut self, id: PoolId, block: u64) -> bool {
        let Some(pool) = self.get_mut(id) else {
            return false;
        };
        match &mut pool.state {
            PoolState::Curve(s) => {
                s.stale = true;
                s.stale_block = s.stale_block.max(block);
            }
            PoolState::Crypto(s) => {
                s.stale = true;
                s.stale_block = s.stale_block.max(block);
            }
            PoolState::Balancer(s) => {
                s.stale = true;
                s.stale_block = s.stale_block.max(block);
            }
            PoolState::Fluid(s) => {
                s.stale = true;
                s.stale_block = s.stale_block.max(block);
            }
            PoolState::V2(_) | PoolState::V3(_) => return false,
        }
        self.generation = self.generation.wrapping_add(1);
        true
    }

    /// Stop unwrapping `wrapper` (no longer admitted). `true` when it was.
    pub fn remove_unwrap(&mut self, wrapper: AssetId) -> bool {
        let removed = self.unwraps.remove(&wrapper).is_some();
        if removed {
            self.generation = self.generation.wrapping_add(1);
        }
        removed
    }

    /// Register a wrapper's unwrap. Its rate starts unread.
    pub fn add_unwrap(&mut self, u: Unwrap) {
        self.unwraps.insert(u.wrapper, u);
        self.generation = self.generation.wrapping_add(1);
    }

    /// Record a wrapper's rate read at `block`.
    pub fn set_unwrap_rate(&mut self, wrapper: AssetId, rate: UnwrapRate, block: u64) -> bool {
        let Some(u) = self.unwraps.get_mut(&wrapper) else {
            return false;
        };
        if block < u.read_block {
            return false;
        }
        let changed = u.rate != rate;
        u.rate = rate;
        u.read_block = block;
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
        true
    }

    /// A live PT's market has reached expiry (the market refuses to trade
    /// from then on): the PT now exits by redeeming through its YT (venue 6)
    /// into the same token, as any expired PT does. The rate starts unread —
    /// the next reseed reads it as a linear redeem at a thousand PT — so the
    /// PT is not routed on the market's last price in between. `true` when
    /// it switched.
    pub fn expire_pendle_market(&mut self, wrapper: AssetId) -> bool {
        let Some(u) = self.unwraps.get_mut(&wrapper) else {
            return false;
        };
        let UnwrapKind::PendleMarket { yt, sy, .. } = u.kind else {
            return false;
        };
        u.kind = UnwrapKind::PendlePt { yt, sy };
        u.rate = UnwrapRate::Unread;
        u.read_block = 0;
        u.scale = u.scale.saturating_mul(U256::from(1_000u64));
        u.gas = u.expiry_gas;
        self.generation = self.generation.wrapping_add(1);
        true
    }

    #[must_use]
    pub fn unwrap_of(&self, wrapper: AssetId) -> Option<&Unwrap> {
        self.unwraps.get(&wrapper)
    }

    pub fn unwraps(&self) -> impl Iterator<Item = &Unwrap> {
        self.unwraps.values()
    }

    /// The exit for `(coll, debt)`: pools holding both when there are any,
    /// otherwise unwrap `coll` (when it is a wrapper with a live rate) and
    /// route what it unwraps into.
    #[must_use]
    pub fn exit_source(&self, coll: AssetId, debt: AssetId) -> ExitSource<'_> {
        let direct = self.legs(coll, debt);
        if !direct.is_empty() {
            return ExitSource::Direct(direct);
        }
        match self.unwraps.get(&coll) {
            Some(u) if u.is_live() && (u.into == debt || !self.legs(u.into, debt).is_empty()) => {
                ExitSource::Unwrap(u, self.legs(u.into, debt))
            }
            _ => ExitSource::Direct(direct),
        }
    }

    /// The exit of `(coll, debt)` through the hub, when the book has one:
    /// `coll` sold into the hub, or what it unwraps into when `coll` has no
    /// pool into the hub, and the hub sold into `debt`. `None` when either
    /// end is the hub (that exit is direct), when a side has no pool, and
    /// when the unwrap pays the hub or the debt itself (the unwrap exit).
    #[must_use]
    pub fn hub_route(&self, coll: AssetId, debt: AssetId) -> Option<HubRoute<'_>> {
        let hub = self.hub?;
        if coll == hub || debt == hub || coll == debt {
            return None;
        }
        let out_of_hub = self.legs(hub, debt);
        if out_of_hub.is_empty() {
            return None;
        }
        let own = self.legs(coll, hub);
        if !own.is_empty() {
            return Some(HubRoute {
                hub,
                unwrap: None,
                into_hub: own,
                out_of_hub,
            });
        }
        let u = self.unwraps.get(&coll).filter(|u| u.is_live())?;
        if u.into == hub || u.into == debt {
            return None;
        }
        let inner = self.legs(u.into, hub);
        (!inner.is_empty()).then_some(HubRoute {
            hub,
            unwrap: Some(u),
            into_hub: inner,
            out_of_hub,
        })
    }

    /// The pools an exit of `(coll, debt)` swaps through, in the order the
    /// solve iterates them.
    #[must_use]
    pub fn exit_legs(&self, coll: AssetId, debt: AssetId) -> &[Leg] {
        match self.exit_source(coll, debt) {
            ExitSource::Direct(l) | ExitSource::Unwrap(_, l) => l,
        }
    }

    /// Bumped on every state change; the warm tier rebuilds when it moves.
    #[inline]
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Apply a Curve read pinned at `block` to pool `id` (off the hot path).
    /// `Ok(false)` when a newer pool log already made that read obsolete or
    /// `id` is not a Curve pool; bumps [`PoolBook::generation`] on success.
    #[allow(clippy::too_many_arguments)]
    pub fn reseed_curve(
        &mut self,
        id: PoolId,
        balances: &[U256],
        a: U256,
        a_precision: U256,
        fee: U256,
        ng: Option<(&[U256], U256)>,
        block: u64,
    ) -> Result<bool, RouteError> {
        let Some(PoolState::Curve(c)) = self.get_mut(id).map(|p| &mut p.state) else {
            return Ok(false);
        };
        if c.ng != ng.is_some() {
            return Err(RouteError::BadLeg);
        }
        if c.stale && c.stale_block > block {
            return Ok(false);
        }
        if let Some((rates, offpeg)) = ng {
            c.set_ng_params(rates, offpeg)?;
        }
        let prev = c.a_precision;
        c.a_precision = a_precision;
        match c.reseed_at(balances, a, fee, block) {
            Ok(true) => {}
            other => {
                c.a_precision = prev;
                return other;
            }
        }
        self.generation = self.generation.wrapping_add(1);
        Ok(true)
    }

    /// Replace a Balancer pool's state from a read pinned at `block`.
    /// Refused (`Ok(false)`) when a Vault log newer than `block` made it
    /// stale, or the pool is not a Balancer pool.
    pub fn reseed_balancer(
        &mut self,
        id: PoolId,
        read: &crate::balancer::BalancerRead,
        block: u64,
    ) -> Result<bool, RouteError> {
        let Some(PoolState::Balancer(b)) = self.get_mut(id).map(|p| &mut p.state) else {
            return Ok(false);
        };
        if b.stale && b.stale_block > block {
            return Ok(false);
        }
        if read.balances.len() != b.balances.len()
            || read.weights.len() != b.weights.len()
            || read.balances.iter().any(|x| x.is_zero())
        {
            return Err(RouteError::BadLeg);
        }
        b.balances = read.balances.iter().copied().collect();
        b.weights = read.weights.iter().copied().collect();
        b.swap_fee = read.swap_fee;
        // A paused pool refuses swaps: not routed until a read finds it open.
        b.stale = read.paused;
        b.read_block = block;
        self.generation = self.generation.wrapping_add(1);
        Ok(true)
    }

    /// Replace a Fluid DEX pool's state from a read pinned at `block`.
    /// Refused (`Ok(false)`) when a Liquidity log newer than `block` made it
    /// stale, or the pool is not a Fluid pool.
    pub fn reseed_fluid(
        &mut self,
        id: PoolId,
        read: &crate::fluid::FluidRead,
        block: u64,
    ) -> Result<bool, RouteError> {
        let Some(PoolState::Fluid(f)) = self.get_mut(id).map(|p| &mut p.state) else {
            return Ok(false);
        };
        if f.stale && f.stale_block > block {
            return Ok(false);
        }
        f.dex_vars = read.dex_vars;
        f.dex_vars2 = read.dex_vars2;
        f.center_ext = read.center_ext;
        f.liq = read.tokens;
        f.exec_ts = read.exec_ts;
        if let Some((prec, deployer)) = read.constants {
            f.prec = prec;
            f.deployer = deployer;
        }
        // Shifts, hooks and pauses are not followed: such a pool reads as
        // not live (`FluidState::is_live`) until it is plain again.
        f.stale = false;
        f.read_block = block;
        self.generation = self.generation.wrapping_add(1);
        Ok(true)
    }

    /// Replace a crypto pool's state from a read pinned at `block`. Refused
    /// (`Ok(false)`) when a pool log newer than `block` made it stale, or the
    /// pool is not a crypto pool.
    pub fn reseed_crypto(
        &mut self,
        id: PoolId,
        read: &crate::crypto::CryptoRead,
        block: u64,
    ) -> Result<bool, RouteError> {
        let Some(PoolState::Crypto(c)) = self.get_mut(id).map(|p| &mut p.state) else {
            return Ok(false);
        };
        if c.stale && c.stale_block > block {
            return Ok(false);
        }
        if read.balances.len() != c.balances.len()
            || read.price_scale.len() != c.price_scale.len()
            || read.balances.iter().any(|b| b.is_zero())
        {
            return Err(RouteError::BadLeg);
        }
        c.balances = read.balances.iter().copied().collect();
        c.price_scale = read.price_scale.iter().copied().collect();
        c.d = read.d;
        c.ann = read.ann;
        c.gamma = read.gamma;
        c.mid_fee = read.mid_fee;
        c.out_fee = read.out_fee;
        c.fee_gamma = read.fee_gamma;
        // A ramping pool recomputes D inside `exchange`: not modelled.
        c.stale = read.ramping;
        c.tweak = read.tweak.clone();
        c.read_block = block;
        self.generation = self.generation.wrapping_add(1);
        Ok(true)
    }

    /// Pools added by `PoolCreated` since start (each needs a filter
    /// re-subscribe at the 03A router).
    #[inline]
    #[must_use]
    pub fn discovered(&self) -> u64 {
        self.discovered
    }

    /// Fold one routed log. Ingest thread. Unknown addresses / topics are
    /// no-ops; a malformed body at a known pool marks that pool stale
    /// (V3/Curve) — never a guess.
    pub fn apply_log(&mut self, log: &DecodedLog<'_>) {
        let Some(&t0) = log.topics.first() else {
            return;
        };
        if Some(log.address) == self.v3_factory {
            if t0 == IUniswapV3Factory::PoolCreated::SIGNATURE_HASH {
                self.on_pool_created(log);
            }
            return;
        }
        if log.address == BALANCER_VAULT {
            let Some(&pid) = log.topics.get(1) else {
                return;
            };
            let Some(id) = self.by_balancer_id.get(&pid).copied() else {
                return;
            };
            let Some(pool) = self
                .pools
                .get_mut(usize::try_from(id.0).unwrap_or(usize::MAX))
            else {
                return;
            };
            if let PoolState::Balancer(s) = &mut pool.state {
                if fold_balancer(s, t0, log) {
                    self.generation = self.generation.wrapping_add(1);
                }
            }
            return;
        }
        if log.address == FLUID_LIQUIDITY {
            if t0 == IFluidLiquidity::LogOperate::SIGNATURE_HASH && self.on_fluid_operate(log) {
                self.generation = self.generation.wrapping_add(1);
            }
            return;
        }
        if log.address == V4_POOL_MANAGER {
            let Some(&pid) = log.topics.get(1) else {
                return;
            };
            let Some(id) = self.by_v4_id.get(&pid).copied() else {
                return;
            };
            let Some(pool) = self
                .pools
                .get_mut(usize::try_from(id.0).unwrap_or(usize::MAX))
            else {
                return;
            };
            if let PoolState::V3(s) = &mut pool.state {
                if fold_v4(s, t0, log) {
                    self.generation = self.generation.wrapping_add(1);
                }
            }
            return;
        }
        let Some(id) = self.by_address.get(&log.address).copied() else {
            return;
        };
        let Some(pool) = self
            .pools
            .get_mut(usize::try_from(id.0).unwrap_or(usize::MAX))
        else {
            return;
        };
        if let Some(last) = self.pending_v2.get_mut(&id) {
            *last = (*last).max(log.block);
        }
        let changed = match &mut pool.state {
            PoolState::V3(s) => fold_v3(s, t0, log),
            PoolState::V2(s) => fold_v2(s, t0, log),
            PoolState::Curve(s) => {
                if s.stale_topics().contains(&t0) {
                    s.stale = true;
                    s.stale_block = s.stale_block.max(log.block);
                    true
                } else {
                    false
                }
            }
            PoolState::Crypto(s) => {
                if CRYPTO_STALE_TOPICS.contains(&t0) {
                    s.stale = true;
                    s.stale_block = s.stale_block.max(log.block);
                    true
                } else {
                    false
                }
            }
            // A Balancer pool's logs are the Vault's, handled above; the
            // pool contract's own (its fee changing, pausing) mark it stale.
            PoolState::Balancer(s) => {
                if BALANCER_POOL_STALE_TOPICS.contains(&t0) {
                    s.stale = true;
                    s.stale_block = s.stale_block.max(log.block);
                    true
                } else {
                    false
                }
            }
            // A Fluid pool's logs are the Liquidity layer's, handled above.
            PoolState::Fluid(_) => false,
        };
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// One `LogOperate` of the Liquidity layer. For every Fluid pool holding
    /// the token: another user's operation leaves the pool's own positions
    /// alone and sets the token's totals and exchange prices to exactly the
    /// words the event carries (the layer's balance moves by the supply and
    /// borrow amounts); the pool's own operation (a swap, a deposit)
    /// changes its positions, which the event does not give: re-read.
    fn on_fluid_operate(&mut self, log: &DecodedLog<'_>) -> bool {
        let Ok(ev) =
            IFluidLiquidity::LogOperate::decode_raw_log(log.topics.iter().copied(), log.data)
        else {
            tracing::warn!("Fluid LogOperate: undecodable body");
            return false;
        };
        let Some(ids) = self.by_fluid_token.get(&ev.token) else {
            return false;
        };
        let mut changed = false;
        for id in ids {
            let Some(pool) = self
                .pools
                .get_mut(usize::try_from(id.0).unwrap_or(usize::MAX))
            else {
                continue;
            };
            let address = pool.address;
            let PoolState::Fluid(f) = &mut pool.state else {
                continue;
            };
            let Some(k) = f.tokens.iter().position(|t| *t == ev.token) else {
                continue;
            };
            if ev.user == address {
                f.stale = true;
                f.stale_block = f.stale_block.max(log.block);
            } else {
                let Some(t) = f.liq.get_mut(k) else {
                    continue;
                };
                t.ep_cfg = ev.exchangePricesAndConfig;
                t.totals = ev.totalAmounts;
                // supply and payback bring the token in; withdraw and borrow
                // take it out.
                match ev.supplyAmount.checked_sub(ev.borrowAmount) {
                    Some(d) if d.is_negative() => {
                        t.balance = t.balance.saturating_sub(d.unsigned_abs());
                    }
                    Some(d) => t.balance = t.balance.saturating_add(d.unsigned_abs()),
                    None => {
                        f.stale = true;
                        f.stale_block = f.stale_block.max(log.block);
                    }
                }
            }
            changed = true;
        }
        changed
    }

    fn on_pool_created(&mut self, log: &DecodedLog<'_>) {
        let Ok(ev) =
            IUniswapV3Factory::PoolCreated::decode_raw_log(log.topics.iter().copied(), log.data)
        else {
            tracing::warn!(address = ?log.address, "PoolCreated: undecodable body");
            return;
        };
        let (Some(&a0), Some(&a1)) = (self.assets.get(&ev.token0), self.assets.get(&ev.token1))
        else {
            return; // not both tracked: not an exit venue for us
        };
        let fee_pips = u32::try_from(ev.fee).unwrap_or(u32::MAX);
        let pool = Pool {
            address: ev.pool,
            assets: SmallVec::from_slice(&[a0, a1]),
            tokens: SmallVec::from_slice(&[ev.token0, ev.token1]),
            hop_gas: self.v3_hop_gas,
            state: PoolState::V3(V3State {
                factory: 0,
                sqrt_price_x96: U256::ZERO,
                tick: 0,
                liquidity: 0,
                fee_pips,
                tick_spacing: ev.tickSpacing.as_i32(),
                ticks: Vec::new(),
                v4: None,
                window: None,
            }),
        };
        if self.add(pool).is_ok() {
            self.discovered = self.discovered.saturating_add(1);
        }
    }
}

/// Uniswap V4 `PoolManager` (the only one on mainnet; the Executor's
/// `MainnetVenues.V4_POOL_MANAGER`).
pub const V4_POOL_MANAGER: Address =
    alloy_primitives::address!("000000000004444c5dc75cB358380D2e3dE08A90");

alloy_sol_types::sol! {
    /// v4-core `IPoolManager` events.
    #[allow(clippy::too_many_arguments)]
    interface IV4PoolManager {
        event Swap(bytes32 indexed id, address indexed sender, int128 amount0, int128 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick, uint24 fee);
        event ModifyLiquidity(bytes32 indexed id, address indexed sender, int24 tickLower, int24 tickUpper, int256 liquidityDelta, bytes32 salt);
    }
}

/// One V4 pool's `Swap` or `ModifyLiquidity`, folded as V3's `Swap` and
/// `Mint`/`Burn` are. A malformed body marks the pool unread.
fn fold_v4(s: &mut V3State, t0: B256, log: &DecodedLog<'_>) -> bool {
    let topics = log.topics.iter().copied();
    if t0 == IV4PoolManager::Swap::SIGNATURE_HASH {
        match IV4PoolManager::Swap::decode_raw_log(topics, log.data) {
            Ok(ev) => {
                s.sqrt_price_x96 = U256::from(ev.sqrtPriceX96);
                s.liquidity = ev.liquidity;
                s.tick = ev.tick.as_i32();
            }
            Err(_) => s.sqrt_price_x96 = U256::ZERO,
        }
        true
    } else if t0 == IV4PoolManager::ModifyLiquidity::SIGNATURE_HASH {
        match IV4PoolManager::ModifyLiquidity::decode_raw_log(topics, log.data) {
            Ok(ev) => {
                let add = !ev.liquidityDelta.is_negative();
                match u128::try_from(ev.liquidityDelta.unsigned_abs()) {
                    Ok(amount) => {
                        v3_modify(s, ev.tickLower.as_i32(), ev.tickUpper.as_i32(), amount, add);
                    }
                    Err(_) => s.sqrt_price_x96 = U256::ZERO,
                }
            }
            Err(_) => s.sqrt_price_x96 = U256::ZERO,
        }
        true
    } else {
        false
    }
}

/// Balancer V2 Vault (`MainnetVenues.BALANCER_VAULT`): every Balancer pool's
/// swaps and balance changes are logs of this one contract.
pub const BALANCER_VAULT: Address =
    alloy_primitives::address!("0xBA12222222228d8Ba445958a75a0704d566BF2C8");

sol! {
    /// Balancer V2 Vault events (`IVault.sol`) and a weighted pool's own.
    interface IBalancerVault {
        event Swap(bytes32 indexed poolId, address indexed tokenIn, address indexed tokenOut, uint256 amountIn, uint256 amountOut);
        event PoolBalanceChanged(bytes32 indexed poolId, address indexed lp, address[] tokens, int256[] deltas, uint256[] protocolFeeAmounts);
        event PoolBalanceManaged(bytes32 indexed poolId, address indexed assetManager, address indexed token, int256 cashDelta, int256 managedDelta);
    }
    interface IBalancerWeightedPool {
        event SwapFeePercentageChanged(uint256 swapFeePercentage);
        event PausedStateChanged(bool paused);
    }
}

/// The Fluid Liquidity layer (proxy): every Fluid DEX pool's positions, and
/// the token totals and exchange prices they trade against, are its state.
pub const FLUID_LIQUIDITY: Address =
    alloy_primitives::address!("0x52Aa899454998Be5b000Ad077a46Bbe360F4e497");

sol! {
    #[allow(clippy::too_many_arguments)] // the event's own eight fields
    /// `FluidLiquidityUserModule` events (`events.sol`, verified source).
    interface IFluidLiquidity {
        event LogOperate(address indexed user, address indexed token, int256 supplyAmount, int256 borrowAmount, address withdrawTo, address borrowTo, uint256 totalAmounts, uint256 exchangePricesAndConfig);
    }
}

/// Topic0s of a weighted pool's own logs that change what it quotes.
const BALANCER_POOL_STALE_TOPICS: [B256; 2] = [
    IBalancerWeightedPool::SwapFeePercentageChanged::SIGNATURE_HASH,
    IBalancerWeightedPool::PausedStateChanged::SIGNATURE_HASH,
];

/// Fold one Vault log of a Balancer pool. A `Swap` moves the pool's balances
/// by exactly its two amounts (the Vault credits `amountIn` and debits
/// `amountOut`; protocol fees are taken later, as BPT, on joins and exits).
/// A join, an exit or an asset manager's move changes balances in ways the
/// log does not give whole: the pool is re-read.
fn fold_balancer(s: &mut BalancerState, t0: B256, log: &DecodedLog<'_>) -> bool {
    if t0 == IBalancerVault::Swap::SIGNATURE_HASH {
        match IBalancerVault::Swap::decode_raw_log(log.topics.iter().copied(), log.data) {
            Ok(ev) => {
                // The Vault's `getPoolTokens` order is ascending by address;
                // the book's coin order is the same, so match by token.
                let (Some(i), Some(j)) = (
                    s.tokens.iter().position(|t| *t == ev.tokenIn),
                    s.tokens.iter().position(|t| *t == ev.tokenOut),
                ) else {
                    s.stale = true;
                    s.stale_block = s.stale_block.max(log.block);
                    return true;
                };
                let (Some(bi), Some(bj)) = (
                    s.balances
                        .get(i)
                        .copied()
                        .and_then(|b| b.checked_add(ev.amountIn)),
                    s.balances
                        .get(j)
                        .copied()
                        .and_then(|b| b.checked_sub(ev.amountOut)),
                ) else {
                    s.stale = true;
                    s.stale_block = s.stale_block.max(log.block);
                    return true;
                };
                if let Some(b) = s.balances.get_mut(i) {
                    *b = bi;
                }
                if let Some(b) = s.balances.get_mut(j) {
                    *b = bj;
                }
            }
            Err(_) => {
                s.stale = true;
                s.stale_block = s.stale_block.max(log.block);
            }
        }
        true
    } else if t0 == IBalancerVault::PoolBalanceChanged::SIGNATURE_HASH
        || t0 == IBalancerVault::PoolBalanceManaged::SIGNATURE_HASH
    {
        s.stale = true;
        s.stale_block = s.stale_block.max(log.block);
        true
    } else {
        false
    }
}

/// The V3 factory ids a pool-direct leg names (`liq_wire::wire`).
pub use liq_wire::wire::{V3_FACTORY_PANCAKE, V3_FACTORY_SUSHI, V3_FACTORY_UNISWAP};

/// A V3 pool's Swap topic0, by the factory that deployed it.
fn v3_swap_topic(factory: u8) -> B256 {
    if factory == V3_FACTORY_PANCAKE {
        IPancakeV3Pool::Swap::SIGNATURE_HASH
    } else {
        IUniswapV3Pool::Swap::SIGNATURE_HASH
    }
}

fn fold_v3(s: &mut V3State, t0: B256, log: &DecodedLog<'_>) -> bool {
    let topics = log.topics.iter().copied();
    if t0 == v3_swap_topic(s.factory) {
        // The price, liquidity and tick the swap left, from whichever
        // event shape this factory's pools emit.
        let after = if s.factory == V3_FACTORY_PANCAKE {
            IPancakeV3Pool::Swap::decode_raw_log(topics, log.data)
                .map(|ev| (ev.sqrtPriceX96, ev.liquidity, ev.tick.as_i32()))
        } else {
            IUniswapV3Pool::Swap::decode_raw_log(topics, log.data)
                .map(|ev| (ev.sqrtPriceX96, ev.liquidity, ev.tick.as_i32()))
        };
        match after {
            Ok((price, liquidity, tick)) => {
                s.sqrt_price_x96 = U256::from(price);
                s.liquidity = liquidity;
                s.tick = tick;
            }
            Err(_) => s.sqrt_price_x96 = U256::ZERO,
        }
        true
    } else if t0 == IUniswapV3Pool::Mint::SIGNATURE_HASH {
        match IUniswapV3Pool::Mint::decode_raw_log(topics, log.data) {
            Ok(ev) => v3_modify(
                s,
                ev.tickLower.as_i32(),
                ev.tickUpper.as_i32(),
                ev.amount,
                true,
            ),
            Err(_) => s.sqrt_price_x96 = U256::ZERO,
        }
        true
    } else if t0 == IUniswapV3Pool::Burn::SIGNATURE_HASH {
        match IUniswapV3Pool::Burn::decode_raw_log(topics, log.data) {
            Ok(ev) => v3_modify(
                s,
                ev.tickLower.as_i32(),
                ev.tickUpper.as_i32(),
                ev.amount,
                false,
            ),
            Err(_) => s.sqrt_price_x96 = U256::ZERO,
        }
        true
    } else if t0 == IUniswapV3Pool::Initialize::SIGNATURE_HASH {
        match IUniswapV3Pool::Initialize::decode_raw_log(topics, log.data) {
            Ok(ev) => {
                s.sqrt_price_x96 = U256::from(ev.sqrtPriceX96);
                s.tick = ev.tick.as_i32();
            }
            Err(_) => s.sqrt_price_x96 = U256::ZERO,
        }
        true
    } else {
        false
    }
}

/// `Pool._modifyPosition` effect on ticks and active liquidity. A
/// `Burn` of more than the tick holds is a fold defect: fail closed by
/// zeroing the price (pool excluded until re-seeded).
fn v3_modify(s: &mut V3State, lower: i32, upper: i32, amount: u128, mint: bool) {
    if amount == 0 || lower >= upper {
        return;
    }
    let Ok(delta) = i128::try_from(amount) else {
        s.sqrt_price_x96 = U256::ZERO;
        return;
    };
    let mut apply = |tick: i32, net_sign_pos: bool| -> bool {
        let pos = s.ticks.partition_point(|t| t.tick < tick);
        let signed = if net_sign_pos {
            delta
        } else {
            delta.wrapping_neg()
        };
        let signed = if mint { signed } else { signed.wrapping_neg() };
        let existing = s.ticks.get(pos).filter(|t| t.tick == tick).copied();
        match existing {
            Some(t) => {
                let (Some(net), Some(gross)) = (
                    t.net.checked_add(signed),
                    if mint {
                        t.gross.checked_add(amount)
                    } else {
                        t.gross.checked_sub(amount)
                    },
                ) else {
                    return false;
                };
                if gross == 0 {
                    s.ticks.remove(pos);
                } else if let Some(slot) = s.ticks.get_mut(pos) {
                    slot.net = net;
                    slot.gross = gross;
                }
                true
            }
            None if mint => {
                s.ticks.insert(
                    pos,
                    Tick {
                        tick,
                        net: signed,
                        gross: amount,
                    },
                );
                true
            }
            None => false,
        }
    };
    let ok = apply(lower, true) && apply(upper, false);
    if !ok {
        s.sqrt_price_x96 = U256::ZERO;
        return;
    }
    if lower <= s.tick && s.tick < upper {
        let l = if mint {
            s.liquidity.checked_add(amount)
        } else {
            s.liquidity.checked_sub(amount)
        };
        match l {
            Some(l) => s.liquidity = l,
            None => s.sqrt_price_x96 = U256::ZERO,
        }
    }
}

fn fold_v2(s: &mut V2State, t0: B256, log: &DecodedLog<'_>) -> bool {
    if t0 != IUniswapV2Pair::Sync::SIGNATURE_HASH {
        return false;
    }
    match IUniswapV2Pair::Sync::decode_raw_log(log.topics.iter().copied(), log.data) {
        Ok(ev) => {
            s.reserve0 = U256::from(ev.reserve0);
            s.reserve1 = U256::from(ev.reserve1);
        }
        Err(_) => {
            s.reserve0 = U256::ZERO;
            s.reserve1 = U256::ZERO;
        }
    }
    true
}

impl LogSubscriber for PoolBook {
    fn subscriptions(&self) -> Vec<LogFilter> {
        let mut out = Vec::with_capacity(self.pools.len().saturating_mul(4).saturating_add(3));
        if !self.by_v4_id.is_empty() {
            for topic0 in [
                IV4PoolManager::Swap::SIGNATURE_HASH,
                IV4PoolManager::ModifyLiquidity::SIGNATURE_HASH,
            ] {
                out.push(LogFilter {
                    address: V4_POOL_MANAGER,
                    topic0,
                });
            }
        }
        if !self.by_balancer_id.is_empty() {
            for topic0 in [
                IBalancerVault::Swap::SIGNATURE_HASH,
                IBalancerVault::PoolBalanceChanged::SIGNATURE_HASH,
                IBalancerVault::PoolBalanceManaged::SIGNATURE_HASH,
            ] {
                out.push(LogFilter {
                    address: BALANCER_VAULT,
                    topic0,
                });
            }
        }
        if !self.by_fluid_token.is_empty() {
            out.push(LogFilter {
                address: FLUID_LIQUIDITY,
                topic0: IFluidLiquidity::LogOperate::SIGNATURE_HASH,
            });
        }
        if let Some(f) = self.v3_factory {
            out.push(LogFilter {
                address: f,
                topic0: IUniswapV3Factory::PoolCreated::SIGNATURE_HASH,
            });
        }
        for p in &self.pools {
            let topics: &[B256] = match p.state {
                // Its logs are the PoolManager's, subscribed above.
                PoolState::V3(V3State { v4: Some(_), .. }) => &[],
                PoolState::V3(ref v3) => &[
                    IUniswapV3Pool::Initialize::SIGNATURE_HASH,
                    v3_swap_topic(v3.factory),
                    IUniswapV3Pool::Mint::SIGNATURE_HASH,
                    IUniswapV3Pool::Burn::SIGNATURE_HASH,
                ],
                PoolState::V2(_) => &[IUniswapV2Pair::Sync::SIGNATURE_HASH],
                PoolState::Curve(ref c) => c.stale_topics(),
                PoolState::Crypto(_) => &CRYPTO_STALE_TOPICS,
                PoolState::Balancer(_) => &BALANCER_POOL_STALE_TOPICS,
                // Its logs are the Liquidity layer's, subscribed above.
                PoolState::Fluid(_) => &[],
            };
            out.extend(topics.iter().map(|&topic0| LogFilter {
                address: p.address,
                topic0,
            }));
        }
        out
    }
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use alloy_primitives::{Address, U256};
    use liq_types::{AssetId, LogSubscriber};
    use uniswap_v3_math::tick_math;

    use super::*;
    use crate::fixtures::*;

    fn abs_diff(a: U256, b: U256) -> U256 {
        a.abs_diff(b)
    }

    /// A seeded pool knows ticks only inside its window. Oracle: the same
    /// pool with the window covering every tick quotes the swap; with the
    /// window ending before the swap's end it refuses
    /// (`InsufficientLiquidity`) instead of carrying the liquidity past the
    /// edge, and a price already outside the window refuses too.
    /// A pool's capacity to its window edge is exactly the largest input
    /// the swap accepts: that much swaps, one wei more is refused. A window
    /// covering every tick caps at the price limit; a price outside its
    /// window absorbs nothing. Oracle: `v3_swap` itself at both sides.
    #[test]
    fn capacity_is_the_largest_input_the_window_accepts() {
        let pool = v3(
            1,
            3_000,
            60,
            SQRT_ONE,
            &[(-600, 600, 1u128 << 80), (-6_000, 6_000, 1u128 << 80)],
        );
        let PoolState::V3(base) = &pool.state else {
            panic!()
        };
        let mut narrow = base.clone();
        narrow.window = Some((-120, 120));
        for zfo in [true, false] {
            let cap = v3_capacity_in(&narrow, zfo).unwrap();
            assert!(!cap.is_zero());
            assert!(v3_swap(&narrow, zfo, cap).is_ok(), "the capacity swaps");
            assert_eq!(
                v3_swap(&narrow, zfo, cap + U256::ONE).unwrap_err(),
                RouteError::InsufficientLiquidity,
                "one wei more is past the edge"
            );
        }
        assert_eq!(
            pool.capacity_in(0, 1),
            Some(v3_capacity_in(base, true).unwrap()),
            "the pool's own, zero-for-one"
        );
        let mut outside = base.clone();
        outside.window = Some((base.tick + 600, base.tick + 1_200));
        assert_eq!(v3_capacity_in(&outside, true).unwrap(), U256::ZERO);
    }

    #[test]
    fn a_seeded_window_bounds_the_swap() {
        // Two positions: a narrow one around the price and a wide one.
        let pool = v3(
            1,
            3_000,
            60,
            SQRT_ONE,
            &[(-600, 600, 1u128 << 80), (-6_000, 6_000, 1u128 << 80)],
        );
        let PoolState::V3(base) = &pool.state else {
            panic!()
        };
        // Big enough to cross tick -600 with the whole map known.
        let big = U256::from(1u128 << 76);
        let unbounded = v3_swap(base, true, big).unwrap();
        assert!(
            unbounded.tick < -600,
            "the swap crosses -600: {}",
            unbounded.tick
        );
        let mut narrow = base.clone();
        narrow.window = Some((-120, 120));
        assert_eq!(
            v3_swap(&narrow, true, big).unwrap_err(),
            RouteError::InsufficientLiquidity,
            "past the window's edge the liquidity is unknown"
        );
        let mut wide = base.clone();
        wide.window = Some((tick_math::MIN_TICK, tick_math::MAX_TICK));
        assert_eq!(
            v3_swap(&wide, true, U256::from(1_000u64))
                .ok()
                .map(|r| r.out),
            v3_swap(base, true, U256::from(1_000u64))
                .ok()
                .map(|r| r.out),
            "a window covering everything changes nothing"
        );
        let mut outside = base.clone();
        outside.window = Some((
            base.tick + 10 * base.tick_spacing,
            base.tick + 20 * base.tick_spacing,
        ));
        assert_eq!(
            v3_swap(&outside, true, U256::from(1_000u64)).unwrap_err(),
            RouteError::InsufficientLiquidity
        );
    }

    /// Oracle: the chain. The LAC/USDC V4 pool (LAC, USDC, fee 10_000,
    /// tick spacing 200, no hook) has id `0xa8f7d314…a45b` (its `Swap` log's
    /// topic 1, and `PositionManager.poolKeys` of that id's first 25 bytes
    /// returns this key). Its slot0 protocol fee 4_097_000 is 1_000 pips each
    /// way (`0x3e8 | 0x3e8 << 12`), so a swap charges 1_000 + 10_000 −
    /// 1_000 · 10_000 / 1e6 = 10_990 pips.
    #[test]
    fn v4_key_id_leg_data_and_swap_fee_match_the_chain() {
        let k = V4Key {
            currency0: alloy_primitives::address!("0df3a853e4b604fc2ac0881e9dc92db27ff7f51b"),
            currency1: alloy_primitives::address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
            fee: 10_000,
            tick_spacing: 200,
            hooks: Address::ZERO,
            id: B256::ZERO,
        };
        assert_eq!(
            k.compute_id().unwrap(),
            alloy_primitives::b256!(
                "a8f7d3148be7c6e66462f7d5da7843c94d974e0697e1768fb9c8b695f986a45b"
            )
        );
        let d = k.leg_data();
        assert_eq!(d.len(), 66);
        assert_eq!(&d[40..43], &[0x00, 0x27, 0x10]);
        assert_eq!(&d[43..46], &[0x00, 0x00, 0xc8]);
        assert_eq!(4_097_000u32 & 0xfff, 1_000);
        assert_eq!(4_097_000u32 >> 12, 1_000);
        assert_eq!(v4_swap_fee(1_000, 10_000), 10_990);
        assert_eq!(v4_swap_fee(0, 3_000), 3_000);
    }

    /// Oracle: closed enum. The venue set is exactly {V2, V3, Curve}; a
    /// Balancer pool has no representation and cannot be added. A Uniswap
    /// V4 pool is V3 state with its key (`V3State::v4`, decision 7 of the
    /// coverage plan), not a venue of its own.
    #[test]
    fn venue_set_is_closed_and_v4_is_v3_state() {
        const ALL: [Venue; 6] = [
            Venue::UniV2,
            Venue::UniV3,
            Venue::CurveStable,
            Venue::CurveCrypto,
            Venue::Balancer,
            Venue::Fluid,
        ];
        for v in ALL {
            // Exhaustive match: adding a variant is a compile error here.
            match v {
                Venue::UniV2
                | Venue::UniV3
                | Venue::CurveStable
                | Venue::CurveCrypto
                | Venue::Balancer
                | Venue::Fluid => (),
            }
        }
        let src = include_str!("solver.rs");
        let start = src.find("pub enum Venue {").unwrap();
        let body = &src[start..src[start..].find('}').unwrap() + start];
        assert!(!body.contains("UniV4"), "{body}");
        assert_eq!(body.matches(',').count(), 6);
    }

    /// Oracle: independent implementation. A full-range V3 position at
    /// price 1 is a constant-product pool with virtual reserves `(L, L)`
    /// and a 0.3 % fee — the V2 closed form must agree to rounding.
    #[test]
    fn v3_full_range_matches_v2_closed_form() {
        let l: u128 = 5_000_000_000_000_000_000_000; // 5000e18
        let p3 = v3(1, 3000, 60, SQRT_ONE, &[(-887_220, 887_220, l)]);
        let p2 = v2(2, U256::from(l), U256::from(l));
        for dx in [
            1u128,
            1_000,
            1_000_000_000,
            1_000_000_000_000_000_000,
            40_000_000_000_000_000_000,
        ] {
            let dx = U256::from(dx);
            for (i, j) in [(0u8, 1u8), (1, 0)] {
                let a = p3.quote_exact_in(i, j, dx).unwrap();
                let b = p2.quote_exact_in(i, j, dx).unwrap();
                assert!(abs_diff(a, b) <= U256::from(3u64), "dx={dx} v3={a} v2={b}");
            }
        }
    }

    /// Oracle: math invariant. Splitting one range into two adjacent
    /// ranges of the same liquidity changes nothing but the number of
    /// steps: output equal to per-step rounding; liquidity unchanged after
    /// the crossing; tick bookkeeping identical to the contract's
    /// `tickNext − 1` rule.
    #[test]
    fn v3_tick_crossing_conserves_output() {
        let l: u128 = 1_000_000_000_000_000_000_000;
        let one = v3(1, 3000, 60, SQRT_ONE, &[(-6000, 6000, l)]);
        let split = v3(2, 3000, 60, SQRT_ONE, &[(-6000, -600, l), (-600, 6000, l)]);
        let dx = e18(40); // 1/sqrtP: 1 → 1.04, tick ≈ −784: crosses −600 zero-for-one
        let a = one.quote_exact_in(0, 1, dx).unwrap();
        let b = split.quote_exact_in(0, 1, dx).unwrap();
        assert!(abs_diff(a, b) <= U256::from(3u64), "{a} vs {b}");
        let mut s = split.clone();
        s.apply_exact_in(0, 1, dx).unwrap();
        let PoolState::V3(st) = &s.state else {
            unreachable!()
        };
        assert_eq!(st.liquidity, l);
        assert!(st.tick < -600);
        assert_eq!(
            st.tick,
            tick_math::get_tick_at_sqrt_ratio(st.sqrt_price_x96).unwrap()
        );
    }

    /// Oracle: monotone in depth. More liquidity beyond the crossed tick
    /// yields more output than less, bounded by the pool with that
    /// liquidity everywhere.
    #[test]
    fn v3_liquidity_jump_bounds() {
        let l: u128 = 1_000_000_000_000_000_000_000;
        let thin = v3(1, 3000, 60, SQRT_ONE, &[(-6000, 6000, l)]);
        let jump = v3(2, 3000, 60, SQRT_ONE, &[(-6000, 6000, l), (-6000, -600, l)]);
        let thick = v3(3, 3000, 60, SQRT_ONE, &[(-6000, 6000, 2 * l)]);
        let dx = e18(40);
        let (a, b, c) = (
            thin.quote_exact_in(0, 1, dx).unwrap(),
            jump.quote_exact_in(0, 1, dx).unwrap(),
            thick.quote_exact_in(0, 1, dx).unwrap(),
        );
        assert!(a < b && b < c, "{a} {b} {c}");
    }

    /// Oracle: the contract reverts (`SPL`) when input outlives range
    /// liquidity; we refuse rather than quote a partial fill.
    #[test]
    fn v3_refuses_beyond_depth() {
        let p = v3(
            1,
            3000,
            60,
            SQRT_ONE,
            &[(-600, 600, 1_000_000_000_000_000_000)],
        );
        assert_eq!(
            p.quote_exact_in(0, 1, e18(1_000)),
            Err(RouteError::InsufficientLiquidity)
        );
        assert!(p.quote_exact_in(0, 1, U256::from(1_000_000u64)).is_ok());
    }

    /// Oracle: `getAmountOut` closed form and reserve update.
    #[test]
    fn v2_get_amount_out_and_update() {
        let mut p = v2(1, e18(1_000), e18(2_000));
        let dx = e18(10);
        let out = p.apply_exact_in(0, 1, dx).unwrap();
        // 997·10·2000 / (1000·1000 + 997·10) = 19_940_000 / 1_009_970 …
        let expect = U256::from(997u64) * dx * e18(2_000)
            / (e18(1_000) * U256::from(1000u64) + U256::from(997u64) * dx);
        assert_eq!(out, expect);
        let PoolState::V2(s) = p.state else {
            unreachable!()
        };
        assert_eq!(s.reserve0, e18(1_010));
        assert_eq!(s.reserve1, e18(2_000) - out);
    }

    /// Oracles for the StableSwap port: (1) `D = S` on a balanced pool;
    /// (2) balanced small swap returns `dx·(1 − fee)` within 1 unit of the
    /// `−1` rounding; (3) the round trip loses only fees; (4) with zero
    /// fee the invariant `D` is preserved by `exchange` to Newton's ±1.
    /// StableSwap-NG against the Python port in
    /// `tools/registry/discover_exits.py`, which reproduced `get_dy` on all
    /// 184 NG pools it admitted. Oracle-style rates, a dynamic fee.
    #[test]
    fn ng_math_matches_the_port_that_matched_every_pool() {
        let u = |v: u128| U256::from(v);
        let s = CurveState {
            balances: SmallVec::from_slice(&[
                u(2_833_345_070_697_224_937_308),
                u(2_187_051_300_082_150_214_224),
            ]),
            rates: SmallVec::from_slice(&[
                u(1_050_000_000_000_000_000),
                u(1_120_000_000_000_000_000),
            ]),
            a: u(20_000),
            a_precision: u(100),
            fee: u(2_000_000),
            stale: false,
            stale_block: 0,
            ng: true,
            d_once: true,
            offpeg_fee_multiplier: u(50_000_000_000),
            dynamic_rates: true,
            read_block: 0,
            handler: 0,
        };
        let xp = curve_xp(&s).unwrap();
        assert_eq!(
            curve_get_d(&s, &xp).unwrap(),
            u(5_424_381_945_823_831_959_820)
        );
        assert_eq!(
            ng_dynamic_fee(xp[0], xp[1], s.fee, s.offpeg_fee_multiplier).unwrap(),
            u(2_015_130)
        );
        for (dx, fwd, back) in [
            (
                1_000_000_000_000_000_000u128,
                936_389_043_014_965_153u128,
                1_067_497_336_108_435_281u128,
            ),
            (
                500_000_000_000_000_000_000,
                467_660_455_744_522_591_678,
                533_192_225_542_925_994_082,
            ),
        ] {
            assert_eq!(curve_dy(&s, 0, 1, u(dx)).unwrap().0, u(fwd));
            assert_eq!(curve_dy(&s, 1, 0, u(dx)).unwrap().0, u(back));
        }
    }

    /// The crvUSD stableswap factory's plain pools (`0x67fe…4286`, Vyper
    /// 0.3.7): plain rates and a static fee, but `get_D` divides by `N^N`
    /// once, as NG does (`d_once`). Oracle: USDT/crvUSD `0x390f…7bf4`'s own
    /// `get_dy` at block 26,147,445 on its state then, both directions, two
    /// sizes. (The two `D` forms agree at these sizes and differ at others:
    /// discovery's gate, every pair at two sizes on the pool's own block,
    /// is where the per-coin form was refused for this pool.)
    #[test]
    fn crvusd_factory_plain_pool_divides_d_by_n_pow_n_once() {
        let u = |v: u128| U256::from(v);
        let s = CurveState {
            balances: SmallVec::from_slice(&[
                u(17_429_362_443_732),
                u(16_759_982_797_341_170_977_388_677),
            ]),
            rates: SmallVec::from_slice(&[
                u(1_000_000_000_000_000_000_000_000_000_000),
                u(1_000_000_000_000_000_000),
            ]),
            a: u(200_000),
            a_precision: u(100),
            fee: u(1_000_000),
            stale: false,
            stale_block: 0,
            ng: false,
            d_once: true,
            offpeg_fee_multiplier: U256::ZERO,
            dynamic_rates: false,
            read_block: 0,
            handler: 4,
        };
        for (i, j, dx, want) in [
            (0usize, 1usize, 1_000_000u128, 999_880_418_303_581_206u128),
            (0, 1, 100_000_000_000, 99_987_748_712_986_498_566_232),
            (1, 0, 1_000_000_000_000_000_000, 999_919),
            (1, 0, 100_000_000_000_000_000_000_000, 99_991_665_335),
        ] {
            assert_eq!(
                curve_dy(&s, i, j, u(dx)).unwrap().0,
                u(want),
                "{i}->{j} {dx}"
            );
        }
    }

    #[test]
    fn curve_invariants() {
        let bal = [e18(10_000_000), e18(10_000_000), e18(10_000_000)];
        let p = curve(1, &bal, 2000 * 100, 1_000_000); // A = 2000, fee 1 bp
        let PoolState::Curve(s) = &p.state else {
            unreachable!()
        };
        let xp = curve_xp(s).unwrap();
        assert_eq!(curve_get_d(s, &xp).unwrap(), e18(30_000_000));
        let dx = e18(1_000);
        let out = p.quote_exact_in(0, 1, dx).unwrap();
        let ideal = dx - dx * U256::from(1_000_000u64) / CURVE_FEE_DENOM;
        assert!(
            out <= ideal && ideal - out < e18(1) / U256::from(1_000u64),
            "{out} vs {ideal}"
        );
        let mut q = p.clone();
        let got = q.apply_exact_in(0, 1, dx).unwrap();
        let back = q.apply_exact_in(1, 0, got).unwrap();
        assert!(back < dx && dx - back < dx * U256::from(3u64) / U256::from(10_000u64));

        let mut z = curve(2, &bal, 2000 * 100, 0);
        let PoolState::Curve(s0) = &z.state else {
            unreachable!()
        };
        let d0 = curve_get_d(s0, &curve_xp(s0).unwrap()).unwrap();
        z.apply_exact_in(0, 2, e18(500_000)).unwrap();
        let PoolState::Curve(s1) = &z.state else {
            unreachable!()
        };
        let d1 = curve_get_d(s1, &curve_xp(s1).unwrap()).unwrap();
        // exchange subtracts `dy + 1` in xp units (the `−1`), so D may
        // grow by that unit's worth; never shrink.
        assert!(d1.abs_diff(d0) <= U256::from(4u64), "{d0} {d1}");
    }

    /// Oracle: `ρ(0)²` is the post-fee marginal price. V2 at reserves
    /// `(r, 2r)` zero-for-one → `0.997 · 2`; V3 at price 1 with 3000 pips
    /// → `0.997`; Curve balanced → `1 − fee`. Checked to 1e-9 relative.
    #[test]
    fn rho_at_zero_is_post_fee_marginal() {
        let scale = U256::from(1_000_000_000u64);
        // m · 1e9 = ρ² · 1e9 / 2^192
        let m9 = |r: U256| mul_div_512(mul_div_512(r, r, Q96).unwrap(), scale, Q96).unwrap();
        let tol = U256::from(2u64);
        let r2 = v2(1, e18(1_000), e18(2_000)).rho_at_zero(0, 1).unwrap();
        assert!(
            m9(r2).abs_diff(U256::from(1_994_000_000u64)) <= tol,
            "{}",
            m9(r2)
        );
        let r2b = v2(1, e18(1_000), e18(2_000)).rho_at_zero(1, 0).unwrap();
        assert!(
            m9(r2b).abs_diff(U256::from(498_500_000u64)) <= tol,
            "{}",
            m9(r2b)
        );
        let r3 = v3(2, 3000, 60, SQRT_ONE, &[(-600, 600, 1u128 << 100)])
            .rho_at_zero(0, 1)
            .unwrap();
        assert!(
            m9(r3).abs_diff(U256::from(997_000_000u64)) <= tol,
            "{}",
            m9(r3)
        );
        let r3b = v3(2, 3000, 60, SQRT_ONE, &[(-600, 600, 1u128 << 100)])
            .rho_at_zero(1, 0)
            .unwrap();
        assert!(
            m9(r3b).abs_diff(U256::from(997_000_000u64)) <= tol,
            "{}",
            m9(r3b)
        );
        let rc = curve(3, &[e18(1_000_000), e18(1_000_000)], 100 * 100, 4_000_000)
            .rho_at_zero(0, 1)
            .unwrap();
        assert!(
            m9(rc).abs_diff(U256::from(999_600_000u64)) <= tol,
            "{}",
            m9(rc)
        );
    }

    /// Oracle: `nextInitializedTickWithinOneWord` semantics — the next
    /// initialized tick in direction, else the word edge un-initialized.
    #[test]
    fn next_tick_word_semantics() {
        let ticks = [
            Tick {
                tick: -600,
                net: 1,
                gross: 1,
            },
            Tick {
                tick: 0,
                net: 1,
                gross: 1,
            },
            Tick {
                tick: 600,
                net: 1,
                gross: 1,
            },
        ];
        assert_eq!(next_tick_within_word(&ticks, 0, 60, true), (0, true)); // lte includes current
        assert_eq!(next_tick_within_word(&ticks, -1, 60, true), (-600, true));
        assert_eq!(next_tick_within_word(&ticks, 0, 60, false), (600, true));
        assert!(!next_tick_within_word(&ticks, 600, 60, false).1);
        // word edge: compressed 10 (tick 600) → word 0 covers compressed 0..255
        assert_eq!(
            next_tick_within_word(&ticks, 600, 60, false),
            (255 * 60, false)
        );
        assert_eq!(
            next_tick_within_word(&ticks, -601, 60, true),
            (-256 * 60, false)
        );
    }

    fn book_with(pools: Vec<Pool>) -> PoolBook {
        let mut assets = std::collections::HashMap::new();
        assets.insert(tok(0), A0);
        assets.insert(tok(1), A1);
        let mut b = PoolBook::new(assets, Some(addr(0xFAC)), HOP_GAS);
        for p in pools {
            b.add(p).unwrap();
        }
        b
    }

    /// A pair added while running is not live until seeded; a seed read
    /// older than a `Sync` already folded in is refused (the `Sync` set the
    /// reserves exactly), and a later one applies once.
    #[test]
    fn runtime_v2_seed_is_refused_behind_a_newer_sync() {
        let mut book = book_with(vec![]);
        let mut p = v2(9, U256::ZERO, U256::ZERO);
        p.address = addr(9);
        let id = book.add_pending_v2(p).unwrap();
        assert!(!book.get(id).unwrap().is_live());
        assert_eq!(book.pending_v2().collect::<Vec<_>>(), vec![id]);
        // A Sync at block 50 lands before the seed read at 40.
        let log = v2_sync_log(addr(9), e18(5), e18(7));
        let mut d = log.decoded();
        d.block = 50;
        book.apply_log(&d);
        assert!(
            !book.seed_pending_v2(id, e18(1), e18(1), 40),
            "older read refused"
        );
        assert!(book.pending_v2().next().is_none(), "no longer pending");
        let PoolState::V2(s) = &book.get(id).unwrap().state else {
            panic!()
        };
        assert_eq!((s.reserve0, s.reserve1), (e18(5), e18(7)));
        // A second pair with no Sync takes its seed.
        let mut q = v2(8, U256::ZERO, U256::ZERO);
        q.address = addr(8);
        let id2 = book.add_pending_v2(q).unwrap();
        assert!(book.seed_pending_v2(id2, e18(3), e18(4), 60));
        assert!(!book.seed_pending_v2(id2, e18(9), e18(9), 61), "seeds once");
        assert!(book.get(id2).unwrap().is_live());
        // A Curve pool added at runtime is forced stale at a block.
        assert!(!book.mark_stale(id2, 70), "V2 has no stale flag");
    }

    /// A V3 pool of the PancakeSwap factory emits a Swap with two more fields,
    /// so a different topic0: its logs fold from that event, and a Uniswap
    /// Swap does not move it (nor a Pancake one a Uniswap pool). Oracle:
    /// real swaps recorded from two live Pancake pools
    /// (`tools/registry/pancake_swap_fixture.py`), each the last event of its
    /// block, so the pool's own `slot0()` and `liquidity()` at that block are
    /// the state the fold must reach. The subscriptions follow the factory.
    #[test]
    fn a_pancake_pool_folds_pancake_swaps_and_only_those() {
        use alloy_primitives::{Bytes, B256};
        let doc: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/pancake_swap_logs.json")).unwrap();
        assert_eq!(
            doc["swap_topic"].as_str().unwrap().parse::<B256>().unwrap(),
            IPancakeV3Pool::Swap::SIGNATURE_HASH,
            "the recorded topic is the event this crate decodes"
        );
        let logs = doc["logs"].as_array().unwrap();
        assert!(logs.len() >= 4, "several real swaps");
        for rec in logs {
            let topics: Vec<B256> = rec["topics"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_str().unwrap().parse().unwrap())
                .collect();
            let data: Bytes = rec["data"].as_str().unwrap().parse().unwrap();
            let pool_addr: Address = rec["pool"].as_str().unwrap().parse().unwrap();
            let want_price: U256 = rec["slot0_sqrt_price_x96"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap();
            let want_tick = i32::try_from(rec["slot0_tick"].as_i64().unwrap()).unwrap();
            let want_liq: u128 = rec["liquidity"].as_str().unwrap().parse().unwrap();

            let mut pool = v3(1, 500, 10, SQRT_ONE, &[(-887_220, 887_220, 7)]);
            pool.address = pool_addr;
            let PoolState::V3(st) = &mut pool.state else {
                unreachable!()
            };
            st.factory = V3_FACTORY_PANCAKE;
            let mut book = book_with(vec![pool]);
            let log = DecodedLog {
                address: pool_addr,
                topics: &topics,
                data: &data,
                block: 0,
                timestamp: 0,
            };
            let g0 = book.generation();
            book.apply_log(&log);
            assert_ne!(book.generation(), g0, "the fold changed the book");
            let PoolState::V3(st) = &book.get(PoolId(0)).unwrap().state else {
                unreachable!()
            };
            assert_eq!(
                (st.sqrt_price_x96, st.tick, st.liquidity),
                (want_price, want_tick, want_liq),
                "block {}",
                rec["block"]
            );

            // The same bytes at a Uniswap-factory pool are no Swap it knows.
            let mut uni = v3(1, 500, 10, SQRT_ONE, &[(-887_220, 887_220, 7)]);
            uni.address = pool_addr;
            let mut ubook = book_with(vec![uni]);
            let before = ubook.generation();
            ubook.apply_log(&log);
            assert_eq!(ubook.generation(), before, "a Uniswap pool ignores it");
        }
        // A Uniswap-shaped Swap does not move a Pancake pool.
        let mut pool = v3(1, 500, 10, SQRT_ONE, &[(-887_220, 887_220, 7)]);
        let PoolState::V3(st) = &mut pool.state else {
            unreachable!()
        };
        st.factory = V3_FACTORY_PANCAKE;
        let mut book = book_with(vec![pool]);
        let g0 = book.generation();
        book.apply_log(&v3_swap_log(addr(1), sqrt_at(120), 7, 120).decoded());
        assert_eq!(book.generation(), g0);

        // Subscriptions: each pool listens for its own factory's Swap.
        let mut uni = v3(2, 500, 10, SQRT_ONE, &[(-887_220, 887_220, 7)]);
        uni.address = addr(2);
        let mut cake = v3(3, 500, 10, SQRT_ONE, &[(-887_220, 887_220, 7)]);
        cake.address = addr(3);
        if let PoolState::V3(st) = &mut cake.state {
            st.factory = V3_FACTORY_PANCAKE;
        }
        let book = book_with(vec![uni, cake]);
        let subs = book.subscriptions();
        let topics_of = |a: Address| -> Vec<B256> {
            subs.iter()
                .filter(|f| f.address == a)
                .map(|f| f.topic0)
                .collect()
        };
        assert!(topics_of(addr(2)).contains(&IUniswapV3Pool::Swap::SIGNATURE_HASH));
        assert!(!topics_of(addr(2)).contains(&IPancakeV3Pool::Swap::SIGNATURE_HASH));
        assert!(topics_of(addr(3)).contains(&IPancakeV3Pool::Swap::SIGNATURE_HASH));
        assert!(!topics_of(addr(3)).contains(&IUniswapV3Pool::Swap::SIGNATURE_HASH));
        // Mint, Burn and Initialize are the same events on both.
        for t in [
            IUniswapV3Pool::Mint::SIGNATURE_HASH,
            IUniswapV3Pool::Burn::SIGNATURE_HASH,
            IUniswapV3Pool::Initialize::SIGNATURE_HASH,
        ] {
            assert!(topics_of(addr(2)).contains(&t) && topics_of(addr(3)).contains(&t));
        }
    }

    /// A Fluid pool folds the Liquidity layer's `LogOperate`s of its tokens:
    /// another user's operation sets the token's totals and exchange prices
    /// to the event's words and moves the layer's balance by supply minus
    /// borrow, while the pool's own operation marks it for a re-read.
    /// Oracle: real events (`tools/registry/fluid_operate_fixture.py`), each
    /// the only operation on its token in its block: the chain's storage
    /// words after the block equal the event's, and the layer's token
    /// balance moved by exactly supply minus borrow.
    #[test]
    fn a_fluid_pool_folds_liquidity_operates_exactly() {
        use alloy_primitives::Bytes;
        let doc: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/fluid_operate_logs.json"))
                .unwrap();
        let samples = doc["samples"].as_array().unwrap();
        assert!(samples.len() >= 3);
        let to_u = |v: &serde_json::Value| -> U256 { v.as_str().unwrap().parse().unwrap() };
        for rec in samples {
            let token: Address = rec["token"].as_str().unwrap().parse().unwrap();
            let topics: Vec<B256> = rec["topics"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_str().unwrap().parse().unwrap())
                .collect();
            let data: Bytes = rec["data"].as_str().unwrap().parse().unwrap();
            let pool_addr = addr(0x77);
            let other = addr(0x78);
            let mut p = v3(1, 500, 10, SQRT_ONE, &[(-887_220, 887_220, 7)]);
            p.address = pool_addr;
            p.tokens = SmallVec::from_slice(&[token, other]);
            p.state = PoolState::Fluid(FluidState {
                prec: [U256::ONE; 4],
                tokens: SmallVec::from_slice(&[token, other]),
                native: [false; 2],
                deployer: Address::ZERO,
                dex_vars: U256::ZERO,
                dex_vars2: U256::ZERO,
                center_ext: None,
                liq: [
                    crate::fluid::LiqToken {
                        ep_cfg: to_u(&rec["ep_cfg_before"]),
                        totals: to_u(&rec["totals_before"]),
                        balance: to_u(&rec["balance_before"]),
                        ..Default::default()
                    },
                    crate::fluid::LiqToken::default(),
                ],
                exec_ts: 0,
                stale: false,
                stale_block: 0,
                read_block: 0,
            });
            let mut book = PoolBook::new(HashMap::new(), None, 0);
            book.add(p).unwrap();
            let log = DecodedLog {
                address: FLUID_LIQUIDITY,
                topics: &topics,
                data: &data,
                block: rec["block"].as_u64().unwrap(),
                timestamp: 0,
            };
            let g0 = book.generation();
            book.apply_log(&log);
            assert_ne!(
                book.generation(),
                g0,
                "{}: the fold changed the book",
                rec["kind"]
            );
            let PoolState::Fluid(f) = &book.get(PoolId(0)).unwrap().state else {
                unreachable!()
            };
            assert!(
                !f.stale,
                "another user's operation leaves the pool readable"
            );
            assert_eq!(
                f.liq[0].ep_cfg,
                to_u(&rec["ep_cfg_after"]),
                "{}",
                rec["kind"]
            );
            assert_eq!(
                f.liq[0].totals,
                to_u(&rec["totals_after"]),
                "{}",
                rec["kind"]
            );
            assert_eq!(
                f.liq[0].balance,
                to_u(&rec["balance_after"]),
                "{}",
                rec["kind"]
            );
            // The pool's own operation: its positions changed, read again.
            let mut own = topics.clone();
            own[1] = B256::left_padding_from(pool_addr.as_slice());
            book.apply_log(&DecodedLog {
                topics: &own,
                ..log
            });
            let PoolState::Fluid(f) = &book.get(PoolId(0)).unwrap().state else {
                unreachable!()
            };
            assert!(f.stale && f.stale_block == log.block);
            // Not the Liquidity layer's: nothing.
            let mut book2 = PoolBook::new(HashMap::new(), None, 0);
            let mut p2 = book.get(PoolId(0)).unwrap().clone();
            if let PoolState::Fluid(f) = &mut p2.state {
                f.stale = false;
            }
            book2.add(p2).unwrap();
            let g = book2.generation();
            book2.apply_log(&DecodedLog {
                address: addr(9),
                ..log
            });
            assert_eq!(book2.generation(), g);
        }
        // The book subscribes to the layer once any Fluid pool is in it.
        let mut book = PoolBook::new(HashMap::new(), None, 0);
        let mut p = v3(1, 500, 10, SQRT_ONE, &[(-887_220, 887_220, 7)]);
        p.address = addr(0x79);
        p.tokens = SmallVec::from_slice(&[addr(1), addr(2)]);
        p.state = PoolState::Fluid(FluidState {
            prec: [U256::ONE; 4],
            tokens: SmallVec::from_slice(&[addr(1), addr(2)]),
            native: [false; 2],
            deployer: Address::ZERO,
            dex_vars: U256::ZERO,
            dex_vars2: U256::ZERO,
            center_ext: None,
            liq: [crate::fluid::LiqToken::default(); 2],
            exec_ts: 0,
            stale: true,
            stale_block: 0,
            read_block: 0,
        });
        book.add(p).unwrap();
        assert!(book
            .subscriptions()
            .iter()
            .any(|f| f.address == FLUID_LIQUIDITY
                && f.topic0 == IFluidLiquidity::LogOperate::SIGNATURE_HASH));
    }

    /// A Balancer pool follows the Vault's logs: a `Swap` moves its balances
    /// by exactly its two amounts; a join, an exit or an asset manager's move
    /// marks it for a re-read. Oracle: real logs of the four reviewed pools
    /// (`tools/registry/balancer_swap_fixture.py`), each a block in which the
    /// Vault logged exactly one event for the pool: the balances before the
    /// block plus the swap give the Vault's own balances after it. A log for
    /// another pool id does nothing, and the book subscribes to the Vault.
    #[test]
    fn a_balancer_pool_folds_vault_swaps_exactly() {
        use alloy_primitives::Bytes;
        let doc: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/balancer_swap_logs.json"))
                .unwrap();
        let recs = doc["pools"].as_array().unwrap();
        assert!(recs.len() >= 4);
        let to_u = |v: &serde_json::Value| -> U256 { v.as_str().unwrap().parse().unwrap() };
        let log_of = |l: &serde_json::Value| -> (Vec<B256>, Bytes) {
            (
                l["topics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t.as_str().unwrap().parse().unwrap())
                    .collect(),
                l["data"].as_str().unwrap().parse().unwrap(),
            )
        };
        let tokens_of = |rec: &serde_json::Value| -> Vec<Address> {
            rec["tokens"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_str().unwrap().parse().unwrap())
                .collect()
        };
        let mk = |rec: &serde_json::Value, bal: &[U256]| {
            let tokens = tokens_of(rec);
            let mut p = v3(1, 500, 10, SQRT_ONE, &[(-887_220, 887_220, 7)]);
            p.address = rec["pool"].as_str().unwrap().parse().unwrap();
            p.tokens = tokens.iter().copied().collect();
            p.state = PoolState::Balancer(BalancerState {
                pool_id: rec["pool_id"].as_str().unwrap().parse().unwrap(),
                tokens: tokens.iter().copied().collect(),
                balances: bal.iter().copied().collect(),
                weights: SmallVec::from_slice(&[WAD / U256::from(2u64), WAD / U256::from(2u64)]),
                scaling: SmallVec::from_slice(&[U256::ONE, U256::ONE]),
                swap_fee: U256::ZERO,
                fast_pow: false,
                stale: false,
                stale_block: 0,
                read_block: 0,
            });
            p
        };
        let (mut swaps, mut changes) = (0, 0);
        for rec in recs {
            if let Some(sw) = rec.get("swap") {
                swaps += 1;
                let before: Vec<U256> = sw["before"].as_array().unwrap().iter().map(to_u).collect();
                let after: Vec<U256> = sw["after"].as_array().unwrap().iter().map(to_u).collect();
                let mut book = PoolBook::new(HashMap::new(), None, 0);
                book.add(mk(rec, &before)).unwrap();
                let (topics, data) = log_of(sw);
                let log = DecodedLog {
                    address: BALANCER_VAULT,
                    topics: &topics,
                    data: &data,
                    block: sw["block"].as_u64().unwrap(),
                    timestamp: 0,
                };
                let g0 = book.generation();
                book.apply_log(&log);
                assert_ne!(book.generation(), g0);
                let PoolState::Balancer(b) = &book.get(PoolId(0)).unwrap().state else {
                    unreachable!()
                };
                assert!(!b.stale, "a swap folds, it does not stale the pool");
                assert_eq!(
                    b.balances.to_vec(),
                    after,
                    "{} block {}",
                    rec["pool"],
                    sw["block"]
                );
                // The same bytes under another pool id move nothing.
                let mut other_topics = topics.clone();
                other_topics[1] = B256::repeat_byte(0xAB);
                let other = DecodedLog {
                    topics: &other_topics,
                    ..log
                };
                let g1 = book.generation();
                book.apply_log(&other);
                assert_eq!(book.generation(), g1);
                // Not the Vault's: nothing.
                book.apply_log(&DecodedLog {
                    address: addr(9),
                    ..log
                });
                let PoolState::Balancer(b) = &book.get(PoolId(0)).unwrap().state else {
                    unreachable!()
                };
                assert_eq!(b.balances.to_vec(), after);
            }
            if let Some(ch) = rec.get("change") {
                changes += 1;
                let mut book = PoolBook::new(HashMap::new(), None, 0);
                book.add(mk(rec, &[WAD, WAD])).unwrap();
                let (topics, data) = log_of(ch);
                book.apply_log(&DecodedLog {
                    address: BALANCER_VAULT,
                    topics: &topics,
                    data: &data,
                    block: ch["block"].as_u64().unwrap(),
                    timestamp: 0,
                });
                let PoolState::Balancer(b) = &book.get(PoolId(0)).unwrap().state else {
                    unreachable!()
                };
                assert!(b.stale && b.stale_block == ch["block"].as_u64().unwrap());
                assert!(
                    !book.get(PoolId(0)).unwrap().is_live(),
                    "re-read before it is quoted"
                );
            }
        }
        assert!(swaps >= 4, "a real swap for each pool: {swaps}");
        assert!(changes >= 1, "a real join or exit: {changes}");
        // Subscriptions: the Vault's three events, and the pool's two.
        let mut book = PoolBook::new(HashMap::new(), None, 0);
        book.add(mk(&recs[0], &[WAD, WAD])).unwrap();
        let subs = book.subscriptions();
        let vault: Vec<B256> = subs
            .iter()
            .filter(|f| f.address == BALANCER_VAULT)
            .map(|f| f.topic0)
            .collect();
        assert_eq!(vault.len(), 3);
        for t in [
            IBalancerVault::Swap::SIGNATURE_HASH,
            IBalancerVault::PoolBalanceChanged::SIGNATURE_HASH,
            IBalancerVault::PoolBalanceManaged::SIGNATURE_HASH,
        ] {
            assert!(vault.contains(&t));
        }
        let pool_addr = book.pools()[0].address;
        let own = subs.iter().filter(|f| f.address == pool_addr).count();
        assert_eq!(own, 2, "the pool's fee and pause logs");
    }

    /// Oracle: log folds reproduce the state a direct read would show —
    /// `Swap` sets slot0 + liquidity; `Mint`/`Burn` edit ticks and active
    /// liquidity exactly as `_modifyPosition`; `Sync` sets reserves; any
    /// Curve log marks stale; a malformed body fails closed.
    #[test]
    fn folds_are_exact_and_fail_closed() {
        let l: u128 = 1_000_000_000_000_000_000;
        let mut book = book_with(vec![
            v3(1, 3000, 60, SQRT_ONE, &[(-600, 600, l)]),
            v2(2, e18(10), e18(10)),
            curve(3, &[e18(1), e18(1)], 100 * 100, 0),
        ]);
        let g0 = book.generation();
        let s2 = sqrt_at(120);
        book.apply_log(&v3_swap_log(addr(1), s2, l, 120).decoded());
        let PoolState::V3(st) = &book.get(PoolId(0)).unwrap().state else {
            unreachable!()
        };
        assert_eq!((st.sqrt_price_x96, st.tick, st.liquidity), (s2, 120, l));
        // Mint a range containing the current tick, then burn half of it.
        book.apply_log(&v3_mint_log(addr(1), -60, 180, 2 * l).decoded());
        let PoolState::V3(st) = &book.get(PoolId(0)).unwrap().state else {
            unreachable!()
        };
        assert_eq!(st.liquidity, 3 * l);
        assert_eq!(
            st.ticks.iter().find(|t| t.tick == 180).unwrap().net,
            -i128::try_from(2 * l).unwrap()
        );
        book.apply_log(&v3_burn_log(addr(1), -60, 180, l).decoded());
        let PoolState::V3(st) = &book.get(PoolId(0)).unwrap().state else {
            unreachable!()
        };
        assert_eq!(st.liquidity, 2 * l);
        assert_eq!(st.ticks.iter().find(|t| t.tick == -60).unwrap().gross, l);
        // Burn the rest: ticks disappear (bitmap flip at gross 0).
        book.apply_log(&v3_burn_log(addr(1), -60, 180, l).decoded());
        let PoolState::V3(st) = &book.get(PoolId(0)).unwrap().state else {
            unreachable!()
        };
        assert!(st.ticks.iter().all(|t| t.tick != -60 && t.tick != 180));
        assert_eq!(st.liquidity, l);
        // Over-burn is a fold defect → fail closed.
        book.apply_log(&v3_burn_log(addr(1), -600, 600, 2 * l).decoded());
        assert!(!book.get(PoolId(0)).unwrap().is_live());

        book.apply_log(&v2_sync_log(addr(2), e18(7), e18(9)).decoded());
        let PoolState::V2(s) = book.get(PoolId(1)).unwrap().state else {
            unreachable!()
        };
        assert_eq!((s.reserve0, s.reserve1), (e18(7), e18(9)));

        assert!(book.get(PoolId(2)).unwrap().is_live());
        book.apply_log(&curve_exchange_log(addr(3)).decoded());
        assert!(!book.get(PoolId(2)).unwrap().is_live());
        assert!(book.generation() > g0);

        // Malformed V3 body → excluded, not guessed.
        let mut bad = v3_swap_log(addr(1), s2, l, 120);
        bad.data = alloy_primitives::LogData::new_unchecked(
            bad.data.topics().to_vec(),
            alloy_primitives::Bytes::from(vec![1u8, 2, 3]),
        );
        let mut book2 = book_with(vec![v3(1, 3000, 60, SQRT_ONE, &[(-600, 600, l)])]);
        book2.apply_log(&bad.decoded());
        assert!(!book2.get(PoolId(0)).unwrap().is_live());
    }

    /// Oracle: discovery admits only pools whose both tokens are tracked,
    /// and the new pool is unquotable until its own `Initialize` arrives.
    #[test]
    fn discovery_from_pool_created() {
        let mut book = book_with(vec![]);
        let f = addr(0xFAC);
        book.apply_log(&pool_created_log(f, tok(0), tok(9), 500, 10, addr(50)).decoded());
        assert_eq!(book.pools().len(), 0);
        book.apply_log(&pool_created_log(f, tok(0), tok(1), 500, 10, addr(51)).decoded());
        assert_eq!(book.pools().len(), 1);
        assert_eq!(book.discovered(), 1);
        let id = book.by_address(addr(51)).unwrap();
        assert!(!book.get(id).unwrap().is_live());
        assert_eq!(book.legs(A0, A1).len(), 1);
        assert_eq!(
            book.legs(A1, A0),
            &[Leg {
                pool: id,
                i: 1,
                j: 0
            }]
        );
        let subs = book.subscriptions();
        assert!(subs
            .iter()
            .any(|s| s.address == f && s.topic0 == IUniswapV3Factory::PoolCreated::SIGNATURE_HASH));
        assert_eq!(subs.iter().filter(|s| s.address == addr(51)).count(), 4);
        // Duplicate address is rejected.
        assert_eq!(book.add(v2(51, e18(1), e18(1))), Err(RouteError::BadLeg));
        let _ = Address::ZERO;
        let _ = AssetId(0);
    }
}
