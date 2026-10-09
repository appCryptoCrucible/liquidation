//! Encoder-only invariants the contract cannot see (PLAN-ENCODING §2a + 10A tails).

use alloy_primitives::{keccak256, Address, B256, U256};
use alloy_sol_types::sol;
use liq_flash::fee_amount;
use liq_protocol::ExecutorAdapter;
use liq_types::fixed::{mul_div, Rounding, WAD};
use liq_types::FlashProvider;
use liq_wire::wire::{
    leg_tie, LegTail, LEG_EXACT_OUT, LEG_TAKE_BALANCE, LEG_TIE_MAX, V2_FACTORY_SUSHI,
    V3_FACTORY_PANCAKE, V3_FACTORY_UNISWAP, V4_KEY_LEN, VENUE_CURVE_CRYPTO_POOL,
    VENUE_CURVE_LP_ONE_COIN, VENUE_CURVE_POOL, VENUE_PENDLE_MARKET_SELL, VENUE_PENDLE_PT_REDEEM,
    VENUE_ROUTER, VENUE_UNIV2_POOL, VENUE_UNIV3_POOL, VENUE_UNIV4_POOL, VENUE_UNWRAP_4626,
};

use crate::error::{EncodeError, Result};
use crate::types::{BatchPlan, FlashGroup, MorphoMarketPin, SwapLeg, ValidateCtx};

sol! {
    struct MarketParams {
        address loanToken;
        address collateralToken;
        address oracle;
        address irm;
        uint256 lltv;
    }
}

pub fn validate(p: &BatchPlan, ctx: &ValidateCtx) -> Result<()> {
    if p.groups.is_empty() {
        return Err(EncodeError::NoGroups);
    }
    if ctx.weth == Address::ZERO {
        return Err(EncodeError::ZeroAddress("weth"));
    }
    if p.min_profit_wei == 0 {
        return Err(EncodeError::ZeroMinProfit);
    }
    for g in &p.groups {
        let reward_only = g.provider == FlashProvider::None;
        if reward_only {
            reward_group(g)?;
        } else {
            nonzero(g.flash_source, "flashSource")?;
        }
        if g.provider == FlashProvider::UniV3Swap {
            flash_swap_group(g)?;
        }
        nonzero(g.debt_asset, "debtAsset")?;
        if g.liqs.is_empty() {
            return Err(EncodeError::NoLegs);
        }
        for l in &g.liqs {
            nonzero(l.market, "market")?;
            nonzero(l.borrower, "borrower")?;
            nonzero(l.collateral_asset, "collateralAsset")?;
            // T13 L1. Liquity is paid by the Stability Pool, not by the
            // liquidator — `batchLiquidateTroves` pulls no BOLD from
            // `msg.sender` (`TroveManager.sol:417-475, 537-545`). The
            // liquidator's compensation is gas comp only. `protocol_pull ==
            // 0` for this adapter is the TRUTHFUL size of the repay leg, not
            // a sizing bug; every other adapter's `0` really does mean
            // nothing to fund.
            if l.protocol_pull == 0 && l.adapter != ExecutorAdapter::LiquityV2 && !reward_only {
                return Err(EncodeError::ZeroPull {
                    pull: l.protocol_pull,
                });
            }
            match (l.adapter, &l.tail) {
                (ExecutorAdapter::AaveV3 | ExecutorAdapter::SiloV2, LegTail::None) => {}
                (
                    ExecutorAdapter::AaveV4,
                    LegTail::AaveV4 {
                        collateral_reserve_id,
                        debt_reserve_id,
                    },
                ) => {
                    pin_v4(ctx, l.market, *collateral_reserve_id, l.collateral_asset)?;
                    pin_v4(ctx, l.market, *debt_reserve_id, g.debt_asset)?;
                }
                (ExecutorAdapter::AaveV4, _) => return Err(EncodeError::V4TailShape),
                (ExecutorAdapter::MorphoBlue, LegTail::Morpho { market_id }) => {
                    check_morpho(ctx, *market_id, l.market, g.debt_asset, l.collateral_asset)?;
                }
                (ExecutorAdapter::MorphoBlue, _) => return Err(EncodeError::MorphoTailShape),
                (
                    ExecutorAdapter::EulerV2,
                    LegTail::Euler {
                        min_yield: _,
                        vault,
                    },
                ) => {
                    if vault.is_zero() {
                        return Err(EncodeError::EulerZeroVault);
                    }
                }
                (ExecutorAdapter::EulerV2, _) => return Err(EncodeError::EulerTailShape),
                (ExecutorAdapter::SiloV2, _) => return Err(EncodeError::SiloTailShape),
                (ExecutorAdapter::LiquityV2, LegTail::Liquity { trove_id }) => {
                    if trove_id.is_zero() {
                        return Err(EncodeError::LiquityZeroTrove);
                    }
                    check_liquity(ctx, l.market, *trove_id, l.borrower)?;
                }
                (ExecutorAdapter::LiquityV2, _) => return Err(EncodeError::LiquityTailShape),
                (
                    ExecutorAdapter::Fluid,
                    LegTail::Fluid {
                        kind,
                        flags,
                        col_per_unit_debt,
                        debt_shares_min_per_token,
                        col_per_share_min,
                    },
                ) => check_fluid(
                    *kind,
                    *flags,
                    *col_per_unit_debt,
                    *debt_shares_min_per_token,
                    *col_per_share_min,
                )?,
                (ExecutorAdapter::Fluid, _) => return Err(EncodeError::FluidTailShape),
                (ExecutorAdapter::Gearbox, LegTail::Gearbox { min_seized, .. }) => {
                    if min_seized.is_zero() {
                        return Err(EncodeError::GearboxZeroMinSeized);
                    }
                }
                (ExecutorAdapter::Gearbox, _) => return Err(EncodeError::GearboxTailShape),
                (
                    ExecutorAdapter::CompoundV2,
                    LegTail::CompoundV2 {
                        ctoken_collateral,
                        is_cether,
                    },
                ) => {
                    if ctoken_collateral.is_zero() {
                        return Err(EncodeError::CompoundZeroCToken);
                    }
                    if *is_cether > 1 {
                        return Err(EncodeError::CompoundBadFlag);
                    }
                    check_compound(ctx, l.market, *ctoken_collateral, *is_cether)?;
                }
                (ExecutorAdapter::CompoundV2, _) => return Err(EncodeError::CompoundTailShape),
                (ExecutorAdapter::AaveV3, _) => return Err(EncodeError::V3TailShape),
            }
        }
        for s in &g.repay_swaps {
            check_swap(s)?;
        }
        ties_ok(g)?;
        assert_exact_out_first(&g.repay_swaps)?;
        unwraps_first(&g.repay_swaps)?;
        // A repay leg pays the debt, or a hub: an exit through a hub (WETH,
        // or an intermediate token the router's graph proposed) sells the
        // collateral into it and buys the debt with it, so a hub is a token
        // another repay leg of the group sells into the debt. An unwrap pays
        // what the collateral wraps.
        let into_debt = |t: Address| {
            g.repay_swaps
                .iter()
                .any(|s| s.token_in == t && s.token_out == g.debt_asset)
        };
        if g.repay_swaps.iter().any(|s| {
            s.token_out != g.debt_asset
                && s.token_out != ctx.weth
                && !into_debt(s.token_out)
                && !crate::types::is_unwrap_venue(s.venue)
        }) {
            return Err(EncodeError::RepayTargetMismatch {
                group: g.debt_asset,
            });
        }
        // The Executor adds the premium, in the debt asset, to the first
        // exact output to run: every exact output buys the debt.
        if let Some(s) = g
            .repay_swaps
            .iter()
            .find(|s| s.flags & LEG_EXACT_OUT != 0 && s.token_out != g.debt_asset)
        {
            return Err(EncodeError::ExactOutNotDebt {
                token: s.token_out,
                debt: g.debt_asset,
            });
        }
        unwrap_outputs_closed(p, g, ctx.weth)?;
        size_repay_to_pull(g)?;
        surplus_debt_routed(p, g, ctx.weth)?;
    }
    for s in &p.profit_swaps {
        check_swap(s)?;
        if s.token_out != ctx.weth {
            return Err(EncodeError::ProfitTargetNotWeth { weth: ctx.weth });
        }
        // The profit blob runs after every group, against the last group's
        // fills: a tie there would name a leg of some other group.
        if leg_tie(s.flags).is_some() {
            return Err(EncodeError::TiedProfitLeg { token: s.token_in });
        }
    }
    assert_exact_out_first(&p.profit_swaps)?;
    close_collaterals(p, ctx.weth)?;
    cascade_ok(&p.groups)?;
    Ok(())
}

