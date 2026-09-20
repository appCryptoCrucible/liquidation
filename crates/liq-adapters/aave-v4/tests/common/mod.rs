//! Shared fixtures for the adapter's tests and benches.
//!
//! **Provenance.** No fork is reachable from this WP, so every fixture is
//! built the way the harness allows without one: from the protocol's
//! published rule. Parameters are the pinned repository's own test-suite
//! defaults (`tests/setup/Base.t.sol` @ `40232a0a`: spoke 1's
//! `LiquidationConfig` and the WETH/DAI reserve configs and prices); the
//! state is produced by folding logs whose shape and emission order are
//! transcribed from `Hub.sol`/`Spoke.sol`/`AaveOracle.sol` (`addAsset`,
//! `addSpoke`, `addReserve`, `supply`, `borrow`, `_notifyRiskPremiumUpdate`)
//! through the adapter's own `apply_log`, so the store holds exactly what
//! a real deployment's log stream would leave. Share counts are the chain's
//! at an empty pool (`toAddedSharesDown` and `toDrawnShares` at index `RAY`
//! with `1e6` virtual shares and assets are the identity), and the premium
//! delta is `UserPositionUtils.calculatePremiumDelta` evaluated by hand.
//! Nothing here is a chain observation; fork fixtures are 04C's.

// Test-side arithmetic is the oracle: an overflow here is a failed test, not
// a hot-path hazard, so the workspace's hot-path lints are relaxed.
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

use alloy_primitives::aliases::{U24, U40};
use alloy_primitives::{uint, Address, B256, I256, U256};
use alloy_sol_types::SolEvent;
use liq_adapters_aave_v4::events::{hub, oracle, spoke};
use liq_adapters_aave_v4::{AaveV4, AssetConfig, Config, HubConfig, SourcePin, SpokeConfig};
use liq_protocol::conformance::JournalStore;
use liq_protocol::{DecodedLog, FeedId, Protocol};
use liq_types::fixed::RAY;
use liq_types::{AssetId, MarketId, PositionId, Price, PriceVector, ProtocolId, Ray, SourceKind};

pub const PROTOCOL: ProtocolId = ProtocolId(4);
pub const HUB_MARKET: MarketId = MarketId(100);
pub const SPOKE_MARKET: MarketId = MarketId(200);
pub const WETH: AssetId = AssetId(0);
pub const DAI: AssetId = AssetId(1);
/// Hub asset ids == reserve ids for this deployment (WETH 0, DAI 1); store
/// slots are `reserve + 1`.
pub const WETH_SLOT: u16 = 1;
pub const DAI_SLOT: u16 = 2;

pub const DEPLOY_BLOCK: u64 = 100;
pub const T0: u64 = 1_700_000_000;
/// 30 days after `T0`.
pub const T1: u64 = T0 + 2_592_000;

/// `10^19`: an 8-decimal oracle answer as a RAY price.
pub const P8_TO_RAY: U256 = uint!(10_000_000_000_000_000_000_U256);

// Base.t.sol spoke 1.
pub const TARGET_HF_WAD: u128 = 1_050_000_000_000_000_000;
pub const HF_FOR_MAX_BONUS_WAD: u64 = 700_000_000_000_000_000;
pub const BONUS_FACTOR_BPS: u16 = 20_00;
// Base.t.sol reserve WETH (spoke 1, reserveParams[0]).
pub const WETH_CF: u16 = 80_00;
pub const WETH_MAX_BONUS: u32 = 105_00;
pub const WETH_FEE: u16 = 10_00;
pub const WETH_RISK: u32 = 15_00;
pub const WETH_P8: u64 = 2000_0000_0000;
// Base.t.sol reserve DAI (spoke 1, reserveParams[2]).
pub const DAI_CF: u16 = 78_00;
pub const DAI_MAX_BONUS: u32 = 102_00;
pub const DAI_FEE: u16 = 10_00;
pub const DAI_RISK: u32 = 20_00;
pub const DAI_P8: u64 = 1_0000_0000;

/// Base drawn rates at listing (RAY / year): 2% WETH, 5% DAI.
pub const WETH_RATE: U256 = uint!(20_000_000_000_000_000_000_000_000_U256);
pub const DAI_RATE: U256 = uint!(50_000_000_000_000_000_000_000_000_U256);

/// Alice: 1 WETH supplied as collateral, 1500 DAI drawn at 2% risk premium.
pub const ALICE_WETH: U256 = uint!(1_000_000_000_000_000_000_U256);
pub const ALICE_DAI_DEBT: U256 = uint!(1_500_000_000_000_000_000_000_U256);
pub const ALICE_RISK_PREMIUM_BPS: u64 = 200;
/// `percentMulUp(1500e18, 200) = 30e18`.
pub const ALICE_PREMIUM_SHARES: U256 = uint!(30_000_000_000_000_000_000_U256);
/// Bob: 10 000 DAI supplied, never collateral.
pub const BOB_DAI: U256 = uint!(10_000_000_000_000_000_000_000_U256);

