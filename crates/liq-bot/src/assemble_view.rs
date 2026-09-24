//! Process [`AssembleView`]: intern + [`TailPins`], fail closed.
//!
//! Empty / missing pin → [`AssembleError::Missing`] via [`leg_meta_from_pins`].
//! Intern/token miss → no address. Do not invent Fluid 1e27, Compound
//! decimals, or Gearbox PriceUpdate.

use std::collections::HashMap;
use std::sync::Arc;

use alloy_primitives::{Address, U256};
use liq_protocol::{ExecutorAdapter, Quote};
use liq_router::{
    euler_min_yield_from_quote, fluid_col_per_unit_debt_from_quote, gearbox_min_seized_from_quote,
    leg_meta_from_pins, AssembleError, AssembleView, LegMeta, MarketView, PairTerms, TailPins,
    ViabilityBand,
};

use crate::bands::{BandKey, BandShared};
use liq_state::AssetInterner;
use liq_types::{AssetId, PositionId, ProtocolId};

/// Process-owned assembly lookups. Starts empty (fail closed).
#[derive(Clone, Debug, Default)]
pub struct ProcessAssembleView {
    intern: AssetInterner,
    pins: HashMap<PositionId, TailPins>,
    per_eth: HashMap<AssetId, U256>,
    pair_terms: HashMap<(ProtocolId, AssetId, AssetId), PairTerms>,
    /// Warm-thread band table. `None` → no bands → nothing sizes.
    bands: Option<Arc<BandShared>>,
    /// Bands the drain computed this block for pairs the table has not
    /// evaluated yet. The newer of this and the table wins.
    local_bands: HashMap<BandKey, ViabilityBand>,
}

impl ProcessAssembleView {
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Intern a token address. Zero address refused.
    pub fn intern_token(&mut self, addr: Address) -> Result<AssetId, AssembleError> {
        if addr.is_zero() {
            tracing::error!("intern refused zero token");
            return Err(AssembleError::Missing("token"));
        }
        self.intern.intern(addr).map_err(|e| {
            tracing::error!(error = %e, "asset intern exhausted");
            AssembleError::Missing("intern")
        })
    }

    pub fn insert_pins(&mut self, pos: PositionId, pins: TailPins) {
        self.pins.insert(pos, pins);
    }

    #[must_use]
    pub fn has_pins(&self, pos: PositionId) -> bool {
        self.pins.contains_key(&pos)
    }

    pub fn insert_per_eth(&mut self, asset: AssetId, units: U256) {
        if units.is_zero() {
            tracing::error!(?asset, "per_eth zero refused");
            return;
        }
        self.per_eth.insert(asset, units);
    }

    pub fn insert_pair_terms(
        &mut self,
        protocol: ProtocolId,
        coll: AssetId,
        debt: AssetId,
        terms: PairTerms,
    ) {
        self.pair_terms.insert((protocol, coll, debt), terms);
    }

    #[must_use]
    pub fn with_bands(mut self, bands: Arc<BandShared>) -> Self {
        self.bands = Some(bands);
        self
    }

    #[must_use]
    pub fn bands(&self) -> Option<&Arc<BandShared>> {
        self.bands.as_ref()
    }

    /// Published table's band for `key`, if the warm thread has one.
    #[must_use]
    pub fn published_band(&self, key: BandKey) -> Option<ViabilityBand> {
        let (p, c, d) = key;
        self.bands
            .as_ref()
            .and_then(|b| b.table.load().get(p, c, d).copied())
    }

    /// Block the published table was built for (`0` = none yet).
    #[must_use]
    pub fn published_block(&self) -> u64 {
        self.bands.as_ref().map_or(0, |b| b.table.load().block)
    }

    pub fn insert_local_band(&mut self, key: BandKey, band: ViabilityBand) {
        self.local_bands.insert(key, band);
    }

    #[must_use]
    pub fn local_band(&self, key: BandKey) -> Option<ViabilityBand> {
        self.local_bands.get(&key).copied()
    }

    /// Drop local bands the published table has caught up with.
    pub fn prune_local_bands(&mut self) {
        let Some(shared) = self.bands.as_ref() else {
            return;
        };
        let table = shared.table.load();
        self.local_bands
            .retain(|&(p, c, d), b| table.get(p, c, d).is_none_or(|t| t.block < b.block));
    }

    /// Quote-derived tail fill on a pinned position. Missing pin → Missing.
    pub fn apply_quote_for(
        &mut self,
        pos: PositionId,
        quote: &Quote,
        repay: usize,
        seize: usize,
        tol_bps: u16,
    ) -> Result<(), AssembleError> {
        let pins = self
            .pins
            .get_mut(&pos)
            .ok_or(AssembleError::Missing("pins"))?;
        Self::apply_quote_derived(pins, quote, repay, seize, tol_bps)
    }

