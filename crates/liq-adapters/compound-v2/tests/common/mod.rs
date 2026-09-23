//! Fixtures for Compound V2 pin math (`a3214f67`). Incentive is **1.1e18**,
//! not a `1.08` constant.

#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::inconsistent_digit_grouping,
    clippy::cast_possible_truncation
)]

use alloy_primitives::{address, uint, Address, Bytes, B256, U256};
use alloy_sol_types::{SolCall, SolEvent};
use liq_adapters_compound_v2::config::{
    AssetConfig, CTokenPin, Config, ConfigError, ForkConfig, RegistryRpc, FAMILY_PROTOCOL,
    OFFICIAL_MARKET, OFFICIAL_UNITROLLER,
};
use liq_adapters_compound_v2::events::views::{
    closeFactorMantissaCall, liquidationIncentiveMantissaCall, oracleCall, underlyingCall,
};
use liq_adapters_compound_v2::events::{comptroller as cmp, ctoken};
use liq_adapters_compound_v2::CompoundV2;
use liq_protocol::conformance::JournalStore;
use liq_protocol::{DecodedLog, FeedId, Protocol};
use liq_types::{AssetId, PositionId, Price, PriceVector, Ray, SourceKind};

pub const PROTOCOL: liq_types::ProtocolId = FAMILY_PROTOCOL;
pub const MARKET: liq_types::MarketId = OFFICIAL_MARKET;
pub const NATIVE: AssetId = AssetId(0);
pub const USDC: AssetId = AssetId(1);
pub const DEPLOY_BLOCK: u64 = 100;
pub const T0: u64 = 1_700_000_000;
pub const WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
pub const USDC_TOKEN: Address = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
pub const CETH: Address = address!("0x4Ddc2D193948926D02f9B1fE9e1daa0718270ED5");
pub const CUSDC: Address = address!("0x39AA39c021dfbaE8faC545936693aC917d5E7563");
pub const ORACLE: Address = address!("0x50ce56A3239671Ab62f185704Bfc9Bb362Aa2A24");
/// Fixture close factor 0.5e18 (within pin bounds). Not a global constant.
pub const CLOSE_FACTOR: u128 = 500_000_000_000_000_000;
/// Fixture incentive 1.1e18 — proves seize math is not hardcoded 1.08.
pub const INCENTIVE: u128 = 1_100_000_000_000_000_000;
pub const COLLATERAL_FACTOR: u128 = 750_000_000_000_000_000;
pub const RAY_ONE: U256 = uint!(1_000_000_000_000_000_000_000_000_000_U256);
pub const ALICE_COLL: U256 = uint!(100_000_000_000_000_000_000_U256);
pub const ALICE_DEBT_OK: U256 = uint!(50_000_000_000_000_000_000_U256);
pub const ALICE_DEBT_LIQ: U256 = uint!(80_000_000_000_000_000_000_U256);
pub const BOB_ID: PositionId = PositionId(0);
pub const ALICE_ID: PositionId = PositionId(1);
pub const BOB_COLL: U256 = uint!(200_000_000_000_000_000_000_U256);

pub struct Deploy {
    pub comptroller: Address,
    pub oracle: Address,
    pub ceth: Address,
    pub cusdc: Address,
    pub weth: Address,
    pub usdc: Address,
    pub alice: Address,
    pub bob: Address,
}

impl Deploy {
    pub fn new() -> Self {
        Self {
            comptroller: OFFICIAL_UNITROLLER,
            oracle: ORACLE,
            ceth: CETH,
            cusdc: CUSDC,
            weth: WETH,
            usdc: USDC_TOKEN,
            alice: Address::repeat_byte(0x11),
            bob: Address::repeat_byte(0x12),
        }
    }

