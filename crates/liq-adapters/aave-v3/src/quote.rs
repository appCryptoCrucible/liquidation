//! `LiquidationLogic.executeLiquidationCall` close factor, bonus, dust.

use alloy_primitives::U256;
use liq_protocol::SlotRef;
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

/// `LiquidationLogic._calculateDebt` (Aave >=3.2, pin 8305565ae):
/// `maxLiquidatableDebt = vars.borrowerReserveDebt`, clamped to
/// `vars.totalDebtInBaseCurrency.percentMul(DEFAULT_LIQUIDATION_CLOSE_FACTOR)`
/// **only when the reserve's own debt exceeds that base-currency cap** — the
/// cap is computed from the POSITION's total debt, not the reserve's. Aave
/// 3.0/3.1 was per-reserve; >=3.2, which this adapter pins, is not.
fn max_liquidatable_debt(p: &AmountsIn<'_>) -> Result<U256> {
    let Some(c) = p.coll.collateral else {
        return Err(ProtocolError::Internal);
    };
    let Some(d) = p.debt.debt else {
        return Err(ProtocolError::Internal);
    };
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
    for t in terms.iter().filter(|t| t.seizable(pos.timestamp)) {
        let Some(c) = t.collateral else {
            continue;
        };
        // A reserve governance has deprecated carries LTV/LT/bonus all zero.
        // `health` still counts its balance as seizable, but `liquidationBonus`
        // below 100_00 is not a bonus at all — seizing it pays less collateral
        // than the debt repaid. Skip the option; erroring here (the previous
        // `checked_sub` underflow) failed the ENTIRE quote, so one deprecated
        // reserve made every other collateral on the position unliquidatable.
        let Some(bonus_span) = t.liq_bonus.checked_sub(BPS) else {
            continue;
        };
        // `liquidationProtocolFee` is skimmed from the bonus portion of the
        // seize, so what the liquidator keeps is
        //   to_liq = base·(1 + b) − base·b·f = base·(1 + b·(1 − f))
        // i.e. the realised bonus is exactly `b · (1 − f)`. Quoting the gross
        // `b` overstated profit by the fee rate on every reserve that charges
        // one (10% is the common mainnet setting) and — because this list is
        // ranked by `bonus` — ranked a 7.5%-bonus/30%-fee reserve above a
        // 6%/0% one despite being worse net.
        let fee_pct = U256::from(t.reserve.liq_protocol_fee);
        let keep_bps = BPS.checked_sub(fee_pct).ok_or(FixedError::Underflow)?;
        let net_span = mul_div(bonus_span, keep_bps, BPS, Rounding::Down)?;
        let bonus = Ray::from_raw(net_span.checked_mul(BPS_RAY).ok_or(FixedError::Overflow)?);
        let curve = BonusCurve::Static { bonus };
        // `max_seize` is what this leg can YIELD, so it is net of the fee too.
        // At the collateral-capped bound the protocol takes the whole balance
        // and hands back `c.assets − fee`; the router must not size an exit
        // for collateral that never arrives.
        let gross_bonus_coll = c
            .assets
            .checked_sub(percent_div_floor(c.assets, t.liq_bonus)?)
            .ok_or(FixedError::Underflow)?;
        let max_fee = percent_mul_ceil(gross_bonus_coll, fee_pct)?;
        let max_seize = c.assets.checked_sub(max_fee).ok_or(FixedError::Underflow)?;
        let value = value_ray_of(max_seize, price_ray(t.row.asset)?, t.row.decimals)?;
        seize.push((
            SeizeOption {
                asset: t.row.asset,
                max_seize,
                bonus,
                curve,
                call_target: alloy_primitives::Address::ZERO,
                slot: SlotRef::ByAsset,
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
    for t in terms.iter().filter(|t| t.repayable(pos.timestamp)) {
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
                slot: SlotRef::ByAsset,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod close_factor_boundary {
    use super::{max_liquidatable_debt, AmountsIn};
    use crate::health::{Collateral, Debt, SlotTerms};
    use crate::layout::{Reserve, UserReserve};
    use crate::math::asset_unit;
    use alloy_primitives::U256;
    use bytemuck::Zeroable;
    use liq_protocol::MarketRow;
    use liq_types::AssetId;

    /// Close-factor cap applies only when `hf_wad > close_hf`. Equality is
    /// a 100% close. Flipping the compare to `>=` caps this case.
    #[test]
    fn equality_with_close_hf_does_not_apply_the_partial_cap() {
        let row = MarketRow::blank(AssetId(0), 18);
        let reserve = Reserve::zeroed();
        let user = UserReserve::ZERO;
        let unit = asset_unit(18).unwrap();
        let coll = SlotTerms {
            slot: 0,
            row: &row,
            reserve: &reserve,
            user: &user,
            supply_scaled: 0,
            debt_scaled: 0,
            p: unit,
            liq_idx: U256::ZERO,
            debt_idx: U256::ZERO,
            collateral: Some(Collateral {
                assets: U256::from(1_000u64),
                per_price: U256::from(1u8),
                value: U256::from(1_000u64),
                lt: U256::from(8_000u64),
            }),
            debt: None,
            liq_bonus: U256::ZERO,
        };
        let debt_side = SlotTerms {
            collateral: None,
            debt: Some(Debt {
                assets: U256::from(1_000u64),
                per_price: U256::from(1u8),
                value: U256::from(1_000u64),
            }),
            ..coll
        };
        let close_hf = U256::from(9_500u64);
        let at_eq = AmountsIn {
            coll: &coll,
            debt: &debt_side,
            total_debt_base: U256::from(1_000u64),
            hf_wad: close_hf,
            bonus_bps: U256::ZERO,
            debt_to_cover: U256::ZERO,
            close_factor_bps: U256::from(5_000u64),
            close_hf,
            min_base: U256::from(1u8),
        };
        assert_eq!(max_liquidatable_debt(&at_eq).unwrap(), U256::from(1_000u64));
        let at_above = AmountsIn {
            hf_wad: close_hf + U256::from(1u8),
            ..at_eq
        };
        assert_eq!(
            max_liquidatable_debt(&at_above).unwrap(),
            U256::from(500u64)
        );

        // The cap is on the POSITION's total debt (>=3.2 semantics), not the
        // reserve's own. Two reserves at 1000 each, total 2000: half of 2000
        // is 1000, which this reserve's 1000 is not strictly above, so the
        // full 1000 passes here — a 100% close on this leg is correct
        // because the OTHER reserve is what has headroom left in the total.
        let two_reserves = AmountsIn {
            hf_wad: close_hf + U256::from(1u8),
            total_debt_base: U256::from(2_000u64),
            ..at_eq
        };
        assert_eq!(
            max_liquidatable_debt(&two_reserves).unwrap(),
            U256::from(1_000u64),
            "the close-factor cap is base-currency, computed off the position total"
        );
    }
}
