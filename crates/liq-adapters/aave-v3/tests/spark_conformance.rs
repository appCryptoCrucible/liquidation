//! Spark TOML → aave-v3 `Config` → existing GUIDE 01 §8 checks.
//! Adapter `src/` is not forked.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::inconsistent_digit_grouping,
    clippy::cast_possible_truncation
)]

mod common;

use alloy_primitives::{address, uint, Address, B256, U256};
use common::*;
use liq_adapters_aave_v3::events::{halt, oracle, pool};
use liq_adapters_aave_v3::{
    alloc_meter, math, AaveV3, AssetConfig, Config, LiquidationParams, PoolConfig, SourcePin,
};
use liq_config::{AaveV3Toml, Intern, OnChainId, Registry};
use liq_protocol::conformance::{run, Fixtures, LogFixture, PositionFixture};
use liq_protocol::FeedId;
use liq_protocol::{
    CallbackShape, DirtySet, ExecutorAdapter, FlashRoute, HealthState, LegChoice, Protocol,
    ProtocolError,
};
use liq_types::fixed::RAY;
use liq_types::{AssetId, MarketId, Price, PriceVector, ProtocolId, Ray, SourceKind, Wad};

const SPARK_WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
const SPARK_DAI: Address = address!("0x6B175474E89094C44Da98b954EedeAC495271d0F");
const SPARK_GNO: Address = address!("0x6810e776880C02933D47DB1b9fc05908e5386b96");
const SPARK_POOL: Address = address!("0xc13e21B648A5Ee794902342038FF3aDab66Be987");
const SPARK_PROVIDER: Address = address!("0x02C3eA4e34C0cBd694D2adFA2c690EeCbc1793eE");
const YEAR: U256 = uint!(31_536_000_U256);
const WAD: U256 = uint!(1_000_000_000_000_000_000_U256);

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .unwrap()
}

fn load_spark_toml() -> AaveV3Toml {
    AaveV3Toml::from_path(&workspace_root().join("config/protocols/spark.toml")).unwrap()
}

fn to_config(t: &AaveV3Toml) -> Config {
    Config {
        protocol: ProtocolId(t.protocol),
        pools: t
            .pools
            .iter()
            .map(|p| PoolConfig {
                address: p.address,
                market: MarketId(p.market),
                oracle: p.oracle,
                provider: p.provider,
                configurator: p.configurator,
                sentinel: p.sentinel,
                sequencer_oracle: p.sequencer_oracle,
            })
            .collect(),
        assets: t
            .assets
            .iter()
            .map(|a| AssetConfig {
                underlying: a.underlying,
                asset: AssetId(a.asset),
                feed: FeedId(a.feed),
                siloed: a.siloed,
                isolated: a.isolated,
                debt_ceiling: a.debt_ceiling,
                decimals: a.decimals,
            })
            .collect(),
        price_sources: t
            .price_sources
            .iter()
            .map(|s| SourcePin {
                pool: s.pool,
                underlying: s.underlying,
                source: s.source,
            })
            .collect(),
        liquidation: LiquidationParams {
            close_factor_bps: t.liquidation.close_factor_bps,
            close_factor_hf_wad: t.liquidation.close_factor_hf_wad,
            min_base_max_close: t.liquidation.min_base_max_close,
            oracle_decimals: t.liquidation.oracle_decimals,
        },
        pinned_through: t.pinned_through,
    }
}

fn spark_coverage_ranks() -> Vec<(B256, u8)> {
    const DOC: &str = include_str!("../../../../docs/coverage/spark.md");
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
    assert!(out.len() >= 50, "coverage table parsed {} rows", out.len());
    out
}

fn spark_deploy(cfg: &Config) -> Deploy {
    let pool = &cfg.pools[0];
    let src_weth = cfg
        .price_sources
        .iter()
        .find(|s| s.pool == pool.address && s.underlying == SPARK_WETH)
        .unwrap()
        .source;
    let src_dai = cfg
        .price_sources
        .iter()
        .find(|s| s.pool == pool.address && s.underlying == SPARK_DAI)
        .unwrap()
        .source;
    let mut d = Deploy::new();
    d.pool = pool.address;
    d.oracle = pool.oracle;
    d.provider = pool.provider;
    d.configurator = pool.configurator;
    d.weth = SPARK_WETH;
    d.dai = SPARK_DAI;
    d.src_weth = src_weth;
    d.src_dai = src_dai;
    d
}

