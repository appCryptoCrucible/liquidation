//! Fixtures from EVK published rules (`Liquidation.sol` @ `bfb325a6`).

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

use alloy_primitives::{uint, Address, Bytes, B256, U256};
use alloy_sol_types::SolEvent;
use liq_adapters_euler_v2::events::{self as ev, evc};
use liq_adapters_euler_v2::{AssetConfig, Config, EulerV2, SourcePin};
use liq_protocol::conformance::JournalStore;
use liq_protocol::{DecodedLog, FeedId, Protocol};
use liq_types::{AssetId, MarketId, PositionId, Price, PriceVector, ProtocolId, Ray, SourceKind};

pub const PROTOCOL: ProtocolId = ProtocolId(4);
pub const CATALOG: MarketId = MarketId(0);
pub const FIRST: MarketId = MarketId(1);
pub const USDC: AssetId = AssetId(0);
pub const WETH_SHARES: AssetId = AssetId(1);
pub const DEPLOY_BLOCK: u64 = 100;
pub const T0: u64 = 1_700_000_000;
pub const T1: u64 = T0 + 2_592_000;
pub const P8_TO_RAY: U256 = uint!(10_000_000_000_000_000_000_U256);
pub const LIQ_LTV: u16 = 8_000;
pub const BORROW_LTV: u16 = 7_800;
pub const MAX_DISCOUNT: u16 = 1_500;
pub const WETH_P8: u64 = 1000_0000_0000;
pub const USDC_P8: u64 = 1_0000_0000;
pub const ALICE_SHARES: U256 = uint!(1_000_000_000_000_000_000_U256);
pub const ALICE_DEBT_HEALTHY: U256 = uint!(700_000000_U256);
pub const ALICE_DEBT_EQ: U256 = uint!(800_000000_U256);
pub const ALICE_DEBT_LIQ: U256 = uint!(850_000000_U256);
pub const ALICE_ID: PositionId = PositionId(0);
pub const BOB_ID: PositionId = PositionId(1);

pub struct Deploy {
    pub factory: Address,
    pub evc: Address,
    pub oracle: Address,
    pub unit: Address,
    pub usdc: Address,
    pub weth: Address,
    pub debt_vault: Address,
    pub coll_vault: Address,
    pub impl_: Address,
    pub alice: Address,
    pub bob: Address,
}

impl Deploy {
    pub fn new() -> Self {
        Self {
            factory: Address::repeat_byte(0xf1),
            evc: Address::repeat_byte(0xec),
            oracle: Address::repeat_byte(0xb0),
            unit: Address::repeat_byte(0xee),
            usdc: Address::repeat_byte(0xa1),
            weth: Address::repeat_byte(0xa0),
            debt_vault: Address::repeat_byte(0xd1),
            coll_vault: Address::repeat_byte(0xc1),
            impl_: Address::repeat_byte(0x11),
            alice: Address::repeat_byte(0xa5),
            bob: Address::repeat_byte(0xb2),
        }
    }

    pub fn config(&self) -> Config {
        Config {
            protocol: PROTOCOL,
            factory: self.factory,
            evc: self.evc,
            catalog: CATALOG,
            first_market: FIRST,
            vaults: vec![self.debt_vault, self.coll_vault],
            interned: vec![],
            assets: vec![
                AssetConfig {
                    underlying: self.usdc,
                    asset: USDC,
                    feed: FeedId(1),
                    decimals: 6,
                },
                AssetConfig {
                    underlying: self.coll_vault,
                    asset: WETH_SHARES,
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

    pub fn adapter(&self) -> EulerV2 {
        EulerV2::new(self.config()).expect("fixture config validates")
    }

    pub fn trailing(&self, underlying: Address) -> Bytes {
        let mut d = Vec::with_capacity(60);
        d.extend_from_slice(underlying.as_slice());
        d.extend_from_slice(self.oracle.as_slice());
        d.extend_from_slice(self.unit.as_slice());
        Bytes::from(d)
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
            d.factory,
            &ev::ProxyCreated {
                proxy: d.debt_vault,
                upgradeable: true,
                implementation: d.impl_,
                trailingData: d.trailing(d.usdc),
            },
            b,
            t,
        ),
        log(
            d.factory,
            &ev::ProxyCreated {
                proxy: d.coll_vault,
                upgradeable: true,
                implementation: d.impl_,
                trailingData: d.trailing(d.weth),
            },
            b,
            t,
        ),
        log(
            d.debt_vault,
            &ev::GovSetLTV {
                collateral: d.coll_vault,
                borrowLTV: BORROW_LTV,
                liquidationLTV: LIQ_LTV,
                initialLiquidationLTV: LIQ_LTV,
                targetTimestamp: alloy_primitives::Uint::<48, 1>::from(t),
                rampDuration: 0,
            },
            b,
            t,
        ),
        log(
            d.debt_vault,
            &ev::GovSetMaxLiquidationDiscount {
                newDiscount: MAX_DISCOUNT,
            },
            b,
            t,
        ),
        log(
            d.debt_vault,
            &ev::GovSetHookConfig {
                newHookTarget: Address::ZERO,
                newHookedOps: 0,
            },
            b,
            t,
        ),
    ]
}

fn user_logs(d: &Deploy, debt: U256) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 1, T0);
    vec![
        log(
            d.evc,
            &evc::ControllerStatus {
                account: d.alice,
                controller: d.debt_vault,
                enabled: true,
            },
            b,
            t,
        ),
        log(
            d.evc,
            &evc::ControllerStatus {
                account: d.bob,
                controller: d.debt_vault,
                enabled: true,
            },
            b,
            t,
        ),
        log(
            d.evc,
            &evc::CollateralStatus {
                account: d.alice,
                collateral: d.coll_vault,
                enabled: true,
            },
            b,
            t,
        ),
        log(
            d.coll_vault,
            &ev::Deposit {
                sender: d.alice,
                owner: d.alice,
                assets: ALICE_SHARES,
                shares: ALICE_SHARES,
            },
            b,
            t,
        ),
        log(
            d.debt_vault,
            &ev::Borrow {
                account: d.alice,
                assets: debt,
            },
            b,
            t,
        ),
    ]
}

pub fn activity_logs(d: &Deploy) -> Vec<OwnedLog> {
    user_logs(d, ALICE_DEBT_HEALTHY)
}

pub fn activity_logs_liq(d: &Deploy) -> Vec<OwnedLog> {
    user_logs(d, ALICE_DEBT_LIQ)
}

pub fn activity_logs_eq(d: &Deploy) -> Vec<OwnedLog> {
    user_logs(d, ALICE_DEBT_EQ)
}

pub fn store_after(p: &EulerV2, logs: &[OwnedLog]) -> JournalStore {
    let mut st = JournalStore::new();
    for l in logs {
        p.apply_log(&mut st, &l.view()).expect("fixture log folds");
    }
    st
}

pub fn prices(weth_p8: u64, usdc_p8: u64) -> PriceVector {
    PriceVector(vec![
        Price {
            asset: USDC,
            price: Ray::from_raw(U256::from(usdc_p8) * P8_TO_RAY),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        },
        Price {
            asset: WETH_SHARES,
            price: Ray::from_raw(U256::from(weth_p8) * P8_TO_RAY),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        },
    ])
}

pub fn coverage_ranks() -> Vec<(B256, u8)> {
    const DOC: &str = include_str!("../../../../../docs/coverage/euler-v2.md");
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
