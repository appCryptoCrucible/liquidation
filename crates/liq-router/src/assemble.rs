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
use liq_flash::{fee_amount, FlashIndex, Haircut};
use liq_plan::{
    col_per_unit_debt_1e18, ensure_surplus_borrow_profit_legs, validate, BatchPlan, FlashGroup,
    LiqLeg, SwapLeg, ValidateCtx, LEG_EXACT_OUT, LEG_TAKE_BALANCE, VENUE_ROUTER, VENUE_UNIV3_POOL,
};
use liq_protocol::{ExecutorAdapter, FlashRoute, Quote};
use liq_types::fixed::RAY;
use liq_types::{AssetId, PositionId};
use smallvec::SmallVec;

use crate::bid::{searcher_net, Bid};
use crate::exact::{Allocation, ExitQuote, GasTerms};
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

/// Pins the 10E tails (ids 3–8). Missing required fields → do not assemble.
///
/// Fluid T1 is `fluid_t1 == Some(true)` plus `col_per_unit_debt`. T2–T4
/// (`Some(false)`) stay Unwired. Gearbox full MultiCall is Unwired — do
/// not invent `PriceUpdate`. Compound `is_cether` is a config pin
/// (`underlying == 0`), never a `decimals()` guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TailPins {
    pub adapter: ExecutorAdapter,
    pub market: Address,
    pub borrower: Address,
    pub protocol_pull: Option<u128>,
    pub euler_min_yield: Option<U256>,
    pub liquity_trove_id: Option<U256>,
    /// `None` missing; `Some(true)` T1; `Some(false)` T2–T4 Unwired.
    pub fluid_t1: Option<bool>,
    pub fluid_col_per_unit_debt: Option<U256>,
    pub gearbox_min_seized: Option<U256>,
    pub gearbox_full_multicall: bool,
    pub compound_ctoken_collateral: Option<Address>,
    pub compound_is_cether: Option<bool>,
}

/// Euler `minYieldBalance` is the quoted yield (`SeizeOption::max_seize`).
pub fn euler_min_yield_from_quote(q: &Quote, seize: usize) -> Result<U256, AssembleError> {
    let s = q
        .seize_options
        .get(seize)
        .ok_or(AssembleError::Missing("euler seize"))?;
    if s.max_seize.is_zero() {
        return Err(AssembleError::Missing("euler min_yield"));
    }
    Ok(s.max_seize)
}

/// Fluid T1 wire `colPerUnitDebt_` from quote seize/repay.
/// Pin 1e18 slip — not FluidOracle 1e27, not internal `colPerDebt`.
pub fn fluid_col_per_unit_debt_from_quote(
    q: &Quote,
    repay: usize,
    seize: usize,
) -> Result<U256, AssembleError> {
    let r = q
        .repay_options
        .get(repay)
        .ok_or(AssembleError::Missing("fluid repay"))?;
    let s = q
        .seize_options
        .get(seize)
        .ok_or(AssembleError::Missing("fluid seize"))?;
    col_per_unit_debt_1e18(s.max_seize, r.max_repay)
        .map_err(|_| AssembleError::Missing("fluid col_per_unit_debt"))
}

/// Gearbox partial `min_seized` is the quoted seize. Full MultiCall is Unwired.
pub fn gearbox_min_seized_from_quote(q: &Quote, seize: usize) -> Result<U256, AssembleError> {
    let s = q
        .seize_options
        .get(seize)
        .ok_or(AssembleError::Missing("gearbox seize"))?;
    if s.max_seize.is_zero() {
        return Err(AssembleError::Missing("gearbox min_seized"));
    }
    Ok(s.max_seize)
}

