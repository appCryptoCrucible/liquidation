//! Measured gas model (`config/liq-gas.toml`; methodology in
//! `tools/gas-measure`). One liquidation plan through our Executor costs
//!
//! ```text
//! tx.base                       once per transaction (intrinsic, calldata,
//!                               one profit swap)
//! + wrap[provider]              once per flash group (executor + flash)
//! + Σ legs ( liquidation[protocol]  the protocol call + our guard reads
//!          + exit swap hops )       sized exactly per plan from the pool book
//! ```
//!
//! A protocol or provider with no measured figure is `0` = not measured:
//! `select` skips that leg and the band refuses that pair rather than
//! pricing gas at zero.

use std::path::Path;

use liq_router::LiqGas;
use liq_types::{FlashProvider, ProtocolId};

/// Band-side view of the model: what one leg costs outside its exit hops.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BandGas {
    pub tx_base: u64,
    /// Per `FlashProvider as usize`, executor + flash. Excludes `tx_base`.
    pub wrap: [u64; 7],
    /// Aave V3 flash wrapping an Aave V4 leg.
    pub wrap_aave_v4: u64,
    pub aave_v4: Option<ProtocolId>,
    pub liq: LiqGas,
}

impl BandGas {
    #[must_use]
    pub const fn none() -> Self {
        Self {
            tx_base: 0,
            wrap: [0; 7],
            wrap_aave_v4: 0,
            aave_v4: None,
            liq: LiqGas::none(),
        }
    }

    fn wrap_for(&self, protocol: ProtocolId, provider: FlashProvider) -> u64 {
        if provider == FlashProvider::Aave && self.aave_v4 == Some(protocol) {
            return self.wrap_aave_v4;
        }
        self.wrap.get(provider as usize).copied().unwrap_or(0)
    }

    /// Non-swap gas of a one-leg plan: the band's `fixed_gas`. `0` when any
    /// part is unmeasured — the band then refuses the pair.
    #[must_use]
    pub fn fixed(&self, protocol: ProtocolId, provider: FlashProvider) -> u64 {
        let wrap = self.wrap_for(protocol, provider);
        let Ok(liq) = self.liq.get(protocol) else {
            return 0;
        };
        if self.tx_base == 0 || wrap == 0 {
            return 0;
        }
        self.tx_base.saturating_add(wrap).saturating_add(liq)
    }
}

/// Per-venue gas of one swap hop inside the Executor.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HopGas {
    pub univ3: u64,
    /// Uniswap V4 swap leg (venue 9): the PoolManager `unlock` around
    /// `swap`, `settle` and `take`.
    pub univ4: u64,
    pub univ2: u64,
    pub curve: u64,
    /// StableSwap-NG `exchange`: dynamic fee, `stored_rates()` (external
    /// rate calls for oracle / ERC-4626 coins) and the oracle upkeep.
    pub curve_ng: u64,
    /// Curve crypto `exchange`: the gamma-curve solve plus `tweak_price`.
    pub curve_crypto: u64,
    /// PancakeSwap V3 pool swap (venue 0, factory id 2): Uniswap's swap plus
    /// the liquidity-mining hook's `accumulateReward` and `crossLmTick`.
    pub pancake_v3: u64,
    /// Balancer V2 `Vault.swap` of a weighted pool (venue 11), the dex
    /// module's delegatecall and the allowance included.
    pub balancer: u64,
    /// Fluid DEX T1 `swapIn` (venue 12): the two Liquidity-layer operations,
    /// the pool's price and reserve math, the allowance, the dex module's
    /// delegatecall and the pool authentication.
    pub fluid: u64,
    /// ERC-4626 `redeem` of a seized wrapper (swap venue 5).
    pub unwrap_4626: u64,
    /// Expired Pendle PT: YT `redeemPY` + SY `redeem` (swap venue 6).
    pub pendle_pt: u64,
    /// Curve NG `remove_liquidity_one_coin` (swap venue 7).
    pub curve_lp: u64,
    /// Pendle market `swapExactPtForSy` + SY `redeem` (swap venue 8).
    pub pendle_market: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GasModel {
    pub tx_base: u64,
    pub wrap: [u64; 7],
    pub wrap_aave_v4: u64,
    /// `(registry family, gas)` as committed.
    pub liquidation: Vec<(String, u64)>,
    pub hop: HopGas,
}