    /// Fill quote-derived tail fields only. Does not invent `fluid_t1`,
    /// Compound `is_cether`, or Gearbox full MultiCall.
    /// `tol_bps` is [`liq_router::select::SelectCfg::min_out_tolerance_bps`]:
    /// the slack between the quoted seize and the minimum handed to the
    /// protocol. Zero reproduces the old behaviour and reverts on any drift.
    pub fn apply_quote_derived(
        pins: &mut TailPins,
        quote: &Quote,
        repay: usize,
        seize: usize,
        tol_bps: u16,
    ) -> Result<(), AssembleError> {
        match pins.adapter {
            ExecutorAdapter::EulerV2 => {
                pins.euler_min_yield = Some(euler_min_yield_from_quote(quote, seize, tol_bps)?);
                let target = quote
                    .seize_options
                    .get(seize)
                    .ok_or(AssembleError::Missing("euler seize"))?
                    .call_target;
                if target.is_zero() {
                    return Err(AssembleError::Missing("euler collateral vault"));
                }
                pins.euler_collateral_vault = Some(target);
            }
            ExecutorAdapter::Fluid => {
                pins.fluid_col_per_unit_debt =
                    Some(fluid_col_per_unit_debt_from_quote(quote, repay, seize)?);
            }
            ExecutorAdapter::Gearbox => {
                pins.gearbox_min_seized =
                    Some(gearbox_min_seized_from_quote(quote, seize, tol_bps)?);
                // The all-or-nothing repay option is the full liquidation.
                pins.gearbox_full = !quote
                    .repay_options
                    .get(repay)
                    .ok_or(AssembleError::Missing("gearbox repay"))?
                    .min_repay
                    .is_zero();
            }
            ExecutorAdapter::AaveV3
            | ExecutorAdapter::AaveV4
            | ExecutorAdapter::MorphoBlue
            | ExecutorAdapter::SiloV2
            | ExecutorAdapter::LiquityV2
            | ExecutorAdapter::CompoundV2 => {}
        }
        Ok(())
    }
}

impl AssembleView for ProcessAssembleView {
    fn token(&self, asset: AssetId) -> Option<Address> {
        self.intern.address(asset).filter(|a| !a.is_zero())
    }

    fn meta(&self, pos: PositionId) -> Option<LegMeta> {
        let pins = self.pins.get(&pos)?;
        match leg_meta_from_pins(pins) {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::error!(error = %e, pos = pos.0, "tail pin refused");
                None
            }
        }
    }

    fn per_eth(&self, asset: AssetId) -> Option<U256> {
        self.per_eth.get(&asset).copied().filter(|v| !v.is_zero())
    }
}

impl MarketView for ProcessAssembleView {
    fn pair_terms(&self, protocol: ProtocolId, coll: AssetId, debt: AssetId) -> Option<PairTerms> {
        self.pair_terms.get(&(protocol, coll, debt)).copied()
    }

    fn per_eth(&self, asset: AssetId) -> Option<U256> {
        AssembleView::per_eth(self, asset)
    }

    fn band(&self, protocol: ProtocolId, coll: AssetId, debt: AssetId) -> Option<ViabilityBand> {
        let key = (protocol, coll, debt);
        match (self.published_band(key), self.local_band(key)) {
            (Some(p), Some(l)) => Some(if l.block > p.block { l } else { p }),
            (p, l) => p.or(l),
        }
    }
}

/// Same mapping `assemble` uses: missing intern/token is an error, not zero.
pub fn require_token(view: &dyn AssembleView, asset: AssetId) -> Result<Address, AssembleError> {
    match view.token(asset) {
        Some(t) if !t.is_zero() => Ok(t),
        _ => Err(AssembleError::Missing("token")),
    }
}

