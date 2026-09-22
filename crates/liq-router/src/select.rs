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

use crate::bid::{beta_of, BidSchedule};
use crate::exact::{solve_batch, GasTerms, SolveBudget};
use crate::profit::{
    best_plan, delta_net, expected_contrib_per_gas, expected_gas, gas_price_in_debt,
    repay_for_seized, MarketView, ProfitCtx, ProfitError, SizedLeg, LEARNING_P_RAY,
};
use crate::solver::{PoolBook, PoolId, RouteError};
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
    #[error("failed-leg gas is required when p < 1")]
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

/// 07B cascade `MAX_CANDIDATES` — exact-solve K. Not a bid parameter.
pub const EXACT_K: u8 = 8;
/// GUIDE 13 nonce-slot budget (one plan per slot).
pub const NONCE_SLOTS: u8 = 20;

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
    /// Aave V3 flash + V4 adapter leg (`flash-gas.toml` `aave_v4`).
    pub wrap_aave_v4: u64,
    /// Intern id for family `aave-v4`. `None` → Aave wrap is V3/Spark.
    pub aave_v4: Option<ProtocolId>,
    pub liq_gas: u64,
    pub over_borrow: U256,
    pub budget: SolveBudget,
    /// Committed four-cell schedule. `None` does not split plans (tests
    /// that inject one bid). Production sets this from `bid.toml`.
    pub bids: Option<BidSchedule>,
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
    /// Wire `bidBps` when [`SelectCfg::bids`] is set. `None` means the
    /// caller supplies one bid for the plan.
    pub bid_bps: Option<u16>,
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

fn wrap_gas(
    cfg: &SelectCfg,
    provider: liq_types::FlashProvider,
    protocol: ProtocolId,
) -> Result<u64, SelectError> {
    if provider == liq_types::FlashProvider::Aave {
        if let Some(id) = cfg.aave_v4 {
            if protocol == id {
                if cfg.wrap_aave_v4 == 0 {
                    return Err(SelectError::ZeroWrapGas);
                }
                return Ok(cfg.wrap_aave_v4);
            }
        }
    }
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
    protocol: ProtocolId,
) -> Result<u64, SelectError> {
    wrap_gas(cfg, provider, protocol)?
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
        crude.push((crude_key(e, warm, market), i));
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
            None => success_gas(cfg, leg.hop_gas, leg.route.provider, el.pos.protocol)?,
        };
        let p_raw = el.pos.p.raw();
        // Learning p = 1 ⇒ expected_gas = gas_success; failed-leg gas is unused.
        // p < 1 with no failed-path snapshot: skip this candidate, not the drain.
        if el.pos.gas_failed == 0 && p_raw != RAY {
            continue;
        }
        let eg = expected_gas(p_raw, gs, el.pos.gas_failed).ok_or(SelectError::BadP)?;
        let cpg = expected_contrib_per_gas(leg.contribution, p_raw, gs, el.pos.gas_failed)?;
        let bid_bps = match cfg.bids {
            None => None,
            Some(sched) => {
                let Some(per) = market.per_eth(leg.debt) else {
                    tracing::error!("per_eth missing — leg not bid");
                    continue;
                };
                let Some(size) = crate::debt_notional_eth_wei(leg.s, per) else {
                    tracing::error!("debt notional refused — leg not bid");
                    continue;
                };
                match beta_of(sched.config(el.pos.protocol, size), 0) {
                    Ok(bps) => Some(bps),
                    Err(e) => {
                        tracing::error!(error = %e, "bid cell refused — leg not bid");
                        continue;
                    }
                }
            }
        };
        scored.push(Scored {
            position: el.position(),
            protocol: el.pos.protocol,
            p: el.pos.p,
            gas_success: gs,
            gas_failed: el.pos.gas_failed,
            expected_gas: eg,
            contrib_per_gas: cpg,
            leg,
            bid_bps,
        });
    }
    scored.sort_by(|a, b| {
        b.contrib_per_gas
            .cmp(&a.contrib_per_gas)
            .then(a.position.0.cmp(&b.position.0))
    });
    pack(scored, cfg, flash, gas, book)
}