fn spark_ids(cfg: &Config) -> (AssetId, AssetId) {
    let w = cfg
        .assets
        .iter()
        .find(|a| a.underlying == SPARK_WETH)
        .unwrap()
        .asset;
    let d = cfg
        .assets
        .iter()
        .find(|a| a.underlying == SPARK_DAI)
        .unwrap()
        .asset;
    (w, d)
}

fn spark_prices(weth: AssetId, dai: AssetId, weth_p8: u64, dai_p8: u64) -> PriceVector {
    let n = usize::from(weth.0)
        .max(usize::from(dai.0))
        .saturating_add(1);
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let asset = AssetId(u16::try_from(i).unwrap());
        v.push(Price {
            asset,
            price: Ray::from_raw(U256::ZERO),
            source: SourceKind::Canonical,
            block: DEPLOY_BLOCK,
            ts: T0,
        });
    }
    v[usize::from(weth.0)] = Price {
        asset: weth,
        price: Ray::from_raw(U256::from(weth_p8) * P8_TO_RAY),
        source: SourceKind::Canonical,
        block: DEPLOY_BLOCK,
        ts: T0,
    };
    v[usize::from(dai.0)] = Price {
        asset: dai,
        price: Ray::from_raw(U256::from(dai_p8) * P8_TO_RAY),
        source: SourceKind::Canonical,
        block: DEPLOY_BLOCK,
        ts: T0,
    };
    PriceVector(v)
}

fn extra_logs(d: &Deploy) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 2, T1);
    vec![
        log(
            d.pool,
            &pool::ReserveDataUpdated {
                reserve: d.dai,
                liquidityRate: DAI_RATE,
                stableBorrowRate: U256::ZERO,
                variableBorrowRate: DAI_RATE,
                liquidityIndex: RAY,
                variableBorrowIndex: RAY + DAI_RATE * U256::from(T1 - T0) / YEAR,
            },
            b,
            t,
        ),
        log(
            d.configurator,
            &liq_adapters_aave_v3::events::cfg::ReservePaused {
                asset: d.weth,
                paused: true,
            },
            b,
            t,
        ),
        log(
            d.oracle,
            &oracle::AssetSourceUpdated {
                asset: d.weth,
                source: Address::repeat_byte(0xff),
            },
            b,
            t,
        ),
        log(
            d.oracle,
            &oracle::AssetSourceUpdated {
                asset: d.weth,
                source: d.src_weth,
            },
            b,
            t,
        ),
        log(
            d.pool,
            &pool::Repay {
                reserve: d.dai,
                user: d.alice,
                repayer: d.alice,
                amount: uint!(500_000_000_000_000_000_000_U256),
                useATokens: false,
            },
            b,
            t,
        ),
        log(
            d.pool,
            &pool::LiquidationCall {
                collateralAsset: d.weth,
                debtAsset: d.dai,
                user: d.alice,
                debtToCover: uint!(100_000_000_000_000_000_000_U256),
                liquidatedCollateralAmount: uint!(50_000_000_000_000_000_U256),
                liquidator: Address::repeat_byte(0xee),
                receiveAToken: true,
            },
            b,
            t,
        ),
        log(
            d.pool,
            &halt::Upgraded {
                implementation: Address::repeat_byte(0x77),
            },
            DEPLOY_BLOCK,
            T0,
        ),
    ]
}

fn alice_by_hand(weth_p8: u64, dai_p8: u64, liq_idx: U256, debt_idx: U256) -> (U256, U256, U256) {
    let coll_assets = ALICE_WETH * liq_idx / RAY;
    let coll_base = coll_assets * U256::from(weth_p8) / uint!(1_000_000_000_000_000_000_U256);
    let weighted = coll_base * U256::from(WETH_LT);
    let debt_assets = (ALICE_DAI_DEBT * debt_idx + RAY - U256::ONE) / RAY;
    let debt_base = (debt_assets * U256::from(dai_p8) + uint!(1_000_000_000_000_000_000_U256)
        - U256::ONE)
        / uint!(1_000_000_000_000_000_000_U256);
    let hf = (weighted * WAD + debt_base / U256::from(2u8)) / debt_base / uint!(10_000_U256);
    (hf, coll_base, debt_base)
}

struct SparkHarness {
    p: AaveV3,
    d: Deploy,
    weth: AssetId,
    dai: AssetId,
}

