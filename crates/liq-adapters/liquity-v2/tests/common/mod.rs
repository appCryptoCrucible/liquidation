//! Fixtures from Liquity V2 published rules (`TroveManager.sol` @ `c8a5a4ee`).
//! Addresses are `contracts/addresses/1.json` WETH branch at that pin.

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

use alloy_primitives::{address, uint, Address, B256, U256};
use alloy_sol_types::SolEvent;
use liq_adapters_liquity_v2::events::{self as ev};
use liq_adapters_liquity_v2::{AssetConfig, BranchConfig, Config, LiquityV2};
use liq_protocol::conformance::JournalStore;
use liq_protocol::{DecodedLog, FeedId, Protocol};
use liq_types::{AssetId, MarketId, PositionId, Price, PriceVector, ProtocolId, Ray, SourceKind};

pub const PROTOCOL: ProtocolId = ProtocolId(9);
pub const MARKET: MarketId = MarketId(0);
pub const WETH: AssetId = AssetId(0);
pub const BOLD: AssetId = AssetId(1);
pub const WSTETH: AssetId = AssetId(2);
pub const DEPLOY_BLOCK: u64 = 22_516_117;
pub const T0: u64 = 1_700_000_000;
pub const T1: u64 = T0 + 86_400;
pub const T_WEEK: u64 = T0 + 604_800;
pub const WAD: U256 = uint!(1_000_000_000_000_000_000_U256);
pub const RAY_PER_WAD: U256 = uint!(1_000_000_000_U256);
pub const MCR_WETH: u128 = 1_100_000_000_000_000_000;
pub const CCR_WETH: u128 = 1_500_000_000_000_000_000;
pub const MCR_SETH: u128 = 1_200_000_000_000_000_000;
pub const CCR_SETH: u128 = 1_600_000_000_000_000_000;
pub const PENALTY_SP: u128 = 50_000_000_000_000_000;
pub const PENALTY_REDIST_WETH: u128 = 100_000_000_000_000_000;
pub const PENALTY_REDIST_SETH: u128 = 200_000_000_000_000_000;
pub const ALICE_COLL: U256 = uint!(10_000_000_000_000_000_000_U256);
pub const ALICE_DEBT: U256 = uint!(5_000_000_000_000_000_000_000_U256);
pub const ZOMBIE_DEBT: U256 = uint!(1_000_000_000_000_000_000_000_U256);
pub const SP_BOLD: U256 = uint!(1_000_000_000_000_000_000_000_000_U256);
pub const ALICE_ID: PositionId = PositionId(0);
pub const ETH_USD_WAD: U256 = uint!(2_000_000_000_000_000_000_000_U256);
pub const ETH_USD_LIQ_WAD: U256 = uint!(100_000_000_000_000_000_000_U256);
/// `MCR * ALICE_DEBT / ALICE_COLL` — exact; ICR == MCR.
pub const ETH_USD_AT_MCR_WAD: U256 = uint!(550_000_000_000_000_000_000_U256);
pub const BOLD_USD_WAD: U256 = WAD;

pub const BATCH_DEBT: U256 = uint!(3_000_000_000_000_000_000_000_U256);
pub const BATCH_SHARES: U256 = uint!(7_U256);
pub const BATCH_TOTAL: U256 = uint!(10_U256);
pub const BATCH_RATE: U256 = uint!(50_000_000_000_000_000_U256);
pub const BATCH_FEE: U256 = uint!(20_000_000_000_000_000_U256);
pub const BATCH_ENTIRE: U256 = uint!(2_102_819_178_082_191_780_820_U256);
pub const BATCH_MANAGER: Address = address!("0x4444444444444444444444444444444444444444");

pub const TM: Address = address!("0x7bcb64b2c9206a5b699ed43363f6f98d4776cf5a");
pub const BO: Address = address!("0x372abd1810eaf23cb9d941bbe7596dfb2c46bc65");
pub const SP: Address = address!("0x5721cbbd64fc7ae3ef44a0a3f9a790a9264cf9bf");
pub const SORTED: Address = address!("0xa25269e41bd072513849f2e64ad221e84f3063f4");
pub const REGISTRY: Address = address!("0x20f7c9ad66983f6523a0881d0f82406541417526");
pub const PRICE_FEED: Address = address!("0xcc5f8102eb670c89a4a3c567c13851260303c24f");
pub const WETH_TOKEN: Address = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
pub const BOLD_TOKEN: Address = address!("0x6440f144b7e50d6a8439336510312d2f54beb01d");

