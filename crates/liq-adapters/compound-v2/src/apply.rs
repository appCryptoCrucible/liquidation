//! `Protocol::apply_log` for Compound V2. Journal-before-write is the writer.

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use liq_protocol::{
    DecodedLog, DirtyPositions, DirtyRows, DirtySet, MarketFlags, MarketRow, MarketSlot,
    ProtocolError, Result, StateWriter,
};
use liq_types::fixed::FixedError;
use liq_types::{MarketId, PositionId, PositionKey};

use crate::config::Config;
use crate::events::{comptroller as cmp, ctoken, halt, pause_global, pause_market};
use crate::layout::{
    BorrowSnap, CTokenRow, ComptrollerMeta, UserExtra, INITIAL_BORROW_INDEX, META_ASSET, META_SLOT,
    UNMAPPED_ASSET,
};
use crate::math::{self, addr20, addr_from, exchange_rate_stored, last_update, u128_of};

#[inline]
fn decode<E: SolEvent>(log: &DecodedLog<'_>) -> Result<E> {
    E::decode_raw_log(log.topics.iter().copied(), log.data).map_err(|_| ProtocolError::MalformedLog)
}

#[inline]
fn positions(ids: &[PositionId]) -> DirtySet {
    DirtySet::Positions(DirtyPositions::from_slice(ids))
}

#[inline]
fn row_dirty(market: MarketId, slot: u16) -> DirtySet {
    DirtySet::MarketReprice(DirtyRows::from_slice(&[MarketSlot { market, slot }]))
}

#[inline]
fn accrual_dirty(market: MarketId, slot: u16) -> DirtySet {
    DirtySet::MarketAccrual(DirtyRows::from_slice(&[MarketSlot { market, slot }]))
}

fn intern_user(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    user: Address,
) -> Result<PositionId> {
    st.intern(&PositionKey {
        protocol: cfg.protocol,
        market,
        user,
    })
}

fn extra(st: &dyn StateWriter, pos: PositionId) -> Result<UserExtra> {
    Ok(*st.extra(pos)?.view::<UserExtra>()?)
}

fn set_extra(st: &mut dyn StateWriter, pos: PositionId, e: UserExtra) -> Result<()> {
    let mut repr = *st.extra(pos)?;
    *repr.view_mut::<UserExtra>()? = e;
    st.set_extra(pos, repr)
}

fn set_entered(st: &mut dyn StateWriter, pos: PositionId, slot: u16, entered: bool) -> Result<()> {
    let mut e = extra(st, pos)?;
    let bit = 1u128
        .checked_shl(u32::from(slot))
        .ok_or(FixedError::Overflow)?;
    e.entered_mask = if entered {
        e.entered_mask | bit
    } else {
        e.entered_mask & !bit
    };
    set_extra(st, pos, e)
}

fn set_borrow_snap(
    st: &mut dyn StateWriter,
    pos: PositionId,
    slot: u16,
    index: u128,
) -> Result<()> {
    let mut repr = *st.slot_extra(pos, slot)?;
    *repr.view_mut::<BorrowSnap>()? = BorrowSnap {
        interest_index: index,
        _pad: [0; 48],
    };
    st.set_slot_extra(pos, slot, repr)
}

fn add_u128(cur: u128, d: U256, add: bool) -> Result<u128> {
    let c = U256::from(cur);
    let n = if add {
        c.checked_add(d).ok_or(FixedError::Overflow)?
    } else {
        c.checked_sub(d).ok_or(FixedError::Underflow)?
    };
    u128_of(n)
}

fn market_id(cfg: &Config, comptroller: Address) -> Result<MarketId> {
    cfg.interned_id(comptroller)
        .ok_or(ProtocolError::UnexpectedLog)
}

fn ensure_meta(cfg: &Config, st: &mut dyn StateWriter, market: MarketId, ts: u64) -> Result<()> {
    if st.markets(market).is_ok() {
        return Ok(());
    }
    let fork = cfg
        .fork_by_market(market)
        .ok_or(ProtocolError::UnknownMarket(market))?;
    let mut row = MarketRow::blank(META_ASSET, 0);
    row.last_update = last_update(ts)?;
    let meta: &mut ComptrollerMeta = row.body_mut()?;
    meta.close_factor_mantissa = fork.close_factor_mantissa;
    meta.liquidation_incentive_mantissa = fork.liquidation_incentive_mantissa;
    meta.oracle = addr20(fork.oracle);
    meta.flags = if fork.close_factor_mantissa != 0 && fork.liquidation_incentive_mantissa != 0 {
        ComptrollerMeta::PARAMS_KNOWN
    } else {
        0
    };
    st.push_market(market, row).map(|_| ())
}

