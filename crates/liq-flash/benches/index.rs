//! GUIDE 07 §4 acceptance: `FlashIndex::available()` is a flat indexed
//! lookup, no allocation, < 100 ns. Also timed: `best_route` (the per-pair
//! eligibility call) and `refresh` (the per-block writer cost).
//!
//! Depths are the 07A block-26M `eth_call` values (USDC / WETH across the
//! five arenas); the remaining asset slots are empty, as an untracked
//! asset is in production.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    unreachable_pub
)]

use alloy_primitives::{address, Address, U256};
use criterion::{criterion_group, criterion_main, Criterion};
use liq_flash::{
    AavePool, AaveReserve, FlashIndex, FlashSource, Haircut, HeldAsset, MorphoBlue, SkyDssFlash,
    UniV3Pool, UniV4PoolManager,
};
use liq_types::AssetId;
use std::hint::black_box;

const USDC: Address = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const AAVE_POOL: Address = address!("0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");
const AAVE_CONFIGURATOR: Address = address!("0x64b761D848206f447Fe2dd461b0c635Ec39EbB27");
const A_USDC: Address = address!("0x98C23E9d8f34FEFb1B7BD6a91B7FF122F4e16F5c");
const A_WETH: Address = address!("0x4d5F47FA6A74757f35C14fD3a6Ef8E3C9BC514E8");
const UNIV3_USDC_WETH_500: Address = address!("0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
const POOL_MANAGER: Address = address!("0x000000000004444c5dc75cB358380D2e3dE08A90");
const MORPHO: Address = address!("0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");
const DSS_FLASH: Address = address!("0x60744434d6339a6B27d73d9Eda62b6F66a0a04FA");
const END: Address = address!("0x0e2e8F1D1326A4B9633D96222Ce399c708B19c28");

const ID_USDC: AssetId = AssetId(0);
const ID_WETH: AssetId = AssetId(1);
const ID_DAI: AssetId = AssetId(2);
const ASSETS: usize = 64;

fn u(s: &str) -> U256 {
    U256::from_str_radix(s, 10).unwrap()
}

fn five() -> Vec<Box<dyn FlashSource>> {
    vec![
        Box::new(AavePool::new(
            AAVE_POOL,
            AAVE_CONFIGURATOR,
            5,
            &[
                AaveReserve {
                    asset: ID_USDC,
                    underlying: USDC,
                    atoken: A_USDC,
                    balance: U256::from(181_646_545_035_048u128),
                    flash_enabled: true,
                    active: true,
                    paused: false,
                },
                AaveReserve {
                    asset: ID_WETH,
                    underlying: WETH,
                    atoken: A_WETH,
                    balance: u("288592198471713941942983"),
                    flash_enabled: true,
                    active: true,
                    paused: false,
                },
            ],
        )),
        Box::new(UniV3Pool::new(
            UNIV3_USDC_WETH_500,
            USDC,
            WETH,
            ID_USDC,
            ID_WETH,
            500,
            U256::from(74_293_828_839_265u128),
            u("12163941530336152750397"),
        )),
        Box::new(UniV4PoolManager::new(
            POOL_MANAGER,
            &[
                HeldAsset {
                    asset: ID_USDC,
                    token: USDC,
                    balance: U256::from(66_230_362_793_739u128),
                },
                HeldAsset {
                    asset: ID_WETH,
                    token: WETH,
                    balance: u("2131150728309835187612"),
                },
            ],
        )),
        Box::new(MorphoBlue::new(
            MORPHO,
            &[
                HeldAsset {
                    asset: ID_USDC,
                    token: USDC,
                    balance: U256::from(108_682_339_582_079u128),
                },
                HeldAsset {
                    asset: ID_WETH,
                    token: WETH,
                    balance: u("15880325145738137013578"),
                },
            ],
        )),
        Box::new(SkyDssFlash::new(
            DSS_FLASH,
            END,
            ID_DAI,
            u("500000000000000000000000000"),
            U256::ZERO,
            true,
        )),
    ]
}

fn bench(c: &mut Criterion) {
    let srcs = five();
    let mut idx = FlashIndex::new(ASSETS);
    idx.refresh(&srcs);
    let h = Haircut::from_bps(9_000).unwrap();
    let need = U256::from(100_000_000_000_000u64); // 100M USDC

    c.bench_function("flash_index_available", |b| {
        b.iter(|| black_box(idx.available(black_box(ID_USDC))))
    });
    c.bench_function("flash_index_available_miss", |b| {
        b.iter(|| black_box(idx.available(black_box(AssetId(63)))))
    });
    c.bench_function("flash_index_best_route", |b| {
        b.iter(|| black_box(idx.best_route(black_box(ID_USDC), black_box(need), h)))
    });
    c.bench_function("flash_index_refresh_64_assets_5_sources", |b| {
        b.iter(|| {
            idx.refresh(&srcs);
            black_box(&idx);
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