pub const MARKET_WSTETH: MarketId = MarketId(1);
pub const TM_WSTETH: Address = address!("0xa2895d6a3bf110561dfe4b71ca539d84e1928b22");
pub const BO_WSTETH: Address = address!("0xa741a32f9dcfe6adba088fd0f97e90742d7d5da3");
pub const SP_WSTETH: Address = address!("0x9502b7c397e9aa22fe9db7ef7daf21cd2aebe56b");
pub const SORTED_WSTETH: Address = address!("0x84eb85a8c25049255614f0536bea8f31682e86f1");
pub const REGISTRY_WSTETH: Address = address!("0x8d733f7ea7c23cbea7c613b6ebd845d46d3aac54");
pub const PRICE_FEED_WSTETH: Address = address!("0xe7aa2ba9e086a379d3beb224098bc634a46e314e");
pub const WSTETH_TOKEN: Address = address!("0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0");

pub struct Deploy {
    pub alice: Address,
    pub trove_id: U256,
}

impl Deploy {
    pub fn new() -> Self {
        let alice = Address::repeat_byte(0xc1);
        Self {
            alice,
            trove_id: pack_trove_id(alice, 0xab),
        }
    }

    pub fn config(&self) -> Config {
        Config {
            protocol: PROTOCOL,
            bold: AssetConfig {
                underlying: BOLD_TOKEN,
                asset: BOLD,
                feed: FeedId(30),
                decimals: 18,
            },
            weth: AssetConfig {
                underlying: WETH_TOKEN,
                asset: WETH,
                feed: FeedId(27),
                decimals: 18,
            },
            branches: vec![BranchConfig {
                coll_symbol: "WETH".to_owned(),
                market: MARKET,
                trove_manager: TM,
                borrower_operations: BO,
                stability_pool: SP,
                sorted_troves: SORTED,
                addresses_registry: REGISTRY,
                price_feed: PRICE_FEED,
                coll_token: WETH_TOKEN,
                coll_asset: WETH,
                coll_decimals: 18,
                coll_feed: FeedId(27),
                mcr: MCR_WETH,
                ccr: CCR_WETH,
                penalty_sp: PENALTY_SP,
                penalty_redist: PENALTY_REDIST_WETH,
            }],
            pinned_through: DEPLOY_BLOCK,
        }
    }

    pub fn adapter(&self) -> LiquityV2 {
        LiquityV2::new(self.config()).expect("fixture config validates")
    }

    pub fn wsteth_config(&self) -> Config {
        Config {
            protocol: PROTOCOL,
            bold: AssetConfig {
                underlying: BOLD_TOKEN,
                asset: BOLD,
                feed: FeedId(30),
                decimals: 18,
            },
            weth: AssetConfig {
                underlying: WETH_TOKEN,
                asset: WETH,
                feed: FeedId(27),
                decimals: 18,
            },
            branches: vec![BranchConfig {
                coll_symbol: "wstETH".to_owned(),
                market: MARKET_WSTETH,
                trove_manager: TM_WSTETH,
                borrower_operations: BO_WSTETH,
                stability_pool: SP_WSTETH,
                sorted_troves: SORTED_WSTETH,
                addresses_registry: REGISTRY_WSTETH,
                price_feed: PRICE_FEED_WSTETH,
                coll_token: WSTETH_TOKEN,
                coll_asset: WSTETH,
                coll_decimals: 18,
                coll_feed: FeedId(28),
                mcr: MCR_SETH,
                ccr: CCR_SETH,
                penalty_sp: PENALTY_SP,
                penalty_redist: PENALTY_REDIST_SETH,
            }],
            pinned_through: DEPLOY_BLOCK,
        }
    }

    pub fn wsteth_adapter(&self) -> LiquityV2 {
        LiquityV2::new(self.wsteth_config()).expect("wsteth fixture config validates")
    }
}

pub fn pack_trove_id(user: Address, hi: u8) -> U256 {
    let mut b = [0u8; 32];
    b[0] = hi;
    b[12..32].copy_from_slice(user.as_slice());
    U256::from_be_bytes(b)
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
    listing_on(
        TM,
        SP,
        d.trove_id,
        ALICE_DEBT,
        ALICE_COLL,
        ev::op::OPEN_TROVE,
        SP_BOLD,
    )
}

