//! WP 10B: encode ⇄ `liq_wire::wire` decode, ≥256 cases.
//! Solidity decode is `contracts/test/encoding/RoundTrip.t.sol` (local forge,
//! no MAINNET_RPC_URL). This crate also writes generated.bin for that test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use alloy_primitives::{address, b256, Address, U256};
use liq_plan::EncodeError;
use liq_plan::{
    col_per_unit_debt_1e18, decode_batch, ensure_surplus_borrow_profit_legs, leg_tie, tie_flags,
    BatchPlan, CompoundMarketPin, EncodedPlan, FlashGroup, LiqLeg, LiquityTrovePin,
    MorphoMarketPin, SwapLeg, V4ReservePin, ValidateCtx, FLAG_SWEEP, HEADER_LEN, LEG_EXACT_OUT,
    LEG_TAKE_BALANCE, LEG_TIE_MAX, LEG_TIE_SHIFT, LIQ_LEG_LEN, SWAP_LEG_HEAD_LEN, VENUE_CURVE_POOL,
    VENUE_ROUTER, VENUE_UNIV2_POOL, VENUE_UNIV3_POOL,
};
use liq_protocol::ExecutorAdapter;
use liq_types::fixed::{RAY, WAD};
use liq_types::FlashProvider;
use liq_wire::wire::LegTail;
use proptest::prelude::*;

/// Canonical WETH9.
const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const DAI: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
const WSTETH: Address = address!("7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0");
/// SparkLend pool (config/protocols/spark.toml).
const SPARK: Address = address!("c13e21b648a5ee794902342038ff3adab66be987");
/// Aave V4 spoke used in ForkMatrix (2 reserves).
const V4_SPOKE: Address = address!("e1900480ac69f0B296841Cd01cC37546d92F35Cd");
const MORPHO: Address = address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");
const AAVE_V3: Address = address!("87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");
/// Uni V3 USDC/WETH 0.05% — CREATE2-verified in ForkMatrix.
const USDC_WETH: Address = address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
const ROUTER_A: Address = address!("E592427A0AEce92De3Edee1F18E0157C05861564");
const SKY_FLASH: Address = address!("60744434d6339a6B27d73d9Eda62b6F66a0a04FA");
const UNIV4_PM: Address = address!("000000000004444c5dc75cB358380D2e3dE08A90");
const TM: Address = address!("3333333333333333333333333333333333333333");
const CDEBT: Address = address!("5555555555555555555555555555555555555555");
const CCOLL: Address = address!("6666666666666666666666666666666666666666");
const USER: Address = address!("000000000000000000000000000000000000dEaD");
const TROVE: U256 = U256::from_limbs([42, 0, 0, 0]);

/// ForkMatrix live Morpho id (wstETH coll, WETH loan). Params from Morpho
/// listing (exchange-rate oracle + AdaptiveCurveIRM, LLTV 94.5%).
fn morpho_wsteth_weth() -> MorphoMarketPin {
    MorphoMarketPin {
        id: b256!("C54D7ACF14DE29E0E5527CABD7A576506870346A78A11A6762E2CCA66322EC41"),
        morpho: MORPHO,
        loan_token: WETH,
        collateral_token: WSTETH,
        oracle: address!("2a01EB9496094dA03c4E364Def50f5aD1280AD72"),
        irm: address!("870aC11D48B15DB9a138Cf899d20F13F79Ba00BC"),
        lltv: U256::from(945_000_000_000_000_000u64),
    }
}

fn ctx() -> ValidateCtx {
    // Keep the hardcoded real on-chain Morpho id (C54D…EC41) from
    // morpho_wsteth_weth(); do NOT overwrite it with the computed id.
    // check_morpho then verifies keccak(abi.encode(params)) == real id,
    // so a wrong field order would fail the test instead of passing tautologically.
    let pin = morpho_wsteth_weth();
    ValidateCtx {
        weth: WETH,
        v4_underlying: vec![
            V4ReservePin {
                spoke: V4_SPOKE,
                reserve_id: 0,
                underlying: WETH,
            },
            V4ReservePin {
                spoke: V4_SPOKE,
                reserve_id: 1,
                underlying: USDC,
            },
        ],
        morpho: vec![pin],
        compound: vec![CompoundMarketPin {
            debt_ctoken: CDEBT,
            ctoken_collateral: CCOLL,
            is_cether: 0,
        }],
        liquity: vec![LiquityTrovePin {
            trove_manager: TM,
            trove_id: TROVE,
            borrower: USER,
        }],
    }
}

fn pool_data(pool: Address) -> Vec<u8> {
    pool.to_vec()
}

fn router_data() -> Vec<u8> {
    let mut d = ROUTER_A.to_vec();
    d.extend_from_slice(&[0x11u8; 4]);
    d
}

fn profit_tb(token_in: Address) -> SwapLeg {
    SwapLeg {
        venue: VENUE_UNIV3_POOL,
        token_in,
        token_out: WETH,
        flags: LEG_TAKE_BALANCE,
        amount: 0,
        data: pool_data(USDC_WETH),
    }
}

fn exact_out(token_in: Address, token_out: Address, amount: u128) -> SwapLeg {
    SwapLeg {
        venue: VENUE_UNIV3_POOL,
        token_in,
        token_out,
        flags: LEG_EXACT_OUT,
        amount,
        data: pool_data(USDC_WETH),
    }
}

fn v3_leg(coll: Address, repay: u128, pull: u128) -> LiqLeg {
    LiqLeg {
        adapter: ExecutorAdapter::AaveV3,
        market: SPARK,
        borrower: address!("000000000000000000000000000000000000dEaD"),
        collateral_asset: coll,
        repay_amount: repay,
        tail: LegTail::None,
        protocol_pull: pull,
    }
}

fn v4_leg(coll: Address, coll_id: u16, debt_id: u16, repay: u128, pull: u128) -> LiqLeg {
    LiqLeg {
        adapter: ExecutorAdapter::AaveV4,
        market: V4_SPOKE,
        borrower: address!("000000000000000000000000000000000000dEaD"),
        collateral_asset: coll,
        repay_amount: repay,
        tail: LegTail::AaveV4 {
            collateral_reserve_id: coll_id,
            debt_reserve_id: debt_id,
        },
        protocol_pull: pull,
    }
}

fn morpho_leg(repay: u128, pull: u128, ctx: &ValidateCtx) -> LiqLeg {
    let id = ctx.morpho[0].id;
    LiqLeg {
        adapter: ExecutorAdapter::MorphoBlue,
        market: MORPHO,
        borrower: address!("000000000000000000000000000000000000dEaD"),
        collateral_asset: WSTETH,
        repay_amount: repay,
        tail: LegTail::Morpho { market_id: id },
        protocol_pull: pull,
    }
}

fn group(
    provider: FlashProvider,
    src: Address,
    debt: Address,
    flash: u128,
    liqs: Vec<LiqLeg>,
    repay: Vec<SwapLeg>,
) -> FlashGroup {
    FlashGroup {
        provider,
        flash_source: src,
        debt_asset: debt,
        flash_amount: flash,
        fee_bps: 0,
        liqs,
        repay_swaps: repay,
    }
}

/// Wire-equal after decode (protocol_pull is off-wire).
fn wire_eq(a: &BatchPlan, b: &BatchPlan) -> bool {
    if a.flags != b.flags
        || a.bid_bps != b.bid_bps
        || a.gas_cost_wei != b.gas_cost_wei
        || a.min_profit_wei != b.min_profit_wei
        || a.groups.len() != b.groups.len()
        || a.profit_swaps != b.profit_swaps
    {
        return false;
    }
    a.groups.iter().zip(b.groups.iter()).all(|(x, y)| {
        x.provider == y.provider
            && x.flash_source == y.flash_source
            && x.debt_asset == y.debt_asset
            && x.flash_amount == y.flash_amount
            && x.repay_swaps == y.repay_swaps
            && x.liqs.len() == y.liqs.len()
            && x.liqs.iter().zip(y.liqs.iter()).all(|(l, r)| {
                l.adapter == r.adapter
                    && l.market == r.market
                    && l.borrower == r.borrower
                    && l.collateral_asset == r.collateral_asset
                    && l.repay_amount == r.repay_amount
                    && l.tail == r.tail
            })
    })
}

fn plan_v3() -> BatchPlan {
    plan_v3_legs(1, 1, 9_000, 12_345)
}

fn plan_v3_legs(n: u8, min_profit_wei: u128, bid_bps: u16, gas_cost_wei: u128) -> BatchPlan {
    let pull = 1_000_000_000u128;
    let n = n.max(1);
    let liqs = (0..n).map(|_| v3_leg(WETH, pull, pull)).collect();
    let total = pull.saturating_mul(u128::from(n));
    BatchPlan {
        flags: FLAG_SWEEP,
        bid_bps,
        gas_cost_wei,
        min_profit_wei,
        groups: vec![group(
            FlashProvider::Aave,
            AAVE_V3,
            DAI,
            total,
            liqs,
            vec![exact_out(WETH, DAI, total)],
        )],
        // Seized WETH is the profit asset. No WETH→WETH closer.
        profit_swaps: vec![],
    }
}

