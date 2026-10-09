//! `LiquidationLogic.executeLiquidationCall` close factor, bonus, dust.

use alloy_primitives::U256;
use liq_protocol::SlotRef;
use liq_protocol::{
    BonusCurve, HealthState, PositionRef, ProtocolError, Quote, RepayOption, Result, SeizeOption,
};
use liq_types::fixed::{mul_div, FixedError, Rounding};
use liq_types::{AssetId, PriceVector, Ray};
use smallvec::SmallVec;

use crate::config::{CloseFactorScope, Config};
use crate::health::{finish, price_p, walk, Account, SlotTerms};
use crate::layout::PoolMeta;
use crate::math::{
    asset_unit, percent_div_ceil, percent_mul_ceil, percent_mul_floor, value_ray_of, BPS, BPS_RAY,
};

type Terms<'a> = SmallVec<[SlotTerms<'a>; 16]>;

fn sentinel_ok(meta: &PoolMeta, ts: u64) -> bool {
    if meta.halted != 0 || meta.pool_paused != 0 {
        return false;
    }
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
    version: crate::config::AaveVersion,
    scope: CloseFactorScope,
}

struct Amounts {
    repay: U256,
    #[allow(dead_code)]
    seize: U256,
    /// Repays the reserve's whole debt or takes the whole collateral (the
    /// dust rule's first two cases), as of the quote's timestamp.
    whole: bool,
}

/// Seconds of interest a whole-debt or whole-collateral leg is sized for
/// past the quote's timestamp.
///
/// The quote values debt and collateral at the view's timestamp, but the
/// call runs a block or more later, and both have grown by then. Repaying
/// exactly the quoted debt then leaves the interest behind — under
/// `MIN_LEFTOVER_BASE`, so `MustNotLeaveDust` (block 26,098,187: the USDT
/// debt went from 1,534.903797 to 1,534.903823 in twelve seconds and every
/// leg reverted). Aave clamps `debtToCover` to what is owed when it runs,
/// so asking for this window's interest more repays all of it whenever the
/// call lands within the window; the Executor approves the larger amount
/// and zeroes the allowance after.
const ACCRUAL_WINDOW_SECS: u64 = 3_600;
const SECONDS_PER_YEAR: u64 = 365 * 24 * 3_600;

