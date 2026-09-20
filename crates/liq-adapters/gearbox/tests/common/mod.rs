//! Fixtures from Gearbox V3 pin rules
//! (`CreditFacadeV3` / `CreditLogic` / `CollateralLogic` @ `510fc654`).

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

use alloy_primitives::aliases::{I96, U24, U40};
use alloy_primitives::{uint, Address, Bytes, B256, U256};
use alloy_sol_types::{SolCall, SolEvent};
use liq_adapters_gearbox::config::{
    AssetConfig, Config, ConfigError, Fees, ManagerConfig, RegistryRpc, TokenConfig,
};
use liq_adapters_gearbox::events::views::ICreditManagerV3::{feesReturn, ltParamsReturn};
use liq_adapters_gearbox::events::views::{
    IContractsRegister, ICreditFacadeV3, ICreditManagerV3, IPoolV3,
};
use liq_adapters_gearbox::events::{facade, factory, pool, quota};
use liq_adapters_gearbox::math::STATIC_LT_RAMP_START;
use liq_adapters_gearbox::{GearboxV3, CATALOG_MARKET, FIRST_MANAGER_MARKET, PROTOCOL};
use liq_protocol::conformance::JournalStore;
use liq_protocol::{DecodedLog, FeedId, Protocol};
use liq_types::{AssetId, MarketId, PositionId, Price, PriceVector, Ray, SourceKind};

pub const MARKET: MarketId = FIRST_MANAGER_MARKET;
pub const UNDERLYING: AssetId = AssetId(0);
pub const COLL: AssetId = AssetId(1);
pub const DEPLOY_BLOCK: u64 = 100;
pub const T0: u64 = 1_700_000_000;
pub const RAY_ONE: U256 = uint!(1_000_000_000_000_000_000_000_000_000_U256);
pub const FEE_INTEREST: u16 = 1_000;
pub const FEE_LIQ: u16 = 150;
pub const DISCOUNT: u16 = 9_500;
pub const FEE_LIQ_EXP: u16 = 100;
pub const DISCOUNT_EXP: u16 = 9_700;
pub const LT_UNDERLYING: u16 = 9_350;
pub const LT_COLL: u16 = 8_500;
pub const ALICE_COLL: U256 = uint!(200_000_000_000_000_000_000_U256);
pub const ALICE_DEBT_OK: U256 = uint!(50_000_000_U256);
pub const ALICE_COLL_LIQ: U256 = uint!(100_000_000_000_000_000_000_U256);
pub const ALICE_DEBT_LIQ: U256 = uint!(100_000_000_U256);
pub const QUOTA: i128 = 1_000_000_000_000_000;
pub const ALICE_ID: PositionId = PositionId(0);

pub struct Deploy {
    pub register: Address,
    pub manager: Address,
    pub facade: Address,
    pub pool: Address,
    pub configurator: Address,
    pub factory: Address,
    pub quota_keeper: Address,
    pub underlying: Address,
    pub coll: Address,
    pub alice: Address,
}

impl Deploy {
    pub fn new() -> Self {
        Self {
            register: Address::repeat_byte(0xf0),
            manager: Address::repeat_byte(0xa1),
            facade: Address::repeat_byte(0xa2),
            pool: Address::repeat_byte(0xa3),
            configurator: Address::repeat_byte(0xa4),
            factory: Address::repeat_byte(0xa5),
            quota_keeper: Address::repeat_byte(0xa6),
            underlying: Address::repeat_byte(0xc0),
            coll: Address::repeat_byte(0xc1),
            alice: Address::repeat_byte(0x11),
        }
    }

    pub fn tokens(&self) -> Vec<TokenConfig> {
        vec![
            TokenConfig {
                token: self.underlying,
                mask: 1,
                slot: 0,
                lt_initial: LT_UNDERLYING,
                lt_final: LT_UNDERLYING,
                ramp_start: STATIC_LT_RAMP_START,
                ramp_duration: 0,
                asset: UNDERLYING,
                feed: FeedId(0),
                decimals: 6,
            },
            TokenConfig {
                token: self.coll,
                mask: 2,
                slot: 1,
                lt_initial: LT_COLL,
                lt_final: LT_COLL,
                ramp_start: STATIC_LT_RAMP_START,
                ramp_duration: 0,
                asset: COLL,
                feed: FeedId(0),
                decimals: 18,
            },
        ]
    }

