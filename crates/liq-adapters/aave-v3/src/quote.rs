//! `LiquidationLogic.executeLiquidationCall` close factor, bonus, dust.

use alloy_primitives::U256;
use liq_protocol::{
    BonusCurve, Constraints, HealthState, PositionRef, ProtocolError, Quote, RepayOption, Result,
    SeizeOption,
};
use liq_types::fixed::{mul_div, FixedError, Rounding, WAD_RAY_RATIO};
use liq_types::{AssetId, PriceVector, Ray};
use smallvec::SmallVec;

use crate::config::Config;
use crate::health::{finish, price_p, walk, Account, SlotTerms};
use crate::layout::PoolMeta;
use crate::math::{
    asset_unit, percent_div_ceil, percent_mul_ceil, percent_mul_floor, value_ray_of, BPS, BPS_RAY,
};

type Terms<'a> = SmallVec<[SlotTerms<'a>; 16]>;

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

struct AmountsIn<'a> {
    coll: &'a SlotTerms<'a>,
    debt: &'a SlotTerms<'a>,
    total_debt_base: U256,
    hf_wad: U256,
    bonus_bps: U256,
    debt_to_cover: U256,
    close_factor_bps: U256,
    close_hf: U256,
    min_base: U256,
}

struct Amounts {
    repay: U256,
    #[allow(dead_code)]
    seize: U256,
}

fn max_liquidatable_debt(p: &AmountsIn<'_>) -> Result<U256> {
    let Some(c) = p.coll.collateral else {
        return Err(ProtocolError::Internal);
    };
    let Some(d) = p.debt.debt else {
        return Err(ProtocolError::Internal);
    };
    let leftover = p
        .min_base
        .checked_div(U256::from(2u8))
        .unwrap_or(U256::ZERO);
    let _ = leftover;
    let mut max_d = d.assets;
    if c.value >= p.min_base && d.value >= p.min_base && p.hf_wad > p.close_hf {
        let cap_base = crate::math::percent_mul(p.total_debt_base, p.close_factor_bps)?;
        if d.value > cap_base {
            max_d = cap_base
                .checked_mul(asset_unit(p.debt.row.decimals)?)
                .ok_or(FixedError::Overflow)?
                .checked_div(p.debt.p)
                .ok_or(FixedError::DivisionByZero)?;
        }
    }
    Ok(max_d)
}

fn available(p: &AmountsIn<'_>, debt_to_cover: U256) -> Result<(U256, U256, U256)> {
    let Some(c) = p.coll.collateral else {
        return Err(ProtocolError::Internal);
    };
    let unit_c = asset_unit(p.coll.row.decimals)?;
    let unit_d = asset_unit(p.debt.row.decimals)?;
    let base = p
        .debt
        .p
        .checked_mul(debt_to_cover)
        .and_then(|v| v.checked_mul(unit_c))
        .ok_or(FixedError::Overflow)?
        .checked_div(p.coll.p.checked_mul(unit_d).ok_or(FixedError::Overflow)?)
        .ok_or(FixedError::DivisionByZero)?;
    let max_coll = percent_mul_floor(base, p.bonus_bps)?;
    let (coll_amt, debt_needed) = if max_coll > c.assets {
        let debt_needed = percent_div_ceil(
            p.coll
                .p
                .checked_mul(c.assets)
                .and_then(|v| v.checked_mul(unit_d))
                .ok_or(FixedError::Overflow)?
                .checked_div(p.debt.p.checked_mul(unit_c).ok_or(FixedError::Overflow)?)
                .ok_or(FixedError::DivisionByZero)?,
            p.bonus_bps,
        )?;
        (c.assets, debt_needed)
    } else {
        (max_coll, debt_to_cover)
    };
    let fee_pct = U256::from(p.coll.reserve.liq_protocol_fee);
    let mut fee = U256::ZERO;
    let mut to_liq = coll_amt;
    if !fee_pct.is_zero() {
        let bonus_coll = coll_amt
            .checked_sub(percent_div_floor(coll_amt, p.bonus_bps)?)
            .ok_or(FixedError::Underflow)?;
        fee = percent_mul_ceil(bonus_coll, fee_pct)?;
        to_liq = coll_amt.checked_sub(fee).ok_or(FixedError::Underflow)?;
    }
    Ok((to_liq, debt_needed, fee))
}

fn percent_div_floor(v: U256, p: U256) -> Result<U256> {
    crate::math::percent_div_floor(v, p)
}