fn plan_v4_clamped() -> BatchPlan {
    // Flash more than V4 will pull; surplus DAI needs TAKE_BALANCE → WETH.
    let asked = 2_000_000u128;
    let pull = 1_000_000u128;
    let mut p = BatchPlan {
        flags: 0,
        bid_bps: 8_000,
        gas_cost_wei: 1,
        min_profit_wei: 1,
        groups: vec![group(
            FlashProvider::UniV4,
            UNIV4_PM,
            USDC,
            asked,
            vec![v4_leg(WETH, 0, 1, asked, pull)],
            vec![exact_out(WETH, USDC, pull)],
        )],
        profit_swaps: vec![],
    };
    ensure_surplus_borrow_profit_legs(&mut p, WETH, USDC_WETH);
    p
}

fn plan_morpho() -> BatchPlan {
    let ctx = ctx();
    let pull = 1_000_000_000_000_000_000u128;
    BatchPlan {
        flags: FLAG_SWEEP,
        bid_bps: 9_800,
        gas_cost_wei: 99,
        min_profit_wei: 7,
        groups: vec![group(
            FlashProvider::Morpho,
            MORPHO,
            WETH,
            pull,
            vec![morpho_leg(pull, pull, &ctx)],
            vec![exact_out(WSTETH, WETH, pull)],
        )],
        profit_swaps: vec![profit_tb(WSTETH)],
    }
}

fn plan_multi() -> BatchPlan {
    let ctx = ctx();
    let p0 = 500u128;
    let p1 = 800u128;
    let mut p = BatchPlan {
        flags: FLAG_SWEEP,
        bid_bps: 1,
        gas_cost_wei: 2,
        min_profit_wei: 3,
        groups: vec![
            group(
                FlashProvider::Aave,
                SPARK,
                DAI,
                p0,
                vec![v3_leg(WETH, p0, p0)],
                vec![exact_out(WETH, DAI, p0)],
            ),
            group(
                FlashProvider::SkyDss,
                SKY_FLASH,
                DAI,
                p0 + 50,
                vec![v3_leg(WSTETH, p0, p0)],
                vec![SwapLeg {
                    venue: VENUE_ROUTER,
                    token_in: WSTETH,
                    token_out: DAI,
                    flags: LEG_EXACT_OUT,
                    amount: p0,
                    data: router_data(),
                }],
            ),
            group(
                FlashProvider::UniV3,
                USDC_WETH,
                USDC,
                p1,
                vec![v4_leg(WETH, 0, 1, p1, p1)],
                vec![exact_out(WETH, USDC, p1)],
            ),
            group(
                FlashProvider::Morpho,
                MORPHO,
                WETH,
                1,
                vec![morpho_leg(1, 1, &ctx)],
                vec![exact_out(WSTETH, WETH, 1)],
            ),
        ],
        // WETH collateral is not closed by a swap. wstETH still is.
        profit_swaps: vec![profit_tb(WSTETH)],
    };
    ensure_surplus_borrow_profit_legs(&mut p, WETH, USDC_WETH);
    p
}

#[test]
fn constants_match_plan_decoder() {
    assert_eq!(HEADER_LEN, 35);
    assert_eq!(LIQ_LEG_LEN, 77);
    assert_eq!(SWAP_LEG_HEAD_LEN, 60);
    assert_eq!(ExecutorAdapter::AaveV3.tail_len(), 0);
    assert_eq!(ExecutorAdapter::AaveV4.tail_len(), 4);
    assert_eq!(ExecutorAdapter::MorphoBlue.tail_len(), 32);
    assert_eq!(ExecutorAdapter::EulerV2.tail_len(), 52);
    assert_eq!(ExecutorAdapter::SiloV2.tail_len(), 0);
    assert_eq!(ExecutorAdapter::LiquityV2.tail_len(), 32);
    assert_eq!(ExecutorAdapter::Fluid.tail_len(), 98);
    assert_eq!(ExecutorAdapter::Gearbox.tail_len(), 33);
    assert_eq!(ExecutorAdapter::CompoundV2.tail_len(), 21);
}

#[test]
fn encode_decode_10e_tails() {
    let c = ctx();
    let vault = address!("1111111111111111111111111111111111111111");
    let hook = address!("2222222222222222222222222222222222222222");
    let facade = address!("4444444444444444444444444444444444444444");
    let asked = 1_000_000u128;
    let legs = [
        LiqLeg {
            adapter: ExecutorAdapter::EulerV2,
            market: vault,
            borrower: USER,
            collateral_asset: WETH,
            repay_amount: asked,
            tail: LegTail::Euler {
                min_yield: U256::from(7u64),
                vault,
            },
            protocol_pull: asked,
        },
        LiqLeg {
            adapter: ExecutorAdapter::SiloV2,
            market: hook,
            borrower: USER,
            collateral_asset: WETH,
            repay_amount: asked,
            tail: LegTail::None,
            protocol_pull: asked,
        },
        LiqLeg {
            adapter: ExecutorAdapter::LiquityV2,
            market: TM,
            borrower: USER,
            collateral_asset: WETH,
            repay_amount: asked,
            tail: LegTail::Liquity { trove_id: TROVE },
            protocol_pull: asked,
        },
        LiqLeg {
            adapter: ExecutorAdapter::Fluid,
            market: vault,
            borrower: USER,
            collateral_asset: WETH,
            repay_amount: asked,
            tail: LegTail::Fluid {
                kind: liq_wire::wire::FLUID_T4,
                flags: liq_wire::wire::FLUID_DEBT_TOKEN1 | liq_wire::wire::FLUID_ABSORB,
                col_per_unit_debt: WAD,
                debt_shares_min_per_token: U256::from(2_254_704u64),
                col_per_share_min: U256::from(2_142_145u64),
            },
            protocol_pull: asked,
        },
        LiqLeg {
            adapter: ExecutorAdapter::Gearbox,
            market: facade,
            borrower: USER,
            collateral_asset: WETH,
            repay_amount: asked,
            tail: LegTail::Gearbox {
                min_seized: U256::from(9u64),
                full: true,
            },
            protocol_pull: asked,
        },
        LiqLeg {
            adapter: ExecutorAdapter::CompoundV2,
            market: CDEBT,
            borrower: USER,
            collateral_asset: WETH,
            repay_amount: asked,
            tail: LegTail::CompoundV2 {
                ctoken_collateral: CCOLL,
                is_cether: 0,
            },
            protocol_pull: asked,
        },
    ];
    for leg in legs {
        let p = BatchPlan {
            flags: FLAG_SWEEP,
            bid_bps: 0,
            gas_cost_wei: 0,
            min_profit_wei: 1,
            groups: vec![FlashGroup {
                provider: FlashProvider::Aave,
                flash_source: AAVE_V3,
                debt_asset: DAI,
                flash_amount: asked,
                fee_bps: 0,
                liqs: vec![leg.clone()],
                repay_swaps: vec![exact_out(WETH, DAI, asked)],
            }],
            profit_swaps: vec![],
        };
        let bytes = EncodedPlan::encode(&p, &c).unwrap().into_bytes();
        let back = decode_batch(&bytes).unwrap();
        assert_eq!(back.groups[0].liqs[0].adapter, leg.adapter);
        assert_eq!(back.groups[0].liqs[0].tail, leg.tail);
        assert_eq!(back.groups[0].liqs[0].market, leg.market);
    }
}

fn one_leg_plan(leg: LiqLeg) -> BatchPlan {
    let asked = leg.protocol_pull;
    BatchPlan {
        flags: FLAG_SWEEP,
        bid_bps: 0,
        gas_cost_wei: 0,
        min_profit_wei: 1,
        groups: vec![FlashGroup {
            provider: FlashProvider::Aave,
            flash_source: AAVE_V3,
            debt_asset: DAI,
            flash_amount: asked,
            fee_bps: 0,
            liqs: vec![leg],
            repay_swaps: vec![exact_out(WETH, DAI, asked)],
        }],
        profit_swaps: vec![],
    }
}

