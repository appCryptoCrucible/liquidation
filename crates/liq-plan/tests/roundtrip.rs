//! WP 10B: encode ⇄ `liq_exec::wire` decode, ≥256 cases.
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
use liq_exec::wire::LegTail;
use liq_plan::{
    col_per_unit_debt_1e18, decode_batch, ensure_surplus_borrow_profit_legs, BatchPlan,
    CompoundMarketPin, EncodedPlan, FlashGroup, LiqLeg, LiquityTrovePin, MorphoMarketPin, SwapLeg,
    V4ReservePin, ValidateCtx, FLAG_SWEEP, HEADER_LEN, LEG_EXACT_OUT, LEG_TAKE_BALANCE,
    LIQ_LEG_LEN, SWAP_LEG_HEAD_LEN, VENUE_ROUTER, VENUE_UNIV3_POOL,
};
use liq_protocol::ExecutorAdapter;
use liq_types::fixed::{RAY, WAD};
use liq_types::FlashProvider;
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
    let pull = 1_000_000_000u128;
    BatchPlan {
        flags: FLAG_SWEEP,
        bid_bps: 9_000,
        gas_cost_wei: 12_345,
        min_profit_wei: 1,
        groups: vec![group(
            FlashProvider::Aave,
            AAVE_V3,
            DAI,
            pull,
            vec![v3_leg(WETH, pull, pull)],
            vec![exact_out(WETH, DAI, pull)],
        )],
        profit_swaps: vec![profit_tb(WETH)],
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
        profit_swaps: vec![profit_tb(WETH)],
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
        profit_swaps: vec![profit_tb(WETH), profit_tb(WSTETH)],
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
    assert_eq!(ExecutorAdapter::EulerV2.tail_len(), 32);
    assert_eq!(ExecutorAdapter::SiloV2.tail_len(), 0);
    assert_eq!(ExecutorAdapter::LiquityV2.tail_len(), 32);
    assert_eq!(ExecutorAdapter::Fluid.tail_len(), 32);
    assert_eq!(ExecutorAdapter::Gearbox.tail_len(), 32);
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
                col_per_unit_debt: WAD,
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
            min_profit_wei: 0,
            groups: vec![FlashGroup {
                provider: FlashProvider::Aave,
                flash_source: AAVE_V3,
                debt_asset: DAI,
                flash_amount: asked,
                liqs: vec![leg.clone()],
                repay_swaps: vec![exact_out(WETH, DAI, asked)],
            }],
            profit_swaps: vec![profit_tb(WETH)],
        };
        let bytes = EncodedPlan::encode(&p, &c).unwrap().into_bytes();
        let back = decode_batch(&bytes).unwrap();
        assert_eq!(back.groups[0].liqs[0].adapter, leg.adapter);
        assert_eq!(back.groups[0].liqs[0].tail, leg.tail);
        assert_eq!(back.groups[0].liqs[0].market, leg.market);
        if let LegTail::Fluid { col_per_unit_debt } = leg.tail {
            assert_eq!(col_per_unit_debt, WAD);
            assert!(col_per_unit_debt < RAY);
        }
    }
}

fn one_leg_plan(leg: LiqLeg) -> BatchPlan {
    let asked = leg.protocol_pull;
    BatchPlan {
        flags: FLAG_SWEEP,
        bid_bps: 0,
        gas_cost_wei: 0,
        min_profit_wei: 0,
        groups: vec![FlashGroup {
            provider: FlashProvider::Aave,
            flash_source: AAVE_V3,
            debt_asset: DAI,
            flash_amount: asked,
            liqs: vec![leg],
            repay_swaps: vec![exact_out(WETH, DAI, asked)],
        }],
        profit_swaps: vec![profit_tb(WETH)],
    }
}

#[test]
fn fluid_1e27_tail_rejected_1e18_helper_is_wire_unit() {
    let c = ctx();
    let asked = 1_000_000u128;
    let vault = address!("1111111111111111111111111111111111111111");
    let base = LiqLeg {
        adapter: ExecutorAdapter::Fluid,
        market: vault,
        borrower: USER,
        collateral_asset: WETH,
        repay_amount: asked,
        tail: LegTail::Fluid {
            col_per_unit_debt: RAY,
        },
        protocol_pull: asked,
    };
    assert_eq!(
        EncodedPlan::encode(&one_leg_plan(base.clone()), &c),
        Err(liq_plan::EncodeError::FluidColPerNot1e18)
    );
    let wire = col_per_unit_debt_1e18(WAD, WAD).unwrap();
    assert_eq!(wire, WAD);
    assert!(
        wire < RAY,
        "1e27 would fail pin (actualCol*1e18)/actualDebt"
    );
    let mut ok = base;
    ok.tail = LegTail::Fluid {
        col_per_unit_debt: wire,
    };
    EncodedPlan::encode(&one_leg_plan(ok), &c).expect("1e18 tail encodes");
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
            exact_out,
            pull: pl,
        }) => {
            assert_eq!(exact_out, pull + 1);
            assert_eq!(pl, pull);
        }
        other => panic!("expected UnderSeizure, got {other:?}"),
    }
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
                    let mut repay = Vec::new();
                    let n_r = if n_repay == 0 { 1 } else { n_repay };
                    let part = pull_sum / u128::from(n_r);
                    let mut left = pull_sum;
                    for ri in 0..n_r {
                        let amt = if ri + 1 == n_r { left } else { part.min(left) };
                        left = left.saturating_sub(amt);
                        repay.push(exact_out(coll0, debt, amt));
                    }
                    groups.push(group(provider, src, debt, pull_sum, liqs, repay));
                }
                let mut profit: Vec<SwapLeg> = collaterals.into_iter().map(profit_tb).collect();
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

/// Writes `contracts/test/encoding/generated.bin` for the Foundry decoder.
#[test]
fn write_solidity_roundtrip_cases() {
    let c = ctx();
    let plans = [plan_v3(), plan_v4_clamped(), plan_morpho(), plan_multi()];
    let mut blob = Vec::new();
    let n: u32 = 256;
    blob.extend_from_slice(&n.to_be_bytes());
    for i in 0..n {
        let p = plans[i as usize % plans.len()].clone();
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

    let contracts = concat!(env!("CARGO_MANIFEST_DIR"), "/../../contracts");
    let forge = if cfg!(windows) { "forge.exe" } else { "forge" };
    let status = std::process::Command::new(forge)
        .args(["test", "--match-contract", "PlanEncodingRoundTrip", "-vv"])
        .current_dir(contracts)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("forge PlanEncodingRoundTrip failed: {s}"),
        Err(e) => panic!("forge not runnable ({e}); Solidity round-trip is required"),
    }
}