/// A reward-only group (`FlashProvider::None`, Executor `P_NONE`): nothing
/// borrowed, nothing pulled, nothing to repay. Its legs' rewards close to
/// WETH through the profit legs like any seized collateral.
fn reward_group(g: &FlashGroup) -> Result<()> {
    if g.flash_amount != 0 || g.flash_source != Address::ZERO || g.fee_bps != 0 {
        return Err(EncodeError::RewardGroupBorrows);
    }
    if !g.repay_swaps.is_empty() {
        return Err(EncodeError::RewardGroupRepays);
    }
    if let Some(l) = g.liqs.iter().find(|l| l.protocol_pull != 0) {
        return Err(EncodeError::RewardGroupPulls {
            pull: l.protocol_pull,
        });
    }
    Ok(())
}

/// A flash-swap group (`FlashProvider::UniV3Swap`, Executor `P_UNIV3_SWAP`):
/// the lender is a Uniswap V3 pool that sells the group exactly
/// `flash_amount` of the debt and is paid its other token inside the swap
/// callback, where the legs run. One liquidation per group: the pool must
/// be paid whatever filled, so a beaten leg beside a live one would leave
/// its debt bought and unpaid for; alone, it reverts the group and the swap
/// with it, and nothing is owed. The repay legs sell the collateral into
/// WETH (or unwrap it) and never buy the debt — the lender did — nor touch
/// the lender, which is locked while it swaps.
fn flash_swap_group(g: &FlashGroup) -> Result<()> {
    if g.liqs.len() != 1 {
        return Err(EncodeError::FlashSwapLegs { legs: g.liqs.len() });
    }
    if g.fee_bps != 0 {
        return Err(EncodeError::UnpriceableFee {
            provider: g.provider,
            fee_bps: g.fee_bps,
        });
    }
    for s in &g.repay_swaps {
        let unwrap = crate::types::is_unwrap_venue(s.venue);
        if s.flags & LEG_EXACT_OUT != 0 || (!unwrap && s.token_out == g.debt_asset) {
            return Err(EncodeError::FlashSwapRepaysDebt { debt: g.debt_asset });
        }
        if s.venue == VENUE_UNIV3_POOL && s.data.get(..20) == Some(g.flash_source.as_slice()) {
            return Err(EncodeError::FlashSwapLegOnLender {
                pool: g.flash_source,
            });
        }
    }
    Ok(())
}

fn nonzero(a: Address, field: &'static str) -> Result<()> {
    if a == Address::ZERO {
        Err(EncodeError::ZeroAddress(field))
    } else {
        Ok(())
    }
}

fn pin_v4(ctx: &ValidateCtx, spoke: Address, id: u16, expected: Address) -> Result<()> {
    match ctx.v4_pin(spoke, id) {
        None => Err(EncodeError::V4ReserveUnpinned { spoke, id }),
        Some(pinned) if pinned == expected => Ok(()),
        Some(pinned) => Err(EncodeError::V4ReserveMismatch {
            spoke,
            id,
            pinned,
            got: expected,
        }),
    }
}

fn check_morpho(
    ctx: &ValidateCtx,
    id: B256,
    market: Address,
    debt: Address,
    coll: Address,
) -> Result<()> {
    let pin = ctx
        .morpho_pin(id)
        .ok_or(EncodeError::MorphoIdUnknown { id })?;
    if pin.morpho != market {
        return Err(EncodeError::ZeroAddress("morpho singleton"));
    }
    let hashed = morpho_id(pin);
    if hashed != pin.id {
        return Err(EncodeError::MorphoIdHashMismatch { id });
    }
    if pin.loan_token != debt || pin.collateral_token != coll {
        return Err(EncodeError::MorphoTokenMismatch {
            id,
            loan: pin.loan_token,
            coll: pin.collateral_token,
            debt,
            got_coll: coll,
        });
    }
    Ok(())
}

