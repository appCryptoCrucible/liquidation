//! `BatchPlan` assembly (GUIDE 12 §4c–§4e, PLAN-ENCODING).
//!
//! Over-borrow, `minProfit` as the worst-acceptable-partial floor, UniV3
//! pool-direct swaps from the exact quote, profit TAKE_BALANCE to WETH.
//! The plan is refused unless [`liq_plan::validate`] accepts it.
//!
//! Venue is the 12A-1 closed enum: UniV3 → pool-direct; UniV2 / Curve →
//! allowlisted router **only** when the caller supplies calldata. Kyber is
//! not a [`crate::Venue`] variant and is never emitted (05E N1).

use alloy_primitives::{Address, U256};
use liq_exec::wire::LegTail;
use liq_flash::fallback_chain;
use liq_flash::{FlashIndex, Haircut};
use liq_plan::{
    validate, BatchPlan, FlashGroup, LiqLeg, SwapLeg, ValidateCtx, LEG_EXACT_OUT, LEG_TAKE_BALANCE,
    VENUE_ROUTER, VENUE_UNIV3_POOL,
};
use liq_protocol::{ExecutorAdapter, FlashRoute};
use liq_types::{AssetId, PositionId};
use smallvec::SmallVec;

use crate::bid::{searcher_net, Bid};
use crate::profit::ProfitError;
use crate::select::{Scored, SelectCfg, SelectedPlan};
use crate::solver::{PoolBook, PoolId, RouteError, Venue};

/// Adapter fields `Protocol::encode` would have supplied. Required per
/// position; missing → the plan is not emitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegMeta {
    pub adapter: ExecutorAdapter,
    pub market: Address,
    pub borrower: Address,
    pub tail: LegTail,
    /// Actual protocol pull. `None` → equal to the sized repay (V3-style
    /// close factor already in `s`). Morpho/V4 clamp must be supplied.
    pub protocol_pull: Option<u128>,
}

