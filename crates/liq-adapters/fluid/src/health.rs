//! Health from the vault's own answer (`apply::apply_state_reads`), not a
//! reproduction of Fluid's tick math: a vault is liquidatable exactly when
//! its dead-address `liquidate` says it would liquidate something, as read
//! at the block the view is for (or the one before it).
//!
//! `hf` carries no price dependence — liquidatable is `RAY - 1`, anything
//! else [`Health::NO_DEBT_HF`] — so no threshold is registered on prices:
//! the engine refolds a vault when the per-block read changes it.

use alloy_primitives::U256;
use liq_protocol::{AssetMask, Health, HealthState, MarketRow, PositionRef, ProtocolError, Result};
use liq_types::fixed::{mul_div, Rounding, RAY};
use liq_types::{PriceVector, Ray, Wad};

use crate::layout::{VaultExtra, VaultRow, UNMAPPED_ASSET};

/// A read is current for its own block and the next (one mainnet slot):
/// the engine evaluates at the target block's time.
pub const FRESH_SECS: u64 = 12;

pub(crate) struct Terms<'a> {
    pub body: &'a VaultRow,
    pub fresh: bool,
}

pub(crate) fn terms<'a>(pos: &PositionRef<'a>) -> Result<Terms<'a>> {
    let head = pos.markets.first().ok_or(ProtocolError::Internal)?;
    let body: &VaultRow = head.body()?;
    let extra: &VaultExtra = pos.extra.view()?;
    let fresh = extra.read_ts != 0
        && pos.timestamp >= extra.read_ts
        && pos.timestamp.saturating_sub(extra.read_ts) <= FRESH_SECS;
    Ok(Terms { body, fresh })
}

#[inline]
pub(crate) fn cell(v: &[u128], slot: u16) -> u128 {
    v.get(usize::from(slot)).copied().unwrap_or(0)
}

/// `amount · price / 10^decimals`, RAY-scaled numeraire; `None` unpriced.
pub(crate) fn value(row: &MarketRow, amount: u128, px: &PriceVector) -> Option<U256> {
    if row.asset == UNMAPPED_ASSET || amount == 0 {
        return None;
    }
    let p =
        px.0.get(usize::from(row.asset.0))
            .filter(|p| p.asset == row.asset && !p.price.raw().is_zero())?;
    let unit = U256::from(10u8).checked_pow(U256::from(row.decimals))?;
    mul_div(U256::from(amount), p.price.raw(), unit, Rounding::Down).ok()
}

/// Largest priced value over `slots` — a smart side's tokens are
/// alternatives, so the side is worth its best one, never the sum.
fn side_value(
    pos: &PositionRef<'_>,
    cells: &[u128],
    slots: core::ops::Range<u16>,
    px: &PriceVector,
) -> U256 {
    slots
        .filter_map(|s| {
            let row = pos.markets.get(usize::from(s))?;
            value(row, cell(cells, s), px)
        })
        .max()
        .unwrap_or(U256::ZERO)
}

pub(crate) fn health(pos: PositionRef<'_>, px: &PriceVector) -> Result<Health> {
    let t = terms(&pos)?;
    let n_col = u16::from(t.body.n_col);
    let n_all = n_col.saturating_add(u16::from(t.body.n_debt));
    let has_debt = (n_col..n_all).any(|s| cell(pos.debt, s) != 0);
    let has_col = (0..n_col).any(|s| cell(pos.supply, s) != 0);
    if !t.fresh || !has_debt {
        return Ok(Health {
            hf: Health::NO_DEBT_HF,
            debt_value: Wad::from_raw(U256::ZERO),
            collateral_value: Wad::from_raw(U256::ZERO),
            price_sensitivity: AssetMask::EMPTY,
            state: HealthState::Healthy,
        });
    }
    let debt_value = Wad::from_raw(side_value(&pos, pos.debt, n_col..n_all, px));
    let collateral_value = Wad::from_raw(side_value(&pos, pos.supply, 0..n_col, px));
    let state = if has_col {
        HealthState::Liquidatable
    } else {
        HealthState::BadDebt {
            deficit: debt_value,
        }
    };
    Ok(Health {
        hf: Ray::from_raw(RAY.saturating_sub(U256::from(1u8))),
        debt_value,
        collateral_value,
        price_sensitivity: AssetMask::EMPTY,
        state,
    })
}