/// The Fluid tail is checked against the vault type: per-share figures on
/// the smart sides and only there, token1 only on a smart side, no unknown
/// flag bit or type. `colPerUnitDebt` is a raw-unit ratio, so a figure above
/// 1e27 (col shares per 6-decimal debt token) is valid.
#[test]
fn fluid_tail_is_checked_against_the_vault_type() {
    use liq_plan::EncodeError;
    use liq_wire::wire::{FLUID_COL_TOKEN1, FLUID_DEBT_TOKEN1, FLUID_T1, FLUID_T2, FLUID_T3};
    let c = ctx();
    let asked = 1_000_000u128;
    let vault = address!("1111111111111111111111111111111111111111");
    let leg = |kind: u8, flags: u8, colper: U256, dps: U256, cps: U256| LiqLeg {
        adapter: ExecutorAdapter::Fluid,
        market: vault,
        borrower: USER,
        collateral_asset: WETH,
        repay_amount: asked,
        tail: LegTail::Fluid {
            kind,
            flags,
            col_per_unit_debt: colper,
            debt_shares_min_per_token: dps,
            col_per_share_min: cps,
        },
        protocol_pull: asked,
    };
    let enc = |l: LiqLeg| EncodedPlan::encode(&one_leg_plan(l), &c).err();
    let one = U256::from(1u8);
    let z = U256::ZERO;
    assert_eq!(enc(leg(FLUID_T1, 0, one, z, z)), None);
    assert_eq!(
        enc(leg(
            FLUID_T2,
            FLUID_COL_TOKEN1,
            RAY * U256::from(1000u32),
            z,
            one
        )),
        None
    );
    assert_eq!(enc(leg(FLUID_T3, FLUID_DEBT_TOKEN1, one, one, z)), None);
    assert_eq!(
        enc(leg(0, 0, one, z, z)),
        Some(EncodeError::FluidBadKind(0))
    );
    assert_eq!(
        enc(leg(5, 0, one, z, z)),
        Some(EncodeError::FluidBadKind(5))
    );
    assert_eq!(
        enc(leg(FLUID_T1, 0, z, z, z)),
        Some(EncodeError::FluidZeroColPer)
    );
    assert_eq!(
        enc(leg(FLUID_T1, 0x20, one, z, z)),
        Some(EncodeError::FluidBadFlags(0x20))
    );
    assert_eq!(
        enc(leg(FLUID_T1, FLUID_DEBT_TOKEN1, one, z, z)),
        Some(EncodeError::FluidBadFlags(FLUID_DEBT_TOKEN1))
    );
    assert_eq!(
        enc(leg(FLUID_T3, 0, one, z, z)),
        Some(EncodeError::FluidPerShare(FLUID_T3))
    );
    assert_eq!(
        enc(leg(FLUID_T1, 0, one, one, z)),
        Some(EncodeError::FluidPerShare(FLUID_T1))
    );
    assert_eq!(
        enc(leg(FLUID_T2, 0, one, z, z)),
        Some(EncodeError::FluidPerShare(FLUID_T2))
    );
    assert_eq!(col_per_unit_debt_1e18(WAD, WAD).unwrap(), WAD);
}

#[test]
fn compound_wrong_ctoken_and_flipped_cether_rejected() {
    let c = ctx();
    let asked = 1_000_000u128;
    let right = LiqLeg {
        adapter: ExecutorAdapter::CompoundV2,
        market: CDEBT,
        borrower: USER,
        collateral_asset: WETH,
        repay_amount: asked,
        tail: LegTail::CompoundV2 {
            ctoken_collateral: CCOLL,
            is_cether: 0,
        },
        protocol_pull: asked,
    };
    EncodedPlan::encode(&one_leg_plan(right.clone()), &c).expect("pinned pair");

    let mut wrong_coll = right.clone();
    wrong_coll.tail = LegTail::CompoundV2 {
        ctoken_collateral: address!("7777777777777777777777777777777777777777"),
        is_cether: 0,
    };
    assert!(matches!(
        EncodedPlan::encode(&one_leg_plan(wrong_coll), &c),
        Err(liq_plan::EncodeError::CompoundUnpinned { .. })
    ));

    let mut flipped = right.clone();
    flipped.tail = LegTail::CompoundV2 {
        ctoken_collateral: CCOLL,
        is_cether: 1,
    };
    assert!(matches!(
        EncodedPlan::encode(&one_leg_plan(flipped), &c),
        Err(liq_plan::EncodeError::CompoundCEtherMismatch { pinned: 0, got: 1 })
    ));

    let mut wrong_debt = right;
    wrong_debt.market = address!("8888888888888888888888888888888888888888");
    assert!(matches!(
        EncodedPlan::encode(&one_leg_plan(wrong_debt), &c),
        Err(liq_plan::EncodeError::CompoundUnpinned { .. })
    ));
}

#[test]
fn liquity_wrong_trove_id_rejected() {
    let c = ctx();
    let asked = 1_000_000u128;
    let right = LiqLeg {
        adapter: ExecutorAdapter::LiquityV2,
        market: TM,
        borrower: USER,
        collateral_asset: WETH,
        repay_amount: asked,
        tail: LegTail::Liquity { trove_id: TROVE },
        protocol_pull: asked,
    };
    EncodedPlan::encode(&one_leg_plan(right.clone()), &c).expect("pinned trove");

    let mut wrong_id = right.clone();
    wrong_id.tail = LegTail::Liquity {
        trove_id: U256::from(99u64),
    };
    assert!(matches!(
        EncodedPlan::encode(&one_leg_plan(wrong_id), &c),
        Err(liq_plan::EncodeError::LiquityUnpinned { .. })
    ));

    let mut wrong_tm = right;
    wrong_tm.market = address!("9999999999999999999999999999999999999999");
    assert!(matches!(
        EncodedPlan::encode(&one_leg_plan(wrong_tm), &c),
        Err(liq_plan::EncodeError::LiquityMarketMismatch { .. })
    ));
}

fn take_balance(token_in: Address, token_out: Address) -> SwapLeg {
    SwapLeg {
        venue: VENUE_UNIV3_POOL,
        token_in,
        token_out,
        flags: LEG_TAKE_BALANCE,
        amount: 0,
        data: pool_data(USDC_WETH),
    }
}

/// One Morpho-flashed (no fee) DAI group liquidating a wstETH position.
fn hub_plan(repay: Vec<SwapLeg>) -> BatchPlan {
    let pull = 1_000_000_000u128;
    BatchPlan {
        flags: FLAG_SWEEP,
        bid_bps: 9_000,
        gas_cost_wei: 12_345,
        min_profit_wei: 1,
        groups: vec![group(
            FlashProvider::Morpho,
            MORPHO,
            DAI,
            pull,
            vec![v3_leg(WSTETH, pull, pull)],
            repay,
        )],
        profit_swaps: vec![],
    }
}

/// An exit through the hub: the repay blob sells the collateral into WETH
/// (TAKE_BALANCE, which closes it) and then buys the debt with WETH
/// (EXACT_OUT). A TAKE_BALANCE ahead of a set amount of another token is
/// that order. A set amount of the same token after its TAKE_BALANCE would
/// find none of it left, and a repay leg into neither the debt nor WETH
/// strands what it buys.
#[test]
fn hub_exit_repay_blob_validates_and_its_mistakes_do_not() {
    let c = ctx();
    let pull = 1_000_000_000u128;
    let p = hub_plan(vec![take_balance(WSTETH, WETH), exact_out(WETH, DAI, pull)]);
    let bytes = EncodedPlan::encode(&p, &c).expect("hub exit").into_bytes();
    assert!(wire_eq(&p, &decode_batch(&bytes).unwrap()));

    let same_token = hub_plan(vec![
        take_balance(WSTETH, WETH),
        exact_out(WSTETH, DAI, pull),
    ]);
    assert!(matches!(
        EncodedPlan::encode(&same_token, &c),
        Err(EncodeError::SpentAfterTakeBalance { token }) if token == WSTETH
    ));
    let mut exact_in = exact_out(WSTETH, DAI, pull);
    exact_in.flags = 0;
    let after = hub_plan(vec![
        take_balance(WSTETH, WETH),
        exact_in,
        exact_out(WETH, DAI, pull),
    ]);
    assert!(matches!(
        EncodedPlan::encode(&after, &c),
        Err(EncodeError::SpentAfterTakeBalance { token }) if token == WSTETH
    ));
    let elsewhere = hub_plan(vec![take_balance(WSTETH, USDC), exact_out(WETH, DAI, pull)]);
    assert!(matches!(
        EncodedPlan::encode(&elsewhere, &c),
        Err(EncodeError::RepayTargetMismatch { .. })
    ));
}

/// Uniswap V3 wstETH/WETH 0.01 %.
const WSTETH_WETH_001: Address = address!("109830a1AAaD605BbF02a9dFA7B0B92EC2FB7dAa");

fn take_balance_on(pool: Address, token_in: Address, token_out: Address) -> SwapLeg {
    SwapLeg {
        venue: VENUE_UNIV3_POOL,
        token_in,
        token_out,
        flags: LEG_TAKE_BALANCE,
        amount: 0,
        data: pool_data(pool),
    }
}

/// One flash-swap group: the USDC/WETH pool lends `pull` of USDC against a
/// wstETH position, with `repay` as the repay blob and `profit` after.
fn flash_swap_plan(
    pull: u128,
    liqs: Vec<LiqLeg>,
    repay: Vec<SwapLeg>,
    profit: Vec<SwapLeg>,
) -> BatchPlan {
    BatchPlan {
        flags: FLAG_SWEEP,
        bid_bps: 9_000,
        gas_cost_wei: 12_345,
        min_profit_wei: 1,
        groups: vec![group(
            FlashProvider::UniV3Swap,
            USDC_WETH,
            USDC,
            pull,
            liqs,
            repay,
        )],
        profit_swaps: profit,
    }
}