pub fn listing_on(
    tm: Address,
    sp: Address,
    trove_id: U256,
    debt: U256,
    coll: U256,
    operation: u8,
    sp_bold: U256,
) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK, T0);
    vec![
        log(
            tm,
            &ev::TroveUpdated {
                troveId: trove_id,
                debt,
                coll,
                stake: coll,
                annualInterestRate: U256::ZERO,
                snapshotOfTotalCollRedist: U256::ZERO,
                snapshotOfTotalDebtRedist: U256::ZERO,
            },
            b,
            t,
        ),
        log(
            tm,
            &ev::TroveOperation {
                troveId: trove_id,
                operation,
                annualInterestRate: U256::ZERO,
                debtIncreaseFromRedist: U256::ZERO,
                debtIncreaseFromUpfrontFee: U256::ZERO,
                debtChangeFromOperation: alloy_primitives::I256::try_from(debt).unwrap(),
                collIncreaseFromRedist: U256::ZERO,
                collChangeFromOperation: alloy_primitives::I256::try_from(coll).unwrap(),
            },
            b,
            t,
        ),
        log(
            sp,
            &ev::StabilityPoolBoldBalanceUpdated {
                newBalance: sp_bold,
            },
            b,
            t,
        ),
    ]
}

pub fn batch_logs(d: &Deploy) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK, T0);
    vec![
        log(
            TM,
            &ev::BatchedTroveUpdated {
                troveId: d.trove_id,
                interestBatchManager: BATCH_MANAGER,
                batchDebtShares: BATCH_SHARES,
                coll: BATCH_ENTIRE,
                stake: BATCH_ENTIRE,
                snapshotOfTotalCollRedist: U256::ZERO,
                snapshotOfTotalDebtRedist: U256::ZERO,
            },
            b,
            t,
        ),
        log(
            TM,
            &ev::TroveOperation {
                troveId: d.trove_id,
                operation: ev::op::OPEN_TROVE_AND_JOIN_BATCH,
                annualInterestRate: BATCH_RATE,
                debtIncreaseFromRedist: U256::ZERO,
                debtIncreaseFromUpfrontFee: U256::ZERO,
                debtChangeFromOperation: alloy_primitives::I256::try_from(BATCH_DEBT).unwrap(),
                collIncreaseFromRedist: U256::ZERO,
                collChangeFromOperation: alloy_primitives::I256::try_from(BATCH_ENTIRE).unwrap(),
            },
            b,
            t,
        ),
        log(
            TM,
            &ev::BatchUpdated {
                interestBatchManager: BATCH_MANAGER,
                operation: 0,
                debt: BATCH_DEBT,
                coll: BATCH_ENTIRE,
                annualInterestRate: BATCH_RATE,
                annualManagementFee: BATCH_FEE,
                totalDebtShares: BATCH_TOTAL,
                debtIncreaseFromUpfrontFee: U256::ZERO,
            },
            b,
            t,
        ),
        log(
            SP,
            &ev::StabilityPoolBoldBalanceUpdated {
                newBalance: SP_BOLD,
            },
            b,
            t,
        ),
    ]
}

pub fn activity_logs(d: &Deploy) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK, T0);
    vec![log(
        TM,
        &ev::TroveUpdated {
            troveId: d.trove_id,
            debt: ALICE_DEBT,
            coll: ALICE_COLL,
            stake: ALICE_COLL,
            annualInterestRate: U256::ZERO,
            snapshotOfTotalCollRedist: U256::ZERO,
            snapshotOfTotalDebtRedist: U256::ZERO,
        },
        b,
        t,
    )]
}

pub fn store_after(p: &LiquityV2, logs: &[OwnedLog]) -> JournalStore {
    let mut st = JournalStore::new();
    for l in logs {
        p.apply_log(&mut st, &l.view()).expect("fixture log folds");
    }
    st
}

pub fn prices(eth_wad: U256, bold_wad: U256) -> PriceVector {
    PriceVector(vec![
        Price {
            asset: WETH,
            price: Ray::from_raw(eth_wad * RAY_PER_WAD),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        },
        Price {
            asset: BOLD,
            price: Ray::from_raw(bold_wad * RAY_PER_WAD),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        },
    ])
}

pub fn prices_wsteth(wsteth_wad: U256, bold_wad: U256, weth_wad: U256) -> PriceVector {
    PriceVector(vec![
        Price {
            asset: WETH,
            price: Ray::from_raw(weth_wad * RAY_PER_WAD),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        },
        Price {
            asset: BOLD,
            price: Ray::from_raw(bold_wad * RAY_PER_WAD),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        },
        Price {
            asset: WSTETH,
            price: Ray::from_raw(wsteth_wad * RAY_PER_WAD),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        },
    ])
}

pub fn coverage_ranks() -> Vec<(B256, u8)> {
    const DOC: &str = include_str!("../../../../../docs/coverage/liquity-v2.md");
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
    assert!(out.len() >= 20, "coverage table parsed {} rows", out.len());
    out
}

pub fn rank_of(ranks: &[(B256, u8)], topic0: B256) -> u8 {
    ranks
        .iter()
        .find(|(t, _)| *t == topic0)
        .map(|(_, r)| *r)
        .unwrap_or_else(|| panic!("topic {topic0} is not in the coverage table"))
}
