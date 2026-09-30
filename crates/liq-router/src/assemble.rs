//! `BatchPlan` assembly (GUIDE 12 §4c–§4e, PLAN-ENCODING).
//!
//! Over-borrow, `minProfit` as the worst-acceptable-partial floor, UniV3
//! pool-direct swaps from the exact quote, profit TAKE_BALANCE to WETH.
//! Seized WETH is not closed by a swap: it is already the profit asset.
//! The plan is refused unless [`liq_plan::validate`] accepts it.
//!
//! Venue is the 12A-1 closed enum: UniV3 → pool-direct; UniV2 / Curve →
//! allowlisted router **only** when the caller supplies calldata. Kyber is
//! not a [`crate::Venue`] variant and is never emitted (05E N1).

use alloy_primitives::{Address, B256, U256};
use liq_flash::fallback_chain;
use liq_flash::{fee_amount, FlashIndex, Haircut};
use liq_plan::{
    col_per_unit_debt_1e18, ensure_surplus_borrow_profit_legs, validate, BatchPlan, FlashGroup,
    LiqLeg, SwapLeg, ValidateCtx, LEG_EXACT_OUT, LEG_TAKE_BALANCE, VENUE_CURVE_CRYPTO_POOL,
    VENUE_CURVE_POOL, VENUE_PENDLE_PT_REDEEM, VENUE_UNIV2_POOL, VENUE_UNIV3_POOL,
    VENUE_UNWRAP_4626,
};
use liq_protocol::{ExecutorAdapter, FlashRoute, Quote};
use liq_types::fixed::{mul_div, Rounding};
use liq_types::{AssetId, PositionId};
use liq_wire::wire::LegTail;
use smallvec::SmallVec;

use crate::bid::{searcher_net, Bid};
use crate::exact::{Allocation, ExitQuote, GasTerms};
use crate::profit::ProfitError;
use crate::select::{Scored, SelectCfg, SelectedPlan};
use crate::solver::{PoolBook, PoolId, PoolState, RouteError, UnwrapKind, Venue};

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
}

/// Pins the 10E tails (ids 3–8). Missing required fields → do not assemble.
///
/// Fluid is `fluid` (the vault's type, the leg's one-token choices, and the
/// quoted liquidation in the vault's own units) plus the tail figures
/// [`fluid_tail_from_quote`] derives from them. Gearbox `gearbox_full` picks the full
/// add/withdraw liquidation over the partial one (no `PriceUpdate` either
/// way). Compound `is_cether` is a config pin
/// (`underlying == 0`), never a `decimals()` guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TailPins {
    pub adapter: ExecutorAdapter,
    pub market: Address,
    pub borrower: Address,
    pub protocol_pull: Option<u128>,
    pub euler_min_yield: Option<U256>,
    /// Collateral vault `liquidate` names. Required for Euler. Zero refuses.
    pub euler_collateral_vault: Option<Address>,
    pub liquity_trove_id: Option<U256>,
    /// Fluid leg facts from the adapter's state. `None`: not pinned.
    pub fluid: Option<FluidPins>,
    /// Fluid tail figures from [`fluid_tail_from_quote`].
    pub fluid_tail: Option<FluidTailFigures>,
    pub gearbox_min_seized: Option<U256>,
    /// Full liquidation (the quote's all-or-nothing repay option), not partial.
    pub gearbox_full: bool,
    pub compound_ctoken_collateral: Option<Address>,
    pub compound_is_cether: Option<bool>,
    /// Reserve id of the seized collateral, `slot - 1` in the spoke's market
    /// (slot 0 is the spoke meta row — never a reserve).
    pub aave_v4_collateral_reserve_id: Option<u16>,
    /// Reserve id of the repaid debt, `slot - 1` in the spoke's market.
    pub aave_v4_debt_reserve_id: Option<u16>,
    /// Morpho `Id` — the market's own `LoanRow.morpho_id`, not derivable
    /// from `MarketId` alone (Morpho assigns `Id`s on-chain at
    /// `CreateMarket`, not from a static config pin).
    pub morpho_market_id: Option<B256>,
}

/// One Fluid leg as the adapter quoted it: which ABI, which one-token
/// choices, and how much of the vault's own units the quoted repay and
/// seize are — debt shares on a smart-debt vault (T3/T4), col shares on a
/// smart-collateral vault (T2/T4), else the token amounts themselves.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FluidPins {
    /// `liq_wire::wire::FLUID_T1`..`FLUID_T4`.
    pub kind: u8,
    /// `liq_wire::wire::FLUID_*` bits: token choices, absorb, native sides.
    pub flags: u8,
    /// Vault debt units the quoted repay covers.
    pub debt_units: U256,
    /// Vault collateral units the quoted seize is.
    pub col_units: U256,
}

/// The three Fluid tail figures (see `LegTail::Fluid`).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FluidTailFigures {
    pub col_per_unit_debt: U256,
    pub debt_shares_min_per_token: U256,
    pub col_per_share_min: U256,
}

/// Lower a quoted seize to the minimum we will accept on the wire.
///
/// Cross-cutting #5: every one of these bounds was set to exactly the quoted
/// figure, so the protocol reverted on any movement at all between quote and
/// inclusion. `tol_bps` is [`crate::select::SelectCfg::min_out_tolerance_bps`].
/// Rounds DOWN, so the result is always reachable.
fn with_min_out_tolerance(v: U256, tol_bps: u16) -> Result<U256, AssembleError> {
    if tol_bps == 0 {
        return Ok(v);
    }
    let keep = U256::from(10_000u32.saturating_sub(u32::from(tol_bps)));
    mul_div(v, keep, U256::from(10_000u32), Rounding::Down)
        .map_err(|_| AssembleError::Missing("min-out tolerance"))
}

/// Euler `minYieldBalance` from the quoted yield (`SeizeOption::max_seize`),
/// less [`crate::select::SelectCfg::min_out_tolerance_bps`] (E5).
pub fn euler_min_yield_from_quote(
    q: &Quote,
    seize: usize,
    tol_bps: u16,
) -> Result<U256, AssembleError> {
    let s = q
        .seize_options
        .get(seize)
        .ok_or(AssembleError::Missing("euler seize"))?;
    if s.max_seize.is_zero() {
        return Err(AssembleError::Missing("euler min_yield"));
    }
    let out = with_min_out_tolerance(s.max_seize, tol_bps)?;
    if out.is_zero() {
        return Err(AssembleError::Missing("euler min_yield"));
    }
    Ok(out)
}

