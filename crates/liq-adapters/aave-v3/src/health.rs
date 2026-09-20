//! `GenericLogic.calculateUserAccountData` @ `8305565ae`.

use alloy_primitives::U256;
use liq_protocol::{
    AssetMask, BlockReason, Health, HealthState, MarketFlags, MarketRow, MarketSlot, PositionRef,
    ProtocolError, Result,
};
use liq_types::fixed::FixedError;
use liq_types::{AssetId, MarketId, PriceVector, Ray, Wad};

use crate::config::Config;
use crate::layout::{PoolMeta, Reserve, UserExtra, UserReserve, UNMAPPED_ASSET};
use crate::math::{
    a_token_balance, asset_unit, hf_wad_to_ray, mul_div_ceil, normalized_debt, normalized_income,
    p_of, v_token_balance, wad_div, BPS,
};

#[derive(Copy, Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct Collateral {
    pub assets: U256,
    pub per_price: U256,
    pub value: U256,
    pub lt: U256,
}

#[derive(Copy, Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct Debt {
    pub assets: U256,
    pub per_price: U256,
    pub value: U256,
}

#[derive(Copy, Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct SlotTerms<'a> {
    pub slot: u16,
    pub row: &'a MarketRow,
    pub reserve: &'a Reserve,
    pub user: &'a UserReserve,
    pub supply_scaled: u128,
    pub debt_scaled: u128,
    pub p: U256,
    pub liq_idx: U256,
    pub debt_idx: U256,
    pub collateral: Option<Collateral>,
    pub debt: Option<Debt>,
    pub liq_bonus: U256,
}

impl SlotTerms<'_> {
    #[inline]
    pub(crate) fn paused(&self) -> bool {
        self.reserve.flags & Reserve::PAUSED != 0
    }
    #[inline]
    pub(crate) fn seizable(&self) -> bool {
        self.collateral.is_some() && !self.paused()
    }
    #[inline]
    pub(crate) fn repayable(&self) -> bool {
        self.debt.is_some() && !self.paused()
    }
    #[inline]
    pub(crate) fn in_grace(&self, ts: u64) -> bool {
        u64::from(self.reserve.grace_until) > ts
    }
}

#[inline]
pub(crate) fn price_p(px: &PriceVector, asset: AssetId, scale: U256) -> Result<U256> {
    px.0.get(usize::from(asset.0))
        .filter(|p| p.asset == asset)
        .and_then(|p| p_of(p.price, scale))
        .ok_or(ProtocolError::MissingPrice(asset))
}

#[inline]
fn cell(v: &[u128], slot: u16) -> u128 {
    v.get(usize::from(slot)).copied().unwrap_or(0)
}

/// LTV / LT / bonus for a user-reserve under e-mode (`getUserReserveLtv` +
/// liquidation-threshold branch in `calculateUserAccountData`).
fn risk_params(meta: &PoolMeta, r: &Reserve, slot: u16, emode: u8) -> (u16, u16, u16) {
    let cat = meta.emode(emode);
    let bit = match emode {
        0 => None,
        id => cat.and_then(|c| {
            let i = meta.emode.iter().position(|e| e.id == id)?;
            let mask = 1u8.checked_shl(i as u32)?;
            Some((c, mask))
        }),
    };
    if let Some((c, mask)) = bit {
        let in_coll = r.emode_coll & mask != 0;
        if in_coll {
            let ltv = if r.emode_ltv0 & mask != 0 { 0 } else { c.ltv };
            return (ltv, c.liq_threshold, c.liq_bonus);
        }
        if c.isolated != 0 {
            return (0, r.liq_threshold, r.liq_bonus);
        }
    }
    let _ = slot;
    (r.ltv, r.liq_threshold, r.liq_bonus)
}

