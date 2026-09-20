//! Candidate selection (GUIDE 12 §4f, D33, D34): the **gate**, then ranking.
//!
//! Eligibility is a separate type from ranking. [`admit`] is the only
//! constructor of [`Eligible`]. A high-net position that is not
//! liquidatable cannot enter the ranked set at any score.
//!
//! Ranking uses expected contribution-per-gas with a **pre-gas** numerator
//! and `p` (start [`crate::profit::LEARNING_P_RAY`] = 1). No batch-count
//! threshold: a single position is one group with `liqCount == 1`.

use alloy_primitives::U256;
use liq_flash::cascade::{plan as plan_cascade, Cascade};
use liq_flash::{CostModel, FlashIndex};
use liq_protocol::{Health, HealthState, Quote};
use liq_types::fixed::RAY;
use liq_types::{AssetId, PositionId, ProtocolId, Ray, TriggerKind};
use smallvec::SmallVec;

use crate::exact::{GasTerms, SolveBudget};
use crate::profit::{
    best_plan, delta_net, expected_contrib_per_gas, expected_gas, gas_price_in_debt, MarketView,
    ProfitCtx, ProfitError, SizedLeg, LEARNING_P_RAY,
};
use crate::solver::PoolBook;
use crate::warm::RouteTable;

/// Why selection refused the whole drain. Per-candidate unavailability is
/// a skip, not this error.
#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    #[error(transparent)]
    Profit(#[from] ProfitError),
    #[error("header gas limit missing (must come from the block header)")]
    MissingGasLimit,
    #[error("nonce slot count is zero")]
    NoNonceSlots,
    #[error("exact-solve budget K is zero")]
    ZeroExactK,
    #[error("wrap gas for the chosen flash provider is zero (10C stub; do not bid)")]
    ZeroWrapGas,
    #[error("failed-leg gas is required")]
    ZeroFailedGas,
    #[error("p > 1")]
    BadP,
}

/// One engine candidate as the router sees it. `liq-engine::Candidate` is
/// not a crate dependency (forbid.txt: engine ↛ router; we take the
/// fields GUIDE 08 already put on the wire).
#[derive(Clone, Debug)]
pub struct PositionInput<'a> {
    pub position: PositionId,
    pub protocol: ProtocolId,
    pub health: Health,
    pub quote: &'a Quote,
    /// Payload-free contest class (liq-types). `OraclePredicted` is never
    /// admitted.
    pub cause: TriggerKind,
    /// Probability still liquidatable at inclusion. Learning phase: [`LEARNING_P_RAY`].
    pub p: Ray,
    /// Gas of a successful attempt (sim, or wrap+liq+hop after exact).
    /// `None` → filled from wrap + `liq_gas` + hop after the exact solve.
    pub gas_success: Option<u64>,
    /// Gas of a beaten / reverted try-catch leg. Required, never defaulted.
    pub gas_failed: u64,
}

/// A position that passed the **gate**. The only way to obtain one is
/// [`admit`]. Ranking takes `&[Eligible]`, never raw inputs.
#[derive(Copy, Clone, Debug)]
pub struct Eligible<'a> {
    pos: &'a PositionInput<'a>,
}

impl<'a> Eligible<'a> {
    #[inline]
    #[must_use]
    pub fn position(&self) -> PositionId {
        self.pos.position
    }

    #[inline]
    #[must_use]
    pub fn quote(&self) -> &'a Quote {
        self.pos.quote
    }

    #[inline]
    #[must_use]
    pub fn health(&self) -> Health {
        self.pos.health
    }

    #[inline]
    #[must_use]
    pub fn inner(&self) -> &'a PositionInput<'a> {
        self.pos
    }
}

/// GUIDE 12 §4f / D33. Simulation against updated state at HF < 1 **and**
/// the protocol will take the liquidation. Ranking never calls this.
#[inline]
#[must_use]
pub fn is_liquidatable(h: &Health) -> bool {
    h.state == HealthState::Liquidatable && h.hf < Ray::ONE
}

/// Gate. `OraclePredicted` is pre-warm only (GUIDE 08). `p > 1` is refused
/// rather than clamped.
#[must_use]
pub fn admit<'a>(p: &'a PositionInput<'a>) -> Option<Eligible<'a>> {
    if p.cause == TriggerKind::OraclePredicted {
        return None;
    }
    if p.p.raw() > RAY {
        return None;
    }
    if !is_liquidatable(&p.health) {
        return None;
    }
    Some(Eligible { pos: p })
}