fn harness() -> SparkHarness {
    let toml = load_spark_toml();
    let cfg = to_config(&toml);
    cfg.validate().unwrap();
    let p = AaveV3::new(cfg.clone()).expect("spark config validates");
    let (weth, dai) = spark_ids(&cfg);
    SparkHarness {
        p,
        d: spark_deploy(&cfg),
        weth,
        dai,
    }
}

fn full_store(h: &SparkHarness) -> liq_protocol::conformance::JournalStore {
    let mut logs = listing_logs(&h.d);
    logs.extend(activity_logs(&h.d));
    store_after(&h.p, &logs)
}

#[test]
fn spark_toml_matches_registry_and_on_chain_pins() {
    let toml = load_spark_toml();
    let cfg = to_config(&toml);
    assert_eq!(cfg.pools[0].address, SPARK_POOL);
    assert_eq!(cfg.pools[0].provider, SPARK_PROVIDER);
    assert_eq!(
        cfg.pools[0].oracle,
        address!("0x8105f69D9C41644c6A0803fDA7D03AA70996cFD9")
    );
    assert_eq!(cfg.assets.len(), 20);
    assert_eq!(cfg.liquidation.close_factor_bps, 5_000);
    assert_eq!(cfg.liquidation.close_factor_hf_wad, 950_000_000_000_000_000);
    assert_eq!(cfg.liquidation.min_base_max_close, 0);
    assert_eq!(cfg.liquidation.oracle_decimals, 8);
    assert_eq!(cfg.pinned_through, 26_018_679);

    let reg = Registry::from_path(&workspace_root().join("registry/registry.json")).unwrap();
    let intern = Intern::from_registry(&reg).unwrap();
    assert_eq!(intern.protocol("spark"), Some(ProtocolId(8)));
    let spark_market = intern
        .markets()
        .iter()
        .find(|m| m.key == OnChainId::Addr(SPARK_POOL))
        .unwrap();
    assert_eq!(spark_market.id, MarketId(3480));
    assert_eq!(intern.asset(SPARK_WETH), Some(AssetId(813)));
    assert_eq!(intern.asset(SPARK_DAI), Some(AssetId(454)));
    // GNO joined the registry through the asset-id ledger at 1073 — the id
    // spark.toml already reserved for it — so the pin now matches the intern.
    assert_eq!(intern.asset(SPARK_GNO), Some(AssetId(1073)));
    let gno = cfg
        .assets
        .iter()
        .find(|a| a.underlying == SPARK_GNO)
        .unwrap();
    assert!(gno.isolated);
    assert_eq!(gno.debt_ceiling, 500_000_000);
    assert_eq!(gno.asset, AssetId(1073));
}

#[test]
fn ten_checks_pass_with_spark_config() {
    let h = harness();
    let st = full_store(&h);
    let px_liq = spark_prices(h.weth, h.dai, 1800_0000_0000, DAI_P8);
    let px_ok = spark_prices(h.weth, h.dai, WETH_P8, DAI_P8);
    let positions = [
        PositionFixture {
            pos: st.view(ALICE_ID, T0).unwrap(),
            px: &px_liq,
            post: None,
        },
        PositionFixture {
            pos: st.view(ALICE_ID, T1).unwrap(),
            px: &px_ok,
            post: None,
        },
        PositionFixture {
            pos: st.view(ALICE_ID, T1).unwrap(),
            px: &px_liq,
            post: None,
        },
        PositionFixture {
            pos: st.view(BOB_ID, T1).unwrap(),
            px: &px_ok,
            post: None,
        },
    ];
    let ranks = spark_coverage_ranks();
    let mut owned = activity_logs(&h.d);
    owned.extend(extra_logs(&h.d));
    let logs: Vec<LogFixture<'_>> = owned
        .iter()
        .map(|l| LogFixture {
            log: l.view(),
            max_dirty_rank: rank_of(&ranks, l.topics[0]),
        })
        .collect();
    let flash_sources: Vec<(CallbackShape, Address)> = CallbackShape::ALL
        .iter()
        .enumerate()
        .map(|(i, s)| (*s, Address::repeat_byte(0x50 + i as u8)))
        .collect();
    let fx = Fixtures {
        positions: &positions,
        logs: &logs,
        flash_sources: &flash_sources,
        recipient: Address::repeat_byte(0x99),
    };
    let mut log_store = store_after(&h.p, &listing_logs(&h.d));
    let rep = run(
        &h.p,
        &mut log_store,
        &fx,
        alloc_meter().map(|m| m as &dyn Fn() -> u64),
    )
    .unwrap_or_else(|f| panic!("{f}"));
    for (i, n) in rep.assertions.iter().enumerate() {
        if i == 3 {
            assert_eq!(*n, 0, "check 4 needs a fork post-state (04C)");
        } else {
            assert!(*n > 0, "check {} was vacuous", i + 1);
        }
    }
}

