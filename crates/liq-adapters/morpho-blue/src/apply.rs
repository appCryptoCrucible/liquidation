//! `Protocol::apply_log` for Morpho Blue. Journal-before-write is the writer.

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use liq_protocol::{
    DecodedLog, DirtyPositions, DirtyRows, DirtySet, MarketFlags, MarketRow, MarketSlot,
    ProtocolError, Result, StateWriter,
};
use liq_types::fixed::FixedError;
use liq_types::{MarketId, PositionId, PositionKey};

use crate::config::Config;
use crate::events::{self, halt, MarketParams};
use crate::layout::{CatalogEntry, LoanRow, CATALOG_ASSET, COLL_SLOT, LOAN_SLOT, UNMAPPED_ASSET};

#[inline]
fn decode<E: SolEvent>(log: &DecodedLog<'_>) -> Result<E> {
    E::decode_raw_log(log.topics.iter().copied(), log.data).map_err(|_| ProtocolError::MalformedLog)
}

#[inline]
fn narrow<T: TryFrom<U256>>(v: U256) -> Result<T> {
    T::try_from(v).map_err(|_| ProtocolError::MalformedLog)
}

#[inline]
fn last_update(ts: u64) -> Result<u32> {
    u32::try_from(ts).map_err(|_| ProtocolError::Fixed(FixedError::Overflow))
}

#[inline]
fn positions(ids: &[PositionId]) -> DirtySet {
    DirtySet::Positions(DirtyPositions::from_slice(ids))
}

#[inline]
fn rows_of(market: MarketId) -> DirtyRows {
    let mut d = DirtyRows::new();
    d.push(MarketSlot {
        market,
        slot: LOAN_SLOT,
    });
    d.push(MarketSlot {
        market,
        slot: COLL_SLOT,
    });
    d
}

