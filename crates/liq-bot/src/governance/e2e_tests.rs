//! A governance change, end to end through the real Aave V3 adapter: state
//! from the adapter's own event fixture (Alice: 1 WETH at LT 82.5 % against
//! 1 500 DAI, WETH 2 000 → HF 1.1), a simulated `CollateralConfigurationChanged`
//! routed by `GovRoutes`, laid over the store by `apply_sim_logs`, and the
//! accounts it makes liquidatable found by `newly_liquidatable`. Oracle: the
//! adapter's own health at the changed parameters, and the fixture's numbers.

use super::*;
use alloy_primitives::{uint, U256};
use alloy_sol_types::SolEvent;
use liq_adapters_aave_v3::events::{cfg as ccfg, oracle, pool};
use liq_adapters_aave_v3::{
    AaveV3, AssetConfig, BalanceModel, CloseFactorScope, Config, LiquidationParams, PoolConfig,
    SourcePin,
};
use liq_engine::EngineConfig;
use liq_flash::{AavePool, AaveReserve, DepthOnlyRouteCache, FlashSource};
use liq_protocol::FeedId;
use liq_state::{StoreConfig, UndoCapacity};
use liq_types::fixed::RAY;
use liq_types::{AssetId, MarketId, Price, PriceVector, Ray, SourceKind};

const PROTOCOL: ProtocolId = ProtocolId(3);
const POOL_MARKET: MarketId = MarketId(200);
const WETH: AssetId = AssetId(0);
const DAI: AssetId = AssetId(1);
const DEPLOY_BLOCK: u64 = 100;
const T0: u64 = 1_700_000_000;
const P8_TO_RAY: U256 = uint!(10_000_000_000_000_000_000_U256);
const RATE: U256 = uint!(20_000_000_000_000_000_000_000_000_U256);

struct Deploy {
    pool: Address,
    oracle: Address,
    configurator: Address,
    weth: Address,
    dai: Address,
    alice: Address,
    bob: Address,
}

fn d() -> Deploy {
    Deploy {
        pool: Address::repeat_byte(0x22),
        oracle: Address::repeat_byte(0x33),
        configurator: Address::repeat_byte(0x55),
        weth: Address::repeat_byte(0xa0),
        dai: Address::repeat_byte(0xa1),
        alice: Address::repeat_byte(0xc1),
        bob: Address::repeat_byte(0xc2),
    }
}

fn adapter(d: &Deploy) -> AaveV3 {
    let asset = |underlying, asset, feed| AssetConfig {
        underlying,
        asset,
        feed: FeedId(feed),
        siloed: false,
        isolated: false,
        debt_ceiling: 0,
        decimals: 18,
    };
    AaveV3::new(Config {
        protocol: PROTOCOL,
        pools: vec![PoolConfig {
            address: d.pool,
            market: POOL_MARKET,
            oracle: d.oracle,
            provider: Address::repeat_byte(0x44),
            configurator: d.configurator,
            sentinel: Address::ZERO,
            sequencer_oracle: Address::ZERO,
            tokens: Vec::new(),
            grace_sentinel: Address::ZERO,
        }],
        assets: vec![asset(d.weth, WETH, 1), asset(d.dai, DAI, 2)],
        price_sources: vec![
            SourcePin {
                pool: d.pool,
                underlying: d.weth,
                source: Address::repeat_byte(0xb0),
            },
            SourcePin {
                pool: d.pool,
                underlying: d.dai,
                source: Address::repeat_byte(0xb1),
            },
        ],
        liquidation: LiquidationParams {
            close_factor_bps: 5_000,
            close_factor_hf_wad: 950_000_000_000_000_000,
            min_base_max_close: 2000 * 100_000_000,
            oracle_decimals: 8,
            balance_model: BalanceModel::TokenMath35,
            close_factor_scope: CloseFactorScope::PositionBase,
            version: Default::default(),
        },
        pinned_through: DEPLOY_BLOCK,
    })
    .unwrap()
}

fn log<E: SolEvent>(address: Address, ev: &E) -> SimLog {
    SimLog {
        address,
        topics: ev.encode_topics().into_iter().map(|t| t.0).collect(),
        data: ev.encode_data().into(),
    }
}

fn collateral(asset: Address, ltv: u16, lt: u16) -> SimLog {
    log(
        d().configurator,
        &ccfg::CollateralConfigurationChanged {
            asset,
            ltv: U256::from(ltv),
            liquidationThreshold: U256::from(lt),
            liquidationBonus: U256::from(10_500_u16),
        },
    )
}