    pub fn manager_config(&self, expirable: bool, expiration_date: u64) -> ManagerConfig {
        ManagerConfig {
            market: MARKET,
            manager: self.manager,
            facade: self.facade,
            pool: self.pool,
            configurator: self.configurator,
            factory: self.factory,
            quota_keeper: self.quota_keeper,
            underlying: self.underlying,
            fees: Fees {
                fee_interest: FEE_INTEREST,
                fee_liquidation: FEE_LIQ,
                liquidation_discount: DISCOUNT,
                fee_liquidation_expired: FEE_LIQ_EXP,
                liquidation_discount_expired: DISCOUNT_EXP,
            },
            lt_underlying: LT_UNDERLYING,
            expirable,
            expiration_date,
            quoted_tokens_mask: 2,
            tokens: self.tokens(),
        }
    }

    pub fn config(&self) -> Config {
        Config {
            protocol: PROTOCOL,
            register: self.register,
            catalog: CATALOG_MARKET,
            first_market: FIRST_MANAGER_MARKET,
            expected_managers: 1,
            managers: vec![self.manager_config(false, 0)],
            assets: vec![
                AssetConfig {
                    underlying: self.underlying,
                    asset: UNDERLYING,
                    feed: FeedId(0),
                    decimals: 6,
                },
                AssetConfig {
                    underlying: self.coll,
                    asset: COLL,
                    feed: FeedId(0),
                    decimals: 18,
                },
            ],
            pinned_through: DEPLOY_BLOCK,
            live_fees_asserted: true,
        }
    }

    pub fn adapter(&self) -> GearboxV3 {
        GearboxV3::new(self.config()).expect("fixture config validates")
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
        &factory::AddCreditManager {
            creditManager: d.manager,
            masterCreditAccount: Address::repeat_byte(0x30),
        },
        DEPLOY_BLOCK,
        T0,
    )]
}

pub fn activity_logs_pos(d: &Deploy, coll: U256, debt: U256) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 1, T0);
    let mut out = vec![log(
        d.facade,
        &facade::OpenCreditAccount {
            creditAccount: d.alice,
            onBehalfOf: d.alice,
            caller: d.alice,
            referralCode: U256::ZERO,
        },
        b,
        t,
    )];
    if !coll.is_zero() {
        out.push(log(
            d.facade,
            &facade::AddCollateral {
                creditAccount: d.alice,
                token: d.coll,
                amount: coll,
            },
            b,
            t,
        ));
        out.push(log(
            d.quota_keeper,
            &quota::UpdateQuota {
                creditAccount: d.alice,
                token: d.coll,
                quotaChange: I96::try_from(QUOTA).expect("quota fits i96"),
            },
            b,
            t,
        ));
    }
    if !debt.is_zero() {
        out.push(log(
            d.pool,
            &pool::Borrow {
                creditManager: d.manager,
                creditAccount: d.alice,
                amount: debt,
            },
            b,
            t,
        ));
    }
    out
}

pub fn activity_logs(d: &Deploy, alice_debt: U256) -> Vec<OwnedLog> {
    activity_logs_pos(d, ALICE_COLL, alice_debt)
}

pub fn store_after(p: &GearboxV3, logs: &[OwnedLog]) -> JournalStore {
    let mut st = JournalStore::new();
    for l in logs {
        p.apply_log(&mut st, &l.view()).expect("fixture log folds");
    }
    st
}

pub fn prices(coll: U256, debt: U256) -> PriceVector {
    PriceVector(vec![
        Price {
            asset: UNDERLYING,
            price: Ray::from_raw(debt),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        },
        Price {
            asset: COLL,
            price: Ray::from_raw(coll),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        },
    ])
}