/// A flash-swap group buys its debt from the lender pool itself, so it
/// carries no repay leg into the debt. Direct (the pool holds the
/// collateral): no repay legs, the leftover collateral closes in the profit
/// blob. Through WETH: the repay blob sells the collateral into WETH on
/// another pool (TAKE_BALANCE, which closes it) and the pool is paid WETH.
/// Both round-trip the wire. Refused: two legs (a beaten one would leave
/// debt bought and unpaid for), a repay leg buying the debt, a leg on the
/// lender pool (locked while it swaps), and a fee (the swap's is in the
/// quote).
#[test]
fn flash_swap_groups_validate_and_their_mistakes_do_not() {
    let c = ctx();
    let pull = 1_000_000_000u128;
    let direct = flash_swap_plan(pull, vec![v3_leg(WETH, pull, pull)], vec![], vec![]);
    let bytes = EncodedPlan::encode(&direct, &c)
        .expect("direct flash swap")
        .into_bytes();
    assert!(wire_eq(&direct, &decode_batch(&bytes).unwrap()));
    let via_weth = flash_swap_plan(
        pull,
        vec![v3_leg(WSTETH, pull, pull)],
        vec![take_balance_on(WSTETH_WETH_001, WSTETH, WETH)],
        vec![],
    );
    let bytes = EncodedPlan::encode(&via_weth, &c)
        .expect("flash swap through WETH")
        .into_bytes();
    assert!(wire_eq(&via_weth, &decode_batch(&bytes).unwrap()));

    let two = flash_swap_plan(
        2 * pull,
        vec![v3_leg(WETH, pull, pull), v3_leg(WETH, pull, pull)],
        vec![],
        vec![],
    );
    assert!(matches!(
        EncodedPlan::encode(&two, &c),
        Err(EncodeError::FlashSwapLegs { legs: 2 })
    ));
    let buys = flash_swap_plan(
        pull,
        vec![v3_leg(WSTETH, pull, pull)],
        vec![exact_out(WSTETH, USDC, pull)],
        vec![profit_tb(WSTETH)],
    );
    assert!(matches!(
        EncodedPlan::encode(&buys, &c),
        Err(EncodeError::FlashSwapRepaysDebt { debt }) if debt == USDC
    ));
    let on_lender = flash_swap_plan(
        pull,
        vec![v3_leg(WSTETH, pull, pull)],
        vec![take_balance(WSTETH, WETH)],
        vec![],
    );
    assert!(matches!(
        EncodedPlan::encode(&on_lender, &c),
        Err(EncodeError::FlashSwapLegOnLender { pool }) if pool == USDC_WETH
    ));
    let mut fee = flash_swap_plan(pull, vec![v3_leg(WETH, pull, pull)], vec![], vec![]);
    fee.groups[0].fee_bps = 5;
    assert!(matches!(
        EncodedPlan::encode(&fee, &c),
        Err(EncodeError::UnpriceableFee {
            provider: FlashProvider::UniV3Swap,
            fee_bps: 5
        })
    ));
}

/// Two groups seize the same collateral. The first closes its own in its
/// repay blob (an exit through the hub); the second runs after it, leaves
/// some, and the profit blob closes that. Each is closed exactly once after
/// its own liquidation. Two TAKE_BALANCE legs on it in one blob are not.
#[test]
fn a_collateral_closes_in_its_own_repay_blob_or_else_in_the_profit_blob() {
    let c = ctx();
    let pull = 1_000_000_000u128;
    let mut p = hub_plan(vec![take_balance(WSTETH, WETH), exact_out(WETH, DAI, pull)]);
    p.groups.push(group(
        FlashProvider::UniV4,
        UNIV4_PM,
        USDC,
        pull,
        vec![v3_leg(WSTETH, pull, pull)],
        vec![exact_out(WSTETH, USDC, pull)],
    ));
    p.profit_swaps = vec![profit_tb(WSTETH)];
    EncodedPlan::encode(&p, &c).expect("closed once each");

    let mut twice = hub_plan(vec![
        take_balance(WSTETH, WETH),
        take_balance(WSTETH, WETH),
        exact_out(WETH, DAI, pull),
    ]);
    twice.profit_swaps = vec![];
    match EncodedPlan::encode(&twice, &c) {
        Err(EncodeError::BadCollateralClosure { closers, need, .. }) => {
            assert_eq!((closers, need), (2, 1));
        }
        other => panic!("expected BadCollateralClosure, got {other:?}"),
    }
}

#[test]
fn weth_collateral_self_closer_is_rejected() {
    let c = ctx();
    let mut p = plan_v3();
    p.profit_swaps = vec![profit_tb(WETH)];
    match EncodedPlan::encode(&p, &c) {
        Err(EncodeError::BadCollateralClosure { need, closers, .. }) => {
            assert_eq!(need, 0);
            assert_eq!(closers, 1);
        }
        other => panic!("expected BadCollateralClosure, got {other:?}"),
    }
}

#[test]
fn encode_decode_fixtures() {
    let c = ctx();
    for p in [plan_v3(), plan_v4_clamped(), plan_morpho(), plan_multi()] {
        EncodedPlan::encode(&p, &c).expect("validate");
        let bytes = EncodedPlan::encode(&p, &c).unwrap().into_bytes();
        let back = decode_batch(&bytes).unwrap();
        assert!(wire_eq(&p, &back), "round-trip wire fields");
    }
}

#[test]
fn surplus_borrow_emits_take_balance_on_debt() {
    let c = ctx();
    let p = plan_v4_clamped();
    assert!(p
        .profit_swaps
        .iter()
        .any(|s| { s.token_in == USDC && s.token_out == WETH && s.flags & LEG_TAKE_BALANCE != 0 }));
    EncodedPlan::encode(&p, &c).unwrap();
}

#[test]
fn surplus_borrow_without_leg_is_rejected() {
    let c = ctx();
    let asked = 2_000_000u128;
    let pull = 1_000_000u128;
    let p = BatchPlan {
        flags: 0,
        bid_bps: 1,
        gas_cost_wei: 1,
        min_profit_wei: 1,
        groups: vec![group(
            FlashProvider::UniV4,
            UNIV4_PM,
            USDC,
            asked,
            vec![v4_leg(WETH, 0, 1, asked, pull)],
            vec![exact_out(WETH, USDC, pull)],
        )],
        profit_swaps: vec![profit_tb(WETH)],
    };
    match EncodedPlan::encode(&p, &c) {
        Err(liq_plan::EncodeError::SurplusDebtUnrouted {
            debt,
            flash,
            pull: pl,
        }) => {
            assert_eq!(debt, USDC);
            assert_eq!(flash, asked);
            assert_eq!(pl, pull);
        }
        other => panic!("expected SurplusDebtUnrouted, got {other:?}"),
    }
}

#[test]
fn under_seizure_exact_out_is_rejected() {
    let c = ctx();
    let pull = 1_000u128;
    let p = BatchPlan {
        flags: 0,
        bid_bps: 1,
        gas_cost_wei: 1,
        min_profit_wei: 1,
        groups: vec![group(
            FlashProvider::Aave,
            SPARK,
            DAI,
            pull,
            vec![v3_leg(WETH, pull, pull)],
            vec![exact_out(WETH, DAI, pull + 1)],
        )],
        profit_swaps: vec![profit_tb(WETH)],
    };
    match EncodedPlan::encode(&p, &c) {
        Err(liq_plan::EncodeError::UnderSeizure {
            leg,
            exact_out,
            pull: p,
        }) => {
            assert_eq!((leg, exact_out, p), (0, pull + 1, pull));
        }
        other => panic!("expected UnderSeizure, got {other:?}"),
    }
}

/// Aave charges the premium on the borrowed amount, and the Executor buys
/// it at run time: the first exact-output pool leg of the group to run buys
/// the fee the provider's callback reported on top of its own amount
/// (`SwapModule.runSwaps`, `T_FEE`). So the exact output is the pull. A
/// plan that also bought the premium would buy it twice; one short of the
/// pull buys too little. Oracle: the Executor's semantics, exercised on the
/// EVM by `ExecutorBeatenLeg.t.sol` and every Aave-flash unit test.
#[test]
fn aave_exact_out_buys_the_pull_and_the_executor_adds_the_premium() {
    let c = ctx();
    let pull = 2_000_000u128;
    let bps = 5u16;
    let fee = liq_flash::fee_amount(FlashProvider::Aave, U256::from(pull), bps).unwrap();
    let fee_u = u128::try_from(fee).unwrap();
    assert_eq!(fee_u, 1_000);
    let buying = |amount: u128| {
        let mut g = group(
            FlashProvider::Aave,
            AAVE_V3,
            DAI,
            pull,
            vec![v3_leg(WETH, pull, pull)],
            vec![exact_out(WETH, DAI, amount)],
        );
        g.fee_bps = bps;
        BatchPlan {
            flags: 0,
            bid_bps: 1,
            gas_cost_wei: 1,
            min_profit_wei: 1,
            groups: vec![g],
            profit_swaps: vec![],
        }
    };
    EncodedPlan::encode(&buying(pull), &c).unwrap();
    match EncodedPlan::encode(&buying(pull + fee_u), &c) {
        Err(EncodeError::UnderSeizure {
            leg,
            exact_out,
            pull: p,
        }) => assert_eq!((leg, exact_out, p), (0, pull + fee_u, pull)),
        other => panic!("expected UnderSeizure, got {other:?}"),
    }
    match EncodedPlan::encode(&buying(pull - 1), &c) {
        Err(EncodeError::RepayNotSizedToPull {
            leg,
            exact_out,
            pull: p,
        }) => assert_eq!((leg, exact_out, p), (0, pull - 1, pull)),
        other => panic!("expected RepayNotSizedToPull, got {other:?}"),
    }
}