/// The adapter fixture's listing and activity, committed to a real store.
fn store(p: &AaveV3, d: &Deploy) -> StateStore {
    let mut st = StateStore::new(StoreConfig {
        base: DEPLOY_BLOCK - 1,
        positions: 8,
        markets: 4,
        undo: UndoCapacity {
            ops: 256,
            extras: 16,
            rows: 16,
        },
    });
    let reserve = |a: Address, v: Address, t: Address| {
        log(
            d.configurator,
            &ccfg::ReserveInitialized {
                asset: a,
                aToken: t,
                stableDebtToken: Address::ZERO,
                variableDebtToken: v,
                interestRateStrategyAddress: Address::repeat_byte(0xd1),
            },
        )
    };
    let index = |r: Address| {
        log(
            d.pool,
            &pool::ReserveDataUpdated {
                reserve: r,
                liquidityRate: RATE,
                stableBorrowRate: U256::ZERO,
                variableBorrowRate: RATE,
                liquidityIndex: RAY,
                variableBorrowIndex: RAY,
            },
        )
    };
    let listing = vec![
        reserve(
            d.weth,
            Address::repeat_byte(0xb4),
            Address::repeat_byte(0xb2),
        ),
        reserve(
            d.dai,
            Address::repeat_byte(0xb5),
            Address::repeat_byte(0xb3),
        ),
        collateral(d.weth, 80_50, 82_50),
        collateral(d.dai, 75_00, 77_00),
        log(
            d.oracle,
            &oracle::AssetSourceUpdated {
                asset: d.weth,
                source: Address::repeat_byte(0xb0),
            },
        ),
        log(
            d.oracle,
            &oracle::AssetSourceUpdated {
                asset: d.dai,
                source: Address::repeat_byte(0xb1),
            },
        ),
        index(d.weth),
        index(d.dai),
    ];
    let activity = vec![
        log(
            d.pool,
            &pool::Supply {
                reserve: d.weth,
                user: d.alice,
                onBehalfOf: d.alice,
                amount: uint!(1_000_000_000_000_000_000_U256),
                referralCode: 0,
            },
        ),
        log(
            d.pool,
            &pool::ReserveUsedAsCollateralEnabled {
                reserve: d.weth,
                user: d.alice,
            },
        ),
        log(
            d.pool,
            &pool::Supply {
                reserve: d.dai,
                user: d.bob,
                onBehalfOf: d.bob,
                amount: uint!(10_000_000_000_000_000_000_000_U256),
                referralCode: 0,
            },
        ),
        log(
            d.pool,
            &pool::Borrow {
                reserve: d.dai,
                user: d.alice,
                onBehalfOf: d.alice,
                amount: uint!(1_500_000_000_000_000_000_000_U256),
                interestRateMode: 2,
                borrowRate: RATE,
                referralCode: 0,
            },
        ),
    ];
    for (block, logs) in [(DEPLOY_BLOCK, listing), (DEPLOY_BLOCK + 1, activity)] {
        st.begin_block(block).unwrap();
        for l in &logs {
            let decoded = DecodedLog {
                address: l.address,
                topics: &l.topics,
                data: &l.data,
                block,
                timestamp: T0,
            };
            p.apply_log(&mut st, &decoded).unwrap();
        }
    }
    st
}

fn prices(weth_p8: u64) -> PriceVector {
    let px = |asset, p8: u64| Price {
        asset,
        price: Ray::from_raw(U256::from(p8) * P8_TO_RAY),
        source: SourceKind::Canonical,
        block: DEPLOY_BLOCK,
        ts: T0,
    };
    PriceVector(vec![px(WETH, weth_p8), px(DAI, 1_0000_0000)])
}

fn flash(d: &Deploy) -> FlashIndex {
    let reserve = |asset, underlying| AaveReserve {
        asset,
        underlying,
        atoken: Address::repeat_byte(0xee),
        balance: uint!(1_000_000_000_000_000_000_000_000_U256),
        flash_enabled: true,
        active: true,
        paused: false,
    };
    let src = AavePool::new(
        Address::repeat_byte(0x77),
        Address::repeat_byte(0x78),
        5,
        &[reserve(DAI, d.dai), reserve(WETH, d.weth)],
    );
    let sources: Vec<Box<dyn FlashSource>> = vec![Box::new(src)];
    let mut idx = FlashIndex::new(2);
    idx.refresh(&sources);
    idx
}