pub const ALICE_ID: PositionId = PositionId(0);
pub const BOB_ID: PositionId = PositionId(1);

pub struct Deploy {
    pub hub: Address,
    pub spoke: Address,
    pub oracle: Address,
    pub weth: Address,
    pub dai: Address,
    pub src_weth: Address,
    pub src_dai: Address,
    pub alice: Address,
    pub bob: Address,
    pub fee_receiver: Address,
    pub ir: Address,
}

impl Deploy {
    pub fn new() -> Self {
        Self {
            hub: Address::repeat_byte(0x11),
            spoke: Address::repeat_byte(0x22),
            oracle: Address::repeat_byte(0x33),
            weth: Address::repeat_byte(0xa0),
            dai: Address::repeat_byte(0xa1),
            src_weth: Address::repeat_byte(0xb0),
            src_dai: Address::repeat_byte(0xb1),
            alice: Address::repeat_byte(0xc1),
            bob: Address::repeat_byte(0xc2),
            fee_receiver: Address::repeat_byte(0xd0),
            ir: Address::repeat_byte(0xd1),
        }
    }

    pub fn config(&self) -> Config {
        Config {
            protocol: PROTOCOL,
            hubs: vec![HubConfig {
                address: self.hub,
                market: HUB_MARKET,
            }],
            spokes: vec![SpokeConfig {
                address: self.spoke,
                market: SPOKE_MARKET,
                oracle: self.oracle,
            }],
            assets: vec![
                AssetConfig {
                    underlying: self.weth,
                    asset: WETH,
                    feed: FeedId(1),
                },
                AssetConfig {
                    underlying: self.dai,
                    asset: DAI,
                    feed: FeedId(2),
                },
            ],
            price_sources: vec![
                SourcePin {
                    spoke: self.spoke,
                    reserve_id: 0,
                    source: self.src_weth,
                },
                SourcePin {
                    spoke: self.spoke,
                    reserve_id: 1,
                    source: self.src_dai,
                },
            ],
            pinned_through: DEPLOY_BLOCK,
        }
    }

    pub fn adapter(&self) -> AaveV4 {
        AaveV4::new(self.config()).expect("fixture config validates")
    }
}

/// An owned log; `view()` borrows it as the router would.
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

fn premium_delta(shares: U256, offset: U256) -> hub::PremiumDelta {
    hub::PremiumDelta {
        sharesDelta: I256::try_from(shares).unwrap(),
        offsetRayDelta: I256::try_from(offset).unwrap(),
        restoredPremiumRay: U256::ZERO,
    }
}

fn spoke_premium_delta(shares: U256, offset: U256) -> spoke::PremiumDelta {
    spoke::PremiumDelta {
        sharesDelta: I256::try_from(shares).unwrap(),
        offsetRayDelta: I256::try_from(offset).unwrap(),
        restoredPremiumRay: U256::ZERO,
    }
}