/// The Executor adds the premium to a pool exact output only (V3, V2): a
/// router's output is fixed in its own calldata, and Curve has none. A leg
/// repaid through a router alone under a fee-charging flash cannot buy the
/// premium; one repaid on Curve sells an overshoot that does; with no fee
/// there is nothing to carry.
#[test]
fn a_repay_that_cannot_carry_the_premium_is_refused() {
    let c = ctx();
    let pull = 2_000_000u128;
    let routed = |bps: u16| {
        let mut g = group(
            FlashProvider::Aave,
            AAVE_V3,
            DAI,
            pull,
            vec![v3_leg(WETH, pull, pull)],
            vec![SwapLeg {
                venue: VENUE_ROUTER,
                token_in: WETH,
                token_out: DAI,
                flags: LEG_EXACT_OUT,
                amount: pull,
                data: router_data(),
            }],
        );
        g.fee_bps = bps;
        BatchPlan {
            flags: 0,
            bid_bps: 1,
            gas_cost_wei: 1,
            min_profit_wei: 1,
            groups: vec![g],
            profit_swaps: vec![],
        }
    };
    assert!(matches!(
        EncodedPlan::encode(&routed(5), &c),
        Err(EncodeError::PremiumUncovered { leg: 0 })
    ));
    EncodedPlan::encode(&routed(0), &c).unwrap();
    let mut curve = routed(5);
    curve.groups[0].repay_swaps[0] = SwapLeg {
        venue: VENUE_CURVE_POOL,
        token_in: WETH,
        token_out: DAI,
        flags: 0,
        amount: pull,
        data: curve_data(1, 0),
    };
    curve.profit_swaps.push(profit_tb(DAI));
    EncodedPlan::encode(&curve, &c).unwrap();
}

/// The tie is the leg's index plus one in flags bits 2–7, as `SwapModule`
/// reads it (`flags >> L_TIE_SHIFT`). Oracle: the constant in the
/// contract's source.
#[test]
fn tie_bits_are_the_leg_index_plus_one_above_the_two_flag_bits() {
    let sol = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/src/SwapModule.sol"
    ))
    .unwrap();
    let line = sol
        .lines()
        .find(|l| l.contains("constant L_TIE_SHIFT"))
        .expect("SwapModule declares L_TIE_SHIFT");
    let shift: u8 = line
        .split('=')
        .nth(1)
        .unwrap()
        .trim()
        .trim_end_matches(';')
        .parse()
        .unwrap();
    assert_eq!(LEG_TIE_SHIFT, shift);
    assert_eq!(tie_flags(LEG_EXACT_OUT, 0), Some(0b0000_0110));
    assert_eq!(tie_flags(0, 1), Some(0b0000_1000));
    assert_eq!(tie_flags(LEG_TAKE_BALANCE | LEG_EXACT_OUT, 62), Some(0xFF));
    assert_eq!(tie_flags(LEG_EXACT_OUT, 63), None, "six bits name 63 legs");
    assert_eq!(
        tie_flags(tie_flags(LEG_EXACT_OUT, 5).unwrap(), 2),
        Some(0b0000_1110),
        "a new tie replaces the old"
    );
    assert_eq!(leg_tie(LEG_EXACT_OUT | LEG_TAKE_BALANCE), None);
    assert_eq!(leg_tie(0xFF), Some(62));
    for leg in 0..LEG_TIE_MAX {
        assert_eq!(leg_tie(tie_flags(LEG_EXACT_OUT, leg).unwrap()), Some(leg));
    }
}

/// An exact-output repay of `amount` from `coll`, tied to leg `leg`.
fn tied(coll: Address, amount: u128, leg: usize) -> SwapLeg {
    let mut s = exact_out(coll, DAI, amount);
    s.flags = tie_flags(LEG_EXACT_OUT, leg).unwrap();
    s
}

/// Two positions in one Aave group: leg 0 on WETH pulling 500, leg 1 on
/// wstETH pulling 800.
fn two_leg_plan(repay: Vec<SwapLeg>, profit: Vec<SwapLeg>) -> BatchPlan {
    let mut g = group(
        FlashProvider::Aave,
        AAVE_V3,
        DAI,
        1_300,
        vec![v3_leg(WETH, 500, 500), v3_leg(WSTETH, 800, 800)],
        repay,
    );
    g.fee_bps = 5;
    BatchPlan {
        flags: 0,
        bid_bps: 1,
        gas_cost_wei: 1,
        min_profit_wei: 1,
        groups: vec![g],
        profit_swaps: profit,
    }
}

/// Each leg of a shared group is repaid by swaps tied to it, buying its own
/// pull: the Executor skips the swaps of a leg that did not fill, and the
/// other's still repay the flash. Each mistake an assembler could make is
/// refused.
#[test]
fn ties_name_a_leg_of_their_group_and_their_mistakes_are_refused() {
    let c = ctx();
    let closer = || vec![profit_tb(WSTETH)];
    let ok = two_leg_plan(vec![tied(WETH, 500, 0), tied(WSTETH, 800, 1)], closer());
    let back = decode_batch(&EncodedPlan::encode(&ok, &c).unwrap().into_bytes()).unwrap();
    assert!(wire_eq(&ok, &back));
    let ties: Vec<_> = back.groups[0]
        .repay_swaps
        .iter()
        .map(|s| leg_tie(s.flags))
        .collect();
    assert_eq!(ties, vec![Some(0), Some(1)]);
    // A leg's exit split across pools: each part tied to it.
    let split = two_leg_plan(
        vec![tied(WETH, 200, 0), tied(WETH, 300, 0), tied(WSTETH, 800, 1)],
        closer(),
    );
    EncodedPlan::encode(&split, &c).unwrap();

    let untied = two_leg_plan(
        vec![tied(WETH, 500, 0), exact_out(WSTETH, DAI, 800)],
        closer(),
    );
    assert!(matches!(
        EncodedPlan::encode(&untied, &c),
        Err(EncodeError::UntiedRepay { token, legs: 2 }) if token == WSTETH
    ));
    let past = two_leg_plan(vec![tied(WETH, 500, 0), tied(WSTETH, 800, 2)], closer());
    assert!(matches!(
        EncodedPlan::encode(&past, &c),
        Err(EncodeError::TieOutOfRange { tie: 2, legs: 2 })
    ));
    // Each leg's swaps sized to the other's pull.
    let swapped = two_leg_plan(vec![tied(WETH, 800, 0), tied(WSTETH, 500, 1)], closer());
    assert!(matches!(
        EncodedPlan::encode(&swapped, &c),
        Err(EncodeError::UnderSeizure {
            leg: 0,
            exact_out: 800,
            pull: 500
        })
    ));
    // Both on leg 0: should leg 1 alone fill, nothing would buy its pull.
    let lumped = two_leg_plan(vec![tied(WETH, 500, 0), tied(WSTETH, 800, 0)], closer());
    assert!(matches!(
        EncodedPlan::encode(&lumped, &c),
        Err(EncodeError::UnderSeizure {
            leg: 0,
            exact_out: 1_300,
            pull: 500
        })
    ));
    let mut tied_take = take_balance(WSTETH, WETH);
    tied_take.flags = tie_flags(LEG_TAKE_BALANCE, 1).unwrap();
    let take = two_leg_plan(
        vec![tied(WETH, 500, 0), tied(WSTETH, 800, 1), tied_take],
        vec![],
    );
    assert!(matches!(
        EncodedPlan::encode(&take, &c),
        Err(EncodeError::TiedTakeBalance { token }) if token == WSTETH
    ));
    let mut tied_closer = profit_tb(WSTETH);
    tied_closer.flags = tie_flags(LEG_TAKE_BALANCE, 1).unwrap();
    let profit = two_leg_plan(
        vec![tied(WETH, 500, 0), tied(WSTETH, 800, 1)],
        vec![tied_closer],
    );
    assert!(matches!(
        EncodedPlan::encode(&profit, &c),
        Err(EncodeError::TiedProfitLeg { token }) if token == WSTETH
    ));
    // An exact output into WETH: the Executor would add the premium, in
    // DAI, to a WETH amount.
    let mut into_weth = tied(WSTETH, 800, 1);
    into_weth.token_out = WETH;
    let wrong = two_leg_plan(vec![tied(WETH, 500, 0), into_weth], closer());
    assert!(matches!(
        EncodedPlan::encode(&wrong, &c),
        Err(EncodeError::ExactOutNotDebt { token, debt }) if token == WETH && debt == DAI
    ));
    // Leg 1 repaid through a router alone: with leg 0 beaten, nothing
    // would buy the premium.
    let routed = two_leg_plan(
        vec![
            tied(WETH, 500, 0),
            SwapLeg {
                venue: VENUE_ROUTER,
                token_in: WSTETH,
                token_out: DAI,
                flags: tie_flags(LEG_EXACT_OUT, 1).unwrap(),
                amount: 800,
                data: router_data(),
            },
        ],
        closer(),
    );
    assert!(matches!(
        EncodedPlan::encode(&routed, &c),
        Err(EncodeError::PremiumUncovered { leg: 1 })
    ));
    // Leg 1 on Curve: its overshoot carries the premium, and the surplus
    // debt is swept.
    let curve = two_leg_plan(
        vec![
            tied(WETH, 500, 0),
            SwapLeg {
                venue: VENUE_CURVE_POOL,
                token_in: WSTETH,
                token_out: DAI,
                flags: tie_flags(0, 1).unwrap(),
                amount: 900,
                data: curve_data(1, 0),
            },
        ],
        vec![profit_tb(WSTETH), profit_tb(DAI)],
    );
    EncodedPlan::encode(&curve, &c).unwrap();
}

