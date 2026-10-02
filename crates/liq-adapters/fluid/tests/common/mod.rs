//! Fixtures from the Fluid vault ABI (`vaultT1|T2|T3/coreModule/main.sol`,
//! `dex/poolT1`, `periphery/resolvers` @ `9496626f`): the answers a vault's
//! dead-address liquidation and a DEX's one-token estimate revert with.

#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::{SolError, SolEvent};
use liq_adapters_fluid::events::{dex, factory, vault};
use liq_adapters_fluid::{
    AssetConfig, Config, Fluid, VaultPin, NATIVE_TOKEN, VAULT_T1, VAULT_T2, VAULT_T3,
};
use liq_protocol::conformance::JournalStore;
use liq_protocol::{DecodedLog, FeedId, MarketRows, Protocol, StateAnswer, StateRead};
use liq_types::{AssetId, MarketId, Price, PriceVector, ProtocolId, Ray, SourceKind};

pub const PROTOCOL: ProtocolId = ProtocolId(10);
pub const WETH_A: AssetId = AssetId(0);
pub const USDC_A: AssetId = AssetId(1);
pub const USDT_A: AssetId = AssetId(2);
pub const T0: u64 = 1_700_000_000;
pub const BLOCK: u64 = 100;
/// 1 RAY = $1; ETH at $2,000.
pub const RAY: U256 = liq_types::fixed::RAY;

pub struct Deploy {
    pub factory: Address,
    pub weth: Address,
    pub usdc: Address,
    pub usdt: Address,
    pub unknown: Address,
    pub liquidity: Address,
    /// T1: native ETH collateral, USDC debt.
    pub v1: Address,
    /// T3: WETH collateral, USDC/USDT smart debt.
    pub v3: Address,
    pub dex_debt: Address,
    /// T2: USDT/WETH smart collateral, USDC debt.
    pub v2: Address,
    pub dex_col: Address,
    /// T1 whose collateral the registry does not know.
    pub vx: Address,
}

impl Deploy {
    pub fn new() -> Self {
        Self {
            factory: Address::repeat_byte(0xf1),
            weth: Address::repeat_byte(0xc0),
            usdc: Address::repeat_byte(0xc1),
            usdt: Address::repeat_byte(0xc2),
            unknown: Address::repeat_byte(0xe5),
            liquidity: Address::repeat_byte(0x11),
            v1: Address::repeat_byte(0xa1),
            v3: Address::repeat_byte(0xa3),
            dex_debt: Address::repeat_byte(0xd3),
            v2: Address::repeat_byte(0xa2),
            dex_col: Address::repeat_byte(0xd2),
            vx: Address::repeat_byte(0xa9),
        }
    }

    pub fn pins(&self) -> Vec<VaultPin> {
        let base = |vault, id, ty| VaultPin {
            vault,
            vault_id: id,
            vault_type: ty,
            supply: self.liquidity,
            borrow: self.liquidity,
            supply0: Address::ZERO,
            supply1: Address::ZERO,
            borrow0: Address::ZERO,
            borrow1: Address::ZERO,
            supply_decimals0: 0,
            supply_decimals1: 0,
            borrow_decimals0: 0,
            borrow_decimals1: 0,
        };
        let mut t1 = base(self.v1, 1, VAULT_T1);
        (t1.supply0, t1.supply_decimals0) = (NATIVE_TOKEN, 18);
        (t1.borrow0, t1.borrow_decimals0) = (self.usdc, 6);
        let mut t3 = base(self.v3, 2, VAULT_T3);
        t3.borrow = self.dex_debt;
        (t3.supply0, t3.supply_decimals0) = (self.weth, 18);
        (t3.borrow0, t3.borrow_decimals0) = (self.usdc, 6);
        (t3.borrow1, t3.borrow_decimals1) = (self.usdt, 6);
        let mut t2 = base(self.v2, 3, VAULT_T2);
        t2.supply = self.dex_col;
        (t2.supply0, t2.supply_decimals0) = (self.usdt, 6);
        (t2.supply1, t2.supply_decimals1) = (self.weth, 18);
        (t2.borrow0, t2.borrow_decimals0) = (self.usdc, 6);
        let mut tx = base(self.vx, 4, VAULT_T1);
        (tx.supply0, tx.supply_decimals0) = (self.unknown, 18);
        (tx.borrow0, tx.borrow_decimals0) = (self.usdc, 6);
        vec![t1, t3, t2, tx]
    }