fn sim(logs: Vec<SimLog>) -> GovSim {
    GovSim {
        action: GovAction::Payload {
            controller: Address::repeat_byte(0x99),
            id: 1,
        },
        base_block: DEPLOY_BLOCK + 1,
        target_block: DEPLOY_BLOCK + 2,
        target_ts: T0 + SLOT_SECONDS,
        exec_gas: 0,
        logs,
    }
}

struct Rig {
    protocols: Vec<BoundProtocol>,
    routes: GovRoutes,
    store: StateStore,
    engine: Engine,
    flash: FlashIndex,
    d: Deploy,
}

fn rig(weth_p8: u64) -> Rig {
    let d = d();
    let p = adapter(&d);
    let store = store(&p, &d);
    let protocols = vec![BoundProtocol::AaveV3(p)];
    let routes = GovRoutes::new(&protocols);
    let mut engine = Engine::new(EngineConfig {
        assets: 2,
        positions: 8,
        queue: 64,
    });
    engine.load_prices(&prices(weth_p8)).unwrap();
    Rig {
        protocols,
        routes,
        store,
        engine,
        flash: flash(&d),
        d,
    }
}

fn run(r: &mut Rig, logs: Vec<SimLog>) -> Vec<Candidate> {
    let routes = DepthOnlyRouteCache(&r.flash);
    let (_, out) = newly_liquidatable(
        &mut r.engine,
        &r.protocols,
        &r.routes,
        &r.store,
        &sim(logs),
        &r.flash,
        &routes,
        Haircut::NONE,
        None,
    )
    .unwrap();
    out
}

fn alice(r: &Rig) -> PositionId {
    r.store
        .position_id(&liq_types::PositionKey {
            protocol: PROTOCOL,
            market: POOL_MARKET,
            user: r.d.alice,
        })
        .unwrap()
}

/// LT 82.5 % → 70 %: HF 1.1 → 0.933. Alice, and only Alice, comes out,
/// with `ParamChange` on her market; the controller's own log is skipped;
/// the committed store is untouched (she is healthy on it).
#[test]
fn an_lt_cut_makes_exactly_the_affected_account_liquidatable() {
    let mut r = rig(2000_0000_0000);
    let untracked = SimLog {
        address: Address::repeat_byte(0x99),
        topics: vec![B256::repeat_byte(0x01)],
        data: Bytes::new(),
    };
    let got = run(&mut r, vec![collateral(d().weth, 60_00, 70_00), untracked]);
    let ids: Vec<PositionId> = got.iter().map(|c| c.position).collect();
    assert_eq!(ids, vec![alice(&r)]);
    assert_eq!(
        got[0].cause,
        TriggerCause::ParamChange {
            market: POOL_MARKET
        }
    );
    // The adapter's own health at the changed parameters is below one.
    assert!(got[0].health.hf < Ray::ONE);
    // Nothing was committed: without the change she is still healthy.
    let pos = r.store.view(T0 + SLOT_SECONDS).position(alice(&r)).unwrap();
    let hf = r.protocols[0]
        .as_dyn()
        .health(pos, r.engine.prices())
        .unwrap()
        .hf;
    assert!(hf > Ray::ONE, "committed state unchanged");
}

/// LT 82.5 % → 80 %: HF 1.067, still healthy. Nothing comes out.
#[test]
fn a_cut_that_leaves_her_healthy_yields_nothing() {
    let mut r = rig(2000_0000_0000);
    assert!(run(&mut r, vec![collateral(d().weth, 78_00, 80_00)]).is_empty());
}

/// WETH at 1 500: HF 0.825 already. The cut changes nothing the ordinary
/// drain does not already have, so it is not a governance candidate.
#[test]
fn an_account_already_liquidatable_is_left_to_the_ordinary_drain() {
    let mut r = rig(1500_0000_0000);
    assert!(run(&mut r, vec![collateral(d().weth, 60_00, 70_00)]).is_empty());
}

/// A change to a reserve Alice does not hold as collateral (DAI) reaches
/// nobody who becomes liquidatable.
#[test]
fn a_change_to_an_unheld_collateral_yields_nothing() {
    let mut r = rig(2000_0000_0000);
    assert!(run(&mut r, vec![collateral(d().dai, 10_00, 11_00)]).is_empty());
}