/// Listing at `DEPLOY_BLOCK`/`T0`, in the chain's emission order.
pub fn listing_logs(d: &Deploy) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK, T0);
    let mut v = Vec::new();
    // Hub.addAsset × 2.
    for (id, underlying, rate) in [(0u8, d.weth, WETH_RATE), (1u8, d.dai, DAI_RATE)] {
        let id = U256::from(id);
        v.push(log(
            d.hub,
            &hub::AddAsset {
                assetId: id,
                underlying,
                decimals: 18,
            },
            b,
            t,
        ));
        v.push(log(
            d.hub,
            &hub::UpdateAssetConfig {
                assetId: id,
                config: hub::AssetConfig {
                    feeReceiver: d.fee_receiver,
                    liquidityFee: 0,
                    irStrategy: d.ir,
                    reinvestmentController: Address::ZERO,
                },
            },
            b,
            t,
        ));
        v.push(log(
            d.hub,
            &hub::UpdateAsset {
                assetId: id,
                drawnIndex: RAY,
                drawnRate: rate,
                accruedFees: U256::ZERO,
            },
            b,
            t,
        ));
        // Hub.addSpoke.
        v.push(log(
            d.hub,
            &hub::AddSpoke {
                assetId: id,
                spoke: d.spoke,
            },
            b,
            t,
        ));
        v.push(log(
            d.hub,
            &hub::UpdateSpokeConfig {
                assetId: id,
                spoke: d.spoke,
                config: hub::SpokeConfig {
                    addCap: U40::from(1_000_000u32),
                    drawCap: U40::from(1_000_000u32),
                    riskPremiumThreshold: U24::from(0u8),
                    active: true,
                    halted: false,
                },
            },
            b,
            t,
        ));
    }
    // Spoke.updateLiquidationConfig.
    v.push(log(
        d.spoke,
        &spoke::UpdateLiquidationConfig {
            config: spoke::LiquidationConfig {
                targetHealthFactor: TARGET_HF_WAD,
                healthFactorForMaxBonus: HF_FOR_MAX_BONUS_WAD,
                liquidationBonusFactor: BONUS_FACTOR_BPS,
            },
        },
        b,
        t,
    ));
    // Spoke.addReserve × 2.
    let reserves = [
        (
            0u8,
            d.src_weth,
            WETH_RISK,
            WETH_CF,
            WETH_MAX_BONUS,
            WETH_FEE,
        ),
        (1u8, d.src_dai, DAI_RISK, DAI_CF, DAI_MAX_BONUS, DAI_FEE),
    ];
    for (id, src, risk, cf, max_bonus, fee) in reserves {
        let id = U256::from(id);
        v.push(log(
            d.oracle,
            &oracle::UpdateReserveSource {
                reserveId: id,
                source: src,
            },
            b,
            t,
        ));
        v.push(log(
            d.spoke,
            &spoke::UpdateReservePriceSource {
                reserveId: id,
                priceSource: src,
            },
            b,
            t,
        ));
        v.push(log(
            d.spoke,
            &spoke::AddReserve {
                reserveId: id,
                assetId: id,
                hub: d.hub,
            },
            b,
            t,
        ));
        v.push(log(
            d.spoke,
            &spoke::UpdateReserveConfig {
                reserveId: id,
                config: spoke::ReserveConfig {
                    collateralRisk: U24::from(risk),
                    paused: false,
                    frozen: false,
                    borrowable: true,
                    receiveSharesEnabled: true,
                },
            },
            b,
            t,
        ));
        v.push(log(
            d.spoke,
            &spoke::AddDynamicReserveConfig {
                reserveId: id,
                dynamicConfigKey: 0,
                config: spoke::DynamicReserveConfig {
                    collateralFactor: cf,
                    maxLiquidationBonus: max_bonus,
                    liquidationFee: fee,
                },
            },
            b,
            t,
        ));
    }
    v
}

/// Alice supplies WETH and enables it; Bob supplies DAI; Alice borrows DAI
/// and is assigned a 2% risk premium. Block `DEPLOY_BLOCK + 1`, time `T0`.
pub fn activity_logs(d: &Deploy) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 1, T0);
    let asset = |id: u8| U256::from(id);
    let mut v = Vec::new();
    // Spoke.supply(WETH) by Alice → Hub.add.
    v.push(log(
        d.hub,
        &hub::UpdateAsset {
            assetId: asset(0),
            drawnIndex: RAY,
            drawnRate: WETH_RATE,
            accruedFees: U256::ZERO,
        },
        b,
        t,
    ));
    v.push(log(
        d.hub,
        &hub::Add {
            assetId: asset(0),
            spoke: d.spoke,
            shares: ALICE_WETH,
            amount: ALICE_WETH,
        },
        b,
        t,
    ));
    v.push(log(
        d.spoke,
        &spoke::Supply {
            reserveId: asset(0),
            caller: d.alice,
            user: d.alice,
            suppliedShares: ALICE_WETH,
            suppliedAmount: ALICE_WETH,
        },
        b,
        t,
    ));
    // Spoke.setUsingAsCollateral(0, true): refresh log precedes the flag log.
    v.push(log(
        d.spoke,
        &spoke::RefreshSingleUserDynamicConfig {
            user: d.alice,
            reserveId: asset(0),
        },
        b,
        t,
    ));
    v.push(log(
        d.spoke,
        &spoke::SetUsingAsCollateral {
            reserveId: asset(0),
            caller: d.alice,
            user: d.alice,
            usingAsCollateral: true,
        },
        b,
        t,
    ));
    // Spoke.supply(DAI) by Bob.
    v.push(log(
        d.hub,
        &hub::UpdateAsset {
            assetId: asset(1),
            drawnIndex: RAY,
            drawnRate: DAI_RATE,
            accruedFees: U256::ZERO,
        },
        b,
        t,
    ));
    v.push(log(
        d.hub,
        &hub::Add {
            assetId: asset(1),
            spoke: d.spoke,
            shares: BOB_DAI,
            amount: BOB_DAI,
        },
        b,
        t,
    ));
    v.push(log(
        d.spoke,
        &spoke::Supply {
            reserveId: asset(1),
            caller: d.bob,
            user: d.bob,
            suppliedShares: BOB_DAI,
            suppliedAmount: BOB_DAI,
        },
        b,
        t,
    ));
    // Spoke.borrow(DAI) by Alice → Hub.draw, then the premium refresh.
    v.push(log(
        d.hub,
        &hub::UpdateAsset {
            assetId: asset(1),
            drawnIndex: RAY,
            drawnRate: DAI_RATE,
            accruedFees: U256::ZERO,
        },
        b,
        t,
    ));
    v.push(log(
        d.hub,
        &hub::Draw {
            assetId: asset(1),
            spoke: d.spoke,
            drawnShares: ALICE_DAI_DEBT,
            drawnAmount: ALICE_DAI_DEBT,
        },
        b,
        t,
    ));
    v.push(log(
        d.spoke,
        &spoke::Borrow {
            reserveId: asset(1),
            caller: d.alice,
            user: d.alice,
            drawnShares: ALICE_DAI_DEBT,
            drawnAmount: ALICE_DAI_DEBT,
        },
        b,
        t,
    ));
    v.push(log(
        d.spoke,
        &spoke::RefreshAllUserDynamicConfig { user: d.alice },
        b,
        t,
    ));
    v.push(log(
        d.spoke,
        &spoke::UpdateUserRiskPremium {
            user: d.alice,
            riskPremium: U256::from(ALICE_RISK_PREMIUM_BPS),
        },
        b,
        t,
    ));
    // offset = premiumShares · drawnIndex − premiumDebtRay(0) at index RAY.
    let offset = ALICE_PREMIUM_SHARES * RAY;
    v.push(log(
        d.hub,
        &hub::RefreshPremium {
            assetId: asset(1),
            spoke: d.spoke,
            premiumDelta: premium_delta(ALICE_PREMIUM_SHARES, offset),
        },
        b,
        t,
    ));
    v.push(log(
        d.spoke,
        &spoke::RefreshPremiumDebt {
            reserveId: asset(1),
            user: d.alice,
            premiumDelta: spoke_premium_delta(ALICE_PREMIUM_SHARES, offset),
        },
        b,
        t,
    ));
    v
}