/// Build [`LegMeta`] for adapter ids 3–8 (and Silo/AaveV3 empty tails).
/// Fail closed if a required tail field is missing.
pub fn leg_meta_from_pins(p: &TailPins) -> Result<LegMeta, AssembleError> {
    let tail = match p.adapter {
        ExecutorAdapter::AaveV3 | ExecutorAdapter::SiloV2 => LegTail::None,
        ExecutorAdapter::EulerV2 => {
            let min_yield = p
                .euler_min_yield
                .ok_or(AssembleError::Missing("euler min_yield"))?;
            if min_yield.is_zero() {
                return Err(AssembleError::Missing("euler min_yield"));
            }
            LegTail::Euler { min_yield }
        }
        ExecutorAdapter::LiquityV2 => {
            let trove_id = p
                .liquity_trove_id
                .ok_or(AssembleError::Missing("liquity trove_id"))?;
            if trove_id.is_zero() {
                return Err(AssembleError::Missing("liquity trove_id"));
            }
            LegTail::Liquity { trove_id }
        }
        ExecutorAdapter::Fluid => match p.fluid_t1 {
            None => return Err(AssembleError::Missing("fluid vault_type")),
            Some(false) => return Err(AssembleError::Missing("fluid T2-T4 unwired")),
            Some(true) => {
                let col_per_unit_debt = p
                    .fluid_col_per_unit_debt
                    .ok_or(AssembleError::Missing("fluid col_per_unit_debt"))?;
                // Pin slip is 1e18. A 1e27-scale tail ExcessSlippage's every T1 leg.
                if col_per_unit_debt.is_zero() || col_per_unit_debt >= RAY {
                    return Err(AssembleError::Missing("fluid col_per_unit_debt"));
                }
                LegTail::Fluid { col_per_unit_debt }
            }
        },
        ExecutorAdapter::Gearbox => {
            if p.gearbox_full_multicall {
                return Err(AssembleError::Missing("gearbox full MultiCall unwired"));
            }
            let min_seized = p
                .gearbox_min_seized
                .ok_or(AssembleError::Missing("gearbox min_seized"))?;
            if min_seized.is_zero() {
                return Err(AssembleError::Missing("gearbox min_seized"));
            }
            LegTail::Gearbox { min_seized }
        }
        ExecutorAdapter::CompoundV2 => {
            let ctoken_collateral = p
                .compound_ctoken_collateral
                .ok_or(AssembleError::Missing("compound ctoken_collateral"))?;
            if ctoken_collateral.is_zero() {
                return Err(AssembleError::Missing("compound ctoken_collateral"));
            }
            let is_cether = p
                .compound_is_cether
                .ok_or(AssembleError::Missing("compound is_cether"))?;
            LegTail::CompoundV2 {
                ctoken_collateral,
                is_cether: u8::from(is_cether),
            }
        }
        ExecutorAdapter::AaveV4 | ExecutorAdapter::MorphoBlue => {
            return Err(AssembleError::Missing("tail pins 0-2 not via 10E helper"));
        }
    };
    if p.market.is_zero() {
        return Err(AssembleError::Missing("market"));
    }
    if p.borrower.is_zero() {
        return Err(AssembleError::Missing("borrower"));
    }
    Ok(LegMeta {
        adapter: p.adapter,
        market: p.market,
        borrower: p.borrower,
        tail,
        protocol_pull: p.protocol_pull,
    })
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
    /// `fee_bps` of the encoded source, parallel to `plan.groups`.
    /// `reencode_next_source` compares against this, never a hardcoded 0.
    pub group_fee_bps: SmallVec<[u16; 4]>,
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

/// Worst-acceptable-partial floor in **wei** (D29 / GUIDE 12 §4d).
///
/// The per-leg numerator is debt-numeraire `contribution − expected gas`
/// (`swap_out − flash_owed`, then gas). Converted with `per_eth(debt)` —
/// raw debt units per 1e18 wei. That is the debt/ETH oracle, **not** a
/// coll→WETH exact quote of leftover collateral after EXACT_OUT. Leftover
/// coll is unknown until the swaps run; inventing a leftover size to quote
/// coll→WETH would fabricate the floor. When debt is WETH, `per_eth` is
/// 1e18 and the conversion is identity. Stable-debt / volatile-coll
/// divergence versus execution TAKE_BALANCE is priced in the band
/// (GUIDE 12 §4d), not guessed here. A missing conversion fails closed.
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

/// Split `pull` across every nonzero `ExitQuote` allocation. Each share is
/// that pool's `amount_out` fraction of the quote; the last pool takes the
/// residual so the shares **sum exactly to `protocol_pull`**.
fn shares_of_pull(
    exit: &ExitQuote,
    pull: u128,
) -> Result<SmallVec<[(Allocation, u128); 6]>, AssembleError> {
    let mut nz: SmallVec<[&Allocation; 6]> = SmallVec::new();
    for a in &exit.allocs {
        if !a.amount_in.is_zero() && !a.amount_out.is_zero() {
            nz.push(a);
        }
    }
    if nz.is_empty() {
        return Err(AssembleError::Missing("allocation"));
    }
    let total_out = nz
        .iter()
        .try_fold(U256::ZERO, |acc, a| acc.checked_add(a.amount_out))
        .ok_or(AssembleError::AmountTooLarge)?;
    if total_out.is_zero() {
        return Err(AssembleError::Missing("allocation"));
    }
    let pull_u = U256::from(pull);
    let mut out: SmallVec<[(Allocation, u128); 6]> = SmallVec::new();
    let mut assigned = 0u128;
    let last = nz
        .len()
        .checked_sub(1)
        .ok_or(AssembleError::Missing("allocation"))?;
    for (i, a) in nz.iter().enumerate() {
        let share = if i == last {
            pull.checked_sub(assigned)
                .ok_or(AssembleError::AmountTooLarge)?
        } else {
            let sh = u128_of(crate::solver::mul_div_512(a.amount_out, pull_u, total_out)?)?;
            assigned = assigned
                .checked_add(sh)
                .ok_or(AssembleError::AmountTooLarge)?;
            sh
        };
        if share == 0 {
            continue;
        }
        out.push((**a, share));
    }
    let sum = out
        .iter()
        .try_fold(0u128, |a, (_, s)| a.checked_add(*s))
        .ok_or(AssembleError::AmountTooLarge)?;
    if sum != pull || out.is_empty() {
        return Err(AssembleError::Missing("alloc shares"));
    }
    Ok(out)
}

/// Encode **every** nonzero allocation. `amount` on each EXACT_OUT swap is
/// that pool's share of `pull`. One TAKE_BALANCE closer per collateral.
fn swaps_for_leg(
    s: &Scored,
    book: &PoolBook,
    view: &dyn AssembleView,
    weth: Address,
    pull: u128,
    debt_addr: Address,
    coll_addr: Address,
) -> Result<(Vec<SwapLeg>, SwapLeg), AssembleError> {
    let shares = shares_of_pull(&s.leg.exit, pull)?;
    let mut repay = Vec::with_capacity(shares.len());
    let mut last: Option<(u8, Vec<u8>)> = None;
    for (a, amount) in &shares {
        let (venue, data) = venue_bytes(book, a.leg.pool, view)?;
        repay.push(SwapLeg {
            venue,
            token_in: coll_addr,
            token_out: debt_addr,
            flags: LEG_EXACT_OUT,
            amount: *amount,
            data: data.clone(),
        });
        last = Some((venue, data));
    }
    let (venue, data) = match closer_pair(book, view, coll_addr, weth) {
        Ok(v) => v,
        Err(_) => last.ok_or(AssembleError::Missing("allocation"))?,
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

/// UniV3 pool-direct, else allowlisted router, for a token pair present in
/// the book. Fail closed when neither exists — do not invent a pool.
fn closer_pair(
    book: &PoolBook,
    view: &dyn AssembleView,
    token_in: Address,
    token_out: Address,
) -> Result<(u8, Vec<u8>), AssembleError> {
    let mut router_err: Option<AssembleError> = None;
    for p in book.pools() {
        if !p.tokens.contains(&token_in) || !p.tokens.contains(&token_out) || !p.is_live() {
            continue;
        }
        match p.venue() {
            Venue::UniV3 => return Ok((VENUE_UNIV3_POOL, p.address.to_vec())),
            Venue::UniV2 | Venue::CurveStable => {
                let Some(id) = book.by_address(p.address) else {
                    continue;
                };
                match venue_bytes(book, id, view) {
                    Ok(v) => return Ok(v),
                    Err(e) => router_err = Some(e),
                }
            }
        }
    }
    match router_err {
        Some(e) => Err(e),
        None => Err(AssembleError::Missing("pair pool")),
    }
}

fn univ3_addr_for(book: &PoolBook, a: Address, b: Address) -> Option<Address> {
    book.pools().iter().find_map(|p| {
        if p.venue() != Venue::UniV3 || !p.is_live() {
            return None;
        }
        let has_a = p.tokens.contains(&a);
        let has_b = p.tokens.contains(&b);
        (has_a && has_b).then_some(p.address)
    })
}

/// Smallest `flash_amount` that is ≥ `take`, ≥ `pull`, and ≥ `pull + fee(f)`
/// under the provider's exact `fee_amount`. Iterates because Aave's fee is
/// charged on the borrowed amount.
fn flash_cover_fee(
    provider: liq_types::FlashProvider,
    fee_bps: u16,
    pull: u128,
    take: u128,
) -> Result<u128, AssembleError> {
    let mut f = take.max(pull);
    let mut n = 0u8;
    while n < 8 {
        let fee = fee_amount(provider, U256::from(f), fee_bps)
            .ok_or(AssembleError::Profit(ProfitError::UnpriceableFee))?;
        let need = U256::from(pull).checked_add(fee).ok_or(RouteError::Math)?;
        let want = u128_of(need.max(U256::from(take)))?;
        if want <= f {
            return Ok(f);
        }
        f = want;
        n = n.checked_add(1).ok_or(AssembleError::AmountTooLarge)?;
    }
    Err(AssembleError::Missing("flash fee cover"))
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
    gas_terms: &GasTerms,
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
            gas_terms,
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
    gas_terms: &GasTerms,
    flags: u8,
    flash: &FlashIndex,
    haircut: Haircut,
) -> Result<Assembled, AssembleError> {
    let min_profit_wei = min_profit_floor(p, bid, view, gas_price_in_debt)?;
    let gas_cost_wei = u128_of(
        U256::from(p.hop_and_wrap_gas)
            .checked_mul(U256::from(gas_terms.accounting_wei_per_gas()?))
            .ok_or(RouteError::Math)?,
    )?;
    let mut groups = Vec::new();
    let mut profit_swaps: Vec<SwapLeg> = Vec::new();
    let mut fallbacks: SmallVec<[SmallVec<[FlashRoute; 6]>; 4]> = SmallVec::new();
    let mut group_fee_bps: SmallVec<[u16; 4]> = SmallVec::new();
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
                repay_swaps.extend(repay);
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
            let take = u128_of(cg.amount)?;
            // `exact_out == pull` (liq-plan). Fee-charging flashes are funded
            // by over-borrow: flash_amount ≥ pull + fee(flash_amount).
            let flash_amt = flash_cover_fee(cg.provider, cg.fee_bps, pull_sum, take)?;
            groups.push(FlashGroup {
                provider: cg.provider,
                flash_source: cg.source,
                debt_asset: debt_addr,
                flash_amount: flash_amt,
                liqs,
                repay_swaps,
            });
            group_fee_bps.push(cg.fee_bps);
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
    let mut plan = BatchPlan {
        flags,
        bid_bps: bid.coinbase_bps,
        gas_cost_wei,
        min_profit_wei,
        groups,
        profit_swaps,
    };
    route_surplus_debt(&mut plan, book, view, weth)?;
    validate(&plan, validate_ctx)?;
    Ok(Assembled {
        plan,
        fallbacks,
        group_fee_bps,
    })
}

/// When `flash_amount > pull` and debt ≠ WETH, emit TAKE_BALANCE debt→WETH
/// (`liq-plan::SurplusDebtUnrouted`). Uses a book pool; never invents one.
fn route_surplus_debt(
    plan: &mut BatchPlan,
    book: &PoolBook,
    view: &dyn AssembleView,
    weth: Address,
) -> Result<(), AssembleError> {
    let mut need_pool: Option<(Address, Address)> = None;
    for g in &plan.groups {
        let pull: u128 = g
            .liqs
            .iter()
            .try_fold(0u128, |a, l| a.checked_add(l.protocol_pull))
            .ok_or(AssembleError::AmountTooLarge)?;
        if g.debt_asset == weth || g.flash_amount <= pull {
            continue;
        }
        let has = plan.profit_swaps.iter().any(|s| {
            s.token_in == g.debt_asset && s.token_out == weth && s.flags & LEG_TAKE_BALANCE != 0
        });
        if has {
            continue;
        }
        match closer_pair(book, view, g.debt_asset, weth) {
            Ok((venue, data)) => {
                plan.profit_swaps.push(SwapLeg {
                    venue,
                    token_in: g.debt_asset,
                    token_out: weth,
                    flags: LEG_TAKE_BALANCE,
                    amount: 0,
                    data,
                });
            }
            Err(_) => need_pool = Some((g.debt_asset, weth)),
        }
    }
    if let Some((debt, w)) = need_pool {
        let pool =
            univ3_addr_for(book, debt, w).ok_or(AssembleError::Missing("surplus v3 pool"))?;
        ensure_surplus_borrow_profit_legs(plan, weth, pool);
    } else if let Some(g0) = plan.groups.first() {
        if let Some(pool) = univ3_addr_for(book, g0.debt_asset, weth) {
            ensure_surplus_borrow_profit_legs(plan, weth, pool);
        }
    }
    Ok(())
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
    if next.fee_bps > fee_of(group_idx, assembled) {
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

fn fee_of(group_idx: usize, assembled: &Assembled) -> u16 {
    assembled.group_fee_bps.get(group_idx).copied().unwrap_or(0)
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
    use crate::exact::{solve_pair, GasTerms};
    use crate::fixtures::*;
    use crate::profit::{gas_price_in_debt, MarketView};
    use crate::select::{learning_p, select, PositionInput, SelectCfg};
    use crate::solver::Pool;
    use alloy_primitives::{Address, U256};
    use liq_flash::{
        AavePool, AaveReserve, CostModel, FlashIndex, FlashSource, HeldAsset, MorphoBlue,
    };
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
        priority_fee_wei: 0,
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
            Some((addr(0x91), Vec::new()))
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
            wrap_aave_v4: 496_704,
            aave_v4: None,
            liq_gas: 80_000,
            over_borrow: U256::from(1u64),
            budget: B,
            bids: None,
        }
    }

    fn vctx(weth: Address) -> ValidateCtx {
        ValidateCtx {
            weth,
            v4_underlying: Vec::new(),
            morpho: Vec::new(),
            compound: Vec::new(),
            liquity: Vec::new(),
        }
    }

    const L: u128 = 1_000_000_000_000_000_000_000;
    const FREE: GasTerms = GasTerms {
        base_fee_wei: 0,
        priority_fee_wei: 0,
        out_per_eth: WEI,
    };

    fn six_pool_book() -> PoolBook {
        book(vec![
            v3(
                1,
                3000,
                60,
                SQRT_ONE,
                &[(-6000, 6000, L), (-1200, -600, L), (-3000, -1800, 2 * L)],
            ),
            v3(
                2,
                500,
                10,
                SQRT_ONE,
                &[(-2000, 2000, L / 2), (-500, 500, L), (-100, 100, 2 * L)],
            ),
            v3(
                3,
                10_000,
                200,
                SQRT_ONE,
                &[(-20_000, 20_000, 3 * L), (-4000, 0, L)],
            ),
            v2(4, e18(2_000), e18(2_000)),
            v2(5, e18(700), e18(690)),
            v2(6, e18(5_000), e18(5_050)),
        ])
    }

    fn v3_ab(
        n: u64,
        a: liq_types::AssetId,
        b: liq_types::AssetId,
        ta: Address,
        tb: Address,
    ) -> Pool {
        let mut p = v3(
            n,
            500,
            10,
            SQRT_ONE,
            &[(-887_220, 887_220, 50_000_000_000_000_000_000_000)],
        );
        p.assets = smallvec::SmallVec::from_slice(&[a, b]);
        p.tokens = smallvec::SmallVec::from_slice(&[ta, tb]);
        p
    }

    fn idx_aave() -> (Vec<Box<dyn FlashSource>>, FlashIndex) {
        let srcs: Vec<Box<dyn FlashSource>> = vec![Box::new(AavePool::new(
            addr(0xB0),
            addr(0xB1),
            5,
            &[AaveReserve {
                asset: A1,
                underlying: tok(1),
                atoken: addr(0xB2),
                balance: e18(10_000_000),
                flash_enabled: true,
                active: true,
                paused: false,
            }],
        ))];
        let mut i = FlashIndex::new(4);
        i.refresh(&srcs);
        (srcs, i)
    }

    fn idx_two_aave() -> (Vec<Box<dyn FlashSource>>, FlashIndex) {
        let r = |atoken: u64| AaveReserve {
            asset: A1,
            underlying: tok(1),
            atoken: addr(atoken),
            balance: e18(10_000_000),
            flash_enabled: true,
            active: true,
            paused: false,
        };
        let srcs: Vec<Box<dyn FlashSource>> = vec![
            Box::new(AavePool::new(addr(0xB0), addr(0xB1), 5, &[r(0xB2)])),
            Box::new(AavePool::new(addr(0xC0), addr(0xC1), 5, &[r(0xC2)])),
        ];
        let mut i = FlashIndex::new(4);
        i.refresh(&srcs);
        (srcs, i)
    }

    fn world_one(quote: &Quote) -> World {
        let mut world = World {
            tokens: HashMap::new(),
            metas: HashMap::new(),
        };
        world.tokens.insert(A0, tok(0));
        world.tokens.insert(A1, tok(1));
        world.tokens.insert(A2, tok(2));
        world.metas.insert(
            quote.position,
            LegMeta {
                adapter: ExecutorAdapter::AaveV3,
                market: addr(0x51),
                borrower: quote.key.user,
                tail: LegTail::None,
                protocol_pull: None,
            },
        );
        world
    }

    fn input_of(quote: &Quote) -> PositionInput<'_> {
        PositionInput {
            position: quote.position,
            protocol: PROTO,
            health: health(),
            quote,
            cause: TriggerKind::Stale,
            p: learning_p(),
            gas_success: None,
            gas_failed: 50_000,
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
            &GAS,
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
            &GAS,
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
            &GAS,
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
            &GAS,
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

    /// H1: every nonzero allocation is encoded; EXACT_OUT amounts are that
    /// pool's share and sum to `protocol_pull`. The 12A-1 six-pool book at
    /// 3000e18 in uses 6 pools — assembly must emit 6 repay swaps, not 1.
    #[test]
    fn split_quote_encodes_every_alloc_summing_to_pull() {
        let bk = six_pool_book();
        let oracle = solve_pair(&bk, A0, A1, e18(3_000), &FREE, &B).unwrap();
        let n = oracle
            .allocs
            .iter()
            .filter(|a| !a.amount_in.is_zero())
            .count();
        assert_eq!(n, 6, "12A-1 six_pool_solve oracle: {n} pools");

        let (_s, flash) = idx();
        let mut quote = q(1);
        quote.repay_options[0].max_repay = e18(10_000);
        // 100 % bonus: seized stays 3000e18 (six-pool size) while s = 1500e18
        // so fee+impact cannot eat contribution the way a 5 % bonus would.
        let fat = Ray::from_raw(RAY);
        quote.seize_options[0].max_seize = e18(3_000);
        quote.seize_options[0].bonus = fat;
        quote.seize_options[0].curve = BonusCurve::Static { bonus: fat };
        let world = world_one(&quote);
        let plans = select(
            &[input_of(&quote)],
            &cfg(),
            &flash,
            H,
            &bk,
            None,
            &world,
            &FREE,
        )
        .unwrap();
        assert_eq!(plans.len(), 1);
        let exit_n = plans[0].groups[0].legs[0]
            .leg
            .exit
            .allocs
            .iter()
            .filter(|a| !a.amount_in.is_zero())
            .count();
        assert_eq!(exit_n, 6, "sized quote must still use 6 pools");

        let bcfg = BidConfig::new(9_900, 9_900, 0, 0).unwrap();
        let bd = bid(&bcfg, 0, 1).unwrap();
        let assembled = assemble(
            &plans,
            &cfg(),
            &bk,
            &world,
            &vctx(tok(1)),
            &bd,
            gas_price_in_debt(&FREE).unwrap(),
            &FREE,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let g = &assembled[0].plan.groups[0];
        validate(&assembled[0].plan, &vctx(tok(1))).unwrap();
        assert_eq!(
            g.repay_swaps.len(),
            6,
            "one swap per alloc, not the first only"
        );
        let pull = g.liqs.iter().map(|l| l.protocol_pull).sum::<u128>();
        let encoded: u128 = g.repay_swaps.iter().map(|s| s.amount).sum();
        assert_eq!(encoded, pull, "shares must sum to protocol_pull");
        assert!(g
            .repay_swaps
            .iter()
            .all(|s| s.flags & LEG_EXACT_OUT == LEG_EXACT_OUT));
        assert!(g.repay_swaps.iter().all(|s| s.amount > 0));
    }

    /// H4: Aave 5 bps is payable. `exact_out == pull`; flash_amount ≥
    /// pull + fee. Not a Morpho 0-fee fixture.
    #[test]
    fn aave_nonzero_fee_is_funded_by_over_borrow() {
        let bk = book(vec![deep()]);
        let (_s, flash) = idx_aave();
        let quote = q(1);
        let world = world_one(&quote);
        let plans = select(
            &[input_of(&quote)],
            &cfg(),
            &flash,
            H,
            &bk,
            None,
            &world,
            &GAS,
        )
        .unwrap();
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
            &GAS,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let plan = &assembled[0].plan;
        validate(plan, &vctx(tok(1))).unwrap();
        let g = &plan.groups[0];
        assert_eq!(g.provider, liq_types::FlashProvider::Aave);
        let pull: u128 = g.liqs.iter().map(|l| l.protocol_pull).sum();
        let exact_out: u128 = g
            .repay_swaps
            .iter()
            .filter(|s| s.flags & LEG_EXACT_OUT != 0)
            .map(|s| s.amount)
            .sum();
        assert_eq!(exact_out, pull, "liq-plan: exact_out == pull");
        let fee = fee_amount(g.provider, U256::from(g.flash_amount), 5).unwrap();
        assert!(!fee.is_zero(), "Aave 5 bps is nonzero");
        assert!(
            U256::from(g.flash_amount) >= U256::from(pull) + fee,
            "flash_amount {} must be ≥ pull {} + fee {}",
            g.flash_amount,
            pull,
            fee
        );
        assert_eq!(assembled[0].group_fee_bps[0], 5);
    }

    /// H5: USDC (non-WETH) debt + over_borrow > 0 must emit TAKE_BALANCE
    /// debt→WETH and `validate()` Ok (`SurplusDebtUnrouted` otherwise).
    #[test]
    fn usdc_over_borrow_emits_surplus_take_balance_and_validates() {
        let bk = book(vec![
            deep(),
            v3_ab(8, A0, A2, tok(0), tok(2)),
            v3_ab(9, A1, A2, tok(1), tok(2)),
        ]);
        let (_s, flash) = idx();
        let quote = q(1);
        let world = world_one(&quote);
        let plans = select(
            &[input_of(&quote)],
            &cfg(),
            &flash,
            H,
            &bk,
            None,
            &world,
            &GAS,
        )
        .unwrap();
        let bcfg = BidConfig::new(9_900, 9_900, 0, 0).unwrap();
        let bd = bid(&bcfg, 0, 1).unwrap();
        let weth = tok(2);
        let assembled = assemble(
            &plans,
            &cfg(),
            &bk,
            &world,
            &vctx(weth),
            &bd,
            gas_price_in_debt(&GAS).unwrap(),
            &GAS,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let plan = &assembled[0].plan;
        validate(plan, &vctx(weth)).unwrap();
        let g = &plan.groups[0];
        let pull: u128 = g.liqs.iter().map(|l| l.protocol_pull).sum();
        assert_eq!(g.debt_asset, tok(1), "debt is USDC, not WETH");
        assert_ne!(g.debt_asset, weth);
        assert!(g.flash_amount > pull, "over_borrow leaves surplus debt");
        assert!(
            plan.profit_swaps.iter().any(|s| {
                s.token_in == g.debt_asset && s.token_out == weth && s.flags & LEG_TAKE_BALANCE != 0
            }),
            "surplus debt must be routed to WETH"
        );
    }

    /// H6: current fee is stored, so same-fee Aave → Aave is not
    /// `FeeIncreased` (the old `fee_of` always returned 0).
    #[test]
    fn reencode_same_fee_aave_is_allowed() {
        let bk = book(vec![deep()]);
        let (_s, flash) = idx_two_aave();
        let quote = q(1);
        let world = world_one(&quote);
        let plans = select(
            &[input_of(&quote)],
            &cfg(),
            &flash,
            H,
            &bk,
            None,
            &world,
            &GAS,
        )
        .unwrap();
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
            &GAS,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        assert_eq!(assembled[0].group_fee_bps[0], 5);
        assert!(
            !assembled[0].fallbacks[0].is_empty(),
            "second Aave source is the fallback"
        );
        let next = reencode_next_source(&assembled[0], 0, &vctx(tok(1))).unwrap();
        validate(&next, &vctx(tok(1))).unwrap();
        assert_eq!(next.groups[0].provider, liq_types::FlashProvider::Aave);
        assert_ne!(
            next.groups[0].flash_source,
            assembled[0].plan.groups[0].flash_source
        );
    }

    fn pins_base(adapter: ExecutorAdapter) -> TailPins {
        TailPins {
            adapter,
            market: addr(0x51),
            borrower: addr(0xB1),
            protocol_pull: None,
            euler_min_yield: None,
            liquity_trove_id: None,
            fluid_t1: None,
            fluid_col_per_unit_debt: None,
            gearbox_min_seized: None,
            gearbox_full_multicall: false,
            compound_ctoken_collateral: None,
            compound_is_cether: None,
        }
    }

    /// 10E tails: missing fields refuse assemble. Negative: each id 3–8
    /// without its pin is `Missing`, not a guessed tail.
    #[test]
    fn leg_meta_ids_3_8_fail_closed_on_missing_tail() {
        assert!(matches!(
            leg_meta_from_pins(&pins_base(ExecutorAdapter::EulerV2)),
            Err(AssembleError::Missing("euler min_yield"))
        ));
        let silo = leg_meta_from_pins(&pins_base(ExecutorAdapter::SiloV2)).unwrap();
        assert_eq!(silo.tail, LegTail::None);
        assert!(matches!(
            leg_meta_from_pins(&pins_base(ExecutorAdapter::LiquityV2)),
            Err(AssembleError::Missing("liquity trove_id"))
        ));
        assert!(matches!(
            leg_meta_from_pins(&pins_base(ExecutorAdapter::Fluid)),
            Err(AssembleError::Missing("fluid vault_type"))
        ));
        let mut t2 = pins_base(ExecutorAdapter::Fluid);
        t2.fluid_t1 = Some(false);
        t2.fluid_col_per_unit_debt = Some(U256::from(1u64));
        assert!(matches!(
            leg_meta_from_pins(&t2),
            Err(AssembleError::Missing("fluid T2-T4 unwired"))
        ));
        assert!(matches!(
            leg_meta_from_pins(&pins_base(ExecutorAdapter::Gearbox)),
            Err(AssembleError::Missing("gearbox min_seized"))
        ));
        let mut full = pins_base(ExecutorAdapter::Gearbox);
        full.gearbox_min_seized = Some(U256::from(1u64));
        full.gearbox_full_multicall = true;
        assert!(matches!(
            leg_meta_from_pins(&full),
            Err(AssembleError::Missing("gearbox full MultiCall unwired"))
        ));
        assert!(matches!(
            leg_meta_from_pins(&pins_base(ExecutorAdapter::CompoundV2)),
            Err(AssembleError::Missing("compound ctoken_collateral"))
        ));
        let mut c = pins_base(ExecutorAdapter::CompoundV2);
        c.compound_ctoken_collateral = Some(addr(0xC1));
        assert!(matches!(
            leg_meta_from_pins(&c),
            Err(AssembleError::Missing("compound is_cether"))
        ));
    }

    #[test]
    fn leg_meta_ids_3_8_ok_when_pins_present() {
        let mut e = pins_base(ExecutorAdapter::EulerV2);
        e.euler_min_yield = Some(U256::from(7u64));
        assert_eq!(
            leg_meta_from_pins(&e).unwrap().tail,
            LegTail::Euler {
                min_yield: U256::from(7u64)
            }
        );
        let mut l = pins_base(ExecutorAdapter::LiquityV2);
        l.liquity_trove_id = Some(U256::from(42u64));
        assert_eq!(
            leg_meta_from_pins(&l).unwrap().tail,
            LegTail::Liquity {
                trove_id: U256::from(42u64)
            }
        );
        let mut f = pins_base(ExecutorAdapter::Fluid);
        f.fluid_t1 = Some(true);
        f.fluid_col_per_unit_debt = Some(liq_types::fixed::WAD);
        assert_eq!(
            leg_meta_from_pins(&f).unwrap().tail,
            LegTail::Fluid {
                col_per_unit_debt: liq_types::fixed::WAD
            }
        );
        let mut f27 = pins_base(ExecutorAdapter::Fluid);
        f27.fluid_t1 = Some(true);
        f27.fluid_col_per_unit_debt = Some(RAY);
        assert!(matches!(
            leg_meta_from_pins(&f27),
            Err(AssembleError::Missing("fluid col_per_unit_debt"))
        ));
        let mut g = pins_base(ExecutorAdapter::Gearbox);
        g.gearbox_min_seized = Some(U256::from(9u64));
        assert_eq!(
            leg_meta_from_pins(&g).unwrap().tail,
            LegTail::Gearbox {
                min_seized: U256::from(9u64)
            }
        );
        let mut c = pins_base(ExecutorAdapter::CompoundV2);
        c.compound_ctoken_collateral = Some(addr(0xC1));
        c.compound_is_cether = Some(true);
        match leg_meta_from_pins(&c).unwrap().tail {
            LegTail::CompoundV2 {
                ctoken_collateral,
                is_cether,
            } => {
                assert_eq!(ctoken_collateral, addr(0xC1));
                assert_eq!(is_cether, 1);
            }
            other => panic!("{other:?}"),
        }
        let q = Quote {
            position: PositionId(1),
            key: PositionKey {
                protocol: PROTO,
                market: MarketId(0),
                user: addr(0xB1),
            },
            repay_options: smallvec::SmallVec::from_slice(&[RepayOption {
                asset: A1,
                max_repay: e18(1),
            }]),
            seize_options: smallvec::SmallVec::from_slice(&[SeizeOption {
                asset: A0,
                max_seize: e18(3),
                bonus: bonus_5(),
                curve: BonusCurve::Static { bonus: bonus_5() },
            }]),
        };
        assert_eq!(euler_min_yield_from_quote(&q, 0).unwrap(), e18(3));
        assert_eq!(gearbox_min_seized_from_quote(&q, 0).unwrap(), e18(3));
        assert!(euler_min_yield_from_quote(&q, 3).is_err());
    }
}