fn check_compound(
    ctx: &ValidateCtx,
    market: Address,
    ctoken_collateral: Address,
    is_cether: u8,
) -> Result<()> {
    let pin = ctx
        .compound_pin(market, ctoken_collateral)
        .ok_or(EncodeError::CompoundUnpinned {
            market,
            ctoken_collateral,
        })?;
    if pin.debt_ctoken != market {
        return Err(EncodeError::CompoundMarketMismatch {
            pinned: pin.debt_ctoken,
            got: market,
        });
    }
    if pin.ctoken_collateral != ctoken_collateral {
        return Err(EncodeError::CompoundCTokenMismatch {
            pinned: pin.ctoken_collateral,
            got: ctoken_collateral,
        });
    }
    if pin.is_cether != is_cether {
        return Err(EncodeError::CompoundCEtherMismatch {
            pinned: pin.is_cether,
            got: is_cether,
        });
    }
    Ok(())
}

fn check_liquity(
    ctx: &ValidateCtx,
    market: Address,
    trove_id: U256,
    borrower: Address,
) -> Result<()> {
    let pin = ctx
        .liquity_pin(trove_id)
        .ok_or(EncodeError::LiquityUnpinned { trove_id })?;
    if pin.trove_manager != market {
        return Err(EncodeError::LiquityMarketMismatch {
            pinned: pin.trove_manager,
            got: market,
        });
    }
    if pin.trove_id != trove_id {
        return Err(EncodeError::LiquityTroveMismatch {
            pinned: pin.trove_id,
            got: trove_id,
        });
    }
    if !pin.borrower.is_zero() && pin.borrower != borrower {
        return Err(EncodeError::LiquityBorrowerMismatch {
            pinned: pin.borrower,
            got: borrower,
        });
    }
    Ok(())
}

/// The Fluid tail the Executor can act on: a known vault type; a nonzero
/// slippage floor; the per-share figure on each smart side and only there;
/// a token1 choice only on a smart side; no undefined flag bit.
fn check_fluid(
    kind: u8,
    flags: u8,
    col_per_unit_debt: U256,
    debt_shares_min_per_token: U256,
    col_per_share_min: U256,
) -> Result<()> {
    use liq_wire::wire::{
        FLUID_COL_TOKEN1, FLUID_DEBT_TOKEN1, FLUID_FLAGS, FLUID_T1, FLUID_T2, FLUID_T3, FLUID_T4,
    };
    if !(FLUID_T1..=FLUID_T4).contains(&kind) {
        return Err(EncodeError::FluidBadKind(kind));
    }
    if flags & !FLUID_FLAGS != 0 {
        return Err(EncodeError::FluidBadFlags(flags));
    }
    if col_per_unit_debt.is_zero() {
        return Err(EncodeError::FluidZeroColPer);
    }
    let smart_debt = kind == FLUID_T3 || kind == FLUID_T4;
    let smart_col = kind == FLUID_T2 || kind == FLUID_T4;
    if smart_debt == debt_shares_min_per_token.is_zero() || smart_col == col_per_share_min.is_zero()
    {
        return Err(EncodeError::FluidPerShare(kind));
    }
    if (!smart_debt && flags & FLUID_DEBT_TOKEN1 != 0)
        || (!smart_col && flags & FLUID_COL_TOKEN1 != 0)
    {
        return Err(EncodeError::FluidBadFlags(flags));
    }
    Ok(())
}

/// Pin `vaultT1/coreModule/main.sol` @ `9496626f`:
/// `colPerUnitDebt_` = min collateral per debt in **1e18**.
/// `(actualCol * 1e18) / actualDebt`. Internal `colPerDebt` (1e27) is a
/// different number — 17A must not copy the oracle 1e27 onto the wire.
pub fn col_per_unit_debt_1e18(actual_col: U256, actual_debt: U256) -> Result<U256> {
    if actual_debt.is_zero() {
        return Err(EncodeError::FluidColPerConvert);
    }
    mul_div(actual_col, WAD, actual_debt, Rounding::Down)
        .map_err(|_| EncodeError::FluidColPerConvert)
}

/// `Id = keccak256(abi.encode(MarketParams))` — Morpho Blue `8e26ca6a`.
#[must_use]
pub fn morpho_id(pin: &MorphoMarketPin) -> B256 {
    use alloy_sol_types::SolValue;
    let p = MarketParams {
        loanToken: pin.loan_token,
        collateralToken: pin.collateral_token,
        oracle: pin.oracle,
        irm: pin.irm,
        lltv: pin.lltv,
    };
    keccak256(p.abi_encode())
}