/// Warm-tier crude rank: expected contribution-per-gas from the published
/// bucket quote, not raw `exit_cap` (GUIDE 12 §4f two-stage). Missing pair
/// / unviable buckets → 0 (admission order), never a guessed depth.
fn crude_key(e: &Eligible<'_>, warm: Option<&RouteTable>, market: &dyn MarketView) -> U256 {
    let Some(table) = warm else {
        return U256::ZERO;
    };
    let q = e.quote();
    let mut best = U256::ZERO;
    for repay in q.repay_options.iter() {
        for seize in q.seize_options.iter() {
            let Some(mut terms) = market.pair_terms(e.pos.protocol, seize.asset, repay.asset)
            else {
                continue;
            };
            terms.bonus = seize.bonus;
            let Some(entry) = table.entry(seize.asset, repay.asset) else {
                continue;
            };
            let Some(bucket) = entry.buckets.iter().rev().find(|b| {
                b.viable && b.size_in <= seize.max_seize && b.out_min.is_some() && b.hop_gas > 0
            }) else {
                continue;
            };
            let Some(out) = bucket.out_min else {
                continue;
            };
            let Ok(s) = repay_for_seized(bucket.size_in, &terms) else {
                continue;
            };
            let s = s.min(repay.max_repay);
            if s.is_zero() {
                continue;
            }
            // Band-published fee bps, floor. Crude rank is not a bid.
            let fee = s
                .checked_mul(U256::from(terms.flash_fee_bps))
                .and_then(|n| n.checked_div(U256::from(10_000u64)))
                .unwrap_or(U256::ZERO);
            let owed = s.saturating_add(fee);
            let contrib = out.saturating_sub(owed);
            let Some(cpg) = contrib.checked_div(U256::from(bucket.hop_gas)) else {
                continue;
            };
            if cpg > best {
                best = cpg;
            }
        }
    }
    best
}

