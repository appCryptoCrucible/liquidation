//! Last-healthy price (bisection on the oracle integer), and the time
//! accrual alone makes a position liquidatable (bisection on the clock).

use alloy_primitives::U256;
use liq_protocol::{HealthState, PositionRef, Result, Timestamp};
use liq_types::fixed::FixedError;
use liq_types::price::SourceKind;
use liq_types::{AssetId, Price, PriceVector, Ray};

use crate::health::{finish, price_ray};
use crate::layout::META_SLOT;

/// Has the position crossed the liquidation boundary at `p_raw`?
///
/// P4. This asked `state == Liquidatable`, which is **not monotone in the
/// price** and so cannot bracket a bisection. Drive a single-collateral
/// position's price far enough down and its collateral value floors to zero,
/// at which point `health` reports `BadDebt` — strictly *worse* than
/// liquidatable, but no longer equal to it. The bisection seeds at `lo = 1`
/// raw RAY, every such position answered `false` there, the
/// `!liquidatable_at(lo)` bracket check bailed, and `liquidation_price`
/// returned `None` for **every single-collateral position** — which is the
/// primary arming signal for the whole adapter.
///
/// `BadDebt` is past the boundary, so it counts as crossed. `Blocked` (seize
/// paused) is a different axis: the position is not callable at any price, so
/// it is correctly not a crossing and the bracket check still bails.
fn crossed_at(pos: PositionRef<'_>, px: &PriceVector, asset: AssetId, p_raw: U256) -> Result<bool> {
    let state = finish(pos, px, Some((asset, p_raw)))?.1.state;
    Ok(matches!(
        state,
        HealthState::Liquidatable | HealthState::BadDebt { .. }
    ))
}

/// How far ahead [`time_to_cross`] looks, as Morpho's does.
pub const HORIZON: u64 = 10 * 365 * 24 * 3600;

/// The first second at which accrual alone, at today's prices, makes the
/// position liquidatable: `health` projected there (each market as
/// `accrueInterest` would leave it, at its last read borrow rate) says
/// liquidatable or bad debt. `Some(now)` when it already is; `None` when it
/// stays healthy over [`HORIZON`], which is also the answer while a
/// market's rate is unread (health is then the stored values, constant in
/// time). Bisection, as Morpho's: interest only grows the debt faster than
/// the collateral's exchange rate when the debt's rate is the higher, and a
/// position whose health rises with time never brackets.
pub(crate) fn time_to_cross(pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Timestamp>> {
    let crossed = |ts: Timestamp| -> Result<bool> {
        let mut at = pos;
        at.timestamp = ts;
        Ok(matches!(
            finish(at, px, None)?.1.state,
            HealthState::Liquidatable | HealthState::BadDebt { .. }
        ))
    };
    let now = pos.timestamp;
    if crossed(now)? {
        return Ok(Some(now));
    }
    let mut hi = now.checked_add(HORIZON).ok_or(FixedError::Overflow)?;
    if !crossed(hi)? {
        return Ok(None);
    }
    let mut lo = now;
    while hi.wrapping_sub(lo) > 1 {
        let mid = lo.wrapping_add(hi.wrapping_sub(lo) / 2);
        if crossed(mid)? {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Ok(Some(hi))
}

fn holds(pos: PositionRef<'_>, asset: AssetId) -> (bool, bool) {
    let mut coll = false;
    let mut debt = false;
    for slot in pos.config.iter() {
        if slot == META_SLOT {
            continue;
        }
        let Some(row) = pos.markets.get(usize::from(slot)) else {
            continue;
        };
        if row.asset != asset {
            continue;
        }
        if pos.supply.get(usize::from(slot)).copied().unwrap_or(0) != 0 {
            coll = true;
        }
        if pos.debt.get(usize::from(slot)).copied().unwrap_or(0) != 0 {
            debt = true;
        }
    }
    (coll, debt)
}

/// Last healthy price: `health` at it is not liquidatable; the adjacent
/// unit toward danger is. Collateral danger is down; debt danger is up.
pub(crate) fn liquidation_price(
    pos: PositionRef<'_>,
    px: &PriceVector,
    asset: AssetId,
) -> Result<Option<Price>> {
    let (t, h) = finish(pos, px, None)?;
    if t.sum_borrow.is_zero() {
        return Ok(None);
    }
    let (is_coll, is_debt) = holds(pos, asset);
    if !is_coll && !is_debt {
        return Ok(None);
    }
    if is_coll && is_debt {
        return Ok(None);
    }
    if t.deprecated_borrow && h.hf >= Ray::ONE {
        return Ok(None);
    }
    let cur = price_ray(px, asset, None)?;
    let tick = px.0.get(usize::from(asset.0));
    let block = tick.map(|p| p.block).unwrap_or(0);
    let ts = tick.map(|p| p.ts).unwrap_or(0);

    let danger_down = is_coll;
    let mut lo = U256::ONE;
    let mut hi = cur
        .checked_mul(U256::from(8u8))
        .ok_or(FixedError::Overflow)?;
    if danger_down {
        if crossed_at(pos, px, asset, cur)? {
            hi = cur;
        }
        if !crossed_at(pos, px, asset, lo)? {
            return Ok(None);
        }
        // last healthy is the largest p in [lo, hi] that is not liquidatable,
        // with p-1 liquidatable.
        while hi.saturating_sub(lo) > U256::ONE {
            let mid = lo
                .checked_add(hi)
                .ok_or(FixedError::Overflow)?
                .checked_div(U256::from(2u8))
                .ok_or(FixedError::DivisionByZero)?;
            if crossed_at(pos, px, asset, mid)? {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let cand = if !crossed_at(pos, px, asset, hi)? {
            hi
        } else {
            return Ok(None);
        };
        return Ok(Some(Price {
            asset,
            price: Ray::from_raw(cand),
            source: SourceKind::Canonical,
            block,
            ts,
        }));
    }
    // Debt: danger is a higher price.
    lo = cur;
    if crossed_at(pos, px, asset, cur)? {
        return Ok(None);
    }
    if !crossed_at(pos, px, asset, hi)? {
        return Ok(None);
    }
    while hi.saturating_sub(lo) > U256::ONE {
        let mid = lo
            .checked_add(hi)
            .ok_or(FixedError::Overflow)?
            .checked_div(U256::from(2u8))
            .ok_or(FixedError::DivisionByZero)?;
        if crossed_at(pos, px, asset, mid)? {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let cand = if !crossed_at(pos, px, asset, lo)? {
        lo
    } else {
        return Ok(None);
    };
    Ok(Some(Price {
        asset,
        price: Ray::from_raw(cand),
        source: SourceKind::Canonical,
        block,
        ts,
    }))
}