/// A store with every log in `logs` folded, in order.
pub fn store_after(p: &AaveV4, logs: &[OwnedLog]) -> JournalStore {
    let mut st = JournalStore::new();
    for (i, l) in logs.iter().enumerate() {
        p.apply_log(&mut st, &l.view())
            .unwrap_or_else(|e| panic!("fixture log #{i} refused: {e:?}"));
    }
    st
}

pub fn ray_of_p8(p8: u64) -> Ray {
    Ray::from_raw(U256::from(p8) * P8_TO_RAY)
}

/// Prices for WETH and DAI as 8-decimal oracle answers.
pub fn prices(weth_p8: u64, dai_p8: u64) -> PriceVector {
    let at = |asset: AssetId, p8: u64| Price {
        asset,
        price: ray_of_p8(p8),
        source: SourceKind::Canonical,
        block: DEPLOY_BLOCK + 1,
        ts: T0,
    };
    PriceVector(vec![at(WETH, weth_p8), at(DAI, dai_p8)])
}

/// `DirtySet` rank per `topic0`, read mechanically from the 03C coverage
/// table (`docs/coverage/aave-v4.md`, columns `log topic(s)` and
/// `DirtySet`). `halt` rows rank 0: at or before the pin they fold to
/// `None`, after it they are errors, never a `DirtySet`.
pub fn coverage_ranks() -> Vec<(B256, u8)> {
    const DOC: &str = include_str!("../../../../../docs/coverage/aave-v4.md");
    let mut out = Vec::new();
    for line in DOC.lines() {
        let cols: Vec<&str> = line.split('|').map(str::trim).collect();
        // "| path | function/event | topic | DirtySet | notes |" → 7 pieces.
        if cols.len() < 6 || !cols[3].starts_with("0x") {
            continue;
        }
        let topic: B256 = cols[3].parse().expect("coverage topic parses");
        let rank = match cols[4] {
            "None" | "halt" => 0,
            "Positions" => 1,
            "MarketAccrual" => 2,
            "MarketReprice" => 3,
            other => panic!("unknown DirtySet class {other:?} in coverage table"),
        };
        out.push((topic, rank));
    }
    assert!(out.len() >= 50, "coverage table parsed {} rows", out.len());
    out
}

pub fn rank_of(table: &[(B256, u8)], topic0: B256) -> u8 {
    table
        .iter()
        .find(|(t, _)| *t == topic0)
        .map(|(_, r)| *r)
        .unwrap_or_else(|| panic!("topic {topic0} is not in the coverage table"))
}