fn check_swap(s: &SwapLeg) -> Result<()> {
    nonzero(s.token_in, "tokenIn")?;
    nonzero(s.token_out, "tokenOut")?;
    match s.venue {
        // pool, then optionally the factory id (absent: Uniswap V3).
        VENUE_UNIV3_POOL => {
            if s.data.len() != 20 && s.data.len() != 21 {
                return Err(EncodeError::BadPoolDataLen(s.data.len()));
            }
            if let Some(&fid) = s.data.get(20) {
                if fid == V3_FACTORY_UNISWAP || fid > V3_FACTORY_PANCAKE {
                    return Err(EncodeError::BadV3Factory(fid));
                }
            }
        }
        VENUE_ROUTER => {
            if s.data.len() < 20 {
                return Err(EncodeError::BadRouterDataLen(s.data.len()));
            }
        }
        VENUE_UNIV2_POOL => {
            if s.data.len() != 21 {
                return Err(EncodeError::BadV2DataLen(s.data.len()));
            }
            if let Some(&fid) = s.data.get(20) {
                if fid > V2_FACTORY_SUSHI {
                    return Err(EncodeError::BadV2Factory(fid));
                }
            }
        }
        // The pool id; the Executor checks the pool is one it allows, and the
        // Vault that it is registered.
        crate::types::VENUE_BALANCER => {
            if s.data.len() != crate::types::BALANCER_POOL_ID_LEN {
                return Err(EncodeError::BadBalancerData(s.data.len()));
            }
        }
        // pool ‖ swap0to1. The pool is checked against the DexFactory and its
        // tokens against the leg's on chain.
        crate::types::VENUE_FLUID => {
            if s.data.len() != crate::types::FLUID_LEG_LEN || s.data.get(20).is_none_or(|b| *b > 1)
            {
                return Err(EncodeError::BadFluidData(s.data.len()));
            }
        }
        // pool ‖ i ‖ j ‖ MetaRegistry handler index. The pool, its handler
        // and its coins are checked on chain.
        VENUE_CURVE_POOL | VENUE_CURVE_CRYPTO_POOL => {
            if s.data.len() != 23 {
                return Err(EncodeError::BadCurveDataLen(s.data.len()));
            }
            if s.flags & LEG_EXACT_OUT != 0 {
                return Err(EncodeError::CurveExactOut);
            }
        }
        VENUE_UNWRAP_4626 => {
            if s.data.len() != 20 || s.data != s.token_in.as_slice() {
                return Err(EncodeError::BadUnwrapData(s.data.len()));
            }
            if s.flags & LEG_EXACT_OUT != 0 {
                return Err(EncodeError::UnwrapExactOut);
            }
        }
        // pool ‖ i ‖ MetaRegistry handler index. The pool is the LP being
        // spent; it, its handler and coin `i` are checked on chain.
        VENUE_CURVE_LP_ONE_COIN => {
            if s.data.len() != 22 || s.data.get(..20) != Some(s.token_in.as_slice()) {
                return Err(EncodeError::BadUnwrapData(s.data.len()));
            }
            if s.flags & LEG_EXACT_OUT != 0 {
                return Err(EncodeError::UnwrapExactOut);
            }
        }
        // The YT (venue 6) is checked against the PT on chain, and the
        // market (venue 8) against Pendle's factory; here only the shape.
        VENUE_PENDLE_PT_REDEEM | VENUE_PENDLE_MARKET_SELL => {
            if s.data.len() != 20 || s.data.iter().all(|b| *b == 0) {
                return Err(EncodeError::BadUnwrapData(s.data.len()));
            }
            if s.flags & LEG_EXACT_OUT != 0 {
                return Err(EncodeError::UnwrapExactOut);
            }
        }
        // The pool key; the PoolManager checks the pool exists, and the
        // Executor checks the hook and that the leg's tokens are the key's.
        VENUE_UNIV4_POOL => {
            if s.data.len() != V4_KEY_LEN {
                return Err(EncodeError::BadPoolDataLen(s.data.len()));
            }
        }
        // The chain's shape; each hop's pool is derived on chain (CREATE2)
        // from its tokens and fee or factory, and must deliver exactly.
        crate::types::VENUE_CHAIN => check_chain(s)?,
        v => return Err(EncodeError::UnknownVenue(v)),
    }
    Ok(())
}

/// A chain leg (venue 10; exact output buys along the path, exact input
/// sells along it): 1 to `CHAIN_MAX_HOPS` hops, each a V3 hop (a non-zero
/// fee tier; Uniswap, SushiSwap or PancakeSwap), a V2 hop (factory 0 or 1), or a V4 or Curve hop whose param is
/// the offset of its extra (a 66-byte pool key; pool ‖ i ‖ j ‖ handler)
/// inside the data; the length the hops and their extras imply; no
/// intermediate token is zero or repeats an end; and exact output only when
/// every hop is V3 or V2, as the Executor refuses otherwise.
fn check_chain(s: &SwapLeg) -> Result<()> {
    use crate::types::{
        BALANCER_POOL_ID_LEN, CHAIN_CURVE_EXTRA, CHAIN_HOP_BALANCER, CHAIN_HOP_CURVE,
        CHAIN_HOP_CURVE_CRYPTO, CHAIN_HOP_FLUID, CHAIN_HOP_V2, CHAIN_HOP_V3, CHAIN_HOP_V3_PANCAKE,
        CHAIN_HOP_V3_SUSHI, CHAIN_HOP_V4, CHAIN_MAX_HOPS, FLUID_LEG_LEN, V4_KEY_LEN,
    };
    let n = usize::from(*s.data.first().ok_or(EncodeError::BadChain("empty"))?);
    if n == 0 || n > CHAIN_MAX_HOPS {
        return Err(EncodeError::BadChain("hop count"));
    }
    let base = n
        .checked_mul(4)
        .and_then(|h| h.checked_add(1))
        .and_then(|h| h.checked_add(n.saturating_sub(1).saturating_mul(20)))
        .ok_or(EncodeError::BadChain("length"))?;
    if s.data.len() < base {
        return Err(EncodeError::BadChain("length"));
    }
    let mut extras = 0usize;
    let mut exact_out_ok = true;
    for h in 0..n {
        let at = |k: usize| {
            s.data
                .get(h.saturating_mul(4).saturating_add(k))
                .copied()
                .unwrap_or(0)
        };
        let (kind, param) = (at(1), [at(2), at(3), at(4)]);
        let len = match kind {
            CHAIN_HOP_V3 | CHAIN_HOP_V3_SUSHI | CHAIN_HOP_V3_PANCAKE if param != [0, 0, 0] => {
                continue
            }
            CHAIN_HOP_V2 if param[0] == 0 && param[1] == 0 && param[2] <= 1 => continue,
            CHAIN_HOP_V4 => V4_KEY_LEN,
            CHAIN_HOP_CURVE | CHAIN_HOP_CURVE_CRYPTO => CHAIN_CURVE_EXTRA,
            CHAIN_HOP_BALANCER => BALANCER_POOL_ID_LEN,
            CHAIN_HOP_FLUID => FLUID_LEG_LEN,
            _ => return Err(EncodeError::BadChain("hop")),
        };
        let off = usize::from(param[0]) << 16 | usize::from(param[1]) << 8 | usize::from(param[2]);
        if off < base || off.saturating_add(len) > s.data.len() {
            return Err(EncodeError::BadChain("extra"));
        }
        extras = extras.saturating_add(len);
        exact_out_ok = false;
    }
    if s.data.len() != base.saturating_add(extras) {
        return Err(EncodeError::BadChain("length"));
    }
    if s.flags & crate::types::LEG_EXACT_OUT != 0 && !exact_out_ok {
        return Err(EncodeError::BadChain(
            "exact output through a hop with an extra (V4, Curve, Balancer, Fluid)",
        ));
    }
    let mids = s
        .data
        .get(n.saturating_mul(4).saturating_add(1)..base)
        .unwrap_or(&[]);
    for t in mids.chunks(20) {
        if t.iter().all(|b| *b == 0) || t == s.token_in.as_slice() || t == s.token_out.as_slice() {
            return Err(EncodeError::BadChain("intermediate token"));
        }
    }
    Ok(())
}

