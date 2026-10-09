//! `getAccountLiquidityInternal` @ `a3214f67` — liquidatable iff shortfall > 0
//! (or the deprecated-market path). Not Aave HF.

use alloy_primitives::U256;
use liq_protocol::{
    AssetMask, BlockReason, Health, HealthState, MarketFlags, MarketRow, MarketSlot, PositionRef,
    ProtocolError, Result,
};
use liq_types::fixed::{FixedError, WAD};
use liq_types::{AssetId, PriceVector, Wad};

use crate::layout::{BorrowSnap, CTokenRow, ComptrollerMeta, UserExtra, META_SLOT, UNMAPPED_ASSET};
use crate::math::{
    accrue, bonus_ray, borrow_balance_stored, ctokens_to_underlying, exchange_rate_stored,
    hf_wad_to_ray, is_deprecated, mul_exp, mul_scalar_truncate_add, value_wad, SLOT_SECONDS,
};

/// A cToken's `(borrowIndex, exchangeRateStored)` at `ts`, as the next
/// `accrueInterest` would leave them: Compound accrues on every
/// liquidation's two cTokens before it checks the account, so a borrower
/// healthy at the tip can be short in the next block on interest alone.
/// Unprojected (the stored values) until the rate and the last accrual's
/// time are known. The exchange rate keeps its stored value when the
/// reserve factor is unknown.
pub(crate) fn projected(body: &CTokenRow, ts: u64) -> Result<(U256, U256)> {
    let stored = (
        U256::from(body.borrow_index),
        U256::from(body.exchange_rate_mantissa),
    );
    if body.flags & CTokenRow::RATE_KNOWN == 0 || body.accrual_ts == 0 {
        return Ok(stored);
    }
    let blocks = ts.saturating_sub(u64::from(body.accrual_ts)) / SLOT_SECONDS;
    if blocks == 0 {
        return Ok(stored);
    }
    let (index, borrows, reserves, fees) = accrue(
        U256::from(body.borrow_index),
        U256::from(body.total_borrows),
        U256::from(body.total_reserves),
        U256::from(body.reserve_factor_mantissa),
        U256::from(body.total_fees),
        U256::from(body.fee_mantissa),
        U256::from(body.borrow_rate_per_block),
        blocks,
    )?;
    let rate = if body.flags & CTokenRow::RF_KNOWN != 0
        && body.flags & CTokenRow::EXRATE_KNOWN != 0
        && body.total_supply != 0
    {
        exchange_rate_stored(
            U256::from(body.cash),
            borrows,
            reserves.checked_add(fees).ok_or(FixedError::Overflow)?,
            U256::from(body.total_supply),
        )?
    } else {
        stored.1
    };
    Ok((index, rate))
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct Terms<'a> {
    pub meta: &'a ComptrollerMeta,
    pub sum_borrow: U256,
    pub deprecated_borrow: bool,
}

pub(crate) type PriceOverride = Option<(AssetId, U256)>;

#[inline]
fn cell(v: &[u128], slot: u16) -> u128 {
    v.get(usize::from(slot)).copied().unwrap_or(0)
}

#[inline]
pub(crate) fn price_ray(px: &PriceVector, asset: AssetId, over: PriceOverride) -> Result<U256> {
    if let Some((a, raw)) = over {
        if a == asset {
            return if raw.is_zero() {
                Err(ProtocolError::MissingPrice(asset))
            } else {
                Ok(raw)
            };
        }
    }
    px.0.get(usize::from(asset.0))
        .filter(|p| p.asset == asset)
        .map(|p| p.price.raw())
        .filter(|r| !r.is_zero())
        .ok_or(ProtocolError::MissingPrice(asset))
}

fn entered(user: &UserExtra, slot: u16) -> Result<bool> {
    let bit = 1u128
        .checked_shl(u32::from(slot))
        .ok_or(FixedError::Overflow)?;
    Ok(user.entered_mask & bit != 0)
}