/// Morpho and UniV4 have no fee. A nonzero bps must not be treated as zero.
#[test]
fn fee_free_provider_rejects_nonzero_bps() {
    let c = ctx();
    let pull = 10u128;
    let mut g = group(
        FlashProvider::Morpho,
        MORPHO,
        WETH,
        pull,
        vec![morpho_leg(pull, pull, &c)],
        vec![exact_out(WSTETH, WETH, pull)],
    );
    g.fee_bps = 5;
    let p = BatchPlan {
        flags: 0,
        bid_bps: 1,
        gas_cost_wei: 1,
        min_profit_wei: 1,
        groups: vec![g],
        profit_swaps: vec![profit_tb(WSTETH)],
    };
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(liq_plan::EncodeError::UnpriceableFee {
            provider: FlashProvider::Morpho,
            fee_bps: 5
        })
    ));
}

#[test]
fn v4_id_must_pin_to_addresses() {
    let c = ctx();
    let pull = 10u128;
    let p = BatchPlan {
        flags: 0,
        bid_bps: 1,
        gas_cost_wei: 1,
        min_profit_wei: 1,
        groups: vec![group(
            FlashProvider::UniV4,
            UNIV4_PM,
            USDC,
            pull,
            vec![v4_leg(WETH, 9, 1, pull, pull)],
            vec![exact_out(WETH, USDC, pull)],
        )],
        profit_swaps: vec![profit_tb(WETH)],
    };
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(liq_plan::EncodeError::V4ReserveUnpinned { id: 9, .. })
    ));
}

#[test]
fn morpho_wrong_collateral_is_rejected() {
    let c = ctx();
    let pull = 1u128;
    let mut leg = morpho_leg(pull, pull, &c);
    leg.collateral_asset = USDC;
    let p = BatchPlan {
        flags: 0,
        bid_bps: 1,
        gas_cost_wei: 1,
        min_profit_wei: 1,
        groups: vec![group(
            FlashProvider::Morpho,
            MORPHO,
            WETH,
            pull,
            vec![leg],
            vec![exact_out(USDC, WETH, pull)],
        )],
        profit_swaps: vec![profit_tb(USDC)],
    };
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(liq_plan::EncodeError::MorphoTokenMismatch { .. })
    ));
}

#[test]
fn morpho_share_rounding_pull_le_asked() {
    let asked = U256::from(1_000_000_000_000_000_000u128);
    let total_a = U256::from(10_000_000_000_000_000_000u128);
    let total_s = U256::from(9_000_000_000_000_000_000u128);
    let pull = liq_plan::morpho_actual_pull(asked, total_a, total_s).unwrap();
    assert!(pull <= asked);
    assert!(pull > U256::ZERO);
}

fn arb_plan() -> impl Strategy<Value = BatchPlan> {
    (
        1u8..=3,
        0u8..=2,
        0u8..=2,
        0u8..=1,
        any::<u16>(),
        any::<u128>(),
        any::<u128>(),
        any::<bool>(),
    )
        .prop_map(
            |(n_g, extra_liq, n_repay, n_profit_extra, bid, gas, minp, sweep)| {
                let c = ctx();
                let n_g = n_g.max(1);
                let mut groups = Vec::new();
                let mut collaterals: Vec<Address> = Vec::new();
                let kinds = [
                    ExecutorAdapter::AaveV3,
                    ExecutorAdapter::AaveV4,
                    ExecutorAdapter::MorphoBlue,
                ];
                for gi in 0..n_g {
                    let kind = kinds[usize::from(gi) % 3];
                    let n_liq = if extra_liq > 0 && kind == ExecutorAdapter::AaveV3 {
                        2
                    } else {
                        1
                    };
                    let mut liqs = Vec::new();
                    let mut pull_sum = 0u128;
                    let (debt, src, provider, coll0) = match kind {
                        ExecutorAdapter::AaveV3 => (DAI, SPARK, FlashProvider::Aave, WETH),
                        ExecutorAdapter::AaveV4 => (USDC, UNIV4_PM, FlashProvider::UniV4, WETH),
                        ExecutorAdapter::MorphoBlue => {
                            (WETH, MORPHO, FlashProvider::Morpho, WSTETH)
                        }
                        _ => unreachable!("proptest kinds are V3/V4/Morpho only"),
                    };
                    for li in 0..n_liq {
                        let pull = 1u128.saturating_add(u128::from(li as u8));
                        pull_sum = pull_sum.saturating_add(pull);
                        let coll = if li == 0 { coll0 } else { WSTETH };
                        if !collaterals.contains(&coll) {
                            collaterals.push(coll);
                        }
                        let asked = pull; // no clamp in arb (surplus covered separately)
                        liqs.push(match kind {
                            ExecutorAdapter::AaveV3 => v3_leg(coll, asked, pull),
                            ExecutorAdapter::AaveV4 => v4_leg(WETH, 0, 1, asked, pull),
                            ExecutorAdapter::MorphoBlue => morpho_leg(asked, pull, &c),
                            _ => unreachable!("proptest kinds are V3/V4/Morpho only"),
                        });
                    }
                    // Each leg's pull, split over `n_r` exact outputs tied
                    // to it.
                    let mut repay = Vec::new();
                    let n_r = if n_repay == 0 { 1 } else { n_repay };
                    for (k, l) in liqs.iter().enumerate() {
                        let part = l.protocol_pull / u128::from(n_r);
                        let mut left = l.protocol_pull;
                        for ri in 0..n_r {
                            let amt = if ri + 1 == n_r { left } else { part.min(left) };
                            left = left.saturating_sub(amt);
                            let mut s = exact_out(l.collateral_asset, debt, amt);
                            s.flags = tie_flags(LEG_EXACT_OUT, k).unwrap();
                            repay.push(s);
                        }
                    }
                    groups.push(group(provider, src, debt, pull_sum, liqs, repay));
                }
                let mut profit: Vec<SwapLeg> = collaterals
                    .into_iter()
                    .filter(|coll| *coll != WETH)
                    .map(profit_tb)
                    .collect();
                for _ in 0..n_profit_extra {
                    if !profit.iter().any(|s| s.token_in == DAI) {
                        profit.push(profit_tb(DAI));
                    }
                }
                BatchPlan {
                    flags: if sweep { FLAG_SWEEP } else { 0 },
                    bid_bps: bid,
                    gas_cost_wei: gas,
                    min_profit_wei: if minp == 0 { 1 } else { minp },
                    groups,
                    profit_swaps: profit,
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn rust_encode_decode_roundtrip(plan in arb_plan()) {
        let c = ctx();
        let enc = EncodedPlan::encode(&plan, &c).expect("generator emits valid plans");
        let back = decode_batch(enc.as_bytes()).expect("rust decode");
        prop_assert!(wire_eq(&plan, &back));
        let re = EncodedPlan::encode(&{
            let mut p = back;
            // restore pulls from original (not on wire)
            for (g, og) in p.groups.iter_mut().zip(plan.groups.iter()) {
                g.fee_bps = og.fee_bps;
                for (l, ol) in g.liqs.iter_mut().zip(og.liqs.iter()) {
                    l.protocol_pull = ol.protocol_pull;
                }
            }
            p
        }, &c);
        prop_assert!(re.is_ok());
        let re = re.unwrap();
        prop_assert_eq!(re.as_bytes(), enc.as_bytes());
    }
}

fn router_data_padded(extra: usize) -> Vec<u8> {
    let mut d = ROUTER_A.to_vec();
    d.extend(std::iter::repeat_n(0x11u8, extra));
    d
}

/// One group. `liq_n`, repay-swap count, profit-swap count, adapter, and
/// router `data` length move on different moduli so none determines another.
fn varied_case(i: u32) -> BatchPlan {
    let c = ctx();
    let liq_n = u8::try_from((i / 3) % 3 + 1).unwrap();
    let repay_n = u8::try_from((i / 9) % 3 + 1).unwrap();
    let profit_extra = (i / 27) % 2 == 1;
    let pad = usize::try_from(i % 32).unwrap();
    let pull = 1_000_000_000u128;
    let total = pull.saturating_mul(u128::from(liq_n));
    let (provider, src, debt, coll, liqs) = match i % 3 {
        0 => (
            FlashProvider::Aave,
            AAVE_V3,
            DAI,
            WETH,
            (0..liq_n).map(|_| v3_leg(WETH, pull, pull)).collect(),
        ),
        1 => (
            FlashProvider::UniV4,
            UNIV4_PM,
            USDC,
            WETH,
            (0..liq_n).map(|_| v4_leg(WETH, 0, 1, pull, pull)).collect(),
        ),
        _ => (
            FlashProvider::Morpho,
            MORPHO,
            WETH,
            WSTETH,
            (0..liq_n).map(|_| morpho_leg(pull, pull, &c)).collect(),
        ),
    };
    // Each leg's pull over `repay_n` exact outputs tied to it.
    let mut repay = Vec::with_capacity(usize::from(repay_n) * usize::from(liq_n));
    for leg in 0..usize::from(liq_n) {
        let flags = tie_flags(LEG_EXACT_OUT, leg).unwrap();
        let mut acc = 0u128;
        for k in 0..repay_n {
            let amt = if k + 1 == repay_n {
                pull - acc
            } else {
                pull / u128::from(repay_n)
            };
            acc = acc.saturating_add(amt);
            if k % 2 == 1 {
                repay.push(SwapLeg {
                    venue: VENUE_ROUTER,
                    token_in: coll,
                    token_out: debt,
                    flags,
                    amount: amt,
                    data: router_data_padded(pad),
                });
            } else {
                let mut s = exact_out(coll, debt, amt);
                s.flags = flags;
                repay.push(s);
            }
        }
    }
    let mut profit = Vec::new();
    // WETH collateral is already the profit asset. A closer is WETH→WETH.
    if coll != WETH {
        if profit_extra {
            profit.push(SwapLeg {
                venue: VENUE_ROUTER,
                token_in: coll,
                token_out: WETH,
                flags: LEG_EXACT_OUT,
                amount: 1,
                data: router_data_padded(pad),
            });
        }
        profit.push(profit_tb(coll));
    }
    BatchPlan {
        flags: FLAG_SWEEP,
        bid_bps: 1u16.saturating_add(u16::try_from(i % 9_000).unwrap()),
        gas_cost_wei: 1u128.saturating_add(u128::from(i)),
        min_profit_wei: 1u128.saturating_add(u128::from(i % 97)),
        groups: vec![group(provider, src, debt, total, liqs, repay)],
        profit_swaps: profit,
    }
}

/// Writes `contracts/test/encoding/generated.bin` for the Foundry decoder.
#[test]
fn write_solidity_roundtrip_cases() {
    let c = ctx();
    let mut blob = Vec::new();
    let n: u32 = 256;
    blob.extend_from_slice(&n.to_be_bytes());
    for i in 0..n {
        let p = varied_case(i);
        let bytes = EncodedPlan::encode(&p, &c).unwrap().into_bytes();
        let len = u32::try_from(bytes.len()).unwrap();
        blob.extend_from_slice(&len.to_be_bytes());
        blob.extend_from_slice(&bytes);
    }
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/test/encoding/generated.bin"
    );
    std::fs::create_dir_all(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/test/encoding"
    ))
    .unwrap();
    std::fs::write(path, blob).unwrap();
}

#[test]
fn varied_cases_move_counts_independently() {
    let mut liq = std::collections::BTreeSet::new();
    let mut repay = std::collections::BTreeSet::new();
    let mut profit = std::collections::BTreeSet::new();
    let mut data = std::collections::BTreeSet::new();
    let mut v4_liq = std::collections::BTreeSet::new();
    let mut morpho_liq = std::collections::BTreeSet::new();
    for i in 0..256u32 {
        let p = varied_case(i);
        let g = &p.groups[0];
        liq.insert(g.liqs.len());
        repay.insert(g.repay_swaps.len());
        profit.insert(p.profit_swaps.len());
        for s in g.repay_swaps.iter().chain(p.profit_swaps.iter()) {
            data.insert(s.data.len());
        }
        match g.liqs[0].adapter {
            ExecutorAdapter::AaveV4 => {
                v4_liq.insert(g.liqs.len());
            }
            ExecutorAdapter::MorphoBlue => {
                morpho_liq.insert(g.liqs.len());
            }
            _ => {}
        }
        EncodedPlan::encode(&p, &ctx()).unwrap();
    }
    assert!(liq.len() > 1 && repay.len() > 1 && profit.len() > 1 && data.len() > 1);
    assert!(
        v4_liq.contains(&2),
        "V4 tail stride must be reached at liq index ≥ 1"
    );
    assert!(
        morpho_liq.contains(&2),
        "Morpho tail stride must be reached at liq index ≥ 1"
    );
}

#[test]
fn zero_min_profit_is_refused() {
    let c = ctx();
    let mut p = plan_v3();
    p.min_profit_wei = 0;
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(liq_plan::EncodeError::ZeroMinProfit)
    ));
}

