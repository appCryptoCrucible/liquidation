//! Euler `calculateLiquidity(..., liquidation=true)` @ `bfb325a6`.
//! Normalised HF is `collAdjValue / liabilityValue` (RAY). Liquidatable iff
//! `collAdjValue <= liabilityValue && liabilityValue != 0` (`Liquidation.sol`).

use alloy_primitives::U256;
use liq_protocol::{
    AssetMask, BlockReason, Health, HealthState, MarketFlags, MarketRow, MarketSlot, PositionRef,
    ProtocolError, Result,
};
use liq_types::fixed::{FixedError, WAD};
use liq_types::{AssetId, PriceVector, Wad};

use crate::layout::{CollRow, UserExtra, VaultRow, DEBT_SLOT, OP_LIQUIDATE, UNMAPPED_ASSET};
use crate::math::{
    accrue_accumulator, collateral_adj_value, current_liquidation_ltv, current_owed,
    discount_factor, hf_wad_to_ray, min_discount_factor, to_assets_up, value_wad, vault_acc,
};

#[derive(Copy, Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct Terms<'a> {
    pub vault: &'a VaultRow,
    pub debt_row: &'a MarketRow,
    pub owed_exact: U256,
    pub liability_assets: U256,
    pub coll_adj: U256,
    pub liability_value: U256,
    pub unweighted_coll: U256,
    pub min_df: U256,
    pub df: U256,
    pub acc: U256,
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

fn extra(pos: PositionRef<'_>) -> Result<&UserExtra> {
    pos.extra.view()
}

fn vault_row(pos: PositionRef<'_>) -> Result<(&MarketRow, &VaultRow)> {
    let row = pos
        .markets
        .get(usize::from(DEBT_SLOT))
        .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
            market: pos.key.market,
            slot: DEBT_SLOT,
        }))?;
    let v: &VaultRow = row.body()?;
    Ok((row, v))
}