fn amounts(p: &AmountsIn<'_>) -> Result<Option<Amounts>> {
    let Some(c) = p.coll.collateral else {
        return Err(ProtocolError::Internal);
    };
    let Some(d) = p.debt.debt else {
        return Err(ProtocolError::Internal);
    };
    let max_d = max_liquidatable_debt(p)?;
    let cover = p.debt_to_cover.min(max_d);
    if cover.is_zero() {
        return Ok(None);
    }
    let (seize, repay, fee) = available(p, cover)?;
    let leftover_base = p
        .min_base
        .checked_div(U256::from(2u8))
        .ok_or(FixedError::DivisionByZero)?;
    if repay < d.assets && seize.checked_add(fee).ok_or(FixedError::Overflow)? < c.assets {
        let debt_left = crate::math::mul_div_ceil(
            d.assets.checked_sub(repay).ok_or(FixedError::Underflow)?,
            p.debt.p,
            asset_unit(p.debt.row.decimals)?,
        )?;
        let coll_left = c
            .assets
            .checked_sub(seize)
            .and_then(|v| v.checked_sub(fee))
            .ok_or(FixedError::Underflow)?
            .checked_mul(p.coll.p)
            .ok_or(FixedError::Overflow)?
            .checked_div(asset_unit(p.coll.row.decimals)?)
            .ok_or(FixedError::DivisionByZero)?;
        if debt_left < leftover_base || coll_left < leftover_base {
            return Ok(None);
        }
    }
    Ok(Some(Amounts { repay, seize }))
}

pub(crate) fn quote(
    cfg: &Config,
    pos: PositionRef<'_>,
    px: &PriceVector,
    cons: &Constraints,
) -> Result<Option<Quote>> {
    let scale = U256::from(cfg.oracle_scale());
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
    let mut terms: Terms<'_> = SmallVec::new();
    walk(
        &pos,
        |a| price_p(px, a, scale),
        |t| {
            acc.add(t, pos.timestamp)?;
            terms.push(*t);
            Ok(())
        },
    )?;
    let health = finish(&acc)?;
    if health.state != HealthState::Liquidatable {
        return Ok(None);
    }
    let hf_wad = acc.hf_wad()?;
    let price_ray = |asset: AssetId| -> Result<Ray> {
        px.0.get(usize::from(asset.0))
            .filter(|p| p.asset == asset)
            .map(|p| p.price)
            .ok_or(ProtocolError::MissingPrice(asset))
    };
    let mut seize: SmallVec<[(SeizeOption, U256, u16, U256); 8]> = SmallVec::new();
    for t in terms.iter().filter(|t| t.seizable()) {
        let Some(c) = t.collateral else {
            continue;
        };
        let bonus_span = t.liq_bonus.checked_sub(BPS).ok_or(FixedError::Underflow)?;
        let bonus = Ray::from_raw(
            bonus_span
                .checked_mul(BPS_RAY)
                .ok_or(FixedError::Overflow)?,
        );
        let curve = BonusCurve::Static { bonus };
        let value = value_ray_of(c.assets, price_ray(t.row.asset)?, t.row.decimals)?;
        seize.push((
            SeizeOption {
                asset: t.row.asset,
                max_seize: c.assets,
                bonus,
                curve,
            },
            value,
            t.slot,
            t.liq_bonus,
        ));
    }
    seize.sort_by(|a, b| b.0.bonus.cmp(&a.0.bonus).then(b.1.cmp(&a.1)));
    let Some(&(_, _, best_slot, bonus_bps)) = seize.first() else {
        return Err(ProtocolError::EmptyQuote);
    };
    let coll = terms
        .iter()
        .find(|t| t.slot == best_slot)
        .ok_or(ProtocolError::Internal)?;

    let mut repay: SmallVec<[(RepayOption, U256); 4]> = SmallVec::new();
    for t in terms.iter().filter(|t| t.repayable()) {
        let price = price_ray(t.row.asset)?;
        let cap = cons.per_liquidation_notional_cap.raw();
        let debt_to_cover = if cap == U256::MAX {
            U256::MAX
        } else {
            mul_div(
                cap.checked_mul(WAD_RAY_RATIO).ok_or(FixedError::Overflow)?,
                asset_unit(t.row.decimals)?,
                price.raw(),
                Rounding::Down,
            )?
        };
        if debt_to_cover.is_zero() {
            continue;
        }
        let Some(a) = amounts(&AmountsIn {
            coll,
            debt: t,
            total_debt_base: acc.debt_value,
            hf_wad,
            bonus_bps,
            debt_to_cover,
            close_factor_bps: U256::from(cfg.liquidation.close_factor_bps),
            close_hf: U256::from(cfg.liquidation.close_factor_hf_wad),
            min_base: U256::from(cfg.liquidation.min_base_max_close),
        })?
        else {
            continue;
        };
        let value = value_ray_of(a.repay, price, t.row.decimals)?;
        repay.push((
            RepayOption {
                asset: t.row.asset,
                max_repay: a.repay,
            },
            value,
        ));
    }
    repay.sort_by_key(|a| core::cmp::Reverse(a.1));
    if repay.is_empty() {
        return Err(ProtocolError::EmptyQuote);
    }
    Ok(Some(Quote {
        position: pos.id,
        key: *pos.key,
        repay_options: repay.into_iter().map(|(o, _)| o).collect(),
        seize_options: seize.into_iter().map(|(o, _, _, _)| o).collect(),
    }))
}
