//! `BatchPlan` assembly (GUIDE 12 §4c–§4e, PLAN-ENCODING).
//!
//! Over-borrow, `minProfit` as the worst-landing floor, UniV3 pool-direct
//! swaps from the exact quote, each tied to its liquidation leg, profit
//! TAKE_BALANCE to WETH. The flash premium is bought at run time.
//! Seized WETH is not closed by a swap: it is already the profit asset.
//! The plan is refused unless [`liq_plan::validate`] accepts it.
//!
//! Venue is the 12A-1 closed enum: UniV3 → pool-direct; UniV2 / Curve →
//! allowlisted router **only** when the caller supplies calldata. Kyber is
//! not a [`crate::Venue`] variant and is never emitted (05E N1).

use alloy_primitives::{Address, B256, I256, U256};
use liq_flash::fallback_chain;
use liq_flash::{fee_amount, FlashIndex, Haircut};
use liq_plan::{
    col_per_unit_debt_1e18, ensure_surplus_borrow_profit_legs, tie_flags, validate, BatchPlan,
    FlashGroup, LiqLeg, SwapLeg, ValidateCtx, LEG_EXACT_OUT, LEG_TAKE_BALANCE, LEG_TIE_MAX,
    VENUE_CURVE_CRYPTO_POOL, VENUE_CURVE_LP_ONE_COIN, VENUE_CURVE_POOL, VENUE_PENDLE_MARKET_SELL,
    VENUE_PENDLE_PT_REDEEM, VENUE_UNIV2_POOL, VENUE_UNIV3_POOL, VENUE_UNIV4_POOL,
    VENUE_UNWRAP_4626,
};
use liq_protocol::{ExecutorAdapter, FlashRoute, Quote};
use liq_types::fixed::{mul_div, Rounding};
use liq_types::{AssetId, FlashProvider, PositionId};
use liq_wire::wire::LegTail;
use smallvec::SmallVec;

use crate::bid::{searcher_net, Bid};
use crate::exact::{Allocation, ExitQuote, GasTerms, HubUse};
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

/// One assembled flash group as [`min_profit_floor`] sees it.
struct FloorGroup<'a> {
    legs: SmallVec<[&'a Scored; 4]>,
    /// Raw debt units per 1e18 wei; 1e18 for a reward-only group, whose
    /// legs' values are wei already.
    per_eth: U256,
    /// What its source charges on the whole flash, in debt units.
    premium: U256,
}

