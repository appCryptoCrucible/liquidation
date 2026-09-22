//! Position / concurrency caps + flash concentration / haircut auto-tune (GUIDE 14 §3–§4).

use std::collections::HashMap;

use alloy_primitives::U256;
use parking_lot::Mutex;
use tracing::{error, warn};

use liq_flash::{FlashIndex, Haircut};
use liq_types::{AssetId, FlashProvider, ProtocolId};

#[derive(Clone, Debug)]
pub struct CapConfig {
    pub per_liquidation_notional: U256,
    pub per_protocol_exposure: U256,
    pub global_concurrent: u32,
    pub per_provider_concurrent: u32,
    pub concentration_alert_bps: u32,
    pub haircut_floor_bps: u16,
    pub haircut_ceil_bps: u16,
}

impl Default for CapConfig {
    fn default() -> Self {
        Self {
            // No notional cap. Size is the viability band's job.
            per_liquidation_notional: U256::MAX,
            per_protocol_exposure: U256::MAX,
            global_concurrent: 20,
            per_provider_concurrent: 1,
            concentration_alert_bps: 8_000,
            haircut_floor_bps: 8_000,
            haircut_ceil_bps: 9_900,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CapError {
    #[error("notional {notional} exceeds per-liquidation cap {cap}")]
    Notional { notional: U256, cap: U256 },
    #[error("protocol exposure would exceed cap")]
    Protocol,
    #[error("global concurrent cap {cap} full")]
    Global { cap: u32 },
    #[error("provider {provider:?} concurrent cap {cap} full")]
    Provider { provider: FlashProvider, cap: u32 },
}

struct Live {
    global: u32,
    by_protocol: HashMap<ProtocolId, U256>,
    by_provider: HashMap<FlashProvider, u32>,
    liq_by_provider: HashMap<FlashProvider, u64>,
    liq_total: u64,
    il_ok: u64,
    il_fail: u64,
    haircut_bps: u16,
}

/// Binding caps. Per-provider concurrent of 1 prevents same-block same-pool contention.
pub struct Caps {
    cfg: CapConfig,
    live: Mutex<Live>,
}

impl Caps {
    #[must_use]
    pub fn new(cfg: CapConfig) -> Self {
        let haircut_bps = cfg.haircut_ceil_bps;
        Self {
            cfg,
            live: Mutex::new(Live {
                global: 0,
                by_protocol: HashMap::new(),
                by_provider: HashMap::new(),
                liq_by_provider: HashMap::new(),
                liq_total: 0,
                il_ok: 0,
                il_fail: 0,
                haircut_bps,
            }),
        }
    }

    pub fn try_acquire(
        &self,
        protocol: ProtocolId,
        provider: FlashProvider,
        notional: U256,
    ) -> Result<CapGuard<'_>, CapError> {
        if notional > self.cfg.per_liquidation_notional {
            return Err(CapError::Notional {
                notional,
                cap: self.cfg.per_liquidation_notional,
            });
        }
        let mut g = self.live.lock();
        if g.global >= self.cfg.global_concurrent {
            return Err(CapError::Global {
                cap: self.cfg.global_concurrent,
            });
        }
        let pc = g.by_provider.get(&provider).copied().unwrap_or(0);
        if pc >= self.cfg.per_provider_concurrent {
            return Err(CapError::Provider {
                provider,
                cap: self.cfg.per_provider_concurrent,
            });
        }
        let exp = g.by_protocol.get(&protocol).copied().unwrap_or(U256::ZERO);
        let next = match exp.checked_add(notional) {
            Some(n) => n,
            None => {
                error!("protocol exposure overflow");
                return Err(CapError::Protocol);
            }
        };
        if next > self.cfg.per_protocol_exposure {
            return Err(CapError::Protocol);
        }
        g.global = g.global.saturating_add(1);
        g.by_provider.insert(provider, pc.saturating_add(1));
        g.by_protocol.insert(protocol, next);
        drop(g);
        Ok(CapGuard {
            caps: self,
            protocol,
            provider,
            notional,
        })
    }

    fn release(&self, protocol: ProtocolId, provider: FlashProvider, notional: U256) {
        let mut g = self.live.lock();
        g.global = g.global.saturating_sub(1);
        if let Some(n) = g.by_provider.get_mut(&provider) {
            *n = n.saturating_sub(1);
        }
        if let Some(e) = g.by_protocol.get_mut(&protocol) {
            *e = e.saturating_sub(notional);
        }
    }

    pub fn record_fill(&self, provider: FlashProvider) {
        let mut g = self.live.lock();
        g.liq_total = g.liq_total.saturating_add(1);
        *g.liq_by_provider.entry(provider).or_insert(0) = g
            .liq_by_provider
            .get(&provider)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        let total = g.liq_total;
        let n = g.liq_by_provider.get(&provider).copied().unwrap_or(0);
        let bps = if total == 0 {
            0
        } else {
            n.saturating_mul(10_000).checked_div(total).unwrap_or(0)
        };
        if bps >= u64::from(self.cfg.concentration_alert_bps) {
            warn!(?provider, bps, "flash provider concentration Class C alert");
        }
    }

    /// Haircut widens (lower remaining bps) when InsufficientLiquidity rate rises.
    pub fn observe_liquidity_result(&self, insufficient: bool) -> Haircut {
        let mut g = self.live.lock();
        if insufficient {
            g.il_fail = g.il_fail.saturating_add(1);
        } else {
            g.il_ok = g.il_ok.saturating_add(1);
        }
        let den = g.il_ok.saturating_add(g.il_fail);
        if den == 0 {
            return Haircut::from_bps(g.haircut_bps).unwrap_or(Haircut::NONE);
        }
        let fail_bps = g
            .il_fail
            .saturating_mul(10_000)
            .checked_div(den)
            .unwrap_or(0);
        if fail_bps > 500 {
            g.haircut_bps = g
                .haircut_bps
                .saturating_sub(50)
                .max(self.cfg.haircut_floor_bps);
        } else if fail_bps == 0 {
            g.haircut_bps = g
                .haircut_bps
                .saturating_add(10)
                .min(self.cfg.haircut_ceil_bps);
        }
        Haircut::from_bps(g.haircut_bps).unwrap_or(Haircut::NONE)
    }

    #[must_use]
    pub fn haircut(&self) -> Haircut {
        let bps = self.live.lock().haircut_bps;
        Haircut::from_bps(bps).unwrap_or(Haircut::NONE)
    }

    /// Class C: major debt asset must have ≥ 2 sources; depth vs median.
    pub fn observe_index(&self, idx: &FlashIndex, majors: &[AssetId], median_notional: U256) {
        for &a in majors {
            let n = idx.entries(a).len();
            if n < 2 {
                warn!(asset = a.0, sources = n, "single flash source Class C");
            }
            let avail = idx.available(a);
            if avail < median_notional {
                warn!(
                    asset = a.0,
                    %avail,
                    %median_notional,
                    "flash depth below median Class C"
                );
            }
        }
    }
}

/// RAII slot. Dropping releases concurrent + exposure.
pub struct CapGuard<'a> {
    caps: &'a Caps,
    protocol: ProtocolId,
    provider: FlashProvider,
    notional: U256,
}

impl Drop for CapGuard<'_> {
    fn drop(&mut self) {
        self.caps
            .release(self.protocol, self.provider, self.notional);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use liq_flash::FlashIndex;

    #[test]
    fn per_provider_concurrent_blocks_second() {
        let caps = Caps::new(CapConfig {
            per_provider_concurrent: 1,
            ..CapConfig::default()
        });
        let a = caps
            .try_acquire(ProtocolId(1), FlashProvider::Aave, U256::from(1u64))
            .unwrap();
        let b = caps.try_acquire(ProtocolId(1), FlashProvider::Aave, U256::from(1u64));
        assert!(matches!(b, Err(CapError::Provider { .. })));
        let c = caps.try_acquire(ProtocolId(1), FlashProvider::Morpho, U256::from(1u64));
        assert!(c.is_ok());
        drop(a);
        assert!(caps
            .try_acquire(ProtocolId(1), FlashProvider::Aave, U256::from(1u64))
            .is_ok());
    }

    #[test]
    fn synthetic_cascade_second_same_pool_denied() {
        let caps = Caps::new(CapConfig {
            per_provider_concurrent: 1,
            global_concurrent: 20,
            ..CapConfig::default()
        });
        let g0 = caps
            .try_acquire(ProtocolId(0), FlashProvider::UniV3, U256::from(10u64))
            .unwrap();
        assert!(caps
            .try_acquire(ProtocolId(0), FlashProvider::UniV3, U256::from(10u64))
            .is_err());
        drop(g0);
    }

    #[test]
    fn haircut_widens_on_insufficient_liquidity() {
        let caps = Caps::new(CapConfig::default());
        let before = caps.haircut();
        for _ in 0..20 {
            let _ = caps.observe_liquidity_result(true);
        }
        let after = caps.haircut();
        assert_ne!(before, after);
    }

    #[test]
    fn index_single_source_is_alert_not_error() {
        let caps = Caps::new(CapConfig::default());
        let idx = FlashIndex::new(2);
        caps.observe_index(&idx, &[AssetId(0)], U256::from(1u64));
    }
}
