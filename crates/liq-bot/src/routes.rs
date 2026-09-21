//! 12A-1 wiring: [`WarmRouteCache`] is what eligibility sees (not 07B DepthOnly).
//! Warm builder thread, V3 tick L check, Curve reseed off the hot path.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{Builder, JoinHandle};
use std::time::Duration;

use alloy_primitives::{Address, U256};
use liq_flash::{is_eligible, FlashIndex, Haircut};
use liq_protocol::{FlashRoute, LegChoice, Quote};
use liq_router::{
    CurveState, Pool, PoolBook, PoolState, RouteError, V3State, WarmBuilder, WarmConfig,
    WarmInputs, WarmRouteCache,
};
use liq_types::AssetId;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SeedError {
    #[error("V3 ticks cannot explain L (empty ticks with L>0 or sum(net) != liquidity)")]
    TicksDoNotExplainL,
    #[error("V3 liquidity net overflow")]
    LiquidityOverflow,
    #[error(transparent)]
    Route(#[from] RouteError),
    #[error("curve reseed source refused: {0}")]
    CurveSource(String),
}

/// Wired eligibility: always [`WarmRouteCache`], never DepthOnly at this layer.
#[must_use]
pub fn evaluate_eligible(
    q: &Quote,
    idx: &FlashIndex,
    routes: &WarmRouteCache,
    haircut: Haircut,
) -> Option<(LegChoice, FlashRoute)> {
    is_eligible(q, idx, routes, haircut)
}

/// Off-hot-path warm rebuild. Never HTTP.
pub fn rebuild_warm(builder: &mut WarmBuilder, book: &PoolBook, inputs: &dyn WarmInputs) {
    let _ = builder.rebuild(book, inputs);
}

/// Missing ladder / price → pair skipped (already). Empty book → empty table.
pub struct AbsentWarmInputs;

impl WarmInputs for AbsentWarmInputs {
    fn bucket_sizes(&self, _: AssetId) -> Option<smallvec::SmallVec<[U256; 4]>> {
        None
    }
    fn per_eth(&self, _: AssetId) -> Option<U256> {
        None
    }
    fn next_base_fee(&self) -> u128 {
        0
    }
    fn block(&self) -> u64 {
        0
    }
}

/// One builder + the slot eligibility reads. Wiring layer only (not 07B).
#[must_use]
pub fn warm_handles() -> (WarmBuilder, WarmRouteCache) {
    let builder = WarmBuilder::new(WarmConfig {
        max_impact_bps: 100,
        twa_blocks: 1,
        budget: liq_router::SolveBudget::default(),
    });
    let cache = WarmRouteCache::new(builder.slot());
    (builder, cache)
}

/// Supervision thread. Empty `PoolBook` / [`AbsentWarmInputs`] publish empty.
pub fn spawn_warm_thread(
    mut builder: WarmBuilder,
    stop: Arc<AtomicBool>,
) -> Result<JoinHandle<()>, std::io::Error> {
    Builder::new().name("liq-bot-warm".into()).spawn(move || {
        let book = PoolBook::new(HashMap::new(), None, 0);
        let inputs = AbsentWarmInputs;
        rebuild_warm(&mut builder, &book, &inputs);
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(1));
            if stop.load(Ordering::Relaxed) {
                break;
            }
            rebuild_warm(&mut builder, &book, &inputs);
        }
    })
}

/// Uniswap V3: `L = sum(net)` over initialized ticks with `tick <= current`.
/// Empty ticks + L>0 fails. Mismatch fails. No invented L.
pub fn v3_ticks_explain_l(s: &V3State) -> Result<(), SeedError> {
    if s.ticks.is_empty() {
        if s.liquidity == 0 {
            return Ok(());
        }
        tracing::error!(tick = s.tick, L = s.liquidity, "V3 ticks empty but L > 0");
        return Err(SeedError::TicksDoNotExplainL);
    }
    let mut acc: i128 = 0;
    for t in &s.ticks {
        if t.tick <= s.tick {
            acc = acc.checked_add(t.net).ok_or(SeedError::LiquidityOverflow)?;
        }
    }
    if acc < 0 {
        tracing::error!(acc, L = s.liquidity, "V3 tick net sum negative");
        return Err(SeedError::TicksDoNotExplainL);
    }
    let explained = u128::try_from(acc).map_err(|_| SeedError::LiquidityOverflow)?;
    if explained != s.liquidity {
        tracing::error!(
            explained,
            L = s.liquidity,
            tick = s.tick,
            n_ticks = s.ticks.len(),
            "V3 ticks do not explain L"
        );
        return Err(SeedError::TicksDoNotExplainL);
    }
    Ok(())
}

/// Balances/A/fee at a block. Missing → leave stale / fail closed at seed.
pub trait CurveBalanceSource {
    fn snapshot(&self, pool: Address) -> Result<CurveSeed, SeedError>;
}

#[derive(Clone, Debug)]
pub struct CurveSeed {
    pub balances: Vec<U256>,
    pub a: U256,
    pub fee: U256,
}

/// Off-hot-path reseed. TokenExchange already marked stale on the log path.
pub fn reseed_curve(pool: &mut Pool, src: &dyn CurveBalanceSource) -> Result<(), SeedError> {
    let PoolState::Curve(c) = &mut pool.state else {
        return Ok(());
    };
    let snap = src.snapshot(pool.address)?;
    c.reseed(&snap.balances, snap.a, snap.fee)?;
    Ok(())
}

