//! Sizing and the profit model (GUIDE 12 §1, §2, §4; D45).
//!
//! `size = min4(max_repay, flash_after_haircut, route_depth, band.max_size)`.
//! A binding flash or route ceiling is a **partial**, not a skip. The
//! viability band (GUIDE 12 §4b) is the sole size filter: no band, or a
//! fitted size below `band.min_size`, and the leg is not taken.
//!
//! Combinations are `repay × seize × source`, evaluated **serially** on
//! the hot thread (GUIDE 12 §3b). 12A-1 forbids `thread::spawn` /
//! `thread::scope` / `rayon` in this crate; twelve exact quotes are
//! cheaper than twelve clones.
//!
//! Fee and impact are charged on **seized** (`exact_quote(seized)`), the
//! flash fee on `s`, gas at plan level (not in the per-leg contribution).
//! This is the 07B `net_of_bonus` revisit: the ranking key is realised
//! `swap_out − flash_owed`, not `bonus − fee_bps`.

use alloy_primitives::U256;
use liq_flash::{fee_amount, FlashIndex, Haircut, SourceEntry};
use liq_protocol::{FlashRoute, LegChoice, Quote};
use liq_types::fixed::{mul_div, Rounding, RAY};
use liq_types::{AssetId, ProtocolId};

use crate::band::{PairTerms, ViabilityBand};
use crate::exact::{solve_pair, ExitQuote, GasTerms, SolveBudget};
use crate::solver::{mul_div_512, PoolBook, RouteError};
use crate::warm::RouteTable;

/// Learning-phase `p = 1` (GUIDE 12 §4f). 12B replaces this with a fitted
/// per-bracket value; this crate does not invent one.
pub const LEARNING_P_RAY: U256 = RAY;

/// Why a combination was refused. A failed combination is "unavailable",
/// never a panic (GUIDE 12 §3b).
#[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProfitError {
    #[error("required input missing: {0}")]
    Missing(&'static str),
    #[error("route: {0}")]
    Route(#[from] RouteError),
    #[error("flash fee unpriceable for this source")]
    UnpriceableFee,
    #[error("historical archive unavailable (WP 11 / 05B D60); profit parity is not invented")]
    ArchiveUnavailable,
}

/// Lookups the profit model cannot default.
pub trait MarketView {
    fn pair_terms(&self, protocol: ProtocolId, coll: AssetId, debt: AssetId) -> Option<PairTerms>;
    /// Raw units of `asset` per `1e18` wei (`WarmInputs::per_eth`).
    fn per_eth(&self, asset: AssetId) -> Option<U256>;
    /// The pair's viability band at this block (GUIDE 12 §4b): the debt
    /// sizes, in raw debt units, for which `net ≥ 0`. The sole source of
    /// truth for sizing — `None` means no viable size and the leg is not
    /// taken. Keyed per `(protocol, coll, debt)`, never per debt alone:
    /// one debt asset has a different band against every collateral.
    fn band(&self, protocol: ProtocolId, coll: AssetId, debt: AssetId) -> Option<ViabilityBand>;
}

/// Inputs sized once per block / candidate drain.
pub struct ProfitCtx<'a> {
    pub protocol: ProtocolId,
    pub flash: &'a FlashIndex,
    pub haircut: Haircut,
    pub book: &'a PoolBook,
    pub warm: Option<&'a RouteTable>,
    pub market: &'a dyn MarketView,
    pub gas: &'a GasTerms,
    pub budget: &'a SolveBudget,
}

/// One evaluated `(repay, seize, source)` triple.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SizedLeg {
    pub choice: LegChoice,
    pub debt: AssetId,
    pub coll: AssetId,
    /// Sized repay (principal flashed, before fee). Binding constraint may
    /// be below `max_repay` — that is the partial.
    pub s: U256,
    pub seized: U256,
    /// Exact quote of `seized` (fee + impact on seized, not on `s`).
    pub swap_out: U256,
    pub flash_fee: U256,
    /// `s + flash_fee`.
    pub flash_owed: U256,
    /// `swap_out − flash_owed`. **No gas.** Ranking numerator (D45).
    pub contribution: U256,
    pub hop_gas: u64,
    pub route: FlashRoute,
    pub exit: ExitQuote,
    pub terms: PairTerms,
}

/// `min` of four ceilings. Every argument is a real ceiling; passing
/// `U256::MAX` is only valid when the caller explicitly has no cap.
#[inline]
#[must_use]
pub fn min4(a: U256, b: U256, c: U256, d: U256) -> U256 {
    a.min(b).min(c).min(d)
}

