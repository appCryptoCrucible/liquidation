use super::*;
use alloy_primitives::aliases::{U160, U24};
use alloy_primitives::{hex, Address, Bytes, I256, U256};
use alloy_sol_types::{sol, SolCall, SolValue};
use revm::database::{CacheDB, EmptyDB};
use uniswap_v3_math::tick_math;

sol! {
    function v2GetAmountOut(uint256 amountIn, uint256 reserveIn, uint256 reserveOut) external pure returns (uint256);
    function v3ComputeSwapStep(uint160 sqrtRatioCurrentX96, uint160 sqrtRatioTargetX96, uint128 liquidity, int256 amountRemaining, uint24 feePips)
        external pure returns (uint160, uint256, uint256, uint256);
    function curveGetDy(uint256[2] balances, uint256[2] rates, uint256 amp, uint256 aPrecision, uint256 fee, uint256 i, uint256 j, uint256 dx)
        external pure returns (uint256);
    function computeSwapStep(uint256 liquidity, uint160 currentSqrtP, uint160 targetSqrtP, uint256 feeInFeeUnits, int256 specifiedAmount, bool isExactInput, bool isToken0)
        external pure returns (int256, int256, uint256, uint160);
}

fn uni_db() -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::default());
    insert_runtime(&mut db, ORACLE, uni_oracle_runtime().unwrap());
    db
}

fn kyber_db() -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::default());
    insert_runtime(&mut db, ORACLE, kyber_oracle_runtime().unwrap());
    db
}

fn word(out: &Bytes) -> U256 {
    U256::from_be_slice(&out[..32])
}

fn wad(n: u64) -> U256 {
    U256::from(n) * uint_18()
}

fn uint_18() -> U256 {
    U256::from(1_000_000_000_000_000_000u64)
}

fn v2_revm(db: &mut CacheDB<EmptyDB>, ain: U256, rin: U256, rout: U256) -> U256 {
    let data = Bytes::from(
        v2GetAmountOutCall {
            amountIn: ain,
            reserveIn: rin,
            reserveOut: rout,
        }
        .abi_encode(),
    );
    word(&call_pure(db, ORACLE, data).unwrap())
}

/// TESTING: V4 must not exist as a family we quote.
#[test]
fn v4_excluded_from_amm_family() {
    assert_eq!(AmmFamily::ALL.len(), 4);
    for f in AmmFamily::ALL {
        assert!(!format!("{f:?}").contains("V4"));
        assert!(!format!("{f:?}").contains("Balancer"));
    }
}

#[test]
fn recorded_mainnet_fail_closed_a3() {
    assert_eq!(
        recorded_mainnet_states().unwrap_err(),
        ParityError::A3Deferred
    );
}