/// Aave >=3.2 (pin 8305565ae) caps from the position's total base-currency
/// debt, and only when this reserve's value exceeds that cap. Spark's
/// deployed `_calculateDebt` (pool impl `0x5ae329…`) ignores that and
/// returns `(stable + variable of this reserve).percentMul(closeFactor)`
/// whenever health is above the threshold.
fn max_liquidatable_debt(p: &AmountsIn<'_>) -> Result<U256> {
    let Some(c) = p.coll.collateral else {
        return Err(ProtocolError::Internal);
    };
    let Some(d) = p.debt.debt else {
        return Err(ProtocolError::Internal);
    };
    if p.scope == CloseFactorScope::ReserveDebt {
        return if p.hf_wad > p.close_hf {
            crate::math::percent_mul(d.assets, p.close_factor_bps)
        } else {
            Ok(d.assets)
        };
    }
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
    let v2 = p.version == crate::config::AaveVersion::V2;
    // V2 (`LendingPoolCollateralManager` `0xcc963272…`) applies the bonus
    // (half-up `percentMul`) before dividing by the collateral's price.
    let max_coll = if v2 {
        crate::math::percent_mul(
            p.debt
                .p
                .checked_mul(debt_to_cover)
                .and_then(|v| v.checked_mul(unit_c))
                .ok_or(FixedError::Overflow)?,
            p.bonus_bps,
        )?
        .checked_div(p.coll.p.checked_mul(unit_d).ok_or(FixedError::Overflow)?)
        .ok_or(FixedError::DivisionByZero)?
    } else {
        percent_mul_floor(base, p.bonus_bps)?
    };
    let (coll_amt, debt_needed) = if max_coll > c.assets && v2 {
        // `...div(debtPrice.mul(10**collDecimals)).percentDiv(bonus)`, half-up.
        let raw = p
            .coll
            .p
            .checked_mul(c.assets)
            .and_then(|v| v.checked_mul(unit_d))
            .ok_or(FixedError::Overflow)?
            .checked_div(p.debt.p.checked_mul(unit_c).ok_or(FixedError::Overflow)?)
            .ok_or(FixedError::DivisionByZero)?;
        (c.assets, crate::math::percent_div(raw, p.bonus_bps)?)
    } else if max_coll > c.assets {
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

/// The most this leg can repay: the close-factor cap, or less where the cap
/// would leave dust.
///
/// `LiquidationLogic` reverts `MustNotLeaveDust` unless a liquidation
/// repays this reserve's whole debt, takes the whole collateral, or leaves
/// at least `MIN_LEFTOVER_BASE` of each. Where the capped amount would
/// leave less, any smaller amount leaves more of both, so the largest that
/// passes is still a valid liquidation; this finds it by bisection. Before,
/// the leg was dropped. Block 26,098,187: the cap on 0.0365431 WBTC would
/// have left $770, and the liquidator repaid 2,465,870 sat, leaving
/// exactly $1,000.00 — the leg this quote used to omit.
fn amounts(p: &AmountsIn<'_>) -> Result<Option<Amounts>> {
    let max_d = max_liquidatable_debt(p)?;
    let cover = p.debt_to_cover.min(max_d);
    if cover.is_zero() {
        return Ok(None);
    }
    if let Some(a) = leaves_no_dust(p, cover)? {
        return Ok(Some(if a.whole { with_accrual(p, a)? } else { a }));
    }
    let (mut lo, mut hi) = (U256::ZERO, cover);
    let mut best = None;
    while hi.checked_sub(lo).ok_or(FixedError::Underflow)? > U256::from(1u8) {
        let mid = lo
            .checked_add(
                hi.checked_sub(lo)
                    .ok_or(FixedError::Underflow)?
                    .checked_div(U256::from(2u8))
                    .ok_or(FixedError::DivisionByZero)?,
            )
            .ok_or(FixedError::Overflow)?;
        match leaves_no_dust(p, mid)? {
            Some(a) => {
                lo = mid;
                best = Some(a);
            }
            None => hi = mid,
        }
    }
    Ok(best)
}

/// A whole-debt or whole-collateral leg asking for [`ACCRUAL_WINDOW_SECS`]
/// of interest more, at the faster of the debt's borrow rate and the
/// collateral's supply rate, rounded up.
fn with_accrual(p: &AmountsIn<'_>, a: Amounts) -> Result<Amounts> {
    let rate = U256::from(p.debt.reserve.variable_borrow_rate)
        .max(U256::from(p.coll.reserve.liquidity_rate));
    let per = rate
        .checked_mul(U256::from(ACCRUAL_WINDOW_SECS))
        .ok_or(FixedError::Overflow)?;
    let year_ray = liq_types::fixed::RAY
        .checked_mul(U256::from(SECONDS_PER_YEAR))
        .ok_or(FixedError::Overflow)?;
    let grow = mul_div(a.repay, per, year_ray, Rounding::Up)?
        .checked_add(U256::from(1u8))
        .ok_or(FixedError::Overflow)?;
    Ok(Amounts {
        repay: a.repay.checked_add(grow).ok_or(FixedError::Overflow)?,
        ..a
    })
}

/// The amounts for repaying `cover`, when Aave's dust rule admits them.
fn leaves_no_dust(p: &AmountsIn<'_>, cover: U256) -> Result<Option<Amounts>> {
    let Some(c) = p.coll.collateral else {
        return Err(ProtocolError::Internal);
    };
    let Some(d) = p.debt.debt else {
        return Err(ProtocolError::Internal);
    };
    let (seize, repay, fee) = available(p, cover)?;
    let leftover_base = p
        .min_base
        .checked_div(U256::from(2u8))
        .ok_or(FixedError::DivisionByZero)?;
    let whole =
        repay >= d.assets || seize.checked_add(fee).ok_or(FixedError::Overflow)? >= c.assets;
    if !whole {
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
    Ok(Some(Amounts {
        repay,
        seize,
        whole,
    }))
}

pub(crate) fn quote(cfg: &Config, pos: PositionRef<'_>, px: &PriceVector) -> Result<Option<Quote>> {
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
        cfg.liquidation.version,
    );
    let mut terms: Terms<'_> = SmallVec::new();
    walk(
        &cfg.liquidation,
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
        let Some(a) = amounts(&AmountsIn {
            coll,
            debt: t,
            total_debt_base: acc.debt_value,
            hf_wad,
            bonus_bps,
            // No caller-side notional cap (GUIDE 12 §4b): `amounts` bounds
            // `max_repay` on its own via the close-factor rule.
            debt_to_cover: U256::MAX,
            close_factor_bps: U256::from(cfg.liquidation.close_factor_bps),
            close_hf: U256::from(cfg.liquidation.close_factor_hf_wad),
            min_base: U256::from(cfg.liquidation.min_base_max_close),
            version: cfg.liquidation.version,
            scope: cfg.liquidation.close_factor_scope,
        })?
        else {
            continue;
        };
        let value = value_ray_of(a.repay, price, t.row.decimals)?;
        repay.push((
            RepayOption {
                min_repay: alloy_primitives::U256::ZERO,
                pair_seize: None,
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::arithmetic_side_effects
)]
mod close_factor_boundary {
    use super::{amounts, max_liquidatable_debt, AmountsIn};
    use crate::health::{Collateral, Debt, SlotTerms};
    use crate::layout::{Reserve, UserReserve};
    use crate::math::asset_unit;
    use alloy_primitives::U256;
    use bytemuck::Zeroable;
    use liq_protocol::MarketRow;
    use liq_types::AssetId;

    /// Oracle: the chain at block 26,098,187 (Aave V3 Core, pool revision
    /// 11). After that block's WETH oracle update the borrower held
    /// 2,063,889,581,742,207,823 wei of WETH ($2,681.12) against 3,654,310
    /// sat of WBTC ($84,143.92173971) in $4,609.23098018 of total debt, HF
    /// 0.9964, e-mode 0; WETH's bonus 10500 and protocol fee 1000. The
    /// close factor caps WBTC at 2,738,897 sat, which leaves $770 — dust —
    /// so Aave accepts at most the amount that leaves $1,000: the real
    /// liquidator's `debtToCover` of 2,465,870 sat, for
    /// `liquidatedCollateralAmount` 808,710,281,724,846,850 wei. The quote
    /// must offer exactly that leg; before, it dropped it.
    #[test]
    fn a_capped_leg_that_would_leave_dust_repays_down_to_the_floor() {
        let (weth_row, wbtc_row) = (
            MarketRow::blank(AssetId(0), 18),
            MarketRow::blank(AssetId(1), 8),
        );
        let mut weth = Reserve::zeroed();
        weth.liq_protocol_fee = 1_000;
        let wbtc = Reserve::zeroed();
        let user = UserReserve::ZERO;
        let (p_weth, p_wbtc) = (
            U256::from(268_112_000_000u64),
            U256::from(8_414_392_173_971u64),
        );
        let coll_assets = U256::from(2_063_889_581_742_207_823u64);
        let debt_assets = U256::from(3_654_310u64);
        let coll = SlotTerms {
            slot: 10,
            row: &weth_row,
            reserve: &weth,
            user: &user,
            supply_scaled: 0,
            debt_scaled: 0,
            p: p_weth,
            liq_idx: U256::ZERO,
            debt_idx: U256::ZERO,
            collateral: Some(Collateral {
                assets: coll_assets,
                per_price: coll_assets,
                value: coll_assets * p_weth / asset_unit(18).unwrap(),
                lt: U256::from(8_300u64),
            }),
            debt: None,
            liq_bonus: U256::from(10_500u64),
        };
        let debt = SlotTerms {
            slot: 11,
            row: &wbtc_row,
            reserve: &wbtc,
            p: p_wbtc,
            collateral: None,
            debt: Some(Debt {
                assets: debt_assets,
                per_price: debt_assets,
                value: debt_assets * p_wbtc / asset_unit(8).unwrap(),
            }),
            liq_bonus: U256::from(10_500u64),
            ..coll
        };
        let p = AmountsIn {
            coll: &coll,
            debt: &debt,
            total_debt_base: U256::from(460_923_098_018u64),
            hf_wad: U256::from(996_443_000_000_000_000u64),
            bonus_bps: U256::from(10_500u64),
            debt_to_cover: U256::MAX,
            close_factor_bps: U256::from(5_000u64),
            close_hf: U256::from(950_000_000_000_000_000u64),
            min_base: U256::from(200_000_000_000u64),
            version: crate::config::AaveVersion::V3,
            scope: crate::config::CloseFactorScope::PositionBase,
        };
        assert_eq!(
            max_liquidatable_debt(&p).unwrap(),
            U256::from(2_738_897u64),
            "the cap"
        );
        let a = amounts(&p).unwrap().expect("a leg");
        assert_eq!(
            a.repay,
            U256::from(2_465_870u64),
            "the liquidator's debtToCover"
        );
        assert_eq!(
            a.seize,
            U256::from(808_710_281_724_846_850u64),
            "the event's liquidatedCollateralAmount"
        );
        let one_more = AmountsIn {
            debt_to_cover: a.repay + U256::from(1u8),
            ..p
        };
        let capped = amounts(&one_more).unwrap().unwrap();
        assert_eq!(capped.repay, a.repay, "one sat more would leave dust");
    }

    /// Oracle: the chain at blocks 26,098,186 → 26,098,187 (twelve seconds
    /// apart). The same borrower's USDT debt was 1,534,903,797 at the first
    /// and 1,534,903,823 at the second (USDT's variable rate 4.383 %); under
    /// $2,000, so the whole of it may be repaid. Quoted at the first
    /// block's timestamp, the leg must still repay all of it when it runs in
    /// the next: `max_repay` covers the 1,534,903,823 owed then (Aave clamps
    /// the rest), and by no more than an hour's interest. Before, it asked
    /// for exactly 1,534,903,797, left 26 behind and reverted
    /// `MustNotLeaveDust`. A partial leg (the WBTC one above) stays exact.
    #[test]
    fn a_whole_debt_leg_covers_the_interest_until_it_runs() {
        let (weth_row, usdt_row) = (
            MarketRow::blank(AssetId(0), 18),
            MarketRow::blank(AssetId(2), 6),
        );
        let mut weth = Reserve::zeroed();
        weth.liq_protocol_fee = 1_000;
        weth.liquidity_rate = 14_707_639_514_169_699_030_719_988;
        let mut usdt = Reserve::zeroed();
        usdt.variable_borrow_rate = 43_831_637_418_001_783_754_149_924;
        let user = UserReserve::ZERO;
        let (p_weth, p_usdt) = (U256::from(268_112_000_000u64), U256::from(99_964_000u64));
        let coll_assets = U256::from(2_063_889_581_742_207_823u64);
        let quoted = U256::from(1_534_903_797u64);
        let coll = SlotTerms {
            slot: 10,
            row: &weth_row,
            reserve: &weth,
            user: &user,
            supply_scaled: 0,
            debt_scaled: 0,
            p: p_weth,
            liq_idx: U256::ZERO,
            debt_idx: U256::ZERO,
            collateral: Some(Collateral {
                assets: coll_assets,
                per_price: coll_assets,
                value: coll_assets * p_weth / asset_unit(18).unwrap(),
                lt: U256::from(8_300u64),
            }),
            debt: None,
            liq_bonus: U256::from(10_500u64),
        };
        let debt = SlotTerms {
            slot: 12,
            row: &usdt_row,
            reserve: &usdt,
            p: p_usdt,
            collateral: None,
            debt: Some(Debt {
                assets: quoted,
                per_price: quoted,
                value: quoted * p_usdt / asset_unit(6).unwrap(),
            }),
            liq_bonus: U256::from(10_450u64),
            ..coll
        };
        let p = AmountsIn {
            coll: &coll,
            debt: &debt,
            total_debt_base: U256::from(460_923_098_018u64),
            hf_wad: U256::from(996_443_000_000_000_000u64),
            bonus_bps: U256::from(10_500u64),
            debt_to_cover: U256::MAX,
            close_factor_bps: U256::from(5_000u64),
            close_hf: U256::from(950_000_000_000_000_000u64),
            min_base: U256::from(200_000_000_000u64),
            version: crate::config::AaveVersion::V3,
            scope: crate::config::CloseFactorScope::PositionBase,
        };
        let a = amounts(&p).unwrap().expect("the whole USDT debt");
        let owed_next_block = U256::from(1_534_903_823u64);
        assert!(
            a.repay >= owed_next_block,
            "{} < {owed_next_block}",
            a.repay
        );
        // An hour at 4.383 % on 1,534.903797 USDT is 7,680 units (+1).
        assert!(
            a.repay <= quoted + U256::from(7_682u64),
            "margin {}",
            a.repay - quoted
        );
    }

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
            version: crate::config::AaveVersion::V3,
            scope: crate::config::CloseFactorScope::PositionBase,
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

    /// Oracle: Spark `LiquidationLogic._calculateDebt` at pool impl
    /// `0x5ae329…`. `percentMul(3, 5000) = (3 * 5000 + 5000) / 10000 = 2`.
    /// The same numbers under the Aave position-base rule leave the reserve
    /// uncapped, so a Spark config that still uses that rule stays green.
    #[test]
    fn spark_close_factor_is_half_up_on_this_reserves_debt() {
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
                assets: U256::from(6u8),
                per_price: U256::from(1u8),
                value: U256::from(6u8),
                lt: U256::from(8_000u64),
            }),
            debt: None,
            liq_bonus: U256::ZERO,
        };
        let debt_side = SlotTerms {
            collateral: None,
            debt: Some(Debt {
                assets: U256::from(3u8),
                per_price: U256::from(1u8),
                value: U256::from(3u8),
            }),
            ..coll
        };
        let close_hf = U256::from(9_500u64);
        let spark = AmountsIn {
            coll: &coll,
            debt: &debt_side,
            total_debt_base: U256::from(6u8),
            hf_wad: close_hf + U256::from(1u8),
            bonus_bps: U256::ZERO,
            debt_to_cover: U256::ZERO,
            close_factor_bps: U256::from(5_000u64),
            close_hf,
            min_base: U256::ZERO,
            version: crate::config::AaveVersion::V3,
            scope: crate::config::CloseFactorScope::ReserveDebt,
        };
        let half_up =
            (U256::from(3u8) * U256::from(5_000u64) + U256::from(5_000u64)) / U256::from(10_000u64);
        assert_eq!(half_up, U256::from(2u8));
        assert_eq!(max_liquidatable_debt(&spark).unwrap(), half_up);
        let aave = AmountsIn {
            scope: crate::config::CloseFactorScope::PositionBase,
            ..spark
        };
        assert_eq!(max_liquidatable_debt(&aave).unwrap(), U256::from(3u8));
        assert_ne!(
            max_liquidatable_debt(&spark).unwrap(),
            max_liquidatable_debt(&aave).unwrap()
        );
    }
}