/// Selection / truncation parameters. `header_gas_limit` is the parent
/// header's gas limit (GUIDE 12 §4f) — never a constant in this crate.
#[derive(Copy, Clone, Debug)]
pub struct SelectCfg {
    pub cost: CostModel,
    pub close_bps: u16,
    /// Exact-solve the top `exact_k` after a warm crude rank. `0` refused.
    pub exact_k: u8,
    /// Max bundles this drain may emit (GUIDE 13 nonce slots).
    pub nonce_slots: u8,
    pub header_gas_limit: u64,
    /// Wrapping gas per `FlashProvider as usize`. 10C measurements.
    pub wrap_gas: [u64; 5],
    pub liq_gas: u64,
    pub over_borrow: U256,
    pub budget: SolveBudget,
}

/// One scored, exact-solved leg ready to batch.
#[derive(Clone, Debug)]
pub struct Scored {
    pub position: PositionId,
    pub protocol: ProtocolId,
    pub p: Ray,
    pub gas_success: u64,
    pub gas_failed: u64,
    pub expected_gas: u64,
    pub contrib_per_gas: U256,
    pub leg: SizedLeg,
}

/// One nonce's worth of legs, grouped by debt asset, with the 07B cascade
/// that funds that debt.
#[derive(Clone, Debug)]
pub struct SelectedPlan {
    pub groups: SmallVec<[DebtGroup; 4]>,
    pub hop_and_wrap_gas: u64,
}

/// Legs sharing a debt asset plus the cascade that funds them.
#[derive(Clone, Debug)]
pub struct DebtGroup {
    pub debt: AssetId,
    pub legs: SmallVec<[Scored; 4]>,
    pub cascade: Cascade,
    /// Σ `s` across legs (flash principal before over-borrow).
    pub need: U256,
}

fn wrap_gas(cfg: &SelectCfg, provider: liq_types::FlashProvider) -> Result<u64, SelectError> {
    let g = cfg
        .wrap_gas
        .get(provider as usize)
        .copied()
        .ok_or(SelectError::ZeroWrapGas)?;
    if g == 0 {
        return Err(SelectError::ZeroWrapGas);
    }
    Ok(g)
}

fn success_gas(
    cfg: &SelectCfg,
    scored_hop: u64,
    provider: liq_types::FlashProvider,
) -> Result<u64, SelectError> {
    wrap_gas(cfg, provider)?
        .checked_add(cfg.liq_gas)
        .and_then(|a| a.checked_add(scored_hop))
        .ok_or(SelectError::ZeroWrapGas)
}

