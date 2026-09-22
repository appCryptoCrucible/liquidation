//! Process-level [`Shared`]: built once, leaked (`Box::leak`). Never `LazyLock`.

use std::sync::Arc;

use arc_swap::ArcSwap;
use liq_exec::submit::SubmitEnabled;
use liq_flash::FlashIndex;
use liq_risk::RiskGate;
use liq_router::WarmRouteCache;
use liq_state::StoreSnapshot;

use crate::lease::SubmitLease;

/// Cold-restart p99 on this box. Not measured here — ABSENT (do not invent).
pub const COLD_RESTART_P99: Option<core::time::Duration> = None;

/// Production pin policy. Tests may pass `true` into [`crate::exex_install::install_hot`].
pub const PROD_ALLOW_UNPINNED: bool = false;

/// Leaked process graph. Field offsets after startup; no `LazyLock`.
pub struct Shared {
    pub state: &'static liq_state::Shared,
    pub routes: WarmRouteCache,
    pub flash: Arc<ArcSwap<FlashIndex>>,
    pub submit_enabled: Arc<SubmitEnabled>,
    pub risk: &'static RiskGate,
    pub lease: &'static SubmitLease,
}

impl Shared {
    /// Startup only. The allocation lives for the process.
    #[must_use]
    pub fn leak(
        state: &'static liq_state::Shared,
        routes: WarmRouteCache,
        flash: Arc<ArcSwap<FlashIndex>>,
        submit_enabled: Arc<SubmitEnabled>,
        risk: &'static RiskGate,
        lease: &'static SubmitLease,
    ) -> &'static Self {
        Box::leak(Box::new(Self {
            state,
            routes,
            flash,
            submit_enabled,
            risk,
            lease,
        }))
    }
}

/// Leak an empty [`liq_state::Shared`] (16A pattern).
#[must_use]
pub fn leak_state_shared() -> &'static liq_state::Shared {
    liq_state::Shared::leak(StoreSnapshot::empty())
}

/// Leak a [`RiskGate`].
#[must_use]
pub fn leak_risk() -> &'static RiskGate {
    Box::leak(Box::new(RiskGate::new()))
}

/// Leak a [`SubmitLease`].
#[must_use]
pub fn leak_lease(lease: SubmitLease) -> &'static SubmitLease {
    Box::leak(Box::new(lease))
}

#[cfg(test)]
mod tests {
    use super::*;
    use liq_router::{WarmBuilder, WarmConfig};

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn leak_is_not_lazylock_and_submit_defaults_false() {
        assert!(COLD_RESTART_P99.is_none(), "cold-restart p99 ABSENT");
        assert!(!PROD_ALLOW_UNPINNED);
        let flag = Arc::new(SubmitEnabled::new(false));
        assert!(!flag.get());
        let builder = WarmBuilder::new(WarmConfig {
            max_impact_bps: 100,
            twa_blocks: 1,
            budget: liq_router::SolveBudget::default(),
        });
        let routes = WarmRouteCache::new(builder.slot());
        let flash = Arc::new(ArcSwap::from_pointee(FlashIndex::new(4)));
        let s = Shared::leak(
            leak_state_shared(),
            routes,
            flash,
            Arc::clone(&flag),
            leak_risk(),
            leak_lease(SubmitLease::refused()),
        );
        assert!(!s.submit_enabled.get());
        assert!(!s.lease.held());
    }
}
