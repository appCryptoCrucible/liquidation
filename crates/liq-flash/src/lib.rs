//! Flash-loan sources, index, eligibility, selection, cascade (GUIDE 07).
//!
//! 07A: five arenas behind [`FlashSource`] (`sources/`). 07B: the
//! per-asset [`FlashIndex`] rebuilt from those sources after each block's
//! logs (`index`), the two-sided eligibility filter with its `Unfundable`
//! bitset and the interim [`DepthOnlyRouteCache`] (`eligibility`),
//! effective-cost ranking with a fallback chain (`select`), and the ≤ 3
//! sibling-group cascade planner (`cascade`).
//!
//! **Deviation from GUIDE-07 §1.** `apply_log` takes `&mut self`, not `&self`.
//! The ExEx ingest thread is the sole writer (02A/03A, RUST-CONVENTIONS §1).
//! `&self` would force `RefCell` (not `Sync`) or a `Mutex` (a lock on the
//! ingest path). Atomics cannot hold a tearing-free `U256`. Mutation through
//! `&mut self` is the borrow-checker proof, matching `StateStore`. Off-thread
//! readers consume 07B's `ArcSwap<FlashIndex>` snapshot, not a live source.
//!
//! `subscriptions()` is inherited from [`LogSubscriber`] (D46 / 03A router)
//! rather than redeclared. Same signature.
//!
//! `source()` is extra: [`liq_protocol::FlashRoute`] needs the flash-source
//! address and `Box<dyn FlashSource>` cannot downcast.

#![deny(clippy::todo, clippy::unimplemented)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use alloy_primitives::{Address, U256};
use liq_protocol::{CallbackShape, DecodedLog};
use liq_types::{AssetId, FlashProvider, LogSubscriber};

pub mod cascade;
pub mod eligibility;
pub mod index;
pub mod select;
pub mod sources;

pub use cascade::{plan as plan_cascade, planable, Cascade, MAX_GROUPS};
pub use eligibility::{is_eligible, DepthOnlyRouteCache, Eligibility};
pub use index::{FlashIndex, Haircut, SourceEntry};
pub use select::{effective_cost, fallback_chain, fee_amount, CostModel};
pub use sources::{
    AavePool, AaveReserve, HeldAsset, MorphoBlue, SkyDssFlash, UniV3Pool, UniV4PoolManager,
};

/// Default wrapping gas when a source is constructed without a 10C snapshot.
/// Production bind applies `config/liq-gas.toml` `[wrap]` via `with_overhead`.
pub const GAS_OVERHEAD_STUB: u64 = 0;

/// One flash-loan venue. Built once at startup; `available` is the hot path.
pub trait FlashSource: LogSubscriber + Send + Sync + 'static {
    fn provider(&self) -> FlashProvider;
    /// Contract the flash is drawn from (`FlashRoute.source`).
    fn source(&self) -> Address;
    /// Max borrowable of `asset` right now. Allocation-free. Missing → `0`.
    fn available(&self, asset: AssetId) -> U256;
    fn fee_bps(&self, asset: AssetId, amount: U256) -> u16;
    fn callback(&self) -> CallbackShape;
    /// Wrapping gas from the 10C snapshot when bound; else [`GAS_OVERHEAD_STUB`].
    fn gas_overhead(&self) -> u64;
    /// Fold one routed log. Ingest thread only.
    fn apply_log(&mut self, log: &DecodedLog<'_>);
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use alloy_primitives::U256;
    use liq_protocol::CallbackShape;
    use liq_types::FlashProvider;

    use crate::sources::fixtures::*;
    use crate::sources::{
        AavePool, AaveReserve, HeldAsset, MorphoBlue, SkyDssFlash, UniV3Pool, UniV4PoolManager,
    };
    use crate::{FlashSource, GAS_OVERHEAD_STUB};

    fn aave() -> AavePool {
        AavePool::new(
            AAVE_POOL,
            AAVE_CONFIGURATOR,
            5,
            &[AaveReserve {
                asset: ID_USDC,
                underlying: USDC,
                atoken: A_USDC,
                balance: U256::from(AUSDC_USDC_26M),
                flash_enabled: true,
                active: true,
                paused: false,
            }],
        )
    }

    fn five() -> [Box<dyn FlashSource>; 5] {
        [
            Box::new(aave()),
            Box::new(UniV3Pool::new(
                UNIV3_USDC_WETH_500,
                USDC,
                WETH,
                ID_USDC,
                ID_WETH,
                500,
                U256::from(V3_500_USDC_26M),
                u256(V3_500_WETH_26M),
            )),
            Box::new(UniV4PoolManager::new(
                POOL_MANAGER,
                &[HeldAsset {
                    asset: ID_USDC,
                    token: USDC,
                    balance: U256::from(PM_USDC_26M),
                }],
            )),
            Box::new(MorphoBlue::new(
                MORPHO,
                &[HeldAsset {
                    asset: ID_USDC,
                    token: USDC,
                    balance: U256::from(MORPHO_USDC_26M),
                }],
            )),
            Box::new(SkyDssFlash::new(
                DSS_FLASH,
                END,
                ID_DAI,
                u256(DSS_MAX_26M),
                U256::ZERO,
                true,
            )),
        ]
    }

    #[test]
    fn five_arenas_no_balancer() {
        let srcs = five();
        assert_eq!(srcs.len(), 5, "oracle: GUIDE-07 §3 — five arenas");
        let providers: Vec<_> = srcs.iter().map(|s| s.provider()).collect();
        assert_eq!(
            providers,
            [
                FlashProvider::Aave,
                FlashProvider::UniV3,
                FlashProvider::UniV4,
                FlashProvider::Morpho,
                FlashProvider::SkyDss,
            ]
        );
        for s in &srcs {
            assert_eq!(s.callback().provider(), s.provider());
            assert_eq!(s.gas_overhead(), GAS_OVERHEAD_STUB);
            match s.callback() {
                CallbackShape::AaveExecuteOperation
                | CallbackShape::UniV3FlashCallback
                | CallbackShape::UniV4UnlockCallback
                | CallbackShape::MorphoFlashCallback
                | CallbackShape::SkyDssOnFlashLoan => {}
            }
        }
        assert_eq!(srcs[4].available(ID_USDC), U256::ZERO);
        assert_eq!(srcs[4].available(ID_DAI), u256(DSS_MAX_26M));
    }

    #[test]
    fn log_driven_never_calls_panic_rpc() {
        let mut rpc = PanicLogSource;
        let mut srcs = five();
        let amt = U256::from(1u64);
        let topics = transfer_topics(alloy_primitives::Address::ZERO, A_USDC);
        let data = u256_be(amt);
        let lg = log(USDC, &topics, &data);
        for s in &mut srcs {
            s.apply_log(&lg);
            let _ = s.available(ID_USDC);
            let _ = s.subscriptions();
        }
        let _keep_poll: fn(&mut PanicLogSource) -> ! = PanicLogSource::poll_block;
        let _keep_call: fn(&PanicLogSource, alloy_primitives::Address, &[u8]) -> ! =
            PanicLogSource::eth_call;
        let _keep_logs: fn(&PanicLogSource) -> ! = PanicLogSource::eth_get_logs;
        let _ = &mut rpc;
    }

    #[test]
    fn empty_log_is_noop() {
        let mut a = aave();
        let before = a.available(ID_USDC);
        a.apply_log(&log(AAVE_POOL, &[], &[]));
        assert_eq!(a.available(ID_USDC), before);
    }
}
