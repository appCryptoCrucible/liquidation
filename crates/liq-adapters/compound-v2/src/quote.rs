//! Close-factor capped repay + seize options. Close factor and incentive
//! come from the comptroller store (admin file), not a `1.08` constant.

use alloy_primitives::U256;
use liq_protocol::{
    BonusCurve, Constraints, HealthState, MarketRow, PositionRef, ProtocolError, Quote,
    RepayOption, Result, SeizeOption,
};
use liq_types::fixed::{mul_div, FixedError, Rounding, WAD_RAY_RATIO};
use liq_types::PriceVector;
use smallvec::SmallVec;

use crate::health::{finish, price_ray};
use crate::layout::{CTokenRow, META_SLOT, UNMAPPED_ASSET};
use crate::math::{
    bonus_ray, borrow_balance_stored, compound_price_mantissa, ctokens_to_underlying,
    is_deprecated, liquidate_calculate_seize_tokens, mul_scalar_truncate, value_wad,
};

#[inline]
fn cell(v: &[u128], slot: u16) -> u128 {
    v.get(usize::from(slot)).copied().unwrap_or(0)
}

fn borrow_now(pos: PositionRef<'_>, slot: u16, body: &CTokenRow) -> Result<U256> {
    let principal = U256::from(cell(pos.debt, slot));
    if principal.is_zero() {
        return Ok(U256::ZERO);
    }
    let snap = pos
        .slot_extra
        .get(usize::from(slot))
        .ok_or(ProtocolError::Internal)?
        .view::<crate::layout::BorrowSnap>()?;
    if snap.interest_index == 0 {
        return Err(ProtocolError::Internal);
    }
    borrow_balance_stored(
        principal,
        U256::from(body.borrow_index),
        U256::from(snap.interest_index),
    )
}

