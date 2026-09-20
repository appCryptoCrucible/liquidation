//! `Protocol::apply_log`: one routed log → state, through `StateWriter`
//! only (journal-before-write is the writer's contract; this module never
//! holds a mutable reference into the store).
//!
//! **Hub logs** mutate the hub row and fan the [`HubAsset`] copy out to
//! every spoke reserve row with `(hub_market, hub_slot)` pointing at it;
//! the reported rows are those reserve rows (`MarketAccrual`). `UpdateAsset`
//! is the canonical index/rate/fee fold — every mutating Hub call emits it
//! after `accrue()` — and the per-operation logs carry the disjoint share
//! and liquidity deltas, so folding them in log order is exact regardless
//! of which comes first inside the transaction.
//!
//! **Spoke logs** mutate positions (`Positions`), the reserve rows
//! (`MarketReprice`) or the slot-0 [`SpokeMeta`] row.
//!
//! Unknown = error: a log for a hub asset or reserve this store has not
//! listed is `UnknownMarket`/`SlotOutOfRange`, never a lazily created row.

use alloy_primitives::{Address, I256, U256};
use alloy_sol_types::SolEvent;
use liq_protocol::{
    DecodedLog, DirtyPositions, DirtyRows, DirtySet, MarketFlags, MarketRow, MarketSlot,
    ProtocolError, Result, StateWriter,
};
use liq_types::fixed::FixedError;
use liq_types::{MarketId, PositionId, PositionKey};
use smallvec::SmallVec;

use crate::config::{Config, Emitter};
use crate::events::{halt, hub, oracle, spoke};
use crate::layout::{
    HubAsset, HubRow, Reserve, ReserveCfg, SpokeFlags, SpokeMeta, UserExtra, UserReserve,
    META_ASSET, UNMAPPED_ASSET,
};
use crate::math::{add_signed, join, signed, split, BPS, HF_THRESHOLD_WAD};

// ---------------------------------------------------------------------------
// Decoding helpers
// ---------------------------------------------------------------------------

#[inline]
fn decode<E: SolEvent>(log: &DecodedLog<'_>) -> Result<E> {
    E::decode_raw_log(log.topics.iter().copied(), log.data).map_err(|_| ProtocolError::MalformedLog)
}

/// A chain `uint256` that the contract stores in a narrower field; the log
/// is malformed when it does not fit.
#[inline]
fn narrow<T: TryFrom<U256>>(v: U256) -> Result<T> {
    T::try_from(v).map_err(|_| ProtocolError::MalformedLog)
}

/// `reserveId` → store slot (`reserveId + 1`; slot 0 is the meta row).
#[inline]
fn reserve_slot(reserve_id: U256) -> Result<u16> {
    let r: u16 = narrow(reserve_id)?;
    r.checked_add(1).ok_or(ProtocolError::MalformedLog)
}

/// Block time as the row's `u32` `last_update`.
#[inline]
fn last_update(ts: u64) -> Result<u32> {
    u32::try_from(ts).map_err(|_| ProtocolError::Fixed(FixedError::Overflow))
}

/// `a + delta` for a `uint120` share count (`MathUtils.add`).
#[inline]
fn add_shares(a: u128, delta: I256) -> Result<u128> {
    narrow(add_signed(U256::from(a), delta)?)
}

/// `offset + delta` for an `int200` offset.
#[inline]
fn add_offset(lo: u128, hi: u128, delta: I256) -> Result<(u128, u128)> {
    let sum = signed(lo, hi)
        .checked_add(delta)
        .ok_or(FixedError::Overflow)?;
    Ok(split(sum.into_raw()))
}

#[inline]
fn checked_add(a: u128, b: U256) -> Result<u128> {
    narrow(U256::from(a).checked_add(b).ok_or(FixedError::Overflow)?)
}

#[inline]
fn checked_sub(a: u128, b: U256) -> Result<u128> {
    narrow(U256::from(a).checked_sub(b).ok_or(FixedError::Underflow)?)
}

/// The neutral header flags a reserve row derives from its body.
#[inline]
fn derive_flags(cfg: &ReserveCfg, priced: bool) -> MarketFlags {
    let mut f = 0u8;
    if cfg.flags & ReserveCfg::PAUSED != 0 {
        f |= MarketFlags::PAUSED.0;
    }
    if cfg.flags & ReserveCfg::FROZEN != 0 {
        f |= MarketFlags::FROZEN.0;
    }
    if !priced {
        f |= MarketFlags::UNPRICED.0;
    }
    MarketFlags(f)
}

#[inline]
fn positions(ids: &[PositionId]) -> DirtySet {
    DirtySet::Positions(DirtyPositions::from_slice(ids))
}

#[inline]
fn rows(market: MarketId, slots: &[u16]) -> DirtyRows {
    slots
        .iter()
        .map(|&slot| MarketSlot { market, slot })
        .collect()
}