/// `s · (1 + bonus) · coll_per_debt`, floor. Same identity the band uses;
/// implemented here with public `mul_div_512` so 12A-1 `band.rs` stays
/// untouched.
pub fn seized_for(s: U256, t: &PairTerms) -> Result<U256, ProfitError> {
    if t.coll_per_debt.raw().is_zero() {
        return Err(ProfitError::Missing("coll_per_debt"));
    }
    let one_plus = RAY.checked_add(t.bonus.raw()).ok_or(RouteError::Math)?;
    let eq = mul_div_512(s, one_plus, RAY)?;
    Ok(mul_div_512(eq, t.coll_per_debt.raw(), RAY)?)
}

/// Inverse of [`seized_for`], floor: largest `s` whose seized amount is
/// `≤ seized`. `None`/`Err` when the oracle ratio is zero.
pub fn repay_for_seized(seized: U256, t: &PairTerms) -> Result<U256, ProfitError> {
    let one_plus = RAY.checked_add(t.bonus.raw()).ok_or(RouteError::Math)?;
    let den = mul_div_512(one_plus, t.coll_per_debt.raw(), RAY)?;
    if den.is_zero() {
        return Err(ProfitError::Missing("coll_per_debt"));
    }
    Ok(mul_div_512(seized, RAY, den)?)
}

/// Haircut-applied depth of one source, in debt raw units.
#[inline]
#[must_use]
pub fn available_after_haircut(e: &SourceEntry, haircut: Haircut) -> U256 {
    haircut.apply(e.available)
}

/// Warm-tier route-depth ceiling in **debt** units: the largest repay `s`
/// whose seized size still fits the published `exit_cap` (impact-bounded
/// coll units). Missing pair → `Missing`, never a guessed depth.
pub fn route_depth_repay(
    warm: &RouteTable,
    coll: AssetId,
    terms: &PairTerms,
) -> Result<U256, ProfitError> {
    let cap = warm.exit_cap(coll);
    if cap.is_zero() {
        return Ok(U256::ZERO);
    }
    repay_for_seized(cap, terms)
}

