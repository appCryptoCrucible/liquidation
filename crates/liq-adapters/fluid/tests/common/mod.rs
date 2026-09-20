//! Fixtures from Fluid T1/T3 published rules
//! (`vaultT1|T3/coreModule/main.sol` @ `9496626f`).

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

use alloy_primitives::{uint, Address, Bytes, B256, I256, U256};
use alloy_sol_types::{SolCall, SolEvent};
use liq_adapters_fluid::config::{ConfigError, FactoryRpc};
use liq_adapters_fluid::events::{factory, vault};
use liq_adapters_fluid::layout::{VAULT_T1, VAULT_T3};
use liq_adapters_fluid::{AssetConfig, Config, Fluid, VaultPin};
use liq_protocol::conformance::JournalStore;
use liq_protocol::{DecodedLog, FeedId, Protocol};
use liq_types::{AssetId, MarketId, PositionId, Price, PriceVector, ProtocolId, Ray, SourceKind};

pub const PROTOCOL: ProtocolId = ProtocolId(10);
pub const CATALOG: MarketId = MarketId(4000);
pub const MARKET_T1: MarketId = MarketId(4001);
pub const MARKET_T3: MarketId = MarketId(4002);
pub const COLL: AssetId = AssetId(0);
pub const DEBT: AssetId = AssetId(1);
pub const DEPLOY_BLOCK: u64 = 100;
pub const PIN_BLOCK: u64 = 26_015_175;
pub const T0: u64 = 1_700_000_000;
pub const RAY_ONE: U256 = uint!(1_000_000_000_000_000_000_000_000_000_U256);
pub const ETH_USD: U256 = uint!(2_000_000_000_000_000_000_000_000_000_000_U256);
pub const EX_PRICE: U256 = uint!(1_000_000_000_000_U256);
pub const ALICE_COLL: U256 = uint!(1_000_000_000_000_000_000_U256);
pub const ALICE_DEBT_OK: U256 = uint!(1_000_000_000_U256);
pub const ALICE_DEBT_LIQ: U256 = uint!(1_900_000_000_U256);
pub const T1_ID: PositionId = PositionId(0);
pub const T3_ID: PositionId = PositionId(1);
pub const THRESHOLD: u16 = 900;
pub const MAX_LIMIT: u16 = 990;
pub const PENALTY: u16 = 100;

pub struct Deploy {
    pub factory: Address,
    pub vault_t1: Address,
    pub vault_t3: Address,
    pub oracle: Address,
    pub weth: Address,
    pub usdc: Address,
}

impl Deploy {
    pub fn new() -> Self {
        Self {
            factory: Address::repeat_byte(0xf1),
            vault_t1: Address::repeat_byte(0xa1),
            vault_t3: Address::repeat_byte(0xa3),
            oracle: Address::repeat_byte(0xd1),
            weth: Address::repeat_byte(0xc0),
            usdc: Address::repeat_byte(0xc1),
        }
    }

    pub fn config(&self) -> Config {
        Config {
            protocol: PROTOCOL,
            factory: self.factory,
            catalog: CATALOG,
            first_market: MarketId(4001),
            d15_vaults: 2,
            vaults: vec![self.vault_t1, self.vault_t3],
            vault_pins: vec![self.pin_t1(), self.pin_t3()],
            assets: vec![
                AssetConfig {
                    underlying: self.weth,
                    asset: COLL,
                    feed: FeedId(0),
                    decimals: 18,
                },
                AssetConfig {
                    underlying: self.usdc,
                    asset: DEBT,
                    feed: FeedId(0),
                    decimals: 6,
                },
            ],
            pinned_through: DEPLOY_BLOCK,
            live_factory_asserted: false,
        }
    }

    pub fn pin_t1(&self) -> VaultPin {
        VaultPin {
            vault: self.vault_t1,
            vault_id: 1,
            vault_type: VAULT_T1,
            supply0: self.weth,
            supply1: Address::ZERO,
            borrow0: self.usdc,
            borrow1: Address::ZERO,
            supply_decimals0: 18,
            supply_decimals1: 0,
            borrow_decimals0: 6,
            borrow_decimals1: 0,
            liq_threshold: THRESHOLD,
            liq_max_limit: MAX_LIMIT,
            liq_penalty: PENALTY,
            oracle: self.oracle,
        }
    }

