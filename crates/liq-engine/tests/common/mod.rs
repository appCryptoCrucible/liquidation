//! Engine fixtures: a real `StateStore` holding Aave V4 positions folded
//! through the real adapter, priced with real Chainlink answers and funded
//! from real Aave V3 flash depth.
//!
//! **Provenance.**
//! * **Chain observations** — read with `cast` against `https://eth.drpc.org`
//!   at block [`PIN_BLOCK`] (the pin 15A-2 was verified against), listed
//!   next to each constant with the exact call. Nothing here is rounded,
//!   assumed or adjusted.
//! * **Positions** — Aave V4 is not deployed on mainnet at the pin, so
//!   every position is synthetic: it is produced by folding logs of the
//!   shapes `Hub.sol`/`Spoke.sol` emit through the adapter's own
//!   `apply_log` (the adapter's published-rule fixture module, imported by
//!   path, supplies the deployment, listing logs and log shapes). Position
//!   *sizes* are test parameters chosen to land in each band; they are
//!   the only free variables and they are labelled as such.
//! * **Time** — the V4 deployment's own clock (`T0`, `DEPLOY_BLOCK`); it is a
//!   test deployment, not the pinned chain.

#![allow(
    dead_code,
    unused_imports,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::vec_init_then_push,
    clippy::inconsistent_digit_grouping
)]

#[path = "../../../liq-adapters/aave-v4/tests/common/mod.rs"]
pub mod v4;

use alloy_primitives::{address, uint, Address, U256};
use liq_adapters_aave_v4::events::{hub, spoke};
use liq_adapters_aave_v4::AaveV4;
use liq_engine::{Engine, EngineConfig, World};
use liq_flash::{AavePool, AaveReserve, DepthOnlyRouteCache, FlashIndex, FlashSource, Haircut};
use liq_protocol::{Constraints, Protocol};
use liq_state::{StateStore, StoreConfig, UndoCapacity};
use liq_types::fixed::RAY;
use liq_types::{AssetId, PositionId, Price, PriceVector, SourceKind};

pub use v4::{
    listing_logs, log, ray_of_p8, Deploy, OwnedLog, DAI, DEPLOY_BLOCK, HUB_MARKET, PROTOCOL,
    SPOKE_MARKET, T0, WETH, WETH_RATE,
};

// ── chain observations @ PIN_BLOCK ─────────────────────────────────────────

/// `cast block 26018679 --field timestamp` → `1789906763`.
pub const PIN_BLOCK: u64 = 26_018_679;
pub const PIN_TS: u64 = 1_789_906_763;

/// Chainlink ETH/USD `0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419`
/// `latestRoundData()` → answer `257588560000` (8 dec), `updatedAt`
/// `1789905719`, round `129127208515966895152`.
pub const ETH_USD_P8: u64 = 257_588_560_000;
/// Chainlink DAI/USD `0xAed0c38402a5d19df6E4c03F4E2DceD6e29c1ee9`
/// `latestRoundData()` → answer `99978334`, `updatedAt` `1789905539`.
pub const DAI_USD_P8: u64 = 99_978_334;
/// Chainlink USDC/USD `0x8fFfFfd4AfB6115b954Bd326cbe7B4BA576818f6`
/// `latestRoundData()` → answer `99984488`, `updatedAt` `1789891235`. A
/// third priced asset no fixture position holds: it exists so a block can
/// have three distinct movers (the correlated-sweep trigger).
pub const USDC_USD_P8: u64 = 99_984_488;
pub const USDC: AssetId = AssetId(2);
pub const ASSETS: usize = 3;

/// Aave V3 Pool `0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2`
/// `FLASHLOAN_PREMIUM_TOTAL()` → `5`.
pub const AAVE_V3_FLASH_PREMIUM_BPS: u16 = 5;
pub const AAVE_V3_POOL: Address = address!("87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");
/// `PoolAddressesProvider.getPoolConfigurator()`.
pub const AAVE_V3_CONFIGURATOR: Address = address!("64b761D848206f447Fe2dd461b0c635Ec39EbB27");
pub const DAI_TOKEN: Address = address!("6B175474E89094C44Da98b954EedeAC495271d0F");
pub const WETH_TOKEN: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
/// `Pool.getReserveAToken(DAI)` / `(WETH)`.
pub const A_DAI: Address = address!("018008bfb33d285247A21d44E50697654f754e63");
pub const A_WETH: Address = address!("4d5F47FA6A74757f35C14fD3a6Ef8E3C9BC514E8");
/// `DAI.balanceOf(aDAI)` → `16637911088295614339191877`.
pub const AAVE_V3_DAI_LIQUIDITY: U256 = uint!(16_637_911_088_295_614_339_191_877_U256);
/// `WETH.balanceOf(aWETH)` → `294708368764155465531255`.
pub const AAVE_V3_WETH_LIQUIDITY: U256 = uint!(294_708_368_764_155_465_531_255_U256);
/// `Pool.getConfiguration(DAI)` = `0x1000…e140000`, `(WETH)` =
/// `0x1000…6c1f72`: bit 56 active = 1, bit 57 frozen = 0, bit 60 paused =
/// 0, bit 63 flashloan-enabled = 1 for both.
pub const AAVE_V3_DAI_WETH_FLASH_ENABLED: bool = true;