fn seed_row(cfg: &Config, market: MarketId, pin: &crate::config::CTokenPin) -> Result<MarketRow> {
    let fork = cfg
        .fork_by_market(market)
        .ok_or(ProtocolError::UnknownMarket(market))?;
    let cether = pin.underlying == Address::ZERO;
    let (asset, decimals, priced) = if cether {
        match cfg.native_asset(fork) {
            Some(a) => (a.asset, a.decimals, true),
            None => (UNMAPPED_ASSET, 18, false),
        }
    } else {
        match cfg.asset_by_underlying(pin.underlying) {
            Some(a) => (a.asset, a.decimals, true),
            None => (UNMAPPED_ASSET, 0, false),
        }
    };
    let mut row = MarketRow::blank(asset, decimals);
    row.flags = if priced {
        MarketFlags::NONE
    } else {
        MarketFlags::UNPRICED
    };
    let body: &mut CTokenRow = row.body_mut()?;
    body.ctoken = addr20(pin.ctoken);
    body.underlying = addr20(pin.underlying);
    body.borrow_index = INITIAL_BORROW_INDEX;
    body.flags = CTokenRow::LISTED | CTokenRow::DEC_KNOWN;
    if cether {
        body.flags |= CTokenRow::CETHER;
    }
    if priced {
        body.flags |= CTokenRow::PRICED;
    }
    Ok(row)
}

fn list_ctoken(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    ctoken: Address,
    ts: u64,
) -> Result<u16> {
    ensure_meta(cfg, st, market, ts)?;
    let rows = st.markets(market)?;
    for (i, r) in rows.iter().enumerate() {
        if i == 0 {
            continue;
        }
        let body: &CTokenRow = r.body()?;
        if addr_from(body.ctoken) == ctoken {
            return u16::try_from(i).map_err(|_| ProtocolError::Internal);
        }
    }
    let seeded = cfg.ctoken_seed(ctoken);
    let mut row = if let Some((_, pin)) = seeded {
        seed_row(cfg, market, pin)?
    } else {
        let mut row = MarketRow::blank(UNMAPPED_ASSET, 0);
        row.flags = MarketFlags::UNPRICED;
        let body: &mut CTokenRow = row.body_mut()?;
        body.ctoken = addr20(ctoken);
        body.borrow_index = INITIAL_BORROW_INDEX;
        body.flags = CTokenRow::LISTED;
        row
    };
    row.last_update = last_update(ts)?;
    let slot = st.push_market(market, row)?;
    Ok(slot.slot)
}

fn find_ctoken(st: &dyn StateWriter, market: MarketId, ctoken: Address) -> Result<u16> {
    let rows = st.markets(market)?;
    for (i, r) in rows.iter().enumerate() {
        if i == 0 {
            continue;
        }
        let body: &CTokenRow = r.body()?;
        if addr_from(body.ctoken) == ctoken {
            return u16::try_from(i).map_err(|_| ProtocolError::Internal);
        }
    }
    Err(ProtocolError::UnexpectedLog)
}

fn locate_ctoken(cfg: &Config, st: &dyn StateWriter, ctoken: Address) -> Result<(MarketId, u16)> {
    if let Some((comptroller, _)) = cfg.ctoken_seed(ctoken) {
        let market = market_id(cfg, comptroller)?;
        if let Ok(slot) = find_ctoken(st, market, ctoken) {
            return Ok((market, slot));
        }
        return Err(ProtocolError::UnexpectedLog);
    }
    for (addr, market) in &cfg.interned {
        if cfg.fork_by_comptroller(*addr).is_none() {
            continue;
        }
        if let Ok(slot) = find_ctoken(st, *market, ctoken) {
            return Ok((*market, slot));
        }
    }
    Err(ProtocolError::UnexpectedLog)
}

fn patch_meta(
    st: &mut dyn StateWriter,
    market: MarketId,
    ts: u32,
    f: impl FnOnce(&mut ComptrollerMeta) -> Result<()>,
) -> Result<()> {
    let at = MarketSlot {
        market,
        slot: META_SLOT,
    };
    let mut row = *st.market(at)?;
    f(row.body_mut::<ComptrollerMeta>()?)?;
    row.last_update = ts;
    st.set_market(at, row)
}

