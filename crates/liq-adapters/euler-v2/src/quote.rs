//! Euler `calculateMaxLiquidation` — one debt vault, N collateral vaults.
//! Repay is per chosen collateral. The quote is one path: the preferred
//! `(repay, seize)` pair, so `max_repay` cannot overstate a smaller coll.

use alloy_primitives::U256;
use liq_protocol::{
    BonusCurve, Constraints, HealthState, PositionRef, ProtocolError, Quote, RepayOption, Result,
    SeizeOption,
};
use liq_types::fixed::{mul_div, FixedError, Rounding, WAD_RAY_RATIO};
use liq_types::PriceVector;
use smallvec::SmallVec;

use crate::health::{finish, terms};
use crate::layout::{CollRow, UserExtra, DEBT_SLOT, UNMAPPED_ASSET};
use crate::math::{asset_unit, bonus_ray, max_liquidation, value_wad};

#[inline]
fn cell(v: &[u128], slot: u16) -> u128 {
    v.get(usize::from(slot)).copied().unwrap_or(0)
}

pub(crate) fn quote(
    pos: PositionRef<'_>,
    px: &PriceVector,
    cons: &Constraints,
) -> Result<Option<Quote>> {
    let t0 = terms(pos)?;
    let (t, health) = finish(&t0, pos, px, None)?;
    if health.state != HealthState::Liquidatable {
        return Ok(None);
    }
    if t.df.is_zero() || t.liability_value.is_zero() {
        return Err(ProtocolError::EmptyQuote);
    }
    let bonus = bonus_ray(t.df)?;
    let curve = BonusCurve::Static { bonus };
    let at_hf = curve
        .bonus_at_hf(health.hf)?
        .ok_or(ProtocolError::Internal)?;
    if at_hf != bonus {
        return Err(ProtocolError::Internal);
    }

    let user: &UserExtra = pos.extra.view()?;
    let mut best: Option<(SeizeOption, U256, U256)> = None;
    for slot in pos.config.iter() {
        if slot == DEBT_SLOT {
            continue;
        }
        let row = pos
            .markets
            .get(usize::from(slot))
            .ok_or(ProtocolError::Internal)?;
        if row.asset == UNMAPPED_ASSET {
            return Err(ProtocolError::OracleSourceMismatch);
        }
        let coll: &CollRow = row.body()?;
        if coll.flags & CollRow::RECOGNIZED == 0 {
            continue;
        }
        let bit = 1u128
            .checked_shl(u32::from(slot))
            .ok_or(FixedError::Overflow)?;
        if user.enabled_mask & bit == 0 {
            continue;
        }
        let shares = U256::from(cell(pos.supply, slot));
        if shares.is_zero() {
            continue;
        }
        let p = crate::health::price_ray(px, row.asset, None)?;
        let coll_value = value_wad(shares, p, row.decimals)?;
        let (repay, yield_bal) = max_liquidation(
            t.liability_assets,
            t.liability_value,
            t.coll_adj,
            shares,
            coll_value,
            t.min_df,
        )?;
        if repay.is_zero() {
            continue;
        }
        let seize_value = value_wad(yield_bal, p, row.decimals)?;
        let cand = (
            SeizeOption {
                asset: row.asset,
                max_seize: yield_bal,
                bonus,
                curve,
            },
            repay,
            seize_value,
        );
        let take = match best.as_ref() {
            None => true,
            Some(cur) => {
                cand.0.bonus > cur.0.bonus || (cand.0.bonus == cur.0.bonus && cand.2 > cur.2)
            }
        };
        if take {
            best = Some(cand);
        }
    }
    let Some((seize, mut max_repay, _)) = best else {
        return Err(ProtocolError::EmptyQuote);
    };
    let cap = cons.per_liquidation_notional_cap.raw();
    if cap != U256::MAX {
        let p_debt = crate::health::price_ray(px, t.debt_row.asset, None)?;
        let raw_cap = mul_div(
            cap.checked_mul(WAD_RAY_RATIO).ok_or(FixedError::Overflow)?,
            asset_unit(t.debt_row.decimals)?,
            p_debt,
            Rounding::Down,
        )?;
        max_repay = max_repay.min(raw_cap);
    }
    if max_repay.is_zero() {
        return Err(ProtocolError::EmptyQuote);
    }
    let mut repay_options = SmallVec::new();
    repay_options.push(RepayOption {
        asset: t.debt_row.asset,
        max_repay,
    });
    let mut seize_options = SmallVec::<[SeizeOption; 8]>::new();
    seize_options.push(seize);
    Ok(Some(Quote {
        position: pos.id,
        key: *pos.key,
        repay_options,
        seize_options,
    }))
}