pub(crate) fn terms(pos: PositionRef<'_>) -> Result<Terms<'_>> {
    let (debt_row, vault) = vault_row(pos)?;
    if debt_row.flags.contains(MarketFlags::UNPRICED)
        || debt_row.asset == UNMAPPED_ASSET
        || vault.flags & VaultRow::PRICED == 0
    {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    if vault.flags & VaultRow::DISCOUNT_KNOWN == 0
        || vault.flags & VaultRow::ACC_KNOWN == 0
        || vault.flags & VaultRow::HOOKS_KNOWN == 0
        || vault.flags & VaultRow::COOL_OFF_KNOWN == 0
    {
        return Err(ProtocolError::Internal);
    }
    let last = u64::from(debt_row.last_update);
    if pos.timestamp < last {
        return Err(ProtocolError::TimestampBeforeUpdate);
    }
    let stored_acc = vault_acc(vault);
    let delta = U256::from(
        pos.timestamp
            .checked_sub(last)
            .ok_or(FixedError::Underflow)?,
    );
    let (acc, _) = accrue_accumulator(
        stored_acc,
        U256::ZERO,
        U256::from(vault.interest_rate),
        delta,
    )?;
    let user = extra(pos)?;
    let owed_stored = U256::from(cell(pos.debt, DEBT_SLOT));
    let user_acc = crate::math::u256_from_limbs(user.user_accumulator_lo, user.user_accumulator_hi);
    let owed_exact = if owed_stored.is_zero() {
        U256::ZERO
    } else if user.flags & UserExtra::ACC_KNOWN == 0 {
        return Err(ProtocolError::Internal);
    } else {
        current_owed(owed_stored, acc, user_acc)?
    };
    let liability_assets = to_assets_up(owed_exact)?;
    Ok(Terms {
        vault,
        debt_row,
        owed_exact,
        liability_assets,
        coll_adj: U256::ZERO,
        liability_value: U256::ZERO,
        unweighted_coll: U256::ZERO,
        min_df: U256::ZERO,
        df: U256::ZERO,
        acc,
    })
}

pub(crate) fn finish<'a>(
    t: &Terms<'a>,
    pos: PositionRef<'a>,
    px: &PriceVector,
    over: PriceOverride,
) -> Result<(Terms<'a>, Health)> {
    let p_debt = price_ray(px, t.debt_row.asset, over)?;
    let liability_value = if t.liability_assets.is_zero() {
        U256::ZERO
    } else {
        value_wad(t.liability_assets, p_debt, t.debt_row.decimals)?
    };
    let user = extra(pos)?;
    let mut coll_adj = U256::ZERO;
    let mut unweighted = U256::ZERO;
    let mut sensitivity = AssetMask::EMPTY;
    if t.liability_assets > U256::ZERO || cell(pos.debt, DEBT_SLOT) != 0 {
        sensitivity = sensitivity.with(DEBT_SLOT).ok_or(ProtocolError::Internal)?;
    }
    for slot in pos.config.iter() {
        if slot == DEBT_SLOT {
            continue;
        }
        let row = pos
            .markets
            .get(usize::from(slot))
            .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
                market: pos.key.market,
                slot,
            }))?;
        let coll: &CollRow = row.body()?;
        if coll.flags & CollRow::RECOGNIZED == 0 {
            continue;
        }
        let bit = 1u128
            .checked_shl(u32::from(slot))
            .ok_or(FixedError::Overflow)?;
        if user.enabled_mask & bit == 0 {
            continue;
        }
        let shares = U256::from(cell(pos.supply, slot));
        if shares.is_zero() {
            continue;
        }
        if row.flags.contains(MarketFlags::UNPRICED) || row.asset == UNMAPPED_ASSET {
            return Err(ProtocolError::OracleSourceMismatch);
        }
        let ltv = current_liquidation_ltv(
            coll.liquidation_ltv,
            coll.initial_liquidation_ltv,
            coll.target_timestamp,
            coll.ramp_duration,
            pos.timestamp,
        );
        let p = price_ray(px, row.asset, over)?;
        let quoted = value_wad(shares, p, row.decimals)?;
        unweighted = unweighted.checked_add(quoted).ok_or(FixedError::Overflow)?;
        coll_adj = coll_adj
            .checked_add(collateral_adj_value(shares, p, row.decimals, ltv)?)
            .ok_or(FixedError::Overflow)?;
        sensitivity = sensitivity.with(slot).ok_or(ProtocolError::Internal)?;
    }

    let min_df = min_discount_factor(t.vault.max_liquidation_discount)?;
    let hf = if liability_value.is_zero() {
        Health::NO_DEBT_HF
    } else {
        hf_wad_to_ray(crate::math::mul_div_down(coll_adj, WAD, liability_value)?)?
    };
    let df = if liability_value.is_zero() {
        WAD
    } else {
        discount_factor(coll_adj, liability_value, min_df)?
    };

    let debt_value = Wad::from_raw(liability_value);
    let collateral_value = Wad::from_raw(unweighted);
    let liquidate_disabled = t.vault.hooked_ops & OP_LIQUIDATE != 0;
    let cool_off = u64::from(t.vault.liquidation_cool_off);
    let in_cool_off =
        cool_off > 0 && pos.timestamp < u64::from(user.last_status_check).saturating_add(cool_off);

    let state = if liability_value.is_zero() || coll_adj > liability_value {
        HealthState::Healthy
    } else if unweighted.is_zero() {
        HealthState::BadDebt {
            deficit: debt_value,
        }
    } else if t.debt_row.flags.contains(MarketFlags::PAUSED) || liquidate_disabled {
        HealthState::Blocked {
            reason: BlockReason::Paused,
        }
    } else if in_cool_off {
        HealthState::Blocked {
            reason: BlockReason::GracePeriod,
        }
    } else {
        HealthState::Liquidatable
    };

    let mut t = *t;
    t.coll_adj = coll_adj;
    t.liability_value = liability_value;
    t.unweighted_coll = unweighted;
    t.min_df = min_df;
    t.df = df;
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
    Ok(finish(&t, pos, px, None)?.1)
}