pub(crate) fn terms(pos: PositionRef<'_>) -> Result<(&ComptrollerMeta, &MarketRow)> {
    let row = pos
        .markets
        .get(usize::from(META_SLOT))
        .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
            market: pos.key.market,
            slot: META_SLOT,
        }))?;
    let meta: &ComptrollerMeta = row.body()?;
    if meta.flags & ComptrollerMeta::PARAMS_KNOWN == 0 {
        return Err(ProtocolError::Internal);
    }
    Ok((meta, row))
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
        .view::<BorrowSnap>()?;
    if snap.interest_index == 0 {
        return Err(ProtocolError::Internal);
    }
    borrow_balance_stored(
        principal,
        projected(body, pos.timestamp)?.0,
        U256::from(snap.interest_index),
    )
}

pub(crate) fn finish<'a>(
    pos: PositionRef<'a>,
    px: &PriceVector,
    over: PriceOverride,
) -> Result<(Terms<'a>, Health)> {
    let (meta, meta_row) = terms(pos)?;
    let user: &UserExtra = pos.extra.view()?;
    if pos.timestamp < u64::from(meta_row.last_update) {
        return Err(ProtocolError::TimestampBeforeUpdate);
    }
    // The Comptroller counts every market an account entered. One outside
    // the config (listed after the pin, or left out) has its token events
    // unfollowed, so no balance of ours speaks for it: fail closed.
    let mut mask = user.entered_mask;
    while mask != 0 {
        let slot = u16::try_from(mask.trailing_zeros()).map_err(|_| ProtocolError::Internal)?;
        mask &= mask.wrapping_sub(1);
        if let Some(row) = pos.markets.get(usize::from(slot)) {
            if slot != META_SLOT && row.asset == UNMAPPED_ASSET {
                return Err(ProtocolError::OracleSourceMismatch);
            }
        }
    }
    let mut sum_coll = U256::ZERO;
    let mut sum_borrow = U256::ZERO;
    let mut unweighted = U256::ZERO;
    let mut sensitivity = AssetMask::EMPTY;
    let mut deprecated_borrow = false;

    for slot in pos.config.iter() {
        if slot == META_SLOT {
            continue;
        }
        if !entered(user, slot)? {
            continue;
        }
        let row = pos
            .markets
            .get(usize::from(slot))
            .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
                market: pos.key.market,
                slot,
            }))?;
        let body: &CTokenRow = row.body()?;
        if body.flags & CTokenRow::LISTED == 0 {
            continue;
        }
        // A market outside the config (listed after the pin, or left out):
        // its token events are not followed, so a zero balance here proves
        // nothing. The Comptroller counts every entered market.
        if row.asset == UNMAPPED_ASSET {
            return Err(ProtocolError::OracleSourceMismatch);
        }
        let ctokens = U256::from(cell(pos.supply, slot));
        let borrow = borrow_now(pos, slot, body)?;
        if ctokens.is_zero() && borrow.is_zero() {
            continue;
        }
        if row.flags.contains(MarketFlags::UNPRICED) || body.flags & CTokenRow::PRICED == 0 {
            return Err(ProtocolError::OracleSourceMismatch);
        }
        if !ctokens.is_zero() && body.flags & CTokenRow::EXRATE_KNOWN == 0 {
            return Err(ProtocolError::Internal);
        }
        let p = price_ray(px, row.asset, over)?;
        if !ctokens.is_zero() {
            let exchange_rate = projected(body, pos.timestamp)?.1;
            let underlying = ctokens_to_underlying(ctokens, exchange_rate)?;
            let raw = value_wad(underlying, p, row.decimals)?;
            unweighted = unweighted.checked_add(raw).ok_or(FixedError::Overflow)?;
            let tokens_to_denom = mul_exp(
                mul_exp(U256::from(body.collateral_factor_mantissa), exchange_rate)?,
                crate::math::compound_price_mantissa(p, row.decimals)?,
            )?;
            sum_coll = mul_scalar_truncate_add(tokens_to_denom, ctokens, sum_coll)?;
            sensitivity = sensitivity.with(slot).ok_or(ProtocolError::Internal)?;
        }
        if !borrow.is_zero() {
            let oracle = crate::math::compound_price_mantissa(p, row.decimals)?;
            sum_borrow = mul_scalar_truncate_add(oracle, borrow, sum_borrow)?;
            sensitivity = sensitivity.with(slot).ok_or(ProtocolError::Internal)?;
            if is_deprecated(
                U256::from(body.collateral_factor_mantissa),
                body.flags & CTokenRow::BORROW_PAUSED != 0,
                U256::from(body.reserve_factor_mantissa),
            ) {
                deprecated_borrow = true;
            }
        }
    }

    let hf = if sum_borrow.is_zero() {
        Health::NO_DEBT_HF
    } else {
        hf_wad_to_ray(crate::math::mul_div_down(sum_coll, WAD, sum_borrow)?)?
    };
    let shortfall = sum_borrow > sum_coll;
    let debt_value = Wad::from_raw(sum_borrow);
    let collateral_value = Wad::from_raw(unweighted);
    let seize_paused = meta.flags & (ComptrollerMeta::SEIZE_PAUSED | ComptrollerMeta::HALTED) != 0;

    let state = if sum_borrow.is_zero() {
        HealthState::Healthy
    } else if unweighted.is_zero() {
        HealthState::BadDebt {
            deficit: debt_value,
        }
    } else if seize_paused {
        HealthState::Blocked {
            reason: BlockReason::Paused,
        }
    } else if shortfall || deprecated_borrow {
        HealthState::Liquidatable
    } else {
        HealthState::Healthy
    };

    let _ = bonus_ray(U256::from(meta.liquidation_incentive_mantissa))?;
    Ok((
        Terms {
            meta,
            sum_borrow,
            deprecated_borrow,
        },
        Health {
            hf,
            debt_value,
            collateral_value,
            price_sensitivity: sensitivity,
            state,
        },
    ))
}