pub fn coverage_ranks() -> Vec<(B256, u8)> {
    const DOC: &str = include_str!("../../../../../docs/coverage/gearbox.md");
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

pub struct MockRpc {
    pub calls: Vec<(Address, Vec<u8>, Bytes)>,
}

impl RegistryRpc for MockRpc {
    fn eth_call(
        &self,
        to: Address,
        data: &[u8],
        _block: u64,
    ) -> core::result::Result<Bytes, ConfigError> {
        for (addr, d, ret) in &self.calls {
            if *addr == to && d.as_slice() == data {
                return Ok(ret.clone());
            }
        }
        Err(ConfigError::RegistryCall(to))
    }
}

pub fn mock_registry(d: &Deploy) -> MockRpc {
    let mut calls = Vec::new();
    calls.push((
        d.register,
        IContractsRegister::getCreditManagersCall {}.abi_encode(),
        Bytes::from(
            IContractsRegister::getCreditManagersCall::abi_encode_returns(&vec![d.manager]),
        ),
    ));
    calls.push((
        d.register,
        IContractsRegister::isCreditManagerCall(d.manager).abi_encode(),
        Bytes::from(IContractsRegister::isCreditManagerCall::abi_encode_returns(
            &true,
        )),
    ));
    calls.push((
        d.manager,
        ICreditManagerV3::underlyingCall {}.abi_encode(),
        pad_addr(d.underlying),
    ));
    calls.push((
        d.manager,
        ICreditManagerV3::creditFacadeCall {}.abi_encode(),
        pad_addr(d.facade),
    ));
    calls.push((
        d.manager,
        ICreditManagerV3::creditConfiguratorCall {}.abi_encode(),
        pad_addr(d.configurator),
    ));
    calls.push((
        d.manager,
        ICreditManagerV3::accountFactoryCall {}.abi_encode(),
        pad_addr(d.factory),
    ));
    calls.push((
        d.manager,
        ICreditManagerV3::poolCall {}.abi_encode(),
        pad_addr(d.pool),
    ));
    calls.push((
        d.pool,
        IPoolV3::poolQuotaKeeperCall {}.abi_encode(),
        pad_addr(d.quota_keeper),
    ));
    let fees = ICreditManagerV3::feesCall::abi_encode_returns(&feesReturn {
        feeInterest: FEE_INTEREST,
        feeLiquidation: FEE_LIQ,
        liquidationDiscount: DISCOUNT,
        feeLiquidationExpired: FEE_LIQ_EXP,
        liquidationDiscountExpired: DISCOUNT_EXP,
    });
    calls.push((
        d.manager,
        ICreditManagerV3::feesCall {}.abi_encode(),
        Bytes::from(fees),
    ));
    calls.push((
        d.manager,
        ICreditManagerV3::collateralTokensCountCall {}.abi_encode(),
        Bytes::from(ICreditManagerV3::collateralTokensCountCall::abi_encode_returns(&2u8)),
    ));
    for (mask, token, lt) in [(1u64, d.underlying, LT_UNDERLYING), (2u64, d.coll, LT_COLL)] {
        calls.push((
            d.manager,
            ICreditManagerV3::getTokenByMaskCall {
                tokenMask: U256::from(mask),
            }
            .abi_encode(),
            pad_addr(token),
        ));
        calls.push((
            d.manager,
            ICreditManagerV3::ltParamsCall { token }.abi_encode(),
            Bytes::from(ICreditManagerV3::ltParamsCall::abi_encode_returns(
                &ltParamsReturn {
                    ltInitial: lt,
                    ltFinal: lt,
                    timestampRampStart: U40::MAX,
                    rampDuration: U24::ZERO,
                },
            )),
        ));
    }
    calls.push((
        d.manager,
        ICreditManagerV3::quotedTokensMaskCall {}.abi_encode(),
        Bytes::from(ICreditManagerV3::quotedTokensMaskCall::abi_encode_returns(
            &U256::from(2u8),
        )),
    ));
    calls.push((
        d.facade,
        ICreditFacadeV3::expirableCall {}.abi_encode(),
        Bytes::from(ICreditFacadeV3::expirableCall::abi_encode_returns(&false)),
    ));
    calls.push((
        d.facade,
        ICreditFacadeV3::expirationDateCall {}.abi_encode(),
        Bytes::from(ICreditFacadeV3::expirationDateCall::abi_encode_returns(
            &U40::ZERO,
        )),
    ));
    MockRpc { calls }
}

fn pad_addr(a: Address) -> Bytes {
    let mut b = [0u8; 32];
    b[12..].copy_from_slice(a.as_slice());
    Bytes::copy_from_slice(&b)
}