/// Fluid tail figures for the quoted `(repay, seize)` pair, each a floor the
/// vault checks, lowered by `tol_bps`
/// ([`crate::select::SelectCfg::min_out_tolerance_bps`]) so drift between
/// quote and inclusion does not revert:
///
/// * `colPerUnitDebt` — collateral units per debt unit (`(actualCol ·
///   1e18) / actualDebt`, the vault's own check, pin `9496626f`).
/// * T3/T4 `debt_shares_min_per_token` — debt shares the exact token repay
///   must burn, per token (1e18). The Executor scales it by the sized repay.
/// * T2/T4 `col_per_share_min` — collateral token per col share (1e18) the
///   one-token withdraw must pay.
pub fn fluid_tail_from_quote(
    f: &FluidPins,
    q: &Quote,
    repay: usize,
    seize: usize,
    tol_bps: u16,
) -> Result<FluidTailFigures, AssembleError> {
    use liq_wire::wire::{FLUID_T2, FLUID_T3, FLUID_T4};
    let r = q
        .repay_options
        .get(repay)
        .ok_or(AssembleError::Missing("fluid repay"))?
        .max_repay;
    let s = q
        .seize_options
        .get(seize)
        .ok_or(AssembleError::Missing("fluid seize"))?
        .max_seize;
    if r.is_zero() || s.is_zero() || f.debt_units.is_zero() || f.col_units.is_zero() {
        return Err(AssembleError::Missing("fluid quoted units"));
    }
    let ratio = |num: U256, den: U256| {
        col_per_unit_debt_1e18(num, den).map_err(|_| AssembleError::Missing("fluid ratio"))
    };
    let floor = |v: U256| -> Result<U256, AssembleError> {
        let out = with_min_out_tolerance(v, tol_bps)?;
        if out.is_zero() {
            return Err(AssembleError::Missing("fluid floor"));
        }
        Ok(out)
    };
    let col_per_unit_debt = floor(ratio(f.col_units, f.debt_units)?)?;
    let debt_shares_min_per_token = if f.kind == FLUID_T3 || f.kind == FLUID_T4 {
        floor(ratio(f.debt_units, r)?)?
    } else {
        U256::ZERO
    };
    let col_per_share_min = if f.kind == FLUID_T2 || f.kind == FLUID_T4 {
        floor(ratio(s, f.col_units)?)?
    } else {
        U256::ZERO
    };
    Ok(FluidTailFigures {
        col_per_unit_debt,
        debt_shares_min_per_token,
        col_per_share_min,
    })
}

