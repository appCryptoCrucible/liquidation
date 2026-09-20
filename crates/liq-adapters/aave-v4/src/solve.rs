//! The two solvers over `health()`: the last healthy price of one asset
//! (rational, exact) and the accrual crossing time (bracketed search).

use alloy_primitives::U256;
use liq_protocol::{PositionRef, ProtocolError, Result, Timestamp};
use liq_types::fixed::{mul_div, FixedError, Rounding};
use liq_types::price::SourceKind;
use liq_types::{AssetId, Price, PriceVector, Ray};

use crate::health::{price_p8, walk, Account};
use crate::math::{HF_THRESHOLD_WAD, P8_TO_RAY};

/// `10^23 = bpsToWad · RAY / WAD`: `hf >= 1e18` ⇔ `Σ cf·V · 1e23 >= Σ D_ray`.
///
/// `healthFactor = floor(W · 1e14 · 1e27 / D)` and `floor(x) >= n ⇔ x >= n`
/// for integer `n`, so `hf >= 1e18 ⇔ W · 1e41 >= 1e18 · D ⇔ W · 1e23 >= D`.
/// Exact: no division is involved.
const W_SCALE: U256 = alloy_primitives::uint!(100_000_000_000_000_000_000_000_U256);

/// `Protocol::liquidation_price` body.
///
/// Every term of `hf` is linear in one asset's price with the others fixed:
/// a collateral slot contributes `cf · assets · 10^(18−d) · p`, a debt slot
/// `debtRay · 10^(18−d) · p`. With `X` the slots holding `asset`,
///
/// `healthy ⇔ p · (1e23·Σ_X cf·a·s) + 1e23·C_other >= p · Σ_X dr·s + D_other`.
///
/// Net collateral (`coef > 0`): healthy for `p >= (D_other − C_other)/coef`;
/// the last healthy 8-decimal price is `p* = ceil(·)`, and the returned RAY
/// price is `p* · 1e19` — one RAY unit below it the oracle sees `p* − 1`.
/// Net debt (`coef < 0`): healthy for `p <= (C_other − D_other)/|coef|`;
/// `p* = floor(·)` and the RAY price is `(p* + 1) · 1e19 − 1`, the largest
/// that still reads as `p*`. `None` when the asset is not held, `coef == 0`,
/// or the boundary lies at or below the oracle's zero (price `0` is the
/// chain's `InvalidPrice` revert, so no valid price crosses).
pub(crate) fn liquidation_price(
    pos: PositionRef<'_>,
    px: &PriceVector,
    asset: AssetId,
) -> Result<Option<Price>> {
    let mut held = false;
    let mut coef_coll = U256::ZERO;
    let mut coef_debt = U256::ZERO;
    let mut c_other = U256::ZERO;
    let mut d_other = U256::ZERO;
    walk(
        &pos,
        |a| price_p8(px, a),
        |t| {
            let is_x = t.row.asset == asset;
            held |= is_x;
            if let Some(c) = t.collateral {
                let w =
                    c.cf.checked_mul(if is_x { c.per_price } else { c.value })
                        .and_then(|w| w.checked_mul(W_SCALE))
                        .ok_or(FixedError::Overflow)?;
                let acc = if is_x { &mut coef_coll } else { &mut c_other };
                *acc = acc.checked_add(w).ok_or(FixedError::Overflow)?;
            }
            if let Some(d) = t.debt {
                let (v, acc) = if is_x {
                    (d.per_price, &mut coef_debt)
                } else {
                    (d.value_ray, &mut d_other)
                };
                *acc = acc.checked_add(v).ok_or(FixedError::Overflow)?;
            }
            Ok(())
        },
    )?;
    if !held {
        return Ok(None);
    }
    let entry =
        px.0.get(usize::from(asset.0))
            .filter(|p| p.asset == asset)
            .ok_or(ProtocolError::MissingPrice(asset))?;

    let raw = if coef_coll > coef_debt {
        // Falls into danger: p* = ceil((D − C) / coef), need p* >= 2 so that
        // p* − 1 is a valid (nonzero) price on the unhealthy side.
        let coef = coef_coll.wrapping_sub(coef_debt);
        let Some(num) = d_other.checked_sub(c_other) else {
            return Ok(None);
        };
        let p8 = mul_div(num, U256::ONE, coef, Rounding::Up)?;
        if p8 < U256::from(2u8) {
            return Ok(None);
        }
        p8.checked_mul(P8_TO_RAY).ok_or(FixedError::Overflow)?
    } else if coef_debt > coef_coll {
        // Rises into danger: p* = floor((C − D) / coef), need p* >= 1.
        let coef = coef_debt.wrapping_sub(coef_coll);
        let Some(num) = c_other.checked_sub(d_other) else {
            return Ok(None);
        };
        let p8 = mul_div(num, U256::ONE, coef, Rounding::Down)?;
        if p8.is_zero() {
            return Ok(None);
        }
        p8.checked_add(U256::ONE)
            .and_then(|p| p.checked_mul(P8_TO_RAY))
            .ok_or(FixedError::Overflow)?
            .wrapping_sub(U256::ONE)
    } else {
        return Ok(None);
    };
    Ok(Some(Price {
        asset,
        price: Ray::from_raw(raw),
        source: SourceKind::Derived {
            deps: smallvec::smallvec![asset],
        },
        block: entry.block,
        ts: entry.ts,
    }))
}

/// Search horizon for [`time_to_cross`]: ten years of seconds. A position
/// that accrual does not bring to the boundary within it reports `None`.
pub const HORIZON: u64 = 10 * 365 * 24 * 3600;

/// `healthFactor` (WAD) of `pos` evaluated at `ts` with prices fixed.
#[inline]
fn hf_wad_at(pos: PositionRef<'_>, px: &PriceVector, ts: Timestamp) -> Result<U256> {
    let mut at = pos;
    at.timestamp = ts;
    let mut acc = Account::new(pos.key.market);
    walk(&at, |a| price_p8(px, a), |t| acc.add(t))?;
    acc.hf_wad()
}

/// `Protocol::time_to_cross` body.
///
/// With prices fixed, every debt term is `shares · idx(t)` and every
/// collateral term a share conversion over `totalAddedAssets(idx(t))`, both
/// affine in `t` up to the chain's per-step rounding, so `hf(t)` is a ratio
/// of affine functions — monotone on `[now, ∞)`. The crossing is therefore
/// bracketed by `hf(now) >= 1 > hf(now + HORIZON)` and found by bisection
/// to the second; each probe is one `health()`.
///
/// Returns `Some(now)` when already below the boundary, `None` with no debt
/// or when the horizon is still healthy.
pub(crate) fn time_to_cross(pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Timestamp>> {
    let now = pos.timestamp;
    let start = hf_wad_at(pos, px, now)?;
    if start == U256::MAX {
        return Ok(None);
    }
    if start < HF_THRESHOLD_WAD {
        return Ok(Some(now));
    }
    let mut hi = now.checked_add(HORIZON).ok_or(FixedError::Overflow)?;
    if hf_wad_at(pos, px, hi)? >= HF_THRESHOLD_WAD {
        return Ok(None);
    }
    // Invariant: hf(lo) >= 1 > hf(hi).
    let mut lo = now;
    while hi.wrapping_sub(lo) > 1 {
        let mid = lo.wrapping_add(hi.wrapping_sub(lo) / 2);
        if hf_wad_at(pos, px, mid)? >= HF_THRESHOLD_WAD {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(Some(hi))
}