/// Real Chainlink answers at the pin as the adapter's RAY vector, slot ==
/// global asset id (WETH 0, DAI 1).
pub fn pinned_prices() -> PriceVector {
    let at = |asset: AssetId, p8: u64| Price {
        asset,
        price: ray_of_p8(p8),
        source: SourceKind::Canonical,
        block: PIN_BLOCK,
        ts: PIN_TS,
    };
    PriceVector(vec![
        at(WETH, ETH_USD_P8),
        at(DAI, DAI_USD_P8),
        at(USDC, USDC_USD_P8),
    ])
}

/// Aave V3 as the flash arena, seeded with the pinned depth.
pub fn pinned_flash() -> FlashIndex {
    let pool = AavePool::new(
        AAVE_V3_POOL,
        AAVE_V3_CONFIGURATOR,
        AAVE_V3_FLASH_PREMIUM_BPS,
        &[
            AaveReserve {
                asset: DAI,
                underlying: DAI_TOKEN,
                atoken: A_DAI,
                balance: AAVE_V3_DAI_LIQUIDITY,
                flash_enabled: AAVE_V3_DAI_WETH_FLASH_ENABLED,
                active: true,
                paused: false,
            },
            AaveReserve {
                asset: WETH,
                underlying: WETH_TOKEN,
                atoken: A_WETH,
                balance: AAVE_V3_WETH_LIQUIDITY,
                flash_enabled: AAVE_V3_DAI_WETH_FLASH_ENABLED,
                active: true,
                paused: false,
            },
        ],
    );
    let sources: Vec<Box<dyn FlashSource>> = vec![Box::new(pool)];
    let mut idx = FlashIndex::new(ASSETS);
    idx.refresh(&sources);
    idx
}

/// An empty flash index: nothing is fundable.
pub fn no_flash() -> FlashIndex {
    FlashIndex::new(ASSETS)
}

// ── synthetic positions ────────────────────────────────────────────────────

/// One borrower: `weth` supplied as collateral, `dai` drawn, at a 2 % risk
/// premium when `premium` (the adapter fixture's Alice, parameterised).
/// `dai == 0` is a supplier only.
#[derive(Copy, Clone, Debug)]
pub struct Borrower {
    pub user: Address,
    pub weth: U256,
    pub dai: U256,
    pub premium: bool,
}

pub const ONE: U256 = uint!(1_000_000_000_000_000_000_U256);

pub fn user(i: u32) -> Address {
    let mut b = [0u8; 20];
    b[0] = 0xc0;
    b[16..20].copy_from_slice(&(i + 1).to_be_bytes());
    Address::from(b)
}

/// `n` borrowers, 1 WETH each, DAI debt `dai_of(i)` (whole DAI).
pub fn borrowers(n: u32, dai_of: impl Fn(u32) -> u64) -> Vec<Borrower> {
    (0..n)
        .map(|i| Borrower {
            user: user(i),
            weth: ONE,
            dai: U256::from(dai_of(i)) * ONE,
            premium: true,
        })
        .collect()
}

/// Risk-premium shares for a 2 % premium: `percentMulUp(debt, 200)`.
fn premium_shares(debt: U256) -> U256 {
    (debt * U256::from(200u64) + U256::from(5_000u64)) / U256::from(10_000u64)
}

