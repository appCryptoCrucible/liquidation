//! Fixtures from Silo V2 published rules
//! (`PartialLiquidation.sol` / `SiloSolvencyLib` @ `570a668a`).

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
use liq_adapters_silo_v2::events::{factory, silo};
use liq_adapters_silo_v2::{AssetConfig, Config, PairConfig, SideConfig, SiloV2};
use liq_protocol::conformance::JournalStore;
use liq_protocol::{DecodedLog, FeedId, Protocol};
use liq_types::{AssetId, MarketId, PositionId, Price, PriceVector, ProtocolId, Ray, SourceKind};

pub const PROTOCOL: ProtocolId = ProtocolId(6);
pub const MARKET: MarketId = MarketId(1);
pub const COLL: AssetId = AssetId(0);
pub const DEBT: AssetId = AssetId(1);
pub const DEPLOY_BLOCK: u64 = 100;
pub const T0: u64 = 1_700_000_000;
pub const LT: u128 = 880_000_000_000_000_000;
pub const TARGET_LTV: u128 = 870_000_000_000_000_000;
pub const FEE: u128 = 40_000_000_000_000_000;
pub const RAY_ONE: U256 = uint!(1_000_000_000_000_000_000_000_000_000_U256);
pub const BOB_COLL: U256 = uint!(10_000_000_000_000_000_000_000_U256);
pub const ALICE_COLL: U256 = uint!(1_000_000_000_000_000_000_000_U256);
pub const ALICE_DEBT_OK: U256 = uint!(100_000_000_U256);
pub const ALICE_DEBT_LIQ: U256 = uint!(900_000_000_U256);
pub const ALICE_ID: PositionId = PositionId(1);
pub const BOB_ID: PositionId = PositionId(0);

pub struct Deploy {
    pub factory: Address,
    pub hook: Address,
    pub silo_config: Address,
    pub silo0: Address,
    pub silo1: Address,
    pub prot0: Address,
    pub prot1: Address,
    pub debt0: Address,
    pub debt1: Address,
    pub token0: Address,
    pub token1: Address,
    pub alice: Address,
    pub bob: Address,
}

impl Deploy {
    pub fn new() -> Self {
        Self {
            factory: Address::repeat_byte(0xf1),
            hook: Address::repeat_byte(0xf2),
            silo_config: Address::repeat_byte(0xf3),
            silo0: Address::repeat_byte(0xa0),
            silo1: Address::repeat_byte(0xa1),
            prot0: Address::repeat_byte(0xb0),
            prot1: Address::repeat_byte(0xb1),
            debt0: Address::repeat_byte(0xb2),
            debt1: Address::repeat_byte(0xb3),
            token0: Address::repeat_byte(0xc0),
            token1: Address::repeat_byte(0xc1),
            alice: Address::repeat_byte(0x11),
            bob: Address::repeat_byte(0x12),
        }
    }

    pub fn config(&self) -> Config {
        Config {
            protocol: PROTOCOL,
            factories: vec![self.factory],
            pairs: vec![PairConfig {
                silo_config: self.silo_config,
                hook_receiver: self.hook,
                market: MARKET,
                silo0: SideConfig {
                    silo: self.silo0,
                    token: self.token0,
                    protected_share: self.prot0,
                    debt_share: self.debt0,
                    solvency_oracle: Address::repeat_byte(0xd0),
                    lt: LT,
                    liquidation_fee: FEE,
                    liquidation_target_ltv: TARGET_LTV,
                },
                silo1: SideConfig {
                    silo: self.silo1,
                    token: self.token1,
                    protected_share: self.prot1,
                    debt_share: self.debt1,
                    solvency_oracle: Address::repeat_byte(0xd1),
                    lt: 0,
                    liquidation_fee: 0,
                    liquidation_target_ltv: 0,
                },
            }],
            assets: vec![
                AssetConfig {
                    underlying: self.token0,
                    asset: COLL,
                    feed: FeedId(0),
                    decimals: 18,
                },
                AssetConfig {
                    underlying: self.token1,
                    asset: DEBT,
                    feed: FeedId(0),
                    decimals: 6,
                },
            ],
            pinned_through: DEPLOY_BLOCK,
        }
    }

    pub fn adapter(&self) -> SiloV2 {
        SiloV2::new(self.config()).expect("fixture config validates")
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
    vec![log(
        d.factory,
        &factory::NewSilo {
            implementation: Address::repeat_byte(0x01),
            token0: d.token0,
            token1: d.token1,
            silo0: d.silo0,
            silo1: d.silo1,
            siloConfig: d.silo_config,
        },
        DEPLOY_BLOCK,
        T0,
    )]
}

pub fn activity_logs(d: &Deploy, alice_debt: U256) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 1, T0);
    vec![
        log(
            d.silo0,
            &silo::Deposit {
                sender: d.bob,
                owner: d.bob,
                assets: BOB_COLL,
                shares: BOB_COLL,
            },
            b,
            t,
        ),
        log(
            d.silo0,
            &silo::Deposit {
                sender: d.alice,
                owner: d.alice,
                assets: ALICE_COLL,
                shares: ALICE_COLL,
            },
            b,
            t,
        ),
        log(
            d.silo1,
            &silo::Borrow {
                sender: d.alice,
                receiver: d.alice,
                owner: d.alice,
                assets: alice_debt,
                shares: alice_debt,
            },
            b,
            t,
        ),
    ]
}

pub fn store_after(p: &SiloV2, logs: &[OwnedLog]) -> JournalStore {
    let mut st = JournalStore::new();
    for l in logs {
        p.apply_log(&mut st, &l.view()).expect("fixture log folds");
    }
    st
}

pub fn prices(coll: U256, debt: U256) -> PriceVector {
    PriceVector(vec![
        Price {
            asset: COLL,
            price: Ray::from_raw(coll),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        },
        Price {
            asset: DEBT,
            price: Ray::from_raw(debt),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        },
    ])
}

pub fn coverage_ranks() -> Vec<(B256, u8)> {
    const DOC: &str = include_str!("../../../../../docs/coverage/silo-v2.md");
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
