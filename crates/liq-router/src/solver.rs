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

use crate::crypto::CryptoState;

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

/// Swap venue family. **No `UniV4`, no `Balancer`** — by construction.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Venue {
    UniV2,
    UniV3,
    CurveStable,
    CurveCrypto,
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

/// Uniswap V3 pool state — exactly what `swap()` reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V3State {
    pub sqrt_price_x96: U256,
    pub tick: i32,
    pub liquidity: u128,
    pub fee_pips: u32,
    pub tick_spacing: i32,
    /// Initialized ticks, ascending by `tick`.
    pub ticks: Vec<Tick>,
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
    /// NG `offpeg_fee_multiplier()` (1e10). At or below 1e10 the fee is
    /// flat. 0 on plain pools.
    pub offpeg_fee_multiplier: U256,
    /// NG pool with an oracle or ERC-4626 rate: `stored_rates()` moves
    /// without a pool log, so the reseed thread re-reads it every block.
    pub dynamic_rates: bool,
    /// Block of the last applied read.
    pub read_block: u64,
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
    #[inline]
    #[must_use]
    pub fn venue(&self) -> Venue {
        match self.state {
            PoolState::V2(_) => Venue::UniV2,
            PoolState::V3(_) => Venue::UniV3,
            PoolState::Curve(_) => Venue::CurveStable,
            PoolState::Crypto(_) => Venue::CurveCrypto,
        }
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
        }
    }

    /// Exact output for `amount_in` of coin `i` into coin `j`, as the
    /// contract would compute it against this state. Allocation-free.
    pub fn quote_exact_in(&self, i: u8, j: u8, amount_in: U256) -> Result<U256, RouteError> {
        self.state.quote(i, j, amount_in)
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
            PoolState::Crypto(s) => {
                // The pool's `tweak_price` moves `D` and `price_scale` after
                // the swap, which this model does not follow: a later leg of
                // the same plan must not quote this pool again.
                let out = s.dy(i, j, amount_in)?;
                s.stale = true;
                Ok(out)
            }
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
        let (mut next_tick, initialized) =
            next_tick_within_word(&s.ticks, tick, s.tick_spacing, zfo);
        next_tick = next_tick.clamp(tick_math::MIN_TICK, tick_math::MAX_TICK);
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
            let den = if s.ng {
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
        if s.ng {
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
    /// expired PTs): linear in size (discovery checks it).
    Linear { assets_per_scale: U256 },
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
            UnwrapRate::Linear { assets_per_scale } => !assets_per_scale.is_zero(),
            UnwrapRate::CurveLp { total_supply } => !total_supply.is_zero(),
            UnwrapRate::Pendle(s) => {
                s.total_pt > alloy_primitives::I256::ZERO && !s.sy_scale.is_zero()
            }
        }
    }

    /// What unwrapping `amount` pays. Linear: the rate less one part per
    /// million and one wei (the vault rounds down, and its rate moves a
    /// little between the read and inclusion). Curve LP: the pool's own
    /// withdrawal math on the book's state of the pool, exactly.
    pub fn convert(&self, amount: U256, book: &PoolBook) -> Result<U256, RouteError> {
        if !self.is_live() {
            return Err(RouteError::StalePool);
        }
        match (self.rate, self.kind) {
            (UnwrapRate::Linear { assets_per_scale }, _) => {
                let raw = mul_div_512(amount, assets_per_scale, self.scale)?;
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
            UnwrapRate::Linear { assets_per_scale } => Ok((assets_per_scale, self.scale)),
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
    v3_factory: Option<Address>,
    /// Gas per V3 hop for discovered pools.
    v3_hop_gas: u64,
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
            v3_factory,
            v3_hop_gas,
            discovered: 0,
            generation: 0,
        }
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
            _ => {}
        }
        let id = PoolId(u32::try_from(self.pools.len()).map_err(|_| RouteError::Math)?);
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
        let Some(id) = self.by_address.get(&log.address).copied() else {
            return;
        };
        let Some(pool) = self
            .pools
            .get_mut(usize::try_from(id.0).unwrap_or(usize::MAX))
        else {
            return;
        };
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
        };
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
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
                sqrt_price_x96: U256::ZERO,
                tick: 0,
                liquidity: 0,
                fee_pips,
                tick_spacing: ev.tickSpacing.as_i32(),
                ticks: Vec::new(),
            }),
        };
        if self.add(pool).is_ok() {
            self.discovered = self.discovered.saturating_add(1);
        }
    }
}

fn fold_v3(s: &mut V3State, t0: B256, log: &DecodedLog<'_>) -> bool {
    let topics = log.topics.iter().copied();
    if t0 == IUniswapV3Pool::Swap::SIGNATURE_HASH {
        match IUniswapV3Pool::Swap::decode_raw_log(topics, log.data) {
            Ok(ev) => {
                s.sqrt_price_x96 = U256::from(ev.sqrtPriceX96);
                s.liquidity = ev.liquidity;
                s.tick = ev.tick.as_i32();
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
        let mut out = Vec::with_capacity(self.pools.len().saturating_mul(4).saturating_add(1));
        if let Some(f) = self.v3_factory {
            out.push(LogFilter {
                address: f,
                topic0: IUniswapV3Factory::PoolCreated::SIGNATURE_HASH,
            });
        }
        for p in &self.pools {
            let topics: &[B256] = match p.state {
                PoolState::V3(_) => &[
                    IUniswapV3Pool::Initialize::SIGNATURE_HASH,
                    IUniswapV3Pool::Swap::SIGNATURE_HASH,
                    IUniswapV3Pool::Mint::SIGNATURE_HASH,
                    IUniswapV3Pool::Burn::SIGNATURE_HASH,
                ],
                PoolState::V2(_) => &[IUniswapV2Pair::Sync::SIGNATURE_HASH],
                PoolState::Curve(ref c) => c.stale_topics(),
                PoolState::Crypto(_) => &CRYPTO_STALE_TOPICS,
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

    /// Oracle: closed enum. The venue set is exactly {V2, V3, Curve}; a
    /// V4 or Balancer pool has no representation and cannot be added.
    #[test]
    fn v4_balancer_never_venues() {
        const ALL: [Venue; 4] = [
            Venue::UniV2,
            Venue::UniV3,
            Venue::CurveStable,
            Venue::CurveCrypto,
        ];
        for v in ALL {
            // Exhaustive match: adding a variant is a compile error here.
            match v {
                Venue::UniV2 | Venue::UniV3 | Venue::CurveStable | Venue::CurveCrypto => (),
            }
        }
        let src = include_str!("solver.rs");
        let start = src.find("pub enum Venue {").unwrap();
        let body = &src[start..src[start..].find('}').unwrap() + start];
        assert!(!body.contains("V4") && !body.contains("Balancer"), "{body}");
        assert_eq!(body.matches(',').count(), 4);
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
            offpeg_fee_multiplier: u(50_000_000_000),
            dynamic_rates: true,
            read_block: 0,
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