pub(crate) fn quote(
    pos: PositionRef<'_>,
    px: &PriceVector,
    cons: &Constraints,
) -> Result<Option<Quote>> {
    let (t, health) = finish(pos, px, None)?;
    if health.state != HealthState::Liquidatable {
        return Ok(None);
    }
    let incentive = U256::from(t.meta.liquidation_incentive_mantissa);
    let bonus = bonus_ray(incentive)?;
    let curve = BonusCurve::Static { bonus };
    let at_hf = curve
        .bonus_at_hf(health.hf)?
        .ok_or(ProtocolError::Internal)?;
    if at_hf != bonus {
        return Err(ProtocolError::Internal);
    }
    let close = U256::from(t.meta.close_factor_mantissa);
    let cap = cons.per_liquidation_notional_cap.raw();

    let mut repay_options = SmallVec::<[RepayOption; 4]>::new();
    let mut seize_options = SmallVec::<[SeizeOption; 8]>::new();

    for slot in pos.config.iter() {
        if slot == META_SLOT {
            continue;
        }
        let row = pos
            .markets
            .get(usize::from(slot))
            .ok_or(ProtocolError::Internal)?;
        if row.asset == UNMAPPED_ASSET {
            return Err(ProtocolError::OracleSourceMismatch);
        }
        let body: &CTokenRow = row.body()?;
        if body.flags & CTokenRow::LISTED == 0 {
            continue;
        }
        let borrow = borrow_now(pos, slot, body)?;
        if !borrow.is_zero() {
            let deprecated = is_deprecated(
                U256::from(body.collateral_factor_mantissa),
                body.flags & CTokenRow::BORROW_PAUSED != 0,
                U256::from(body.reserve_factor_mantissa),
            );
            let mut max_repay = if deprecated {
                borrow
            } else {
                mul_scalar_truncate(close, borrow)?
            };
            if cap != U256::MAX {
                let p = price_ray(px, row.asset, None)?;
                let raw_cap = mul_div(
                    cap.checked_mul(WAD_RAY_RATIO).ok_or(FixedError::Overflow)?,
                    crate::math::asset_unit(row.decimals)?,
                    p,
                    Rounding::Down,
                )?;
                max_repay = max_repay.min(raw_cap);
            }
            if !max_repay.is_zero() {
                repay_options.push(RepayOption {
                    asset: row.asset,
                    max_repay,
                });
            }
        }
        let ctokens = U256::from(cell(pos.supply, slot));
        if !ctokens.is_zero() {
            if body.flags & CTokenRow::EXRATE_KNOWN == 0 {
                return Err(ProtocolError::Internal);
            }
            let underlying =
                ctokens_to_underlying(ctokens, U256::from(body.exchange_rate_mantissa))?;
            if !underlying.is_zero() {
                seize_options.push(SeizeOption {
                    asset: row.asset,
                    max_seize: underlying,
                    bonus,
                    curve,
                });
            }
        }
    }
    if repay_options.is_empty() || seize_options.is_empty() {
        return Err(ProtocolError::EmptyQuote);
    }
    repay_options.sort_by(|a, b| {
        let pa = price_ray(px, a.asset, None).ok();
        let pb = price_ray(px, b.asset, None).ok();
        let va = pa.and_then(|p| {
            pos.markets
                .iter()
                .find(|r| r.asset == a.asset)
                .and_then(|r| value_wad(a.max_repay, p, r.decimals).ok())
        });
        let vb = pb.and_then(|p| {
            pos.markets
                .iter()
                .find(|r| r.asset == b.asset)
                .and_then(|r| value_wad(b.max_repay, p, r.decimals).ok())
        });
        vb.cmp(&va)
    });
    seize_options.sort_by(|a, b| {
        b.bonus.cmp(&a.bonus).then_with(|| {
            let pa = price_ray(px, a.asset, None).ok();
            let pb = price_ray(px, b.asset, None).ok();
            let va = pa.and_then(|p| {
                pos.markets
                    .iter()
                    .find(|r| r.asset == a.asset)
                    .and_then(|r| value_wad(a.max_seize, p, r.decimals).ok())
            });
            let vb = pb.and_then(|p| {
                pos.markets
                    .iter()
                    .find(|r| r.asset == b.asset)
                    .and_then(|r| value_wad(b.max_seize, p, r.decimals).ok())
            });
            vb.cmp(&va)
        })
    });
    Ok(Some(Quote {
        position: pos.id,
        key: *pos.key,
        repay_options,
        seize_options,
    }))
}

/// Pin seize tokens for a chosen repay/seize pair (tests + 10R preview).
pub fn seize_tokens_for(
    pos: PositionRef<'_>,
    px: &PriceVector,
    repay_asset: liq_types::AssetId,
    seize_asset: liq_types::AssetId,
    repay: U256,
) -> Result<U256> {
    let (t, _) = finish(pos, px, None)?;
    let mut borrowed: Option<(&MarketRow, &CTokenRow)> = None;
    let mut coll: Option<(&MarketRow, &CTokenRow)> = None;
    for (i, row) in pos.markets.iter().enumerate() {
        if i == 0 {
            continue;
        }
        let body: &CTokenRow = row.body()?;
        if row.asset == repay_asset {
            borrowed = Some((row, body));
        }
        if row.asset == seize_asset {
            coll = Some((row, body));
        }
    }
    let (brow, _) = borrowed.ok_or(ProtocolError::Internal)?;
    let (crow, cbody) = coll.ok_or(ProtocolError::Internal)?;
    if cbody.flags & CTokenRow::EXRATE_KNOWN == 0 {
        return Err(ProtocolError::Internal);
    }
    let pb = compound_price_mantissa(price_ray(px, brow.asset, None)?, brow.decimals)?;
    let pc = compound_price_mantissa(price_ray(px, crow.asset, None)?, crow.decimals)?;
    liquidate_calculate_seize_tokens(
        U256::from(t.meta.liquidation_incentive_mantissa),
        pb,
        pc,
        U256::from(cbody.exchange_rate_mantissa),
        repay,
    )
}