/// Gearbox `min_seized` (partial: the facade's check; full: the
/// Executor's) from the quoted seize, less
/// [`crate::select::SelectCfg::min_out_tolerance_bps`].
///
/// G6. This is an exact on-chain minimum on a quantity Gearbox derives from
/// its own 8-decimal price feeds, which `config.rs` deliberately does not
/// join — so the bot's figure and the manager's will differ by rounding even
/// when nothing moved. Zero tolerance made that difference a revert.
pub fn gearbox_min_seized_from_quote(
    q: &Quote,
    seize: usize,
    tol_bps: u16,
) -> Result<U256, AssembleError> {
    let s = q
        .seize_options
        .get(seize)
        .ok_or(AssembleError::Missing("gearbox seize"))?;
    if s.max_seize.is_zero() {
        return Err(AssembleError::Missing("gearbox min_seized"));
    }
    let out = with_min_out_tolerance(s.max_seize, tol_bps)?;
    if out.is_zero() {
        return Err(AssembleError::Missing("gearbox min_seized"));
    }
    Ok(out)
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
            let vault = p
                .euler_collateral_vault
                .ok_or(AssembleError::Missing("euler collateral vault"))?;
            if vault.is_zero() {
                return Err(AssembleError::Missing("euler collateral vault"));
            }
            LegTail::Euler { min_yield, vault }
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
        ExecutorAdapter::Fluid => {
            let f = p.fluid.ok_or(AssembleError::Missing("fluid vault_type"))?;
            let t = p
                .fluid_tail
                .ok_or(AssembleError::Missing("fluid col_per_unit_debt"))?;
            if t.col_per_unit_debt.is_zero() {
                return Err(AssembleError::Missing("fluid col_per_unit_debt"));
            }
            LegTail::Fluid {
                kind: f.kind,
                flags: f.flags,
                col_per_unit_debt: t.col_per_unit_debt,
                debt_shares_min_per_token: t.debt_shares_min_per_token,
                col_per_share_min: t.col_per_share_min,
            }
        }
        ExecutorAdapter::Gearbox => {
            let min_seized = p
                .gearbox_min_seized
                .ok_or(AssembleError::Missing("gearbox min_seized"))?;
            if min_seized.is_zero() {
                return Err(AssembleError::Missing("gearbox min_seized"));
            }
            LegTail::Gearbox {
                min_seized,
                full: p.gearbox_full,
            }
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
        ExecutorAdapter::AaveV4 => {
            let collateral_reserve_id = p
                .aave_v4_collateral_reserve_id
                .ok_or(AssembleError::Missing("aave v4 collateral_reserve_id"))?;
            let debt_reserve_id = p
                .aave_v4_debt_reserve_id
                .ok_or(AssembleError::Missing("aave v4 debt_reserve_id"))?;
            LegTail::AaveV4 {
                collateral_reserve_id,
                debt_reserve_id,
            }
        }
        ExecutorAdapter::MorphoBlue => {
            let market_id = p
                .morpho_market_id
                .ok_or(AssembleError::Missing("morpho market_id"))?;
            if market_id.is_zero() {
                return Err(AssembleError::Missing("morpho market_id"));
            }
            LegTail::Morpho { market_id }
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
        // A reward-only group's contribution is already WETH wei.
        let per_eth = if g.reward_only {
            WEI
        } else {
            view.per_eth(g.debt)
                .ok_or(AssembleError::Missing("per_eth"))?
        };
        if per_eth.is_zero() {
            return Err(AssembleError::Missing("per_eth"));
        }
        for s in &g.legs {
            // `gas_price_in_debt` comes from the plan-level WETH-numeraire
            // terms, i.e. wei per gas; convert the debt-unit contribution to
            // wei before subtracting it, never mix the two.
            let cost_wei = U256::from(s.expected_gas)
                .checked_mul(gas_price_in_debt)
                .ok_or(RouteError::Math)?;
            let contribution_wei = crate::solver::mul_div_512(s.leg.contribution, WEI, per_eth)?;
            let wei = contribution_wei.saturating_sub(cost_wei);
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

/// Pool-direct wire data for a swap through `pool_id` from coin `i` to coin
/// `j`. Every venue is pool-direct and verified on chain by the Executor:
/// V3 by the callback's CREATE2 check, V2 by CREATE2 against the pair's
/// factory, Curve by MetaRegistry + coin indices.
fn venue_bytes(
    book: &PoolBook,
    pool_id: PoolId,
    i: u8,
    j: u8,
) -> Result<(u8, Vec<u8>), AssembleError> {
    let pool = book.get(pool_id).ok_or(AssembleError::Missing("pool"))?;
    let mut d = pool.address.to_vec();
    match &pool.state {
        PoolState::V3(_) => Ok((VENUE_UNIV3_POOL, d)),
        PoolState::V2(v2) => {
            d.push(v2.factory);
            Ok((VENUE_UNIV2_POOL, d))
        }
        PoolState::Curve(_) => {
            d.extend_from_slice(&[i, j]);
            Ok((VENUE_CURVE_POOL, d))
        }
        PoolState::Crypto(_) => {
            d.extend_from_slice(&[i, j]);
            Ok((VENUE_CURVE_CRYPTO_POOL, d))
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
/// that pool's share of `pull`. One TAKE_BALANCE closer per non-WETH
/// collateral. WETH collateral returns no closer.
///
/// A Curve share cannot be exact-output: it sells the collateral that buys
/// its share at the quoted rate, raised by `overshoot_bps` (flash fee +
/// min-out tolerance), and the surplus debt is swept to WETH
/// ([`route_surplus_debt`]). The lender's pull enforces the total on chain.
#[allow(clippy::too_many_arguments)] // each input is a distinct plan term
fn swaps_for_leg(
    s: &Scored,
    book: &PoolBook,
    weth: Address,
    pull: u128,
    debt_addr: Address,
    coll_addr: Address,
    overshoot_bps: u16,
) -> Result<(Vec<SwapLeg>, Option<SwapLeg>), AssembleError> {
    let shares = shares_of_pull(&s.leg.exit, pull)?;
    let mut repay = Vec::with_capacity(shares.len());
    let mut last: Option<(u8, Vec<u8>)> = None;
    for (a, amount) in &shares {
        let (venue, data) = venue_bytes(book, a.leg.pool, a.leg.i, a.leg.j)?;
        let (flags, amount) = if venue == VENUE_CURVE_POOL || venue == VENUE_CURVE_CRYPTO_POOL {
            (0, curve_exact_in(a, *amount, overshoot_bps)?)
        } else {
            (LEG_EXACT_OUT, *amount)
        };
        repay.push(SwapLeg {
            venue,
            token_in: coll_addr,
            token_out: debt_addr,
            flags,
            amount,
            data: data.clone(),
        });
        last = Some((venue, data));
    }
    // Residual seized WETH is the profit asset. A closer would name a
    // WETH→WETH pool, which does not exist, and the liquidation would revert
    // instead of sweeping.
    if coll_addr == weth {
        return Ok((repay, None));
    }
    let (venue, data) = match closer_pair(book, coll_addr, weth) {
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
    Ok((repay, Some(profit)))
}

/// Collateral sold exact-in on Curve to buy `share` of debt: the quoted
/// input for that share, rounded up, plus `overshoot_bps`.
fn curve_exact_in(a: &Allocation, share: u128, overshoot_bps: u16) -> Result<u128, AssembleError> {
    if a.amount_out.is_zero() {
        return Err(AssembleError::Missing("curve quote"));
    }
    let base = mul_div(a.amount_in, U256::from(share), a.amount_out, Rounding::Up)
        .map_err(|_| RouteError::Math)?;
    let keep = U256::from(10_000u32.saturating_add(u32::from(overshoot_bps)));
    let with =
        mul_div(base, keep, U256::from(10_000u32), Rounding::Up).map_err(|_| RouteError::Math)?;
    u128_of(with)
}

/// A live pool holding both tokens, for a take-balance closer: UniV3 first,
/// then V2, then Curve. Fail closed when none exists — do not invent a pool.
fn closer_pair(
    book: &PoolBook,
    token_in: Address,
    token_out: Address,
) -> Result<(u8, Vec<u8>), AssembleError> {
    for want in [
        Venue::UniV3,
        Venue::UniV2,
        Venue::CurveStable,
        Venue::CurveCrypto,
    ] {
        for p in book.pools() {
            if p.venue() != want || !p.is_live() {
                continue;
            }
            let (Some(i), Some(j)) = (
                p.tokens.iter().position(|t| *t == token_in),
                p.tokens.iter().position(|t| *t == token_out),
            ) else {
                continue;
            };
            let (Ok(i), Ok(j)) = (u8::try_from(i), u8::try_from(j)) else {
                continue;
            };
            let Some(id) = book.by_address(p.address) else {
                continue;
            };
            return venue_bytes(book, id, i, j);
        }
    }
    Err(AssembleError::Missing("pair pool"))
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

/// Premium the pool will pull on top of `flash_amount`.
fn flash_premium(
    provider: liq_types::FlashProvider,
    fee_bps: u16,
    flash_amount: u128,
) -> Result<u128, AssembleError> {
    let fee = fee_amount(provider, U256::from(flash_amount), fee_bps)
        .ok_or(AssembleError::Profit(ProfitError::UnpriceableFee))?;
    u128_of(fee)
}

/// Add `premium` onto the exact-out repay legs so they sum to
/// `pull + premium`. Shares stay proportional; the last leg takes the
/// remainder so the sum is exact.
fn fund_premium(swaps: &mut [SwapLeg], premium: u128) -> Result<(), AssembleError> {
    if premium == 0 {
        return Ok(());
    }
    let mut n = 0u32;
    let mut total = U256::ZERO;
    for s in swaps.iter() {
        if s.flags & LEG_EXACT_OUT == 0 {
            continue;
        }
        n = n.checked_add(1).ok_or(AssembleError::AmountTooLarge)?;
        total = total
            .checked_add(U256::from(s.amount))
            .ok_or(AssembleError::AmountTooLarge)?;
    }
    if n == 0 || total.is_zero() {
        // Exact-in (Curve) repay legs carry the premium in their overshoot;
        // an unwrap into the debt carries it in the seize bonus.
        if swaps.iter().any(|s| {
            s.flags & (LEG_EXACT_OUT | LEG_TAKE_BALANCE) == 0 || liq_plan::is_unwrap_venue(s.venue)
        }) {
            return Ok(());
        }
        return Err(AssembleError::Missing("repay swap"));
    }
    let prem = U256::from(premium);
    let mut left = premium;
    let mut seen = 0u32;
    for s in swaps.iter_mut() {
        if s.flags & LEG_EXACT_OUT == 0 {
            continue;
        }
        seen = seen.checked_add(1).ok_or(AssembleError::AmountTooLarge)?;
        let add = if seen == n {
            left
        } else {
            let share = mul_div(U256::from(s.amount), prem, total, Rounding::Down)
                .map_err(|_| RouteError::Math)?;
            let share_u = u128_of(share)?;
            left = left
                .checked_sub(share_u)
                .ok_or(AssembleError::AmountTooLarge)?;
            share_u
        };
        s.amount = s
            .amount
            .checked_add(add)
            .ok_or(AssembleError::AmountTooLarge)?;
    }
    Ok(())
}

/// Move an already-funded premium when a fallback source charges a different fee.
fn shift_premium(swaps: &mut [SwapLeg], old_fee: u128, new_fee: u128) -> Result<(), AssembleError> {
    if old_fee == new_fee {
        return Ok(());
    }
    if !swaps.iter().any(|s| s.flags & LEG_EXACT_OUT != 0)
        && swaps.iter().any(|s| liq_plan::is_unwrap_venue(s.venue))
    {
        // Every repay is an unwrap straight into the debt: the premium comes
        // out of the unwrapped surplus, whatever the fee.
        return Ok(());
    }
    let last = swaps
        .iter_mut()
        .rev()
        .find(|s| s.flags & LEG_EXACT_OUT != 0)
        .ok_or(AssembleError::Missing("repay swap"))?;
    if new_fee > old_fee {
        let d = new_fee
            .checked_sub(old_fee)
            .ok_or(AssembleError::AmountTooLarge)?;
        last.amount = last
            .amount
            .checked_add(d)
            .ok_or(AssembleError::AmountTooLarge)?;
    } else {
        let d = old_fee
            .checked_sub(new_fee)
            .ok_or(AssembleError::AmountTooLarge)?;
        last.amount = last
            .amount
            .checked_sub(d)
            .ok_or(AssembleError::Missing("premium headroom"))?;
    }
    Ok(())
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
                // T13 L1. Liquity is paid by the Stability Pool; the
                // liquidator repays nothing, so `protocol_pull == 0` here is
                // the truthful size of the leg, not a missing-data gate
                // firing. Every other adapter's `0` really is a sizing bug.
                //
                // This alone does not make a Liquity-only plan flash-fundable
                // or profitable to select — `profit.rs`/`select.rs` still
                // size legs off `protocol_pull`, and sizing a gas-comp-only
                // leg off `seize.max_seize` instead is separately scoped, per
                // the spec, from this fail-closed-gate fix.
                if pull == 0 && meta.adapter != ExecutorAdapter::LiquityV2 && !g.reward_only {
                    return Err(AssembleError::Missing("protocol_pull"));
                }
                // A reward-only group borrows nothing, so nothing may be pulled.
                if g.reward_only && pull != 0 {
                    return Err(AssembleError::Missing("reward-only leg with a pull"));
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
                if g.reward_only {
                    // Nothing to repay: every paid asset closes to WETH.
                    for &(asset, _) in &s.leg.rewards {
                        let addr = token(view, asset)?;
                        if addr == weth
                            || profit_swaps
                                .iter()
                                .any(|x| x.token_in == addr && x.flags & LEG_TAKE_BALANCE != 0)
                        {
                            continue;
                        }
                        let (venue, data) = closer_pair(book, addr, weth)?;
                        profit_swaps.push(SwapLeg {
                            venue,
                            token_in: addr,
                            token_out: weth,
                            flags: LEG_TAKE_BALANCE,
                            amount: 0,
                            data,
                        });
                    }
                    continue;
                }
                let overshoot = cfg.min_out_tolerance_bps.saturating_add(cg.fee_bps);
                // A wrapper with no pool of its own is unwrapped first (the
                // whole balance, ahead of every selling leg) and what it
                // unwraps into is sold instead.
                let sell_addr = match s.leg.exit.unwrap {
                    Some(u) => {
                        let into = token(view, u.into)?;
                        if !repay_swaps.iter().any(|x: &SwapLeg| {
                            liq_plan::is_unwrap_venue(x.venue) && x.token_in == coll_addr
                        }) {
                            let (venue, data) = match u.kind {
                                UnwrapKind::Erc4626 => (VENUE_UNWRAP_4626, coll_addr.to_vec()),
                                UnwrapKind::PendlePt { yt, .. } => {
                                    (VENUE_PENDLE_PT_REDEEM, yt.to_vec())
                                }
                            };
                            repay_swaps.insert(
                                0,
                                SwapLeg {
                                    venue,
                                    token_in: coll_addr,
                                    token_out: into,
                                    flags: LEG_TAKE_BALANCE,
                                    amount: 0,
                                    data,
                                },
                            );
                        }
                        into
                    }
                    None => coll_addr,
                };
                // Unwrapped straight into the debt: nothing to sell; the
                // surplus debt is swept by `route_surplus_debt`.
                let (repay, profit) = if sell_addr == debt_addr {
                    (Vec::new(), None)
                } else {
                    swaps_for_leg(s, book, weth, pull, debt_addr, sell_addr, overshoot)?
                };
                repay_swaps.extend(repay);
                // One TAKE_BALANCE closer per non-WETH collateral across the
                // whole plan. WETH collateral has no closer.
                if let Some(profit) = profit {
                    if !profit_swaps
                        .iter()
                        .any(|x| x.token_in == sell_addr && x.flags & LEG_TAKE_BALANCE != 0)
                    {
                        profit_swaps.push(profit);
                    }
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
            // Borrow the pull, plus over-borrow dust the source can spare.
            // The premium is not borrowed: the lender pulls
            // `flash_amount + fee(flash_amount)`, so an extra `fee` of
            // principal comes straight back out and the fee is still unpaid.
            // The repay swap buys `pull + fee(flash_amount)`.
            let spare = take
                .checked_sub(pull_sum)
                .ok_or(AssembleError::Missing("flash depth"))?;
            let extra = u128_of(cfg.over_borrow)?;
            let add = extra.min(spare);
            let flash_amt = pull_sum
                .checked_add(add)
                .ok_or(AssembleError::AmountTooLarge)?;
            let premium = flash_premium(cg.provider, cg.fee_bps, flash_amt)?;
            fund_premium(&mut repay_swaps, premium)?;
            groups.push(FlashGroup {
                provider: cg.provider,
                flash_source: cg.source,
                debt_asset: debt_addr,
                flash_amount: flash_amt,
                fee_bps: cg.fee_bps,
                liqs,
                repay_swaps,
            });
            group_fee_bps.push(cg.fee_bps);
            if g.reward_only {
                fallbacks.push(SmallVec::new());
                continue;
            }
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
    route_surplus_debt(&mut plan, book, weth)?;
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
    weth: Address,
) -> Result<(), AssembleError> {
    let mut need_pool: Option<(Address, Address)> = None;
    for g in &plan.groups {
        let pull: u128 = g
            .liqs
            .iter()
            .try_fold(0u128, |a, l| a.checked_add(l.protocol_pull))
            .ok_or(AssembleError::AmountTooLarge)?;
        let exact_in = g.repay_swaps.iter().any(|s| {
            s.token_out == g.debt_asset
                && (s.flags & (LEG_EXACT_OUT | LEG_TAKE_BALANCE) == 0
                    || liq_plan::is_unwrap_venue(s.venue))
        });
        if g.debt_asset == weth || (g.flash_amount <= pull && !exact_in) {
            continue;
        }
        let has = plan.profit_swaps.iter().any(|s| {
            s.token_in == g.debt_asset && s.token_out == weth && s.flags & LEG_TAKE_BALANCE != 0
        });
        if has {
            continue;
        }
        match closer_pair(book, g.debt_asset, weth) {
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
    let old_fee = flash_premium(g.provider, g.fee_bps, g.flash_amount)?;
    g.provider = next.provider;
    g.flash_source = next.source;
    g.fee_bps = next.fee_bps;
    let new_fee = flash_premium(g.provider, g.fee_bps, g.flash_amount)?;
    shift_premium(&mut g.repay_swaps, old_fee, new_fee)?;
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
    use alloy_primitives::{Address, B256, U256};
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
                min_repay: alloy_primitives::U256::ZERO,
                pair_seize: None,
                asset: A1,
                max_repay: e18(10),
                slot: liq_protocol::SlotRef::ByAsset,
            }]),
            seize_options: smallvec::SmallVec::from_slice(&[SeizeOption {
                asset: A0,
                max_seize: e18(20),
                bonus: bonus_5(),
                curve: BonusCurve::Static { bonus: bonus_5() },
                call_target: alloy_primitives::Address::ZERO,
                slot: liq_protocol::SlotRef::ByAsset,
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
            legs_per_plan: u8::MAX,
            nonce_slots: 4,
            header_gas_limit: 30_000_000,
            wrap_gas: [366_332, 355_632, 460_032, 370_435, 384_134],
            wrap_aave_v4: 496_704,
            aave_v4: None,
            liq_gas: crate::select::LiqGas::uniform(80_000),
            over_borrow: U256::from(1u64),
            min_out_tolerance_bps: crate::select::MIN_OUT_TOLERANCE_BPS,
            budget: B,
            bids: None,
            weth: Some(A1),
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

    /// Units regression: `contribution` is in debt units, gas cost is in
    /// wei. A non-WETH debt (here priced like USDC: 3000e6 raw per ETH)
    /// must be converted to wei *before* gas is subtracted.
    #[test]
    fn profit_floor_converts_debt_contribution_before_subtracting_wei_gas() {
        struct Usdc<'a>(&'a World);
        impl AssembleView for Usdc<'_> {
            fn token(&self, a: AssetId) -> Option<Address> {
                self.0.token(a)
            }
            fn meta(&self, p: PositionId) -> Option<LegMeta> {
                self.0.meta(p)
            }
            fn per_eth(&self, _: AssetId) -> Option<U256> {
                Some(U256::from(3_000_000_000u64))
            }
        }
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
        let world = World {
            tokens: HashMap::new(),
            metas: HashMap::new(),
        };
        let plans = select(&[input], &cfg(), &flash, H, &bk, None, &world, &GAS).unwrap();
        let leg = &plans[0].groups[0].legs[0];
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
        let wei_per_gas = U256::from(10_000_000_000u64);
        let floor = min_profit_floor(&plans[0], &bd, &Usdc(&world), wei_per_gas).unwrap();
        let contribution_wei = leg.leg.contribution * WEI / U256::from(3_000_000_000u64);
        let cost_wei = U256::from(leg.expected_gas) * wei_per_gas;
        let want =
            searcher_net(contribution_wei.saturating_sub(cost_wei), bd.coinbase_bps).unwrap();
        assert_eq!(U256::from(floor), want);
    }

    /// Curve-only exit: the repay share is sold exact-in (Curve has no exact
    /// output) with the overshoot, the collateral closer is a take-balance
    /// Curve leg, and the plan still validates.
    #[test]
    fn curve_exit_repays_exact_in_with_overshoot() {
        use liq_plan::VENUE_CURVE_POOL;
        let bk = book(vec![crate::fixtures::curve(
            7,
            &[e18(10_000_000), e18(10_000_000)],
            100_000,
            4_000_000,
        )]);
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
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
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
        let plan = &assembled[0].plan;
        validate(plan, &vctx(tok(1))).unwrap();
        let repay = &plan.groups[0].repay_swaps;
        assert_eq!(repay.len(), 1);
        assert_eq!(repay[0].venue, VENUE_CURVE_POOL);
        assert_eq!(repay[0].flags & LEG_EXACT_OUT, 0, "curve is exact-in");
        assert_eq!(&repay[0].data[20..], &[0u8, 1u8], "coin i = coll, j = debt");
        // Sells the collateral that buys the pull at the quoted rate, plus the
        // overshoot (min-out tolerance + the Morpho source's 0 bps fee); the
        // take-balance closer sells the rest.
        let leg = &plans[0].groups[0].legs[0].leg;
        let alloc = &leg.exit.allocs[0];
        let base = mul_div(alloc.amount_in, leg.s, alloc.amount_out, Rounding::Up).unwrap();
        let want = mul_div(
            base,
            U256::from(10_000u32 + u32::from(cfg().min_out_tolerance_bps)),
            U256::from(10_000u32),
            Rounding::Up,
        )
        .unwrap();
        assert_eq!(U256::from(repay[0].amount), want);
        assert!(want > base && want < alloc.amount_in);
        assert!(plan
            .profit_swaps
            .iter()
            .any(|s| s.venue == VENUE_CURVE_POOL && s.flags & LEG_TAKE_BALANCE != 0));
    }

    /// A2 is an ERC-4626 wrapper of `into` at 1.1 assets per share.
    fn unwrap_a2(into: AssetId) -> crate::solver::Unwrap {
        crate::solver::Unwrap {
            kind: crate::solver::UnwrapKind::Erc4626,
            wrapper: A2,
            wrapper_token: tok(2),
            into,
            into_token: if into == A0 { tok(0) } else { tok(1) },
            assets_per_scale: e18(11) / U256::from(10u64),
            scale: e18(1),
            read_block: 1,
            gas: 60_000,
        }
    }

    fn assemble_a2_exit(bk: &PoolBook) -> (SmallVec<[SelectedPlan; 4]>, BatchPlan) {
        let (_s, flash) = idx();
        let mut quote = q(1);
        quote.seize_options[0].asset = A2;
        let world = world_one(&quote);
        let plans = select(
            &[input_of(&quote)],
            &cfg(),
            &flash,
            H,
            bk,
            None,
            &world,
            &GAS,
        )
        .unwrap();
        assert_eq!(plans.len(), 1);
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
        let price = gas_price_in_debt(&GAS).unwrap();
        let assembled = assemble(
            &plans,
            &cfg(),
            bk,
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
        let plan = assembled[0].plan.clone();
        validate(&plan, &vctx(tok(1))).unwrap();
        (plans, plan)
    }

    /// A wrapper with no pool is solved through its unwrap: the pools see the
    /// converted amount, the marginal is scaled by the rate, and the unwrap's
    /// gas is on the route.
    #[test]
    fn wrapper_without_a_pool_routes_through_its_unwrap() {
        let mut bk = book(vec![deep()]);
        assert!(solve_pair(&bk, A2, A1, e18(10), &GAS, &B).is_err());
        bk.add_unwrap(unwrap_a2(A0));
        let via = solve_pair(&bk, A2, A1, e18(10), &GAS, &B).unwrap();
        let u = via.unwrap.unwrap();
        assert_eq!((u.wrapper, u.into), (A2, A0));
        // 10 shares at 1.1, less 1 ppm and 1 wei.
        let raw = e18(11);
        assert_eq!(
            u.amount_out,
            raw - raw / U256::from(1_000_000u64) - U256::ONE
        );
        let inner = solve_pair(&bk, A0, A1, u.amount_out, &GAS, &B).unwrap();
        assert_eq!(via.amount_out, inner.amount_out);
        assert_eq!(via.allocs, inner.allocs);
        assert_eq!(via.amount_in, e18(10));
        assert_eq!(via.hop_gas, inner.hop_gas + 60_000);
        assert!(via.rho0 > inner.rho0, "rate 1.1 raises the marginal");
        assert!(bk.pairs().any(|p| p == (A2, A1)));
        // An unread rate is not routed.
        bk.add_unwrap(crate::solver::Unwrap {
            read_block: 0,
            ..unwrap_a2(A0)
        });
        assert!(solve_pair(&bk, A2, A1, e18(10), &GAS, &B).is_err());
    }

    /// The seized wrapper is unwrapped (whole balance) ahead of the leg that
    /// sells what it unwraps into, and that asset is closed to WETH.
    #[test]
    fn unwrap_exit_assembles_ahead_of_the_repay() {
        use liq_plan::VENUE_UNWRAP_4626;
        let mut bk = book(vec![deep()]);
        bk.add_unwrap(unwrap_a2(A0));
        let (plans, plan) = assemble_a2_exit(&bk);
        assert!(plans[0].groups[0].legs[0].leg.exit.unwrap.is_some());
        let repay = &plan.groups[0].repay_swaps;
        assert_eq!(plan.groups[0].liqs[0].collateral_asset, tok(2));
        assert_eq!(repay[0].venue, VENUE_UNWRAP_4626);
        assert_eq!((repay[0].token_in, repay[0].token_out), (tok(2), tok(0)));
        assert_eq!(repay[0].flags, LEG_TAKE_BALANCE);
        assert_eq!(repay[0].data, tok(2).to_vec());
        assert_eq!((repay[1].token_in, repay[1].token_out), (tok(0), tok(1)));
        assert_eq!(repay[1].flags & LEG_EXACT_OUT, LEG_EXACT_OUT);
        assert!(plan.profit_swaps.iter().any(|s| s.token_in == tok(0)
            && s.token_out == tok(1)
            && s.flags & LEG_TAKE_BALANCE != 0));
        assert!(!plan.profit_swaps.iter().any(|s| s.token_in == tok(2)));
    }

    /// Liquity-shaped quote: nothing to repay; the protocol pays a WETH
    /// reward and a collateral reward. It is sized as one reward-only leg
    /// counting both, and assembled into a flash-less group whose non-WETH
    /// reward closes to WETH.
    #[test]
    fn reward_only_quote_assembles_a_flash_less_group() {
        let bk = book(vec![deep()]);
        let (_s, flash) = idx();
        let mut quote = q(1);
        quote.repay_options[0].asset = A2; // paid by the protocol, not by us
        quote.repay_options[0].max_repay = U256::ZERO;
        let mut weth_reward = quote.seize_options[0];
        weth_reward.asset = A1;
        weth_reward.max_seize = e18(1) / U256::from(20u64);
        let mut coll_reward = quote.seize_options[0];
        coll_reward.max_seize = e18(1) / U256::from(10u64);
        quote.seize_options = smallvec::SmallVec::from_vec(vec![weth_reward, coll_reward]);
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
        assert_eq!(plans.len(), 1);
        let g = &plans[0].groups[0];
        assert!(g.reward_only);
        assert_eq!(g.cascade.groups[0].provider, liq_types::FlashProvider::None);
        let leg = &g.legs[0].leg;
        assert!(leg.is_reward_only());
        assert_eq!(leg.s, U256::ZERO);
        assert!(
            leg.contribution > e18(1) / U256::from(20u64),
            "both rewards counted: {}",
            leg.contribution
        );

        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
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
        let plan = &assembled[0].plan;
        validate(plan, &vctx(tok(1))).unwrap();
        let fg = &plan.groups[0];
        assert_eq!(fg.provider, liq_types::FlashProvider::None);
        assert_eq!((fg.flash_amount, fg.flash_source), (0, Address::ZERO));
        assert!(fg.repay_swaps.is_empty());
        assert_eq!(fg.liqs[0].protocol_pull, 0);
        assert!(plan.profit_swaps.iter().any(|s| s.token_in == tok(0)
            && s.token_out == tok(1)
            && s.flags & LEG_TAKE_BALANCE != 0));
        assert!(
            assembled[0].fallbacks[0].is_empty(),
            "no flash to fall back from"
        );
    }

    /// An expired Pendle PT exits through venue 6 with its YT as the data;
    /// the rest of the plan is the same as a vault's.
    #[test]
    fn pendle_pt_exit_assembles_as_venue_6() {
        use liq_plan::VENUE_PENDLE_PT_REDEEM;
        let yt = addr(0x7777);
        let mut bk = book(vec![deep()]);
        bk.add_unwrap(crate::solver::Unwrap {
            kind: crate::solver::UnwrapKind::PendlePt {
                yt,
                sy: addr(0x5555),
            },
            ..unwrap_a2(A0)
        });
        let (_, plan) = assemble_a2_exit(&bk);
        let repay = &plan.groups[0].repay_swaps;
        assert_eq!(repay[0].venue, VENUE_PENDLE_PT_REDEEM);
        assert_eq!((repay[0].token_in, repay[0].token_out), (tok(2), tok(0)));
        assert_eq!(repay[0].data, yt.to_vec());
        assert_eq!(repay[1].flags & LEG_EXACT_OUT, LEG_EXACT_OUT);
    }

    /// A wrapper of the debt asset itself needs no pool: the unwrap pays the
    /// repay and the surplus is the profit.
    #[test]
    fn unwrap_into_the_debt_needs_no_pool() {
        use liq_plan::VENUE_UNWRAP_4626;
        let mut bk = book(vec![deep()]);
        bk.add_unwrap(unwrap_a2(A1));
        let q = solve_pair(&bk, A2, A1, e18(10), &GAS, &B).unwrap();
        assert!(q.allocs.is_empty());
        assert_eq!(q.amount_out, q.unwrap.unwrap().amount_out);
        let (_, plan) = assemble_a2_exit(&bk);
        let repay = &plan.groups[0].repay_swaps;
        assert_eq!(repay.len(), 1);
        assert_eq!(repay[0].venue, VENUE_UNWRAP_4626);
        assert_eq!((repay[0].token_in, repay[0].token_out), (tok(2), tok(1)));
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

    /// Collateral is WETH (A0). Repay still buys the debt token. No
    /// WETH→WETH closer: the residual is already the profit asset.
    #[test]
    fn weth_collateral_has_repay_and_no_self_swap() {
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
        assert_eq!(plans.len(), 1);
        let mut c = cfg();
        c.over_borrow = U256::ZERO;
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
        let price = gas_price_in_debt(&GAS).unwrap();
        let weth = tok(0);
        let assembled = assemble(
            &plans,
            &c,
            &bk,
            &world,
            &vctx(weth),
            &bd,
            price,
            &GAS,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let plan = &assembled[0].plan;
        validate(plan, &vctx(weth)).unwrap();
        assert_eq!(plan.groups[0].liqs[0].collateral_asset, weth);
        let repay = &plan.groups[0].repay_swaps;
        assert!(!repay.is_empty(), "debt token is still bought");
        assert!(repay
            .iter()
            .all(|s| s.token_in == weth && s.token_out != weth));
        assert!(
            plan.profit_swaps.iter().all(|s| s.token_in != s.token_out),
            "no WETH to WETH leg"
        );
        assert!(
            !plan.profit_swaps.iter().any(|s| s.token_in == weth),
            "residual WETH is not swapped"
        );
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

    /// H4: Aave 5 bps is bought by the repay swap.
    /// `exact_out == pull + fee(flash_amount)`. Over-borrow is the 1 wei
    /// dust, not the premium: borrowing the premium raises the debt by
    /// the same amount the callback still has to pay.
    #[test]
    fn aave_premium_is_bought_by_exact_out() {
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
        assert_eq!(g.fee_bps, 5);
        let pull: u128 = g.liqs.iter().map(|l| l.protocol_pull).sum();
        let exact_out: u128 = g
            .repay_swaps
            .iter()
            .filter(|s| s.flags & LEG_EXACT_OUT != 0)
            .map(|s| s.amount)
            .sum();
        let fee = fee_amount(g.provider, U256::from(g.flash_amount), g.fee_bps).unwrap();
        assert!(!fee.is_zero(), "Aave 5 bps is nonzero");
        assert_eq!(
            U256::from(exact_out),
            U256::from(pull) + fee,
            "exact_out must be pull + the premium charged on flash_amount"
        );
        assert_eq!(g.flash_amount, pull + 1, "over-borrow stays 1 wei of dust");
        assert!(
            fee > U256::from(1u8),
            "the premium is larger than the dust, so it is not inside flash_amount"
        );
        let balance = U256::from(g.flash_amount) - U256::from(pull) + U256::from(exact_out);
        let owed = U256::from(g.flash_amount) + fee;
        assert_eq!(balance, owed, "callback can pay amount + premium");
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
            euler_collateral_vault: None,
            liquity_trove_id: None,
            fluid: None,
            fluid_tail: None,
            gearbox_min_seized: None,
            gearbox_full: false,
            compound_ctoken_collateral: None,
            compound_is_cether: None,
            aave_v4_collateral_reserve_id: None,
            aave_v4_debt_reserve_id: None,
            morpho_market_id: None,
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
        t2.fluid = Some(FluidPins {
            kind: liq_wire::wire::FLUID_T2,
            flags: 0,
            debt_units: U256::from(1u64),
            col_units: U256::from(1u64),
        });
        assert!(matches!(
            leg_meta_from_pins(&t2),
            Err(AssembleError::Missing("fluid col_per_unit_debt"))
        ));
        assert!(matches!(
            leg_meta_from_pins(&pins_base(ExecutorAdapter::Gearbox)),
            Err(AssembleError::Missing("gearbox min_seized"))
        ));
        let mut full = pins_base(ExecutorAdapter::Gearbox);
        full.gearbox_min_seized = Some(U256::from(1u64));
        full.gearbox_full = true;
        assert_eq!(
            leg_meta_from_pins(&full).unwrap().tail,
            LegTail::Gearbox {
                min_seized: U256::from(1u64),
                full: true
            }
        );
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
        assert!(matches!(
            leg_meta_from_pins(&pins_base(ExecutorAdapter::AaveV4)),
            Err(AssembleError::Missing("aave v4 collateral_reserve_id"))
        ));
        let mut v4 = pins_base(ExecutorAdapter::AaveV4);
        v4.aave_v4_collateral_reserve_id = Some(1);
        assert!(matches!(
            leg_meta_from_pins(&v4),
            Err(AssembleError::Missing("aave v4 debt_reserve_id"))
        ));
        assert!(matches!(
            leg_meta_from_pins(&pins_base(ExecutorAdapter::MorphoBlue)),
            Err(AssembleError::Missing("morpho market_id"))
        ));
        let mut morpho_zero = pins_base(ExecutorAdapter::MorphoBlue);
        morpho_zero.morpho_market_id = Some(B256::ZERO);
        assert!(matches!(
            leg_meta_from_pins(&morpho_zero),
            Err(AssembleError::Missing("morpho market_id"))
        ));
    }

    #[test]
    fn leg_meta_ids_3_8_ok_when_pins_present() {
        let mut e = pins_base(ExecutorAdapter::EulerV2);
        e.euler_min_yield = Some(U256::from(7u64));
        assert!(matches!(
            leg_meta_from_pins(&e),
            Err(AssembleError::Missing("euler collateral vault"))
        ));
        e.euler_collateral_vault = Some(addr(0xE1));
        assert_eq!(
            leg_meta_from_pins(&e).unwrap().tail,
            LegTail::Euler {
                min_yield: U256::from(7u64),
                vault: addr(0xE1),
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
        f.fluid = Some(FluidPins {
            kind: liq_wire::wire::FLUID_T1,
            flags: liq_wire::wire::FLUID_ABSORB,
            debt_units: U256::from(1u64),
            col_units: U256::from(1u64),
        });
        f.fluid_tail = Some(FluidTailFigures {
            col_per_unit_debt: liq_types::fixed::WAD,
            debt_shares_min_per_token: U256::ZERO,
            col_per_share_min: U256::ZERO,
        });
        assert_eq!(
            leg_meta_from_pins(&f).unwrap().tail,
            LegTail::Fluid {
                kind: liq_wire::wire::FLUID_T1,
                flags: liq_wire::wire::FLUID_ABSORB,
                col_per_unit_debt: liq_types::fixed::WAD,
                debt_shares_min_per_token: U256::ZERO,
                col_per_share_min: U256::ZERO,
            }
        );
        let mut g = pins_base(ExecutorAdapter::Gearbox);
        g.gearbox_min_seized = Some(U256::from(9u64));
        assert_eq!(
            leg_meta_from_pins(&g).unwrap().tail,
            LegTail::Gearbox {
                min_seized: U256::from(9u64),
                full: false
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
        let mut v4 = pins_base(ExecutorAdapter::AaveV4);
        v4.aave_v4_collateral_reserve_id = Some(3);
        v4.aave_v4_debt_reserve_id = Some(5);
        assert_eq!(
            leg_meta_from_pins(&v4).unwrap().tail,
            LegTail::AaveV4 {
                collateral_reserve_id: 3,
                debt_reserve_id: 5,
            }
        );
        let mut morpho = pins_base(ExecutorAdapter::MorphoBlue);
        morpho.morpho_market_id = Some(B256::repeat_byte(0x11));
        assert_eq!(
            leg_meta_from_pins(&morpho).unwrap().tail,
            LegTail::Morpho {
                market_id: B256::repeat_byte(0x11)
            }
        );
        let q = Quote {
            position: PositionId(1),
            key: PositionKey {
                protocol: PROTO,
                market: MarketId(0),
                user: addr(0xB1),
            },
            repay_options: smallvec::SmallVec::from_slice(&[RepayOption {
                min_repay: alloy_primitives::U256::ZERO,
                pair_seize: None,
                asset: A1,
                max_repay: e18(1),
                slot: liq_protocol::SlotRef::ByAsset,
            }]),
            seize_options: smallvec::SmallVec::from_slice(&[SeizeOption {
                asset: A0,
                max_seize: e18(3),
                bonus: bonus_5(),
                curve: BonusCurve::Static { bonus: bonus_5() },
                call_target: alloy_primitives::Address::ZERO,
                slot: liq_protocol::SlotRef::ByAsset,
            }]),
        };
        assert_eq!(euler_min_yield_from_quote(&q, 0, 0).unwrap(), e18(3));
        assert_eq!(gearbox_min_seized_from_quote(&q, 0, 0).unwrap(), e18(3));
        assert!(euler_min_yield_from_quote(&q, 3, 0).is_err());
    }
}
