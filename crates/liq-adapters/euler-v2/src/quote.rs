//! Euler `calculateMaxLiquidation` — one debt vault, N collateral vaults.
//! Repay is per chosen collateral. The quote is one path: the preferred
//! `(repay, seize)` pair, so `max_repay` cannot overstate a smaller coll.

use alloy_primitives::U256;
use liq_protocol::SlotRef;
use liq_protocol::{
    BonusCurve, HealthState, PositionRef, ProtocolError, Quote, RepayOption, Result, SeizeOption,
};
use liq_types::fixed::{mul_div, FixedError, Rounding};
use liq_types::PriceVector;
use smallvec::SmallVec;

use crate::health::{finish, terms};
use crate::layout::{CollRow, UserExtra, DEBT_SLOT, UNMAPPED_ASSET};
use crate::math::{addr_from, bonus_ray, max_liquidation, value_wad};

/// Basis points of `max_repay` given back so one block of interest accrual
/// between quote and inclusion cannot trip `E_ExcessiveRepayAmount` (E3).
/// One bp is far above a block of accrual at any plausible rate and far
/// below the bonus, so it costs nothing in the good case.
const REPAY_HEADROOM_BPS: u16 = 1;

/// Euler stores the discount factor in WAD; [`BonusCurve::Reciprocal`] takes
/// RAY. Exact: RAY is WAD scaled by 1e9.
#[inline]
fn wad_to_ray(wad: U256) -> Result<liq_types::Ray> {
    wad.checked_mul(U256::from(1_000_000_000u64))
        .map(liq_types::Ray::from_raw)
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))
}

#[inline]
fn cell(v: &[u128], slot: u16) -> u128 {
    v.get(usize::from(slot)).copied().unwrap_or(0)
}

pub(crate) fn quote(pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Quote>> {
    let t0 = terms(pos)?;
    let (t, health) = finish(&t0, pos, px, None)?;
    if health.state != HealthState::Liquidatable {
        return Ok(None);
    }
    if t.df.is_zero() || t.liability_value.is_zero() {
        return Err(ProtocolError::EmptyQuote);
    }
    let bonus = bonus_ray(t.df)?;
    // E4. Euler's discount is health-dependent: `df = max(hf, min_df)` and
    // the liquidator is paid `1/df - 1`. Quoting that as `Static` pinned the
    // bonus at the health the quote happened to be built at and claimed it
    // held everywhere — throwing away exactly the Aave-V4-shaped edge this
    // engine exists to exploit, and making the assertion below a tautology
    // (`Static::bonus_at_hf` returns its own field for any input).
    //
    // With the real curve the assertion is a genuine cross-check of two
    // independent expressions: `bonus_ray(t.df)` from the adapter's own
    // discount factor, against the curve evaluated at the reported health.
    let curve = BonusCurve::Reciprocal {
        min_df: wad_to_ray(t.min_df)?,
    };
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
                call_target: addr_from(coll.vault),
                slot: SlotRef::ByAsset,
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
    let Some((mut seize, mut max_repay, _)) = best else {
        return Err(ProtocolError::EmptyQuote);
    };
    // No caller-side notional cap (GUIDE 12 §4b): `max_repay` is bounded
    // only by the protocol rule (`max_liquidation` above) and the accrual
    // headroom below.
    //
    // E3. `repay` has zero headroom: Euler rejects `repayAssets` above the
    // current maximum with `E_ExcessiveRepayAmount`, and in the
    // collateral-capped regime that maximum strictly DECREASES as debt
    // accrues — so a quote built at block N is always slightly too large at
    // N+1. Give back a basis point. The seize scales with it, keeping the
    // `minYieldBalance` bound consistent with the repay actually sent.
    if !max_repay.is_zero() {
        let before = max_repay;
        let keep_bps = 10_000u32
            .checked_sub(u32::from(REPAY_HEADROOM_BPS))
            .ok_or(FixedError::Underflow)?;
        max_repay = mul_div(
            max_repay,
            U256::from(keep_bps),
            U256::from(10_000u32),
            Rounding::Down,
        )?;
        if max_repay < before && !before.is_zero() {
            seize.max_seize = mul_div(seize.max_seize, max_repay, before, Rounding::Down)?;
        }
    }
    if max_repay.is_zero() || seize.max_seize.is_zero() {
        return Err(ProtocolError::EmptyQuote);
    }
    let mut repay_options = SmallVec::new();
    repay_options.push(RepayOption {
        min_repay: alloy_primitives::U256::ZERO,
        pair_seize: None,
        asset: t.debt_row.asset,
        max_repay,
        slot: SlotRef::ByAsset,
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
