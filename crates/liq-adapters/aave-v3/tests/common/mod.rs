//! Fixtures from Aave V3 published rules (`GenericLogic` / `LiquidationLogic`
//! @ `8305565ae`). No fork; share counts from TokenMath at index RAY.

#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::inconsistent_digit_grouping,
    clippy::vec_init_then_push,
    clippy::cast_possible_truncation
)]

use alloy_primitives::{uint, Address, B256, U256};
use alloy_sol_types::SolEvent;
use liq_adapters_aave_v3::events::{cfg as ccfg, oracle, pool};
use liq_adapters_aave_v3::{AaveV3, AssetConfig, Config, LiquidationParams, PoolConfig, SourcePin};
use liq_protocol::conformance::JournalStore;
use liq_protocol::{DecodedLog, FeedId, Protocol};
use liq_types::fixed::RAY;
use liq_types::{AssetId, MarketId, PositionId, Price, PriceVector, ProtocolId, Ray, SourceKind};

pub const PROTOCOL: ProtocolId = ProtocolId(3);
pub const POOL_MARKET: MarketId = MarketId(200);
pub const WETH: AssetId = AssetId(0);
pub const DAI: AssetId = AssetId(1);
pub const WETH_SLOT: u16 = 1;
pub const DAI_SLOT: u16 = 2;
pub const DEPLOY_BLOCK: u64 = 100;
pub const T0: u64 = 1_700_000_000;
pub const T1: u64 = T0 + 2_592_000;
pub const P8_TO_RAY: U256 = uint!(10_000_000_000_000_000_000_U256);

pub const WETH_LT: u16 = 82_50;
pub const WETH_LTV: u16 = 80_50;
pub const WETH_BONUS: u16 = 105_00;
pub const DAI_LT: u16 = 77_00;
pub const DAI_LTV: u16 = 75_00;
pub const DAI_BONUS: u16 = 105_00;
pub const WETH_P8: u64 = 2000_0000_0000;
pub const DAI_P8: u64 = 1_0000_0000;
pub const WETH_RATE: U256 = uint!(20_000_000_000_000_000_000_000_000_U256);
pub const DAI_RATE: U256 = uint!(50_000_000_000_000_000_000_000_000_U256);
pub const ALICE_WETH: U256 = uint!(1_000_000_000_000_000_000_U256);
pub const ALICE_DAI_DEBT: U256 = uint!(1_500_000_000_000_000_000_000_U256);
pub const ALICE_ID: PositionId = PositionId(0);
pub const BOB_DAI: U256 = uint!(10_000_000_000_000_000_000_000_U256);
pub const BOB_ID: PositionId = PositionId(1);

pub struct Deploy {
    pub pool: Address,
    pub oracle: Address,
    pub provider: Address,
    pub configurator: Address,
    pub weth: Address,
    pub dai: Address,
    pub a_weth: Address,
    pub a_dai: Address,
    pub v_weth: Address,
    pub v_dai: Address,
    pub src_weth: Address,
    pub src_dai: Address,
    pub alice: Address,
    pub bob: Address,
}

impl Deploy {
    pub fn new() -> Self {
        Self {
            pool: Address::repeat_byte(0x22),
            oracle: Address::repeat_byte(0x33),
            provider: Address::repeat_byte(0x44),
            configurator: Address::repeat_byte(0x55),
            weth: Address::repeat_byte(0xa0),
            dai: Address::repeat_byte(0xa1),
            a_weth: Address::repeat_byte(0xb2),
            a_dai: Address::repeat_byte(0xb3),
            v_weth: Address::repeat_byte(0xb4),
            v_dai: Address::repeat_byte(0xb5),
            src_weth: Address::repeat_byte(0xb0),
            src_dai: Address::repeat_byte(0xb1),
            alice: Address::repeat_byte(0xc1),
            bob: Address::repeat_byte(0xc2),
        }
    }

    pub fn config(&self) -> Config {
        Config {
            protocol: PROTOCOL,
            pools: vec![PoolConfig {
                address: self.pool,
                market: POOL_MARKET,
                oracle: self.oracle,
                provider: self.provider,
                configurator: self.configurator,
                sentinel: Address::ZERO,
                sequencer_oracle: Address::ZERO,
            }],
            assets: vec![
                AssetConfig {
                    underlying: self.weth,
                    asset: WETH,
                    feed: FeedId(1),
                    siloed: false,
                    isolated: false,
                    debt_ceiling: 0,
                    decimals: 18,
                },
                AssetConfig {
                    underlying: self.dai,
                    asset: DAI,
                    feed: FeedId(2),
                    siloed: false,
                    isolated: false,
                    debt_ceiling: 0,
                    decimals: 18,
                },
            ],
            price_sources: vec![
                SourcePin {
                    pool: self.pool,
                    underlying: self.weth,
                    source: self.src_weth,
                },
                SourcePin {
                    pool: self.pool,
                    underlying: self.dai,
                    source: self.src_dai,
                },
            ],
            liquidation: LiquidationParams {
                close_factor_bps: 5_000,
                close_factor_hf_wad: 950_000_000_000_000_000,
                min_base_max_close: 2000 * 100_000_000,
                oracle_decimals: 8,
            },
            pinned_through: DEPLOY_BLOCK,
        }
    }

