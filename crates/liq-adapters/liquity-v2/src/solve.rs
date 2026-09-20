//! Last healthy collateral price (`ICR = MCR`) and interest-accrual crossing.

use alloy_primitives::U256;
use liq_protocol::{PositionRef, ProtocolError, Result, Timestamp};
use liq_types::fixed::FixedError;
use liq_types::price::SourceKind;
use liq_types::{AssetId, Price, PriceVector, Ray};

use crate::health::{finish, terms};
use crate::math::{mul_div_up, wad_to_ray_price};

pub const HORIZON: u64 = 10 * 365 * 24 * 3600;

fn hf_at_ts(pos: PositionRef<'_>, px: &PriceVector, weth: AssetId, ts: Timestamp) -> Result<Ray> {
    let mut at = pos;
    at.timestamp = ts;
    let t = terms(at)?;
    Ok(finish(&t, px, weth)?.1.hf)
}

pub(crate) fn time_to_cross(
    pos: PositionRef<'_>,
    px: &PriceVector,
    weth: AssetId,
) -> Result<Option<Timestamp>> {
    let now = pos.timestamp;
    let start = hf_at_ts(pos, px, weth, now)?;
    if start.raw() == U256::MAX {
        return Ok(None);
    }
    if start < Ray::ONE {
        return Ok(Some(now));
    }
    let hi = now.checked_add(HORIZON).ok_or(FixedError::Overflow)?;
    if hf_at_ts(pos, px, weth, hi)? >= Ray::ONE {
        return Ok(None);
    }
    let mut lo = now;
    let mut hi = hi;
    while hi.wrapping_sub(lo) > 1 {
        let mid = lo.wrapping_add(hi.wrapping_sub(lo) / 2);
        if hf_at_ts(pos, px, weth, mid)? >= Ray::ONE {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(Some(hi))
}

pub(crate) fn liquidation_price(
    pos: PositionRef<'_>,
    px: &PriceVector,
    asset: AssetId,
    weth: AssetId,
) -> Result<Option<Price>> {
    let t0 = terms(pos)?;
    let (t, _) = finish(&t0, px, weth)?;
    if t.coll_row.asset != asset {
        return Ok(None);
    }
    if t.entire_debt.is_zero() || t.entire_coll.is_zero() {
        return Ok(None);
    }
    let entry =
        px.0.get(usize::from(asset.0))
            .filter(|p| p.asset == asset)
            .ok_or(ProtocolError::MissingPrice(asset))?;
    let mcr = U256::from(t.branch.mcr);
    let p_wad = mul_div_up(mcr, t.entire_debt, t.entire_coll)?;
    if p_wad.is_zero() {
        return Ok(None);
    }
    let raw = wad_to_ray_price(p_wad)?;
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