    pub fn pin_t3(&self) -> VaultPin {
        VaultPin {
            vault: self.vault_t3,
            vault_id: 2,
            vault_type: VAULT_T3,
            supply0: self.weth,
            supply1: Address::ZERO,
            borrow0: self.usdc,
            borrow1: Address::ZERO,
            supply_decimals0: 18,
            supply_decimals1: 0,
            borrow_decimals0: 6,
            borrow_decimals1: 0,
            liq_threshold: THRESHOLD,
            liq_max_limit: MAX_LIMIT,
            liq_penalty: PENALTY,
            oracle: self.oracle,
        }
    }

    pub fn rpc(&self) -> PinRpc {
        PinRpc {
            factory: self.factory,
            total: U256::from(2u8),
            vault1: self.vault_t1,
        }
    }

    pub fn adapter(&self) -> Fluid {
        let mut cfg = self.config();
        cfg.assert_live_factory(&self.rpc(), DEPLOY_BLOCK)
            .expect("fixture factory assert");
        Fluid::new(cfg).expect("fixture config boots")
    }
}

pub struct PinRpc {
    pub factory: Address,
    pub total: U256,
    pub vault1: Address,
}

impl FactoryRpc for PinRpc {
    fn eth_call(
        &self,
        to: Address,
        data: &[u8],
        _block: u64,
    ) -> core::result::Result<Bytes, ConfigError> {
        use liq_adapters_fluid::events::views::{getVaultAddressCall, totalVaultsCall};
        if to != self.factory {
            return Err(ConfigError::FactoryCall(to));
        }
        let sel = data.get(..4).ok_or(ConfigError::FactoryCall(to))?;
        if sel == totalVaultsCall::SELECTOR {
            return Ok(Bytes::copy_from_slice(&self.total.to_be_bytes::<32>()));
        }
        if sel == getVaultAddressCall::SELECTOR {
            let mut out = [0u8; 32];
            out[12..32].copy_from_slice(self.vault1.as_slice());
            return Ok(Bytes::copy_from_slice(&out));
        }
        Err(ConfigError::FactoryCall(to))
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

fn i256(v: U256) -> I256 {
    I256::try_from(v).expect("fixture amount fits i256")
}

pub fn listing_logs(d: &Deploy) -> Vec<OwnedLog> {
    vec![
        log(
            d.factory,
            &factory::VaultDeployed {
                vault: d.vault_t1,
                vaultId: U256::from(1u8),
            },
            DEPLOY_BLOCK,
            T0,
        ),
        log(
            d.factory,
            &factory::VaultDeployed {
                vault: d.vault_t3,
                vaultId: U256::from(2u8),
            },
            DEPLOY_BLOCK,
            T0,
        ),
        log(
            d.vault_t1,
            &vault::LogUpdateExchangePrice {
                supplyExPrice_: EX_PRICE,
                borrowExPrice_: EX_PRICE,
            },
            DEPLOY_BLOCK,
            T0,
        ),
        log(
            d.vault_t3,
            &vault::LogUpdateExchangePrice {
                supplyExPrice_: EX_PRICE,
                borrowExPrice_: EX_PRICE,
            },
            DEPLOY_BLOCK,
            T0,
        ),
    ]
}

pub fn activity_logs(d: &Deploy, vault: Address, coll: U256, debt: U256) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 1, T0);
    vec![
        log(
            d.factory,
            &factory::NewPositionMinted {
                vault,
                user: Address::repeat_byte(0x11),
                tokenId: U256::from(1u8),
            },
            b,
            t,
        ),
        log(
            vault,
            &vault::LogOperate {
                user_: Address::repeat_byte(0x11),
                nftId_: U256::from(1u8),
                colAmt_: i256(coll),
                debtAmt_: i256(debt),
                to_: Address::repeat_byte(0x11),
            },
            b,
            t,
        ),
    ]
}

pub fn store_after(p: &Fluid, logs: &[OwnedLog]) -> JournalStore {
    let mut st = JournalStore::default();
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
    const DOC: &str = include_str!("../../../../../docs/coverage/fluid.md");
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