    pub fn config(&self) -> Config {
        Config {
            protocol: PROTOCOL,
            forks: vec![ForkConfig {
                comptroller: self.comptroller,
                close_factor_mantissa: CLOSE_FACTOR,
                liquidation_incentive_mantissa: INCENTIVE,
                oracle: self.oracle,
                native: self.weth,
                ctokens: vec![
                    CTokenPin {
                        ctoken: self.ceth,
                        underlying: Address::ZERO,
                    },
                    CTokenPin {
                        ctoken: self.cusdc,
                        underlying: self.usdc,
                    },
                ],
            }],
            interned: vec![(self.comptroller, MARKET)],
            assets: vec![
                AssetConfig {
                    underlying: self.weth,
                    asset: NATIVE,
                    feed: FeedId(0),
                    decimals: 18,
                },
                AssetConfig {
                    underlying: self.usdc,
                    asset: USDC,
                    feed: FeedId(0),
                    decimals: 18,
                },
            ],
            pinned_through: DEPLOY_BLOCK,
            live_registry_asserted: false,
        }
    }

    pub fn adapter(&self) -> CompoundV2 {
        let cfg = assert_pin_registry(self.config());
        CompoundV2::new(cfg).expect("fixture config boots")
    }
}

pub struct PinRpc {
    rows: Vec<(Address, [u8; 4], Bytes)>,
}

impl RegistryRpc for PinRpc {
    fn eth_call(
        &self,
        to: Address,
        data: &[u8],
        _block: u64,
    ) -> core::result::Result<Bytes, ConfigError> {
        let sel = data.get(..4).ok_or(ConfigError::RegistryCall(to))?;
        for (addr, s, v) in &self.rows {
            if *addr == to && s.as_slice() == sel {
                return Ok(v.clone());
            }
        }
        Err(ConfigError::RegistryCall(to))
    }
}

fn word_u256(v: U256) -> Bytes {
    Bytes::copy_from_slice(&v.to_be_bytes::<32>())
}

fn word_addr(a: Address) -> Bytes {
    let mut b = [0u8; 32];
    b[12..32].copy_from_slice(a.as_slice());
    Bytes::copy_from_slice(&b)
}

pub fn pin_rpc_from_cfg(cfg: &Config) -> PinRpc {
    let mut rows = Vec::new();
    for f in &cfg.forks {
        rows.push((
            f.comptroller,
            closeFactorMantissaCall::SELECTOR,
            word_u256(U256::from(f.close_factor_mantissa)),
        ));
        rows.push((
            f.comptroller,
            liquidationIncentiveMantissaCall::SELECTOR,
            word_u256(U256::from(f.liquidation_incentive_mantissa)),
        ));
        rows.push((f.comptroller, oracleCall::SELECTOR, word_addr(f.oracle)));
        for c in &f.ctokens {
            if c.underlying == Address::ZERO {
                continue;
            }
            rows.push((c.ctoken, underlyingCall::SELECTOR, word_addr(c.underlying)));
        }
    }
    PinRpc { rows }
}

pub fn assert_pin_registry(mut cfg: Config) -> Config {
    let rpc = pin_rpc_from_cfg(&cfg);
    cfg.assert_live_registry(&rpc, cfg.pinned_through)
        .expect("fixture pin views equal config");
    cfg
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
        log(d.comptroller, &cmp::MarketListed { cToken: d.ceth }, b, t),
        log(d.comptroller, &cmp::MarketListed { cToken: d.cusdc }, b, t),
        log(
            d.comptroller,
            &cmp::NewCloseFactor {
                oldCloseFactorMantissa: U256::ZERO,
                newCloseFactorMantissa: U256::from(CLOSE_FACTOR),
            },
            b,
            t,
        ),
        log(
            d.comptroller,
            &cmp::NewLiquidationIncentive {
                oldLiquidationIncentiveMantissa: U256::ZERO,
                newLiquidationIncentiveMantissa: U256::from(INCENTIVE),
            },
            b,
            t,
        ),
        log(
            d.ceth,
            &ctoken::NewReserveFactor {
                oldReserveFactorMantissa: U256::ZERO,
                newReserveFactorMantissa: U256::ZERO,
            },
            b,
            t,
        ),
        log(
            d.cusdc,
            &ctoken::NewReserveFactor {
                oldReserveFactorMantissa: U256::ZERO,
                newReserveFactorMantissa: U256::ZERO,
            },
            b,
            t,
        ),
        log(
            d.comptroller,
            &cmp::NewCollateralFactor {
                cToken: d.ceth,
                oldCollateralFactorMantissa: U256::ZERO,
                newCollateralFactorMantissa: U256::from(COLLATERAL_FACTOR),
            },
            b,
            t,
        ),
        log(
            d.ceth,
            &ctoken::AccrueInterest {
                cashPrior: U256::ZERO,
                interestAccumulated: U256::ZERO,
                borrowIndex: U256::from(1_000_000_000_000_000_000u64),
                totalBorrows: U256::ZERO,
            },
            b,
            t,
        ),
        log(
            d.cusdc,
            &ctoken::AccrueInterest {
                cashPrior: uint!(1_000_000_000_000_000_000_000_U256),
                interestAccumulated: U256::ZERO,
                borrowIndex: U256::from(1_000_000_000_000_000_000u64),
                totalBorrows: U256::ZERO,
            },
            b,
            t,
        ),
    ]
}