/// Repay swaps tied to their liquidation leg (flags bits 2–7) are skipped on
/// chain when it did not fill. In a group of several legs every set-amount
/// repay swap is tied: untied, a beaten leg's swap would still run and pay
/// for its output with another leg's collateral, or fail the plan. A
/// TAKE_BALANCE leg is never tied: it spends what arrived, whichever leg
/// seized it, and nothing when none did.
fn ties_ok(g: &FlashGroup) -> Result<()> {
    let legs = g.liqs.len();
    for s in &g.repay_swaps {
        let take = s.flags & LEG_TAKE_BALANCE != 0;
        match leg_tie(s.flags) {
            Some(tie) if tie >= legs => return Err(EncodeError::TieOutOfRange { tie, legs }),
            Some(_) if take => return Err(EncodeError::TiedTakeBalance { token: s.token_in }),
            None if legs > 1 && !take => {
                if legs > LEG_TIE_MAX {
                    return Err(EncodeError::TooManyTiedLegs {
                        legs,
                        max: LEG_TIE_MAX,
                    });
                }
                return Err(EncodeError::UntiedRepay {
                    token: s.token_in,
                    legs,
                });
            }
            _ => {}
        }
    }
    Ok(())
}

/// Unwrap legs convert the seized collateral before anything sells it.
fn unwraps_first(blob: &[SwapLeg]) -> Result<()> {
    let mut seen_other = false;
    for s in blob {
        if crate::types::is_unwrap_venue(s.venue) {
            if seen_other {
                return Err(EncodeError::UnwrapNotFirst);
            }
        } else {
            seen_other = true;
        }
    }
    Ok(())
}

/// What an unwrap produces must leave the Executor: it is the debt asset
/// (spent by the repay, surplus swept), WETH (swept), or closed to WETH by a
/// TAKE_BALANCE leg.
fn unwrap_outputs_closed(p: &BatchPlan, g: &FlashGroup, weth: Address) -> Result<()> {
    for u in g
        .repay_swaps
        .iter()
        .filter(|s| crate::types::is_unwrap_venue(s.venue))
    {
        let out = u.token_out;
        if out == g.debt_asset || out == weth {
            continue;
        }
        let closed = all_swaps(p).any(|s| {
            s.token_in == out
                && s.token_out == weth
                && s.flags & LEG_TAKE_BALANCE != 0
                && !crate::types::is_unwrap_venue(s.venue)
        });
        if !closed {
            return Err(EncodeError::UnwrapOutputUnclosed { asset: out });
        }
    }
    Ok(())
}

/// Within a blob, every leg that spends a set amount of a token (EXACT_OUT,
/// or exact input) runs before the TAKE_BALANCE leg on that token, which
/// leaves none of it. TAKE_BALANCE on one token ahead of set amounts of
/// another is an exit through the hub: the collateral into WETH, then WETH
/// for the debt.
fn assert_exact_out_first(blob: &[SwapLeg]) -> Result<()> {
    let mut taken: Vec<Address> = Vec::new();
    for s in blob {
        if s.flags & LEG_TAKE_BALANCE != 0 {
            taken.push(s.token_in);
        } else if taken.contains(&s.token_in) {
            return Err(EncodeError::SpentAfterTakeBalance { token: s.token_in });
        }
    }
    Ok(())
}

/// Every seized collateral leaves the Executor through exactly one
/// TAKE_BALANCE leg that runs after its liquidation: in its own group's
/// repay blob (an unwrap, or an exit through the hub), or else in the
/// profit blob. Another group's repay blob does not count: it ran before
/// this group seized anything.
fn close_collaterals(p: &BatchPlan, weth: Address) -> Result<()> {
    let takes = |blob: &[SwapLeg], token: Address| {
        blob.iter()
            .filter(|s| s.token_in == token && s.flags & LEG_TAKE_BALANCE != 0)
            .count()
    };
    for g in &p.groups {
        for l in &g.liqs {
            let own = takes(&g.repay_swaps, l.collateral_asset);
            let closers = if own > 0 {
                own
            } else {
                takes(&p.profit_swaps, l.collateral_asset)
            };
            // Seized WETH is already the profit asset. A closer would be a
            // WETH→WETH swap, which has no pool; the repay legs sold the debt
            // that is owed and the residual WETH is swept.
            let need = usize::from(l.collateral_asset != weth);
            if closers != need {
                return Err(EncodeError::BadCollateralClosure {
                    collateral: l.collateral_asset,
                    closers,
                    need,
                });
            }
        }
    }
    Ok(())
}

fn all_swaps(p: &BatchPlan) -> impl Iterator<Item = &SwapLeg> {
    p.groups
        .iter()
        .flat_map(|g| g.repay_swaps.iter())
        .chain(p.profit_swaps.iter())
}