pub(crate) fn walk<'a, F, P>(pos: &PositionRef<'a>, mut price_of: P, mut f: F) -> Result<()>
where
    F: FnMut(&SlotTerms<'a>) -> Result<()>,
    P: FnMut(AssetId) -> Result<U256>,
{
    let meta: &PoolMeta = pos
        .markets
        .first()
        .ok_or(ProtocolError::UnknownMarket(pos.key.market))?
        .body()?;
    let extra: &UserExtra = pos.extra.view()?;
    for slot in pos.config.iter() {
        if slot == 0 {
            continue;
        }
        let row = pos
            .markets
            .get(usize::from(slot))
            .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
                market: pos.key.market,
                slot,
            }))?;
        let supply_scaled = cell(pos.supply, slot);
        let debt_scaled = cell(pos.debt, slot);
        let user: &UserReserve = match pos.slot_extra.get(usize::from(slot)) {
            Some(e) => e.view()?,
            None => &UserReserve::ZERO,
        };
        let reserve: &Reserve = row.body()?;
        let counted = user.flags & UserReserve::USING_AS_COLLATERAL != 0 && supply_scaled > 0;
        let borrowing = debt_scaled > 0;
        if !counted && !borrowing {
            continue;
        }
        if row.flags.contains(MarketFlags::UNPRICED)
            || row.asset == UNMAPPED_ASSET
            || reserve.flags & Reserve::PRICED == 0
        {
            return Err(ProtocolError::OracleSourceMismatch);
        }
        let p = price_of(row.asset)?;
        let unit = asset_unit(row.decimals)?;
        let liq_idx = normalized_income(reserve, row.last_update, pos.timestamp)?;
        let debt_idx = normalized_debt(reserve, row.last_update, pos.timestamp)?;
        let (_ltv, lt, bonus) = risk_params(meta, reserve, slot, extra.emode);

        let collateral = if counted && lt > 0 {
            let assets = a_token_balance(U256::from(supply_scaled), liq_idx)?;
            let value = assets
                .checked_mul(p)
                .ok_or(FixedError::Overflow)?
                .checked_div(unit)
                .ok_or(FixedError::DivisionByZero)?;
            Some(Collateral {
                assets,
                per_price: assets,
                value,
                lt: U256::from(lt),
            })
        } else if counted {
            let assets = a_token_balance(U256::from(supply_scaled), liq_idx)?;
            let value = assets
                .checked_mul(p)
                .ok_or(FixedError::Overflow)?
                .checked_div(unit)
                .ok_or(FixedError::DivisionByZero)?;
            Some(Collateral {
                assets,
                per_price: assets,
                value,
                lt: U256::ZERO,
            })
        } else {
            None
        };

        let debt = if borrowing {
            let assets = v_token_balance(U256::from(debt_scaled), debt_idx)?;
            let value = mul_div_ceil(assets, p, unit)?;
            Some(Debt {
                assets,
                per_price: assets,
                value,
            })
        } else {
            None
        };

        f(&SlotTerms {
            slot,
            row,
            reserve,
            user,
            supply_scaled,
            debt_scaled,
            p,
            liq_idx,
            debt_idx,
            collateral,
            debt,
            liq_bonus: U256::from(bonus),
        })?;
    }
    Ok(())
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct Account {
    pub market: MarketId,
    pub weighted: U256,
    pub collateral_value: U256,
    pub debt_value: U256,
    pub any_seizable: bool,
    pub any_repayable: bool,
    pub any_grace: bool,
    pub sentinel_ok: bool,
    pub oracle_decimals: u8,
    pub sensitivity: AssetMask,
}

impl Account {
    pub(crate) fn new(market: MarketId, sentinel_ok: bool, oracle_decimals: u8) -> Self {
        Self {
            market,
            weighted: U256::ZERO,
            collateral_value: U256::ZERO,
            debt_value: U256::ZERO,
            any_seizable: false,
            any_repayable: false,
            any_grace: false,
            sentinel_ok,
            oracle_decimals,
            sensitivity: AssetMask::EMPTY,
        }
    }

    pub(crate) fn add(&mut self, t: &SlotTerms<'_>, ts: u64) -> Result<()> {
        if let Some(c) = t.collateral {
            self.collateral_value = self
                .collateral_value
                .checked_add(c.value)
                .ok_or(FixedError::Overflow)?;
            let w = c.value.checked_mul(c.lt).ok_or(FixedError::Overflow)?;
            self.weighted = self.weighted.checked_add(w).ok_or(FixedError::Overflow)?;
            self.any_seizable |= t.seizable();
        }
        if let Some(d) = t.debt {
            self.debt_value = self
                .debt_value
                .checked_add(d.value)
                .ok_or(FixedError::Overflow)?;
            self.any_repayable |= t.repayable();
        }
        self.any_grace |= t.in_grace(ts);
        self.sensitivity = self
            .sensitivity
            .with(t.slot)
            .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
                market: self.market,
                slot: t.slot,
            }))?;
        Ok(())
    }

    /// `avgLiquidationThreshold.wadDiv(totalDebt) / 100_00`.
    pub(crate) fn hf_wad(&self) -> Result<U256> {
        if self.debt_value.is_zero() {
            return Ok(U256::MAX);
        }
        wad_div(self.weighted, self.debt_value)?
            .checked_div(BPS)
            .ok_or(ProtocolError::Fixed(FixedError::DivisionByZero))
    }
}

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

pub(crate) fn health_with(cfg: &Config, pos: PositionRef<'_>, px: &PriceVector) -> Result<Health> {
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
    walk(
        &pos,
        |asset| price_p(px, asset, scale),
        |t| acc.add(t, pos.timestamp),
    )?;
    finish(&acc)
}

pub(crate) fn finish(acc: &Account) -> Result<Health> {
    let hf = hf_wad_to_ray(acc.hf_wad()?)?;
    let lift = U256::from(10u8)
        .checked_pow(U256::from(
            18u8.checked_sub(acc.oracle_decimals)
                .ok_or(FixedError::Overflow)?,
        ))
        .ok_or(FixedError::Overflow)?;
    let debt_value = Wad::from_raw(
        acc.debt_value
            .checked_mul(lift)
            .ok_or(FixedError::Overflow)?,
    );
    let collateral_value = Wad::from_raw(
        acc.collateral_value
            .checked_mul(lift)
            .ok_or(FixedError::Overflow)?,
    );
    let state = if hf >= Ray::ONE {
        HealthState::Healthy
    } else if acc.collateral_value.is_zero() {
        HealthState::BadDebt {
            deficit: debt_value,
        }
    } else if !acc.sentinel_ok || acc.any_grace {
        HealthState::Blocked {
            reason: if !acc.sentinel_ok {
                BlockReason::Paused
            } else {
                BlockReason::GracePeriod
            },
        }
    } else if acc.any_seizable && acc.any_repayable {
        HealthState::Liquidatable
    } else {
        HealthState::Blocked {
            reason: BlockReason::Paused,
        }
    };
    Ok(Health {
        hf,
        debt_value,
        collateral_value,
        price_sensitivity: acc.sensitivity,
        state,
    })
}