/// `CTokenInterface.mintFresh` (pin `a3214f67`) emits BOTH of these, in this
/// order:
///
/// ```solidity
/// emit Mint(minter, actualMintAmount, mintTokens);
/// emit Transfer(address(this), minter, mintTokens);
/// ```
///
/// A fixture that emitted only `Mint` is what let the double-count (P2) live:
/// the adapter credited the minter from `Mint` *and* from `Transfer`, so every
/// supplier's balance was exactly 2x on chain while the fixture — seeing one
/// event — agreed with it. Emitting the pair is the whole point; do not drop
/// the `Transfer` to make a test simpler.
fn mint_pair(ctoken: Address, minter: Address, amount: U256) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 1, T0);
    vec![
        log(
            ctoken,
            &ctoken::Mint {
                minter,
                mintAmount: amount,
                mintTokens: amount,
            },
            b,
            t,
        ),
        log(
            ctoken,
            &ctoken::Transfer {
                from: ctoken,
                to: minter,
                amount,
            },
            b,
            t,
        ),
    ]
}

pub fn activity_logs(d: &Deploy, alice_debt: U256) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 1, T0);
    let mut out: Vec<OwnedLog> = Vec::new();

    out.push(log(
        d.comptroller,
        &cmp::MarketEntered {
            cToken: d.ceth,
            account: d.bob,
        },
        b,
        t,
    ));
    out.extend(mint_pair(d.ceth, d.bob, BOB_COLL));

    out.push(log(
        d.comptroller,
        &cmp::MarketEntered {
            cToken: d.ceth,
            account: d.alice,
        },
        b,
        t,
    ));
    out.push(log(
        d.comptroller,
        &cmp::MarketEntered {
            cToken: d.cusdc,
            account: d.alice,
        },
        b,
        t,
    ));
    out.extend(mint_pair(d.ceth, d.alice, ALICE_COLL));

    out.push(log(
        d.cusdc,
        &ctoken::Borrow {
            borrower: d.alice,
            borrowAmount: alice_debt,
            accountBorrows: alice_debt,
            totalBorrows: alice_debt,
        },
        b,
        t,
    ));
    out
}

pub fn store_after(p: &CompoundV2, logs: &[OwnedLog]) -> JournalStore {
    let mut st = JournalStore::new();
    for l in logs {
        p.apply_log(&mut st, &l.view()).expect("fixture log folds");
    }
    st
}

pub fn full_store(d: &Deploy, debt: U256) -> (CompoundV2, JournalStore) {
    let p = d.adapter();
    let mut logs = listing_logs(d);
    logs.extend(activity_logs(d, debt));
    let st = store_after(&p, &logs);
    (p, st)
}

pub fn prices(native: U256, usdc: U256) -> PriceVector {
    PriceVector(vec![
        Price {
            asset: NATIVE,
            price: Ray::from_raw(native),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        },
        Price {
            asset: USDC,
            price: Ray::from_raw(usdc),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        },
    ])
}

pub fn coverage_ranks() -> Vec<(B256, u8)> {
    const DOC: &str = include_str!("../../../../../docs/coverage/compound-v2.md");
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
    assert!(out.len() >= 12, "coverage table parsed {} rows", out.len());
    out
}

pub fn rank_of(ranks: &[(B256, u8)], topic0: B256) -> u8 {
    ranks
        .iter()
        .find(|(t, _)| *t == topic0)
        .map(|(_, r)| *r)
        .unwrap_or_else(|| panic!("topic {topic0} is not in the coverage table"))
}