/// Activity at `DEPLOY_BLOCK + 1`/`T0`, in the chain's emission order:
/// every borrower supplies WETH and enables it as collateral; a lender
/// supplies twice the aggregate DAI; every borrower with debt draws it and
/// is assigned the 2 % premium. Borrower `i` interns as `PositionId(i)`,
/// the lender as `PositionId(n)`.
pub fn activity_logs(d: &Deploy, bs: &[Borrower]) -> Vec<OwnedLog> {
    let (b, t) = (DEPLOY_BLOCK + 1, T0);
    let asset = |id: u8| U256::from(id);
    let mut v = Vec::with_capacity(bs.len() * 12 + 4);
    let update = |v: &mut Vec<OwnedLog>, id: u8, rate: U256| {
        v.push(log(
            d.hub,
            &hub::UpdateAsset {
                assetId: asset(id),
                drawnIndex: RAY,
                drawnRate: rate,
                accruedFees: U256::ZERO,
            },
            b,
            t,
        ));
    };
    for br in bs {
        update(&mut v, 0, WETH_RATE);
        v.push(log(
            d.hub,
            &hub::Add {
                assetId: asset(0),
                spoke: d.spoke,
                shares: br.weth,
                amount: br.weth,
            },
            b,
            t,
        ));
        v.push(log(
            d.spoke,
            &spoke::Supply {
                reserveId: asset(0),
                caller: br.user,
                user: br.user,
                suppliedShares: br.weth,
                suppliedAmount: br.weth,
            },
            b,
            t,
        ));
        v.push(log(
            d.spoke,
            &spoke::RefreshSingleUserDynamicConfig {
                user: br.user,
                reserveId: asset(0),
            },
            b,
            t,
        ));
        v.push(log(
            d.spoke,
            &spoke::SetUsingAsCollateral {
                reserveId: asset(0),
                caller: br.user,
                user: br.user,
                usingAsCollateral: true,
            },
            b,
            t,
        ));
    }
    let total: U256 = bs.iter().map(|br| br.dai).fold(U256::ZERO, |a, x| a + x);
    let lender_dai = total * U256::from(2u64) + ONE;
    update(&mut v, 1, v4::DAI_RATE);
    v.push(log(
        d.hub,
        &hub::Add {
            assetId: asset(1),
            spoke: d.spoke,
            shares: lender_dai,
            amount: lender_dai,
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
            suppliedShares: lender_dai,
            suppliedAmount: lender_dai,
        },
        b,
        t,
    ));
    for br in bs.iter().filter(|br| !br.dai.is_zero()) {
        update(&mut v, 1, v4::DAI_RATE);
        v.push(log(
            d.hub,
            &hub::Draw {
                assetId: asset(1),
                spoke: d.spoke,
                drawnShares: br.dai,
                drawnAmount: br.dai,
            },
            b,
            t,
        ));
        v.push(log(
            d.spoke,
            &spoke::Borrow {
                reserveId: asset(1),
                caller: br.user,
                user: br.user,
                drawnShares: br.dai,
                drawnAmount: br.dai,
            },
            b,
            t,
        ));
        v.push(log(
            d.spoke,
            &spoke::RefreshAllUserDynamicConfig { user: br.user },
            b,
            t,
        ));
        if !br.premium {
            continue;
        }
        v.push(log(
            d.spoke,
            &spoke::UpdateUserRiskPremium {
                user: br.user,
                riskPremium: U256::from(v4::ALICE_RISK_PREMIUM_BPS),
            },
            b,
            t,
        ));
        let shares = premium_shares(br.dai);
        let offset = shares * RAY;
        let delta = |s: U256, o: U256| hub::PremiumDelta {
            sharesDelta: alloy_primitives::I256::try_from(s).unwrap(),
            offsetRayDelta: alloy_primitives::I256::try_from(o).unwrap(),
            restoredPremiumRay: U256::ZERO,
        };
        v.push(log(
            d.hub,
            &hub::RefreshPremium {
                assetId: asset(1),
                spoke: d.spoke,
                premiumDelta: delta(shares, offset),
            },
            b,
            t,
        ));
        v.push(log(
            d.spoke,
            &spoke::RefreshPremiumDebt {
                reserveId: asset(1),
                user: br.user,
                premiumDelta: spoke::PremiumDelta {
                    sharesDelta: alloy_primitives::I256::try_from(shares).unwrap(),
                    offsetRayDelta: alloy_primitives::I256::try_from(offset).unwrap(),
                    restoredPremiumRay: U256::ZERO,
                },
            },
            b,
            t,
        ));
    }
    v
}

