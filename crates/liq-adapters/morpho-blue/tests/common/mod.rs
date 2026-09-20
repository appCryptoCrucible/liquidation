//! Fixtures from Morpho Blue published rules (`Morpho.sol` @ `8e26ca6a`).

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
use liq_adapters_morpho_blue::events::{self as ev};
use liq_adapters_morpho_blue::{AssetConfig, Config, MorphoBlue, SourcePin};
use liq_protocol::conformance::JournalStore;
use liq_protocol::{DecodedLog, FeedId, Protocol};
use liq_types::{AssetId, MarketId, PositionId, Price, PriceVector, ProtocolId, Ray, SourceKind};

pub const PROTOCOL: ProtocolId = ProtocolId(2);
pub const CATALOG: MarketId = MarketId(0);
pub const FIRST: MarketId = MarketId(1);
pub const WETH: AssetId = AssetId(0);
pub const DAI: AssetId = AssetId(1);
pub const DEPLOY_BLOCK: u64 = 100;
pub const T0: u64 = 1_700_000_000;
pub const T1: u64 = T0 + 2_592_000;
pub const P8_TO_RAY: U256 = uint!(10_000_000_000_000_000_000_U256);
pub const LLTV: U256 = uint!(860_000_000_000_000_000_U256);
pub const WETH_P8: u64 = 2000_0000_0000;
pub const DAI_P8: u64 = 1_0000_0000;
pub const ALICE_WETH: U256 = uint!(1_000_000_000_000_000_000_U256);
pub const ALICE_DAI_DEBT: U256 = uint!(1_400_000_000_000_000_000_000_U256);
pub const BOB_DAI: U256 = uint!(10_000_000_000_000_000_000_000_U256);
pub const ALICE_ID: PositionId = PositionId(1);
pub const BOB_ID: PositionId = PositionId(0);
pub const MARKET_ID: B256 = B256::repeat_byte(0x11);

pub struct Deploy {
    pub morpho: Address,
    pub oracle: Address,
    pub irm: Address,
    pub weth: Address,
    pub dai: Address,
    pub alice: Address,
    pub bob: Address,
}

impl Deploy {
    pub fn new() -> Self {
        Self {
            morpho: Address::repeat_byte(0xbb),
            oracle: Address::repeat_byte(0xb0),
            irm: Address::repeat_byte(0xb1),
            weth: Address::repeat_byte(0xa0),
            dai: Address::repeat_byte(0xa1),
            alice: Address::repeat_byte(0xc1),
            bob: Address::repeat_byte(0xc2),
        }
    }

    pub fn config(&self) -> Config {
        Config {
            protocol: PROTOCOL,
            morpho: self.morpho,
            catalog: CATALOG,
            first_market: FIRST,
            assets: vec![
                AssetConfig {
                    underlying: self.weth,
                    asset: WETH,
                    feed: FeedId(1),
                    decimals: 18,
                },
                AssetConfig {
                    underlying: self.dai,
                    asset: DAI,
                    feed: FeedId(2),
                    decimals: 18,
                },
            ],
            price_sources: vec![SourcePin {
                oracle: self.oracle,
            }],
            pinned_through: DEPLOY_BLOCK,
        }
    }

    pub fn adapter(&self) -> MorphoBlue {
        MorphoBlue::new(self.config()).expect("fixture config validates")
    }

    pub fn params(&self) -> ev::MarketParams {
        ev::MarketParams {
            loanToken: self.dai,
            collateralToken: self.weth,
            oracle: self.oracle,
            irm: self.irm,
            lltv: LLTV,
        }
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
    vec![log(
        d.morpho,
        &ev::CreateMarket {
            id: MARKET_ID,
            marketParams: d.params(),
        },
        b,
        t,
    )]
}

pub fn activity_logs(d: &Deploy) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 1, T0);
    vec![
        log(
            d.morpho,
            &ev::Supply {
                id: MARKET_ID,
                caller: d.bob,
                onBehalf: d.bob,
                assets: BOB_DAI,
                shares: BOB_DAI * uint!(1_000_000_U256),
            },
            b,
            t,
        ),
        log(
            d.morpho,
            &ev::SupplyCollateral {
                id: MARKET_ID,
                caller: d.alice,
                onBehalf: d.alice,
                assets: ALICE_WETH,
            },
            b,
            t,
        ),
        log(
            d.morpho,
            &ev::Borrow {
                id: MARKET_ID,
                caller: d.alice,
                onBehalf: d.alice,
                receiver: d.alice,
                assets: ALICE_DAI_DEBT,
                shares: ALICE_DAI_DEBT * uint!(1_000_000_U256),
            },
            b,
            t,
        ),
    ]
}

pub fn store_after(p: &MorphoBlue, logs: &[OwnedLog]) -> JournalStore {
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

pub fn coverage_ranks() -> Vec<(B256, u8)> {
    const DOC: &str = include_str!("../../../../../docs/coverage/morpho-blue.md");
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
    assert!(out.len() >= 16, "coverage table parsed {} rows", out.len());
    out
}

pub fn rank_of(ranks: &[(B256, u8)], topic0: B256) -> u8 {
    ranks
        .iter()
        .find(|(t, _)| *t == topic0)
        .map(|(_, r)| *r)
        .unwrap_or_else(|| panic!("topic {topic0} is not in the coverage table"))
}
