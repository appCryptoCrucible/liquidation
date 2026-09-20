//! Encoder-only invariants the contract cannot see (PLAN-ENCODING §2a + 10A tails).

use alloy_primitives::{keccak256, Address, B256, U256};
use alloy_sol_types::sol;
use liq_exec::wire::{LegTail, LEG_EXACT_OUT, LEG_TAKE_BALANCE, VENUE_ROUTER, VENUE_UNIV3_POOL};
use liq_protocol::ExecutorAdapter;

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
            if l.protocol_pull == 0 {
                return Err(EncodeError::ZeroPull {
                    pull: l.protocol_pull,
                });
            }
            match (l.adapter, &l.tail) {
                (ExecutorAdapter::AaveV3, LegTail::None) => {}
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
    let exact_out: u128 = g.repay_swaps.iter().try_fold(0u128, |a, s| {
        if s.flags & LEG_EXACT_OUT == 0 {
            return Ok(a);
        }
        a.checked_add(s.amount).ok_or(EncodeError::TooManyLegs)
    })?;
    if exact_out > pull {
        return Err(EncodeError::UnderSeizure { exact_out, pull });
    }
    if exact_out != pull {
        // Dust: EXACT_OUT under-sized vs pull is also a revert on flash repay
        // unless pull-exact_out is the share-rounding dust routed by sweep.
        // The assembler must size EXACT_OUT to `protocol_pull`.
        return Err(EncodeError::RepayNotSizedToPull { exact_out, pull });
    }
    Ok(())
}

fn surplus_debt_routed(p: &BatchPlan, g: &FlashGroup, weth: Address) -> Result<()> {
    let pull: u128 = g.liqs.iter().try_fold(0u128, |a, l| {
        a.checked_add(l.protocol_pull)
            .ok_or(EncodeError::TooManyLegs)
    })?;
    if g.debt_asset == weth || g.flash_amount <= pull {
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