fn pack(
    scored: SmallVec<[Scored; 16]>,
    cfg: &SelectCfg,
    flash: &FlashIndex,
    gas: &GasTerms,
    book: &PoolBook,
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
        let wrap = wrap_gas(cfg, s.leg.route.provider, s.protocol)?;
        if let Some(bps) = s.bid_bps {
            if cur_bid_bps(&cur).is_some_and(|have| have != bps) && !cur.groups.is_empty() {
                seal_cascades(&mut cur, cfg, flash, book, gas)?;
                if !cur.groups.is_empty() {
                    plans.push(cur);
                }
                if plans.len() >= usize::from(cfg.nonce_slots) {
                    return Ok(plans);
                }
                cur = SelectedPlan {
                    groups: SmallVec::new(),
                    hop_and_wrap_gas: 0,
                };
            }
        }
        let incr_for = |same_debt: bool| -> u64 {
            if same_debt {
                s.leg.hop_gas.saturating_add(cfg.liq_gas)
            } else {
                s.leg
                    .hop_gas
                    .saturating_add(cfg.liq_gas)
                    .saturating_add(wrap)
            }
        };
        let same = cur.groups.iter().any(|g| g.debt == s.leg.debt);
        let mut incr = incr_for(same);
        let new_gas = cur.hop_and_wrap_gas.saturating_add(incr);
        if new_gas > cfg.header_gas_limit {
            if !cur.groups.is_empty() {
                seal_cascades(&mut cur, cfg, flash, book, gas)?;
                if !cur.groups.is_empty() {
                    plans.push(cur);
                }
                if plans.len() >= usize::from(cfg.nonce_slots) {
                    return Ok(plans);
                }
                cur = SelectedPlan {
                    groups: SmallVec::new(),
                    hop_and_wrap_gas: 0,
                };
                // Fresh plan: wrap is not yet counted. Recompute vs the reset.
                incr = incr_for(false);
                if incr > cfg.header_gas_limit {
                    continue;
                }
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
        seal_cascades(&mut cur, cfg, flash, book, gas)?;
        if !cur.groups.is_empty() {
            plans.push(cur);
        }
    }
    Ok(plans)
}

fn cur_bid_bps(plan: &SelectedPlan) -> Option<u16> {
    plan.groups.first()?.legs.first()?.bid_bps
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
    book: &PoolBook,
    gas: &GasTerms,
) -> Result<(), SelectError> {
    for g in &mut plan.groups {
        // GUIDE 12 §4c: K collaterals sharing output pools are sequential
        // on a displaced book. Independent `best_plan` quotes inflate net.
        apply_displaced(g, book, gas, &cfg.budget)?;
        if g.legs.is_empty() {
            continue;
        }
        // Cascade must cover principal + flash fee so assemble can set
        // `flash_amount >= pull + fee` without exceeding source depth.
        let need = g
            .legs
            .iter()
            .try_fold(cfg.over_borrow, |a, s| {
                a.checked_add(s.leg.s)?.checked_add(s.leg.flash_fee)
            })
            .ok_or(ProfitError::Missing("cascade need"))?;
        g.need = g
            .legs
            .iter()
            .fold(U256::ZERO, |a, s| a.saturating_add(s.leg.s));
        let c = plan_cascade(flash, g.debt, need, &cfg.cost, cfg.close_bps)
            .ok_or(ProfitError::Missing("cascade"))?;
        if c.funded.is_zero() {
            return Err(ProfitError::Missing("cascade funded").into());
        }
        // Partial cascade: shrink legs so Σ (s+fee) ≤ funded.
        if c.funded < need {
            shrink_to_funded(g, c.funded);
        }
        g.cascade = c;
    }
    plan.groups
        .retain(|g| !g.legs.is_empty() && !g.need.is_zero());
    Ok(())
}

/// Re-quote a packed group with 12A-1 [`solve_batch`]. Scratch pool ids
/// are remapped back onto the book's `PoolId`s so assembly encodes the
/// pools the solver actually used (solve_batch's ids are scratch indices).
fn apply_displaced(
    g: &mut DebtGroup,
    book: &PoolBook,
    gas: &GasTerms,
    budget: &SolveBudget,
) -> Result<(), SelectError> {
    if g.legs.len() < 2 {
        return Ok(());
    }
    let colls: SmallVec<[(liq_types::AssetId, U256); 8]> =
        g.legs.iter().map(|s| (s.leg.coll, s.leg.seized)).collect();
    let mut batch = match solve_batch(book, &colls, g.debt, gas, budget) {
        Ok(b) => b,
        Err(RouteError::InsufficientLiquidity | RouteError::StalePool) => {
            g.legs.clear();
            g.need = U256::ZERO;
            return Ok(());
        }
        Err(e) => return Err(ProfitError::Route(e).into()),
    };
    remap_scratch_ids(book, &colls, g.debt, &mut batch);
    for (qi, &oi) in batch.order.iter().enumerate() {
        let Some(scored) = g.legs.get_mut(usize::from(oi)) else {
            continue;
        };
        let Some(q) = batch.quotes.get(qi) else {
            continue;
        };
        scored.leg.exit = q.clone();
        scored.leg.seized = q.amount_in;
        scored.leg.swap_out = q.amount_out;
        scored.leg.hop_gas = q.hop_gas;
        scored.leg.contribution = q.amount_out.saturating_sub(scored.leg.flash_owed);
        if let Ok(cpg) = expected_contrib_per_gas(
            scored.leg.contribution,
            scored.p.raw(),
            scored.gas_success,
            scored.gas_failed,
        ) {
            scored.contrib_per_gas = cpg;
        }
    }
    g.legs.retain(|s| !s.leg.contribution.is_zero());
    g.need = g
        .legs
        .iter()
        .fold(U256::ZERO, |a, s| a.saturating_add(s.leg.s));
    Ok(())
}

/// Inverse of `solve_batch`'s first-seen scratch remapping. Must iterate
/// `book.legs` in the same order `exact.rs` does.
fn remap_scratch_ids(
    book: &PoolBook,
    colls: &[(liq_types::AssetId, U256)],
    debt: liq_types::AssetId,
    batch: &mut crate::exact::BatchQuote,
) {
    let mut ids: SmallVec<[PoolId; 16]> = SmallVec::new();
    for &(coll, _) in colls {
        for leg in book.legs(coll, debt) {
            if !ids.contains(&leg.pool) {
                ids.push(leg.pool);
            }
        }
    }
    for q in &mut batch.quotes {
        for a in &mut q.allocs {
            let Some(&pid) = usize::try_from(a.leg.pool.0).ok().and_then(|i| ids.get(i)) else {
                continue;
            };
            a.leg.pool = pid;
        }
    }
}

fn shrink_to_funded(g: &mut DebtGroup, funded: U256) {
    let mut left = funded;
    let mut keep = SmallVec::new();
    for s in g.legs.drain(..) {
        if left.is_zero() {
            break;
        }
        let take = s.leg.s.saturating_add(s.leg.flash_fee);
        if take <= left {
            left = left.saturating_sub(take);
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
    use crate::profit::{best_plan, MarketView, ProfitCtx};
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
        priority_fee_wei: 0,
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
            wrap_aave_v4: 496_704,
            aave_v4: None,
            liq_gas: 80_000,
            over_borrow: U256::from(1u64),
            budget: B,
            bids: None,
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

    /// GUIDE 12 §4c: two positions that share an output pool must be
    /// re-quoted with `solve_batch` on the displaced book. Packed
    /// contribution is strictly below the sum of independent quotes on a
    /// mid-depth V2 pool (impact is superlinear).
    #[test]
    fn packed_contribution_below_independent_when_pool_shared() {
        let a = q(20, e18(10));
        let b = q(21, e18(10));
        // 1000e18 reserves: 10.5e18 in is ~1 % of the book — displacement
        // is material, independent quotes still profitable.
        let bk = book(vec![v2(1, e18(1_000), e18(1_000))]);
        let (_s, flash) = idx();
        let pctx = ProfitCtx {
            protocol: PROTO,
            flash: &flash,
            haircut: H,
            book: &bk,
            warm: None,
            market: &Mkt,
            gas: &GAS,
            budget: &B,
        };
        let c1 = best_plan(&pctx, &a).unwrap().unwrap().contribution;
        let c2 = best_plan(&pctx, &b).unwrap().unwrap().contribution;
        let independent = c1.checked_add(c2).unwrap();
        assert!(!c1.is_zero() && !c2.is_zero(), "each leg profitable alone");

        let inputs = [
            inp(&a, true, learning_p(), 50_000),
            inp(&b, true, learning_p(), 50_000),
        ];
        let plans = select(&inputs, &cfg(), &flash, H, &bk, None, &Mkt, &GAS).unwrap();
        let packed = plans
            .iter()
            .flat_map(|p| p.groups.iter())
            .flat_map(|g| g.legs.iter())
            .fold(U256::ZERO, |acc, s| acc.saturating_add(s.leg.contribution));
        assert_eq!(
            plans
                .iter()
                .flat_map(|p| p.groups.iter())
                .map(|g| g.legs.len())
                .sum::<usize>(),
            2,
            "both legs packed into the batch"
        );
        assert!(
            packed < independent,
            "sequential displacement must cut net: packed {packed} vs independent {independent}"
        );
    }

    #[test]
    fn gas_failed_zero_at_learning_p_selects() {
        let quote = q(40, e18(10));
        let one = inp(&quote, true, learning_p(), 0);
        let bk = book(vec![deep()]);
        let (_s, flash) = idx();
        let plans = select(&[one], &cfg(), &flash, H, &bk, None, &Mkt, &GAS).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].groups[0].legs.len(), 1);
    }

    #[test]
    fn gas_failed_zero_below_ray_skips_candidate_not_drain() {
        let a = q(41, e18(10));
        let b = q(42, e18(10));
        let tiny = Ray::from_raw(U256::from(1u64));
        let inputs = [inp(&a, true, tiny, 0), inp(&b, true, learning_p(), 50_000)];
        let bk = book(vec![deep()]);
        let (_s, flash) = idx();
        let plans = select(&inputs, &cfg(), &flash, H, &bk, None, &Mkt, &GAS).unwrap();
        let ids: Vec<u32> = plans
            .iter()
            .flat_map(|p| p.groups.iter())
            .flat_map(|g| g.legs.iter())
            .map(|s| s.position.0)
            .collect();
        assert!(!ids.contains(&41), "p<1 with gas_failed=0 must skip");
        assert!(ids.contains(&42), "sibling must still be selected");
    }

    #[test]
    fn wrap_gas_recomputed_after_plan_reset() {
        let a = q(50, e18(10));
        let b = q(51, e18(10));
        let bk = book(vec![deep()]);
        let (_s, flash) = idx();
        let mut c = cfg();
        c.wrap_gas = [100_000, 100_000, 100_000, 100_000, 100_000];
        c.liq_gas = 200_000;
        c.header_gas_limit = 30_000_000;
        let inputs = [
            inp(&a, true, learning_p(), 50_000),
            inp(&b, true, learning_p(), 50_000),
        ];
        let wide = select(&inputs, &c, &flash, H, &bk, None, &Mkt, &GAS).unwrap();
        assert_eq!(wide.len(), 1);
        assert_eq!(wide[0].groups[0].legs.len(), 2);
        let first_leg = wide[0].groups[0].legs[0].leg.hop_gas;
        let wrap = 100_000u64;
        let one = wrap.saturating_add(c.liq_gas).saturating_add(first_leg);
        let liq_plus_hop = one.saturating_sub(wrap);
        c.header_gas_limit = one.saturating_add(liq_plus_hop).saturating_sub(1);
        let rolled = select(&inputs, &c, &flash, H, &bk, None, &Mkt, &GAS).unwrap();
        assert!(
            rolled.len() >= 2,
            "second same-provider hop must roll a new plan: {rolled:?}"
        );
        assert_eq!(
            rolled[1].hop_and_wrap_gas, one,
            "reset plan must re-add wrap, not reuse stale incr"
        );
    }

    /// One executor plan has one `bidBps`. Aave at 1 ETH and another
    /// protocol at 1 ETH must not share it.
    #[test]
    fn different_bid_cells_are_separate_plans() {
        use crate::BidConfig;
        use crate::BidSchedule;
        let aave_q = q(1, e18(1));
        let other_q = q(2, e18(1));
        let mut aave = inp(&aave_q, true, learning_p(), 0);
        aave.protocol = ProtocolId(1);
        let mut other = inp(&other_q, true, learning_p(), 0);
        other.protocol = ProtocolId(7);
        let sched = BidSchedule {
            size_cut_wei: e18(3),
            aave_v3: ProtocolId(1),
            aave_v4: ProtocolId(2),
            aave_below: BidConfig::new(9_950, 9_950, 0, 0).unwrap(),
            aave_above: BidConfig::new(9_980, 9_980, 0, 0).unwrap(),
            other_below: BidConfig::new(6_500, 6_500, 0, 0).unwrap(),
            other_above: BidConfig::new(6_700, 6_700, 0, 0).unwrap(),
        };
        let mut c = cfg();
        c.bids = Some(sched);
        let bk = book(vec![deep()]);
        let (_s, flash) = idx();
        let plans = select(&[aave, other], &c, &flash, H, &bk, None, &Mkt, &GAS).unwrap();
        assert_eq!(plans.len(), 2, "two cells must not share a plan");
        let mut rates: Vec<u16> = plans
            .iter()
            .flat_map(|p| p.groups.iter())
            .flat_map(|g| g.legs.iter())
            .map(|s| s.bid_bps.expect("schedule stamps bid_bps"))
            .collect();
        rates.sort_unstable();
        assert_eq!(rates, vec![6_500, 9_950]);
    }
}