/// A real `StateStore` with the listing and `bs`'s activity folded through
/// the adapter, blocks journaled as the node would.
pub fn store(p: &AaveV4, d: &Deploy, bs: &[Borrower]) -> StateStore {
    let mut st = StateStore::new(StoreConfig {
        base: DEPLOY_BLOCK - 1,
        positions: bs.len() + 1,
        markets: 4,
        undo: UndoCapacity {
            ops: 64,
            extras: 8,
            rows: 8,
        },
    });
    st.begin_block(DEPLOY_BLOCK).unwrap();
    for l in listing_logs(d) {
        p.apply_log(&mut st, &l.view()).unwrap();
    }
    st.begin_block(DEPLOY_BLOCK + 1).unwrap();
    for (i, l) in activity_logs(d, bs).iter().enumerate() {
        p.apply_log(&mut st, &l.view())
            .unwrap_or_else(|e| panic!("activity log #{i} refused: {e:?}"));
    }
    st
}

/// Everything one engine test needs, owned.
pub struct Rig {
    pub d: Deploy,
    pub p: AaveV4,
    pub st: StateStore,
    pub flash: FlashIndex,
    pub px: PriceVector,
    pub engine: Engine,
}

impl Rig {
    pub fn new(bs: &[Borrower], flash: FlashIndex, queue: usize) -> Self {
        let d = Deploy::new();
        let p = d.adapter();
        let st = store(&p, &d, bs);
        let px = pinned_prices();
        let mut engine = Engine::new(EngineConfig {
            assets: ASSETS,
            positions: bs.len() + 1,
            queue,
        });
        engine.load_prices(&px).unwrap();
        Self {
            d,
            p,
            st,
            flash,
            px,
            engine,
        }
    }

    /// Fold the whole universe at `ts` on canonical state.
    pub fn resync(&mut self, ts: u64) {
        let protocols: [&dyn Protocol; 1] = [&self.p];
        let routes = DepthOnlyRouteCache(&self.flash);
        let w = World {
            view: self.st.view(ts),
            protocols: &protocols,
            flash: &self.flash,
            routes: &routes,
            haircut: Haircut::NONE,
            cons: &Constraints::UNBOUNDED,
        };
        self.engine.resync(&w).unwrap();
    }

    /// Run `f` with a `World` at `ts`.
    pub fn with_world<R>(&mut self, ts: u64, f: impl FnOnce(&mut Engine, &World<'_>) -> R) -> R {
        let protocols: [&dyn Protocol; 1] = [&self.p];
        let routes = DepthOnlyRouteCache(&self.flash);
        let w = World {
            view: self.st.view(ts),
            protocols: &protocols,
            flash: &self.flash,
            routes: &routes,
            haircut: Haircut::NONE,
            cons: &Constraints::UNBOUNDED,
        };
        f(&mut self.engine, &w)
    }

    pub fn tick(&self, asset: AssetId, p8: u64, source: SourceKind) -> Price {
        Price {
            asset,
            price: ray_of_p8(p8),
            source,
            block: PIN_BLOCK,
            ts: PIN_TS,
        }
    }

    /// The pinned vector with `asset` at `p8`.
    pub fn px_with(&self, asset: AssetId, p8: u64) -> PriceVector {
        let mut px = pinned_prices();
        px.0[usize::from(asset.0)].price = ray_of_p8(p8);
        px
    }

    /// Independent oracle: every position liquidatable **and** fundable at
    /// `px`/`ts` per the adapter and `liq_flash::is_eligible` called
    /// directly — exactly what the engine must emit, no more, no less.
    pub fn expected(&self, ts: u64, px: &PriceVector) -> Vec<PositionId> {
        let view = self.st.view(ts);
        let routes = DepthOnlyRouteCache(&self.flash);
        (0..view.len() as u32)
            .map(PositionId)
            .filter(|&id| {
                let pos = view.position(id).unwrap();
                let h = self.p.health(pos, px).unwrap();
                if h.hf >= liq_types::Ray::ONE {
                    return false;
                }
                match self.p.quote(pos, px, &Constraints::UNBOUNDED).unwrap() {
                    Some(q) => {
                        liq_flash::is_eligible(&q, &self.flash, &routes, Haircut::NONE).is_some()
                    }
                    None => false,
                }
            })
            .collect()
    }

    /// Adapter health factor of `id` at `px`/`ts`.
    pub fn hf(&self, id: PositionId, ts: u64, px: &PriceVector) -> liq_types::Ray {
        self.p
            .health(self.st.view(ts).position(id).unwrap(), px)
            .unwrap()
            .hf
    }
}

/// Sorted position ids of drained candidates.
pub fn ids(cands: impl Iterator<Item = liq_engine::Candidate>) -> Vec<PositionId> {
    let mut v: Vec<PositionId> = cands.map(|c| c.position).collect();
    v.sort_unstable();
    v
}

pub fn pid(i: u32) -> PositionId {
    PositionId(i)
}