fn patch_ctoken(
    st: &mut dyn StateWriter,
    market: MarketId,
    slot: u16,
    ts: u32,
    f: impl FnOnce(&mut CTokenRow) -> Result<()>,
) -> Result<()> {
    let at = MarketSlot { market, slot };
    let mut row = *st.market(at)?;
    f(row.body_mut::<CTokenRow>()?)?;
    row.last_update = ts;
    let viewed = row.body::<CTokenRow>()?;
    row.flags = if viewed.flags & CTokenRow::PRICED == 0 {
        MarketFlags::UNPRICED
    } else {
        MarketFlags::NONE
    };
    st.set_market(at, row)
}

fn recompute_exrate(body: &mut CTokenRow) -> Result<()> {
    if body.total_supply == 0 {
        body.flags &= !CTokenRow::EXRATE_KNOWN;
        return Ok(());
    }
    let er = exchange_rate_stored(
        U256::from(body.cash),
        U256::from(body.total_borrows),
        U256::from(body.total_reserves),
        U256::from(body.total_supply),
    )?;
    body.exchange_rate_mantissa = u128_of(er)?;
    body.flags |= CTokenRow::EXRATE_KNOWN;
    Ok(())
}

const HALT: &[B256] = &[
    halt::Upgraded::SIGNATURE_HASH,
    halt::AdminChanged::SIGNATURE_HASH,
    halt::Initialized::SIGNATURE_HASH,
];

pub(crate) fn apply_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let topic0 = *log.topics.first().ok_or(ProtocolError::MalformedLog)?;
    if HALT.contains(&topic0) {
        return if log.block <= cfg.pinned_through {
            Ok(DirtySet::None)
        } else {
            Err(ProtocolError::HaltSignal)
        };
    }
    if let Some(market) = cfg.interned_id(log.address) {
        if cfg.fork_by_market(market).is_some() {
            return comptroller_log(cfg, st, log, market, topic0);
        }
    }
    if let Some((comptroller, _)) = cfg.ctoken_seed(log.address) {
        let market = market_id(cfg, comptroller)?;
        let slot = find_ctoken(st, market, log.address)?;
        return ctoken_log(cfg, st, log, market, slot, topic0);
    }
    // Discovered cToken already listed on a configured fork.
    if let Ok((market, slot)) = locate_ctoken(cfg, st, log.address) {
        return ctoken_log(cfg, st, log, market, slot, topic0);
    }
    Err(ProtocolError::UnexpectedLog)
}

