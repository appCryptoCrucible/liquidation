//! `GenericLogic.calculateUserAccountData` @ `8305565ae`.

use alloy_primitives::U256;
use liq_protocol::{
    AssetMask, BlockReason, Health, HealthState, MarketFlags, MarketRow, MarketSlot, PositionRef,
    ProtocolError, Result,
};
use liq_types::fixed::FixedError;
use liq_types::{AssetId, MarketId, PriceVector, Ray, Wad};

use crate::config::Config;
use crate::layout::{
    emode_place, EModeCat, EModeRow, PoolMeta, Reserve, UserExtra, UserReserve, FIRST_RESERVE,
    UNMAPPED_ASSET,
};
use crate::math::{
    asset_unit, debt_assets, hf_wad_to_ray, mul_div_ceil, normalized_income, p_of, supply_assets,
    wad_div, BPS,
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
    /// Usable as the COLLATERAL side of a liquidation call at `ts`.
    ///
    /// A8. `LiquidationLogic._validateLiquidationCall` (pin 8305565ae) gates
    /// on the grace period of the two reserves named in *that call*:
    ///
    /// ```solidity
    /// require(
    ///   block.timestamp > collateralReserve.getLiquidationGracePeriod() &&
    ///   block.timestamp > debtReserve.getLiquidationGracePeriod(),
    ///   Errors.LIQUIDATION_GRACE_SENTINEL_CHECK_FAILED
    /// );
    /// ```
    ///
    /// It is not a position-wide veto. Treating it as one — the previous
    /// `any_grace` fold — made one freshly-unpaused reserve block every other
    /// collateral and debt on the position, for the whole grace window.
    #[inline]
    pub(crate) fn seizable(&self, ts: u64) -> bool {
        self.collateral.is_some() && !self.paused() && !self.in_grace(ts)
    }
    /// Usable as the DEBT side of a liquidation call at `ts`. Same gate.
    #[inline]
    pub(crate) fn repayable(&self, ts: u64) -> bool {
        self.debt.is_some() && !self.paused() && !self.in_grace(ts)
    }
    /// Ignores grace: "would be usable but for the grace window", so the
    /// health state can name grace as the reason rather than reporting a
    /// misleading `Paused`.
    #[inline]
    pub(crate) fn seizable_but_for_grace(&self, ts: u64) -> bool {
        self.collateral.is_some() && !self.paused() && self.in_grace(ts)
    }
    #[inline]
    pub(crate) fn repayable_but_for_grace(&self, ts: u64) -> bool {
        self.debt.is_some() && !self.paused() && self.in_grace(ts)
    }
    #[inline]
    /// Liquidation is refused while `gracePeriodUntil >= block.timestamp`:
    /// V3.1+ `ValidationLogic.validateLiquidationCall` requires `until <
    /// block.timestamp`, and V2's collateral manager (`0xcc963272…`) returns
    /// `ON_GRACE_PERIOD` when `until >= block.timestamp`. The last second is
    /// still inside the window.
    pub(crate) fn in_grace(&self, ts: u64) -> bool {
        self.reserve.grace_until != 0 && u64::from(self.reserve.grace_until) >= ts
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

/// The account's e-mode category `id`, as the pool configured it; `None`
/// when e-mode is off or the pool has no such category.
fn emode_cat(markets: &[MarketRow], market: MarketId, id: u8) -> Result<Option<&EModeCat>> {
    let Some((slot, i)) = emode_place(id) else {
        return Ok(None);
    };
    let row: &EModeRow = markets
        .get(usize::from(slot))
        .ok_or(ProtocolError::SlotOutOfRange(MarketSlot { market, slot }))?
        .body()?;
    Ok(row.cats.get(i).filter(|c| c.id == id))
}

/// LTV / LT / bonus for a user-reserve under e-mode (`getUserReserveLtv` +
/// liquidation-threshold branch in `calculateUserAccountData`), with `cat`
/// the account's category.
fn risk_params(cat: Option<&EModeCat>, r: &Reserve) -> (u16, u16, u16) {
    if let Some(c) = cat {
        if r.emode_coll.contains(c.id) {
            let ltv = if r.emode_ltv0.contains(c.id) {
                0
            } else {
                c.ltv
            };
            return (ltv, c.liq_threshold, c.liq_bonus);
        }
        if c.isolated != 0 {
            return (0, r.liq_threshold, r.liq_bonus);
        }
    }
    (r.ltv, r.liq_threshold, r.liq_bonus)
}

pub(crate) fn walk<'a, F, P>(
    lp: &crate::config::LiquidationParams,
    pos: &PositionRef<'a>,
    mut price_of: P,
    mut f: F,
) -> Result<()>
where
    F: FnMut(&SlotTerms<'a>) -> Result<()>,
    P: FnMut(AssetId) -> Result<U256>,
{
    if pos.markets.is_empty() {
        return Err(ProtocolError::UnknownMarket(pos.key.market));
    }
    let model = lp.balance_model;
    let version = lp.version;
    let extra: &UserExtra = pos.extra.view()?;
    if extra.stable_slots != 0 {
        return Err(ProtocolError::UntrackedDebt);
    }
    let cat = emode_cat(pos.markets, pos.key.market, extra.emode)?;
    for slot in pos.config.iter() {
        if slot < FIRST_RESERVE {
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
        let debt_idx =
            crate::math::normalized_debt_for(version, reserve, row.last_update, pos.timestamp)?;
        let (_ltv, lt, bonus) = risk_params(cat, reserve);

        let collateral = if counted && lt > 0 {
            let assets = supply_assets(model, U256::from(supply_scaled), liq_idx)?;
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
            let assets = supply_assets(model, U256::from(supply_scaled), liq_idx)?;
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
            let assets = debt_assets(model, U256::from(debt_scaled), debt_idx)?;
            // V2 floors the debt's value (`price.mul(balance).div(unit)`);
            // V3.5 rounds it up.
            let value = if version == crate::config::AaveVersion::V2 {
                assets
                    .checked_mul(p)
                    .ok_or(FixedError::Overflow)?
                    .checked_div(unit)
                    .ok_or(FixedError::DivisionByZero)?
            } else {
                mul_div_ceil(assets, p, unit)?
            };
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
    /// A collateral (resp. debt) that is only held back by its reserve's
    /// grace window. Used to attribute the block reason, never to veto.
    pub grace_seizable: bool,
    pub grace_repayable: bool,
    pub sentinel_ok: bool,
    pub oracle_decimals: u8,
    pub sensitivity: AssetMask,
    /// V2: `Σ value` of the collateral with a non-zero threshold (what V2
    /// averages its threshold over), and the version.
    pub lt_collateral: U256,
    pub version: crate::config::AaveVersion,
}

impl Account {
    pub(crate) fn new(
        market: MarketId,
        sentinel_ok: bool,
        oracle_decimals: u8,
        version: crate::config::AaveVersion,
    ) -> Self {
        Self {
            market,
            weighted: U256::ZERO,
            collateral_value: U256::ZERO,
            debt_value: U256::ZERO,
            any_seizable: false,
            any_repayable: false,
            grace_seizable: false,
            grace_repayable: false,
            sentinel_ok,
            oracle_decimals,
            sensitivity: AssetMask::EMPTY,
            lt_collateral: U256::ZERO,
            version,
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
            if !c.lt.is_zero() {
                self.lt_collateral = self
                    .lt_collateral
                    .checked_add(c.value)
                    .ok_or(FixedError::Overflow)?;
            }
            self.any_seizable |= t.seizable(ts);
            self.grace_seizable |= t.seizable_but_for_grace(ts);
        }
        if let Some(d) = t.debt {
            self.debt_value = self
                .debt_value
                .checked_add(d.value)
                .ok_or(FixedError::Overflow)?;
            self.any_repayable |= t.repayable(ts);
            self.grace_repayable |= t.repayable_but_for_grace(ts);
        }
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
        if self.version == crate::config::AaveVersion::V2 {
            return crate::math::hf_wad_v2(self.weighted, self.lt_collateral, self.debt_value);
        }
        if self.debt_value.is_zero() {
            return Ok(U256::MAX);
        }
        wad_div(self.weighted, self.debt_value)?
            .checked_div(BPS)
            .ok_or(ProtocolError::Fixed(FixedError::DivisionByZero))
    }
}

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
        cfg.liquidation.version,
    );
    walk(
        &cfg.liquidation,
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
    } else if !acc.sentinel_ok {
        HealthState::Blocked {
            reason: BlockReason::Paused,
        }
    } else if acc.any_seizable && acc.any_repayable {
        // At least one (collateral, debt) pair clears both grace gates, so
        // the pool will accept a call — even if some OTHER reserve on this
        // position is still inside its window.
        HealthState::Liquidatable
    } else {
        // Nothing is callable. Name grace as the reason only when lifting the
        // grace windows would actually make a pair available; otherwise the
        // position is short a side for the ordinary paused/absent reason.
        let grace_would_unblock = (acc.any_seizable || acc.grace_seizable)
            && (acc.any_repayable || acc.grace_repayable)
            && (acc.grace_seizable || acc.grace_repayable);
        HealthState::Blocked {
            reason: if grace_would_unblock {
                BlockReason::GracePeriod
            } else {
                BlockReason::Paused
            },
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
