//! Test-only pool constructors and log encoders. No chain data is
//! fabricated here: every fixture is a *synthetic* pool whose expected
//! behaviour is fixed by an independent oracle in the test that uses it
//! (TESTING.md — invariant, independent implementation, or closed form).

#![allow(
    unreachable_pub,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use alloy_primitives::aliases::{I24, U160};
use alloy_primitives::{Address, LogData, U256};
use alloy_sol_types::SolEvent;
use liq_protocol::DecodedLog;
use liq_types::AssetId;
use smallvec::SmallVec;
use uniswap_v3_math::tick_math;

use crate::solver::{
    CurveState, ICurvePool, IUniswapV2Pair, IUniswapV3Factory, IUniswapV3Pool, Pool, PoolState,
    Tick, V2State, V3State, Q96, WAD,
};

pub const A0: AssetId = AssetId(0);
pub const A1: AssetId = AssetId(1);
pub const A2: AssetId = AssetId(2);
pub const HOP_GAS: u64 = 100_000;

pub fn addr(n: u64) -> Address {
    Address::from_slice(&{
        let mut b = [0u8; 20];
        b[12..].copy_from_slice(&n.to_be_bytes());
        b
    })
}

pub fn tok(n: u64) -> Address {
    addr(0x7000_0000 + n)
}

pub fn e18(n: u64) -> U256 {
    U256::from(n) * WAD
}

pub fn i24(t: i32) -> I24 {
    I24::try_from(t).unwrap()
}

/// V3 pool from positions `(lower, upper, liquidity)`; active liquidity
/// is the sum over positions containing `tick`.
pub fn v3(
    n: u64,
    fee_pips: u32,
    spacing: i32,
    sqrt_price_x96: U256,
    positions: &[(i32, i32, u128)],
) -> Pool {
    let tick = tick_math::get_tick_at_sqrt_ratio(sqrt_price_x96).unwrap();
    let mut ticks: Vec<Tick> = Vec::new();
    let mut liquidity = 0u128;
    for &(lo, hi, l) in positions {
        assert!(lo < hi && lo % spacing == 0 && hi % spacing == 0);
        for (t, sign) in [(lo, 1i128), (hi, -1i128)] {
            let net = sign * i128::try_from(l).unwrap();
            match ticks.iter_mut().find(|x| x.tick == t) {
                Some(x) => {
                    x.net += net;
                    x.gross += l;
                }
                None => ticks.push(Tick {
                    tick: t,
                    net,
                    gross: l,
                }),
            }
        }
        if lo <= tick && tick < hi {
            liquidity += l;
        }
    }
    ticks.sort_by_key(|t| t.tick);
    Pool {
        address: addr(n),
        assets: SmallVec::from_slice(&[A0, A1]),
        tokens: SmallVec::from_slice(&[tok(0), tok(1)]),
        hop_gas: HOP_GAS,
        state: PoolState::V3(V3State {
            sqrt_price_x96,
            tick,
            liquidity,
            fee_pips,
            tick_spacing: spacing,
            ticks,
        }),
    }
}

pub fn v2(n: u64, r0: U256, r1: U256) -> Pool {
    Pool {
        address: addr(n),
        assets: SmallVec::from_slice(&[A0, A1]),
        tokens: SmallVec::from_slice(&[tok(0), tok(1)]),
        hop_gas: HOP_GAS,
        state: PoolState::V2(V2State {
            reserve0: r0,
            reserve1: r1,
            factory: 0,
        }),
    }
}