/// Fail closed at startup if any Curve pool is still stale after reseed attempt.
pub fn require_curve_fresh(pools: &[Pool]) -> Result<(), SeedError> {
    for p in pools {
        if let PoolState::Curve(CurveState { stale: true, .. }) = &p.state {
            tracing::error!(pool = ?p.address, "Curve pool stale after reseed");
            return Err(SeedError::CurveSource(format!("stale {}", p.address)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use arc_swap::ArcSwap;
    use liq_flash::{FlashSource, HeldAsset, MorphoBlue};
    use liq_protocol::{BonusCurve, RouteCache, SeizeOption};
    use liq_router::{Tick, WarmConfig};
    use liq_types::fixed::RAY;
    use liq_types::{AssetId, MarketId, PositionId, PositionKey, ProtocolId, Ray};
    use smallvec::SmallVec;
    use std::sync::Arc;

    fn e18(n: u64) -> U256 {
        U256::from(n)
            .checked_mul(U256::from(10u64).pow(U256::from(18u64)))
            .unwrap()
    }

    #[test]
    fn v3_ticks_must_explain_l() {
        let mut s = V3State {
            sqrt_price_x96: U256::from(1u64),
            tick: 0,
            liquidity: 1_000,
            fee_pips: 500,
            tick_spacing: 10,
            ticks: vec![Tick {
                tick: -10,
                net: 1_000,
                gross: 1_000,
            }],
        };
        v3_ticks_explain_l(&s).unwrap();
        s.liquidity = 2_000;
        assert!(matches!(
            v3_ticks_explain_l(&s),
            Err(SeedError::TicksDoNotExplainL)
        ));
        s.ticks.clear();
        s.liquidity = 1;
        assert!(matches!(
            v3_ticks_explain_l(&s),
            Err(SeedError::TicksDoNotExplainL)
        ));
        s.liquidity = 0;
        v3_ticks_explain_l(&s).unwrap();
    }

    #[test]
    fn wired_eligibility_uses_warm_not_depth_only() {
        let coll = AssetId(0);
        let debt = AssetId(1);
        let srcs: Vec<Box<dyn FlashSource>> = vec![Box::new(MorphoBlue::new(
            alloy_primitives::Address::repeat_byte(0xA0),
            &[
                HeldAsset {
                    asset: coll,
                    token: alloy_primitives::Address::repeat_byte(0xC0),
                    balance: e18(10_000_000),
                },
                HeldAsset {
                    asset: debt,
                    token: alloy_primitives::Address::repeat_byte(0xD0),
                    balance: e18(10_000_000),
                },
            ],
        ))];
        let mut idx = FlashIndex::new(4);
        idx.refresh(&srcs);
        let depth = liq_flash::DepthOnlyRouteCache(&idx);
        assert!(
            depth.has_exit(coll, e18(1)),
            "DepthOnly would admit from inventory"
        );
        let builder = WarmBuilder::new(WarmConfig {
            max_impact_bps: 100,
            twa_blocks: 1,
            budget: liq_router::SolveBudget::default(),
        });
        let warm = WarmRouteCache::new(builder.slot());
        assert!(
            !warm.has_exit(coll, e18(1)),
            "empty WarmRouteCache has no exit"
        );
        let q = Quote {
            position: PositionId(1),
            key: PositionKey {
                protocol: ProtocolId(0),
                market: MarketId(0),
                user: alloy_primitives::Address::repeat_byte(0xB0),
            },
            repay_options: SmallVec::from_slice(&[liq_protocol::RepayOption {
                asset: debt,
                max_repay: e18(1),
            }]),
            seize_options: SmallVec::from_slice(&[SeizeOption {
                asset: coll,
                max_seize: e18(1),
                bonus: Ray::from_raw(RAY / U256::from(20u64)),
                curve: BonusCurve::Static {
                    bonus: Ray::from_raw(RAY / U256::from(20u64)),
                },
            }]),
        };
        let h = Haircut::from_bps(10_000).unwrap();
        assert!(
            evaluate_eligible(&q, &idx, &warm, h).is_none(),
            "wired graph must see WarmRouteCache (no exit), not DepthOnly"
        );
        let _ = Arc::new(ArcSwap::from_pointee(idx));
    }

    #[test]
    fn empty_book_and_absent_inputs_publish_empty() {
        let (mut builder, cache) = warm_handles();
        let book = PoolBook::new(std::collections::HashMap::new(), None, 0);
        rebuild_warm(&mut builder, &book, &AbsentWarmInputs);
        assert!(
            cache.table().is_empty(),
            "empty PoolBook must not invent pools"
        );
        assert!(
            !cache.has_exit(AssetId(0), e18(1)),
            "empty publish is no exit, not a guessed route"
        );
    }

    struct MissingCurve;
    impl CurveBalanceSource for MissingCurve {
        fn snapshot(&self, pool: Address) -> Result<CurveSeed, SeedError> {
            Err(SeedError::CurveSource(format!("no rpc {pool}")))
        }
    }

    #[test]
    fn curve_reseed_fails_closed_without_source() {
        let mut p = Pool {
            address: alloy_primitives::Address::repeat_byte(0x42),
            assets: SmallVec::from_slice(&[AssetId(0), AssetId(1)]),
            tokens: SmallVec::from_slice(&[
                alloy_primitives::Address::repeat_byte(1),
                alloy_primitives::Address::repeat_byte(2),
            ]),
            hop_gas: 80_000,
            state: PoolState::Curve(CurveState {
                balances: SmallVec::from_slice(&[e18(1), e18(1)]),
                rates: SmallVec::from_slice(&[e18(1), e18(1)]),
                a: U256::from(2000u64),
                a_precision: U256::from(100u64),
                fee: U256::from(4_000_000u64),
                stale: true,
            }),
        };
        assert!(reseed_curve(&mut p, &MissingCurve).is_err());
        assert!(require_curve_fresh(&[p]).is_err());
    }
}