/// Shrink `s` until `solve_pair(seized(s))` succeeds. Partial beats skip:
/// a smaller `s` is returned rather than `InsufficientLiquidity` at the
/// top of the range. `Ok(0)` means no positive size fits.
fn fit_size(
    book: &PoolBook,
    coll: AssetId,
    debt: AssetId,
    s_max: U256,
    terms: &PairTerms,
    gas: &GasTerms,
    budget: &SolveBudget,
) -> Result<U256, ProfitError> {
    if s_max.is_zero() {
        return Ok(U256::ZERO);
    }
    if try_quote(book, coll, debt, s_max, terms, gas, budget)?.is_some() {
        return Ok(s_max);
    }
    let mut lo = U256::ZERO;
    let mut hi = s_max;
    for _ in 0..80 {
        let span = match hi.checked_sub(lo) {
            Some(s) if s > U256::from(1u64) => s,
            _ => break,
        };
        let mid = lo
            .checked_add(span.wrapping_shr(1))
            .ok_or(RouteError::Math)?;
        if try_quote(book, coll, debt, mid, terms, gas, budget)?.is_some() {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

fn try_quote(
    book: &PoolBook,
    coll: AssetId,
    debt: AssetId,
    s: U256,
    terms: &PairTerms,
    gas: &GasTerms,
    budget: &SolveBudget,
) -> Result<Option<ExitQuote>, ProfitError> {
    if s.is_zero() {
        return Ok(None);
    }
    let seized = seized_for(s, terms)?;
    if seized.is_zero() {
        return Ok(None);
    }
    match solve_pair(book, coll, debt, seized, gas, budget) {
        Ok(q) => Ok(Some(q)),
        Err(RouteError::InsufficientLiquidity | RouteError::StalePool) => Ok(None),
        Err(e) => Err(ProfitError::Route(e)),
    }
}

fn flash_fee(route: &FlashRoute, s: U256) -> Result<U256, ProfitError> {
    fee_amount(route.provider, s, route.fee_bps).ok_or(ProfitError::UnpriceableFee)
}

/// Evaluate one triple. `None` = unavailable (unfundable, unroutable,
/// or `swap_out < flash_owed`).
pub fn evaluate(
    ctx: &ProfitCtx<'_>,
    q: &Quote,
    choice: LegChoice,
    entry: &SourceEntry,
) -> Result<Option<SizedLeg>, ProfitError> {
    let repay = q
        .repay_options
        .get(usize::from(choice.repay))
        .ok_or(ProfitError::Missing("repay option"))?;
    let seize = q
        .seize_options
        .get(usize::from(choice.seize))
        .ok_or(ProfitError::Missing("seize option"))?;
    if !repay.pairs_with(choice.seize) {
        return Ok(None);
    }
    let mut terms = ctx
        .market
        .pair_terms(ctx.protocol, seize.asset, repay.asset)
        .ok_or(ProfitError::Missing("pair_terms"))?;
    // The exit solve outputs debt units, so hop gas must be priced in this
    // leg's debt, not the plan's WETH numeraire: with the identity, a USDC
    // leg would see hop gas ~1e12x too dear and a DAI leg ~3000x too cheap.
    let per = ctx
        .market
        .per_eth(repay.asset)
        .filter(|p| !p.is_zero())
        .ok_or(ProfitError::Missing("per_eth"))?;
    let leg_gas = GasTerms {
        out_per_eth: per,
        ..*ctx.gas
    };
    // Seize-option bonus is authoritative (D26 / GUIDE 01). Overlay so two
    // seize legs of one pair with different e-mode bonuses stay distinct.
    terms.bonus = seize.bonus;
    let Some(band) = ctx.market.band(ctx.protocol, seize.asset, repay.asset) else {
        return Ok(None);
    };
    let cap = band.max_size;
    let flash_cap = available_after_haircut(entry, ctx.haircut);
    let route_cap = match ctx.warm {
        Some(w) => route_depth_repay(w, seize.asset, &terms)?,
        None => repay.max_repay, // exact fit_size below is the real ceiling
    };
    // GUIDE 01 `max_seize` is a real ceiling: close-factor `max_repay` can
    // demand more coll than the position holds. Fold into the first min4
    // argument so the WP signature stays four-wide.
    let seize_cap = repay_for_seized(seize.max_seize, &terms)?;
    let protocol_cap = repay.max_repay.min(seize_cap);
    let s0 = min4(protocol_cap, flash_cap, route_cap, cap);
    let s = fit_size(
        ctx.book,
        seize.asset,
        repay.asset,
        s0,
        &terms,
        &leg_gas,
        ctx.budget,
    )?;
    // Below the band's lower edge gas dominates: not a partial, a skip.
    // Below the protocol's own minimum (an all-or-nothing leg) the call
    // would revert: also a skip, never a shrunken size.
    if s.is_zero() || s < band.min_size || s < repay.min_repay {
        return Ok(None);
    }
    let Some(exit) = try_quote(
        ctx.book,
        seize.asset,
        repay.asset,
        s,
        &terms,
        &leg_gas,
        ctx.budget,
    )?
    else {
        return Ok(None);
    };
    let route = entry.route(repay.asset, s);
    let fee = flash_fee(&route, s)?;
    let owed = s.checked_add(fee).ok_or(RouteError::Math)?;
    let contribution = match exit.amount_out.checked_sub(owed) {
        Some(c) if !c.is_zero() => c,
        _ => return Ok(None),
    };
    Ok(Some(SizedLeg {
        choice,
        debt: repay.asset,
        coll: seize.asset,
        s,
        seized: exit.amount_in,
        swap_out: exit.amount_out,
        flash_fee: fee,
        flash_owed: owed,
        contribution,
        hop_gas: exit.hop_gas,
        route,
        exit,
        terms,
    }))
}

/// Serial joint search. The winner maximises **contribution**
/// (`swap_out − flash_owed`), not min exit-cost (GUIDE 12 §4c / D26).
/// Ties keep quote preference order (lower repay index, then seize, then
/// index order of sources).
pub fn best_plan(ctx: &ProfitCtx<'_>, q: &Quote) -> Result<Option<SizedLeg>, ProfitError> {
    let mut best: Option<SizedLeg> = None;
    for (ri, repay) in q.repay_options.iter().enumerate() {
        let Ok(ri) = u8::try_from(ri) else {
            continue;
        };
        let sources = ctx.flash.entries(repay.asset);
        if sources.is_empty() {
            continue;
        }
        for (si, _) in q.seize_options.iter().enumerate() {
            let Ok(si) = u8::try_from(si) else {
                continue;
            };
            if !repay.pairs_with(si) {
                continue;
            }
            let choice = LegChoice {
                repay: ri,
                seize: si,
            };
            for e in sources {
                match evaluate(ctx, q, choice, e) {
                    Ok(Some(leg)) => {
                        let better = best
                            .as_ref()
                            .is_none_or(|b| leg.contribution > b.contribution);
                        if better {
                            best = Some(leg);
                        }
                    }
                    Ok(None) => {}
                    Err(ProfitError::Missing(_) | ProfitError::UnpriceableFee) => {
                        tracing::debug!(repay = ri, seize = si, "combo unavailable");
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }
    Ok(best)
}

/// Pre-gas expected contribution-per-gas (GUIDE 12 §4f).
///
/// `expected_gas = p · gas_success + (1 − p) · gas_failed`
/// `expected_contrib_per_gas = p · contribution / expected_gas`
///
/// `p` is a RAY fraction (`LEARNING_P_RAY` = 1). Numerator is pre-gas
/// contribution. Truncation is [`delta_net`], which subtracts gas once.
/// For a common `p` and a common wei-per-gas, `(contribution − gas) /
/// expected_gas` differs from this ratio by a constant and ranks the
/// same; the forms stop being order-identical once `gas_failed > 0`
/// makes `expected_gas` not proportional to `p`.
pub fn expected_contrib_per_gas(
    contribution: U256,
    p_ray: U256,
    gas_success: u64,
    gas_failed: u64,
) -> Result<U256, ProfitError> {
    if p_ray > RAY {
        return Err(ProfitError::Missing("p > 1"));
    }
    let gs = U256::from(gas_success);
    let gf = U256::from(gas_failed);
    let one_m = RAY.checked_sub(p_ray).ok_or(RouteError::Math)?;
    let failed_term = one_m.checked_mul(gf).ok_or(RouteError::Math)?;
    let eg = p_ray
        .checked_mul(gs)
        .and_then(|a| a.checked_add(failed_term))
        .ok_or(RouteError::Math)?
        .checked_div(RAY)
        .ok_or(RouteError::Math)?;
    if eg.is_zero() {
        return Err(ProfitError::Missing("expected_gas"));
    }
    let numer = p_ray
        .checked_mul(contribution)
        .and_then(|n| n.checked_div(RAY))
        .ok_or(RouteError::Math)?;
    numer
        .checked_div(eg)
        .ok_or(ProfitError::Missing("contrib/gas"))
}

/// Marginal Δ`net_bundle_profit` of adding a leg (GUIDE 12 §4f):
/// `p · contribution − expected_gas · base_fee_in_debt`.
/// `None` on overflow. `Some(0)` or under → truncate.
pub fn delta_net(
    contribution: U256,
    p_ray: U256,
    expected_gas: u64,
    gas_cost_in_debt: U256,
) -> Result<Option<U256>, ProfitError> {
    if p_ray > RAY {
        return Err(ProfitError::Missing("p > 1"));
    }
    let exp_c = p_ray
        .checked_mul(contribution)
        .and_then(|n| n.checked_div(RAY))
        .ok_or(RouteError::Math)?;
    let cost = U256::from(expected_gas)
        .checked_mul(gas_cost_in_debt)
        .ok_or(RouteError::Math)?;
    Ok(exp_c.checked_sub(cost))
}

/// `gas_success` *per wei of gas* in the debt asset: `base_fee · debt_per_eth / 1e18`.
pub fn gas_price_in_debt(gas: &GasTerms) -> Result<U256, ProfitError> {
    Ok(gas.cost_in_out(1)?)
}

/// Expected gas units for one attempt.
#[must_use]
pub fn expected_gas(p_ray: U256, gas_success: u64, gas_failed: u64) -> Option<u64> {
    if p_ray > RAY {
        return None;
    }
    let gs = U256::from(gas_success);
    let gf = U256::from(gas_failed);
    let one_m = RAY.checked_sub(p_ray)?;
    let eg = p_ray
        .checked_mul(gs)
        .and_then(|a| a.checked_add(one_m.checked_mul(gf)?))?
        .checked_div(RAY)?;
    u64::try_from(eg).ok()
}

/// WP 11 D60 / 12A-2 "profit within 2% on 100 replays": the archive is
/// not on disk. This is the fail-closed seam — it does not invent fills.
#[inline]
pub fn historical_profit_parity() -> Result<(), ProfitError> {
    Err(ProfitError::ArchiveUnavailable)
}

/// Haircut as a 90 % default is **not** provided. Callers pass a
/// constructed [`Haircut`]. Helper for tests and wiring that already
/// have a bps figure.
#[inline]
#[must_use]
pub fn haircut_bps(bps: u16) -> Option<Haircut> {
    Haircut::from_bps(bps)
}

/// Planable 99 % buffer, same identity as 07B (exposed so assembly can
/// cap over-borrow without depending on `liq_flash::planable` name).
#[inline]
#[must_use]
pub fn planable(available: U256) -> U256 {
    mul_div(
        available,
        U256::from(99u64),
        U256::from(100u64),
        Rounding::Down,
    )
    .unwrap_or(U256::ZERO)
}

/// Over-borrow headroom: `s + extra`, capped at `planable(available)`.
pub fn flash_with_over_borrow(s: U256, extra: U256, available: U256) -> Option<U256> {
    let want = s.checked_add(extra)?;
    Some(
        want.min(planable(available))
            .max(s.min(planable(available))),
    )
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
    use crate::exact::GasTerms;
    use crate::fixtures::*;
    use crate::solver::{Pool, PoolBook};
    use alloy_primitives::{Address, U256};
    use liq_flash::{FlashIndex, FlashSource, HeldAsset, MorphoBlue};
    use liq_protocol::{BonusCurve, LegChoice, Quote, RepayOption, SeizeOption};
    use liq_types::{AssetId, MarketId, PositionId, PositionKey, ProtocolId, Ray};
    use std::collections::HashMap;

    const PROTO: ProtocolId = ProtocolId(0);
    /// 5 % RAY = RAY / 20. Not a `const` — `U256` div is not `const`.
    fn bonus_5() -> Ray {
        Ray::from_raw(RAY / U256::from(20u64))
    }

    struct Mkt {
        terms: PairTerms,
        cap: U256,
        min_size: U256,
        per_eth: U256,
    }
    impl MarketView for Mkt {
        fn pair_terms(&self, _: ProtocolId, _: AssetId, _: AssetId) -> Option<PairTerms> {
            Some(self.terms)
        }
        fn per_eth(&self, _: AssetId) -> Option<U256> {
            Some(self.per_eth)
        }
        fn band(
            &self,
            _: ProtocolId,
            _: AssetId,
            _: AssetId,
        ) -> Option<crate::band::ViabilityBand> {
            Some(crate::band::ViabilityBand {
                min_size: self.min_size,
                max_size: self.cap,
                base_fee: 0,
                block: 0,
            })
        }
    }

    fn terms() -> PairTerms {
        PairTerms {
            bonus: bonus_5(),
            coll_per_debt: Ray::from_raw(RAY),
            flash_fee_bps: 0,
            fixed_gas: 50_000,
        }
    }

    fn mkt(cap: U256) -> Mkt {
        Mkt {
            terms: terms(),
            cap,
            min_size: U256::ZERO,
            per_eth: e18(1),
        }
    }

    fn book(pools: Vec<Pool>) -> PoolBook {
        let mut assets = HashMap::new();
        assets.insert(tok(0), A0);
        assets.insert(tok(1), A1);
        assets.insert(tok(2), A2);
        let mut b = PoolBook::new(assets, None, HOP_GAS);
        for p in pools {
            b.add(p).unwrap();
        }
        b
    }

    fn deep_v3() -> Pool {
        v3(
            1,
            500,
            10,
            SQRT_ONE,
            &[(-887_220, 887_220, 50_000_000_000_000_000_000_000)],
        )
    }

    fn flash_morpho(bal: U256) -> (Vec<Box<dyn FlashSource>>, FlashIndex) {
        let srcs: Vec<Box<dyn FlashSource>> = vec![Box::new(MorphoBlue::new(
            addr(0xA0),
            &[HeldAsset {
                asset: A1,
                token: tok(1),
                balance: bal,
            }],
        ))];
        let mut idx = FlashIndex::new(4);
        idx.refresh(&srcs);
        (srcs, idx)
    }

    fn quote(repay: &[(AssetId, U256)], seize: &[(AssetId, U256, Ray)]) -> Quote {
        Quote {
            position: PositionId(1),
            key: PositionKey {
                protocol: PROTO,
                market: MarketId(0),
                user: Address::ZERO,
            },
            repay_options: repay
                .iter()
                .map(|&(asset, max_repay)| RepayOption {
                    min_repay: alloy_primitives::U256::ZERO,
                    pair_seize: None,
                    asset,
                    max_repay,
                    slot: liq_protocol::SlotRef::ByAsset,
                })
                .collect(),
            seize_options: seize
                .iter()
                .map(|&(asset, max_seize, bonus)| SeizeOption {
                    asset,
                    max_seize,
                    bonus,
                    curve: BonusCurve::Static { bonus },
                    call_target: Address::ZERO,
                    slot: liq_protocol::SlotRef::ByAsset,
                })
                .collect(),
        }
    }

    const WEI: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
    const FREE: GasTerms = GasTerms {
        base_fee_wei: 0,
        priority_fee_wei: 0,
        out_per_eth: WEI,
    };
    const B: SolveBudget = SolveBudget {
        max_pools: 6,
        max_iters: 64,
    };
    const H: Haircut = match Haircut::from_bps(10_000) {
        Some(h) => h,
        None => unreachable!(),
    };

    fn ctx<'a>(
        flash: &'a FlashIndex,
        book: &'a PoolBook,
        market: &'a dyn MarketView,
        gas: &'a GasTerms,
    ) -> ProfitCtx<'a> {
        ProfitCtx {
            protocol: PROTO,
            flash,
            haircut: H,
            book,
            warm: None,
            market,
            gas,
            budget: &B,
        }
    }

    /// Independent oracle: `seized = s · 1.05` at 1:1 coll_per_debt.
    #[test]
    fn seized_is_s_times_one_plus_bonus() {
        let t = terms();
        assert_eq!(seized_for(e18(100), &t).unwrap(), e18(105));
        assert_eq!(repay_for_seized(e18(105), &t).unwrap(), e18(100));
        let mut z = t;
        z.coll_per_debt = Ray::ZERO;
        assert!(seized_for(e18(1), &z).is_err());
    }

    /// Size is the min of four ceilings; a binding flash depth is a
    /// partial, not a skip.
    #[test]
    fn min4_partial_beats_skip() {
        assert_eq!(
            min4(e18(100), e18(20), e18(50), e18(1_000)),
            e18(20),
            "flash binds"
        );
        let bk = book(vec![deep_v3()]);
        let (_s, idx) = flash_morpho(e18(20));
        let q = quote(&[(A1, e18(100))], &[(A0, e18(200), bonus_5())]);
        let market = mkt(U256::MAX);
        let c = ctx(&idx, &bk, &market, &FREE);
        let leg = best_plan(&c, &q).unwrap().unwrap();
        assert_eq!(leg.s, e18(20), "took the flash-bound partial");
        assert!(leg.s < e18(100));
        assert!(!leg.contribution.is_zero());
    }

    /// The viability band is the sole size filter: `max_size` caps the
    /// size, a fitted size below `min_size` is a skip, and a pair with no
    /// band is not taken at all.
    #[test]
    fn band_caps_size_rejects_below_floor_and_is_required() {
        let bk = book(vec![deep_v3()]);
        let (_s, idx) = flash_morpho(e18(10_000));
        let q = quote(&[(A1, e18(100))], &[(A0, e18(200), bonus_5())]);

        let capped = mkt(e18(30));
        let leg = best_plan(&ctx(&idx, &bk, &capped, &FREE), &q)
            .unwrap()
            .unwrap();
        assert_eq!(leg.s, e18(30), "band max_size binds");

        let mut floored = mkt(U256::MAX);
        floored.min_size = e18(1_000);
        assert!(
            best_plan(&ctx(&idx, &bk, &floored, &FREE), &q)
                .unwrap()
                .is_none(),
            "a size below the band floor is a skip, not a partial"
        );

        struct NoBand(Mkt);
        impl MarketView for NoBand {
            fn pair_terms(&self, p: ProtocolId, c: AssetId, d: AssetId) -> Option<PairTerms> {
                self.0.pair_terms(p, c, d)
            }
            fn per_eth(&self, a: AssetId) -> Option<U256> {
                self.0.per_eth(a)
            }
            fn band(&self, _: ProtocolId, _: AssetId, _: AssetId) -> Option<ViabilityBand> {
                None
            }
        }
        let none = NoBand(mkt(U256::MAX));
        assert!(
            best_plan(&ctx(&idx, &bk, &none, &FREE), &q)
                .unwrap()
                .is_none(),
            "no band, no leg"
        );
    }

    /// All-or-nothing legs (`min_repay == max_repay`, Gearbox full) are
    /// skipped — never shrunk — when a ceiling binds below them; a repay
    /// option only combines with its paired seize option.
    #[test]
    fn all_or_nothing_leg_is_skipped_not_shrunk_and_pairs_hold() {
        let bk = book(vec![deep_v3()]);
        let (_s, idx) = flash_morpho(e18(10_000));
        let mut q = quote(&[(A1, e18(100))], &[(A0, e18(200), bonus_5())]);
        q.repay_options[0].min_repay = e18(100);

        let open = mkt(U256::MAX);
        let leg = best_plan(&ctx(&idx, &bk, &open, &FREE), &q)
            .unwrap()
            .unwrap();
        assert_eq!(leg.s, e18(100), "whole leg when nothing binds");

        let capped = mkt(e18(30));
        assert!(
            best_plan(&ctx(&idx, &bk, &capped, &FREE), &q)
                .unwrap()
                .is_none(),
            "band cap below the all-or-nothing size is a skip"
        );

        // Two seize options; the only repay option pairs with the second.
        let mut p = quote(
            &[(A1, e18(10))],
            &[
                (
                    A0,
                    e18(200),
                    Ray::from_raw(bonus_5().raw() * U256::from(2u8)),
                ),
                (A0, e18(200), bonus_5()),
            ],
        );
        p.repay_options[0].pair_seize = Some(1);
        let leg = best_plan(&ctx(&idx, &bk, &open, &FREE), &p)
            .unwrap()
            .unwrap();
        assert_eq!(
            leg.choice.seize, 1,
            "paired seize option, not the richer one"
        );
    }

    /// `SeizeOption.max_seize` binds when `max_repay` would seize more
    /// coll than the position holds. Oracle: `repay_for_seized(max_seize)`
    /// at 5 % bonus, 1:1 coll_per_debt.
    #[test]
    fn max_seize_caps_size_when_max_repay_demands_more_coll() {
        let bk = book(vec![deep_v3()]);
        let (_s, idx) = flash_morpho(e18(10_000));
        // max_repay 100 → seized 105 at 5 %. max_seize 50 binds.
        let q = quote(&[(A1, e18(100))], &[(A0, e18(50), bonus_5())]);
        let market = mkt(U256::MAX);
        let c = ctx(&idx, &bk, &market, &FREE);
        let leg = best_plan(&c, &q).unwrap().unwrap();
        let cap = repay_for_seized(e18(50), &terms()).unwrap();
        assert_eq!(leg.s, cap, "size is repay_for_seized(max_seize)");
        assert!(leg.s < e18(100), "max_repay did not win");
        assert!(leg.seized <= e18(50), "must not seize above max_seize");
        assert_eq!(leg.seized, seized_for(leg.s, &terms()).unwrap());
    }

    /// Fee + impact charged on seized, not on `s`. Independent oracle =
    /// `pool.quote_exact_in(seized)`.
    #[test]
    fn fee_and_impact_on_seized_not_on_s() {
        let p = deep_v3();
        let bk = book(vec![p.clone()]);
        let (_s, idx) = flash_morpho(e18(10_000));
        let q = quote(&[(A1, e18(100))], &[(A0, e18(200), bonus_5())]);
        let market = mkt(U256::MAX);
        let c = ctx(&idx, &bk, &market, &FREE);
        let leg = best_plan(&c, &q).unwrap().unwrap();
        assert_eq!(leg.s, e18(100));
        assert_eq!(leg.seized, e18(105));
        let oracle = p.quote_exact_in(0, 1, e18(105)).unwrap();
        assert_eq!(leg.swap_out, oracle, "exact quote of seized");
        let on_s = p.quote_exact_in(0, 1, e18(100)).unwrap();
        assert_ne!(
            oracle, on_s,
            "quoting s would understate cost (smaller notional)"
        );
        assert_eq!(leg.flash_fee, U256::ZERO, "Morpho is fee-free");
        assert_eq!(leg.flash_owed, leg.s);
        assert_eq!(leg.contribution, oracle - e18(100));
    }

    /// Joint search: largest debt unfundable, smaller debt fundable →
    /// the second repay option wins (GUIDE 12 §2).
    #[test]
    fn second_repay_leg_wins_when_first_unfundable() {
        let bk = book(vec![deep_v3()]);
        // Morpho holds A1 only. A2 (first repay, larger) has no source.
        let (_s, idx) = flash_morpho(e18(10_000));
        let q = quote(
            &[(A2, e18(500)), (A1, e18(80))],
            &[(A0, e18(200), bonus_5())],
        );
        let market = mkt(U256::MAX);
        let c = ctx(&idx, &bk, &market, &FREE);
        let leg = best_plan(&c, &q).unwrap().unwrap();
        assert_eq!(leg.choice, LegChoice { repay: 1, seize: 0 });
        assert_eq!(leg.debt, A1);
        assert_eq!(leg.s, e18(80));
    }

    /// D26 revisit: high-bonus thin vs low-bonus deep. Contribution uses
    /// exact quotes, so the winner is the better *trade*, not the cheaper
    /// exit. Here both share the same pool; the 10 % bonus seizes more and
    /// must win.
    #[test]
    fn ranking_is_contribution_not_min_exit_cost() {
        let bonus_10 = Ray::from_raw(RAY / U256::from(10u64));
        let bk = book(vec![deep_v3()]);
        let (_s, idx) = flash_morpho(e18(10_000));
        let q = quote(
            &[(A1, e18(50))],
            &[(A0, e18(200), bonus_5()), (A0, e18(200), bonus_10)],
        );
        let mut market = mkt(U256::MAX);
        let entry = idx.entries(A1)[0];
        market.terms.bonus = bonus_5();
        let c5 = ctx(&idx, &bk, &market, &FREE);
        let l5 = evaluate(&c5, &q, LegChoice { repay: 0, seize: 0 }, &entry)
            .unwrap()
            .unwrap();
        market.terms.bonus = bonus_10;
        let c10 = ctx(&idx, &bk, &market, &FREE);
        let l10 = evaluate(&c10, &q, LegChoice { repay: 0, seize: 1 }, &entry)
            .unwrap()
            .unwrap();
        assert!(
            l10.contribution > l5.contribution,
            "higher bonus must raise contribution with exact quotes: {} vs {}",
            l10.contribution,
            l5.contribution
        );
        assert_eq!(l10.seized, e18(55));
        assert_eq!(l5.seized, e18(52) + e18(1) / U256::from(2u64)); // 50*1.05=52.5
        assert_eq!(l5.seized, seized_for(e18(50), &terms()).unwrap());
    }

    /// Flash fee on `s` (Aave 5 bps, `percentMulCeil`). Independent:
    /// `fee_amount(Aave, s, 5)`.
    #[test]
    fn flash_fee_is_charged_on_s() {
        use liq_flash::{AavePool, AaveReserve};
        let srcs: Vec<Box<dyn FlashSource>> = vec![Box::new(AavePool::new(
            addr(0xB0),
            addr(0xB1),
            5,
            &[AaveReserve {
                asset: A1,
                underlying: tok(1),
                atoken: addr(0xB2),
                balance: e18(10_000),
                flash_enabled: true,
                active: true,
                paused: false,
            }],
        ))];
        let mut idx = FlashIndex::new(4);
        idx.refresh(&srcs);
        let bk = book(vec![deep_v3()]);
        let q = quote(&[(A1, e18(1_000))], &[(A0, e18(2_000), bonus_5())]);
        let market = mkt(U256::MAX);
        let c = ctx(&idx, &bk, &market, &FREE);
        let leg = best_plan(&c, &q).unwrap().unwrap();
        let fee = fee_amount(leg.route.provider, leg.s, 5).unwrap();
        assert_eq!(leg.flash_fee, fee);
        assert_eq!(leg.flash_owed, leg.s + fee);
        assert_eq!(leg.contribution, leg.swap_out - leg.flash_owed);
        assert_eq!(
            fee,
            e18(1_000) * U256::from(5u64) / U256::from(10_000u64),
            "1_000e18 at 5 bps is 0.5e18 (exact, so ceil does not add a wei)"
        );
    }

    /// `p = 1` → expected_gas = gas_success; numerator is contribution,
    /// not contribution − gas.
    #[test]
    fn expected_contrib_per_gas_is_pre_gas_and_folds_p() {
        let c = U256::from(1_000u64);
        let at_one = expected_contrib_per_gas(c, RAY, 10, 100).unwrap();
        assert_eq!(at_one, U256::from(100u64), "1000/10");
        // p = 1/2: expected_gas = 0.5*10 + 0.5*100 = 55; expected contrib = 500;
        // 500/55 = 9 (floor).
        let half = RAY / U256::from(2u64);
        assert_eq!(
            expected_contrib_per_gas(c, half, 10, 100).unwrap(),
            U256::from(9u64)
        );
        // p → 0: expected contrib 0, expected_gas = gas_failed, ratio 0.
        assert_eq!(
            expected_contrib_per_gas(c, U256::ZERO, 10, 100).unwrap(),
            U256::ZERO
        );
        assert!(expected_contrib_per_gas(c, RAY + U256::from(1u64), 10, 10).is_err());
        assert!(expected_contrib_per_gas(c, RAY, 0, 0).is_err());
    }

    /// Δnet uses pre-gas contribution minus expected gas cost. A positive
    /// contribution can still have Δnet ≤ 0 when gas dominates — truncate.
    #[test]
    fn delta_net_truncates_when_gas_dominates() {
        // contribution 100, p=1, expected_gas 10, cost/gas 11 → 100 − 110 < 0.
        assert_eq!(
            delta_net(U256::from(100u64), RAY, 10, U256::from(11u64)).unwrap(),
            None
        );
        assert_eq!(
            delta_net(U256::from(100u64), RAY, 10, U256::from(9u64)).unwrap(),
            Some(U256::from(10u64))
        );
    }

    #[test]
    fn historical_parity_is_fail_closed_without_archive() {
        assert_eq!(
            historical_profit_parity(),
            Err(ProfitError::ArchiveUnavailable)
        );
    }

    #[test]
    fn missing_pair_terms_is_unavailable_not_guessed() {
        struct Empty;
        impl MarketView for Empty {
            fn pair_terms(&self, _: ProtocolId, _: AssetId, _: AssetId) -> Option<PairTerms> {
                None
            }
            fn per_eth(&self, _: AssetId) -> Option<U256> {
                None
            }
            fn band(
                &self,
                _: ProtocolId,
                _: AssetId,
                _: AssetId,
            ) -> Option<crate::band::ViabilityBand> {
                Some(crate::band::ViabilityBand {
                    min_size: U256::ZERO,
                    max_size: U256::MAX,
                    base_fee: 0,
                    block: 0,
                })
            }
        }
        let bk = book(vec![deep_v3()]);
        let (_s, idx) = flash_morpho(e18(100));
        let q = quote(&[(A1, e18(10))], &[(A0, e18(20), bonus_5())]);
        let market = Empty;
        let c = ProfitCtx {
            protocol: PROTO,
            flash: &idx,
            haircut: H,
            book: &bk,
            warm: None,
            market: &market,
            gas: &FREE,
            budget: &B,
        };
        let entry = idx.entries(A1)[0];
        assert!(matches!(
            evaluate(&c, &q, LegChoice::PREFERRED, &entry),
            Err(ProfitError::Missing(_))
        ));
    }
}
