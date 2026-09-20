//! Morpho `_isHealthy` @ `8e26ca6a`, plus accrual to `pos.timestamp`.

use alloy_primitives::U256;
use liq_protocol::{
    AssetMask, Health, HealthState, MarketFlags, MarketRow, MarketSlot, PositionRef, ProtocolError,
    Result,
};
use liq_types::fixed::{FixedError, WAD};
use liq_types::{AssetId, PriceVector, Ray, Wad};

use crate::layout::{LoanRow, COLL_SLOT, LOAN_SLOT, UNMAPPED_ASSET};
use crate::math::{
    accrued, asset_unit, hf_wad_to_ray, mul_div_down, to_assets_up, value_wad, w_mul_down,
    ORACLE_PRICE_SCALE,
};

#[derive(Copy, Clone, Debug)]
pub(crate) struct Terms<'a> {
    pub loan: &'a LoanRow,
    pub loan_row: &'a MarketRow,
    pub coll_row: &'a MarketRow,
    pub borrow_shares: U256,
    pub collateral: U256,
    pub borrow_a: U256,
    pub borrow_s: U256,
    pub borrowed: U256,
    pub oracle: U256,
    pub max_borrow: U256,
    pub p_loan: U256,
    pub p_coll: U256,
}

#[inline]
fn cell(v: &[u128], slot: u16) -> u128 {
    v.get(usize::from(slot)).copied().unwrap_or(0)
}

#[inline]
pub(crate) fn price_ray(px: &PriceVector, asset: AssetId) -> Result<U256> {
    px.0.get(usize::from(asset.0))
        .filter(|p| p.asset == asset)
        .map(|p| p.price.raw())
        .filter(|r| !r.is_zero())
        .ok_or(ProtocolError::MissingPrice(asset))
}

/// `IOracle.price` reconstruction: collateral quoted in loan, 1e36.
#[inline]
pub(crate) fn oracle_price(
    p_coll: U256,
    p_loan: U256,
    coll_decimals: u8,
    loan_decimals: u8,
) -> Result<U256> {
    match loan_decimals.cmp(&coll_decimals) {
        core::cmp::Ordering::Equal => mul_div_down(p_coll, ORACLE_PRICE_SCALE, p_loan),
        core::cmp::Ordering::Greater => {
            let lift = asset_unit(loan_decimals.wrapping_sub(coll_decimals))?;
            let scale = ORACLE_PRICE_SCALE
                .checked_mul(lift)
                .ok_or(FixedError::Overflow)?;
            mul_div_down(p_coll, scale, p_loan)
        }
        core::cmp::Ordering::Less => {
            let lift = asset_unit(coll_decimals.wrapping_sub(loan_decimals))?;
            let den = p_loan.checked_mul(lift).ok_or(FixedError::Overflow)?;
            mul_div_down(p_coll, ORACLE_PRICE_SCALE, den)
        }
    }
}

pub(crate) fn terms(pos: PositionRef<'_>) -> Result<Terms<'_>> {
    let loan_row = pos
        .markets
        .get(usize::from(LOAN_SLOT))
        .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
            market: pos.key.market,
            slot: LOAN_SLOT,
        }))?;
    let coll_row = pos
        .markets
        .get(usize::from(COLL_SLOT))
        .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
            market: pos.key.market,
            slot: COLL_SLOT,
        }))?;
    let loan: &LoanRow = loan_row.body()?;
    if loan_row.flags.contains(MarketFlags::UNPRICED)
        || coll_row.flags.contains(MarketFlags::UNPRICED)
        || loan_row.asset == UNMAPPED_ASSET
        || coll_row.asset == UNMAPPED_ASSET
        || loan.flags & LoanRow::PRICED == 0
    {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    let (_, _, borrow_a, borrow_s) = accrued(loan, loan_row.last_update, pos.timestamp)?;
    let borrow_shares = U256::from(cell(pos.debt, LOAN_SLOT));
    let collateral = U256::from(cell(pos.supply, COLL_SLOT));
    let borrowed = if borrow_shares.is_zero() {
        U256::ZERO
    } else {
        to_assets_up(borrow_shares, borrow_a, borrow_s)?
    };
    Ok(Terms {
        loan,
        loan_row,
        coll_row,
        borrow_shares,
        collateral,
        borrow_a,
        borrow_s,
        borrowed,
        oracle: U256::ZERO,
        max_borrow: U256::ZERO,
        p_loan: U256::ZERO,
        p_coll: U256::ZERO,
    })
}

pub(crate) fn finish<'a>(t: &Terms<'a>, px: &PriceVector) -> Result<(Terms<'a>, Health)> {
    let p_loan = price_ray(px, t.loan_row.asset)?;
    let p_coll = price_ray(px, t.coll_row.asset)?;
    let oracle = oracle_price(p_coll, p_loan, t.loan.coll_decimals, t.loan_row.decimals)?;
    let quoted = mul_div_down(t.collateral, oracle, ORACLE_PRICE_SCALE)?;
    let max_borrow = w_mul_down(quoted, U256::from(t.loan.lltv))?;
    let mut t = *t;
    t.oracle = oracle;
    t.max_borrow = max_borrow;
    t.p_loan = p_loan;
    t.p_coll = p_coll;

    let hf_wad = if t.borrowed.is_zero() {
        U256::MAX
    } else {
        mul_div_down(max_borrow, WAD, t.borrowed)?
    };
    let hf = hf_wad_to_ray(hf_wad)?;
    let mut sensitivity = AssetMask::EMPTY;
    if t.collateral > U256::ZERO || t.borrow_shares > U256::ZERO {
        sensitivity = sensitivity
            .with(LOAN_SLOT)
            .and_then(|s| s.with(COLL_SLOT))
            .ok_or(ProtocolError::Internal)?;
    }
    let debt_value = Wad::from_raw(value_wad(t.borrowed, p_loan, t.loan_row.decimals)?);
    let collateral_value = Wad::from_raw(value_wad(t.collateral, p_coll, t.loan.coll_decimals)?);
    let state = if hf >= Ray::ONE {
        HealthState::Healthy
    } else if t.collateral.is_zero() {
        HealthState::BadDebt {
            deficit: debt_value,
        }
    } else if t.loan_row.flags.contains(MarketFlags::PAUSED) {
        HealthState::Blocked {
            reason: liq_protocol::BlockReason::Paused,
        }
    } else {
        HealthState::Liquidatable
    };
    Ok((
        t,
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
    let t = terms(pos)?;
    Ok(finish(&t, px)?.1)
}