pub(crate) fn health(pos: PositionRef<'_>, px: &PriceVector) -> Result<Health> {
    Ok(finish(pos, px, None)?.1)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod accrual_tests {
    use super::projected;
    use crate::layout::CTokenRow;
    use crate::math::accrue;
    use alloy_primitives::U256;
    use bytemuck::Zeroable;

    fn e(v: u128) -> U256 {
        U256::from(v)
    }

    /// `accrueInterest` @ `a3214f67` worked by hand: rate 2e10 per block,
    /// 3 blocks, so `simpleInterestFactor` = 6e10. On 1e12 of borrows the
    /// interest is 6e10 · 1e12 / 1e18 = 60_000; reserves take 10 % of it
    /// (6_000); the index grows by 6e10 · 1.2e18 / 1e18 = 7.2e10.
    #[test]
    fn accrue_matches_compounds_formulas_worked_by_hand() {
        let (index, borrows, reserves, fees) = accrue(
            e(1_200_000_000_000_000_000),
            e(1_000_000_000_000),
            e(50_000_000_000),
            e(100_000_000_000_000_000),
            e(0),
            e(0),
            e(20_000_000_000),
            3,
        )
        .unwrap();
        assert_eq!(index, e(1_200_000_072_000_000_000));
        assert_eq!(borrows, e(1_000_000_060_000));
        assert_eq!(reserves, e(50_000_006_000));
        assert_eq!(fees, e(0), "a plain fork accrues no fees");
        let none = accrue(e(7), e(9), e(1), e(0), e(4), e(0), e(20_000_000_000), 0).unwrap();
        assert_eq!(none, (e(7), e(9), e(1), e(4)), "no blocks, no interest");
    }

    /// Fuse `finishInterestAccrual` (`CToken.sol` of `0x67db14e7…`): the
    /// same 60_000 of interest, with a 10 % Fuse fee and a 5 % admin fee
    /// (combined 15 %), adds 9_000 to the fees beside the reserves' 6_000.
    /// The projected exchange rate subtracts both: (2e12 + 1_000_000_060_000
    /// − 50_000_006_000 − 1_000_009_000) · 1e18 / 1e14 = 2_949_000_045_000 · 1e4
    /// = 29_490_000_450_000_000.
    #[test]
    fn fuse_fees_accrue_with_interest_and_lower_the_exchange_rate() {
        let (_, _, reserves, fees) = accrue(
            e(1_200_000_000_000_000_000),
            e(1_000_000_000_000),
            e(50_000_000_000),
            e(100_000_000_000_000_000),
            e(1_000_000_000),
            e(150_000_000_000_000_000),
            e(20_000_000_000),
            3,
        )
        .unwrap();
        assert_eq!(reserves, e(50_000_006_000));
        assert_eq!(fees, e(1_000_009_000));
        let mut b = row();
        b.flags |= CTokenRow::RATE_KNOWN;
        b.flags2 = CTokenRow::FUSE | CTokenRow::FEES_KNOWN;
        b.total_fees = 1_000_000_000;
        b.fee_mantissa = 150_000_000_000_000_000;
        assert_eq!(projected(&b, 1_036).unwrap().1, e(29_490_000_450_000_000));
    }

    fn row() -> CTokenRow {
        let mut b = CTokenRow::zeroed();
        b.borrow_index = 1_200_000_000_000_000_000;
        b.total_borrows = 1_000_000_000_000;
        b.total_reserves = 50_000_000_000;
        b.reserve_factor_mantissa = 100_000_000_000_000_000;
        b.cash = 2_000_000_000_000;
        b.total_supply = 100_000_000_000_000;
        b.exchange_rate_mantissa = 29_500_000_000_000_000;
        b.borrow_rate_per_block = 20_000_000_000;
        b.accrual_ts = 1_000;
        b.flags = CTokenRow::LISTED | CTokenRow::RF_KNOWN | CTokenRow::EXRATE_KNOWN;
        b
    }

    /// Blocks are whole slots since the last accrual: 36 s is 3 blocks,
    /// 35 s is 2. The exchange rate is `(cash + borrows − reserves) · 1e18
    /// / supply` on the accrued totals: (2e12 + 1_000_000_060_000 −
    /// 50_000_006_000) · 1e18 / 1e14 = 29_500_000_540_000_000. Without a
    /// known rate nothing moves.
    #[test]
    fn projection_counts_whole_slots_and_needs_a_known_rate() {
        let mut b = row();
        assert_eq!(
            projected(&b, 1_036).unwrap(),
            (e(1_200_000_000_000_000_000), e(29_500_000_000_000_000)),
            "rate unknown: the stored values"
        );
        b.flags |= CTokenRow::RATE_KNOWN;
        assert_eq!(
            projected(&b, 1_036).unwrap(),
            (e(1_200_000_072_000_000_000), e(29_500_000_540_000_000))
        );
        let two = accrue(
            U256::from(b.borrow_index),
            U256::from(b.total_borrows),
            U256::from(b.total_reserves),
            U256::from(b.reserve_factor_mantissa),
            U256::from(b.total_fees),
            U256::from(b.fee_mantissa),
            U256::from(b.borrow_rate_per_block),
            2,
        )
        .unwrap();
        assert_eq!(projected(&b, 1_035).unwrap().0, two.0);
        assert_eq!(
            projected(&b, 1_011).unwrap().0,
            U256::from(b.borrow_index),
            "under one slot"
        );
        b.accrual_ts = 0;
        assert_eq!(
            projected(&b, 1_036).unwrap().0,
            U256::from(b.borrow_index),
            "no accrual seen"
        );
    }
}