/// Worst-landing floor in **wei** (D29 / GUIDE 12 §4d).
///
/// The plan lands when every flash group fills at least one leg: a group
/// that fills none reverts `AllLegsFailed`, and the plan with it. Its worst
/// landing is then each group's weakest leg alone. That leg's exit sells
/// its seize into `swap_out` of the debt, `s` of which buys back its pull;
/// besides, the group owes its source's premium on the whole flash, which
/// the Executor buys whichever legs fill (a beaten leg's share of the flash
/// goes back unspent; its premium does not). So a group lands at least
/// `min_k (swap_out_k − s_k − slack_k) − premium`, and the plan at least
/// their sum less `gasCostWei`, the gas `execute` subtracts before it takes
/// the bid. The floor is what we keep of that after the bid. Below zero it
/// is 1 wei: `execute` refuses a net loss itself (`gross − gasCostWei`
/// underflows), and a floor of 0 is refused.
///
/// Values convert with `per_eth(debt)` — raw debt units per 1e18 wei, the
/// debt/ETH oracle, **not** a coll→WETH exact quote of leftover collateral
/// after EXACT_OUT. Leftover coll is unknown until the swaps run; inventing
/// a leftover size to quote coll→WETH would fabricate the floor. When debt
/// is WETH, `per_eth` is 1e18 and the conversion is identity. Gains round
/// down and costs up. Stable-debt / volatile-coll divergence versus
/// execution TAKE_BALANCE is priced in the band (GUIDE 12 §4d), not
/// guessed here. A missing conversion fails closed.
///
/// A floor, not the expectation (GUIDE 12 "Set `minProfit` as a floor";
/// GUIDE 10: "from the quoted economics with a tolerance band"). The swaps
/// and seizes are guarded `tol_bps` under their quoted outputs, so a leg
/// may realize that much less of the value its exit sells and still be one
/// we accept: `slack_k`. Set to the exact quoted keep, any shortfall at
/// all reverted the plan: block 26,098,187's WBTC leg kept 0.000185 ETH
/// against a 0.000186 floor, 0.5 % short.
fn min_profit_floor(
    groups: &[FloorGroup<'_>],
    bid: &Bid,
    gas_cost_wei: u128,
    tol_bps: u16,
) -> Result<u128, AssembleError> {
    let wei = |v: U256, per_eth: U256, r: Rounding| {
        mul_div(v, WEI, per_eth, r).map_err(|_| AssembleError::Missing("per_eth"))
    };
    let signed = |v: U256| I256::try_from(v).map_err(|_| AssembleError::AmountTooLarge);
    let math = || AssembleError::Route(RouteError::Math);
    let mut worst = I256::ZERO;
    for fg in groups {
        let mut weakest: Option<I256> = None;
        for s in &fg.legs {
            let moved = wei(s.leg.swap_out, fg.per_eth, Rounding::Down)?;
            let owed = wei(s.leg.s, fg.per_eth, Rounding::Up)?;
            let slack = mul_div(
                moved,
                U256::from(tol_bps),
                U256::from(10_000u32),
                Rounding::Up,
            )
            .map_err(|_| AssembleError::Missing("profit tolerance"))?;
            let v = signed(moved)?
                .checked_sub(signed(owed)?)
                .and_then(|v| v.checked_sub(signed(slack).ok()?))
                .ok_or_else(math)?;
            weakest = Some(weakest.map_or(v, |w| w.min(v)));
        }
        let weakest = weakest.ok_or(AssembleError::Missing("no legs"))?;
        let premium = signed(wei(fg.premium, fg.per_eth, Rounding::Up)?)?;
        worst = worst
            .checked_add(weakest)
            .and_then(|w| w.checked_sub(premium))
            .ok_or_else(math)?;
    }
    if groups.is_empty() {
        return Err(AssembleError::Missing("no legs"));
    }
    let worst = worst
        .checked_sub(signed(U256::from(gas_cost_wei))?)
        .ok_or_else(math)?;
    if !worst.is_positive() {
        return Ok(1);
    }
    let keep = searcher_net(worst.into_raw(), bid.coinbase_bps)
        .ok_or(AssembleError::Missing("searcher_net"))?;
    Ok(u128_of(keep)?.max(1))
}

/// Pool-direct wire data for a swap through `pool_id` from coin `i` to coin
/// `j`. Every venue is pool-direct and verified on chain by the Executor:
/// V3 by the callback's CREATE2 check, V2 by CREATE2 against the pair's
/// factory, Curve by the MetaRegistry handler the leg names (the pool's
/// own, from the registry) + coin indices.
fn venue_bytes(
    book: &PoolBook,
    pool_id: PoolId,
    i: u8,
    j: u8,
) -> Result<(u8, Vec<u8>), AssembleError> {
    let pool = book.get(pool_id).ok_or(AssembleError::Missing("pool"))?;
    let mut d = pool.address.to_vec();
    match &pool.state {
        // A V4 pool is named by its key; the PoolManager is fixed.
        PoolState::V3(s) => match &s.v4 {
            Some(k) => Ok((VENUE_UNIV4_POOL, k.leg_data())),
            // A fork's pool names its factory in a 21st byte; Uniswap's is
            // the bare address.
            None => {
                if s.factory != crate::solver::V3_FACTORY_UNISWAP {
                    d.push(s.factory);
                }
                Ok((VENUE_UNIV3_POOL, d))
            }
        },
        PoolState::V2(v2) => {
            d.push(v2.factory);
            Ok((VENUE_UNIV2_POOL, d))
        }
        PoolState::Curve(c) => {
            d.extend_from_slice(&[i, j, c.handler]);
            Ok((VENUE_CURVE_POOL, d))
        }
        PoolState::Crypto(c) => {
            d.extend_from_slice(&[i, j, c.handler]);
            Ok((VENUE_CURVE_CRYPTO_POOL, d))
        }
        // The Vault's pool id; the Vault takes the direction from the tokens.
        PoolState::Balancer(b) => Ok((liq_plan::VENUE_BALANCER, b.pool_id.to_vec())),
        // The pool and the direction: coin 0 is the pool's token0 (native
        // ETH is named as WETH, wrapped and unwrapped by the module).
        PoolState::Fluid(_) => {
            d.push(u8::from(i == 0 && j == 1));
            Ok((liq_plan::VENUE_FLUID, d))
        }
    }
}

/// Wire data for withdrawing the Curve LP `lp` as coin `i` (venue 7): the
/// LP is its own pool, in the book, and the leg names that pool's
/// MetaRegistry handler.
fn curve_lp_bytes(book: &PoolBook, lp: Address, i: u8) -> Result<Vec<u8>, AssembleError> {
    let pool = book
        .by_address(lp)
        .and_then(|id| book.get(id))
        .ok_or(AssembleError::Missing("curve lp pool"))?;
    let PoolState::Curve(c) = &pool.state else {
        return Err(AssembleError::Missing("curve lp pool"));
    };
    let mut d = lp.to_vec();
    d.extend_from_slice(&[i, c.handler]);
    Ok(d)
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
    let (repay, last) = repay_legs(&s.leg.exit, book, pull, coll_addr, debt_addr, overshoot_bps)?;
    // Residual seized WETH is the profit asset. A closer would name a
    // WETH→WETH pool, which does not exist, and the liquidation would revert
    // instead of sweeping.
    if coll_addr == weth {
        return Ok((repay, None));
    }
    // The closer sells what the repay swaps leave into WETH. It used to
    // fall back to the last repay pool, which holds the debt, not WETH: a
    // leg the Executor's callback check refuses. Without a pool into WETH
    // there is no closer, and no plan.
    let (venue, data) = closer_pair(book, coll_addr, weth, residual_after(&s.leg, pull))?;
    let _ = last;
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

/// A swap leg's venue id and its wire data ([`venue_bytes`]).
type VenueData = (u8, Vec<u8>);

/// The legs buying `pull` of the debt with `token_in`: each pool of `exit`
/// its share, exact-out (Curve, which has no exact output, exact-in with
/// `overshoot_bps`). Also the last pool's venue and data.
fn repay_legs(
    exit: &ExitQuote,
    book: &PoolBook,
    pull: u128,
    token_in: Address,
    debt_addr: Address,
    overshoot_bps: u16,
) -> Result<(Vec<SwapLeg>, Option<VenueData>), AssembleError> {
    // Each chain buys its part of the pull along its path, exact output: the
    // whole pull alone, or, split with direct pools or other chains, the
    // part in proportion to what it buys of the exit's output (the pools
    // share the rest).
    let mut chain_legs: Vec<SwapLeg> = Vec::new();
    let mut pull = pull;
    if let Some(c) = &exit.chain {
        let direct = exit.allocs.iter().any(|a| !a.amount_out.is_zero());
        let all: Vec<&crate::exact::ChainUse> =
            core::iter::once(c.as_ref()).chain(c.with.iter()).collect();
        let total = pull;
        // An exact-input chain is sized on the book as it stands when its
        // leg runs: after the direct legs, and the chains before it, have
        // moved the pools they share. Sized on the untouched book it
        // delivers less than quoted and the flash lender is left short
        // (block 25,791,740: a third of a $3.2M sale through crvUSD).
        let mut displaced = if all.iter().any(|ch| !ch.exact_out) {
            let mut work = book.clone();
            for a in exit.allocs.iter().filter(|a| !a.amount_in.is_zero()) {
                if let Some(p) = work.get_mut(a.leg.pool) {
                    let _ = p.apply_exact_in(a.leg.i, a.leg.j, a.amount_in);
                }
            }
            Some(work)
        } else {
            None
        };
        for (k, ch) in all.iter().enumerate() {
            // Without direct pools the last chain takes what is left, so the
            // parts add to the pull exactly.
            let part = if !direct && k.saturating_add(1) == all.len() {
                pull
            } else if exit.amount_out.is_zero() {
                0
            } else {
                u128_of(crate::solver::mul_div_512(
                    U256::from(total),
                    ch.amount_out,
                    exit.amount_out,
                )?)?
                .min(pull)
            };
            if part != 0 {
                // Through a V4 or Curve hop the chain sells exact input: what
                // buys its part on the book, raised by `overshoot_bps` as a
                // Curve leg's is; the surplus debt is swept to WETH.
                let (flags, amount) = if ch.exact_out {
                    if let Some(work) = displaced.as_mut() {
                        if let Some(need) =
                            crate::exact::path_in_for(work, &ch.hops, U256::from(part))
                        {
                            let mut x = need;
                            for l in &ch.hops {
                                match work.get_mut(l.pool).map(|p| p.apply_exact_in(l.i, l.j, x)) {
                                    Some(Ok(o)) => x = o,
                                    _ => break,
                                }
                            }
                        }
                    }
                    (LEG_EXACT_OUT, part)
                } else {
                    let on = displaced.as_ref().unwrap_or(book);
                    let need = crate::exact::path_in_for(on, &ch.hops, U256::from(part))
                        .ok_or(AssembleError::Missing("chain input"))?;
                    if let Some(work) = displaced.as_mut() {
                        let mut x = need;
                        for l in &ch.hops {
                            match work.get_mut(l.pool).map(|p| p.apply_exact_in(l.i, l.j, x)) {
                                Some(Ok(o)) => x = o,
                                _ => break,
                            }
                        }
                    }
                    let keep = U256::from(10_000u32.saturating_add(u32::from(overshoot_bps)));
                    let with = mul_div(need, keep, U256::from(10_000u32), Rounding::Up)
                        .map_err(|_| RouteError::Math)?;
                    (0, u128_of(with)?)
                };
                chain_legs.push(SwapLeg {
                    venue: liq_plan::VENUE_CHAIN,
                    token_in,
                    token_out: debt_addr,
                    flags,
                    amount,
                    data: ch.data.clone(),
                });
            }
            pull = pull
                .checked_sub(part)
                .ok_or(AssembleError::AmountTooLarge)?;
        }
        if pull == 0 {
            if chain_legs.is_empty() {
                return Err(AssembleError::Missing("chain share"));
            }
            return Ok((chain_legs, Some((liq_plan::VENUE_CHAIN, c.data.clone()))));
        }
    }
    let shares = shares_of_pull(exit, pull)?;
    let mut repay = Vec::with_capacity(shares.len());
    let mut last: Option<VenueData> = None;
    for (a, amount) in &shares {
        let (venue, data) = venue_bytes(book, a.leg.pool, a.leg.i, a.leg.j)?;
        let (flags, amount) = if venue == VENUE_CURVE_POOL
            || venue == VENUE_CURVE_CRYPTO_POOL
            || venue == liq_plan::VENUE_FLUID
        {
            (0, curve_exact_in(a, *amount, overshoot_bps)?)
        } else {
            (LEG_EXACT_OUT, *amount)
        };
        repay.push(SwapLeg {
            venue,
            token_in,
            token_out: debt_addr,
            flags,
            amount,
            data: data.clone(),
        });
        last = Some((venue, data));
    }
    repay.extend(chain_legs);
    Ok((repay, last))
}

/// The repay legs of an exit through a hub token (`hub_addr`: WETH, or an
/// intermediate token the graph proposed): what is sold (`sell_addr`, the
/// collateral or what it unwrapped into) into the hub ([`hub_sell_legs`]),
/// then the hub for `pull` of the debt ([`repay_legs`]). What hub the
/// second step leaves is profit: already WETH, or closed into WETH through
/// one more pool (the returned profit leg).
#[allow(clippy::too_many_arguments)] // each input is a distinct plan term
fn hub_swaps_for_leg(
    exit: &ExitQuote,
    hub: &HubUse,
    book: &PoolBook,
    weth: Address,
    hub_addr: Address,
    pull: u128,
    debt_addr: Address,
    sell_addr: Address,
    overshoot_bps: u16,
    tol_bps: u16,
) -> Result<(Vec<SwapLeg>, Option<SwapLeg>), AssembleError> {
    let mut legs = hub_sell_legs(hub, book, hub_addr, sell_addr, tol_bps)?;
    let (repay, _) = repay_legs(exit, book, pull, hub_addr, debt_addr, overshoot_bps)?;
    legs.extend(repay);
    if hub_addr == weth {
        return Ok((legs, None));
    }
    // The hub left over: its share of what was bought beyond the pull.
    let left = if exit.amount_out.is_zero() {
        hub.amount_out
    } else {
        crate::solver::mul_div_512(
            hub.amount_out,
            exit.amount_out.saturating_sub(U256::from(pull)),
            exit.amount_out,
        )
        .unwrap_or(hub.amount_out)
    };
    let (venue, data) = closer_pair(book, hub_addr, weth, left)?;
    Ok((
        legs,
        Some(SwapLeg {
            venue,
            token_in: hub_addr,
            token_out: weth,
            flags: LEG_TAKE_BALANCE,
            amount: 0,
            data,
        }),
    ))
}

/// The swap legs of a leg funded by a flash swap: the lender pool takes
/// what it is owed, in its own token, inside its callback, so no repay leg
/// buys the debt. Through WETH, the repay blob sells all the collateral
/// into WETH ([`hub_sell_legs`], which closes it) and the pool is paid
/// WETH. On the collateral's own pool nothing is sold before the pool is
/// paid; what collateral is left closes in the profit blob, or is WETH
/// already.
fn flash_swap_swaps_for_leg(
    s: &Scored,
    book: &PoolBook,
    weth: Address,
    pull: u128,
    sell_addr: Address,
    tol_bps: u16,
) -> Result<(Vec<SwapLeg>, Option<SwapLeg>), AssembleError> {
    if let Some(hub) = &s.leg.exit.hub {
        // A flash swap's lender is paid in WETH (`lender_of` refuses other
        // hubs): the WETH hub only.
        if Some(hub.hub) != book.hub() {
            return Err(AssembleError::Missing(
                "flash swap through a hub other than WETH",
            ));
        }
        return Ok((hub_sell_legs(hub, book, weth, sell_addr, tol_bps)?, None));
    }
    if sell_addr == weth {
        return Ok((Vec::new(), None));
    }
    let (venue, data) = closer_pair(book, sell_addr, weth, residual_after(&s.leg, pull))?;
    Ok((
        Vec::new(),
        Some(SwapLeg {
            venue,
            token_in: sell_addr,
            token_out: weth,
            flags: LEG_TAKE_BALANCE,
            amount: 0,
            data,
        }),
    ))
}

/// What the repay swaps leave of the seized collateral: the seize less the
/// share the exit sells for `pull`.
fn residual_after(leg: &crate::profit::SizedLeg, pull: u128) -> U256 {
    let sold = if leg.exit.amount_out.is_zero() {
        leg.seized
    } else {
        crate::solver::mul_div_512(leg.seized, U256::from(pull), leg.exit.amount_out)
            .unwrap_or(leg.seized)
    };
    leg.seized.saturating_sub(sold)
}

/// The first step of an exit through a hub: `sell_addr` (the collateral,
/// or what it unwrapped into) into `hub_addr`. Every pool but the largest sells
/// its allocation exact-in, lowered by `tol_bps` because the seize can come
/// in a little under its quote; the largest then sells the whole balance
/// (TAKE_BALANCE), which closes the collateral, so no profit closer
/// follows.
fn hub_sell_legs(
    hub: &HubUse,
    book: &PoolBook,
    hub_addr: Address,
    sell_addr: Address,
    tol_bps: u16,
) -> Result<Vec<SwapLeg>, AssembleError> {
    let sold: SmallVec<[&Allocation; 6]> = hub
        .allocs
        .iter()
        .filter(|a| !a.amount_in.is_zero())
        .collect();
    let close = sold
        .iter()
        .enumerate()
        .max_by_key(|(_, a)| a.amount_in)
        .map(|(i, _)| i)
        .ok_or(AssembleError::Missing("hub allocation"))?;
    let mut legs = Vec::with_capacity(sold.len());
    let mut closer = None;
    for (i, a) in sold.iter().enumerate() {
        let (venue, data) = venue_bytes(book, a.leg.pool, a.leg.i, a.leg.j)?;
        if i == close {
            closer = Some(SwapLeg {
                venue,
                token_in: sell_addr,
                token_out: hub_addr,
                flags: LEG_TAKE_BALANCE,
                amount: 0,
                data,
            });
            continue;
        }
        legs.push(SwapLeg {
            venue,
            token_in: sell_addr,
            token_out: hub_addr,
            flags: 0,
            amount: u128_of(with_min_out_tolerance(a.amount_in, tol_bps)?)?,
            data,
        });
    }
    legs.extend(closer);
    Ok(legs)
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

/// The pool a TAKE_BALANCE closer sells `token_in` into `token_out`
/// through: the one quoting the most out for `amount` on the book's state
/// (best zero-size marginal when `amount` is zero).
///
/// The closer sells its whole balance at no minimum, so its pool decides
/// what the leftover is worth. It used to be the first live pool in book
/// order, however thin: at block 26,103,141 the leftover 8.11 LINK went
/// through the 1 % LINK/WETH pool for 0.0243 WETH, about 43 % under the
/// oracle price, and the plan failed its own profit check.
fn closer_pair(
    book: &PoolBook,
    token_in: Address,
    token_out: Address,
    amount: U256,
) -> Result<(u8, Vec<u8>), AssembleError> {
    let mut best: Option<(U256, PoolId, u8, u8)> = None;
    for p in book.pools() {
        if !matches!(
            p.venue(),
            Venue::UniV3
                | Venue::UniV2
                | Venue::CurveStable
                | Venue::CurveCrypto
                | Venue::Balancer
                | Venue::Fluid
        ) || !p.is_live()
        {
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
        let score = if amount.is_zero() {
            p.rho_at_zero(i, j).unwrap_or(U256::ZERO)
        } else {
            p.quote_exact_in(i, j, amount).unwrap_or(U256::ZERO)
        };
        if best.as_ref().is_none_or(|(b, ..)| score > *b) {
            best = Some((score, id, i, j));
        }
    }
    if let Some((_, id, i, j)) = best {
        return venue_bytes(book, id, i, j);
    }
    // No pool between them: an exact-input chain through the graph (venue
    // 10), which the closer's TAKE_BALANCE sells the whole balance along.
    let (Some(from), Some(to)) = (book.asset_id(token_in), book.asset_id(token_out)) else {
        return Err(AssembleError::Missing("pair pool"));
    };
    let free = GasTerms {
        base_fee_wei: 0,
        priority_fee_wei: 0,
        out_per_eth: crate::exact::OUT_PER_ETH_WETH,
    };
    let amount = if amount.is_zero() {
        U256::from(1u64)
    } else {
        amount
    };
    match crate::graph::best_chain(book, from, to, amount, &free, 1) {
        Some((_, _, data, _)) => Ok((liq_plan::VENUE_CHAIN, data)),
        None => Err(AssembleError::Missing("pair pool")),
    }
}

fn univ3_addr_for(book: &PoolBook, a: Address, b: Address) -> Option<Address> {
    book.pools().iter().find_map(|p| {
        if !p.is_v3_contract() || !p.is_live() {
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

/// Whether `legs` hold an exact output the Executor adds the flash premium
/// to: a pool-direct V3, V2 or V4 one (`SwapModule.runSwaps`). A router's
/// output is fixed in its own calldata, and Curve has no exact output.
fn buys_premium(legs: &[SwapLeg]) -> bool {
    legs.iter().any(|s| {
        s.flags & LEG_EXACT_OUT != 0
            && (s.venue == VENUE_UNIV3_POOL
                || s.venue == VENUE_UNIV2_POOL
                || s.venue == VENUE_UNIV4_POOL
                || s.venue == liq_plan::VENUE_CHAIN)
    })
}

/// `premium` in bps of `pull`, rounded up: what an exact-input repay of
/// `pull` overshoots by to buy the whole premium too.
fn premium_bps_of(premium: u128, pull: u128) -> Result<u16, AssembleError> {
    if premium == 0 {
        return Ok(0);
    }
    if pull == 0 {
        return Ok(u16::MAX);
    }
    let bps = mul_div(
        U256::from(premium),
        U256::from(10_000u32),
        U256::from(pull),
        Rounding::Up,
    )
    .map_err(|_| RouteError::Math)?;
    Ok(u16::try_from(bps).unwrap_or(u16::MAX))
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
    gas_terms: &GasTerms,
    flags: u8,
    flash: &FlashIndex,
    haircut: Haircut,
) -> Result<Assembled, AssembleError> {
    let gas_cost_wei = u128_of(
        U256::from(p.hop_and_wrap_gas)
            .checked_mul(U256::from(gas_terms.accounting_wei_per_gas()?))
            .ok_or(RouteError::Math)?,
    )?;
    let tol = cfg.min_out_tolerance_bps;
    let mut groups = Vec::new();
    let mut floor_groups: Vec<FloorGroup<'_>> = Vec::new();
    let mut profit_swaps: Vec<SwapLeg> = Vec::new();
    let mut fallbacks: SmallVec<[SmallVec<[FlashRoute; 6]>; 4]> = SmallVec::new();
    let mut group_fee_bps: SmallVec<[u16; 4]> = SmallVec::new();
    let weth = validate_ctx.weth;

    for g in &p.groups {
        let debt_addr = token(view, g.debt)?;
        // A reward-only group's values are WETH wei already.
        let per_eth = if g.reward_only {
            WEI
        } else {
            view.per_eth(g.debt)
                .filter(|p| !p.is_zero())
                .ok_or(AssembleError::Missing("per_eth"))?
        };
        let mut assigned = vec![false; g.legs.len()];
        for cg in &g.cascade.groups {
            let flash_swap = cg.provider == FlashProvider::UniV3Swap;
            // The legs this source funds: in rank order while they fit, and
            // no more than a repay swap's tie can name.
            let mut capacity = cg.amount;
            let mut members: SmallVec<[&Scored; 4]> = SmallVec::new();
            for (i, s) in g.legs.iter().enumerate() {
                if members.len() >= LEG_TIE_MAX {
                    break;
                }
                if assigned.get(i).copied().unwrap_or(true) || s.leg.s > capacity {
                    continue;
                }
                capacity = capacity.saturating_sub(s.leg.s);
                if let Some(flag) = assigned.get_mut(i) {
                    *flag = true;
                }
                members.push(s);
            }
            if members.is_empty() {
                continue;
            }
            let mut liqs = Vec::with_capacity(members.len());
            for s in &members {
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
            // The repay swaps buy each leg's pull; the Executor buys the fee
            // its provider reports, with the group's first exact-output pool
            // leg to run.
            let spare = take
                .checked_sub(pull_sum)
                .ok_or(AssembleError::Missing("flash depth"))?;
            let extra = u128_of(cfg.over_borrow)?;
            // A flash swap buys the pull exactly: the pool is paid for every
            // unit it sells, surplus included.
            let add = if flash_swap { 0 } else { extra.min(spare) };
            let flash_amt = pull_sum
                .checked_add(add)
                .ok_or(AssembleError::AmountTooLarge)?;
            let premium = flash_premium(cg.provider, cg.fee_bps, flash_amt)?;
            let mut repay_swaps: Vec<SwapLeg> = Vec::new();
            for (k, (s, l)) in members.iter().zip(liqs.iter()).enumerate() {
                let coll_addr = l.collateral_asset;
                let pull = l.protocol_pull;
                if g.reward_only {
                    // Nothing to repay: every paid asset closes to WETH.
                    for &(asset, paid) in &s.leg.rewards {
                        let addr = token(view, asset)?;
                        if addr == weth
                            || profit_swaps
                                .iter()
                                .any(|x| x.token_in == addr && x.flags & LEG_TAKE_BALANCE != 0)
                        {
                            continue;
                        }
                        let (venue, data) = closer_pair(book, addr, weth, paid)?;
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
                                UnwrapKind::CurveLp { i } => {
                                    (VENUE_CURVE_LP_ONE_COIN, curve_lp_bytes(book, coll_addr, i)?)
                                }
                                UnwrapKind::PendleMarket { market, .. } => {
                                    (VENUE_PENDLE_MARKET_SELL, market.to_vec())
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
                // surplus debt is swept by `route_surplus_debt`. Through the
                // hub, the repay legs close the collateral themselves.
                let legs_at = |overshoot: u16| -> Result<_, AssembleError> {
                    if sell_addr == debt_addr {
                        Ok((Vec::new(), None))
                    } else if flash_swap {
                        flash_swap_swaps_for_leg(s, book, weth, pull, sell_addr, tol)
                    } else if let Some(hub) = &s.leg.exit.hub {
                        // Sold into the hub token, bought from it; a hub
                        // other than WETH closes its leftover into WETH.
                        let hub_addr = token(view, hub.hub)?;
                        hub_swaps_for_leg(
                            &s.leg.exit,
                            hub,
                            book,
                            weth,
                            hub_addr,
                            pull,
                            debt_addr,
                            sell_addr,
                            overshoot,
                            tol,
                        )
                    } else {
                        swaps_for_leg(s, book, weth, pull, debt_addr, sell_addr, overshoot)
                    }
                };
                // Curve's exact-input legs overshoot by the tolerance. Should
                // this leg be the only one of its group to fill, one of its
                // own legs must buy the premium too: the Executor adds it to
                // a pool exact output, and with none here the Curve legs
                // overshoot by the whole premium.
                let (mut repay, profit) = legs_at(tol)?;
                if premium != 0 && !buys_premium(&repay) {
                    (repay, _) = legs_at(tol.saturating_add(premium_bps_of(premium, pull)?))?;
                }
                // Tied to their leg: the Executor skips them should it not
                // fill. What takes a whole balance spends what arrived.
                for r in &mut repay {
                    if r.flags & LEG_TAKE_BALANCE == 0 {
                        r.flags = tie_flags(r.flags, k).ok_or(AssembleError::Missing("tie"))?;
                    }
                }
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
            groups.push(FlashGroup {
                provider: cg.provider,
                flash_source: cg.source,
                debt_asset: debt_addr,
                flash_amount: flash_amt,
                fee_bps: cg.fee_bps,
                liqs,
                repay_swaps,
            });
            floor_groups.push(FloorGroup {
                legs: members,
                per_eth,
                premium: U256::from(premium),
            });
            group_fee_bps.push(cg.fee_bps);
            // Nothing to fall back to: a reward-only group borrows nothing,
            // and a flash swap's repay blob has no leg a loan could stand
            // behind (re-encoding it onto a source would leave the debt
            // unbought).
            if g.reward_only || flash_swap {
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
    let min_profit_wei = min_profit_floor(&floor_groups, bid, gas_cost_wei, tol)?;
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
        let surplus = U256::from(g.flash_amount.saturating_sub(pull));
        match closer_pair(book, g.debt_asset, weth, surplus) {
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
/// fee is refused (`FeeIncreased`): the floor and any Curve overshoot were
/// sized for this one. The swaps stay as they are; the Executor buys
/// whatever premium the new source charges.
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
    g.fee_bps = next.fee_bps;
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
    // Off the wire, but validation prices the premium with it: kept at the
    // old source's, a fee-free next source would be an unpriceable fee.
    g.fee_bps = next.fee_bps;
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
    use crate::profit::MarketView;
    use crate::select::{learning_p, select, PositionInput, SelectCfg};
    use crate::solver::Pool;
    use alloy_primitives::{Address, B256, I256, U256};
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

    /// The closer's pool decides what the leftover is worth, since it sells
    /// the whole balance at no minimum. Two pools at the same price: a thin
    /// one listed first, a deep one second. For a 5-token leftover the deep
    /// pool pays more (oracle: the constant-product formula on each). The
    /// first-listed pool used to win however thin (block 26,103,141: the 1 %
    /// LINK/WETH pool, 43 % under the oracle price).
    #[test]
    fn the_closer_sells_through_the_pool_that_pays_most() {
        let mut assets = std::collections::HashMap::new();
        assets.insert(tok(0), A0);
        assets.insert(tok(1), A1);
        let mut bk = PoolBook::new(assets, None, HOP_GAS);
        bk.add(v2(1, e18(10), e18(10))).unwrap();
        bk.add(v2(2, e18(10_000), e18(10_000))).unwrap();
        let (venue, data) = closer_pair(&bk, tok(0), tok(1), e18(5)).unwrap();
        assert_eq!(venue, VENUE_UNIV2_POOL);
        assert_eq!(&data[..20], addr(2).as_slice(), "the deep pool");
        // 5 in: thin pays 10·5·0.997/(10+5·0.997) ≈ 3.33, deep ≈ 4.98.
        let thin = bk
            .get(PoolId(0))
            .unwrap()
            .quote_exact_in(0, 1, e18(5))
            .unwrap();
        let deep = bk
            .get(PoolId(1))
            .unwrap()
            .quote_exact_in(0, 1, e18(5))
            .unwrap();
        assert!(deep > thin + e18(1), "{deep} vs {thin}");
    }

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
            wrap_gas: [366_332, 355_632, 460_032, 370_435, 384_134, 0, 0],
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

    fn v2_ab(
        n: u64,
        a: liq_types::AssetId,
        b: liq_types::AssetId,
        ta: Address,
        tb: Address,
        reserve: U256,
    ) -> Pool {
        let mut p = crate::fixtures::v2(n, reserve, reserve);
        p.assets = smallvec::SmallVec::from_slice(&[a, b]);
        p.tokens = smallvec::SmallVec::from_slice(&[ta, tb]);
        p
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

    /// A pool-direct V3 leg is the pool address, then for a fork the factory
    /// id of its deployer (1 SushiSwap, 2 PancakeSwap); a Uniswap pool is the
    /// bare address (`liq_wire::wire::V3_FACTORY_*`). A V4 pool is untouched.
    #[test]
    fn a_v3_leg_names_the_factory_of_a_fork_pool() {
        for (factory, tail) in [(0u8, None), (1, Some(1u8)), (2, Some(2))] {
            let mut p = v3_ab(7, A0, A1, tok(0), tok(1));
            if let crate::solver::PoolState::V3(s) = &mut p.state {
                s.factory = factory;
            }
            let bk = book(vec![p]);
            let (venue, data) = venue_bytes(&bk, PoolId(0), 0, 1).unwrap();
            assert_eq!(venue, VENUE_UNIV3_POOL);
            let mut want = addr(7).to_vec();
            want.extend(tail);
            assert_eq!(data, want, "factory {factory}");
        }
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

    /// The floor's terms in wei, worked by hand from the assembled plan's
    /// own legs: `swap_out` (rounded down) and `s` (rounded up) through
    /// `per_eth`, the tolerance slack on what moved, the group's premium,
    /// and the plan's gas cost once.
    fn floor_by_hand(
        legs: &[&crate::profit::SizedLeg],
        per_eth: U256,
        premium: U256,
        gas_cost_wei: u128,
        bid_bps: u16,
        tol: u16,
    ) -> u128 {
        let weakest = legs
            .iter()
            .map(|l| {
                let moved = l.swap_out * WEI / per_eth;
                let owed = (l.s * WEI).div_ceil(per_eth);
                let slack = (moved * U256::from(tol)).div_ceil(U256::from(10_000u32));
                I256::from_raw(moved) - I256::from_raw(owed) - I256::from_raw(slack)
            })
            .min()
            .unwrap();
        let premium_wei = (premium * WEI).div_ceil(per_eth);
        let worst =
            weakest - I256::from_raw(premium_wei) - I256::from_raw(U256::from(gas_cost_wei));
        if !worst.is_positive() {
            return 1;
        }
        let keep =
            worst.into_raw() * U256::from(10_000 - u32::from(bid_bps)) / U256::from(10_000u32);
        u128::try_from(keep).unwrap().max(1)
    }

    /// Units regression: a leg's value is in debt units and the gas cost in
    /// wei. A non-WETH debt (here priced like USDC: 3000e6 raw per ETH) is
    /// converted to wei *before* gas is subtracted.
    #[test]
    fn profit_floor_converts_debt_value_before_subtracting_wei_gas() {
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
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
        let tol = cfg().min_out_tolerance_bps;
        let assembled = assemble(
            &plans,
            &cfg(),
            &bk,
            &Usdc(&world),
            &vctx(tok(1)),
            &bd,
            &GAS,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let plan = &assembled[0].plan;
        let leg = &plans[0].groups[0].legs[0].leg;
        assert_eq!(
            plan.groups[0].provider,
            liq_types::FlashProvider::Morpho,
            "no premium"
        );
        let want = floor_by_hand(
            &[leg],
            U256::from(3_000_000_000u64),
            U256::ZERO,
            plan.gas_cost_wei,
            bd.coinbase_bps,
            tol,
        );
        assert_eq!(plan.min_profit_wei, want);
        assert!(want > 1, "a real floor, not the 1-wei stop");
    }

    /// Two positions in one Aave group (5 bps). Each leg's repay swaps are
    /// tied to it and buy its own pull; none buys the premium, which the
    /// Executor adds on chain. The floor is the worst landing: the weaker
    /// leg alone, less the premium on the whole flash (a beaten leg's share
    /// goes back unspent, its premium does not) and the plan's gas once.
    /// Oracle: the arithmetic from the legs' quoted values, and Aave's
    /// `percentMulCeil` premium on the flash amount.
    #[test]
    fn a_shared_group_ties_each_leg_and_floors_at_its_weakest_leg_alone() {
        let bk = book(vec![deep()]);
        let (_s, flash) = idx_aave();
        let big = q(1);
        let mut small = q(2);
        small.repay_options[0].max_repay = e18(4);
        small.seize_options[0].max_seize = e18(8);
        let mut world = world_one(&big);
        world.metas.insert(
            small.position,
            LegMeta {
                adapter: ExecutorAdapter::AaveV3,
                market: addr(0x51),
                borrower: small.key.user,
                tail: LegTail::None,
                protocol_pull: None,
            },
        );
        let inputs = [input_of(&big), input_of(&small)];
        let plans = select(&inputs, &cfg(), &flash, H, &bk, None, &world, &GAS).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].groups[0].legs.len(), 2, "one debt group of two");
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
        let assembled = assemble(
            &plans,
            &cfg(),
            &bk,
            &world,
            &vctx(tok(1)),
            &bd,
            &GAS,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let plan = &assembled[0].plan;
        assert_eq!(plan.groups.len(), 1, "one flash group");
        let g = &plan.groups[0];
        assert_eq!(g.liqs.len(), 2);
        assert_eq!(g.fee_bps, 5);
        for (k, l) in g.liqs.iter().enumerate() {
            let bought: u128 = g
                .repay_swaps
                .iter()
                .filter(|s| liq_plan::leg_tie(s.flags) == Some(k))
                .map(|s| {
                    assert_ne!(s.flags & LEG_EXACT_OUT, 0);
                    s.amount
                })
                .sum();
            assert_eq!(
                bought, l.protocol_pull,
                "leg {k} buys its own pull, no premium"
            );
        }
        assert!(
            g.repay_swaps
                .iter()
                .all(|s| liq_plan::leg_tie(s.flags).is_some()),
            "every repay swap is tied"
        );
        // Aave's percentMulCeil on the flash.
        let premium =
            (U256::from(g.flash_amount) * U256::from(5u32)).div_ceil(U256::from(10_000u32));
        let by_pos = |p: PositionId| {
            &plans[0].groups[0]
                .legs
                .iter()
                .find(|s| s.position == p)
                .unwrap()
                .leg
        };
        let legs = [by_pos(big.position), by_pos(small.position)];
        let tol = cfg().min_out_tolerance_bps;
        let want = floor_by_hand(&legs, WEI, premium, plan.gas_cost_wei, bd.coinbase_bps, tol);
        assert_eq!(plan.min_profit_wei, want);
        // The floor is the smaller leg alone: what the larger one alone would
        // keep is above it, and so is the two together.
        let alone = |l: &crate::profit::SizedLeg| {
            floor_by_hand(&[l], WEI, premium, plan.gas_cost_wei, bd.coinbase_bps, tol)
        };
        assert_eq!(want, alone(legs[1]));
        assert!(alone(legs[0]) > want);
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
        let assembled = assemble(
            &plans,
            &cfg(),
            &bk,
            &world,
            &vctx(tok(1)),
            &bd,
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
        assert_eq!(
            &repay[0].data[20..],
            &[0u8, 1u8, CURVE_HANDLER],
            "coin i = coll, j = debt, then the pool's MetaRegistry handler"
        );
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

    /// A Curve-only exit under Aave's 5 bps. Nothing in the leg's repay is a
    /// pool exact output, which is all the Executor adds the premium to, so
    /// the Curve leg's overshoot carries the whole premium on the flash
    /// (pull and over-borrow) beside the min-out tolerance: should the leg
    /// fill alone, its own sale buys it. Oracle: at the quoted rate, what
    /// the leg sells buys the pull and the premium; Aave's `percentMulCeil`
    /// premium.
    #[test]
    fn a_curve_only_leg_overshoots_by_the_whole_premium() {
        use liq_plan::VENUE_CURVE_POOL;
        let bk = book(vec![crate::fixtures::curve(
            7,
            &[e18(10_000_000), e18(10_000_000)],
            100_000,
            4_000_000,
        )]);
        let (_s, flash) = idx_aave();
        let quote = q(1);
        let mut world = world_one(&quote);
        world.tokens.remove(&A2);
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
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
        let assembled = assemble(
            &plans,
            &cfg(),
            &bk,
            &world,
            &vctx(tok(1)),
            &bd,
            &GAS,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let g = &assembled[0].plan.groups[0];
        assert_eq!((g.provider, g.fee_bps), (liq_types::FlashProvider::Aave, 5));
        let repay = &g.repay_swaps;
        assert_eq!(repay.len(), 1);
        assert_eq!(repay[0].venue, VENUE_CURVE_POOL);
        assert_eq!(
            repay[0].flags,
            tie_flags(0, 0).unwrap(),
            "exact-in, tied to its leg"
        );
        let leg = &plans[0].groups[0].legs[0].leg;
        let pull = g.liqs[0].protocol_pull;
        let premium =
            (U256::from(g.flash_amount) * U256::from(5u32)).div_ceil(U256::from(10_000u32));
        let premium_bps = (premium * U256::from(10_000u32)).div_ceil(U256::from(pull));
        let alloc = &leg.exit.allocs[0];
        let base = mul_div(alloc.amount_in, leg.s, alloc.amount_out, Rounding::Up).unwrap();
        let tol = U256::from(cfg().min_out_tolerance_bps);
        let want = mul_div(
            base,
            U256::from(10_000u32) + tol + premium_bps,
            U256::from(10_000u32),
            Rounding::Up,
        )
        .unwrap();
        assert_eq!(U256::from(repay[0].amount), want);
        let bought = mul_div(want, alloc.amount_out, alloc.amount_in, Rounding::Down).unwrap();
        assert!(
            bought >= U256::from(pull) + premium,
            "at the quoted rate the sale buys the pull and the premium"
        );
        validate(&assembled[0].plan, &vctx(tok(1))).unwrap();
    }

    /// An exit through WETH, end to end. A0 collateral, A1 debt, A2 WETH,
    /// and no pool between A0 and A1, so the exit is the hub's. The repay
    /// blob sells all the collateral into WETH (one pool, the whole
    /// balance: TAKE_BALANCE, which closes it), then buys exactly the pull
    /// of A1 with WETH. No profit leg sells A0. Oracle: PLAN-ENCODING's leg
    /// semantics and `validate`; the Executor runs this shape at block
    /// 26,106,490 in the historical replay.
    #[test]
    fn an_exit_through_weth_sells_the_collateral_then_buys_the_debt() {
        let mut bk = book(vec![
            v3_ab(1, A0, A2, tok(0), tok(2)),
            v3_ab(2, A2, A1, tok(2), tok(1)),
        ]);
        bk.set_hub(A2);
        let (_s, flash) = idx();
        let quote = q(1);
        let world = world_one(&quote);
        let mut c = cfg();
        c.weth = Some(A2);
        let plans = select(&[input_of(&quote)], &c, &flash, H, &bk, None, &world, &GAS).unwrap();
        assert!(
            plans[0].groups[0].legs[0].leg.exit.hub.is_some(),
            "the hub's exit"
        );
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
        let assembled = assemble(
            &plans,
            &c,
            &bk,
            &world,
            &vctx(tok(2)),
            &bd,
            &GAS,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let plan = &assembled[0].plan;
        let g = &plan.groups[0];
        let pull = g.liqs[0].protocol_pull;
        let r = &g.repay_swaps;
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(
            (r[0].token_in, r[0].token_out, r[0].flags),
            (tok(0), tok(2), LEG_TAKE_BALANCE)
        );
        assert_eq!(&r[0].data[..20], addr(1).as_slice());
        assert_eq!(
            (r[1].token_in, r[1].token_out, r[1].amount),
            (tok(2), tok(1), pull),
            "exactly the pull"
        );
        assert_eq!(
            r[1].flags,
            tie_flags(LEG_EXACT_OUT, 0).unwrap(),
            "exact output, tied to its leg"
        );
        assert_eq!(&r[1].data[..20], addr(2).as_slice());
        assert!(
            plan.profit_swaps.iter().all(|s| s.token_in != tok(0)),
            "the repay blob closed A0"
        );
        validate(plan, &vctx(tok(2))).unwrap();
    }

    /// An exit through a token the graph proposes, end to end. A0
    /// collateral, A1 debt, A2 WETH, A3 the intermediate: A0 trades only
    /// against A3, and A3 against A1 and WETH (A1 has its WETH pool, as a
    /// debt does). Without the graph there is
    /// no exit; with it the repay blob sells all A0 into A3 (TAKE_BALANCE),
    /// buys exactly the pull of A1 with A3, and the profit blob closes the
    /// A3 left into WETH (TAKE_BALANCE). Oracle: PLAN-ENCODING's leg
    /// semantics and `validate`.
    #[test]
    fn an_exit_through_a_graph_hub_closes_its_leftover_into_weth() {
        const A3: AssetId = AssetId(3);
        let mut assets = HashMap::new();
        for (t, a) in [(tok(0), A0), (tok(1), A1), (tok(2), A2), (tok(3), A3)] {
            assets.insert(t, a);
        }
        let mut bk = PoolBook::new(assets, None, HOP_GAS);
        for pool in [
            v3_ab(1, A0, A3, tok(0), tok(3)),
            v3_ab(2, A3, A1, tok(3), tok(1)),
            v3_ab(3, A3, A2, tok(3), tok(2)),
            // The debt's own WETH pool (the plan routes surplus debt there).
            v3_ab(4, A1, A2, tok(1), tok(2)),
        ] {
            bk.add(pool).unwrap();
        }
        bk.set_hub(A2);
        assert!(
            solve_pair(&bk, A0, A1, e18(10), &GAS, &B).is_err(),
            "no exit without the graph"
        );
        let g = crate::graph::GraphRoutes::build(&bk, |_| true, 3).unwrap();
        bk.set_graph(Some(std::sync::Arc::new(g)));
        let exit = solve_pair(&bk, A0, A1, e18(10), &GAS, &B).unwrap();
        assert_eq!(exit.hub.as_ref().map(|h| h.hub), Some(A3));
        let (_s, flash) = idx();
        let quote = q(1);
        let mut world = world_one(&quote);
        world.tokens.insert(A3, tok(3));
        let mut c = cfg();
        c.weth = Some(A2);
        let plans = select(&[input_of(&quote)], &c, &flash, H, &bk, None, &world, &GAS).unwrap();
        assert_eq!(
            plans[0].groups[0].legs[0]
                .leg
                .exit
                .hub
                .as_ref()
                .map(|h| h.hub),
            Some(A3)
        );
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
        let assembled = assemble(
            &plans,
            &c,
            &bk,
            &world,
            &vctx(tok(2)),
            &bd,
            &GAS,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let plan = &assembled[0].plan;
        let g = &plan.groups[0];
        let pull = g.liqs[0].protocol_pull;
        let r = &g.repay_swaps;
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(
            (r[0].token_in, r[0].token_out, r[0].flags),
            (tok(0), tok(3), LEG_TAKE_BALANCE)
        );
        assert_eq!(
            (r[1].token_in, r[1].token_out, r[1].amount),
            (tok(3), tok(1), pull)
        );
        assert_eq!(r[1].flags, tie_flags(LEG_EXACT_OUT, 0).unwrap());
        assert!(
            plan.profit_swaps.iter().any(|s| s.token_in == tok(3)
                && s.token_out == tok(2)
                && s.flags & LEG_TAKE_BALANCE != 0),
            "the A3 left closes into WETH: {:?}",
            plan.profit_swaps
        );
        validate(plan, &vctx(tok(2))).unwrap();
    }

    /// A chain exit, end to end. A0 collateral, A1 debt, A2 WETH; A0
    /// reaches A1 only through A3 and A4 (A0→A3 V3, A3→A4 V2, A4→A1 V3).
    /// With the graph the exit is the chain: the repay blob is one
    /// exact-output venue-10 leg of exactly the pull, whose data names its
    /// three hops (V3 0.05 %, V2 Uniswap, V3 0.05 %) and A3 and A4; the
    /// profit blob closes the A0 left into WETH. Oracle: the venue-10
    /// layout (`liq_wire`) and `validate`.
    #[test]
    fn a_chain_exit_buys_the_pull_along_its_path() {
        const A3: AssetId = AssetId(3);
        const A4: AssetId = AssetId(4);
        let mut assets = HashMap::new();
        for (t, a) in [
            (tok(0), A0),
            (tok(1), A1),
            (tok(2), A2),
            (tok(3), A3),
            (tok(4), A4),
        ] {
            assets.insert(t, a);
        }
        let mut bk = PoolBook::new(assets, None, HOP_GAS);
        let mut pair = crate::fixtures::v2(5, e18(1_000_000), e18(1_000_000));
        pair.assets = smallvec::SmallVec::from_slice(&[A3, A4]);
        pair.tokens = smallvec::SmallVec::from_slice(&[tok(3), tok(4)]);
        for pool in [
            v3_ab(1, A0, A3, tok(0), tok(3)),
            pair,
            v3_ab(6, A4, A1, tok(4), tok(1)),
            // The collateral's and the debt's WETH pools, shallow.
            v2_ab(7, A0, A2, tok(0), tok(2), e18(10)),
            v2_ab(8, A1, A2, tok(1), tok(2), e18(10)),
        ] {
            bk.add(pool).unwrap();
        }
        bk.set_hub(A2);
        let g = crate::graph::GraphRoutes::build(&bk, |_| true, 4).unwrap();
        bk.set_graph(Some(std::sync::Arc::new(g)));
        let exit = solve_pair(&bk, A0, A1, e18(10), &GAS, &B).unwrap();
        let c = exit.chain.as_ref().unwrap();
        assert_eq!(c.hops.len(), 3);
        let (_s, flash) = idx();
        let quote = q(1);
        let mut world = world_one(&quote);
        world.tokens.insert(A3, tok(3));
        world.tokens.insert(A4, tok(4));
        let mut cfgc = cfg();
        cfgc.weth = Some(A2);
        let plans = select(
            &[input_of(&quote)],
            &cfgc,
            &flash,
            H,
            &bk,
            None,
            &world,
            &GAS,
        )
        .unwrap();
        assert!(plans[0].groups[0].legs[0].leg.exit.chain.is_some());
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
        let assembled = assemble(
            &plans,
            &cfgc,
            &bk,
            &world,
            &vctx(tok(2)),
            &bd,
            &GAS,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let plan = &assembled[0].plan;
        let g0 = &plan.groups[0];
        let pull = g0.liqs[0].protocol_pull;
        let r = &g0.repay_swaps;
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(
            (r[0].venue, r[0].token_in, r[0].token_out, r[0].amount),
            (liq_plan::VENUE_CHAIN, tok(0), tok(1), pull)
        );
        assert_eq!(r[0].flags & LEG_EXACT_OUT, LEG_EXACT_OUT);
        let mut want = vec![3u8, 0, 0x00, 0x01, 0xf4, 2, 0, 0, 0, 0, 0x00, 0x01, 0xf4];
        want.extend_from_slice(tok(3).as_slice());
        want.extend_from_slice(tok(4).as_slice());
        assert_eq!(r[0].data, want);
        assert!(plan.profit_swaps.iter().any(|s| s.token_in == tok(0)
            && s.token_out == tok(2)
            && s.flags & LEG_TAKE_BALANCE != 0));
        validate(plan, &vctx(tok(2))).unwrap();
    }

    /// The venue-10 data of a V3 (0.05 %) hop then a V2 (Uniswap) hop
    /// through `mid`.
    fn v3_v2_data(mid: Address) -> Vec<u8> {
        let mut d = vec![2u8, 0, 0x00, 0x01, 0xf4, 2, 0, 0, 0];
        d.extend_from_slice(mid.as_slice());
        d
    }

    /// A collateral with no pool into WETH closes its leftover along an
    /// exact-input chain. A0 collateral, A1 debt, A2 WETH; A0 sells into
    /// A1 directly, and reaches WETH only through A3 (A0→A3 V3, A3→A2 V2).
    /// Without the graph there is no closer and no plan; with it the profit
    /// blob sells the whole A0 balance (TAKE_BALANCE, exact input) along
    /// the chain. Oracle: the venue-10 layout (`liq_wire`) and `validate`.
    #[test]
    fn a_leftover_without_a_weth_pool_closes_along_a_chain() {
        const A3: AssetId = AssetId(3);
        let mut assets = HashMap::new();
        for (t, a) in [(tok(0), A0), (tok(1), A1), (tok(2), A2), (tok(3), A3)] {
            assets.insert(t, a);
        }
        let mut bk = PoolBook::new(assets, None, HOP_GAS);
        for pool in [
            v3_ab(1, A0, A1, tok(0), tok(1)),
            v3_ab(2, A0, A3, tok(0), tok(3)),
            v2_ab(3, A3, A2, tok(3), tok(2), e18(1_000_000)),
            v2_ab(4, A1, A2, tok(1), tok(2), e18(10)),
        ] {
            bk.add(pool).unwrap();
        }
        bk.set_hub(A2);
        let (_s, flash) = idx();
        let quote = q(1);
        let mut world = world_one(&quote);
        world.tokens.insert(A3, tok(3));
        let mut cfgc = cfg();
        cfgc.weth = Some(A2);
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
        let run = |bk: &PoolBook| {
            let plans = select(
                &[input_of(&quote)],
                &cfgc,
                &flash,
                H,
                bk,
                None,
                &world,
                &GAS,
            )
            .ok()?;
            assemble(
                &plans,
                &cfgc,
                bk,
                &world,
                &vctx(tok(2)),
                &bd,
                &GAS,
                FLAG_SWEEP,
                &flash,
                H,
            )
            .ok()
        };
        assert!(run(&bk).is_none(), "no closer without the graph");
        let g = crate::graph::GraphRoutes::build(&bk, |_| true, 4).unwrap();
        bk.set_graph(Some(std::sync::Arc::new(g)));
        let assembled = run(&bk).unwrap();
        let plan = &assembled[0].plan;
        let r = &plan.groups[0].repay_swaps;
        assert!(r.iter().all(|l| l.venue == VENUE_UNIV3_POOL), "{r:?}");
        let closers: Vec<_> = plan
            .profit_swaps
            .iter()
            .filter(|l| l.token_in == tok(0))
            .collect();
        assert_eq!(closers.len(), 1, "{plan:?}");
        let c = closers[0];
        assert_eq!(
            (c.venue, c.token_out, c.flags, c.amount),
            (liq_plan::VENUE_CHAIN, tok(2), LEG_TAKE_BALANCE, 0)
        );
        assert_eq!(c.data, v3_v2_data(tok(3)));
        validate(plan, &vctx(tok(2))).unwrap();
    }

    /// Unwrap, then a chain, then a chain closer. A2 wraps A0 (ERC-4626,
    /// 1.1); A1 debt; A4 WETH. A0 reaches A1 only along three hops
    /// (A0→A3 V3, A3→A5 V2, A5→A1 V3), so no hub exit exists, and reaches
    /// WETH only through A3 (A0→A3 V3, A3→A4 V2). The exit unwraps the
    /// seize, buys the pull along the three hops exact output, and is
    /// charged the unwrap's gas, the three hops' and the closer chain's two.
    /// The repay blob is the unwrap then the venue-10 leg from A0; the
    /// profit blob closes A0 along the two-hop chain. Oracle: the venue-10
    /// layout, the pools' hop gas, and `validate`.
    #[test]
    fn a_wrapped_collateral_unwraps_then_chains() {
        const A3: AssetId = AssetId(3);
        const A4: AssetId = AssetId(4);
        const A5: AssetId = AssetId(5);
        let mut assets = HashMap::new();
        for n in 0..6u64 {
            assets.insert(tok(n), AssetId(u16::try_from(n).unwrap()));
        }
        let mut bk = PoolBook::new(assets, None, HOP_GAS);
        for pool in [
            v3_ab(1, A0, A3, tok(0), tok(3)),
            v2_ab(2, A3, A5, tok(3), tok(5), e18(1_000_000)),
            v3_ab(3, A5, A1, tok(5), tok(1)),
            v2_ab(4, A3, A4, tok(3), tok(4), e18(1_000_000)),
            v2_ab(5, A1, A4, tok(1), tok(4), e18(10)),
        ] {
            bk.add(pool).unwrap();
        }
        bk.set_hub(A4);
        bk.add_unwrap(unwrap_a2(A0));
        let g = crate::graph::GraphRoutes::build(&bk, |_| true, 4).unwrap();
        bk.set_graph(Some(std::sync::Arc::new(g)));

        let exit = solve_pair(&bk, A2, A1, e18(10), &GAS, &B).unwrap();
        let u = exit.unwrap.unwrap();
        assert_eq!((u.wrapper, u.into), (A2, A0));
        let c = exit.chain.as_ref().unwrap();
        assert_eq!(c.hops.len(), 3);
        let hop = |k: usize| bk.pools()[k].hop_gas;
        assert_eq!(
            exit.hop_gas,
            60_000 + hop(0) + hop(1) + hop(2) + hop(0) + hop(3) + 2 * crate::graph::CHAIN_LEG_GAS,
            "unwrap, three hops, the two-hop closer, and each chain leg's own gas"
        );

        let (_s, flash) = idx();
        let mut quote = q(1);
        quote.seize_options[0].asset = A2;
        let mut world = world_one(&quote);
        for (a, n) in [(A3, 3), (A4, 4), (A5, 5)] {
            world.tokens.insert(a, tok(n));
        }
        let mut cfgc = cfg();
        cfgc.weth = Some(A4);
        let plans = select(
            &[input_of(&quote)],
            &cfgc,
            &flash,
            H,
            &bk,
            None,
            &world,
            &GAS,
        )
        .unwrap();
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
        let assembled = assemble(
            &plans,
            &cfgc,
            &bk,
            &world,
            &vctx(tok(4)),
            &bd,
            &GAS,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let plan = &assembled[0].plan;
        let g0 = &plan.groups[0];
        let pull = g0.liqs[0].protocol_pull;
        let r = &g0.repay_swaps;
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!((r[0].venue, r[0].token_in), (VENUE_UNWRAP_4626, tok(2)));
        assert_eq!(
            (r[1].venue, r[1].token_in, r[1].token_out, r[1].amount),
            (liq_plan::VENUE_CHAIN, tok(0), tok(1), pull)
        );
        assert_eq!(
            r[1].flags & (LEG_EXACT_OUT | LEG_TAKE_BALANCE),
            LEG_EXACT_OUT
        );
        assert_eq!(liq_plan::leg_tie(r[1].flags), Some(0), "tied to its leg");
        let mut want = vec![3u8, 0, 0x00, 0x01, 0xf4, 2, 0, 0, 0, 0, 0x00, 0x01, 0xf4];
        want.extend_from_slice(tok(3).as_slice());
        want.extend_from_slice(tok(5).as_slice());
        assert_eq!(r[1].data, want);
        let closers: Vec<_> = plan
            .profit_swaps
            .iter()
            .filter(|l| l.token_in == tok(0))
            .collect();
        assert_eq!(closers.len(), 1, "{plan:?}");
        assert_eq!(
            (closers[0].venue, closers[0].token_out, closers[0].flags),
            (liq_plan::VENUE_CHAIN, tok(4), LEG_TAKE_BALANCE)
        );
        assert_eq!(closers[0].data, v3_v2_data(tok(3)));
        validate(plan, &vctx(tok(4))).unwrap();
    }

    /// Output of a Uniswap V2 swap (0.3 % fee), by hand.
    fn v2_out(r_in: U256, r_out: U256, x: U256) -> U256 {
        let xf = x * U256::from(997u64);
        r_out * xf / (r_in * U256::from(1000u64) + xf)
    }

    /// A seize too large for either route alone splits between them (4E).
    /// A0 collateral, A1 debt, A2 WETH; a direct A0/A1 V2 pool and a chain
    /// A0→A3→A1 through two V2 pools, all of 1,000 a side. Selling 400 A0,
    /// either alone leaves much on the curve. The split's output equals the
    /// direct pool's quote of its part plus the chain's of the rest, both by
    /// the V2 formula; it beats either route alone, and moving 1 % of the
    /// sale either way does no better. The repay blob is one exact-output
    /// V2 leg and one chain leg whose amounts add to the pull. Oracle: the
    /// V2 formula by hand and `validate`.
    #[test]
    fn a_large_exit_splits_between_the_pool_and_a_chain() {
        const A3: AssetId = AssetId(3);
        let mut assets = HashMap::new();
        for (t, a) in [(tok(0), A0), (tok(1), A1), (tok(2), A2), (tok(3), A3)] {
            assets.insert(t, a);
        }
        let mut bk = PoolBook::new(assets, None, HOP_GAS);
        let r = e18(1_000);
        for pool in [
            v2_ab(1, A0, A1, tok(0), tok(1), r),
            v2_ab(2, A0, A3, tok(0), tok(3), r),
            v2_ab(3, A3, A1, tok(3), tok(1), r),
            v2_ab(4, A0, A2, tok(0), tok(2), e18(10)),
            v2_ab(5, A1, A2, tok(1), tok(2), e18(10)),
        ] {
            bk.add(pool).unwrap();
        }
        bk.set_hub(A2);
        let g = crate::graph::GraphRoutes::build(&bk, |_| true, 4).unwrap();
        bk.set_graph(Some(std::sync::Arc::new(g)));
        let sold = e18(400);
        let direct = |x: U256| v2_out(r, r, x);
        let chain = |y: U256| v2_out(r, r, v2_out(r, r, y));
        let exit = crate::exact::refine_exit(
            &bk,
            A0,
            A1,
            &FREE,
            &B,
            solve_pair(&bk, A0, A1, sold, &FREE, &B).unwrap(),
        )
        .unwrap();
        let c = exit.chain.as_ref().unwrap();
        assert_eq!(c.hops.len(), 2);
        let x = exit
            .allocs
            .iter()
            .map(|a| a.amount_in)
            .fold(U256::ZERO, |a, b| a + b);
        assert!(!x.is_zero() && x < sold, "both sides sell: {x}");
        assert_eq!(c.amount_out, chain(sold - x), "the chain's part, by hand");
        assert_eq!(exit.amount_out, direct(x) + chain(sold - x));
        assert!(exit.amount_out > direct(sold) && exit.amount_out > chain(sold));
        let step = sold / U256::from(100u64);
        for y in [x - step, x + step] {
            assert!(
                direct(y) + chain(sold - y) <= exit.amount_out,
                "no better 1 % away"
            );
        }

        let (_s, flash) = idx();
        let quote = q(1);
        let mut world = world_one(&quote);
        world.tokens.insert(A3, tok(3));
        let mut cfgc = cfg();
        cfgc.weth = Some(A2);
        let plans = select(
            &[input_of(&quote)],
            &cfgc,
            &flash,
            H,
            &bk,
            None,
            &world,
            &FREE,
        )
        .unwrap();
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
        let assembled = assemble(
            &plans,
            &cfgc,
            &bk,
            &world,
            &vctx(tok(2)),
            &bd,
            &FREE,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let plan = &assembled[0].plan;
        let g0 = &plan.groups[0];
        let pull = g0.liqs[0].protocol_pull;
        let r = &g0.repay_swaps;
        let venues: Vec<u8> = r.iter().map(|l| l.venue).collect();
        assert_eq!(
            venues,
            vec![VENUE_UNIV2_POOL, liq_plan::VENUE_CHAIN],
            "{r:?}"
        );
        assert!(r.iter().all(|l| l.flags & LEG_EXACT_OUT != 0));
        assert_eq!(r[0].amount + r[1].amount, pull, "the parts add to the pull");
        validate(plan, &vctx(tok(2))).unwrap();
    }

    /// A sale too large for any two routes splits across three (4E): the
    /// direct A0/A1 pool and two chains, A0→A3→A1 and A0→A4→A1, sharing no
    /// pool, every pool V2 with 1,000 a side. Selling 600 A0, each route's
    /// output is the V2 formula on its part and the parts add up; the total
    /// beats every split across two of them on a 1 % grid; the repay blob is one V2 leg
    /// and two chain legs whose amounts add to the pull. Oracle: the V2
    /// formula by hand and `validate`.
    #[test]
    fn a_larger_exit_splits_across_the_pool_and_two_chains() {
        const A3: AssetId = AssetId(3);
        const A4: AssetId = AssetId(4);
        let mut assets = HashMap::new();
        for (t, a) in [
            (tok(0), A0),
            (tok(1), A1),
            (tok(2), A2),
            (tok(3), A3),
            (tok(4), A4),
        ] {
            assets.insert(t, a);
        }
        let mut bk = PoolBook::new(assets, None, HOP_GAS);
        let r = e18(1_000);
        for pool in [
            v2_ab(1, A0, A1, tok(0), tok(1), r),
            v2_ab(2, A0, A3, tok(0), tok(3), r),
            v2_ab(3, A3, A1, tok(3), tok(1), r),
            v2_ab(4, A0, A4, tok(0), tok(4), r),
            v2_ab(5, A4, A1, tok(4), tok(1), r),
            v2_ab(6, A0, A2, tok(0), tok(2), e18(10)),
            v2_ab(7, A1, A2, tok(1), tok(2), e18(10)),
        ] {
            bk.add(pool).unwrap();
        }
        bk.set_hub(A2);
        let g = crate::graph::GraphRoutes::build(&bk, |_| true, 4).unwrap();
        bk.set_graph(Some(std::sync::Arc::new(g)));
        let sold = e18(600);
        let direct = |x: U256| v2_out(r, r, x);
        let chain = |y: U256| v2_out(r, r, v2_out(r, r, y));
        let exit = crate::exact::refine_exit(
            &bk,
            A0,
            A1,
            &FREE,
            &B,
            solve_pair(&bk, A0, A1, sold, &FREE, &B).unwrap(),
        )
        .unwrap();
        let c = exit.chain.as_ref().unwrap();
        assert_eq!(c.with.len(), 1, "two chains");
        let x = exit
            .allocs
            .iter()
            .map(|a| a.amount_in)
            .fold(U256::ZERO, |a, b| a + b);
        let parts = [c.amount_out, c.with[0].amount_out];
        assert!(
            !x.is_zero() && parts.iter().all(|p| !p.is_zero()),
            "all three sell"
        );
        assert_eq!(exit.amount_out, direct(x) + parts[0] + parts[1]);
        // Each chain's part is the V2 formula on what it sold (found by
        // bisection: the formula is monotone): the two chains sold `sold -
        // x` between them.
        let sold_by = |part: U256| {
            let (mut lo, mut hi) = (U256::ZERO, sold - x);
            while lo < hi {
                let mid = (lo + hi) / U256::from(2u64);
                if chain(mid) < part {
                    lo = mid + U256::ONE;
                } else {
                    hi = mid;
                }
            }
            assert_eq!(
                chain(lo),
                part,
                "a chain's output is the V2 formula on its share"
            );
            lo
        };
        // The formula is flat over a few wei of input: the smallest input
        // paying each part may sit a wei or two under the share.
        let shares = sold_by(parts[0]) + sold_by(parts[1]);
        assert!(
            shares <= sold - x && sold - x - shares <= U256::from(2u64),
            "the chains' shares add to the rest: {shares} vs {}",
            sold - x
        );
        // No split across only two routes does better.
        let step = sold / U256::from(100u64);
        let mut best_two = U256::ZERO;
        let mut y = U256::ZERO;
        while y <= sold {
            best_two = best_two
                .max(direct(sold - y) + chain(y))
                .max(chain(sold - y) + chain(y));
            y += step;
        }
        assert!(
            exit.amount_out > best_two,
            "{} vs {}",
            exit.amount_out,
            best_two
        );

        let (_s, flash) = idx();
        let quote = q(1);
        let mut world = world_one(&quote);
        world.tokens.insert(A3, tok(3));
        world.tokens.insert(A4, tok(4));
        let mut cfgc = cfg();
        cfgc.weth = Some(A2);
        let plans = select(
            &[input_of(&quote)],
            &cfgc,
            &flash,
            H,
            &bk,
            None,
            &world,
            &FREE,
        )
        .unwrap();
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
        let assembled = assemble(
            &plans,
            &cfgc,
            &bk,
            &world,
            &vctx(tok(2)),
            &bd,
            &FREE,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let plan = &assembled[0].plan;
        let g0 = &plan.groups[0];
        let pull = g0.liqs[0].protocol_pull;
        let rl = &g0.repay_swaps;
        let venues: Vec<u8> = rl.iter().map(|l| l.venue).collect();
        assert_eq!(
            venues,
            vec![
                VENUE_UNIV2_POOL,
                liq_plan::VENUE_CHAIN,
                liq_plan::VENUE_CHAIN
            ],
            "{rl:?}"
        );
        assert!(rl.iter().all(|l| l.flags & LEG_EXACT_OUT != 0));
        assert_eq!(
            rl.iter().map(|l| l.amount).sum::<u128>(),
            pull,
            "the parts add to the pull"
        );
        validate(plan, &vctx(tok(2))).unwrap();
    }

    /// A chain through a Curve pool repays exact input (plan 4F, full
    /// graph). A0 reaches the debt A1 only through Curve (A0/A3, a 2-coin
    /// stable pool) then a V2 pair (A3/A1). The repay blob is one venue-10
    /// leg without the exact-output flag; its amount is the least input whose
    /// quoted path output covers the pull, raised by the overshoot; its data
    /// names the Curve hop's extra by offset (pool, i, j, handler) after the
    /// V2 hop; the surplus debt is swept to WETH. Oracle: the path quote
    /// (the pools' own math), the venue-10 layout and `validate`.
    #[test]
    fn a_chain_through_curve_repays_exact_input() {
        const A3: AssetId = AssetId(3);
        let mut assets = HashMap::new();
        for (t, a) in [(tok(0), A0), (tok(1), A1), (tok(2), A2), (tok(3), A3)] {
            assets.insert(t, a);
        }
        let mut bk = PoolBook::new(assets, None, HOP_GAS);
        let mut c = crate::fixtures::curve(1, &[e18(1_000_000), e18(1_000_000)], 20_000, 4_000_000);
        c.assets = smallvec::SmallVec::from_slice(&[A0, A3]);
        c.tokens = smallvec::SmallVec::from_slice(&[tok(0), tok(3)]);
        for pool in [
            c,
            v2_ab(2, A3, A1, tok(3), tok(1), e18(1_000_000)),
            v2_ab(3, A0, A2, tok(0), tok(2), e18(1_000)),
            v2_ab(4, A1, A2, tok(1), tok(2), e18(1_000)),
        ] {
            bk.add(pool).unwrap();
        }
        bk.set_hub(A2);
        let g = crate::graph::GraphRoutes::build(&bk, |_| true, 4).unwrap();
        bk.set_graph(Some(std::sync::Arc::new(g)));
        let exit = solve_pair(&bk, A0, A1, e18(10), &GAS, &B).unwrap();
        let ch = exit.chain.as_ref().unwrap();
        assert!(!ch.exact_out, "a Curve hop sells exact input");

        let (_s, flash) = idx();
        let quote = q(1);
        let mut world = world_one(&quote);
        world.tokens.insert(A3, tok(3));
        let mut cfgc = cfg();
        cfgc.weth = Some(A2);
        let plans = select(
            &[input_of(&quote)],
            &cfgc,
            &flash,
            H,
            &bk,
            None,
            &world,
            &GAS,
        )
        .unwrap();
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
        let assembled = assemble(
            &plans,
            &cfgc,
            &bk,
            &world,
            &vctx(tok(2)),
            &bd,
            &GAS,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let plan = &assembled[0].plan;
        let g0 = &plan.groups[0];
        let pull = g0.liqs[0].protocol_pull;
        let r = &g0.repay_swaps;
        assert_eq!(r.len(), 1, "{r:?}");
        let leg = &r[0];
        assert_eq!(
            (leg.venue, leg.token_in, leg.token_out),
            (liq_plan::VENUE_CHAIN, tok(0), tok(1))
        );
        assert_eq!(
            leg.flags & (LEG_EXACT_OUT | LEG_TAKE_BALANCE),
            0,
            "exact input"
        );
        // The least input covering the pull, then the overshoot.
        let out = |x: U256| crate::exact::path_out(&bk, &ch.hops, x).unwrap();
        let need = crate::exact::path_in_for(&bk, &ch.hops, U256::from(pull)).unwrap();
        assert!(out(need) >= U256::from(pull) && out(need - U256::ONE) < U256::from(pull));
        let tol = U256::from(10_000u32 + u32::from(crate::select::MIN_OUT_TOLERANCE_BPS));
        assert_eq!(
            U256::from(leg.amount),
            (need * tol).div_ceil(U256::from(10_000u32)),
            "the input plus the overshoot"
        );
        // Data: 2 hops, the Curve hop's extra at 1 + 2 * 4 + 20 = 29.
        let mut want = vec![2u8, 3, 0, 0, 29, 2, 0, 0, 0];
        want.extend_from_slice(tok(3).as_slice());
        want.extend_from_slice(addr(1).as_slice());
        want.extend_from_slice(&[0, 1, crate::fixtures::CURVE_HANDLER]);
        assert_eq!(leg.data, want);
        assert!(
            plan.profit_swaps.iter().any(|s| s.token_in == tok(1)
                && s.token_out == tok(2)
                && s.flags & LEG_TAKE_BALANCE != 0),
            "the surplus debt is swept to WETH"
        );
        validate(plan, &vctx(tok(2))).unwrap();
    }

    /// A sale worth 5 ETH or more is routed as a flow (4E): slices go to
    /// whichever path pays most on the pools the earlier slices moved, so
    /// one hop is spread across parallel pools and paths share a pool. A0
    /// collateral, A1 debt (WETH-priced: `FREE` gas, one debt unit per ETH),
    /// every pool V2-priced with 1,000 a side: a direct A0/A1 pool, two A0/A3
    /// pools in parallel (Uniswap V2 and SushiSwap), and one A3/A1 pool both
    /// of those feed. Selling 900 A0, the flow uses all three A0 pools; its
    /// output is at least the best on a 5 % grid over how much each A0 pool
    /// takes (the shared A3/A1 pool takes the two chains' A3 one after the
    /// other, as two chain legs do), and beats every split that leaves one
    /// of them out. Oracle: the V2 formula by hand.
    #[test]
    fn a_large_sale_flows_across_parallel_and_shared_pools() {
        const A3: AssetId = AssetId(3);
        let mut assets = HashMap::new();
        for (t, a) in [(tok(0), A0), (tok(1), A1), (tok(2), A2), (tok(3), A3)] {
            assets.insert(t, a);
        }
        let mut bk = PoolBook::new(assets, None, HOP_GAS);
        let r = e18(1_000);
        // The second A0/A3 pool is a SushiSwap pair: one factory has one pair
        // per token pair, and a chain hop names it by factory.
        let mut sushi = v2_ab(3, A0, A3, tok(0), tok(3), r);
        if let crate::solver::PoolState::V2(st) = &mut sushi.state {
            st.factory = 1;
        }
        for pool in [
            v2_ab(1, A0, A1, tok(0), tok(1), r),
            v2_ab(2, A0, A3, tok(0), tok(3), r),
            sushi,
            v2_ab(4, A3, A1, tok(3), tok(1), r),
            v2_ab(5, A0, A2, tok(0), tok(2), e18(10)),
            v2_ab(6, A1, A2, tok(1), tok(2), e18(10)),
        ] {
            bk.add(pool).unwrap();
        }
        bk.set_hub(A2);
        let g = crate::graph::GraphRoutes::build(&bk, |_| true, 4).unwrap();
        bk.set_graph(Some(std::sync::Arc::new(g)));
        let sold = e18(900);
        // As `profit::evaluate` does once a leg is sized.
        let exit = crate::exact::refine_exit(
            &bk,
            A0,
            A1,
            &FREE,
            &B,
            solve_pair(&bk, A0, A1, sold, &FREE, &B).unwrap(),
        )
        .unwrap();
        let c = exit.chain.as_ref().unwrap();
        // The direct pool rides in `allocs` (the water-fill as one route),
        // the two-hop paths as chains.
        let first_pools: std::collections::HashSet<_> = std::iter::once(c.as_ref())
            .chain(c.with.iter())
            .filter_map(|ch| ch.hops.first().map(|l| l.pool))
            .chain(exit.allocs.iter().map(|a| a.leg.pool))
            .collect();
        assert_eq!(first_pools.len(), 3, "every A0 pool sells: {first_pools:?}");
        // By hand: x to the direct pool, a and b to the two A0/A3 pools; the
        // A3/A1 pool sells what each delivers, one chain after the other.
        let v2 = |x: U256| v2_out(r, r, x);
        let total = |x: U256, a: U256, b: U256| {
            let (ya, yb) = (v2(a), v2(b));
            let first = v2(ya);
            v2(x) + first + v2_out(r + ya, r - first, yb)
        };
        let step = sold / U256::from(20u64);
        let (mut grid_best, mut without_one) = (U256::ZERO, U256::ZERO);
        let mut x = U256::ZERO;
        while x <= sold {
            let mut a = U256::ZERO;
            while x + a <= sold {
                let b = sold - x - a;
                let t = total(x, a, b);
                grid_best = grid_best.max(t);
                if x.is_zero() || a.is_zero() || b.is_zero() {
                    without_one = without_one.max(t);
                }
                a += step;
            }
            x += step;
        }
        assert!(
            exit.amount_out >= grid_best,
            "{} vs grid {}",
            exit.amount_out,
            grid_best
        );
        assert!(
            exit.amount_out > without_one,
            "{} vs {}",
            exit.amount_out,
            without_one
        );
    }

    /// An exit through WETH buys its debt with WETH, which does not run out
    /// when its own liquidation is beaten, so in a group with another leg
    /// its WETH→debt leg would spend that leg's WETH or the Executor's. It
    /// never shares a flash group: two such positions on one debt are two
    /// plans, where two direct ones share a group.
    #[test]
    fn exits_through_weth_never_share_a_flash_group() {
        let (_s, flash) = idx();
        let (q1, q2) = (q(1), q(2));
        let mut world = world_one(&q1);
        let meta = LegMeta {
            borrower: q2.key.user,
            ..world.metas[&q1.position].clone()
        };
        world.metas.insert(q2.position, meta);
        let inputs = [input_of(&q1), input_of(&q2)];
        let mut c = cfg();
        c.weth = Some(A2);
        let direct = book(vec![v3_ab(3, A0, A1, tok(0), tok(1))]);
        let plans = select(&inputs, &c, &flash, H, &direct, None, &world, &GAS).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].groups.len(), 1);
        assert_eq!(
            plans[0].groups[0].legs.len(),
            2,
            "direct legs share a group"
        );

        let mut hub = book(vec![
            v3_ab(1, A0, A2, tok(0), tok(2)),
            v3_ab(2, A2, A1, tok(2), tok(1)),
        ]);
        hub.set_hub(A2);
        let plans = select(&inputs, &c, &flash, H, &hub, None, &world, &GAS).unwrap();
        assert_eq!(plans.len(), 2, "a plan each");
        for p in &plans {
            assert_eq!(p.groups.len(), 1);
            assert_eq!(p.groups[0].legs.len(), 1);
            assert!(p.groups[0].legs[0].leg.exit.hub.is_some());
        }
    }

    /// A leg funded by a flash swap assembles as a group the pool lends:
    /// provider `UniV3Swap`, the pool as source, the pull as the amount, no
    /// fee, no repay leg (the pool is paid the collateral inside its
    /// callback), the leftover collateral closed in the profit blob, no
    /// fallback source. Through WETH (the WETH/debt pool lends), the repay
    /// blob sells all the collateral into WETH and no profit leg sells it.
    /// Both validate.
    #[test]
    fn a_flash_swap_group_assembles_without_a_repay_leg() {
        let (_s, flash) = idx();
        let quote = q(1);
        let world = world_one(&quote);
        let mut c = cfg();
        c.wrap_gas[liq_types::FlashProvider::UniV3Swap as usize] = 300_000;
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();

        let bk = book(vec![deep()]);
        let plans = select(&[input_of(&quote)], &c, &flash, H, &bk, None, &world, &GAS).unwrap();
        let leg = &plans[0].groups[0].legs[0].leg;
        assert_eq!(leg.route.provider, liq_types::FlashProvider::UniV3Swap);
        let assembled = assemble(
            &plans,
            &c,
            &bk,
            &world,
            &vctx(tok(1)),
            &bd,
            &GAS,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let plan = &assembled[0].plan;
        let g = &plan.groups[0];
        assert_eq!(g.provider, liq_types::FlashProvider::UniV3Swap);
        assert_eq!(g.flash_source, addr(1));
        assert_eq!((g.fee_bps, g.flash_amount), (0, g.liqs[0].protocol_pull));
        assert!(g.repay_swaps.is_empty(), "{:?}", g.repay_swaps);
        assert!(
            plan.profit_swaps
                .iter()
                .any(|s| s.token_in == tok(0) && s.flags & LEG_TAKE_BALANCE != 0),
            "the leftover collateral closes in the profit blob"
        );
        assert!(
            assembled[0].fallbacks[0].is_empty(),
            "no source to fall back to"
        );
        validate(plan, &vctx(tok(1))).unwrap();

        let mut hub = book(vec![
            v3_ab(1, A0, A2, tok(0), tok(2)),
            v3_ab(2, A2, A1, tok(2), tok(1)),
        ]);
        hub.set_hub(A2);
        c.weth = Some(A2);
        let plans = select(&[input_of(&quote)], &c, &flash, H, &hub, None, &world, &GAS).unwrap();
        let leg = &plans[0].groups[0].legs[0].leg;
        assert_eq!(leg.route.provider, liq_types::FlashProvider::UniV3Swap);
        assert_eq!(leg.route.source, addr(2), "the WETH/debt pool lends");
        let assembled = assemble(
            &plans,
            &c,
            &hub,
            &world,
            &vctx(tok(2)),
            &bd,
            &GAS,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let plan = &assembled[0].plan;
        let g = &plan.groups[0];
        assert_eq!(
            (g.provider, g.flash_source),
            (liq_types::FlashProvider::UniV3Swap, addr(2))
        );
        assert_eq!(g.repay_swaps.len(), 1, "{:?}", g.repay_swaps);
        let r = &g.repay_swaps[0];
        assert_eq!(
            (r.token_in, r.token_out, r.flags),
            (tok(0), tok(2), LEG_TAKE_BALANCE)
        );
        assert_eq!(&r.data[..20], addr(1).as_slice());
        assert!(plan.profit_swaps.iter().all(|s| s.token_in != tok(0)));
        validate(plan, &vctx(tok(2))).unwrap();
    }

    /// Two positions on one debt, both funded by flash swaps, are two
    /// plans of one leg each: a flash swap's lender must be paid whatever
    /// filled, so its leg is never beside another.
    #[test]
    fn flash_swap_legs_never_share_a_flash_group() {
        let (_s, flash) = idx();
        let (q1, q2) = (q(1), q(2));
        let world = world_one(&q1);
        let mut c = cfg();
        c.wrap_gas[liq_types::FlashProvider::UniV3Swap as usize] = 300_000;
        let bk = book(vec![deep()]);
        let inputs = [input_of(&q1), input_of(&q2)];
        let plans = select(&inputs, &c, &flash, H, &bk, None, &world, &GAS).unwrap();
        assert_eq!(plans.len(), 2, "a plan each");
        for p in &plans {
            assert_eq!(p.groups.len(), 1);
            assert_eq!(p.groups[0].legs.len(), 1);
            let leg = &p.groups[0].legs[0].leg;
            assert_eq!(leg.route.provider, liq_types::FlashProvider::UniV3Swap);
            assert_eq!(p.groups[0].cascade.groups.as_slice(), &[leg.route]);
        }
    }

    /// A2 is an ERC-4626 wrapper of `into` at 1.1 assets per share.
    fn unwrap_a2(into: AssetId) -> crate::solver::Unwrap {
        crate::solver::Unwrap {
            kind: crate::solver::UnwrapKind::Erc4626,
            wrapper: A2,
            wrapper_token: tok(2),
            into,
            into_token: if into == A0 { tok(0) } else { tok(1) },
            rate: crate::solver::UnwrapRate::Linear {
                assets_per_scale: e18(11) / U256::from(10u64),
                max_into: None,
            },
            scale: e18(1),
            read_block: 1,
            gas: 60_000,
            expiry_gas: 0,
            cash_capped: false,
        }
    }

    /// An EVK vault pays at most its cash: 10 shares at 1.1 need 11 assets,
    /// so a cash of 10.9 refuses the unwrap (EVK `E_InsufficientCash`) and
    /// a cash of 11 pays it, less the rate haircut. Oracle: the rate by
    /// hand.
    #[test]
    fn a_cash_capped_unwrap_refuses_more_than_the_vault_holds() {
        let bk = book(vec![deep()]);
        let capped = |cap: U256| crate::solver::Unwrap {
            rate: crate::solver::UnwrapRate::Linear {
                assets_per_scale: e18(11) / U256::from(10u64),
                max_into: Some(cap),
            },
            cash_capped: true,
            ..unwrap_a2(A0)
        };
        let need = e18(11);
        assert!(matches!(
            capped(need - U256::ONE).convert(e18(10), &bk),
            Err(RouteError::InsufficientLiquidity)
        ));
        let paid = capped(need).convert(e18(10), &bk).unwrap();
        assert_eq!(paid, need - need / U256::from(1_000_000u64) - U256::ONE);
        assert_eq!(
            unwrap_a2(A0).convert(e18(10), &bk).unwrap(),
            paid,
            "uncapped pays the same"
        );
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
        let assembled = assemble(
            &plans,
            &cfg(),
            bk,
            &world,
            &vctx(tok(1)),
            &bd,
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
        let assembled = assemble(
            &plans,
            &cfg(),
            &bk,
            &world,
            &vctx(tok(1)),
            &bd,
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

    /// A Curve NG LP exits through venue 7: `pool ‖ i`, the pool being the
    /// LP itself. Quoted on the pool's state in the book; an unread supply
    /// is not routed.
    #[test]
    fn curve_lp_exit_assembles_as_venue_7() {
        use liq_plan::VENUE_CURVE_LP_ONE_COIN;
        // The LP (A2, tok(2)) is an NG pool of (A0, A1): withdraw coin 0.
        let mut ng =
            crate::fixtures::curve(2, &[e18(10_000_000), e18(10_000_000)], 100_000, 4_000_000);
        ng.address = tok(2);
        ng.assets = smallvec::SmallVec::from_slice(&[A0, A1]);
        ng.tokens = smallvec::SmallVec::from_slice(&[tok(0), tok(1)]);
        if let crate::solver::PoolState::Curve(c) = &mut ng.state {
            c.ng = true;
        }
        let mut bk = book(vec![deep(), ng]);
        let mut lp = unwrap_a2(A0);
        lp.kind = crate::solver::UnwrapKind::CurveLp { i: 0 };
        lp.rate = crate::solver::UnwrapRate::Unread;
        lp.scale = e18(1) / U256::from(1_000u64);
        bk.add_unwrap(lp);
        assert!(
            solve_pair(&bk, A2, A1, e18(10), &GAS, &B).is_err(),
            "unread"
        );
        assert!(bk.set_unwrap_rate(
            A2,
            crate::solver::UnwrapRate::CurveLp {
                total_supply: e18(20_000_000)
            },
            1
        ));
        let via = solve_pair(&bk, A2, A1, e18(10), &GAS, &B).unwrap();
        let u = via.unwrap.unwrap();
        // ~1 coin per LP in a balanced pool, less the withdrawal fee.
        assert!(u.amount_out < e18(10) && u.amount_out > e18(9));
        let (_, plan) = assemble_a2_exit(&bk);
        let repay = &plan.groups[0].repay_swaps;
        assert_eq!(repay[0].venue, VENUE_CURVE_LP_ONE_COIN);
        assert_eq!((repay[0].token_in, repay[0].token_out), (tok(2), tok(0)));
        // The LP, coin 0, and the MetaRegistry handler of the LP's own pool.
        let mut want = tok(2).to_vec();
        want.extend_from_slice(&[0, CURVE_HANDLER]);
        assert_eq!(repay[0].data, want);
    }

    /// A live Pendle PT exits through venue 8 with its market as the data,
    /// quoted by the market math on a recorded mainnet state (PT-apyUSD's
    /// market: one PT sold for 686343938057976701 SY at this state).
    #[test]
    fn pendle_market_exit_assembles_as_venue_8() {
        use liq_plan::VENUE_PENDLE_MARKET_SELL;
        let i = |s: &str| s.parse::<alloy_primitives::I256>().unwrap();
        let u = |s: &str| s.parse::<U256>().unwrap();
        let snap = crate::pendle::MarketSnapshot {
            total_pt: i("7187962440982406255028943"),
            total_sy: i("9112559297158223896269820"),
            scalar_root: i("22856551821273811277"),
            expiry: 1_793_836_800,
            ln_fee_rate_root: u("11533235813673030"),
            reserve_fee_percent: U256::from(80u64),
            last_ln_implied_rate: u("145627713077751416"),
            index: u("1434777256254465538"),
            quote_ts: 1_790_753_291,
            // The SY redeems at 1.6 here, so a PT is worth ~1.1 and the
            // test world's 5 % bonus leg clears.
            out_per_sy_scale: e18(1600),
            sy_scale: e18(1000),
        };
        assert_eq!(
            crate::pendle::sell_pt(&snap, e18(1)).unwrap(),
            u("686343938057976701")
        );
        let market = addr(0x8888);
        let mut bk = book(vec![deep()]);
        let mut pt = unwrap_a2(A0);
        pt.kind = crate::solver::UnwrapKind::PendleMarket {
            market,
            yt: addr(0x7777),
            sy: addr(0x5555),
        };
        pt.rate = crate::solver::UnwrapRate::Pendle(snap);
        bk.add_unwrap(pt);
        let via = solve_pair(&bk, A2, A1, e18(10), &GAS, &B).unwrap();
        // 10 PT → ~6.86 SY → ~10.98 out (less the SY step's 1 ppm haircut).
        let out = via.unwrap.unwrap().amount_out;
        assert!(out > e18(10) && out < e18(11), "{out}");
        let (_, plan) = assemble_a2_exit(&bk);
        let repay = &plan.groups[0].repay_swaps;
        assert_eq!(repay[0].venue, VENUE_PENDLE_MARKET_SELL);
        assert_eq!((repay[0].token_in, repay[0].token_out), (tok(2), tok(0)));
        assert_eq!(repay[0].data, market.to_vec());
    }

    /// At expiry a live PT switches from its market (venue 8) to the
    /// post-expiry redeem (venue 6) through the same YT and SY into the same
    /// token: not routed until the redeem rate is read, then assembled as
    /// venue 6 with the redeem's gas.
    #[test]
    fn expired_market_pt_switches_to_the_redeem() {
        use liq_plan::VENUE_PENDLE_PT_REDEEM;
        let (yt, sy) = (addr(0x7777), addr(0x5555));
        let mut bk = book(vec![deep()]);
        let mut pt = unwrap_a2(A0);
        pt.kind = crate::solver::UnwrapKind::PendleMarket {
            market: addr(0x8888),
            yt,
            sy,
        };
        pt.scale = e18(1);
        pt.gas = 445_782;
        pt.expiry_gas = 141_280;
        bk.add_unwrap(pt);
        assert!(bk.expire_pendle_market(A2));
        assert!(!bk.expire_pendle_market(A2), "switches once");
        let u = bk.unwrap_of(A2).unwrap();
        assert_eq!(u.kind, crate::solver::UnwrapKind::PendlePt { yt, sy });
        assert_eq!((u.gas, u.scale), (141_280, e18(1000)));
        assert!(
            solve_pair(&bk, A2, A1, e18(10), &GAS, &B).is_err(),
            "not routed on the market's last price"
        );
        assert!(bk.set_unwrap_rate(
            A2,
            crate::solver::UnwrapRate::Linear {
                assets_per_scale: e18(1100),
                max_into: None,
            },
            2
        ));
        let via = solve_pair(&bk, A2, A1, e18(10), &GAS, &B).unwrap();
        let inner = solve_pair(&bk, A0, A1, via.unwrap.unwrap().amount_out, &GAS, &B).unwrap();
        assert_eq!(via.hop_gas, inner.hop_gas + 141_280);
        let (_, plan) = assemble_a2_exit(&bk);
        let repay = &plan.groups[0].repay_swaps;
        assert_eq!(repay[0].venue, VENUE_PENDLE_PT_REDEEM);
        assert_eq!(repay[0].data, yt.to_vec());
    }

    /// Collateral seized in the debt asset itself (an Aave WETH/WETH loop,
    /// replay 26087344) needs no exit at all: the seize repays the flash
    /// and the surplus is the profit, with no swap and no hop gas.
    #[test]
    fn a_seize_in_the_debt_asset_needs_no_swap() {
        let bk = book(vec![deep()]);
        let exit = solve_pair(&bk, A1, A1, e18(10), &GAS, &B).unwrap();
        assert!(exit.allocs.is_empty() && exit.unwrap.is_none() && exit.hub.is_none());
        assert_eq!(
            (exit.amount_in, exit.amount_out, exit.hop_gas),
            (e18(10), e18(10), 0)
        );
        let (_s, flash) = idx();
        let mut quote = q(1);
        quote.seize_options[0].asset = A1;
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
        let bd = bid(&BidConfig::new(7_500, 7_500, 0, 0).unwrap(), 0, 1).unwrap();
        let assembled = assemble(
            &plans,
            &cfg(),
            &bk,
            &world,
            &vctx(tok(1)),
            &bd,
            &GAS,
            FLAG_SWEEP,
            &flash,
            H,
        )
        .unwrap();
        let plan = &assembled[0].plan;
        validate(plan, &vctx(tok(1))).unwrap();
        assert!(plan.groups[0].repay_swaps.is_empty());
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
        let assembled = assemble(
            &plans,
            &cfg(),
            &bk,
            &world,
            &vctx(tok(1)),
            &bd,
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
        let weth = tok(0);
        let assembled = assemble(
            &plans,
            &c,
            &bk,
            &world,
            &vctx(weth),
            &bd,
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

    /// H4: Aave charges 5 bps on the flash amount, and the Executor buys it
    /// at run time with the group's first exact-output pool leg
    /// (`SwapModule.runSwaps`), so the plan's exact output is the pull
    /// alone: `exact_out == pull`, the premium `fee(flash_amount)` left out.
    /// Over-borrow is the 1 wei dust, not the premium: borrowing the
    /// premium raises the debt by the same amount the callback still has
    /// to pay.
    #[test]
    fn aave_premium_is_left_to_the_executor() {
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
        assert_eq!(exact_out, pull, "the exact output buys the pull alone");
        assert_eq!(g.flash_amount, pull + 1, "over-borrow stays 1 wei of dust");
        assert!(
            fee > U256::from(1u8),
            "the premium is larger than the dust, so it is not inside flash_amount"
        );
        // What the callback holds once the swaps ran: the unspent flash, the
        // pull bought back, and the premium the Executor adds to it.
        let balance = U256::from(g.flash_amount) - U256::from(pull) + U256::from(exact_out) + fee;
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