    pub fn adapter(&self) -> AaveV3 {
        AaveV3::new(self.config()).expect("fixture config validates")
    }
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

pub fn log<E: SolEvent>(address: Address, ev: &E, block: u64, timestamp: u64) -> OwnedLog {
    OwnedLog {
        address,
        topics: ev.encode_topics().into_iter().map(|t| t.0).collect(),
        data: ev.encode_data(),
        block,
        timestamp,
    }
}

pub fn listing_logs(d: &Deploy) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK, T0);
    vec![
        log(
            d.configurator,
            &ccfg::ReserveInitialized {
                asset: d.weth,
                aToken: d.a_weth,
                stableDebtToken: Address::ZERO,
                variableDebtToken: d.v_weth,
                interestRateStrategyAddress: Address::repeat_byte(0xd1),
            },
            b,
            t,
        ),
        log(
            d.configurator,
            &ccfg::ReserveInitialized {
                asset: d.dai,
                aToken: d.a_dai,
                stableDebtToken: Address::ZERO,
                variableDebtToken: d.v_dai,
                interestRateStrategyAddress: Address::repeat_byte(0xd1),
            },
            b,
            t,
        ),
        log(
            d.configurator,
            &ccfg::CollateralConfigurationChanged {
                asset: d.weth,
                ltv: U256::from(WETH_LTV),
                liquidationThreshold: U256::from(WETH_LT),
                liquidationBonus: U256::from(WETH_BONUS),
            },
            b,
            t,
        ),
        log(
            d.configurator,
            &ccfg::CollateralConfigurationChanged {
                asset: d.dai,
                ltv: U256::from(DAI_LTV),
                liquidationThreshold: U256::from(DAI_LT),
                liquidationBonus: U256::from(DAI_BONUS),
            },
            b,
            t,
        ),
        log(
            d.oracle,
            &oracle::AssetSourceUpdated {
                asset: d.weth,
                source: d.src_weth,
            },
            b,
            t,
        ),
        log(
            d.oracle,
            &oracle::AssetSourceUpdated {
                asset: d.dai,
                source: d.src_dai,
            },
            b,
            t,
        ),
        log(
            d.pool,
            &pool::ReserveDataUpdated {
                reserve: d.weth,
                liquidityRate: WETH_RATE,
                stableBorrowRate: U256::ZERO,
                variableBorrowRate: WETH_RATE,
                liquidityIndex: RAY,
                variableBorrowIndex: RAY,
            },
            b,
            t,
        ),
        log(
            d.pool,
            &pool::ReserveDataUpdated {
                reserve: d.dai,
                liquidityRate: DAI_RATE,
                stableBorrowRate: U256::ZERO,
                variableBorrowRate: DAI_RATE,
                liquidityIndex: RAY,
                variableBorrowIndex: RAY,
            },
            b,
            t,
        ),
    ]
}

pub fn activity_logs(d: &Deploy) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 1, T0);
    vec![
        log(
            d.pool,
            &pool::Supply {
                reserve: d.weth,
                user: d.alice,
                onBehalfOf: d.alice,
                amount: ALICE_WETH,
                referralCode: 0,
            },
            b,
            t,
        ),
        log(
            d.pool,
            &pool::ReserveUsedAsCollateralEnabled {
                reserve: d.weth,
                user: d.alice,
            },
            b,
            t,
        ),
        log(
            d.pool,
            &pool::Supply {
                reserve: d.dai,
                user: d.bob,
                onBehalfOf: d.bob,
                amount: BOB_DAI,
                referralCode: 0,
            },
            b,
            t,
        ),
        log(
            d.pool,
            &pool::Borrow {
                reserve: d.dai,
                user: d.alice,
                onBehalfOf: d.alice,
                amount: ALICE_DAI_DEBT,
                interestRateMode: 2,
                borrowRate: DAI_RATE,
                referralCode: 0,
            },
            b,
            t,
        ),
    ]
}

pub fn store_after(p: &AaveV3, logs: &[OwnedLog]) -> JournalStore {
    let mut st = JournalStore::new();
    for l in logs {
        p.apply_log(&mut st, &l.view()).expect("fixture log folds");
    }
    st
}

pub fn prices(weth_p8: u64, dai_p8: u64) -> PriceVector {
    PriceVector(vec![
        Price {
            asset: WETH,
            price: Ray::from_raw(U256::from(weth_p8) * P8_TO_RAY),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        },
        Price {
            asset: DAI,
            price: Ray::from_raw(U256::from(dai_p8) * P8_TO_RAY),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        },
    ])
}

pub fn ray_of_p8(p8: u64) -> Ray {
    Ray::from_raw(U256::from(p8) * P8_TO_RAY)
}

pub fn coverage_ranks() -> Vec<(B256, u8)> {
    const DOC: &str = include_str!("../../../../../docs/coverage/aave-v3.md");
    let mut out = Vec::new();
    for line in DOC.lines() {
        if !line.starts_with('|') || line.contains("---") || line.contains("path |") {
            continue;
        }
        let cols: Vec<&str> = line
            .split('|')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if cols.len() < 4 {
            continue;
        }
        let topic_cell = cols[2];
        if !topic_cell.starts_with("0x") || topic_cell.len() < 66 {
            continue;
        }
        let topic: B256 = topic_cell[..66].parse().expect("coverage topic parses");
        let rank = match cols[3] {
            "None" => 0,
            "Positions" => 1,
            "MarketAccrual" => 2,
            "MarketReprice" => 3,
            "ProtocolWide" => 4,
            "halt" => 0,
            other => panic!("unknown DirtySet class {other:?} in coverage table"),
        };
        out.push((topic, rank));
    }
    assert!(out.len() >= 50, "coverage table parsed {} rows", out.len());
    out
}

pub fn rank_of(ranks: &[(B256, u8)], topic0: B256) -> u8 {
    ranks
        .iter()
        .find(|(t, _)| *t == topic0)
        .map(|(_, r)| *r)
        .unwrap_or_else(|| panic!("topic {topic0} is not in the coverage table"))
}