fn comptroller_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    market: MarketId,
    topic0: B256,
) -> Result<DirtySet> {
    let ts = last_update(log.timestamp)?;
    ensure_meta(cfg, st, market, log.timestamp)?;
    if topic0 == cmp::MarketListed::SIGNATURE_HASH {
        let ev = decode::<cmp::MarketListed>(log)?;
        let slot = list_ctoken(cfg, st, market, ev.cToken, log.timestamp)?;
        return Ok(row_dirty(market, slot));
    }
    if topic0 == cmp::MarketEntered::SIGNATURE_HASH {
        let ev = decode::<cmp::MarketEntered>(log)?;
        let slot = find_ctoken(st, market, ev.cToken)?;
        let pos = intern_user(cfg, st, market, ev.account)?;
        set_entered(st, pos, slot, true)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == cmp::MarketExited::SIGNATURE_HASH {
        let ev = decode::<cmp::MarketExited>(log)?;
        let slot = find_ctoken(st, market, ev.cToken)?;
        let pos = intern_user(cfg, st, market, ev.account)?;
        set_entered(st, pos, slot, false)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == cmp::NewCloseFactor::SIGNATURE_HASH {
        let ev = decode::<cmp::NewCloseFactor>(log)?;
        let cf = u128_of(ev.newCloseFactorMantissa)?;
        if !math::close_factor_in_pin_bounds(ev.newCloseFactorMantissa) {
            return Err(ProtocolError::HaltSignal);
        }
        patch_meta(st, market, ts, |m| {
            m.close_factor_mantissa = cf;
            m.flags |= ComptrollerMeta::PARAMS_KNOWN;
            Ok(())
        })?;
        return Ok(DirtySet::ProtocolWide);
    }
    if topic0 == cmp::NewLiquidationIncentive::SIGNATURE_HASH {
        let ev = decode::<cmp::NewLiquidationIncentive>(log)?;
        let li = u128_of(ev.newLiquidationIncentiveMantissa)?;
        patch_meta(st, market, ts, |m| {
            m.liquidation_incentive_mantissa = li;
            m.flags |= ComptrollerMeta::PARAMS_KNOWN;
            Ok(())
        })?;
        return Ok(DirtySet::ProtocolWide);
    }
    if topic0 == cmp::NewCollateralFactor::SIGNATURE_HASH {
        let ev = decode::<cmp::NewCollateralFactor>(log)?;
        let slot = find_ctoken(st, market, ev.cToken)?;
        let cf = u128_of(ev.newCollateralFactorMantissa)?;
        patch_ctoken(st, market, slot, ts, |b| {
            b.collateral_factor_mantissa = cf;
            Ok(())
        })?;
        return Ok(row_dirty(market, slot));
    }
    if topic0 == cmp::NewPriceOracle::SIGNATURE_HASH {
        let ev = decode::<cmp::NewPriceOracle>(log)?;
        patch_meta(st, market, ts, |m| {
            m.oracle = addr20(ev.newPriceOracle);
            Ok(())
        })?;
        return Ok(DirtySet::ProtocolWide);
    }
    if topic0 == pause_global::ActionPaused::SIGNATURE_HASH {
        let ev = decode::<pause_global::ActionPaused>(log)?;
        if ev.action == "Seize" {
            patch_meta(st, market, ts, |m| {
                if ev.pauseState {
                    m.flags |= ComptrollerMeta::SEIZE_PAUSED;
                } else {
                    m.flags &= !ComptrollerMeta::SEIZE_PAUSED;
                }
                Ok(())
            })?;
            return Ok(DirtySet::ProtocolWide);
        }
        return Ok(DirtySet::None);
    }
    if topic0 == pause_market::ActionPaused::SIGNATURE_HASH {
        let ev = decode::<pause_market::ActionPaused>(log)?;
        if ev.action != "Borrow" {
            return Ok(DirtySet::None);
        }
        let slot = find_ctoken(st, market, ev.cToken)?;
        patch_ctoken(st, market, slot, ts, |b| {
            if ev.pauseState {
                b.flags |= CTokenRow::BORROW_PAUSED;
            } else {
                b.flags &= !CTokenRow::BORROW_PAUSED;
            }
            Ok(())
        })?;
        return Ok(row_dirty(market, slot));
    }
    Err(ProtocolError::UnexpectedLog)
}

fn ctoken_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    market: MarketId,
    slot: u16,
    topic0: B256,
) -> Result<DirtySet> {
    let ts = last_update(log.timestamp)?;
    if topic0 == ctoken::AccrueInterest::SIGNATURE_HASH {
        let ev = decode::<ctoken::AccrueInterest>(log)?;
        patch_ctoken(st, market, slot, ts, |b| {
            b.cash = u128_of(ev.cashPrior)?;
            b.borrow_index = u128_of(ev.borrowIndex)?;
            b.total_borrows = u128_of(ev.totalBorrows)?;
            if !ev.interestAccumulated.is_zero() {
                if b.flags & CTokenRow::RF_KNOWN == 0 {
                    b.flags &= !CTokenRow::EXRATE_KNOWN;
                    return Ok(());
                }
                let add = math::mul_scalar_truncate(
                    U256::from(b.reserve_factor_mantissa),
                    ev.interestAccumulated,
                )?;
                b.total_reserves = add_u128(b.total_reserves, add, true)?;
            }
            recompute_exrate(b)
        })?;
        return Ok(accrual_dirty(market, slot));
    }
    if topic0 == ctoken::Mint::SIGNATURE_HASH {
        let ev = decode::<ctoken::Mint>(log)?;
        patch_ctoken(st, market, slot, ts, |b| {
            b.cash = add_u128(b.cash, ev.mintAmount, true)?;
            b.total_supply = add_u128(b.total_supply, ev.mintTokens, true)?;
            recompute_exrate(b)
        })?;
        let pos = intern_user(cfg, st, market, ev.minter)?;
        let cur = st.supply(pos, slot)?;
        st.set_supply(pos, slot, add_u128(cur, ev.mintTokens, true)?)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == ctoken::Redeem::SIGNATURE_HASH {
        let ev = decode::<ctoken::Redeem>(log)?;
        patch_ctoken(st, market, slot, ts, |b| {
            b.cash = add_u128(b.cash, ev.redeemAmount, false)?;
            b.total_supply = add_u128(b.total_supply, ev.redeemTokens, false)?;
            recompute_exrate(b)
        })?;
        let pos = intern_user(cfg, st, market, ev.redeemer)?;
        let cur = st.supply(pos, slot)?;
        st.set_supply(pos, slot, add_u128(cur, ev.redeemTokens, false)?)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == ctoken::Borrow::SIGNATURE_HASH {
        let ev = decode::<ctoken::Borrow>(log)?;
        let idx = {
            let row = st.market(MarketSlot { market, slot })?;
            let b: &CTokenRow = row.body()?;
            b.borrow_index
        };
        patch_ctoken(st, market, slot, ts, |b| {
            b.cash = add_u128(b.cash, ev.borrowAmount, false)?;
            b.total_borrows = u128_of(ev.totalBorrows)?;
            recompute_exrate(b)
        })?;
        let pos = intern_user(cfg, st, market, ev.borrower)?;
        st.set_debt(pos, slot, u128_of(ev.accountBorrows)?)?;
        set_borrow_snap(st, pos, slot, idx)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == ctoken::RepayBorrow::SIGNATURE_HASH {
        let ev = decode::<ctoken::RepayBorrow>(log)?;
        let idx = {
            let row = st.market(MarketSlot { market, slot })?;
            let b: &CTokenRow = row.body()?;
            b.borrow_index
        };
        patch_ctoken(st, market, slot, ts, |b| {
            b.cash = add_u128(b.cash, ev.repayAmount, true)?;
            b.total_borrows = u128_of(ev.totalBorrows)?;
            recompute_exrate(b)
        })?;
        let pos = intern_user(cfg, st, market, ev.borrower)?;
        st.set_debt(pos, slot, u128_of(ev.accountBorrows)?)?;
        set_borrow_snap(st, pos, slot, idx)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == ctoken::LiquidateBorrow::SIGNATURE_HASH {
        let ev = decode::<ctoken::LiquidateBorrow>(log)?;
        let coll_slot = find_ctoken(st, market, ev.cTokenCollateral)?;
        let borrower = intern_user(cfg, st, market, ev.borrower)?;
        let liquidator = intern_user(cfg, st, market, ev.liquidator)?;
        let seize = u128_of(ev.seizeTokens)?;
        let cur = st.supply(borrower, coll_slot)?;
        if cur >= seize {
            st.set_supply(
                borrower,
                coll_slot,
                cur.checked_sub(seize).ok_or(FixedError::Underflow)?,
            )?;
        }
        return Ok(positions(&[borrower, liquidator]));
    }
    if topic0 == ctoken::NewReserveFactor::SIGNATURE_HASH {
        let ev = decode::<ctoken::NewReserveFactor>(log)?;
        patch_ctoken(st, market, slot, ts, |b| {
            b.reserve_factor_mantissa = u128_of(ev.newReserveFactorMantissa)?;
            b.flags |= CTokenRow::RF_KNOWN;
            Ok(())
        })?;
        return Ok(row_dirty(market, slot));
    }
    if topic0 == ctoken::Transfer::SIGNATURE_HASH {
        let ev = decode::<ctoken::Transfer>(log)?;
        if ev.from == Address::ZERO || ev.to == Address::ZERO {
            return Ok(DirtySet::None);
        }
        let amt = u128_of(ev.amount)?;
        let mut ids = DirtyPositions::new();
        if ev.from != Address::ZERO {
            let p = intern_user(cfg, st, market, ev.from)?;
            let cur = st.supply(p, slot)?;
            if cur >= amt {
                st.set_supply(p, slot, cur.checked_sub(amt).ok_or(FixedError::Underflow)?)?;
                ids.push(p);
            }
        }
        if ev.to != Address::ZERO {
            let p = intern_user(cfg, st, market, ev.to)?;
            let cur = st.supply(p, slot)?;
            st.set_supply(p, slot, add_u128(cur, ev.amount, true)?)?;
            ids.push(p);
        }
        return Ok(DirtySet::Positions(ids));
    }
    Err(ProtocolError::UnexpectedLog)
}