const UNIV2_DAI_WETH: Address = address!("A478c2975Ab1Ea89e8196811F51A7B7Ade33eB11");
const CURVE_3POOL: Address = address!("bEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7");

fn v2_data(fid: u8) -> Vec<u8> {
    let mut d = UNIV2_DAI_WETH.to_vec();
    d.push(fid);
    d
}

/// MetaRegistry handler index of the 3pool: the base registry's handler,
/// `get_registry(0)` on mainnet.
const CURVE_3POOL_HANDLER: u8 = 0;

/// Venue 3 / 4 data: pool ‖ i ‖ j ‖ MetaRegistry handler index.
fn curve_data(i: u8, j: u8) -> Vec<u8> {
    let mut d = CURVE_3POOL.to_vec();
    d.extend_from_slice(&[i, j, CURVE_3POOL_HANDLER]);
    d
}

/// A V2 pair-direct exact-out repay validates and round-trips.
#[test]
fn univ2_exact_out_repay_round_trips() {
    let c = ctx();
    let mut p = plan_v3();
    let r = &mut p.groups[0].repay_swaps[0];
    r.venue = VENUE_UNIV2_POOL;
    r.data = v2_data(0);
    let bytes = EncodedPlan::encode(&p, &c).expect("validate").into_bytes();
    assert!(wire_eq(&p, &decode_batch(&bytes).unwrap()));

    p.groups[0].repay_swaps[0].data = v2_data(2);
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::BadV2Factory(2))
    ));
    p.groups[0].repay_swaps[0].data = UNIV2_DAI_WETH.to_vec();
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::BadV2DataLen(20))
    ));
}

/// Curve repays exact-in with an overshoot. The exact-out sum may then be
/// below what is owed, but only if the surplus debt is swept to WETH.
#[test]
fn curve_exact_in_repay_requires_surplus_sweep() {
    let c = ctx();
    let mut p = plan_v3();
    let owed = p.groups[0].repay_swaps[0].amount;
    p.groups[0].repay_swaps[0] = SwapLeg {
        venue: VENUE_CURVE_POOL,
        token_in: WETH,
        token_out: DAI,
        flags: 0,
        amount: owed,
        data: curve_data(1, 0),
    };
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::SurplusDebtUnrouted { .. })
    ));
    p.profit_swaps.push(profit_tb(DAI));
    let bytes = EncodedPlan::encode(&p, &c).expect("validate").into_bytes();
    assert!(wire_eq(&p, &decode_batch(&bytes).unwrap()));

    p.groups[0].repay_swaps[0].flags = LEG_EXACT_OUT;
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::CurveExactOut)
    ));
    p.groups[0].repay_swaps[0].flags = 0;
    p.groups[0].repay_swaps[0].data = curve_data(1, 0)[..21].to_vec();
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::BadCurveDataLen(21))
    ));
    // The layout before the handler byte (pool ‖ i ‖ j): the Executor
    // refuses it, so the encoder does too.
    p.groups[0].repay_swaps[0].data = curve_data(1, 0)[..22].to_vec();
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::BadCurveDataLen(22))
    ));
}

/// Without an exact-in leg the exact outputs buy exactly the pull.
#[test]
fn exact_out_short_without_exact_in_leg_is_refused() {
    let c = ctx();
    let mut p = plan_v3();
    p.groups[0].repay_swaps[0].amount -= 1;
    p.profit_swaps.push(profit_tb(DAI));
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::RepayNotSizedToPull { .. })
    ));
}

/// A governance plan is the ordinary plan plus the flag and a 5-byte id:
/// every byte before the id is unchanged, and the wire decoder reads it back.
#[test]
fn gov_payload_appends_the_id_and_sets_the_flag() {
    use liq_plan::FLAG_GOV_EXEC;
    let plain = EncodedPlan::encode(&plan_v3(), &ctx()).unwrap();
    let gov = plain.clone().with_gov_payload(469).unwrap();
    let (p, g) = (plain.as_bytes(), gov.as_bytes());
    assert_eq!(g.len(), p.len() + 5);
    assert_eq!(g[0], p[0] | FLAG_GOV_EXEC);
    assert_eq!(&g[1..p.len()], &p[1..]);
    assert_eq!(&g[p.len()..], &[0x00, 0x00, 0x00, 0x01, 0xd5]);
    let h = *liq_wire::wire::Plan::parse(g).unwrap().header();
    assert_eq!(h.payload_id, Some(469));
    assert_eq!(h.flags & FLAG_SWEEP, FLAG_SWEEP, "other flags kept");

    let top = liq_wire::wire::PAYLOAD_ID_MAX;
    let g = plain.clone().with_gov_payload(top).unwrap();
    assert_eq!(
        liq_wire::wire::Plan::parse(g.as_bytes())
            .unwrap()
            .header()
            .payload_id,
        Some(top)
    );
}