/// Rows of `market`, or none when the market has no row yet.
fn rows_or_empty(st: &dyn StateWriter, market: MarketId) -> Result<&[MarketRow]> {
    match st.markets(market) {
        Ok(r) => Ok(r),
        Err(ProtocolError::UnknownMarket(_)) => Ok(&[]),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// `Protocol::apply_log` body.
pub(crate) fn apply_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let topic0 = *log.topics.first().ok_or(ProtocolError::MalformedLog)?;
    let emitter = cfg
        .emitter(log.address)
        .ok_or(ProtocolError::UnexpectedLog)?;
    // Halt-class logs first: they are the same on every proxy.
    if matches!(
        topic0,
        halt::Upgraded::SIGNATURE_HASH
            | halt::AdminChanged::SIGNATURE_HASH
            | halt::Initialized::SIGNATURE_HASH
            | halt::AuthorityUpdated::SIGNATURE_HASH
            | spoke::SetSpokeImmutables::SIGNATURE_HASH
            | oracle::SetSpoke::SIGNATURE_HASH
    ) {
        return if log.block <= cfg.pinned_through {
            Ok(DirtySet::None)
        } else {
            Err(ProtocolError::HaltSignal)
        };
    }
    match emitter {
        Emitter::Hub(market) => apply_hub(cfg, st, market, topic0, log),
        Emitter::Spoke(market, idx) => apply_spoke(cfg, st, market, idx, topic0, log),
        Emitter::Oracle(idx) => {
            if topic0 == oracle::UpdateReserveSource::SIGNATURE_HASH {
                let ev: oracle::UpdateReserveSource = decode(log)?;
                let s = cfg.spokes.get(idx).ok_or(ProtocolError::UnexpectedLog)?;
                set_source(cfg, st, s.market, s.address, ev.reserveId, ev.source)
            } else {
                Err(ProtocolError::UnexpectedLog)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Hub
// ---------------------------------------------------------------------------

/// Read-modify-write the hub row of `(market, slot)` and fan the asset copy
/// out to every spoke reserve row that denormalises it. `ts` is the new
/// `last_update` when the log carries one (`UpdateAsset`).
fn hub_update(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    slot: u16,
    ts: Option<u32>,
    f: impl FnOnce(&mut HubAsset) -> Result<()>,
) -> Result<DirtyRows> {
    let at = MarketSlot { market, slot };
    let mut row = *st.market(at)?;
    f(&mut row.body_mut::<HubRow>()?.asset)?;
    if let Some(t) = ts {
        row.last_update = t;
    }
    let asset = row.body::<HubRow>()?.asset;
    let last = row.last_update;
    st.set_market(at, row)?;
    fan_out(cfg, st, market, slot, |r| {
        r.body_mut::<Reserve>()?.hub = asset;
        r.last_update = last;
        Ok(())
    })
}

/// Apply `f` to every spoke reserve row pointing at hub `(market, slot)`;
/// returns those rows.
fn fan_out(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    slot: u16,
    mut f: impl FnMut(&mut MarketRow) -> Result<()>,
) -> Result<DirtyRows> {
    let mut touched: DirtyRows = DirtyRows::new();
    for s in &cfg.spokes {
        let rows = rows_or_empty(st, s.market)?;
        for (i, r) in rows.iter().enumerate() {
            if r.hub_market == market.0 && r.hub_slot == slot {
                touched.push(MarketSlot {
                    market: s.market,
                    slot: u16::try_from(i).map_err(|_| ProtocolError::MalformedLog)?,
                });
            }
        }
    }
    for at in &touched {
        let mut row = *st.market(*at)?;
        f(&mut row)?;
        st.set_market(*at, row)?;
    }
    Ok(touched)
}

fn premium_delta_hub(a: &mut HubAsset, shares: I256, offset: I256) -> Result<()> {
    a.premium_shares = add_shares(a.premium_shares, shares)?;
    let (lo, hi) = add_offset(a.premium_offset_lo, a.premium_offset_hi, offset)?;
    a.premium_offset_lo = lo;
    a.premium_offset_hi = hi;
    Ok(())
}

fn apply_hub(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    topic0: alloy_primitives::B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let accrual = |rows: DirtyRows| {
        if rows.is_empty() {
            DirtySet::None
        } else {
            DirtySet::MarketAccrual(rows)
        }
    };
    match topic0 {
        hub::UpdateAsset::SIGNATURE_HASH => {
            let ev: hub::UpdateAsset = decode(log)?;
            let slot: u16 = narrow(ev.assetId)?;
            let ts = last_update(log.timestamp)?;
            let rows = hub_update(cfg, st, market, slot, Some(ts), |a| {
                a.drawn_index = narrow(ev.drawnIndex)?;
                a.drawn_rate = narrow(ev.drawnRate)?;
                a.realized_fees = narrow(ev.accruedFees)?;
                Ok(())
            })?;
            Ok(accrual(rows))
        }
        hub::Add::SIGNATURE_HASH => {
            let ev: hub::Add = decode(log)?;
            let rows = hub_update(cfg, st, market, narrow(ev.assetId)?, None, |a| {
                a.liquidity = checked_add(a.liquidity, ev.amount)?;
                a.added_shares = checked_add(a.added_shares, ev.shares)?;
                Ok(())
            })?;
            Ok(accrual(rows))
        }
        hub::Remove::SIGNATURE_HASH => {
            let ev: hub::Remove = decode(log)?;
            let rows = hub_update(cfg, st, market, narrow(ev.assetId)?, None, |a| {
                a.liquidity = checked_sub(a.liquidity, ev.amount)?;
                a.added_shares = checked_sub(a.added_shares, ev.shares)?;
                Ok(())
            })?;
            Ok(accrual(rows))
        }
        hub::Draw::SIGNATURE_HASH => {
            let ev: hub::Draw = decode(log)?;
            let rows = hub_update(cfg, st, market, narrow(ev.assetId)?, None, |a| {
                a.drawn_shares = checked_add(a.drawn_shares, ev.drawnShares)?;
                a.liquidity = checked_sub(a.liquidity, ev.drawnAmount)?;
                Ok(())
            })?;
            Ok(accrual(rows))
        }
        hub::Restore::SIGNATURE_HASH => {
            let ev: hub::Restore = decode(log)?;
            let rows = hub_update(cfg, st, market, narrow(ev.assetId)?, None, |a| {
                a.drawn_shares = checked_sub(a.drawn_shares, ev.drawnShares)?;
                premium_delta_hub(
                    a,
                    ev.premiumDelta.sharesDelta,
                    ev.premiumDelta.offsetRayDelta,
                )?;
                let inflow = ev
                    .drawnAmount
                    .checked_add(ev.premiumAmount)
                    .ok_or(FixedError::Overflow)?;
                a.liquidity = checked_add(a.liquidity, inflow)?;
                Ok(())
            })?;
            Ok(accrual(rows))
        }
        hub::RefreshPremium::SIGNATURE_HASH => {
            let ev: hub::RefreshPremium = decode(log)?;
            let rows = hub_update(cfg, st, market, narrow(ev.assetId)?, None, |a| {
                premium_delta_hub(
                    a,
                    ev.premiumDelta.sharesDelta,
                    ev.premiumDelta.offsetRayDelta,
                )
            })?;
            Ok(accrual(rows))
        }
        hub::ReportDeficit::SIGNATURE_HASH => {
            let ev: hub::ReportDeficit = decode(log)?;
            let rows = hub_update(cfg, st, market, narrow(ev.assetId)?, None, |a| {
                a.drawn_shares = checked_sub(a.drawn_shares, ev.drawnShares)?;
                premium_delta_hub(
                    a,
                    ev.premiumDelta.sharesDelta,
                    ev.premiumDelta.offsetRayDelta,
                )?;
                let d = join(a.deficit_ray_lo, a.deficit_ray_hi)
                    .checked_add(ev.deficitAmountRay)
                    .ok_or(FixedError::Overflow)?;
                (a.deficit_ray_lo, a.deficit_ray_hi) = split(d);
                Ok(())
            })?;
            Ok(accrual(rows))
        }
        hub::EliminateDeficit::SIGNATURE_HASH => {
            let ev: hub::EliminateDeficit = decode(log)?;
            let rows = hub_update(cfg, st, market, narrow(ev.assetId)?, None, |a| {
                a.added_shares = checked_sub(a.added_shares, ev.shares)?;
                let d = join(a.deficit_ray_lo, a.deficit_ray_hi)
                    .checked_sub(ev.deficitAmountRay)
                    .ok_or(FixedError::Underflow)?;
                (a.deficit_ray_lo, a.deficit_ray_hi) = split(d);
                Ok(())
            })?;
            Ok(accrual(rows))
        }
        hub::Sweep::SIGNATURE_HASH => {
            let ev: hub::Sweep = decode(log)?;
            let rows = hub_update(cfg, st, market, narrow(ev.assetId)?, None, |a| {
                a.liquidity = checked_sub(a.liquidity, ev.amount)?;
                a.swept = checked_add(a.swept, ev.amount)?;
                Ok(())
            })?;
            Ok(accrual(rows))
        }
        hub::Reclaim::SIGNATURE_HASH => {
            let ev: hub::Reclaim = decode(log)?;
            let rows = hub_update(cfg, st, market, narrow(ev.assetId)?, None, |a| {
                a.liquidity = checked_add(a.liquidity, ev.amount)?;
                a.swept = checked_sub(a.swept, ev.amount)?;
                Ok(())
            })?;
            Ok(accrual(rows))
        }
        hub::MintFeeShares::SIGNATURE_HASH => {
            // `realizedFees` is reset to zero on-chain here; the absolute
            // value arrives in the `UpdateAsset` of the same call.
            let ev: hub::MintFeeShares = decode(log)?;
            let rows = hub_update(cfg, st, market, narrow(ev.assetId)?, None, |a| {
                a.added_shares = checked_add(a.added_shares, ev.shares)?;
                Ok(())
            })?;
            Ok(accrual(rows))
        }
        hub::TransferShares::SIGNATURE_HASH => {
            // Spoke-level `addedShares` move; the asset totals `health()`
            // reads are unchanged.
            let _: hub::TransferShares = decode(log)?;
            Ok(DirtySet::None)
        }
        hub::UpdateAssetConfig::SIGNATURE_HASH => {
            let ev: hub::UpdateAssetConfig = decode(log)?;
            let rows = hub_update(cfg, st, market, narrow(ev.assetId)?, None, |a| {
                a.liquidity_fee = ev.config.liquidityFee;
                Ok(())
            })?;
            Ok(if rows.is_empty() {
                DirtySet::None
            } else {
                DirtySet::MarketReprice(rows)
            })
        }
        hub::AddAsset::SIGNATURE_HASH => {
            let ev: hub::AddAsset = decode(log)?;
            let slot: u16 = narrow(ev.assetId)?;
            let have = u16::try_from(rows_or_empty(st, market)?.len())
                .map_err(|_| ProtocolError::MalformedLog)?;
            if have != slot {
                return Err(ProtocolError::SlotMismatch {
                    expected: have,
                    got: slot,
                });
            }
            let mut row = match cfg.asset_by_underlying(ev.underlying) {
                Some(a) => {
                    let mut r = MarketRow::blank(a.asset, ev.decimals);
                    r.price_feed = a.feed;
                    r
                }
                None => {
                    let mut r = MarketRow::blank(UNMAPPED_ASSET, ev.decimals);
                    r.flags = MarketFlags::UNPRICED;
                    r
                }
            };
            *row.body_mut::<HubRow>()? = bytemuck::Zeroable::zeroed();
            st.push_market(market, row)?;
            Ok(DirtySet::None)
        }
        hub::AddSpoke::SIGNATURE_HASH => {
            let _: hub::AddSpoke = decode(log)?;
            Ok(DirtySet::None)
        }
        hub::UpdateSpokeConfig::SIGNATURE_HASH => {
            let ev: hub::UpdateSpokeConfig = decode(log)?;
            let Some(idx) = cfg.spoke_index(ev.spoke) else {
                return Ok(DirtySet::None);
            };
            let slot: u16 = narrow(ev.assetId)?;
            let mut bits = 0u8;
            if ev.config.active {
                bits |= SpokeFlags::ACTIVE;
            }
            if ev.config.halted {
                bits |= SpokeFlags::HALTED;
            }
            let at = MarketSlot { market, slot };
            let mut row = *st.market(at)?;
            *row.body_mut::<HubRow>()?
                .spokes
                .0
                .get_mut(idx)
                .ok_or(ProtocolError::UnexpectedLog)? = bits;
            st.set_market(at, row)?;
            // Only this spoke's reserve row denormalises these bits.
            let spoke_market = cfg
                .spokes
                .get(idx)
                .ok_or(ProtocolError::UnexpectedLog)?
                .market;
            let mut touched = DirtyRows::new();
            let rows = rows_or_empty(st, spoke_market)?;
            for (i, r) in rows.iter().enumerate() {
                if r.hub_market == market.0 && r.hub_slot == slot {
                    touched.push(MarketSlot {
                        market: spoke_market,
                        slot: u16::try_from(i).map_err(|_| ProtocolError::MalformedLog)?,
                    });
                }
            }
            for at in &touched {
                let mut row = *st.market(*at)?;
                let c = &mut row.body_mut::<Reserve>()?.cfg;
                c.flags &= !(ReserveCfg::SPOKE_ACTIVE | ReserveCfg::SPOKE_HALTED);
                if ev.config.active {
                    c.flags |= ReserveCfg::SPOKE_ACTIVE;
                }
                if ev.config.halted {
                    c.flags |= ReserveCfg::SPOKE_HALTED;
                }
                st.set_market(*at, row)?;
            }
            Ok(if touched.is_empty() {
                DirtySet::None
            } else {
                DirtySet::MarketReprice(touched)
            })
        }
        _ => Err(ProtocolError::UnexpectedLog),
    }
}

// ---------------------------------------------------------------------------
// Spoke
// ---------------------------------------------------------------------------

/// Slot 0 of a spoke market exists before any reserve: create it on first
/// touch. `UpdateLiquidationConfig` and `UpdateReserveSource` legitimately
/// precede `AddReserve`.
fn ensure_meta(st: &mut dyn StateWriter, market: MarketId) -> Result<()> {
    if rows_or_empty(st, market)?.is_empty() {
        let mut row = MarketRow::blank(META_ASSET, 0);
        row.flags = MarketFlags::UNPRICED;
        st.push_market(market, row)?;
    }
    Ok(())
}

fn meta(st: &dyn StateWriter, market: MarketId) -> Result<SpokeMeta> {
    Ok(*st
        .market(MarketSlot { market, slot: 0 })?
        .body::<SpokeMeta>()?)
}

fn set_meta(st: &mut dyn StateWriter, market: MarketId, m: SpokeMeta) -> Result<()> {
    let at = MarketSlot { market, slot: 0 };
    let mut row = *st.market(at)?;
    *row.body_mut::<SpokeMeta>()? = m;
    st.set_market(at, row)
}

/// Read-modify-write one reserve row.
fn reserve_update(
    st: &mut dyn StateWriter,
    at: MarketSlot,
    f: impl FnOnce(&mut MarketRow) -> Result<()>,
) -> Result<()> {
    let mut row = *st.market(at)?;
    f(&mut row)?;
    st.set_market(at, row)
}

/// Both `Spoke.UpdateReservePriceSource` and `AaveOracle.UpdateReserveSource`
/// (06A-1): priced iff `source` is the registry's pin for this reserve.
fn set_source(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    spoke: Address,
    reserve_id: U256,
    source: Address,
) -> Result<DirtySet> {
    ensure_meta(st, market)?;
    let r: u16 = narrow(reserve_id)?;
    let bit = 1u128
        .checked_shl(u32::from(r))
        .ok_or(ProtocolError::MalformedLog)?;
    let priced = cfg.pinned_source(spoke, r) == Some(source);
    let mut m = meta(st, market)?;
    if priced {
        m.priced |= bit;
    } else {
        m.priced &= !bit;
    }
    set_meta(st, market, m)?;
    let slot = reserve_slot(reserve_id)?;
    let at = MarketSlot { market, slot };
    if usize::from(slot) >= rows_or_empty(st, market)?.len() {
        // Listing in progress: `AddReserve` reads the bit.
        return Ok(DirtySet::None);
    }
    reserve_update(st, at, |row| {
        let unmapped = row.asset == UNMAPPED_ASSET;
        row.flags = derive_flags(&row.body::<Reserve>()?.cfg, priced && !unmapped);
        Ok(())
    })?;
    Ok(DirtySet::MarketReprice(rows(market, &[slot])))
}

/// Snapshot the reserve's current dynamic config into one user-reserve pair
/// (`userPosition.dynamicConfigKey = reserve.dynamicConfigKey`).
fn snapshot(st: &mut dyn StateWriter, market: MarketId, pos: PositionId, slot: u16) -> Result<()> {
    let cfg = st
        .market(MarketSlot { market, slot })?
        .body::<Reserve>()?
        .cfg;
    let mut e = *st.slot_extra(pos, slot)?;
    {
        let u = e.view_mut::<UserReserve>()?;
        u.collateral_factor = cfg.collateral_factor;
        u.liquidation_fee = cfg.liquidation_fee;
        u.max_liquidation_bonus = cfg.max_liquidation_bonus;
        u.dyn_key = cfg.dyn_key;
    }
    st.set_slot_extra(pos, slot, e)
}

fn user_reserve_update(
    st: &mut dyn StateWriter,
    pos: PositionId,
    slot: u16,
    f: impl FnOnce(&mut UserReserve) -> Result<()>,
) -> Result<()> {
    let mut e = *st.slot_extra(pos, slot)?;
    f(e.view_mut::<UserReserve>()?)?;
    st.set_slot_extra(pos, slot, e)
}

fn premium_delta_user(u: &mut UserReserve, shares: I256, offset: I256) -> Result<()> {
    u.premium_shares = add_shares(u.premium_shares, shares)?;
    let (lo, hi) = add_offset(u.premium_offset_lo, u.premium_offset_hi, offset)?;
    u.premium_offset_lo = lo;
    u.premium_offset_hi = hi;
    Ok(())
}

fn apply_spoke(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    idx: usize,
    topic0: alloy_primitives::B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let key = |user: Address| PositionKey {
        protocol: cfg.protocol,
        market,
        user,
    };
    match topic0 {
        // ---- positions --------------------------------------------------
        spoke::Supply::SIGNATURE_HASH => {
            let ev: spoke::Supply = decode(log)?;
            let slot = reserve_slot(ev.reserveId)?;
            let pos = st.intern(&key(ev.user))?;
            let s = checked_add(st.supply(pos, slot)?, ev.suppliedShares)?;
            st.set_supply(pos, slot, s)?;
            Ok(positions(&[pos]))
        }
        spoke::Withdraw::SIGNATURE_HASH => {
            let ev: spoke::Withdraw = decode(log)?;
            let slot = reserve_slot(ev.reserveId)?;
            let pos = st.intern(&key(ev.user))?;
            let s = checked_sub(st.supply(pos, slot)?, ev.withdrawnShares)?;
            st.set_supply(pos, slot, s)?;
            Ok(positions(&[pos]))
        }
        spoke::Borrow::SIGNATURE_HASH => {
            let ev: spoke::Borrow = decode(log)?;
            let slot = reserve_slot(ev.reserveId)?;
            let pos = st.intern(&key(ev.user))?;
            let d = checked_add(st.debt(pos, slot)?, ev.drawnShares)?;
            st.set_debt(pos, slot, d)?;
            Ok(positions(&[pos]))
        }
        spoke::Repay::SIGNATURE_HASH => {
            let ev: spoke::Repay = decode(log)?;
            let slot = reserve_slot(ev.reserveId)?;
            let pos = st.intern(&key(ev.user))?;
            let d = checked_sub(st.debt(pos, slot)?, ev.drawnShares)?;
            st.set_debt(pos, slot, d)?;
            user_reserve_update(st, pos, slot, |u| {
                premium_delta_user(
                    u,
                    ev.premiumDelta.sharesDelta,
                    ev.premiumDelta.offsetRayDelta,
                )
            })?;
            Ok(positions(&[pos]))
        }
        spoke::LiquidationCall::SIGNATURE_HASH => {
            let ev: spoke::LiquidationCall = decode(log)?;
            let c = reserve_slot(ev.collateralReserveId)?;
            let d = reserve_slot(ev.debtReserveId)?;
            let pos = st.intern(&key(ev.user))?;
            let s = checked_sub(st.supply(pos, c)?, ev.collateralSharesLiquidated)?;
            st.set_supply(pos, c, s)?;
            let dd = checked_sub(st.debt(pos, d)?, ev.drawnSharesLiquidated)?;
            st.set_debt(pos, d, dd)?;
            user_reserve_update(st, pos, d, |u| {
                premium_delta_user(
                    u,
                    ev.premiumDelta.sharesDelta,
                    ev.premiumDelta.offsetRayDelta,
                )
            })?;
            if ev.receiveShares && !ev.collateralSharesToLiquidator.is_zero() {
                let liq = st.intern(&key(ev.liquidator))?;
                let s = checked_add(st.supply(liq, c)?, ev.collateralSharesToLiquidator)?;
                st.set_supply(liq, c, s)?;
                return Ok(positions(&[pos, liq]));
            }
            Ok(positions(&[pos]))
        }
        spoke::ReportDeficit::SIGNATURE_HASH => {
            let ev: spoke::ReportDeficit = decode(log)?;
            let slot = reserve_slot(ev.reserveId)?;
            let pos = st.intern(&key(ev.user))?;
            let d = checked_sub(st.debt(pos, slot)?, ev.drawnShares)?;
            st.set_debt(pos, slot, d)?;
            user_reserve_update(st, pos, slot, |u| {
                premium_delta_user(
                    u,
                    ev.premiumDelta.sharesDelta,
                    ev.premiumDelta.offsetRayDelta,
                )
            })?;
            Ok(positions(&[pos]))
        }
        spoke::RefreshPremiumDebt::SIGNATURE_HASH => {
            let ev: spoke::RefreshPremiumDebt = decode(log)?;
            let slot = reserve_slot(ev.reserveId)?;
            let pos = st.intern(&key(ev.user))?;
            user_reserve_update(st, pos, slot, |u| {
                premium_delta_user(
                    u,
                    ev.premiumDelta.sharesDelta,
                    ev.premiumDelta.offsetRayDelta,
                )
            })?;
            Ok(positions(&[pos]))
        }
        spoke::SetUsingAsCollateral::SIGNATURE_HASH => {
            let ev: spoke::SetUsingAsCollateral = decode(log)?;
            let slot = reserve_slot(ev.reserveId)?;
            let pos = st.intern(&key(ev.user))?;
            user_reserve_update(st, pos, slot, |u| {
                if ev.usingAsCollateral {
                    u.flags |= UserReserve::USING_AS_COLLATERAL;
                } else {
                    u.flags &= !UserReserve::USING_AS_COLLATERAL;
                }
                Ok(())
            })?;
            Ok(positions(&[pos]))
        }
        spoke::UpdateUserRiskPremium::SIGNATURE_HASH => {
            let ev: spoke::UpdateUserRiskPremium = decode(log)?;
            let pos = st.intern(&key(ev.user))?;
            let rp: u32 = narrow(ev.riskPremium)?;
            if rp > 0xFF_FFFF {
                return Err(ProtocolError::MalformedLog);
            }
            let mut e = *st.extra(pos)?;
            e.view_mut::<UserExtra>()?.risk_premium = rp;
            st.set_extra(pos, e)?;
            Ok(positions(&[pos]))
        }
        spoke::RefreshSingleUserDynamicConfig::SIGNATURE_HASH => {
            let ev: spoke::RefreshSingleUserDynamicConfig = decode(log)?;
            let slot = reserve_slot(ev.reserveId)?;
            let pos = st.intern(&key(ev.user))?;
            snapshot(st, market, pos, slot)?;
            Ok(positions(&[pos]))
        }
        spoke::RefreshAllUserDynamicConfig::SIGNATURE_HASH => {
            // `_processUserAccountData(refreshConfig = true)`: every reserve
            // with the user's collateral bit set takes the current key.
            let ev: spoke::RefreshAllUserDynamicConfig = decode(log)?;
            let pos = st.intern(&key(ev.user))?;
            let n = u16::try_from(st.markets(market)?.len())
                .map_err(|_| ProtocolError::MalformedLog)?;
            let mut flagged: SmallVec<[u16; 16]> = SmallVec::new();
            for slot in 1..n {
                let u: &UserReserve = st.slot_extra(pos, slot)?.view()?;
                if u.flags & UserReserve::USING_AS_COLLATERAL != 0 {
                    flagged.push(slot);
                }
            }
            for slot in flagged {
                snapshot(st, market, pos, slot)?;
            }
            Ok(positions(&[pos]))
        }
        spoke::SetUserPositionManager::SIGNATURE_HASH => {
            let _: spoke::SetUserPositionManager = decode(log)?;
            Ok(DirtySet::None)
        }
        spoke::UpdatePositionManager::SIGNATURE_HASH => {
            let _: spoke::UpdatePositionManager = decode(log)?;
            Ok(DirtySet::None)
        }
        // ---- spoke configuration -----------------------------------------
        spoke::UpdateLiquidationConfig::SIGNATURE_HASH => {
            let ev: spoke::UpdateLiquidationConfig = decode(log)?;
            let c = ev.config;
            if U256::from(c.targetHealthFactor) < HF_THRESHOLD_WAD
                || U256::from(c.liquidationBonusFactor) > BPS
                || U256::from(c.healthFactorForMaxBonus) >= HF_THRESHOLD_WAD
            {
                return Err(ProtocolError::MalformedLog);
            }
            ensure_meta(st, market)?;
            let mut m = meta(st, market)?;
            m.target_hf = c.targetHealthFactor;
            m.hf_for_max_bonus = c.healthFactorForMaxBonus;
            m.bonus_factor = c.liquidationBonusFactor;
            set_meta(st, market, m)?;
            let n = st.markets(market)?.len();
            let slots: SmallVec<[u16; 16]> = (1..n)
                .map(|i| u16::try_from(i).map_err(|_| ProtocolError::MalformedLog))
                .collect::<Result<_>>()?;
            Ok(if slots.is_empty() {
                DirtySet::None
            } else {
                DirtySet::MarketReprice(rows(market, &slots))
            })
        }
        spoke::AddReserve::SIGNATURE_HASH => {
            let ev: spoke::AddReserve = decode(log)?;
            ensure_meta(st, market)?;
            let slot = reserve_slot(ev.reserveId)?;
            let have = u16::try_from(st.markets(market)?.len())
                .map_err(|_| ProtocolError::MalformedLog)?;
            if have != slot {
                return Err(ProtocolError::SlotMismatch {
                    expected: have,
                    got: slot,
                });
            }
            let r: u16 = narrow(ev.reserveId)?;
            let priced = meta(st, market)?.is_priced(r);
            let hub_slot: u16 = narrow(ev.assetId)?;
            let row = match cfg.hub_market(ev.hub) {
                Some(hm) => {
                    let h = *st.market(MarketSlot {
                        market: hm,
                        slot: hub_slot,
                    })?;
                    let hb: &HubRow = h.body()?;
                    let spoke_bits = *hb.spokes.0.get(idx).ok_or(ProtocolError::UnexpectedLog)?;
                    let mut row = MarketRow::blank(h.asset, h.decimals);
                    row.price_feed = h.price_feed;
                    row.hub_market = hm.0;
                    row.hub_slot = hub_slot;
                    row.last_update = h.last_update;
                    let mut flags = 0u8;
                    if spoke_bits & SpokeFlags::ACTIVE != 0 {
                        flags |= ReserveCfg::SPOKE_ACTIVE;
                    }
                    if spoke_bits & SpokeFlags::HALTED != 0 {
                        flags |= ReserveCfg::SPOKE_HALTED;
                    }
                    let cfg_body = ReserveCfg {
                        flags,
                        ..bytemuck::Zeroable::zeroed()
                    };
                    *row.body_mut::<Reserve>()? = Reserve {
                        hub: hb.asset,
                        cfg: cfg_body,
                    };
                    row.flags = derive_flags(&cfg_body, priced && h.asset != UNMAPPED_ASSET);
                    row
                }
                None => {
                    // A hub the registry does not pin: the reserve exists
                    // (slots must stay dense) but nothing in it is priced.
                    let mut row = MarketRow::blank(UNMAPPED_ASSET, 0);
                    row.flags = MarketFlags::UNPRICED;
                    row
                }
            };
            st.push_market(market, row)?;
            Ok(DirtySet::None)
        }
        spoke::UpdateReserveConfig::SIGNATURE_HASH => {
            let ev: spoke::UpdateReserveConfig = decode(log)?;
            let slot = reserve_slot(ev.reserveId)?;
            let at = MarketSlot { market, slot };
            reserve_update(st, at, |row| {
                let unpriced = row.flags.contains(MarketFlags::UNPRICED);
                let c = &mut row.body_mut::<Reserve>()?.cfg;
                c.collateral_risk = ev.config.collateralRisk.to::<u32>();
                c.flags &= ReserveCfg::SPOKE_ACTIVE | ReserveCfg::SPOKE_HALTED;
                if ev.config.paused {
                    c.flags |= ReserveCfg::PAUSED;
                }
                if ev.config.frozen {
                    c.flags |= ReserveCfg::FROZEN;
                }
                if ev.config.borrowable {
                    c.flags |= ReserveCfg::BORROWABLE;
                }
                if ev.config.receiveSharesEnabled {
                    c.flags |= ReserveCfg::RECEIVE_SHARES;
                }
                let c = *c;
                row.flags = derive_flags(&c, !unpriced);
                Ok(())
            })?;
            Ok(DirtySet::MarketReprice(rows(market, &[slot])))
        }
        spoke::UpdateReservePriceSource::SIGNATURE_HASH => {
            let ev: spoke::UpdateReservePriceSource = decode(log)?;
            set_source(cfg, st, market, log.address, ev.reserveId, ev.priceSource)
        }
        spoke::AddDynamicReserveConfig::SIGNATURE_HASH => {
            // The reserve's *current* key moves; positions keep their
            // snapshot until a refresh log — nothing the engine reads
            // changed yet.
            let ev: spoke::AddDynamicReserveConfig = decode(log)?;
            let slot = reserve_slot(ev.reserveId)?;
            reserve_update(st, MarketSlot { market, slot }, |row| {
                let c = &mut row.body_mut::<Reserve>()?.cfg;
                c.collateral_factor = ev.config.collateralFactor;
                c.liquidation_fee = ev.config.liquidationFee;
                c.max_liquidation_bonus = ev.config.maxLiquidationBonus;
                c.dyn_key = ev.dynamicConfigKey;
                Ok(())
            })?;
            Ok(DirtySet::None)
        }
        spoke::UpdateDynamicReserveConfig::SIGNATURE_HASH => {
            // The values behind an existing key change: every user-reserve
            // pair snapshotted on that key (and using the reserve as
            // collateral) follows, and so does the reserve's current
            // config when the key is the current one.
            let ev: spoke::UpdateDynamicReserveConfig = decode(log)?;
            let slot = reserve_slot(ev.reserveId)?;
            let at = MarketSlot { market, slot };
            let current = st.market(at)?.body::<Reserve>()?.cfg.dyn_key;
            if current == ev.dynamicConfigKey {
                reserve_update(st, at, |row| {
                    let c = &mut row.body_mut::<Reserve>()?.cfg;
                    c.collateral_factor = ev.config.collateralFactor;
                    c.liquidation_fee = ev.config.liquidationFee;
                    c.max_liquidation_bonus = ev.config.maxLiquidationBonus;
                    Ok(())
                })?;
            }
            let mut affected: Vec<PositionId> = Vec::new();
            for i in 0..st.positions_len() {
                let pos = PositionId(i);
                if st.position_key(pos)?.market != market {
                    continue;
                }
                let u: &UserReserve = st.slot_extra(pos, slot)?.view()?;
                if u.flags & UserReserve::USING_AS_COLLATERAL != 0
                    && u.dyn_key == ev.dynamicConfigKey
                {
                    affected.push(pos);
                }
            }
            for pos in affected {
                user_reserve_update(st, pos, slot, |u| {
                    u.collateral_factor = ev.config.collateralFactor;
                    u.liquidation_fee = ev.config.liquidationFee;
                    u.max_liquidation_bonus = ev.config.maxLiquidationBonus;
                    Ok(())
                })?;
            }
            Ok(DirtySet::MarketReprice(rows(market, &[slot])))
        }
        _ => Err(ProtocolError::UnexpectedLog),
    }
}