/// Each leg's repay buys exactly what its protocol pulls, and the group can
/// buy the flash premium whichever of its legs fill.
///
/// The protocol pulls each leg's `protocol_pull` before the repay swaps; the
/// flash lender then pulls `flash_amount + fee(flash_amount)`, of which the
/// unspent part of the flash (over-borrow, a beaten leg's share) is still
/// here. So the swaps tied to a leg buy its pull, and the premium is bought
/// at run time: the Executor adds the fee its provider charged to the
/// group's first exact-output pool leg that runs (`SwapModule.runSwaps`).
/// A leg's exact-input legs into the debt (Curve), an unwrap of its
/// collateral into the debt, or a seize in the debt asset itself (no swap)
/// cover the rest with an overshoot, the surplus swept
/// ([`surplus_debt_routed`]); the lender's pull enforces the total on
/// chain. In a group of one leg its untied swaps are its own.
fn size_repay_to_pull(g: &FlashGroup) -> Result<()> {
    let pull: u128 = g.liqs.iter().try_fold(0u128, |a, l| {
        a.checked_add(l.protocol_pull)
            .ok_or(EncodeError::TooManyLegs)
    })?;
    if g.flash_amount < pull {
        return Err(EncodeError::FlashShort {
            flash: g.flash_amount,
            pull,
        });
    }
    // A flash swap's lender sells the group exactly `flash_amount` of the
    // debt: there is no repay leg into it to size ([`flash_swap_group`]).
    if g.provider == FlashProvider::UniV3Swap {
        return Ok(());
    }
    let fee = fee_amount(g.provider, U256::from(g.flash_amount), g.fee_bps).ok_or(
        EncodeError::UnpriceableFee {
            provider: g.provider,
            fee_bps: g.fee_bps,
        },
    )?;
    u128::try_from(fee).map_err(|_| EncodeError::PremiumOverflow)?;
    let single = g.liqs.len() == 1;
    for (leg, l) in g.liqs.iter().enumerate() {
        let mine = || {
            g.repay_swaps
                .iter()
                .filter(move |s| leg_tie(s.flags).map_or(single, |t| t == leg))
        };
        let exact_out: u128 = mine().try_fold(0u128, |a, s| {
            if s.flags & LEG_EXACT_OUT == 0 {
                return Ok(a);
            }
            a.checked_add(s.amount).ok_or(EncodeError::TooManyLegs)
        })?;
        let pull = l.protocol_pull;
        if exact_out > pull {
            return Err(EncodeError::UnderSeizure {
                leg,
                exact_out,
                pull,
            });
        }
        let overshoots = mine().any(|s| {
            s.token_out == g.debt_asset && s.flags & (LEG_EXACT_OUT | LEG_TAKE_BALANCE) == 0
        }) || g.repay_swaps.iter().any(|s| {
            crate::types::is_unwrap_venue(s.venue)
                && s.token_in == l.collateral_asset
                && s.token_out == g.debt_asset
        })
            // Seized in the debt asset itself: the seize is the repay.
            || l.collateral_asset == g.debt_asset;
        if exact_out != pull && !overshoots {
            return Err(EncodeError::RepayNotSizedToPull {
                leg,
                exact_out,
                pull,
            });
        }
        // A router's output is fixed in its own calldata, so the Executor
        // does not add the fee to it; nor to Curve, which has no exact
        // output. When this leg is the only one to fill, one of its own
        // legs must buy the premium.
        let carries = overshoots
            || mine().any(|s| {
                s.flags & LEG_EXACT_OUT != 0
                    && (s.venue == VENUE_UNIV3_POOL
                        || s.venue == VENUE_UNIV2_POOL
                        || s.venue == VENUE_UNIV4_POOL
                        || s.venue == crate::types::VENUE_BALANCER
                        || s.venue == crate::types::VENUE_FLUID
                        || s.venue == crate::types::VENUE_CHAIN)
            });
        if !fee.is_zero() && !carries {
            return Err(EncodeError::PremiumUncovered { leg });
        }
    }
    Ok(())
}

/// A repay leg that sells a fixed amount into the debt asset.
fn has_exact_in_repay(g: &FlashGroup) -> bool {
    g.repay_swaps.iter().any(|s| {
        s.token_out == g.debt_asset
            && (s.flags & (LEG_EXACT_OUT | LEG_TAKE_BALANCE) == 0
                // Unwrapping straight into the debt asset covers the pull
                // like an exact-input leg; the surplus is swept.
                || crate::types::is_unwrap_venue(s.venue))
    })
}

fn surplus_debt_routed(p: &BatchPlan, g: &FlashGroup, weth: Address) -> Result<()> {
    let pull: u128 = g.liqs.iter().try_fold(0u128, |a, l| {
        a.checked_add(l.protocol_pull)
            .ok_or(EncodeError::TooManyLegs)
    })?;
    if g.debt_asset == weth || (g.flash_amount <= pull && !has_exact_in_repay(g)) {
        return Ok(());
    }
    let routed = p.profit_swaps.iter().any(|s| {
        s.token_in == g.debt_asset && s.token_out == weth && s.flags & LEG_TAKE_BALANCE != 0
    });
    if routed {
        Ok(())
    } else {
        Err(EncodeError::SurplusDebtUnrouted {
            debt: g.debt_asset,
            flash: g.flash_amount,
            pull,
        })
    }
}

fn cascade_ok(groups: &[FlashGroup]) -> Result<()> {
    let mut i = 0usize;
    while i < groups.len() {
        let Some(g0) = groups.get(i) else {
            break;
        };
        let debt = g0.debt_asset;
        let mut n = 0usize;
        let mut j = 0usize;
        while j < groups.len() {
            let Some(g) = groups.get(j) else {
                break;
            };
            if g.debt_asset == debt {
                n = n.checked_add(1).ok_or(EncodeError::TooManyGroups)?;
            }
            j = j.checked_add(1).ok_or(EncodeError::TooManyGroups)?;
        }
        if n > 3 {
            return Err(EncodeError::TooManySourcesForDebt { debt, n });
        }
        if n > 1 {
            let mut k = 0usize;
            while k < groups.len() {
                let Some(a) = groups.get(k) else {
                    break;
                };
                if a.debt_asset == debt {
                    let mut m = k.checked_add(1).ok_or(EncodeError::TooManyGroups)?;
                    while m < groups.len() {
                        let Some(b) = groups.get(m) else {
                            break;
                        };
                        if b.debt_asset == debt
                            && a.provider == b.provider
                            && a.flash_source == b.flash_source
                        {
                            return Err(EncodeError::DuplicateSourceForDebt {
                                debt,
                                provider: a.provider,
                            });
                        }
                        m = m.checked_add(1).ok_or(EncodeError::TooManyGroups)?;
                    }
                }
                k = k.checked_add(1).ok_or(EncodeError::TooManyGroups)?;
            }
        }
        i = i.checked_add(1).ok_or(EncodeError::TooManyGroups)?;
    }
    Ok(())
}