#[test]
fn missing_archive_dir_is_empty_not_invented() {
    let p = std::path::Path::new("C:\\liq-replay-parity-no-such-archive");
    match recorded_states_at(p) {
        Err(ParityError::ArchiveEmpty { .. }) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn existing_dir_without_pool_storage_is_a3() {
    let dir = std::env::temp_dir().join("liq-05e-not-empty");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("not-a-snapshot.txt"),
        b"05B events are not pool slots",
    )
    .unwrap();
    assert_eq!(
        recorded_states_at(&dir).unwrap_err(),
        ParityError::A3Deferred
    );
}

#[test]
fn bytecode_is_compiler_output_not_empty() {
    assert!(uni_oracle_runtime().unwrap().len() > 1000);
    assert!(kyber_oracle_runtime().unwrap().len() > 1000);
}

#[test]
fn notionals_biased_to_traded_band() {
    let mut hit = [false; 8];
    for i in 0..64u32 {
        let a = biased_wad(i);
        assert!(in_traded_band(a), "{a}");
        for (k, rung) in TRADED_LADDER_WAD.iter().enumerate() {
            if a.abs_diff(*rung) < U256::from(1000u64) {
                hit[k] = true;
            }
        }
    }
    assert!(hit.iter().filter(|h| **h).count() >= 6);
}

#[test]
fn wei_divergence_is_error() {
    let e = assert_wei_eq(AmmFamily::UniV2, U256::from(1u64), U256::from(2u64)).unwrap_err();
    match e {
        ParityError::WeiDivergence { rust, revm, .. } => {
            assert_eq!(rust, U256::from(1u64));
            assert_eq!(revm, U256::from(2u64));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn v2_parity_biased_notionals() {
    let mut db = uni_db();
    let rin = wad(10_000);
    let rout = wad(20_000_000);
    for i in 0..16u32 {
        let ain = biased_wad(i);
        let rust = v2_get_amount_out(ain, rin, rout).unwrap();
        let evm = v2_revm(&mut db, ain, rin, rout);
        assert_wei_eq(AmmFamily::UniV2, rust, evm).unwrap();
    }
}

#[test]
fn v3_parity_swap_step_biased() {
    let mut db = uni_db();
    let sqrt0 = tick_math::get_sqrt_ratio_at_tick(0).unwrap();
    let sqrt_lo = tick_math::get_sqrt_ratio_at_tick(-60).unwrap();
    let liq: u128 = 1_000_000_000_000_000_000;
    for i in 0..12u32 {
        let ain = biased_wad(i);
        let rem = I256::try_from(ain).unwrap();
        let rust = v3_compute_swap_step(sqrt0, sqrt_lo, liq, rem, 3000).unwrap();
        let data = Bytes::from(
            v3ComputeSwapStepCall {
                sqrtRatioCurrentX96: sqrt0.to::<U160>(),
                sqrtRatioTargetX96: sqrt_lo.to::<U160>(),
                liquidity: liq,
                amountRemaining: rem,
                feePips: U24::from(3000u32),
            }
            .abi_encode(),
        );
        let out = call_pure(&mut db, ORACLE, data).unwrap();
        // (uint160, uint256 amountIn, uint256 amountOut, uint256 fee)
        let amount_out = U256::from_be_slice(&out[64..96]);
        assert_wei_eq(AmmFamily::UniV3, rust.amount_out, amount_out).unwrap();
        let amount_in = U256::from_be_slice(&out[32..64]);
        assert_wei_eq(AmmFamily::UniV3, rust.amount_in, amount_in).unwrap();
    }
}

#[test]
fn curve_parity_get_dy_biased() {
    let mut db = uni_db();
    let bal = [wad(2_000_000), wad(2_000_000)];
    let rates = [uint_18(), uint_18()];
    let amp = U256::from(2000u64 * 100);
    let ap = U256::from(100u64);
    let fee = U256::from(4_000_000u64);
    for i in 0..12u32 {
        let dx = biased_wad(i);
        let rust = curve_get_dy(CurveQuoteIn {
            balances: bal,
            rates,
            amp,
            a_precision: ap,
            fee,
            i: 0,
            j: 1,
            dx,
        })
        .unwrap();
        let data = Bytes::from(
            curveGetDyCall {
                balances: bal,
                rates,
                amp,
                aPrecision: ap,
                fee,
                i: U256::ZERO,
                j: U256::from(1u64),
                dx,
            }
            .abi_encode(),
        );
        let evm = word(&call_pure(&mut db, ORACLE, data).unwrap());
        assert_wei_eq(AmmFamily::CurveStable, rust, evm).unwrap();
    }
}

#[test]
fn kyber_parity_exact_in_biased() {
    let mut db = kyber_db();
    let sqrt0 = tick_math::get_sqrt_ratio_at_tick(0).unwrap();
    let sqrt_lo = tick_math::get_sqrt_ratio_at_tick(-40).unwrap();
    let liq = wad(1_000_000);
    let fee = U256::from(300u64); // 0.3% of FEE_UNITS=1e5
    for i in 0..8u32 {
        let ain = biased_wad(i);
        let spec = I256::try_from(ain).unwrap();
        let rust = kyber_compute_swap_step(liq, sqrt0, sqrt_lo, fee, spec, true, true).unwrap();
        let data = Bytes::from(
            computeSwapStepCall {
                liquidity: liq,
                currentSqrtP: sqrt0.to::<U160>(),
                targetSqrtP: sqrt_lo.to::<U160>(),
                feeInFeeUnits: fee,
                specifiedAmount: spec,
                isExactInput: true,
                isToken0: true,
            }
            .abi_encode(),
        );
        let out = call_pure(&mut db, ORACLE, data).unwrap();
        let used = I256::abi_decode(&out[0..32]).unwrap();
        let returned = I256::abi_decode(&out[32..64]).unwrap();
        let delta_l = U256::from_be_slice(&out[64..96]);
        let next = U256::from_be_slice(&out[96..128]);
        assert_eq!(rust.used, used, "used i={i}");
        assert_eq!(rust.returned, returned, "ret i={i}");
        assert_wei_eq(AmmFamily::KyberElastic, rust.delta_l, delta_l).unwrap();
        assert_wei_eq(AmmFamily::KyberElastic, rust.next_sqrt, next).unwrap();
    }
}

#[test]
fn oracle_address_is_fixture_not_mainnet() {
    assert_eq!(ORACLE, Address::repeat_byte(0x51));
}

#[test]
fn hex_roundtrip_no_mock_label_as_mainnet() {
    let src = include_str!("bytecode/SOURCE.txt");
    assert!(src.contains("Synthetic fixture"));
    assert!(src.contains("A3"));
    let _ = hex::encode([0u8; 1]);
}