/// Drain → plans. Gate first, then two-stage rank, then group / cascade /
/// truncate on Δnet, header gas, nonce slots.
#[allow(clippy::too_many_arguments)] // gate inputs are all required, none defaulted
pub fn select(
    inputs: &[PositionInput<'_>],
    cfg: &SelectCfg,
    flash: &FlashIndex,
    haircut: liq_flash::Haircut,
    book: &PoolBook,
    warm: Option<&RouteTable>,
    market: &dyn MarketView,
    gas: &GasTerms,
) -> Result<SmallVec<[SelectedPlan; 4]>, SelectError> {
    if cfg.header_gas_limit == 0 {
        return Err(SelectError::MissingGasLimit);
    }
    if cfg.nonce_slots == 0 {
        return Err(SelectError::NoNonceSlots);
    }
    if cfg.exact_k == 0 {
        return Err(SelectError::ZeroExactK);
    }

    let mut admitted: SmallVec<[Eligible<'_>; 16]> = SmallVec::new();
    for p in inputs {
        if let Some(e) = admit(p) {
            admitted.push(e);
        }
    }
    rank(&admitted, cfg, flash, haircut, book, warm, market, gas)
}

#[allow(clippy::too_many_arguments)] // same required set as select
fn rank(
    admitted: &[Eligible<'_>],
    cfg: &SelectCfg,
    flash: &FlashIndex,
    haircut: liq_flash::Haircut,
    book: &PoolBook,
    warm: Option<&RouteTable>,
    market: &dyn MarketView,
    gas: &GasTerms,
) -> Result<SmallVec<[SelectedPlan; 4]>, SelectError> {
    if admitted.is_empty() {
        return Ok(SmallVec::new());
    }
    // Exact-solve every admitted candidate up to exact_k after a crude
    // pre-rank. With no warm table the crude key is 0 and we exact-solve
    // the first exact_k in admission order (still gated).
    let mut crude: SmallVec<[(U256, usize); 16]> = SmallVec::new();
    for (i, e) in admitted.iter().enumerate() {
        crude.push((crude_key(e, warm), i));
    }
    crude.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let k = usize::from(cfg.exact_k).min(crude.len());

    let mut scored: SmallVec<[Scored; 16]> = SmallVec::new();
    for &(_, idx) in crude.iter().take(k) {
        let Some(el) = admitted.get(idx) else {
            continue;
        };
        let pctx = ProfitCtx {
            protocol: el.pos.protocol,
            flash,
            haircut,
            book,
            warm,
            market,
            gas,
            budget: &cfg.budget,
        };
        let Some(leg) = best_plan(&pctx, el.quote())? else {
            continue;
        };
        let gs = match el.pos.gas_success {
            Some(g) if g > 0 => g,
            Some(_) => return Err(SelectError::ZeroWrapGas),
            None => success_gas(cfg, leg.hop_gas, leg.route.provider)?,
        };
        if el.pos.gas_failed == 0 {
            return Err(SelectError::ZeroFailedGas);
        }
        let p_raw = el.pos.p.raw();
        let eg = expected_gas(p_raw, gs, el.pos.gas_failed).ok_or(SelectError::BadP)?;
        let cpg = expected_contrib_per_gas(leg.contribution, p_raw, gs, el.pos.gas_failed)?;
        scored.push(Scored {
            position: el.position(),
            protocol: el.pos.protocol,
            p: el.pos.p,
            gas_success: gs,
            gas_failed: el.pos.gas_failed,
            expected_gas: eg,
            contrib_per_gas: cpg,
            leg,
        });
    }
    scored.sort_by(|a, b| {
        b.contrib_per_gas
            .cmp(&a.contrib_per_gas)
            .then(a.position.0.cmp(&b.position.0))
    });
    pack(scored, cfg, flash, gas)
}

fn crude_key(e: &Eligible<'_>, warm: Option<&RouteTable>) -> U256 {
    let Some(t) = warm else {
        return U256::ZERO;
    };
    let q = e.quote();
    let Some(s0) = q.seize_options.first() else {
        return U256::ZERO;
    };
    t.exit_cap(s0.asset)
}

fn pack(
    scored: SmallVec<[Scored; 16]>,
    cfg: &SelectCfg,
    flash: &FlashIndex,
    gas: &GasTerms,
) -> Result<SmallVec<[SelectedPlan; 4]>, SelectError> {
    let price = gas_price_in_debt(gas)?;
    let mut plans: SmallVec<[SelectedPlan; 4]> = SmallVec::new();
    let mut cur = SelectedPlan {
        groups: SmallVec::new(),
        hop_and_wrap_gas: 0,
    };

    for s in scored {
        let p_raw = s.p.raw();
        let d = delta_net(s.leg.contribution, p_raw, s.expected_gas, price)?;
        if d.is_none_or(|v| v.is_zero()) {
            continue;
        }
        let wrap = wrap_gas(cfg, s.leg.route.provider)?;
        let same = cur.groups.iter().any(|g| g.debt == s.leg.debt);
        let incr = if same {
            s.leg.hop_gas.saturating_add(cfg.liq_gas)
        } else {
            s.leg
                .hop_gas
                .saturating_add(cfg.liq_gas)
                .saturating_add(wrap)
        };
        let new_gas = cur.hop_and_wrap_gas.saturating_add(incr);
        if new_gas > cfg.header_gas_limit {
            if !cur.groups.is_empty() {
                seal_cascades(&mut cur, cfg, flash)?;
                plans.push(cur);
                if plans.len() >= usize::from(cfg.nonce_slots) {
                    return Ok(plans);
                }
                cur = SelectedPlan {
                    groups: SmallVec::new(),
                    hop_and_wrap_gas: 0,
                };
            } else {
                // A single leg exceeds the header limit: cannot include it.
                continue;
            }
        }
        if plans.len() >= usize::from(cfg.nonce_slots) && cur.groups.is_empty() {
            break;
        }
        push_leg(&mut cur, s, incr);
    }
    if !cur.groups.is_empty() && plans.len() < usize::from(cfg.nonce_slots) {
        seal_cascades(&mut cur, cfg, flash)?;
        plans.push(cur);
    }
    Ok(plans)
}

fn push_leg(plan: &mut SelectedPlan, s: Scored, incr: u64) {
    plan.hop_and_wrap_gas = plan.hop_and_wrap_gas.saturating_add(incr);
    if let Some(g) = plan.groups.iter_mut().find(|g| g.debt == s.leg.debt) {
        g.need = g.need.saturating_add(s.leg.s);
        g.legs.push(s);
        return;
    }
    let debt = s.leg.debt;
    let need = s.leg.s;
    plan.groups.push(DebtGroup {
        debt,
        legs: SmallVec::from_elem(s, 1),
        cascade: Cascade {
            groups: SmallVec::new(),
            funded: U256::ZERO,
            cost: U256::ZERO,
        },
        need,
    });
}

fn seal_cascades(
    plan: &mut SelectedPlan,
    cfg: &SelectCfg,
    flash: &FlashIndex,
) -> Result<(), SelectError> {
    for g in &mut plan.groups {
        let need = g.need.saturating_add(cfg.over_borrow);
        let c = plan_cascade(flash, g.debt, need, &cfg.cost, cfg.close_bps)
            .ok_or(ProfitError::Missing("cascade"))?;
        if c.funded.is_zero() {
            return Err(ProfitError::Missing("cascade funded").into());
        }
        // Partial cascade: shrink legs so Σ s ≤ funded (partial beats skip).
        if c.funded < g.need {
            shrink_to_funded(g, c.funded);
        }
        g.cascade = c;
    }
    plan.groups
        .retain(|g| !g.legs.is_empty() && !g.need.is_zero());
    Ok(())
}

fn shrink_to_funded(g: &mut DebtGroup, funded: U256) {
    let mut left = funded;
    let mut keep = SmallVec::new();
    for s in g.legs.drain(..) {
        if left.is_zero() {
            break;
        }
        if s.leg.s <= left {
            left = left.saturating_sub(s.leg.s);
            keep.push(s);
        } else {
            // Partial the last included leg. Contribution scaled by s'/s
            // would be a guess on a nonlinear quote — drop rather than
            // fabricate. The already-kept prefix is the partial batch.
            break;
        }
    }
    g.legs = keep;
    g.need = g
        .legs
        .iter()
        .fold(U256::ZERO, |a, s| a.saturating_add(s.leg.s));
}

/// `p` used when the caller has no fitted value (learning phase).
#[inline]
#[must_use]
pub fn learning_p() -> Ray {
    Ray::from_raw(LEARNING_P_RAY)
}

#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use super::*;
    use crate::band::PairTerms;
    use crate::exact::GasTerms;
    use crate::fixtures::*;
    use crate::profit::MarketView;
    use crate::solver::{Pool, PoolBook};
    use alloy_primitives::U256;
    use liq_flash::{FlashIndex, FlashSource, Haircut, HeldAsset, MorphoBlue};
    use liq_protocol::{
        AssetMask, BonusCurve, Health, HealthState, Quote, RepayOption, SeizeOption,
    };
    use liq_types::fixed::RAY;
    use liq_types::{
        AssetId, MarketId, PositionId, PositionKey, ProtocolId, Ray, TriggerKind, Wad,
    };
    use std::collections::HashMap;

    const PROTO: ProtocolId = ProtocolId(0);
    const WEI: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
    fn bonus_5() -> Ray {
        Ray::from_raw(RAY / U256::from(20u64))
    }
    const B: SolveBudget = SolveBudget {
        max_pools: 6,
        max_iters: 64,
    };
    const GAS: GasTerms = GasTerms {
        base_fee_wei: 1,
        out_per_eth: WEI,
    };
    const H: Haircut = match Haircut::from_bps(10_000) {
        Some(h) => h,
        None => unreachable!(),
    };

    struct Mkt;
    impl MarketView for Mkt {
        fn pair_terms(&self, _: ProtocolId, _: AssetId, _: AssetId) -> Option<PairTerms> {
            Some(PairTerms {
                bonus: bonus_5(),
                coll_per_debt: Ray::from_raw(RAY),
                flash_fee_bps: 0,
                fixed_gas: 50_000,
            })
        }
        fn per_eth(&self, _: AssetId) -> Option<U256> {
            Some(e18(1))
        }
        fn notional_cap_raw(&self, _: AssetId) -> Option<U256> {
            Some(U256::MAX)
        }
    }

    fn book(pools: Vec<Pool>) -> PoolBook {
        let mut assets = HashMap::new();
        assets.insert(tok(0), A0);
        assets.insert(tok(1), A1);
        let mut b = PoolBook::new(assets, None, HOP_GAS);
        for p in pools {
            b.add(p).unwrap();
        }
        b
    }

    fn deep() -> Pool {
        v3(
            1,
            500,
            10,
            SQRT_ONE,
            &[(-887_220, 887_220, 50_000_000_000_000_000_000_000)],
        )
    }

    fn idx() -> (Vec<Box<dyn FlashSource>>, FlashIndex) {
        let srcs: Vec<Box<dyn FlashSource>> = vec![Box::new(MorphoBlue::new(
            addr(0xA0),
            &[HeldAsset {
                asset: A1,
                token: tok(1),
                balance: e18(10_000_000),
            }],
        ))];
        let mut i = FlashIndex::new(4);
        i.refresh(&srcs);
        (srcs, i)
    }

    fn health(liq: bool) -> Health {
        Health {
            hf: if liq {
                Ray::from_raw(RAY / U256::from(2u64))
            } else {
                Ray::from_raw(RAY + RAY / U256::from(20u64))
            },
            debt_value: Wad::ZERO,
            collateral_value: Wad::ZERO,
            price_sensitivity: AssetMask::EMPTY,
            state: if liq {
                HealthState::Liquidatable
            } else {
                HealthState::Healthy
            },
        }
    }

    fn q(pos: u32, repay: U256) -> Quote {
        Quote {
            position: PositionId(pos),
            key: PositionKey {
                protocol: PROTO,
                market: MarketId(0),
                user: addr(pos as u64),
            },
            repay_options: SmallVec::from_slice(&[RepayOption {
                asset: A1,
                max_repay: repay,
            }]),
            seize_options: SmallVec::from_slice(&[SeizeOption {
                asset: A0,
                max_seize: repay * U256::from(2u64),
                bonus: bonus_5(),
                curve: BonusCurve::Static { bonus: bonus_5() },
            }]),
        }
    }

    fn cfg() -> SelectCfg {
        SelectCfg {
            cost: CostModel::FEE_ONLY,
            close_bps: 0,
            exact_k: 8,
            nonce_slots: 4,
            header_gas_limit: 30_000_000,
            wrap_gas: [366_332, 355_632, 460_032, 370_435, 384_134],
            liq_gas: 80_000,
            over_borrow: U256::from(1u64),
            budget: B,
        }
    }

    fn inp<'a>(quote: &'a Quote, liq: bool, p: Ray, failed: u64) -> PositionInput<'a> {
        PositionInput {
            position: quote.position,
            protocol: PROTO,
            health: health(liq),
            quote,
            cause: TriggerKind::Stale,
            p,
            gas_success: None,
            gas_failed: failed,
        }
    }

    /// D33 / GUIDE 12 §4f: eligibility is a type-level gate. A healthy
    /// position with enormous notional never becomes [`Eligible`], so it
    /// cannot appear in a plan.
    #[test]
    fn non_liquidatable_high_net_never_enters() {
        let fat = q(1, e18(1_000_000));
        let thin = q(2, e18(10));
        let healthy = inp(&fat, false, learning_p(), 50_000);
        let liq = inp(&thin, true, learning_p(), 50_000);
        assert!(admit(&healthy).is_none(), "gate refuses Healthy");
        assert!(admit(&liq).is_some());
        assert!(!is_liquidatable(&healthy.health));
        assert!(is_liquidatable(&liq.health));

        let bk = book(vec![deep()]);
        let (_s, flash) = idx();
        let plans = select(&[healthy, liq], &cfg(), &flash, H, &bk, None, &Mkt, &GAS).unwrap();
        let ids: Vec<u32> = plans
            .iter()
            .flat_map(|p| p.groups.iter())
            .flat_map(|g| g.legs.iter())
            .map(|s| s.position.0)
            .collect();
        assert!(!ids.contains(&1), "healthy high-net never entered");
        assert!(ids.contains(&2));
    }

    /// Blocked / HF≥1 / Predicted are all outside the gate, regardless of
    /// contribution.
    #[test]
    fn gate_rejects_blocked_hf_ge_one_and_predicted() {
        let quote = q(3, e18(100));
        let mut blocked = inp(&quote, true, learning_p(), 50_000);
        blocked.health.state = HealthState::Blocked {
            reason: liq_protocol::BlockReason::Paused,
        };
        assert!(admit(&blocked).is_none());

        let mut hf1 = inp(&quote, true, learning_p(), 50_000);
        hf1.health.hf = Ray::ONE;
        hf1.health.state = HealthState::Liquidatable;
        assert!(admit(&hf1).is_none(), "HF = 1 is not liquidatable");

        let mut pred = inp(&quote, true, learning_p(), 50_000);
        pred.cause = TriggerKind::OraclePredicted;
        assert!(admit(&pred).is_none());
    }

    /// No count threshold: one liquidatable position is one plan, same
    /// path as N. `liqCount` is the group's leg count.
    #[test]
    fn single_position_is_a_batch_of_one() {
        let quote = q(4, e18(10));
        let one = inp(&quote, true, learning_p(), 50_000);
        let bk = book(vec![deep()]);
        let (_s, flash) = idx();
        let plans = select(&[one], &cfg(), &flash, H, &bk, None, &Mkt, &GAS).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].groups.len(), 1);
        assert_eq!(plans[0].groups[0].legs.len(), 1);
        assert_eq!(plans[0].groups[0].debt, A1);
        assert!(!plans[0].groups[0].cascade.groups.is_empty());
    }

    /// Lower `p` raises expected gas and cuts contrib-per-gas; a second
    /// leg that was worth adding at `p = 1` is truncated at small `p`.
    #[test]
    fn batch_shrinks_when_p_falls() {
        let a = q(10, e18(10));
        let b = q(11, e18(10));
        let bk = book(vec![deep()]);
        let (_s, flash) = idx();
        let p1 = [
            inp(&a, true, learning_p(), 50_000),
            inp(&b, true, learning_p(), 50_000),
        ];
        let full = select(&p1, &cfg(), &flash, H, &bk, None, &Mkt, &GAS).unwrap();
        let n_full: usize = full
            .iter()
            .flat_map(|p| p.groups.iter())
            .map(|g| g.legs.len())
            .sum();
        assert_eq!(n_full, 2, "both legs add at p=1");

        // p tiny: expected_gas ≈ gas_failed, Δnet of each leg is contribution*p − gas*price.
        // With base_fee_wei=1 and out_per_eth=1e18, cost_in_out(1) = 1 (wei debt per gas
        // at 1:1). expected_gas ~ 50_000, cost 50_000; contribution on 10e18 is huge, so
        // Δnet still positive. Use a huge gas_failed so Δnet ≤ 0 at low p.
        let tiny = Ray::from_raw(U256::from(1u64)); // 1 / 1e27
        let p0 = [inp(&a, true, tiny, 50_000), inp(&b, true, tiny, 50_000)];
        // Make failed-leg gas dominate: 10C-scale wrap already ~3e5; bump failed.
        let mut low = p0;
        low[0].gas_failed = 10_000_000_000_000; // 1e13
        low[1].gas_failed = 10_000_000_000_000;
        let shrunk = select(&low, &cfg(), &flash, H, &bk, None, &Mkt, &GAS).unwrap();
        let n_low: usize = shrunk
            .iter()
            .flat_map(|p| p.groups.iter())
            .map(|g| g.legs.len())
            .sum();
        assert!(n_low < n_full, "p↓ truncates: {n_low} vs {n_full}");
    }

    #[test]
    fn header_gas_limit_and_k_are_required() {
        let quote = q(1, e18(10));
        let one = inp(&quote, true, learning_p(), 50_000);
        let bk = book(vec![deep()]);
        let (_s, flash) = idx();
        let mut c = cfg();
        c.header_gas_limit = 0;
        assert!(matches!(
            select(
                std::slice::from_ref(&one),
                &c,
                &flash,
                H,
                &bk,
                None,
                &Mkt,
                &GAS
            ),
            Err(SelectError::MissingGasLimit)
        ));
        c = cfg();
        c.exact_k = 0;
        assert!(matches!(
            select(
                std::slice::from_ref(&one),
                &c,
                &flash,
                H,
                &bk,
                None,
                &Mkt,
                &GAS
            ),
            Err(SelectError::ZeroExactK)
        ));
        c = cfg();
        c.nonce_slots = 0;
        assert!(matches!(
            select(&[one], &c, &flash, H, &bk, None, &Mkt, &GAS),
            Err(SelectError::NoNonceSlots)
        ));
    }

    /// `rank` only accepts [`Eligible`]. This test is the type-level
    /// proof plus a runtime one: `select` never calls `best_plan` on a
    /// healthy input (admission list would have to contain it).
    #[test]
    fn ranking_does_not_construct_eligible() {
        let src = include_str!("select.rs");
        let code: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let needle = concat!("Elig", "ible {");
        let ctors = code.matches(needle).count();
        assert_eq!(ctors, 1, "Eligible is constructed only in admit()");
    }
}
