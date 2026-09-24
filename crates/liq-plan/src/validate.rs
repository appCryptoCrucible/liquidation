//! Encoder-only invariants the contract cannot see (PLAN-ENCODING §2a + 10A tails).

use alloy_primitives::{keccak256, Address, B256, U256};
use alloy_sol_types::sol;
use liq_flash::fee_amount;
use liq_protocol::ExecutorAdapter;
use liq_types::fixed::{mul_div, Rounding, RAY, WAD};
use liq_wire::wire::{
    LegTail, LEG_EXACT_OUT, LEG_TAKE_BALANCE, V2_FACTORY_SUSHI, VENUE_CURVE_POOL, VENUE_ROUTER,
    VENUE_UNIV2_POOL, VENUE_UNIV3_POOL,
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
        nonzero(g.flash_source, "flashSource")?;
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
            if l.protocol_pull == 0 && l.adapter != ExecutorAdapter::LiquityV2 {
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
                (ExecutorAdapter::Fluid, LegTail::Fluid { col_per_unit_debt }) => {
                    if col_per_unit_debt.is_zero() {
                        return Err(EncodeError::FluidZeroColPer);
                    }
                    // Pin slip is 1e18. A 1e27-scale tail ExcessSlippage's every T1 leg.
                    if *col_per_unit_debt >= RAY {
                        return Err(EncodeError::FluidColPerNot1e18);
                    }
                }
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
        assert_exact_out_first(&g.repay_swaps)?;
        if g.repay_swaps.iter().any(|s| s.token_out != g.debt_asset) {
            return Err(EncodeError::RepayTargetMismatch {
                group: g.debt_asset,
            });
        }
        size_repay_to_pull(g)?;
        surplus_debt_routed(p, g, ctx.weth)?;
    }
    for s in &p.profit_swaps {
        check_swap(s)?;
        if s.token_out != ctx.weth {
            return Err(EncodeError::ProfitTargetNotWeth { weth: ctx.weth });
        }
    }
    assert_exact_out_first(&p.profit_swaps)?;
    close_collaterals(p)?;
    cascade_ok(&p.groups)?;
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
        VENUE_UNIV3_POOL => {
            if s.data.len() != 20 {
                return Err(EncodeError::BadPoolDataLen(s.data.len()));
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
        VENUE_CURVE_POOL => {
            if s.data.len() != 22 {
                return Err(EncodeError::BadCurveDataLen(s.data.len()));
            }
            if s.flags & LEG_EXACT_OUT != 0 {
                return Err(EncodeError::CurveExactOut);
            }
        }
        v => return Err(EncodeError::UnknownVenue(v)),
    }
    Ok(())
}

fn assert_exact_out_first(blob: &[SwapLeg]) -> Result<()> {
    let mut seen_tb = false;
    for s in blob {
        let tb = s.flags & LEG_TAKE_BALANCE != 0;
        let eo = s.flags & LEG_EXACT_OUT != 0;
        if eo && seen_tb {
            return Err(EncodeError::ExactOutAfterTakeBalance);
        }
        if tb {
            seen_tb = true;
        }
    }
    Ok(())
}

fn close_collaterals(p: &BatchPlan) -> Result<()> {
    for g in &p.groups {
        for l in &g.liqs {
            let closers = all_swaps(p)
                .filter(|s| s.token_in == l.collateral_asset && s.flags & LEG_TAKE_BALANCE != 0)
                .count();
            if closers != 1 {
                return Err(EncodeError::BadCollateralClosure {
                    collateral: l.collateral_asset,
                    closers,
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

fn size_repay_to_pull(g: &FlashGroup) -> Result<()> {
    let pull: u128 = g.liqs.iter().try_fold(0u128, |a, l| {
        a.checked_add(l.protocol_pull)
            .ok_or(EncodeError::TooManyLegs)
    })?;
    // The protocol pulls `pull` before the repay swap. The flash lender then
    // pulls `flash_amount + fee(flash_amount)`. Borrowing the fee as well
    // raises both sides by the fee, so the swap has to buy `pull + fee`.
    if g.flash_amount < pull {
        return Err(EncodeError::FlashShort {
            flash: g.flash_amount,
            pull,
        });
    }
    let fee = fee_amount(g.provider, U256::from(g.flash_amount), g.fee_bps).ok_or(
        EncodeError::UnpriceableFee {
            provider: g.provider,
            fee_bps: g.fee_bps,
        },
    )?;
    let fee_u = u128::try_from(fee).map_err(|_| EncodeError::PremiumOverflow)?;
    let owed = pull
        .checked_add(fee_u)
        .ok_or(EncodeError::PremiumOverflow)?;
    let exact_out: u128 = g.repay_swaps.iter().try_fold(0u128, |a, s| {
        if s.flags & LEG_EXACT_OUT == 0 {
            return Ok(a);
        }
        a.checked_add(s.amount).ok_or(EncodeError::TooManyLegs)
    })?;
    if exact_out > owed {
        return Err(EncodeError::UnderSeizure { exact_out, owed });
    }
    // Exact-input repay legs (Curve has no exact output) cover the rest with
    // an overshoot; the lender's pull enforces the total on chain, and the
    // surplus must be swept ([`surplus_debt_routed`]). Without one, the
    // exact-output legs must buy exactly what is owed.
    if exact_out != owed && !has_exact_in_repay(g) {
        return Err(EncodeError::RepayNotSizedToPull { exact_out, owed });
    }
    Ok(())
}

/// A repay leg that sells a fixed amount into the debt asset.
fn has_exact_in_repay(g: &FlashGroup) -> bool {
    g.repay_swaps
        .iter()
        .any(|s| s.token_out == g.debt_asset && s.flags & (LEG_EXACT_OUT | LEG_TAKE_BALANCE) == 0)
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