/// Lookups assembly cannot default.
pub trait AssembleView {
    fn token(&self, asset: AssetId) -> Option<Address>;
    fn meta(&self, pos: PositionId) -> Option<LegMeta>;
    fn per_eth(&self, asset: AssetId) -> Option<U256>;
    /// `(router_target, calldata after the 20-byte target)` for a non-V3
    /// pool. `None` → that pool cannot be encoded (fail closed).
    fn router_leg(&self, pool: Address) -> Option<(Address, Vec<u8>)>;
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AssembleError {
    #[error(transparent)]
    Profit(#[from] ProfitError),
    #[error("required input missing: {0}")]
    Missing(&'static str),
    #[error("amount does not fit u128")]
    AmountTooLarge,
    #[error("no UniV3 allocation and no router calldata for pool {0}")]
    NoEncodableVenue(Address),
    #[error("next flash source charges a higher fee; reprice required")]
    FeeIncreased,
    #[error("next flash source cannot fund the flash amount")]
    NextSourceTooShallow,
    #[error("plan validate: {0}")]
    Validate(Box<liq_plan::EncodeError>),
    #[error("route: {0}")]
    Route(#[from] RouteError),
}

impl From<liq_plan::EncodeError> for AssembleError {
    fn from(e: liq_plan::EncodeError) -> Self {
        Self::Validate(Box::new(e))
    }
}

/// One assembled plan plus the fallback chain 13A/11 walks on
/// `InsufficientLiquidity`.
#[derive(Clone, Debug)]
pub struct Assembled {
    pub plan: BatchPlan,
    /// Per flash-group, the 07B chain excluding the chosen source (next
    /// is `[0]`).
    pub fallbacks: SmallVec<[SmallVec<[FlashRoute; 6]>; 4]>,
}

const WEI: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

fn u128_of(x: U256) -> Result<u128, AssembleError> {
    u128::try_from(x).map_err(|_| AssembleError::AmountTooLarge)
}

fn token(view: &dyn AssembleView, a: AssetId) -> Result<Address, AssembleError> {
    match view.token(a) {
        Some(t) if !t.is_zero() => Ok(t),
        _ => Err(AssembleError::Missing("token")),
    }
}

/// Worst-acceptable-partial floor in wei: min over included legs of
/// `searcher_net(Δnet_in_wei, bid_bps)`. A missing conversion fails
/// closed rather than using a guessed ETH price.
pub fn min_profit_floor(
    plan: &SelectedPlan,
    bid: &Bid,
    view: &dyn AssembleView,
    gas_price_in_debt: U256,
) -> Result<u128, AssembleError> {
    let mut worst: Option<U256> = None;
    for g in &plan.groups {
        let per_eth = view
            .per_eth(g.debt)
            .ok_or(AssembleError::Missing("per_eth"))?;
        if per_eth.is_zero() {
            return Err(AssembleError::Missing("per_eth"));
        }
        for s in &g.legs {
            let cost = U256::from(s.expected_gas)
                .checked_mul(gas_price_in_debt)
                .ok_or(RouteError::Math)?;
            let net_debt = s.leg.contribution.saturating_sub(cost);
            let wei = crate::solver::mul_div_512(net_debt, WEI, per_eth)?;
            let keep = searcher_net(wei, bid.coinbase_bps)
                .ok_or(AssembleError::Missing("searcher_net"))?;
            worst = Some(match worst {
                Some(w) => w.min(keep),
                None => keep,
            });
        }
    }
    let Some(w) = worst else {
        return Err(AssembleError::Missing("no legs"));
    };
    u128_of(w)
}

fn venue_bytes(
    book: &PoolBook,
    alloc_pool: PoolId,
    view: &dyn AssembleView,
) -> Result<(u8, Vec<u8>), AssembleError> {
    let pool = book.get(alloc_pool).ok_or(AssembleError::Missing("pool"))?;
    match pool.venue() {
        Venue::UniV3 => Ok((VENUE_UNIV3_POOL, pool.address.to_vec())),
        Venue::UniV2 | Venue::CurveStable => {
            let (target, call) = view
                .router_leg(pool.address)
                .ok_or(AssembleError::NoEncodableVenue(pool.address))?;
            if target.is_zero() {
                return Err(AssembleError::Missing("router"));
            }
            let mut d = target.to_vec();
            d.extend_from_slice(&call);
            if d.len() < 20 {
                return Err(AssembleError::Missing("router data"));
            }
            Ok((VENUE_ROUTER, d))
        }
    }
}

fn best_encodable_alloc(
    exit: &crate::exact::ExitQuote,
    book: &PoolBook,
    view: &dyn AssembleView,
) -> Result<(u8, Vec<u8>), AssembleError> {
    let mut last_err: Option<AssembleError> = None;
    for a in &exit.allocs {
        if a.amount_in.is_zero() {
            continue;
        }
        match venue_bytes(book, a.leg.pool, view) {
            Ok(v) => return Ok(v),
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => Err(e),
        None => Err(AssembleError::Missing("allocation")),
    }
}

fn swaps_for_leg(
    s: &Scored,
    book: &PoolBook,
    view: &dyn AssembleView,
    weth: Address,
    pull: u128,
    debt_addr: Address,
    coll_addr: Address,
) -> Result<(SwapLeg, SwapLeg), AssembleError> {
    let (venue, data) = best_encodable_alloc(&s.leg.exit, book, view)?;
    let repay = SwapLeg {
        venue,
        token_in: coll_addr,
        token_out: debt_addr,
        flags: LEG_EXACT_OUT,
        amount: pull,
        data: data.clone(),
    };
    let profit = SwapLeg {
        venue,
        token_in: coll_addr,
        token_out: weth,
        flags: LEG_TAKE_BALANCE,
        amount: 0,
        data,
    };
    Ok((repay, profit))
}

/// Assemble every selected plan. Empty `plans` → empty output (a skipped
/// drain, not an error).
#[allow(clippy::too_many_arguments)] // each arg is a required fail-closed input
pub fn assemble(
    plans: &[SelectedPlan],
    cfg: &SelectCfg,
    book: &PoolBook,
    view: &dyn AssembleView,
    validate_ctx: &ValidateCtx,
    bid: &Bid,
    gas_price_in_debt: U256,
    base_fee_wei: u128,
    flags: u8,
    flash: &FlashIndex,
    haircut: Haircut,
) -> Result<SmallVec<[Assembled; 4]>, AssembleError> {
    if validate_ctx.weth.is_zero() {
        return Err(AssembleError::Missing("weth"));
    }
    let mut out = SmallVec::new();
    for p in plans {
        out.push(assemble_one(
            p,
            cfg,
            book,
            view,
            validate_ctx,
            bid,
            gas_price_in_debt,
            base_fee_wei,
            flags,
            flash,
            haircut,
        )?);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)] // mirrors assemble
fn assemble_one(
    p: &SelectedPlan,
    cfg: &SelectCfg,
    book: &PoolBook,
    view: &dyn AssembleView,
    validate_ctx: &ValidateCtx,
    bid: &Bid,
    gas_price_in_debt: U256,
    base_fee_wei: u128,
    flags: u8,
    flash: &FlashIndex,
    haircut: Haircut,
) -> Result<Assembled, AssembleError> {
    let min_profit_wei = min_profit_floor(p, bid, view, gas_price_in_debt)?;
    let gas_cost_wei = u128_of(
        U256::from(p.hop_and_wrap_gas)
            .checked_mul(U256::from(base_fee_wei))
            .ok_or(RouteError::Math)?,
    )?;
    let mut groups = Vec::new();
    let mut profit_swaps: Vec<SwapLeg> = Vec::new();
    let mut fallbacks: SmallVec<[SmallVec<[FlashRoute; 6]>; 4]> = SmallVec::new();
    let weth = validate_ctx.weth;

    for g in &p.groups {
        let debt_addr = token(view, g.debt)?;
        let mut assigned = vec![false; g.legs.len()];
        for cg in &g.cascade.groups {
            let mut capacity = cg.amount;
            let mut liqs = Vec::new();
            let mut repay_swaps = Vec::new();
            for (i, s) in g.legs.iter().enumerate() {
                if assigned.get(i).copied().unwrap_or(true) {
                    continue;
                }
                if s.leg.s > capacity {
                    continue;
                }
                capacity = capacity.saturating_sub(s.leg.s);
                if let Some(flag) = assigned.get_mut(i) {
                    *flag = true;
                }
                let meta = view
                    .meta(s.position)
                    .ok_or(AssembleError::Missing("leg meta"))?;
                let coll_addr = token(view, s.leg.coll)?;
                let repay_u = u128_of(s.leg.s)?;
                let pull = meta.protocol_pull.unwrap_or(repay_u);
                if pull == 0 {
                    return Err(AssembleError::Missing("protocol_pull"));
                }
                liqs.push(LiqLeg {
                    adapter: meta.adapter,
                    market: meta.market,
                    borrower: meta.borrower,
                    collateral_asset: coll_addr,
                    repay_amount: repay_u,
                    tail: meta.tail,
                    protocol_pull: pull,
                });
                let (repay, profit) =
                    swaps_for_leg(s, book, view, weth, pull, debt_addr, coll_addr)?;
                repay_swaps.push(repay);
                // One TAKE_BALANCE closer per collateral across the whole plan.
                if !profit_swaps
                    .iter()
                    .any(|x| x.token_in == coll_addr && x.flags & LEG_TAKE_BALANCE != 0)
                {
                    profit_swaps.push(profit);
                }
            }
            if liqs.is_empty() {
                continue;
            }
            let pull_sum: u128 = liqs.iter().try_fold(0u128, |a, l| {
                a.checked_add(l.protocol_pull)
                    .ok_or(AssembleError::AmountTooLarge)
            })?;
            // Cascade `need` already includes over-borrow (select.rs). Flash
            // at least the pull; at most this sibling's take.
            let take = u128_of(cg.amount)?;
            let flash_amt = take.max(pull_sum);
            groups.push(FlashGroup {
                provider: cg.provider,
                flash_source: cg.source,
                debt_asset: debt_addr,
                flash_amount: flash_amt,
                liqs,
                repay_swaps,
            });
            let chain = fallback_chain(flash, g.debt, cg.amount, haircut, &cfg.cost);
            let rest: SmallVec<[FlashRoute; 6]> = chain
                .into_iter()
                .filter(|r| r.source != cg.source || r.provider != cg.provider)
                .collect();
            fallbacks.push(rest);
        }
        if assigned.iter().any(|a| !*a) {
            tracing::debug!("cascade could not place every sized leg");
        }
    }
    if groups.is_empty() {
        return Err(AssembleError::Missing("groups"));
    }
    let plan = BatchPlan {
        flags,
        bid_bps: bid.coinbase_bps,
        gas_cost_wei,
        min_profit_wei,
        groups,
        profit_swaps,
    };
    validate(&plan, validate_ctx)?;
    Ok(Assembled { plan, fallbacks })
}

/// 07B deferred criterion: on sim `InsufficientLiquidity`, rebuild the
/// named group against the next source in its fallback chain. A higher
/// fee is refused (`FeeIncreased`) so minProfit cannot silently overstate.
pub fn reencode_next_source(
    assembled: &Assembled,
    group_idx: usize,
    validate_ctx: &ValidateCtx,
) -> Result<BatchPlan, AssembleError> {
    let next = assembled
        .fallbacks
        .get(group_idx)
        .and_then(|c| c.first())
        .ok_or(AssembleError::Missing("fallback"))?;
    let mut plan = assembled.plan.clone();
    let g = plan
        .groups
        .get_mut(group_idx)
        .ok_or(AssembleError::Missing("group"))?;
    if next.fee_bps > fee_of(g.provider, assembled) {
        return Err(AssembleError::FeeIncreased);
    }
    let pull: u128 = g.liqs.iter().try_fold(0u128, |a, l| {
        a.checked_add(l.protocol_pull)
            .ok_or(AssembleError::AmountTooLarge)
    })?;
    let need = U256::from(g.flash_amount);
    if next.amount < need && next.amount < U256::from(pull) {
        return Err(AssembleError::NextSourceTooShallow);
    }
    g.provider = next.provider;
    g.flash_source = next.source;
    validate(&plan, validate_ctx)?;
    Ok(plan)
}

fn fee_of(provider: liq_types::FlashProvider, assembled: &Assembled) -> u16 {
    assembled
        .plan
        .groups
        .iter()
        .find(|g| g.provider == provider)
        .and_then(|_| assembled.fallbacks.first())
        .map(|_| {
            // The chosen source's fee is not stored on FlashGroup. Conservative:
            // treat unknown as 0 so a move onto a fee-charging source is
            // `FeeIncreased` unless we know the current fee is already ≥.
            0
        })
        .unwrap_or(0)
}

/// Walk `fallback_chain` for a debt and return the next route after `used`.
#[must_use]
pub fn next_in_chain<'a>(chain: &'a [FlashRoute], used: &FlashRoute) -> Option<&'a FlashRoute> {
    let i = chain
        .iter()
        .position(|r| r.provider == used.provider && r.source == used.source)?;
    chain.get(i.checked_add(1)?)
}

/// Re-encode helper that takes an explicit next [`FlashRoute`] (the 11
/// worker maps `SimError::InsufficientLiquidity` onto this).
pub fn reencode_with(
    mut plan: BatchPlan,
    group_idx: usize,
    next: &FlashRoute,
    current_fee_bps: u16,
    validate_ctx: &ValidateCtx,
) -> Result<BatchPlan, AssembleError> {
    if next.fee_bps > current_fee_bps {
        return Err(AssembleError::FeeIncreased);
    }
    let g = plan
        .groups
        .get_mut(group_idx)
        .ok_or(AssembleError::Missing("group"))?;
    let pull: u128 = g.liqs.iter().try_fold(0u128, |a, l| {
        a.checked_add(l.protocol_pull)
            .ok_or(AssembleError::AmountTooLarge)
    })?;
    if next.amount < U256::from(g.flash_amount) && next.amount < U256::from(pull) {
        return Err(AssembleError::NextSourceTooShallow);
    }
    g.provider = next.provider;
    g.flash_source = next.source;
    validate(&plan, validate_ctx)?;
    Ok(plan)
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
    use crate::bid::{bid, BidConfig};
    use crate::exact::GasTerms;
    use crate::fixtures::*;
    use crate::profit::{gas_price_in_debt, MarketView};
    use crate::select::{learning_p, select, PositionInput, SelectCfg};
    use crate::solver::Pool;
    use alloy_primitives::{Address, U256};
    use liq_flash::{CostModel, FlashIndex, FlashSource, HeldAsset, MorphoBlue};
    use liq_plan::FLAG_SWEEP;
    use liq_protocol::{
        AssetMask, BonusCurve, Health, HealthState, Quote, RepayOption, SeizeOption,
    };
    use liq_types::fixed::RAY;
    use liq_types::{MarketId, PositionKey, ProtocolId, Ray, TriggerKind, Wad};
    use std::collections::HashMap;

    const PROTO: ProtocolId = ProtocolId(0);
    const WEI: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
    fn bonus_5() -> Ray {
        Ray::from_raw(RAY / U256::from(20u64))
    }
    const B: crate::exact::SolveBudget = crate::exact::SolveBudget {
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

    struct World {
        tokens: HashMap<AssetId, Address>,
        metas: HashMap<PositionId, LegMeta>,
    }
    impl MarketView for World {
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
    impl AssembleView for World {
        fn token(&self, asset: AssetId) -> Option<Address> {
            self.tokens.get(&asset).copied()
        }
        fn meta(&self, pos: PositionId) -> Option<LegMeta> {
            self.metas.get(&pos).cloned()
        }
        fn per_eth(&self, _: AssetId) -> Option<U256> {
            Some(e18(1))
        }
        fn router_leg(&self, _: Address) -> Option<(Address, Vec<u8>)> {
            None
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

    fn q(pos: u32) -> Quote {
        Quote {
            position: PositionId(pos),
            key: PositionKey {
                protocol: PROTO,
                market: MarketId(0),
                user: addr(0xB0 + u64::from(pos)),
            },
            repay_options: smallvec::SmallVec::from_slice(&[RepayOption {
                asset: A1,
                max_repay: e18(10),
            }]),
            seize_options: smallvec::SmallVec::from_slice(&[SeizeOption {
                asset: A0,
                max_seize: e18(20),
                bonus: bonus_5(),
                curve: BonusCurve::Static { bonus: bonus_5() },
            }]),
        }
    }

    fn health() -> Health {
        Health {
            hf: Ray::from_raw(RAY / U256::from(2u64)),
            debt_value: Wad::ZERO,
            collateral_value: Wad::ZERO,
            price_sensitivity: AssetMask::EMPTY,
            state: HealthState::Liquidatable,
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

    fn vctx(weth: Address) -> ValidateCtx {
        ValidateCtx {
            weth,
            v4_underlying: Vec::new(),
            morpho: Vec::new(),
        }
    }

    /// Assembled plan satisfies `liq-plan::validate`. Debt = WETH (A1)
    /// so surplus-borrow routing is not required; coll A0 is closed by
    /// exactly one TAKE_BALANCE into WETH.
    #[test]
    fn assembled_plan_validates() {
        let p = deep();
        let bk = book(vec![p]);
        let (_s, flash) = idx();
        let quote = q(1);
        let input = PositionInput {
            position: quote.position,
            protocol: PROTO,
            health: health(),
            quote: &quote,
            cause: TriggerKind::Stale,
            p: learning_p(),
            gas_success: None,
            gas_failed: 50_000,
        };
        let mut world = World {
            tokens: HashMap::new(),
            metas: HashMap::new(),
        };
        world.tokens.insert(A0, tok(0));
        world.tokens.insert(A1, tok(1));
        world.metas.insert(
            PositionId(1),
            LegMeta {
                adapter: ExecutorAdapter::AaveV3,
                market: addr(0x51),
                borrower: quote.key.user,
                tail: LegTail::None,
                protocol_pull: None,
            },
        );
        let plans = select(&[input], &cfg(), &flash, H, &bk, None, &world, &GAS).unwrap();
        assert_eq!(plans.len(), 1);
        let bcfg = BidConfig::new(9_900, 9_900, 0, 0).unwrap();
        let bd = bid(&bcfg, 0, 1).unwrap();
        let price = gas_price_in_debt(&GAS).unwrap();
        let assembled = assemble(
            &plans,
            &cfg(),
            &bk,
            &world,
            &vctx(tok(1)),
            &bd,
            price,
            GAS.base_fee_wei,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        assert_eq!(assembled.len(), 1);
        let plan = &assembled[0].plan;
        validate(plan, &vctx(tok(1))).unwrap();
        assert_eq!(plan.bid_bps, 9_900);
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].liqs.len(), 1, "liqCount == 1");
        assert_eq!(plan.groups[0].provider, liq_types::FlashProvider::Morpho);
        assert!(plan.groups[0].flash_amount >= plan.groups[0].liqs[0].protocol_pull);
        assert_eq!(
            plan.groups[0].repay_swaps[0].flags & LEG_EXACT_OUT,
            LEG_EXACT_OUT
        );
        assert_eq!(plan.profit_swaps.len(), 1);
        assert_eq!(
            plan.profit_swaps[0].flags & LEG_TAKE_BALANCE,
            LEG_TAKE_BALANCE
        );
        assert_eq!(plan.profit_swaps[0].token_out, tok(1));
        assert_eq!(plan.profit_swaps[0].venue, VENUE_UNIV3_POOL);
        assert!(plan.min_profit_wei > 0);
    }

    /// Over-borrow: flash_amount ≥ pull. With extra=1 and a zero-fee
    /// source, surplus is 1 wei when pull fits u128.
    #[test]
    fn over_borrow_exceeds_pull_on_zero_fee_source() {
        let bk = book(vec![deep()]);
        let (_s, flash) = idx();
        let quote = q(1);
        let input = PositionInput {
            position: quote.position,
            protocol: PROTO,
            health: health(),
            quote: &quote,
            cause: TriggerKind::Stale,
            p: learning_p(),
            gas_success: None,
            gas_failed: 50_000,
        };
        let mut world = World {
            tokens: HashMap::new(),
            metas: HashMap::new(),
        };
        world.tokens.insert(A0, tok(0));
        world.tokens.insert(A1, tok(1));
        world.metas.insert(
            PositionId(1),
            LegMeta {
                adapter: ExecutorAdapter::AaveV3,
                market: addr(0x51),
                borrower: quote.key.user,
                tail: LegTail::None,
                protocol_pull: None,
            },
        );
        let plans = select(&[input], &cfg(), &flash, H, &bk, None, &world, &GAS).unwrap();
        let bcfg = BidConfig::new(9_900, 9_900, 0, 0).unwrap();
        let bd = bid(&bcfg, 0, 1).unwrap();
        let assembled = assemble(
            &plans,
            &cfg(),
            &bk,
            &world,
            &vctx(tok(1)),
            &bd,
            gas_price_in_debt(&GAS).unwrap(),
            1,
            0,
            &flash,
            H,
        )
        .unwrap();
        let g = &assembled[0].plan.groups[0];
        let pull = g.liqs[0].protocol_pull;
        assert!(g.flash_amount >= pull);
        assert_eq!(g.flash_amount, pull + 1, "over_borrow = 1 wei");
    }

    /// Missing meta fails closed.
    #[test]
    fn missing_meta_fails_closed() {
        let bk = book(vec![deep()]);
        let (_s, flash) = idx();
        let quote = q(1);
        let input = PositionInput {
            position: quote.position,
            protocol: PROTO,
            health: health(),
            quote: &quote,
            cause: TriggerKind::Stale,
            p: learning_p(),
            gas_success: None,
            gas_failed: 50_000,
        };
        let mut world = World {
            tokens: HashMap::new(),
            metas: HashMap::new(),
        };
        world.tokens.insert(A0, tok(0));
        world.tokens.insert(A1, tok(1));
        let plans = select(&[input], &cfg(), &flash, H, &bk, None, &world, &GAS).unwrap();
        let bcfg = BidConfig::new(9_900, 9_900, 0, 0).unwrap();
        let bd = bid(&bcfg, 0, 1).unwrap();
        let err = assemble(
            &plans,
            &cfg(),
            &bk,
            &world,
            &vctx(tok(1)),
            &bd,
            gas_price_in_debt(&GAS).unwrap(),
            1,
            0,
            &flash,
            H,
        )
        .unwrap_err();
        assert!(matches!(err, AssembleError::Missing("leg meta")));
    }

    /// 05E N1: Venue has no Kyber; assembly match is exhaustive on
    /// `{UniV2, UniV3, CurveStable}`.
    #[test]
    fn venue_enum_has_no_kyber() {
        let src = include_str!("solver.rs");
        let start = src.find("pub enum Venue {").unwrap();
        let body = &src[start..src[start..].find('}').unwrap() + start];
        assert!(!body.contains("Kyber"));
        let asm = include_str!("assemble.rs");
        let code: String = asm
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!code.contains(concat!("Kyber", "Elastic")));
        assert!(!code.contains(concat!("VENUE_", "KYBER")));
    }

    /// `reencode_with` refuses a higher-fee source (would overstate net).
    #[test]
    fn reencode_refuses_higher_fee() {
        let bk = book(vec![deep()]);
        let (_s, flash) = idx();
        let quote = q(1);
        let input = PositionInput {
            position: quote.position,
            protocol: PROTO,
            health: health(),
            quote: &quote,
            cause: TriggerKind::Stale,
            p: learning_p(),
            gas_success: None,
            gas_failed: 50_000,
        };
        let mut world = World {
            tokens: HashMap::new(),
            metas: HashMap::new(),
        };
        world.tokens.insert(A0, tok(0));
        world.tokens.insert(A1, tok(1));
        world.metas.insert(
            PositionId(1),
            LegMeta {
                adapter: ExecutorAdapter::AaveV3,
                market: addr(0x51),
                borrower: quote.key.user,
                tail: LegTail::None,
                protocol_pull: None,
            },
        );
        let plans = select(&[input], &cfg(), &flash, H, &bk, None, &world, &GAS).unwrap();
        let bcfg = BidConfig::new(9_900, 9_900, 0, 0).unwrap();
        let bd = bid(&bcfg, 0, 1).unwrap();
        let assembled = assemble(
            &plans,
            &cfg(),
            &bk,
            &world,
            &vctx(tok(1)),
            &bd,
            gas_price_in_debt(&GAS).unwrap(),
            1,
            0,
            &flash,
            H,
        )
        .unwrap();
        let route = FlashRoute {
            provider: liq_types::FlashProvider::Aave,
            source: addr(0x77),
            asset: A1,
            amount: e18(10_000),
            fee_bps: 5,
            callback: liq_protocol::CallbackShape::AaveExecuteOperation,
        };
        let err =
            reencode_with(assembled[0].plan.clone(), 0, &route, 0, &vctx(tok(1))).unwrap_err();
        assert!(matches!(err, AssembleError::FeeIncreased));
    }
}