    pub fn config(&self) -> Config {
        Config {
            protocol: PROTOCOL,
            factory: self.factory,
            catalog: MarketId(4000),
            first_market: MarketId(4001),
            weth: self.weth,
            vault_pins: self.pins(),
            assets: vec![
                AssetConfig {
                    underlying: self.weth,
                    asset: WETH_A,
                    feed: FeedId(0),
                    decimals: 18,
                },
                AssetConfig {
                    underlying: self.usdc,
                    asset: USDC_A,
                    feed: FeedId(0),
                    decimals: 6,
                },
                AssetConfig {
                    underlying: self.usdt,
                    asset: USDT_A,
                    feed: FeedId(0),
                    decimals: 6,
                },
            ],
            live_bound: true,
        }
    }

    pub fn adapter(&self) -> Fluid {
        Fluid::new(self.config()).expect("fixture config boots")
    }
}

pub struct NoRows;
impl MarketRows for NoRows {
    fn rows(&self, _: MarketId) -> Option<&[liq_protocol::MarketRow]> {
        None
    }
}

/// `FluidLiquidateResult(col, debt)` revert data.
pub fn sim(col: u128, debt: u128) -> Vec<u8> {
    vault::FluidLiquidateResult {
        colLiquidated: U256::from(col),
        debtLiquidated: U256::from(debt),
    }
    .abi_encode()
}

pub fn payback(amt: u128) -> Vec<u8> {
    dex::FluidDexSingleTokenOutput {
        tokenAmt: U256::from(amt),
    }
    .abi_encode()
}

pub fn withdraw(amt: u128) -> Vec<u8> {
    dex::FluidDexLiquidityOutput {
        tokenAmt: U256::from(amt),
    }
    .abi_encode()
}

/// What the chain answers, by read: `answer(read) -> revert data`.
pub type Chain<'a> = &'a dyn Fn(&StateRead) -> Vec<u8>;

/// Run the adapter's two read stages against `chain` and fold the result,
/// as the reader thread and the ingest thread do.
pub fn read_block(
    p: &Fluid,
    st: &mut JournalStore,
    chain: Chain<'_>,
    ts: u64,
) -> liq_protocol::DirtySet {
    let first = p.state_reads(&NoRows);
    let mut all: Vec<(StateRead, Vec<u8>)> = first.iter().map(|r| (r.clone(), chain(r))).collect();
    let mut follow = Vec::new();
    for (r, data) in &all {
        let a = StateAnswer {
            read: r,
            success: false,
            data,
        };
        for f in p.state_follow_ups(a) {
            let d = chain(&f);
            follow.push((f, d));
        }
    }
    all.extend(follow);
    let answers: Vec<StateAnswer<'_>> = all
        .iter()
        .map(|(r, d)| StateAnswer {
            read: r,
            success: false,
            data: d,
        })
        .collect();
    let sets = p.apply_state_reads(st, ts, &answers).expect("answers fold");
    assert!(sets.len() <= 1, "Fluid reports one set per batch");
    sets.into_iter().next().unwrap_or(liq_protocol::DirtySet::None)
}

#[derive(Clone, Debug)]
pub struct OwnedLog {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Vec<u8>,
    pub block: u64,
    pub timestamp: u64,
}

impl OwnedLog {
    pub fn view(&self) -> DecodedLog<'_> {
        DecodedLog {
            address: self.address,
            topics: &self.topics,
            data: &self.data,
            block: self.block,
            timestamp: self.timestamp,
        }
    }
}

pub fn deployed(d: &Deploy, vault: Address, id: u64) -> OwnedLog {
    let ev = factory::VaultDeployed {
        vault,
        vaultId: U256::from(id),
    };
    OwnedLog {
        address: d.factory,
        topics: ev.encode_topics().into_iter().map(|t| t.0).collect(),
        data: ev.encode_data(),
        block: BLOCK,
        timestamp: T0,
    }
}

pub fn prices(eth_usd: u64) -> PriceVector {
    let p = |asset, dollars: u64| Price {
        asset,
        price: Ray::from_raw(U256::from(dollars) * RAY),
        source: SourceKind::Canonical,
        block: BLOCK,
        ts: T0,
    };
    PriceVector(vec![p(WETH_A, eth_usd), p(USDC_A, 1), p(USDT_A, 1)])
}