/// Morpho `toAssetsUp(toSharesDown(assets))` — actual pull ≤ asked.
#[must_use]
pub fn morpho_actual_pull(asked: U256, total_assets: U256, total_shares: U256) -> Option<U256> {
    const VIRTUAL_SHARES: U256 = U256::from_limbs([1_000_000, 0, 0, 0]);
    const VIRTUAL_ASSETS: U256 = U256::from_limbs([1, 0, 0, 0]);
    let s_plus = total_shares.checked_add(VIRTUAL_SHARES)?;
    let a_plus = total_assets.checked_add(VIRTUAL_ASSETS)?;
    let shares = asked.checked_mul(s_plus)?.checked_div(a_plus)?;
    let num = shares
        .checked_mul(a_plus)?
        .checked_add(s_plus)?
        .checked_sub(U256::from(1))?;
    num.checked_div(s_plus)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod chain_tests {
    use super::*;
    use crate::types::{CHAIN_HOP_V2, CHAIN_HOP_V3, VENUE_CHAIN};

    fn a(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    /// WBTC → WETH (V3 0.30 %) → USDC (Uniswap V2), exactly the amount.
    fn leg(flags: u8, data: Vec<u8>) -> SwapLeg {
        SwapLeg {
            venue: VENUE_CHAIN,
            token_in: a(1),
            token_out: a(3),
            flags,
            amount: 1_000,
            data,
        }
    }

    fn two_hops() -> Vec<u8> {
        let mut d = vec![2, CHAIN_HOP_V3, 0x00, 0x0b, 0xb8, CHAIN_HOP_V2, 0, 0, 0];
        d.extend_from_slice(a(2).as_slice());
        d
    }

    /// A Curve hop then a V4 hop: base 1 + 2·4 + 20 = 29 bytes; the Curve
    /// extra (23) at 29, the V4 key (66) at 52; 118 in all.
    fn curve_then_v4() -> Vec<u8> {
        use crate::types::{CHAIN_HOP_CURVE, CHAIN_HOP_V4};
        let mut d = vec![2, CHAIN_HOP_CURVE, 0, 0, 29, CHAIN_HOP_V4, 0, 0, 52];
        d.extend_from_slice(a(2).as_slice());
        d.extend_from_slice(a(9).as_slice());
        d.extend_from_slice(&[0, 1, 2]);
        d.extend_from_slice(&[7u8; 66]);
        d
    }

    /// A SushiSwap or PancakeSwap V3 hop is a V3 hop on another factory: the
    /// param is the fee tier (non-zero), it needs no extra, and it buys an
    /// exact output like a Uniswap V3 hop. The Executor's `_chainCheck` takes
    /// exactly kinds 0, 5 and 6 as V3.
    #[test]
    fn v3_fork_hops_are_v3_hops_on_another_factory() {
        use crate::types::{CHAIN_HOP_V3_PANCAKE, CHAIN_HOP_V3_SUSHI};
        for kind in [CHAIN_HOP_V3, CHAIN_HOP_V3_SUSHI, CHAIN_HOP_V3_PANCAKE] {
            // 0.25 % (Pancake's tier) and 0.30 %.
            for fee in [[0x00u8, 0x09, 0xc4], [0x00, 0x0b, 0xb8]] {
                let mut d = vec![2, kind, fee[0], fee[1], fee[2], CHAIN_HOP_V2, 0, 0, 0];
                d.extend_from_slice(a(2).as_slice());
                check_swap(&leg(0, d.clone())).unwrap();
                check_swap(&leg(LEG_EXACT_OUT, d.clone())).unwrap();
                check_swap(&leg(LEG_TAKE_BALANCE, d)).unwrap();
            }
            // A zero fee tier is no pool.
            let mut zero = vec![2, kind, 0, 0, 0, CHAIN_HOP_V2, 0, 0, 0];
            zero.extend_from_slice(a(2).as_slice());
            assert!(matches!(
                check_swap(&leg(0, zero)).unwrap_err(),
                EncodeError::BadChain("hop")
            ));
        }
        // A kind past the last (8, Fluid) is none.
        let mut seven = vec![2, 9, 0x00, 0x0b, 0xb8, CHAIN_HOP_V2, 0, 0, 0];
        seven.extend_from_slice(a(2).as_slice());
        assert!(matches!(
            check_swap(&leg(0, seven)).unwrap_err(),
            EncodeError::BadChain("hop")
        ));
    }

    /// A pool-direct V3 leg is the pool, then optionally the factory id of a
    /// fork (1: SushiSwap, 2: PancakeSwap). Uniswap's id is the absent byte:
    /// an explicit 0 is a second spelling of it, refused so one leg has one
    /// encoding; anything past Pancake's is an unknown factory.
    #[test]
    fn a_pool_direct_v3_leg_names_its_factory_by_one_optional_byte() {
        use crate::types::{V3_FACTORY_PANCAKE, V3_FACTORY_SUSHI, VENUE_UNIV3_POOL};
        let pool_leg = |data: Vec<u8>| SwapLeg {
            venue: VENUE_UNIV3_POOL,
            token_in: a(1),
            token_out: a(3),
            flags: 0,
            amount: 1,
            data,
        };
        let pool = a(9).to_vec();
        check_swap(&pool_leg(pool.clone())).unwrap();
        for fid in [V3_FACTORY_SUSHI, V3_FACTORY_PANCAKE] {
            let mut d = pool.clone();
            d.push(fid);
            check_swap(&pool_leg(d)).unwrap();
        }
        for fid in [0u8, 3, 255] {
            let mut d = pool.clone();
            d.push(fid);
            assert!(matches!(
                check_swap(&pool_leg(d)).unwrap_err(),
                EncodeError::BadV3Factory(f) if f == fid
            ));
        }
        for len in [0usize, 19, 22, 40] {
            assert!(matches!(
                check_swap(&pool_leg(vec![9; len])).unwrap_err(),
                EncodeError::BadPoolDataLen(l) if l == len
            ));
        }
    }

    /// Venues 11 (Balancer: a 32-byte pool id) and 12 (Fluid DEX: pool and a
    /// 0/1 direction byte) are the leg's data and nothing else; the pool is
    /// authenticated on chain by the `DexModule`.
    #[test]
    fn balancer_and_fluid_legs_carry_their_pool_and_nothing_else() {
        use crate::types::{VENUE_BALANCER, VENUE_FLUID};
        let leg = |venue: u8, data: Vec<u8>| SwapLeg {
            venue,
            token_in: a(1),
            token_out: a(3),
            flags: 0,
            amount: 1,
            data,
        };
        check_swap(&leg(VENUE_BALANCER, vec![7; 32])).unwrap();
        for len in [0usize, 20, 31, 33, 52] {
            assert!(matches!(
                check_swap(&leg(VENUE_BALANCER, vec![7; len])).unwrap_err(),
                EncodeError::BadBalancerData(l) if l == len
            ));
        }
        for dir in [0u8, 1] {
            let mut d = a(9).to_vec();
            d.push(dir);
            check_swap(&leg(VENUE_FLUID, d)).unwrap();
        }
        let mut bad_dir = a(9).to_vec();
        bad_dir.push(2);
        assert!(matches!(
            check_swap(&leg(VENUE_FLUID, bad_dir)).unwrap_err(),
            EncodeError::BadFluidData(21)
        ));
        for len in [0usize, 20, 22, 40] {
            assert!(matches!(
                check_swap(&leg(VENUE_FLUID, vec![1; len])).unwrap_err(),
                EncodeError::BadFluidData(l) if l == len
            ));
        }
        // 13 is no venue.
        assert!(matches!(
            check_swap(&leg(13, vec![])).unwrap_err(),
            EncodeError::UnknownVenue(13)
        ));
    }

    /// A Balancer hop (extra: 32 bytes) and a Fluid hop (extra: 21) name
    /// their extras by offset, sell an exact input only, and need the exact
    /// length the Executor's `_chainCheck` implies.
    #[test]
    fn balancer_and_fluid_hops_are_exact_input_with_their_extras() {
        use crate::types::{CHAIN_HOP_BALANCER, CHAIN_HOP_FLUID};
        // Two hops: base 1 + 2·4 + 20 = 29; the pool id at 29, the Fluid
        // extra at 61; 82 bytes in all.
        let mut d = vec![2, CHAIN_HOP_BALANCER, 0, 0, 29, CHAIN_HOP_FLUID, 0, 0, 61];
        d.extend_from_slice(a(2).as_slice());
        d.extend_from_slice(&[0x11; 32]);
        d.extend_from_slice(&[0x22; 21]);
        check_swap(&leg(0, d.clone())).unwrap();
        check_swap(&leg(LEG_TAKE_BALANCE, d.clone())).unwrap();
        assert!(matches!(
            check_swap(&leg(LEG_EXACT_OUT, d.clone())).unwrap_err(),
            EncodeError::BadChain(
                "exact output through a hop with an extra (V4, Curve, Balancer, Fluid)"
            )
        ));
        let bad = |d: Vec<u8>| check_swap(&leg(0, d)).unwrap_err();
        let mut short = d.clone();
        short.pop();
        assert!(matches!(bad(short), EncodeError::BadChain("extra")));
        let mut long = d.clone();
        long.push(0);
        assert!(matches!(bad(long), EncodeError::BadChain("length")));
        let mut early = d.clone();
        early[4] = 28; // inside the hops and tokens
        assert!(matches!(bad(early), EncodeError::BadChain("extra")));
        // Kind 9 is none.
        let mut nine = d;
        nine[1] = 9;
        assert!(matches!(bad(nine), EncodeError::BadChain("hop")));
    }

    /// V4 and Curve hops name their extras by offset; they sell an exact
    /// input only, as the Executor's `_chainCheck` refuses otherwise.
    #[test]
    fn v4_and_curve_hops_are_exact_input_with_their_extras() {
        check_swap(&leg(0, curve_then_v4())).unwrap();
        check_swap(&leg(LEG_TAKE_BALANCE, curve_then_v4())).unwrap();
        assert!(matches!(
            check_swap(&leg(LEG_EXACT_OUT, curve_then_v4())).unwrap_err(),
            EncodeError::BadChain(
                "exact output through a hop with an extra (V4, Curve, Balancer, Fluid)"
            )
        ));
        let bad = |d: Vec<u8>| check_swap(&leg(0, d)).unwrap_err();
        let mut early = curve_then_v4();
        early[4] = 28; // inside the hops and tokens
        assert!(matches!(bad(early), EncodeError::BadChain("extra")));
        let mut past = curve_then_v4();
        past[8] = 53; // the key would run past the end
        assert!(matches!(bad(past), EncodeError::BadChain("extra")));
        let mut trailing = curve_then_v4();
        trailing.push(0);
        assert!(matches!(bad(trailing), EncodeError::BadChain("length")));
        let mut short = curve_then_v4();
        short.pop();
        assert!(matches!(bad(short), EncodeError::BadChain("extra")));
    }

    #[test]
    fn a_well_formed_chain_passes() {
        check_swap(&leg(LEG_EXACT_OUT, two_hops())).unwrap();
    }

    #[test]
    fn malformed_chains_are_refused() {
        let bad = |flags: u8, data: Vec<u8>| check_swap(&leg(flags, data)).unwrap_err();
        // Exact input sells along the path (a profit closer).
        check_swap(&leg(LEG_TAKE_BALANCE, two_hops())).unwrap();
        assert!(matches!(
            bad(LEG_EXACT_OUT, vec![]),
            EncodeError::BadChain("empty")
        ));
        assert!(matches!(
            bad(LEG_EXACT_OUT, vec![0]),
            EncodeError::BadChain("hop count")
        ));
        assert!(matches!(
            bad(LEG_EXACT_OUT, vec![5]),
            EncodeError::BadChain("hop count")
        ));
        let mut short = two_hops();
        short.pop();
        assert!(matches!(
            bad(LEG_EXACT_OUT, short),
            EncodeError::BadChain("length")
        ));
        let mut kind = two_hops();
        kind[1] = 9;
        assert!(matches!(
            bad(LEG_EXACT_OUT, kind),
            EncodeError::BadChain("hop")
        ));
        let mut no_fee = two_hops();
        no_fee[2..5].copy_from_slice(&[0, 0, 0]);
        assert!(matches!(
            bad(LEG_EXACT_OUT, no_fee),
            EncodeError::BadChain("hop")
        ));
        let mut factory = two_hops();
        factory[8] = 2;
        assert!(matches!(
            bad(LEG_EXACT_OUT, factory),
            EncodeError::BadChain("hop")
        ));
        let mut zero_mid = two_hops();
        zero_mid[9..].copy_from_slice(&[0u8; 20]);
        assert!(matches!(
            bad(LEG_EXACT_OUT, zero_mid),
            EncodeError::BadChain("intermediate token")
        ));
        let mut end_mid = two_hops();
        end_mid[9..].copy_from_slice(a(3).as_slice());
        assert!(matches!(
            bad(LEG_EXACT_OUT, end_mid),
            EncodeError::BadChain("intermediate token")
        ));
    }
}
