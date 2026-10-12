//! The test book: two Uniswap V2 pools between WETH (`A`) and a token
//! (`B`), the anchor 2 % off the other, and the oracle their output is
//! checked against (V2 `getAmountOut`, written out).

#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects, clippy::panic)]

use crate::anchor::ArbCtx;
use alloy_primitives::{Address, U256};
use liq_router::{Pool, PoolBook, PoolId, PoolState, SolveBudget, V2State};
use liq_types::AssetId;
use smallvec::SmallVec;
use std::collections::HashMap;

pub(crate) const A: AssetId = AssetId(0); // WETH
pub(crate) const B: AssetId = AssetId(1);
pub(crate) const HOP: u64 = 90_000;
pub(crate) const BASE: u64 = 60_000;
pub(crate) const E21: u128 = 1_000_000_000_000_000_000_000;

pub(crate) fn tok(n: u8) -> Address {
    Address::repeat_byte(n.wrapping_add(0x10))
}

pub(crate) fn v2(n: u8, r0: u128, r1: u128) -> Pool {
    Pool {
        address: Address::repeat_byte(n),
        assets: SmallVec::from_slice(&[A, B]),
        tokens: SmallVec::from_slice(&[tok(0), tok(1)]),
        hop_gas: HOP,
        state: PoolState::V2(V2State {
            reserve0: U256::from(r0),
            reserve1: U256::from(r1),
            factory: 0,
        }),
    }
}

/// Pool 0 (the anchor) sells B 2 % cheaper than pool 1 buys it.
pub(crate) fn book() -> (PoolBook, PoolId, PoolId) {
    let mut assets = HashMap::new();
    assets.insert(tok(0), A);
    assets.insert(tok(1), B);
    let mut b = PoolBook::new(assets, None, 0);
    let p0 = b.add(v2(1, E21, E21 * 102 / 100)).unwrap();
    let p1 = b.add(v2(2, E21, E21)).unwrap();
    (b, p0, p1)
}

/// Uniswap V2 `getAmountOut`.
pub(crate) fn v2_out(amt: U256, rin: U256, rout: U256) -> U256 {
    let a = amt * U256::from(997u32);
    a * rout / (rin * U256::from(1000u32) + a)
}

pub(crate) fn reserves(b: &PoolBook, p: PoolId) -> (U256, U256) {
    match &b.get(p).unwrap().state {
        PoolState::V2(s) => (s.reserve0, s.reserve1),
        _ => panic!(),
    }
}

pub(crate) fn ctx<'a>(per: &'a [U256], flash: &'a [AssetId]) -> ArbCtx<'a> {
    ArbCtx {
        base_fee_wei: 1_000_000_000,
        per_eth: per,
        flashable: flash,
        weth: A,
        budget: SolveBudget::default(),
        base_gas: BASE,
        max_evals: 40,
    }
}