#[derive(serde::Deserialize)]
struct Toml {
    tx: TxToml,
    wrap: WrapToml,
    liquidation: std::collections::BTreeMap<String, u64>,
    swap: SwapToml,
}

#[derive(serde::Deserialize)]
struct TxToml {
    base: u64,
}

#[derive(serde::Deserialize)]
struct WrapToml {
    aave: Option<u64>,
    aave_v4: Option<u64>,
    univ3: Option<u64>,
    univ4: Option<u64>,
    morpho: Option<u64>,
    sky_dss: Option<u64>,
    univ3_swap: Option<u64>,
}

#[derive(serde::Deserialize)]
struct SwapToml {
    univ3: Option<u64>,
    univ4: Option<u64>,
    univ2: Option<u64>,
    curve: Option<u64>,
    curve_ng: Option<u64>,
    curve_crypto: Option<u64>,
    pancake_v3: Option<u64>,
    balancer: Option<u64>,
    fluid: Option<u64>,
    unwrap_4626: Option<u64>,
    pendle_pt: Option<u64>,
    curve_lp: Option<u64>,
    pendle_market: Option<u64>,
}

impl GasModel {
    /// `None` when the file is missing or malformed (logged) — every leg is
    /// then unmeasured and nothing sizes, rather than sizing at zero gas.
    #[must_use]
    pub fn load(path: &Path) -> Option<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, path = %path.display(), "liq-gas.toml unreadable — no leg is sized");
                return None;
            }
        };
        let t: Toml = match toml::from_str(&raw) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = %e, "liq-gas.toml malformed — no leg is sized");
                return None;
            }
        };
        let mut wrap = [0u64; 7];
        for (p, g) in [
            (FlashProvider::Aave, t.wrap.aave),
            (FlashProvider::UniV3, t.wrap.univ3),
            (FlashProvider::UniV4, t.wrap.univ4),
            (FlashProvider::Morpho, t.wrap.morpho),
            (FlashProvider::SkyDss, t.wrap.sky_dss),
            (FlashProvider::UniV3Swap, t.wrap.univ3_swap),
        ] {
            if let Some(slot) = wrap.get_mut(p as usize) {
                *slot = g.unwrap_or(0);
            }
        }
        Some(Self {
            tx_base: t.tx.base,
            wrap,
            wrap_aave_v4: t.wrap.aave_v4.unwrap_or(0),
            liquidation: t.liquidation.into_iter().collect(),
            hop: HopGas {
                univ3: t.swap.univ3.unwrap_or(0),
                univ4: t.swap.univ4.unwrap_or(0),
                univ2: t.swap.univ2.unwrap_or(0),
                curve: t.swap.curve.unwrap_or(0),
                curve_ng: t.swap.curve_ng.unwrap_or(0),
                curve_crypto: t.swap.curve_crypto.unwrap_or(0),
                pancake_v3: t.swap.pancake_v3.unwrap_or(0),
                balancer: t.swap.balancer.unwrap_or(0),
                fluid: t.swap.fluid.unwrap_or(0),
                unwrap_4626: t.swap.unwrap_4626.unwrap_or(0),
                pendle_pt: t.swap.pendle_pt.unwrap_or(0),
                curve_lp: t.swap.curve_lp.unwrap_or(0),
                pendle_market: t.swap.pendle_market.unwrap_or(0),
            },
        })
    }

    /// Per-protocol leg gas keyed by protocol id. `resolve` maps a family
    /// name to its id (registry intern, then config-defined adapters such as
    /// Fluid). Unknown families are logged and dropped.
    #[must_use]
    pub fn liq_gas(&self, resolve: &dyn Fn(&str) -> Option<ProtocolId>) -> LiqGas {
        let mut out = LiqGas::none();
        for (family, gas) in &self.liquidation {
            match resolve(family) {
                Some(id) => {
                    if !out.set(id, *gas) {
                        tracing::error!(family, id = id.0, "protocol id outside LiqGas table");
                    }
                }
                None => tracing::error!(family, "liq-gas.toml family not loaded"),
            }
        }
        out
    }

    /// Plan-level wrap per provider as `select` charges it once per flash
    /// group: executor + flash, plus the per-transaction base (conservative
    /// for a multi-group plan, exact for the common one-group plan).
    #[must_use]
    pub fn select_wrap(&self) -> crate::bind::WrapGas {
        let add = |g: u64| {
            if g == 0 {
                0
            } else {
                g.saturating_add(self.tx_base)
            }
        };
        crate::bind::WrapGas {
            by_provider: self.wrap.map(add),
            aave_v4: add(self.wrap_aave_v4),
        }
    }

    #[must_use]
    pub fn band_gas(&self, resolve: &dyn Fn(&str) -> Option<ProtocolId>) -> BandGas {
        BandGas {
            tx_base: self.tx_base,
            wrap: self.wrap,
            wrap_aave_v4: self.wrap_aave_v4,
            aave_v4: resolve("aave-v4"),
            liq: self.liq_gas(resolve),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use liq_config::Intern;

    fn root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    #[test]
    fn committed_liq_gas_loads_and_every_family_resolves() {
        let intern = Intern::from_registry(
            &liq_config::Registry::from_path(&root().join("registry/registry.json")).unwrap(),
        )
        .unwrap();
        let m = GasModel::load(&root().join("config/liq-gas.toml")).unwrap();
        // Fluid / Liquity / Gearbox ids are config-defined (not in
        // registry.json): `protocol` in config/protocols/<family>.toml.
        let resolve = |f: &str| {
            intern.protocol(f).or(match f {
                "fluid" => Some(ProtocolId(10)),
                "liquity-v2" => Some(ProtocolId(9)),
                "gearbox" => Some(ProtocolId(11)),
                "aave-v2" => Some(ProtocolId(12)),
                _ => None,
            })
        };
        for (family, gas) in &m.liquidation {
            let id = resolve(family).unwrap_or_else(|| panic!("{family} unresolved"));
            assert_eq!(m.liq_gas(&resolve).get(id).unwrap(), *gas);
        }
        assert!(m.tx_base > 21_000 && m.hop.univ3 > 0);
        // Slot 5 is `FlashProvider::None`: nothing wraps a reward-only group.
        assert!(
            m.wrap
                .iter()
                .enumerate()
                .all(|(i, &w)| i == FlashProvider::None as usize || w > 0),
            "every provider measured"
        );
        let w = m.select_wrap();
        assert_eq!(
            w.by_provider[FlashProvider::UniV4 as usize],
            m.wrap[FlashProvider::UniV4 as usize] + m.tx_base
        );
        let band = m.band_gas(&resolve);
        let v3 = intern.protocol("aave-v3").unwrap();
        assert_eq!(
            band.fixed(v3, FlashProvider::Morpho),
            m.tx_base + m.wrap[FlashProvider::Morpho as usize] + 453_511
        );
    }

    #[test]
    fn fixed_gas_is_zero_when_any_part_is_unmeasured() {
        let mut liq = LiqGas::none();
        liq.set(ProtocolId(1), 120_000);
        let g = BandGas {
            tx_base: 100_000,
            wrap: [200_000, 130_000, 0, 110_000, 0, 0, 0],
            wrap_aave_v4: 0,
            aave_v4: None,
            liq,
        };
        assert_eq!(g.fixed(ProtocolId(1), FlashProvider::Aave), 420_000);
        assert_eq!(
            g.fixed(ProtocolId(1), FlashProvider::UniV4),
            0,
            "unmeasured provider"
        );
        assert_eq!(
            g.fixed(ProtocolId(2), FlashProvider::Aave),
            0,
            "unmeasured protocol"
        );
        let no_base = BandGas { tx_base: 0, ..g };
        assert_eq!(no_base.fixed(ProtocolId(1), FlashProvider::Aave), 0);
    }
}