/// Same mapping `assemble` uses: missing / refused pin is an error, not a zero tail.
pub fn require_meta(view: &dyn AssembleView, pos: PositionId) -> Result<LegMeta, AssembleError> {
    view.meta(pos).ok_or(AssembleError::Missing("leg meta"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, U256};
    use liq_exec::wire::LegTail;
    use liq_protocol::{BonusCurve, RepayOption, SeizeOption};
    use liq_types::fixed::RAY;
    use liq_types::{MarketId, PositionKey, ProtocolId, Ray};

    fn empty_pins(adapter: ExecutorAdapter) -> TailPins {
        TailPins {
            adapter,
            market: address!("0x0000000000000000000000000000000000000051"),
            borrower: address!("0x00000000000000000000000000000000000000b1"),
            protocol_pull: None,
            euler_min_yield: None,
            euler_collateral_vault: None,
            liquity_trove_id: None,
            fluid_t1: None,
            fluid_col_per_unit_debt: None,
            gearbox_min_seized: None,
            gearbox_full: false,
            compound_ctoken_collateral: None,
            compound_is_cether: None,
            aave_v4_collateral_reserve_id: None,
            aave_v4_debt_reserve_id: None,
            morpho_market_id: None,
        }
    }

    #[test]
    fn empty_view_and_missing_intern_fail_closed() {
        let view = ProcessAssembleView::empty();
        assert!(matches!(
            require_token(&view, AssetId(0)),
            Err(AssembleError::Missing("token"))
        ));
        assert!(matches!(
            require_meta(&view, PositionId(1)),
            Err(AssembleError::Missing("leg meta"))
        ));
        assert!(view.token(AssetId(0)).is_none());
        assert!(view.meta(PositionId(1)).is_none());
    }

    #[test]
    fn empty_tail_pins_are_missing_not_zero_guess() {
        let mut view = ProcessAssembleView::empty();
        view.insert_pins(PositionId(1), empty_pins(ExecutorAdapter::EulerV2));
        assert!(
            matches!(
                leg_meta_from_pins(&empty_pins(ExecutorAdapter::EulerV2)),
                Err(AssembleError::Missing("euler min_yield"))
            ),
            "empty Euler pin must not become a zero min_yield"
        );
        assert!(matches!(
            require_meta(&view, PositionId(1)),
            Err(AssembleError::Missing("leg meta"))
        ));
        assert!(
            view.meta(PositionId(1)).is_none(),
            "refused pin must not yield a guessed LegTail"
        );

        let mut fluid = empty_pins(ExecutorAdapter::Fluid);
        fluid.fluid_t1 = Some(true);
        fluid.fluid_col_per_unit_debt = Some(RAY);
        assert!(
            matches!(
                leg_meta_from_pins(&fluid),
                Err(AssembleError::Missing("fluid col_per_unit_debt"))
            ),
            "1e27-scale Fluid pin must refuse, not assemble"
        );
    }

    #[test]
    fn interned_token_is_the_interned_address() {
        let mut view = ProcessAssembleView::empty();
        let weth = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let id = view.intern_token(weth).unwrap();
        assert_eq!(require_token(&view, id).unwrap(), weth);
        assert!(require_token(&view, AssetId(id.0.saturating_add(1))).is_err());
    }

    #[test]
    fn quote_helpers_fill_euler_not_zero() {
        let q = Quote {
            position: PositionId(1),
            key: PositionKey {
                protocol: ProtocolId(1),
                market: MarketId(0),
                user: address!("0x00000000000000000000000000000000000000b1"),
            },
            repay_options: smallvec::SmallVec::from_slice(&[RepayOption {
                min_repay: alloy_primitives::U256::ZERO,
                pair_seize: None,
                asset: AssetId(1),
                max_repay: U256::from(1u64),
                slot: liq_protocol::SlotRef::ByAsset,
            }]),
            seize_options: smallvec::SmallVec::from_slice(&[SeizeOption {
                asset: AssetId(0),
                max_seize: U256::from(9u64),
                bonus: Ray::from_raw(RAY / U256::from(20u64)),
                curve: BonusCurve::Static {
                    bonus: Ray::from_raw(RAY / U256::from(20u64)),
                },
                call_target: address!("0x00000000000000000000000000000000000000e1"),
                slot: liq_protocol::SlotRef::ByAsset,
            }]),
        };
        let mut pins = empty_pins(ExecutorAdapter::EulerV2);
        let zero = q.clone();
        let mut zero_q = zero;
        zero_q.seize_options[0].call_target = Address::ZERO;
        assert!(matches!(
            ProcessAssembleView::apply_quote_derived(&mut pins, &zero_q, 0, 0, 0),
            Err(AssembleError::Missing("euler collateral vault"))
        ));
        ProcessAssembleView::apply_quote_derived(&mut pins, &q, 0, 0, 0).unwrap();
        match leg_meta_from_pins(&pins).unwrap().tail {
            LegTail::Euler { min_yield, vault } => {
                assert_eq!(min_yield, U256::from(9u64));
                assert_eq!(
                    vault,
                    address!("0x00000000000000000000000000000000000000e1")
                );
            }
            other => panic!("expected Euler tail, got {other:?}"),
        }
    }
}
