//! Last healthy price (bisection on the oracle integer) and accrual crossing.

use alloy_primitives::U256;
use liq_protocol::{PositionRef, ProtocolError, Result, Timestamp};
use liq_types::fixed::{mul_div, FixedError, Rounding};
use liq_types::price::SourceKind;
use liq_types::{AssetId, Price, PriceVector, Ray};

use crate::config::Config;
use crate::health::{price_p, walk, Account};
use crate::layout::PoolMeta;
use crate::math::{hf_wad_to_ray, HF_THRESHOLD_WAD};

pub const HORIZON: u64 = 10 * 365 * 24 * 3600;

fn sentinel_ok(meta: &PoolMeta, ts: u64) -> bool {
    if meta.sentinel_present == 0 {
        return true;
    }
    if meta.sequencer_updated_at == 0 {
        return false;
    }
    meta.sequencer_answer == 0
        && ts >= u64::from(meta.sequencer_updated_at).saturating_add(u64::from(meta.sentinel_grace))
}

fn hf_wad_at(cfg: &Config, pos: PositionRef<'_>, px: &PriceVector, ts: Timestamp) -> Result<U256> {
    let mut at = pos;
    at.timestamp = ts;
    let scale = U256::from(cfg.oracle_scale());
    let meta: &PoolMeta = at
        .markets
        .first()
        .ok_or(ProtocolError::UnknownMarket(at.key.market))?
        .body()?;
    let mut acc = Account::new(
        at.key.market,
        sentinel_ok(meta, ts),
        cfg.liquidation.oracle_decimals,
    );
    walk(&at, |a| price_p(px, a, scale), |t| acc.add(t, ts))?;
    acc.hf_wad()
}

pub(crate) fn time_to_cross(
    cfg: &Config,
    pos: PositionRef<'_>,
    px: &PriceVector,
) -> Result<Option<Timestamp>> {
    let now = pos.timestamp;
    let start = hf_wad_at(cfg, pos, px, now)?;
    if start == U256::MAX {
        return Ok(None);
    }
    if start < HF_THRESHOLD_WAD {
        return Ok(Some(now));
    }
    let mut hi = now.checked_add(HORIZON).ok_or(FixedError::Overflow)?;
    if hf_wad_at(cfg, pos, px, hi)? >= HF_THRESHOLD_WAD {
        return Ok(None);
    }
    let mut lo = now;
    while hi.wrapping_sub(lo) > 1 {
        let mid = lo.wrapping_add(hi.wrapping_sub(lo) / 2);
        if hf_wad_at(cfg, pos, px, mid)? >= HF_THRESHOLD_WAD {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(Some(hi))
}

fn hf_at_p(
    cfg: &Config,
    pos: PositionRef<'_>,
    px: &PriceVector,
    asset: AssetId,
    p_int: U256,
) -> Result<Ray> {
    let scale = U256::from(cfg.oracle_scale());
    let raw = p_int.checked_mul(scale).ok_or(FixedError::Overflow)?;
    let mut v = px.clone();
    let i = usize::from(asset.0);
    let e =
        v.0.get_mut(i)
            .filter(|p| p.asset == asset)
            .ok_or(ProtocolError::MissingPrice(asset))?;
    e.price = Ray::from_raw(raw);
    let meta: &PoolMeta = pos
        .markets
        .first()
        .ok_or(ProtocolError::UnknownMarket(pos.key.market))?
        .body()?;
    let mut acc = Account::new(
        pos.key.market,
        sentinel_ok(meta, pos.timestamp),
        cfg.liquidation.oracle_decimals,
    );
    walk(
        &pos,
        |a| price_p(&v, a, scale),
        |t| acc.add(t, pos.timestamp),
    )?;
    hf_wad_to_ray(acc.hf_wad()?)
}

pub(crate) fn liquidation_price(
    cfg: &Config,
    pos: PositionRef<'_>,
    px: &PriceVector,
    asset: AssetId,
) -> Result<Option<Price>> {
    let scale = U256::from(cfg.oracle_scale());
    let mut held = false;
    walk(
        &pos,
        |a| price_p(px, a, scale),
        |t| {
            held |= t.row.asset == asset;
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
    let cur = entry
        .price
        .raw()
        .checked_div(scale)
        .ok_or(FixedError::DivisionByZero)?;
    if cur.is_zero() {
        return Ok(None);
    }
    let hf_cur = hf_at_p(cfg, pos, px, asset, cur)?;
    let hf_up = hf_at_p(
        cfg,
        pos,
        px,
        asset,
        cur.checked_add(U256::ONE).ok_or(FixedError::Overflow)?,
    )?;
    let coll_like = hf_up >= hf_cur;
    // Bracket a crossing of Ray::ONE on the oracle integer.
    let mut lo = U256::ONE;
    let mut hi = {
        let mut h = cur;
        for _ in 0..96u8 {
            h = h.saturating_mul(U256::from(2u8)).max(U256::from(2u8));
            if h == U256::MAX {
                break;
            }
            let hf = hf_at_p(cfg, pos, px, asset, h)?;
            if coll_like && hf >= Ray::ONE {
                continue;
            }
            if !coll_like && hf < Ray::ONE {
                continue;
            }
            break;
        }
        h
    };
    if coll_like {
        // healthy for p >= p*; last healthy is min p with hf>=1? No: last healthy
        // toward danger is decreasing p, so max p with hf>=1 when falling...
        // coll_like: higher p healthier, danger is lower p. Last healthy = min p with hf>=1?
        // Last healthy = largest p that is still the boundary from danger: the smallest
        // integer p with hf>=1, returned as that * scale (one unit below is unhealthy).
        if hf_at_p(cfg, pos, px, asset, U256::ONE)? >= Ray::ONE {
            return Ok(None);
        }
        if hf_at_p(cfg, pos, px, asset, hi)? < Ray::ONE {
            return Ok(None);
        }
        while hi.saturating_sub(lo) > U256::ONE {
            let mid = lo
                .checked_add(hi)
                .ok_or(FixedError::Overflow)?
                .wrapping_div(U256::from(2u8));
            if hf_at_p(cfg, pos, px, asset, mid)? >= Ray::ONE {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        if hi < U256::from(2u8) {
            return Ok(None);
        }
        let raw = hi.checked_mul(scale).ok_or(FixedError::Overflow)?;
        return Ok(Some(Price {
            asset,
            price: Ray::from_raw(raw),
            source: SourceKind::Derived {
                deps: smallvec::smallvec![asset],
            },
            block: entry.block,
            ts: entry.ts,
        }));
    }
    // debt-like: higher p worse. Last healthy = max p with hf>=1; RAY is (p+1)*scale-1.
    if hf_at_p(cfg, pos, px, asset, U256::ONE)? < Ray::ONE {
        return Ok(None);
    }
    lo = U256::ONE;
    if hf_at_p(cfg, pos, px, asset, hi)? >= Ray::ONE {
        return Ok(None);
    }
    while hi.saturating_sub(lo) > U256::ONE {
        let mid = lo
            .checked_add(hi)
            .ok_or(FixedError::Overflow)?
            .wrapping_div(U256::from(2u8));
        if hf_at_p(cfg, pos, px, asset, mid)? >= Ray::ONE {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    if lo.is_zero() {
        return Ok(None);
    }
    let raw = lo
        .checked_add(U256::ONE)
        .and_then(|p| p.checked_mul(scale))
        .ok_or(FixedError::Overflow)?
        .wrapping_sub(U256::ONE);
    let _ = mul_div;
    let _ = Rounding::Down;
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