/// Curve plain pool, all coins 18 decimals, `A` in the `A_PRECISION = 100`
/// convention (`a` stored pre-multiplied, like the contract).
pub fn curve(n: u64, balances: &[U256], a_times_100: u64, fee_1e10: u64) -> Pool {
    let k = balances.len();
    Pool {
        address: addr(n),
        assets: (0..k).map(|i| AssetId(u16::try_from(i).unwrap())).collect(),
        tokens: (0..k).map(|i| tok(u64::try_from(i).unwrap())).collect(),
        hop_gas: HOP_GAS,
        state: PoolState::Curve(CurveState {
            balances: balances.iter().copied().collect(),
            rates: (0..k).map(|_| WAD).collect(),
            a: U256::from(a_times_100),
            a_precision: U256::from(100u64),
            fee: U256::from(fee_1e10),
            stale: false,
            stale_block: 0,
        }),
    }
}

pub fn sqrt_at(tick: i32) -> U256 {
    tick_math::get_sqrt_ratio_at_tick(tick).unwrap()
}

pub const SQRT_ONE: U256 = Q96;

// ───────────────────────────── logs ─────────────────────────────

pub struct OwnedLog {
    pub address: Address,
    pub data: LogData,
}

impl OwnedLog {
    pub fn decoded(&self) -> DecodedLog<'_> {
        // Pool folds are keyed on address + topic + body only; block and
        // timestamp are carried for the trait and not read by `PoolBook`.
        DecodedLog {
            address: self.address,
            topics: self.data.topics(),
            data: &self.data.data,
            block: 0,
            timestamp: 0,
        }
    }
}

pub fn v3_swap_log(pool: Address, sqrt_price_x96: U256, liquidity: u128, tick: i32) -> OwnedLog {
    let ev = IUniswapV3Pool::Swap {
        sender: Address::ZERO,
        recipient: Address::ZERO,
        amount0: alloy_primitives::I256::ZERO,
        amount1: alloy_primitives::I256::ZERO,
        sqrtPriceX96: sqrt_price_x96.to::<U160>(),
        liquidity,
        tick: i24(tick),
    };
    OwnedLog {
        address: pool,
        data: ev.encode_log_data(),
    }
}

pub fn v3_mint_log(pool: Address, lower: i32, upper: i32, amount: u128) -> OwnedLog {
    let ev = IUniswapV3Pool::Mint {
        sender: Address::ZERO,
        owner: Address::ZERO,
        tickLower: i24(lower),
        tickUpper: i24(upper),
        amount,
        amount0: U256::ZERO,
        amount1: U256::ZERO,
    };
    OwnedLog {
        address: pool,
        data: ev.encode_log_data(),
    }
}

pub fn v3_burn_log(pool: Address, lower: i32, upper: i32, amount: u128) -> OwnedLog {
    let ev = IUniswapV3Pool::Burn {
        owner: Address::ZERO,
        tickLower: i24(lower),
        tickUpper: i24(upper),
        amount,
        amount0: U256::ZERO,
        amount1: U256::ZERO,
    };
    OwnedLog {
        address: pool,
        data: ev.encode_log_data(),
    }
}

pub fn v2_sync_log(pool: Address, r0: U256, r1: U256) -> OwnedLog {
    let ev = IUniswapV2Pair::Sync {
        reserve0: r0.to(),
        reserve1: r1.to(),
    };
    OwnedLog {
        address: pool,
        data: ev.encode_log_data(),
    }
}

pub fn curve_exchange_log(pool: Address) -> OwnedLog {
    let ev = ICurvePool::TokenExchange {
        buyer: Address::ZERO,
        sold_id: 0i128,
        tokens_sold: U256::ONE,
        bought_id: 1i128,
        tokens_bought: U256::ONE,
    };
    OwnedLog {
        address: pool,
        data: ev.encode_log_data(),
    }
}

pub fn pool_created_log(
    factory: Address,
    token0: Address,
    token1: Address,
    fee: u32,
    spacing: i32,
    pool: Address,
) -> OwnedLog {
    let ev = IUniswapV3Factory::PoolCreated {
        token0,
        token1,
        fee: alloy_primitives::aliases::U24::from(fee),
        tickSpacing: i24(spacing),
        pool,
    };
    OwnedLog {
        address: factory,
        data: ev.encode_log_data(),
    }
}