fn intern(
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

fn lookup(cfg: &Config, st: &dyn StateWriter, id: B256) -> Result<MarketId> {
    let cat = st.markets(cfg.catalog)?;
    for row in cat {
        let e: &CatalogEntry = row.body()?;
        if e.morpho_id == *id {
            return Ok(MarketId(e.market));
        }
    }
    Err(ProtocolError::UnknownMarket(cfg.catalog))
}

fn add_u128(cur: u128, d: U256, add: bool) -> Result<u128> {
    let c = U256::from(cur);
    let n = if add {
        c.checked_add(d).ok_or(FixedError::Overflow)?
    } else {
        c.checked_sub(d).ok_or(FixedError::Underflow)?
    };
    narrow(n)
}

fn patch_loan(
    st: &mut dyn StateWriter,
    market: MarketId,
    ts: Option<u32>,
    f: impl FnOnce(&mut LoanRow) -> Result<()>,
) -> Result<DirtyRows> {
    let at = MarketSlot {
        market,
        slot: LOAN_SLOT,
    };
    let mut row = *st.market(at)?;
    f(row.body_mut::<LoanRow>()?)?;
    if let Some(t) = ts {
        row.last_update = t;
    }
    let priced = row.body::<LoanRow>()?.flags & LoanRow::PRICED != 0;
    row.flags = if priced {
        MarketFlags::NONE
    } else {
        MarketFlags::UNPRICED
    };
    st.set_market(at, row)?;
    Ok(rows_of(market))
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
    if log.address != cfg.morpho {
        return Err(ProtocolError::UnexpectedLog);
    }
    let topic0 = *log.topics.first().ok_or(ProtocolError::MalformedLog)?;
    if HALT.contains(&topic0) {
        return if log.block <= cfg.pinned_through {
            Ok(DirtySet::None)
        } else {
            Err(ProtocolError::HaltSignal)
        };
    }
    if topic0 == events::CreateMarket::SIGNATURE_HASH {
        return create_market(cfg, st, log);
    }
    if topic0 == events::AccrueInterest::SIGNATURE_HASH {
        return accrue(cfg, st, log);
    }
    if topic0 == events::SetFee::SIGNATURE_HASH {
        return set_fee(cfg, st, log);
    }
    if topic0 == events::Supply::SIGNATURE_HASH {
        return supply(cfg, st, log, true);
    }
    if topic0 == events::Withdraw::SIGNATURE_HASH {
        return supply(cfg, st, log, false);
    }
    if topic0 == events::Borrow::SIGNATURE_HASH {
        return borrow(cfg, st, log, true);
    }
    if topic0 == events::Repay::SIGNATURE_HASH {
        return borrow(cfg, st, log, false);
    }
    if topic0 == events::SupplyCollateral::SIGNATURE_HASH {
        return coll(cfg, st, log, true);
    }
    if topic0 == events::WithdrawCollateral::SIGNATURE_HASH {
        return coll(cfg, st, log, false);
    }
    if topic0 == events::Liquidate::SIGNATURE_HASH {
        return liquidate(cfg, st, log);
    }
    if topic0 == events::SetOwner::SIGNATURE_HASH
        || topic0 == events::SetFeeRecipient::SIGNATURE_HASH
        || topic0 == events::EnableIrm::SIGNATURE_HASH
        || topic0 == events::EnableLltv::SIGNATURE_HASH
        || topic0 == events::FlashLoan::SIGNATURE_HASH
        || topic0 == events::SetAuthorization::SIGNATURE_HASH
        || topic0 == events::IncrementNonce::SIGNATURE_HASH
    {
        return Ok(DirtySet::None);
    }
    Ok(DirtySet::None)
}

fn create_market(cfg: &Config, st: &mut dyn StateWriter, log: &DecodedLog<'_>) -> Result<DirtySet> {
    let ev = decode::<events::CreateMarket>(log)?;
    let n = match st.markets(cfg.catalog) {
        Ok(r) => r.len(),
        Err(ProtocolError::UnknownMarket(_)) => 0,
        Err(e) => return Err(e),
    };
    let slot = u16::try_from(n).map_err(|_| ProtocolError::MalformedLog)?;
    let market = cfg.assigned_market(slot);
    let loan_tok = cfg
        .asset_by_underlying(ev.marketParams.loanToken)
        .ok_or(ProtocolError::OracleSourceMismatch)?;
    let coll_tok = cfg
        .asset_by_underlying(ev.marketParams.collateralToken)
        .ok_or(ProtocolError::OracleSourceMismatch)?;
    let priced = cfg.oracle_pinned(ev.marketParams.oracle);
    let mut cat = MarketRow::blank(CATALOG_ASSET, 0);
    {
        let e: &mut CatalogEntry = cat.body_mut()?;
        e.morpho_id = *ev.id;
        e.market = market.0;
    }
    st.push_market(cfg.catalog, cat)?;

    let mut loan = MarketRow::blank(loan_tok.asset, loan_tok.decimals);
    loan.price_feed = loan_tok.feed;
    loan.last_update = last_update(log.timestamp)?;
    loan.flags = if priced {
        MarketFlags::NONE
    } else {
        MarketFlags::UNPRICED
    };
    {
        let b: &mut LoanRow = loan.body_mut()?;
        b.lltv = narrow(ev.marketParams.lltv)?;
        b.coll_asset = coll_tok.asset.0;
        b.coll_decimals = coll_tok.decimals;
        b.flags = if priced { LoanRow::PRICED } else { 0 };
        b.oracle = *ev.marketParams.oracle.as_ref();
        b.irm = *ev.marketParams.irm.as_ref();
        b.morpho_id = *ev.id;
    }
    let got = st.push_market(market, loan)?;
    if got.slot != LOAN_SLOT {
        return Err(ProtocolError::SlotMismatch {
            expected: LOAN_SLOT,
            got: got.slot,
        });
    }
    let mut coll = MarketRow::blank(coll_tok.asset, coll_tok.decimals);
    coll.price_feed = coll_tok.feed;
    coll.last_update = last_update(log.timestamp)?;
    coll.flags = if priced {
        MarketFlags::NONE
    } else {
        MarketFlags::UNPRICED
    };
    let got = st.push_market(market, coll)?;
    if got.slot != COLL_SLOT {
        return Err(ProtocolError::SlotMismatch {
            expected: COLL_SLOT,
            got: got.slot,
        });
    }
    let _ = core::mem::size_of::<MarketParams>();
    Ok(DirtySet::MarketReprice(rows_of(market)))
}

fn accrue(cfg: &Config, st: &mut dyn StateWriter, log: &DecodedLog<'_>) -> Result<DirtySet> {
    let ev = decode::<events::AccrueInterest>(log)?;
    let market = lookup(cfg, st, ev.id)?;
    patch_loan(st, market, Some(last_update(log.timestamp)?), |b| {
        b.last_borrow_rate = narrow(ev.prevBorrowRate)?;
        b.total_borrow_assets = add_u128(b.total_borrow_assets, ev.interest, true)?;
        b.total_supply_assets = add_u128(b.total_supply_assets, ev.interest, true)?;
        b.total_supply_shares = add_u128(b.total_supply_shares, ev.feeShares, true)?;
        Ok(())
    })?;
    Ok(DirtySet::MarketAccrual(rows_of(market)))
}

fn set_fee(cfg: &Config, st: &mut dyn StateWriter, log: &DecodedLog<'_>) -> Result<DirtySet> {
    let ev = decode::<events::SetFee>(log)?;
    let market = lookup(cfg, st, ev.id)?;
    patch_loan(st, market, None, |b| {
        b.fee = narrow(ev.newFee)?;
        Ok(())
    })?;
    Ok(DirtySet::MarketReprice(rows_of(market)))
}

fn supply(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    add: bool,
) -> Result<DirtySet> {
    let (id, on_behalf, assets, shares) = if add {
        let ev = decode::<events::Supply>(log)?;
        (ev.id, ev.onBehalf, ev.assets, ev.shares)
    } else {
        let ev = decode::<events::Withdraw>(log)?;
        (ev.id, ev.onBehalf, ev.assets, ev.shares)
    };
    let market = lookup(cfg, st, id)?;
    let pos = intern(cfg, st, market, on_behalf)?;
    let cur = st.supply(pos, LOAN_SLOT)?;
    st.set_supply(pos, LOAN_SLOT, add_u128(cur, shares, add)?)?;
    patch_loan(st, market, Some(last_update(log.timestamp)?), |b| {
        b.total_supply_assets = add_u128(b.total_supply_assets, assets, add)?;
        b.total_supply_shares = add_u128(b.total_supply_shares, shares, add)?;
        Ok(())
    })?;
    Ok(positions(&[pos]))
}

fn borrow(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    add: bool,
) -> Result<DirtySet> {
    let (id, on_behalf, assets, shares) = if add {
        let ev = decode::<events::Borrow>(log)?;
        (ev.id, ev.onBehalf, ev.assets, ev.shares)
    } else {
        let ev = decode::<events::Repay>(log)?;
        (ev.id, ev.onBehalf, ev.assets, ev.shares)
    };
    let market = lookup(cfg, st, id)?;
    let pos = intern(cfg, st, market, on_behalf)?;
    let cur = st.debt(pos, LOAN_SLOT)?;
    st.set_debt(pos, LOAN_SLOT, add_u128(cur, shares, add)?)?;
    patch_loan(st, market, Some(last_update(log.timestamp)?), |b| {
        if add {
            b.total_borrow_assets = add_u128(b.total_borrow_assets, assets, true)?;
            b.total_borrow_shares = add_u128(b.total_borrow_shares, shares, true)?;
        } else {
            let ba = U256::from(b.total_borrow_assets);
            b.total_borrow_assets = narrow(ba.saturating_sub(assets))?;
            b.total_borrow_shares = add_u128(b.total_borrow_shares, shares, false)?;
        }
        Ok(())
    })?;
    Ok(positions(&[pos]))
}

fn coll(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    add: bool,
) -> Result<DirtySet> {
    let (id, on_behalf, assets) = if add {
        let ev = decode::<events::SupplyCollateral>(log)?;
        (ev.id, ev.onBehalf, ev.assets)
    } else {
        let ev = decode::<events::WithdrawCollateral>(log)?;
        (ev.id, ev.onBehalf, ev.assets)
    };
    let market = lookup(cfg, st, id)?;
    let pos = intern(cfg, st, market, on_behalf)?;
    let cur = st.supply(pos, COLL_SLOT)?;
    st.set_supply(pos, COLL_SLOT, add_u128(cur, assets, add)?)?;
    Ok(positions(&[pos]))
}

fn liquidate(cfg: &Config, st: &mut dyn StateWriter, log: &DecodedLog<'_>) -> Result<DirtySet> {
    let ev = decode::<events::Liquidate>(log)?;
    let market = lookup(cfg, st, ev.id)?;
    let pos = intern(cfg, st, market, ev.borrower)?;
    let debt = st.debt(pos, LOAN_SLOT)?;
    st.set_debt(pos, LOAN_SLOT, add_u128(debt, ev.repaidShares, false)?)?;
    let coll = st.supply(pos, COLL_SLOT)?;
    st.set_supply(pos, COLL_SLOT, add_u128(coll, ev.seizedAssets, false)?)?;
    if !ev.badDebtShares.is_zero() {
        let left = st.debt(pos, LOAN_SLOT)?;
        st.set_debt(pos, LOAN_SLOT, add_u128(left, ev.badDebtShares, false)?)?;
    }
    patch_loan(st, market, Some(last_update(log.timestamp)?), |b| {
        let ba = U256::from(b.total_borrow_assets);
        b.total_borrow_assets = narrow(ba.saturating_sub(ev.repaidAssets))?;
        b.total_borrow_shares = add_u128(b.total_borrow_shares, ev.repaidShares, false)?;
        if !ev.badDebtAssets.is_zero() {
            b.total_borrow_assets = add_u128(b.total_borrow_assets, ev.badDebtAssets, false)?;
            b.total_supply_assets = add_u128(b.total_supply_assets, ev.badDebtAssets, false)?;
            b.total_borrow_shares = add_u128(b.total_borrow_shares, ev.badDebtShares, false)?;
        }
        Ok(())
    })?;
    let _ = UNMAPPED_ASSET;
    Ok(positions(&[pos]))
}