#[test]
fn health_matches_hand_derivation_at_t0() {
    let h = harness();
    let st = full_store(&h);
    let px = spark_prices(h.weth, h.dai, 1800_0000_0000, DAI_P8);
    let hf = h.p.health(st.view(ALICE_ID, T0).unwrap(), &px).unwrap();
    let (exp, coll, debt) = alice_by_hand(1800_0000_0000, DAI_P8, RAY, RAY);
    assert_eq!(hf.hf, Ray::from_raw(exp * uint!(1_000_000_000_U256)));
    assert_eq!(
        hf.collateral_value,
        Wad::from_raw(coll * uint!(10_000_000_000_U256))
    );
    assert_eq!(
        hf.debt_value,
        Wad::from_raw(debt * uint!(10_000_000_000_U256))
    );
    assert_eq!(hf.state, HealthState::Liquidatable);
}

#[test]
fn quote_static_bonus_and_encode() {
    let h = harness();
    let st = full_store(&h);
    let px = spark_prices(h.weth, h.dai, 1800_0000_0000, DAI_P8);
    let pos = st.view(ALICE_ID, T0).unwrap();
    let q = h.p.quote(pos, &px).unwrap().expect("liquidatable");
    assert_eq!(q.repay_options[0].asset, h.dai);
    assert_eq!(q.seize_options[0].asset, h.weth);
    assert_eq!(
        q.seize_options[0].bonus,
        Ray::from_raw(U256::from(500u16) * math::BPS_RAY)
    );
    let route = FlashRoute {
        provider: CallbackShape::AaveExecuteOperation.provider(),
        source: Address::repeat_byte(0x50),
        asset: h.dai,
        amount: q.repay_options[0].max_repay,
        fee_bps: 0,
        callback: CallbackShape::AaveExecuteOperation,
    };
    let plan =
        h.p.encode(&q, LegChoice::PREFERRED, &route, Address::repeat_byte(0x99))
            .unwrap();
    assert_eq!(plan.leg.adapter, ExecutorAdapter::AaveV3);
    assert_eq!(plan.leg.market, h.d.pool);
    let mut q2 = q.clone();
    q2.key.protocol = ProtocolId(99);
    assert_eq!(
        h.p.encode(
            &q2,
            LegChoice::PREFERRED,
            &route,
            Address::repeat_byte(0x99)
        ),
        Err(ProtocolError::ProtocolMismatch)
    );
}

#[test]
fn oracle_source_swap_fails_closed() {
    let h = harness();
    let mut st = full_store(&h);
    let px = spark_prices(h.weth, h.dai, 1800_0000_0000, DAI_P8);
    let swap = log(
        h.d.oracle,
        &oracle::AssetSourceUpdated {
            asset: h.d.weth,
            source: Address::repeat_byte(0xff),
        },
        DEPLOY_BLOCK + 2,
        T0,
    );
    let dirty = h.p.apply_log(&mut st, &swap.view()).unwrap();
    assert!(matches!(dirty, DirtySet::MarketReprice(_)));
    assert_eq!(
        h.p.health(st.view(ALICE_ID, T0).unwrap(), &px),
        Err(ProtocolError::OracleSourceMismatch)
    );
}

#[test]
fn halt_logs_fold_before_the_pin_and_error_after() {
    let h = harness();
    let mut st = full_store(&h);
    let pin = h.p.config().pinned_through;
    let impl_ = Address::repeat_byte(0x77);
    let before = log(
        h.d.pool,
        &halt::Upgraded {
            implementation: impl_,
        },
        pin,
        T0,
    );
    assert_eq!(
        h.p.apply_log(&mut st, &before.view()).unwrap(),
        DirtySet::None
    );
    let after = log(
        h.d.pool,
        &halt::Upgraded {
            implementation: impl_,
        },
        pin + 1,
        T0,
    );
    assert_eq!(
        h.p.apply_log(&mut st, &after.view()),
        Err(ProtocolError::HaltSignal)
    );
}