#[test]
fn gov_payload_refuses_range_repeat_and_bare_flag() {
    use liq_plan::FLAG_GOV_EXEC;
    let plain = EncodedPlan::encode(&plan_v3(), &ctx()).unwrap();
    let over = liq_wire::wire::PAYLOAD_ID_MAX + 1;
    assert_eq!(
        plain.clone().with_gov_payload(over),
        Err(EncodeError::PayloadIdRange(over))
    );
    let once = plain.with_gov_payload(1).unwrap();
    assert_eq!(once.with_gov_payload(2), Err(EncodeError::GovPayloadTwice));
    let mut p = plan_v3();
    p.flags |= FLAG_GOV_EXEC;
    assert_eq!(
        EncodedPlan::encode(&p, &ctx()),
        Err(EncodeError::GovFlagWithoutPayload)
    );
}

#[test]
fn gov_spell_appends_the_address_and_excludes_a_payload() {
    use liq_plan::{FLAG_GOV_EXEC, FLAG_GOV_SPELL};
    let spell = address!("F01b594aF26fC8A8ae1e24DCaF904ECB6Fd1BaDC");
    let plain = EncodedPlan::encode(&plan_v3(), &ctx()).unwrap();
    let g = plain.clone().with_gov_spell(spell).unwrap();
    assert_eq!(g.as_bytes()[0], plain.as_bytes()[0] | FLAG_GOV_SPELL);
    assert_eq!(&g.as_bytes()[plain.as_bytes().len()..], spell.as_slice());
    let h = *liq_wire::wire::Plan::parse(g.as_bytes()).unwrap().header();
    assert_eq!(h.spell, Some(spell));
    assert_eq!(
        g.clone().with_gov_payload(1),
        Err(EncodeError::GovPayloadTwice)
    );
    assert_eq!(
        plain
            .clone()
            .with_gov_payload(1)
            .unwrap()
            .with_gov_spell(spell),
        Err(EncodeError::GovPayloadTwice)
    );
    assert_eq!(
        plain.with_gov_spell(Address::ZERO),
        Err(EncodeError::ZeroSpell)
    );
    let mut p = plan_v3();
    p.flags |= FLAG_GOV_SPELL;
    assert_eq!(
        EncodedPlan::encode(&p, &ctx()),
        Err(EncodeError::GovFlagWithoutPayload)
    );
    let _ = FLAG_GOV_EXEC;
}

/// A Curve crypto leg (venue 4) is exact-in like a plain Curve leg: the same
/// 23-byte data, the same surplus-sweep rule, exact-out refused.
#[test]
fn curve_crypto_leg_validates_like_curve() {
    let c = ctx();
    let mut p = plan_v3();
    let owed = p.groups[0].repay_swaps[0].amount;
    p.groups[0].repay_swaps[0] = SwapLeg {
        venue: liq_plan::VENUE_CURVE_CRYPTO_POOL,
        token_in: WETH,
        token_out: DAI,
        flags: 0,
        amount: owed,
        data: curve_data(2, 0),
    };
    p.profit_swaps.push(profit_tb(DAI));
    let bytes = EncodedPlan::encode(&p, &c).expect("validate").into_bytes();
    let back = decode_batch(&bytes).unwrap();
    assert!(wire_eq(&p, &back));
    p.groups[0].repay_swaps[0].flags = LEG_EXACT_OUT;
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::CurveExactOut)
    ));
    p.groups[0].repay_swaps[0].flags = 0;
    p.groups[0].repay_swaps[0].data = curve_data(2, 0)[..22].to_vec();
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::BadCurveDataLen(22))
    ));
}

/// A Curve NG LP is withdrawn as one coin first (venue 7). Its data is the
/// LP itself (the pool), the coin index and the pool's MetaRegistry handler
/// index: 22 bytes. The layout before the handler byte is refused, as the
/// Executor refuses it (`SwapModule._withdrawCurveLp`).
#[test]
fn curve_lp_withdrawal_leg_carries_the_handler_byte() {
    const LP: Address = address!("3ee841f47947fefbe510366e4bbb49e145484195"); // NG USR/USDC
    const NG_HANDLER: u8 = 6;
    let c = ctx();
    let mut p = plan_v3();
    let owed = p.groups[0].repay_swaps[0].amount;
    for l in &mut p.groups[0].liqs {
        l.collateral_asset = LP;
    }
    let mut data = LP.to_vec();
    data.extend_from_slice(&[1, NG_HANDLER]);
    let withdraw = SwapLeg {
        venue: liq_plan::VENUE_CURVE_LP_ONE_COIN,
        token_in: LP,
        token_out: USDC,
        flags: LEG_TAKE_BALANCE,
        amount: 0,
        data,
    };
    p.groups[0].repay_swaps = vec![withdraw.clone(), exact_out(USDC, DAI, owed)];
    p.profit_swaps = vec![profit_tb(USDC)];
    let bytes = EncodedPlan::encode(&p, &c).expect("validate").into_bytes();
    assert!(wire_eq(&p, &decode_batch(&bytes).unwrap()));

    let mut old = withdraw.clone();
    old.data.truncate(21);
    p.groups[0].repay_swaps = vec![old, exact_out(USDC, DAI, owed)];
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::BadUnwrapData(21))
    ));
    // The data must name the LP being spent.
    let mut other = withdraw;
    other.data[..20].copy_from_slice(USDC.as_slice());
    p.groups[0].repay_swaps = vec![other, exact_out(USDC, DAI, owed)];
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::BadUnwrapData(22))
    ));
}

/// Seized ERC-4626 shares (`VAULT`, wrapping WETH… here DAI-debt with a USDC
/// vault) are redeemed first; the repay then sells the asset, and the asset's
/// residual is closed to WETH.
#[test]
fn unwrap_leg_converts_the_collateral_before_the_repay() {
    const VAULT: Address = address!("dd0f28e19c1780eb6396170735d45153d261490d"); // gtUSDC
    let c = ctx();
    let mut p = plan_v3();
    let owed = p.groups[0].repay_swaps[0].amount;
    for l in &mut p.groups[0].liqs {
        l.collateral_asset = VAULT;
    }
    let unwrap = SwapLeg {
        venue: liq_plan::VENUE_UNWRAP_4626,
        token_in: VAULT,
        token_out: USDC,
        flags: LEG_TAKE_BALANCE,
        amount: 0,
        data: VAULT.to_vec(),
    };
    let repay = exact_out(USDC, DAI, owed);
    p.groups[0].repay_swaps = vec![unwrap.clone(), repay.clone()];
    // The unwrapped USDC must be closed to WETH.
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::UnwrapOutputUnclosed { asset }) if asset == USDC
    ));
    p.profit_swaps = vec![profit_tb(USDC)];
    let bytes = EncodedPlan::encode(&p, &c).expect("validate").into_bytes();
    assert!(wire_eq(&p, &decode_batch(&bytes).unwrap()));

    // Unwrap after a selling leg: the shares would be sold unconverted.
    p.groups[0].repay_swaps = vec![repay, unwrap.clone()];
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::UnwrapNotFirst)
    ));
    // The data must name the vault being spent.
    let mut bad = unwrap;
    bad.data = USDC.to_vec();
    p.groups[0].repay_swaps = vec![bad, exact_out(USDC, DAI, owed)];
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::BadUnwrapData(20))
    ));
}

/// An expired Pendle PT is redeemed first (venue 6, data = its YT) and
/// validates like any unwrap: first, exact input, output closed.
#[test]
fn pendle_pt_redeem_leg_validates_like_an_unwrap() {
    const PT: Address = address!("9f56094c450763769ba0ea9fe2876070c0fd5f77");
    const YT: Address = address!("029d6247adb0a57138c62e3019c92d3dfc9c1840");
    let c = ctx();
    let mut p = plan_v3();
    let owed = p.groups[0].repay_swaps[0].amount;
    for l in &mut p.groups[0].liqs {
        l.collateral_asset = PT;
    }
    let redeem = SwapLeg {
        venue: liq_plan::VENUE_PENDLE_PT_REDEEM,
        token_in: PT,
        token_out: USDC,
        flags: LEG_TAKE_BALANCE,
        amount: 0,
        data: YT.to_vec(),
    };
    p.groups[0].repay_swaps = vec![redeem.clone(), exact_out(USDC, DAI, owed)];
    p.profit_swaps = vec![profit_tb(USDC)];
    let bytes = EncodedPlan::encode(&p, &c).expect("validate").into_bytes();
    assert!(wire_eq(&p, &decode_batch(&bytes).unwrap()));
    let mut exact = redeem.clone();
    exact.flags = LEG_EXACT_OUT;
    p.groups[0].repay_swaps = vec![exact, exact_out(USDC, DAI, owed)];
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::UnwrapExactOut)
    ));
    let mut empty = redeem;
    empty.data = vec![0; 20];
    p.groups[0].repay_swaps = vec![empty, exact_out(USDC, DAI, owed)];
    assert!(matches!(
        EncodedPlan::encode(&p, &c),
        Err(EncodeError::BadUnwrapData(20))
    ));
}
