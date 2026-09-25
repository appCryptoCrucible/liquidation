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
pub fn oracle_price(
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

/// Prices whose `oracle_price` equals `price()` exactly.
///
/// A loan price of `10^27` cannot hit a `price()` that is not a multiple of
/// `10^9` (`floor(p_coll · 10^36 / 10^27) = p_coll · 10^9`). The loan price
/// is `ORACLE_PRICE_SCALE` (lifted when the loan has more decimals), so the
/// reconstruction matches `IOracle.price()` to the wei. These are ratio
/// prices, not USD: the book must not size from them.
pub fn prices_matching_oracle(
    price: U256,
    loan_decimals: u8,
    coll_decimals: u8,
) -> Result<(U256, U256)> {
    if price.is_zero() {
        return Err(ProtocolError::Internal);
    }
    let (p_loan, p_coll) = match loan_decimals.cmp(&coll_decimals) {
        core::cmp::Ordering::Equal => (ORACLE_PRICE_SCALE, price),
        core::cmp::Ordering::Greater => {
            let lift = asset_unit(loan_decimals.wrapping_sub(coll_decimals))?;
            (
                ORACLE_PRICE_SCALE
                    .checked_mul(lift)
                    .ok_or(FixedError::Overflow)?,
                price,
            )
        }
        core::cmp::Ordering::Less => {
            let lift = asset_unit(coll_decimals.wrapping_sub(loan_decimals))?;
            (
                ORACLE_PRICE_SCALE,
                price.checked_mul(lift).ok_or(FixedError::Overflow)?,
            )
        }
    };
    let back = oracle_price(p_coll, p_loan, coll_decimals, loan_decimals)?;
    if back != price {
        return Err(ProtocolError::Internal);
    }
    Ok((p_loan, p_coll))
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod hf_boundary {
    use super::{finish, Terms};
    use crate::layout::LoanRow;
    use alloy_primitives::U256;
    use bytemuck::Zeroable;
    use liq_protocol::{HealthState, MarketRow};
    use liq_types::fixed::WAD;
    use liq_types::{AssetId, Price, PriceVector, Ray, SourceKind};

    fn px() -> PriceVector {
        PriceVector(vec![
            Price {
                asset: AssetId(0),
                price: Ray::ONE,
                source: SourceKind::Canonical,
                block: 0,
                ts: 0,
            },
            Price {
                asset: AssetId(1),
                price: Ray::ONE,
                source: SourceKind::Canonical,
                block: 0,
                ts: 0,
            },
        ])
    }

    /// Equal prices, 18 decimals, LLTV = 1, borrowed == collateral ⇒ HF == 1.
    /// `hf > ONE` would mark this liquidatable. One wei past is liquidatable.
    #[test]
    fn hf_exactly_one_is_healthy() {
        let mut loan = LoanRow::zeroed();
        loan.lltv = 1_000_000_000_000_000_000;
        loan.coll_decimals = 18;
        let loan_row = MarketRow::blank(AssetId(0), 18);
        let coll_row = MarketRow::blank(AssetId(1), 18);
        let size = U256::from(1_000_000_000_000_000_000u128);
        let t = Terms {
            loan: &loan,
            loan_row: &loan_row,
            coll_row: &coll_row,
            borrow_shares: U256::ZERO,
            collateral: size,
            borrow_a: U256::ZERO,
            borrow_s: U256::ZERO,
            borrowed: size,
            oracle: U256::ZERO,
            max_borrow: U256::ZERO,
            p_loan: U256::ZERO,
            p_coll: U256::ZERO,
        };
        let prices = px();
        let (_, h) = finish(&t, &prices).unwrap();
        assert_eq!(h.hf, Ray::ONE);
        assert_eq!(h.hf.raw(), WAD * U256::from(1_000_000_000u64));
        assert!(matches!(h.state, HealthState::Healthy));

        let past = Terms {
            borrowed: size + U256::from(1u8),
            ..t
        };
        let (_, h2) = finish(&past, &prices).unwrap();
        assert!(matches!(h2.state, HealthState::Liquidatable));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
mod oracle_inverse {
    use super::{oracle_price, prices_matching_oracle};
    use crate::math::ORACLE_PRICE_SCALE;
    use alloy_primitives::U256;
    use liq_types::fixed::RAY;

    #[test]
    fn inverse_matches_price_that_is_not_a_multiple_of_1e9() {
        // 1e36 + 1 is not divisible by 1e9, so no integer collateral price
        // with the loan at 1 RAY reproduces it.
        let price = ORACLE_PRICE_SCALE + U256::from(1u8);
        let floored = price / U256::from(1_000_000_000u64);
        let wrong = oracle_price(floored, RAY, 18, 18).unwrap();
        assert_ne!(wrong, price);

        let (p_loan, p_coll) = prices_matching_oracle(price, 18, 18).unwrap();
        assert_eq!(p_loan, ORACLE_PRICE_SCALE);
        assert_eq!(p_coll, price);
        assert_eq!(oracle_price(p_coll, p_loan, 18, 18).unwrap(), price);
    }

    #[test]
    fn inverse_lifts_when_collateral_has_more_decimals() {
        // loan 6, coll 18: lift = 10^12. price = 1_000_000_001.
        // p_coll = 1_000_000_001 * 10^12 = 1_000_000_001_000_000_000_000.
        let price = U256::from(1_000_000_001u64);
        let (p_loan, p_coll) = prices_matching_oracle(price, 6, 18).unwrap();
        assert_eq!(p_loan, ORACLE_PRICE_SCALE);
        assert_eq!(p_coll, U256::from(1_000_000_001_000_000_000_000u128));
        assert_eq!(oracle_price(p_coll, p_loan, 18, 6).unwrap(), price);
    }

    #[test]
    fn zero_oracle_price_is_not_published() {
        assert!(prices_matching_oracle(U256::ZERO, 18, 18).is_err());
    }
}
