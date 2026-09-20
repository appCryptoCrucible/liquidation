//! `Protocol::apply_log` for Liquity V2. Journal-before-write is the writer.

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use liq_protocol::{
    DecodedLog, DirtyPositions, DirtyRows, DirtySet, MarketFlags, MarketRow, MarketSlot,
    ProtocolError, Result, StateWriter,
};
use liq_types::fixed::FixedError;
use liq_types::{MarketId, PositionId, PositionKey};

use crate::config::{BranchConfig, Config, Emitter};
use crate::events::{self, halt, op};
use crate::layout::{BranchRow, TroveCollExtra, TroveDebtExtra, TroveExtra, BOLD_SLOT, COLL_SLOT};

#[inline]
fn decode<E: SolEvent>(log: &DecodedLog<'_>) -> Result<E> {
    E::decode_raw_log(log.topics.iter().copied(), log.data).map_err(|_| ProtocolError::MalformedLog)
}

#[inline]
fn narrow_u128(v: U256) -> Result<u128> {
    u128::try_from(v).map_err(|_| ProtocolError::MalformedLog)
}

#[inline]
fn narrow_u64(v: U256) -> Result<u64> {
    u64::try_from(v).map_err(|_| ProtocolError::MalformedLog)
}

#[inline]
fn last_update(ts: u64) -> Result<u32> {
    u32::try_from(ts).map_err(|_| ProtocolError::Fixed(FixedError::Overflow))
}

#[inline]
fn trove_user(id: U256) -> Result<Address> {
    let b = id.to_be_bytes::<32>();
    let tail = b.get(12..32).ok_or(ProtocolError::MalformedLog)?;
    let mut out = [0u8; 20];
    out.copy_from_slice(tail);
    Ok(Address::from(out))
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
        slot: BOLD_SLOT,
    });
    d.push(MarketSlot {
        market,
        slot: COLL_SLOT,
    });
    d
}

const HALT: &[B256] = &[
    halt::Upgraded::SIGNATURE_HASH,
    halt::AdminChanged::SIGNATURE_HASH,
    halt::Initialized::SIGNATURE_HASH,
];

const BINDING: &[B256] = &[
    events::TroveNFTAddressChanged::SIGNATURE_HASH,
    events::BorrowerOperationsAddressChanged::SIGNATURE_HASH,
    events::BoldTokenAddressChanged::SIGNATURE_HASH,
    events::StabilityPoolAddressChanged::SIGNATURE_HASH,
    events::GasPoolAddressChanged::SIGNATURE_HASH,
    events::CollSurplusPoolAddressChanged::SIGNATURE_HASH,
    events::SortedTrovesAddressChanged::SIGNATURE_HASH,
    events::CollateralRegistryAddressChanged::SIGNATURE_HASH,
    events::ActivePoolAddressChanged::SIGNATURE_HASH,
    events::DefaultPoolAddressChanged::SIGNATURE_HASH,
    events::PriceFeedAddressChanged::SIGNATURE_HASH,
    events::TroveManagerAddressChanged::SIGNATURE_HASH,
];

pub(crate) fn apply_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let Some(em) = cfg.emitter(log.address) else {
        return Err(ProtocolError::UnexpectedLog);
    };
    let topic0 = *log.topics.first().ok_or(ProtocolError::MalformedLog)?;
    if HALT.contains(&topic0) || BINDING.contains(&topic0) {
        if log.block > cfg.pinned_through {
            return Err(ProtocolError::HaltSignal);
        }
        if let Emitter::TroveManager(i) = em {
            let b = cfg.branches.get(i).ok_or(ProtocolError::Internal)?;
            ensure_branch(cfg, b, st)?;
        }
        return Ok(DirtySet::None);
    }
    match em {
        Emitter::TroveManager(i) => {
            let b = cfg.branches.get(i).ok_or(ProtocolError::Internal)?;
            ensure_branch(cfg, b, st)?;
            tm(cfg, b, st, log, topic0)
        }
        Emitter::StabilityPool(i) => {
            let b = cfg.branches.get(i).ok_or(ProtocolError::Internal)?;
            ensure_branch(cfg, b, st)?;
            sp(b, st, log, topic0)
        }
        Emitter::BorrowerOperations(i) => {
            let b = cfg.branches.get(i).ok_or(ProtocolError::Internal)?;
            ensure_branch(cfg, b, st)?;
            bo(b, st, log, topic0)
        }
        Emitter::PriceFeed(i) => {
            let b = cfg.branches.get(i).ok_or(ProtocolError::Internal)?;
            ensure_branch(cfg, b, st)?;
            pf(b, st, log, topic0)
        }
    }
}

fn ensure_branch(cfg: &Config, b: &BranchConfig, st: &mut dyn StateWriter) -> Result<MarketId> {
    match st.markets(b.market) {
        Ok(rows) => {
            if rows.len() < 2 {
                return Err(ProtocolError::SlotMismatch {
                    expected: COLL_SLOT,
                    got: u16::try_from(rows.len()).unwrap_or(u16::MAX),
                });
            }
            Ok(b.market)
        }
        Err(ProtocolError::UnknownMarket(_)) => {
            let mut loan = MarketRow::blank(cfg.bold.asset, cfg.bold.decimals);
            loan.price_feed = cfg.bold.feed;
            loan.flags = MarketFlags::NONE;
            {
                let body: &mut BranchRow = loan.body_mut()?;
                *body = BranchRow {
                    mcr: b.mcr,
                    ccr: b.ccr,
                    penalty_sp: b.penalty_sp,
                    penalty_redist: b.penalty_redist,
                    l_coll: 0,
                    l_bold_debt: 0,
                    sp_bold_deposits: 0,
                    shutdown_time: 0,
                    flags: BranchRow::SEEDED,
                    coll_decimals: b.coll_decimals,
                    coll_asset: b.coll_asset.0,
                    bold_asset: cfg.bold.asset.0,
                    weth_asset: cfg.weth.asset.0,
                    _pad: [0; 4],
                };
            }
            let got = st.push_market(b.market, loan)?;
            if got.slot != BOLD_SLOT {
                return Err(ProtocolError::SlotMismatch {
                    expected: BOLD_SLOT,
                    got: got.slot,
                });
            }
            let mut coll = MarketRow::blank(b.coll_asset, b.coll_decimals);
            coll.price_feed = b.coll_feed;
            let got = st.push_market(b.market, coll)?;
            if got.slot != COLL_SLOT {
                return Err(ProtocolError::SlotMismatch {
                    expected: COLL_SLOT,
                    got: got.slot,
                });
            }
            Ok(b.market)
        }
        Err(e) => Err(e),
    }
}

fn intern_trove(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    trove_id: U256,
) -> Result<PositionId> {
    if trove_id.is_zero() {
        return Err(ProtocolError::MalformedLog);
    }
    let user = trove_user(trove_id)?;
    let id = st.intern(&PositionKey {
        protocol: cfg.protocol,
        market,
        user,
    })?;
    let mut extra = *st.extra(id)?.view::<TroveExtra>()?;
    let cur = U256::from_be_bytes(extra.trove_id);
    if !cur.is_zero() && cur != trove_id {
        return Err(ProtocolError::Internal);
    }
    extra.trove_id = trove_id.to_be_bytes();
    let mut repr = *st.extra(id)?;
    *repr.view_mut::<TroveExtra>()? = extra;
    st.set_extra(id, repr)?;
    Ok(id)
}

/// Pin `onRemoveFromBatch` (TroveManager.sol L1920–2005): `interestBatchManager
/// = 0` and `batchDebtShares = 0` before `TroveUpdated`, then `BatchUpdated`
/// for remaining members. Clearing here (and on close/liquidate) so
/// `BatchUpdated` cannot re-match the leaver.
fn clear_batch_denorm(st: &mut dyn StateWriter, pos: PositionId) -> Result<()> {
    let mut dx = *st.slot_extra(pos, BOLD_SLOT)?.view::<TroveDebtExtra>()?;
    dx.batch_debt_shares = 0;
    dx.batch_recorded_debt = 0;
    dx.batch_total_shares = 0;
    let mut dxr = *st.slot_extra(pos, BOLD_SLOT)?;
    *dxr.view_mut::<TroveDebtExtra>()? = dx;
    st.set_slot_extra(pos, BOLD_SLOT, dxr)?;

    let mut cx = *st.slot_extra(pos, COLL_SLOT)?.view::<TroveCollExtra>()?;
    cx.batch_manager = [0u8; 20];
    cx.batch_management_fee = 0;
    let mut cxr = *st.slot_extra(pos, COLL_SLOT)?;
    *cxr.view_mut::<TroveCollExtra>()? = cx;
    st.set_slot_extra(pos, COLL_SLOT, cxr)?;
    Ok(())
}

#[inline]
fn is_batched(manager: [u8; 20]) -> bool {
    manager != [0u8; 20]
}

fn patch_branch(
    st: &mut dyn StateWriter,
    market: MarketId,
    ts: Option<u32>,
    f: impl FnOnce(&mut BranchRow) -> Result<()>,
) -> Result<()> {
    let at = MarketSlot {
        market,
        slot: BOLD_SLOT,
    };
    let mut row = *st.market(at)?;
    f(row.body_mut::<BranchRow>()?)?;
    if let Some(t) = ts {
        row.last_update = t;
    }
    st.set_market(at, row)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_trove(
    st: &mut dyn StateWriter,
    pos: PositionId,
    debt: U256,
    coll: U256,
    stake: U256,
    rate: U256,
    snap_c: U256,
    snap_d: U256,
    ts: u64,
    status: Option<u8>,
) -> Result<()> {
    st.set_debt(pos, BOLD_SLOT, narrow_u128(debt)?)?;
    st.set_supply(pos, COLL_SLOT, narrow_u128(coll)?)?;
    let mut extra = *st.extra(pos)?.view::<TroveExtra>()?;
    extra.stake = narrow_u128(stake)?;
    extra.annual_interest_rate = narrow_u64(rate)?;
    extra.last_debt_update = last_update(ts)?;
    if let Some(s) = status {
        extra.status = s;
    } else if extra.status == TroveExtra::STATUS_CLOSED_BY_OWNER
        || extra.status == TroveExtra::STATUS_CLOSED_BY_LIQUIDATION
    {
        // Pin `_liquidate` (TroveManager.sol L299–318): TroveUpdated(debt 0)
        // then TroveOperation(liquidate). Duplicate/out-of-order Updated
        // must not force ACTIVE.
    } else if !debt.is_zero() && debt < crate::math::MIN_DEBT {
        extra.status = TroveExtra::STATUS_ZOMBIE;
    } else if extra.status == TroveExtra::STATUS_NONEXISTENT && !debt.is_zero() {
        extra.status = TroveExtra::STATUS_ACTIVE;
    }
    let mut repr = *st.extra(pos)?;
    *repr.view_mut::<TroveExtra>()? = extra;
    st.set_extra(pos, repr)?;

    let mut dx = *st.slot_extra(pos, BOLD_SLOT)?.view::<TroveDebtExtra>()?;
    dx.snapshot_bold = narrow_u128(snap_d)?;
    let mut dxr = *st.slot_extra(pos, BOLD_SLOT)?;
    *dxr.view_mut::<TroveDebtExtra>()? = dx;
    st.set_slot_extra(pos, BOLD_SLOT, dxr)?;

    let mut cx = *st.slot_extra(pos, COLL_SLOT)?.view::<TroveCollExtra>()?;
    cx.snapshot_coll = narrow_u128(snap_c)?;
    let mut cxr = *st.slot_extra(pos, COLL_SLOT)?;
    *cxr.view_mut::<TroveCollExtra>()? = cx;
    st.set_slot_extra(pos, COLL_SLOT, cxr)?;
    Ok(())
}

fn tm(
    cfg: &Config,
    b: &BranchConfig,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    topic0: B256,
) -> Result<DirtySet> {
    if topic0 == events::TroveUpdated::SIGNATURE_HASH {
        let ev = decode::<events::TroveUpdated>(log)?;
        let pos = intern_trove(cfg, st, b.market, ev.troveId)?;
        let cx = *st.slot_extra(pos, COLL_SLOT)?.view::<TroveCollExtra>()?;
        let extra = *st.extra(pos)?.view::<TroveExtra>()?;
        let was_batched = is_batched(cx.batch_manager);
        // `onApplyTroveInterest` for a still-batched trove emits BatchUpdated
        // then TroveUpdated (L1623–1656). BatchUpdated already wrote
        // last_debt_update = this timestamp; do not unbatch. Leave-batch
        // (`onRemoveFromBatch` L1944) emits TroveUpdated first.
        let batch_already_this_ts = extra.last_debt_update == last_update(log.timestamp)?;
        write_trove(
            st,
            pos,
            ev.debt,
            ev.coll,
            ev.stake,
            ev.annualInterestRate,
            ev.snapshotOfTotalCollRedist,
            ev.snapshotOfTotalDebtRedist,
            log.timestamp,
            None,
        )?;
        if was_batched && !batch_already_this_ts {
            clear_batch_denorm(st, pos)?;
        }
        return Ok(positions(&[pos]));
    }
    if topic0 == events::BatchedTroveUpdated::SIGNATURE_HASH {
        let ev = decode::<events::BatchedTroveUpdated>(log)?;
        let pos = intern_trove(cfg, st, b.market, ev.troveId)?;
        st.set_supply(pos, COLL_SLOT, narrow_u128(ev.coll)?)?;
        let mut extra = *st.extra(pos)?.view::<TroveExtra>()?;
        extra.stake = narrow_u128(ev.stake)?;
        extra.last_debt_update = last_update(log.timestamp)?;
        if extra.status == TroveExtra::STATUS_NONEXISTENT {
            extra.status = TroveExtra::STATUS_ACTIVE;
        }
        let mut repr = *st.extra(pos)?;
        *repr.view_mut::<TroveExtra>()? = extra;
        st.set_extra(pos, repr)?;

        let mut dx = *st.slot_extra(pos, BOLD_SLOT)?.view::<TroveDebtExtra>()?;
        dx.snapshot_bold = narrow_u128(ev.snapshotOfTotalDebtRedist)?;
        dx.batch_debt_shares = narrow_u128(ev.batchDebtShares)?;
        let mut dxr = *st.slot_extra(pos, BOLD_SLOT)?;
        *dxr.view_mut::<TroveDebtExtra>()? = dx;
        st.set_slot_extra(pos, BOLD_SLOT, dxr)?;

        let mut cx = *st.slot_extra(pos, COLL_SLOT)?.view::<TroveCollExtra>()?;
        cx.snapshot_coll = narrow_u128(ev.snapshotOfTotalCollRedist)?;
        cx.batch_manager = *ev.interestBatchManager.as_ref();
        let mut cxr = *st.slot_extra(pos, COLL_SLOT)?;
        *cxr.view_mut::<TroveCollExtra>()? = cx;
        st.set_slot_extra(pos, COLL_SLOT, cxr)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == events::TroveOperation::SIGNATURE_HASH {
        let ev = decode::<events::TroveOperation>(log)?;
        let pos = intern_trove(cfg, st, b.market, ev.troveId)?;
        let mut extra = *st.extra(pos)?.view::<TroveExtra>()?;
        extra.status = match ev.operation {
            op::OPEN_TROVE | op::OPEN_TROVE_AND_JOIN_BATCH => TroveExtra::STATUS_ACTIVE,
            op::CLOSE_TROVE => TroveExtra::STATUS_CLOSED_BY_OWNER,
            op::LIQUIDATE => TroveExtra::STATUS_CLOSED_BY_LIQUIDATION,
            _ => extra.status,
        };
        if extra.status == TroveExtra::STATUS_NONEXISTENT {
            extra.status = TroveExtra::STATUS_ACTIVE;
        }
        extra.annual_interest_rate = narrow_u64(ev.annualInterestRate)?;
        let mut repr = *st.extra(pos)?;
        *repr.view_mut::<TroveExtra>()? = extra;
        st.set_extra(pos, repr)?;
        if ev.operation == op::REMOVE_FROM_BATCH
            || ev.operation == op::CLOSE_TROVE
            || ev.operation == op::LIQUIDATE
        {
            clear_batch_denorm(st, pos)?;
        }
        return Ok(positions(&[pos]));
    }
    if topic0 == events::BatchUpdated::SIGNATURE_HASH {
        let ev = decode::<events::BatchUpdated>(log)?;
        return batch_updated(st, b.market, ev, log.timestamp);
    }
    if topic0 == events::Liquidation::SIGNATURE_HASH {
        let ev = decode::<events::Liquidation>(log)?;
        patch_branch(st, b.market, Some(last_update(log.timestamp)?), |body| {
            body.l_coll = narrow_u128(ev.lColl)?;
            body.l_bold_debt = narrow_u128(ev.lBoldDebt)?;
            Ok(())
        })?;
        return Ok(DirtySet::MarketAccrual(rows_of(b.market)));
    }
    if topic0 == events::Redemption::SIGNATURE_HASH
        || topic0 == events::RedemptionFeePaidToTrove::SIGNATURE_HASH
    {
        return Ok(DirtySet::None);
    }
    Ok(DirtySet::None)
}

fn batch_updated(
    st: &mut dyn StateWriter,
    market: MarketId,
    ev: events::BatchUpdated,
    ts: u64,
) -> Result<DirtySet> {
    let n = st.positions_len();
    let mut ids = DirtyPositions::new();
    let rate = narrow_u64(ev.annualInterestRate)?;
    let fee = narrow_u64(ev.annualManagementFee)?;
    let rec = narrow_u128(ev.debt)?;
    let shares = narrow_u128(ev.totalDebtShares)?;
    let lu = last_update(ts)?;
    for i in 0..n {
        let pid = PositionId(i);
        let key = st.position_key(pid)?;
        if key.market != market {
            continue;
        }
        let cx = *st.slot_extra(pid, COLL_SLOT)?.view::<TroveCollExtra>()?;
        if Address::from(cx.batch_manager) != ev.interestBatchManager {
            continue;
        }
        let mut extra = *st.extra(pid)?.view::<TroveExtra>()?;
        extra.annual_interest_rate = rate;
        extra.last_debt_update = lu;
        let mut repr = *st.extra(pid)?;
        *repr.view_mut::<TroveExtra>()? = extra;
        st.set_extra(pid, repr)?;

        let mut dx = *st.slot_extra(pid, BOLD_SLOT)?.view::<TroveDebtExtra>()?;
        dx.batch_recorded_debt = rec;
        dx.batch_total_shares = shares;
        let mut dxr = *st.slot_extra(pid, BOLD_SLOT)?;
        *dxr.view_mut::<TroveDebtExtra>()? = dx;
        st.set_slot_extra(pid, BOLD_SLOT, dxr)?;

        let mut c2 = cx;
        c2.batch_management_fee = fee;
        let mut cxr = *st.slot_extra(pid, COLL_SLOT)?;
        *cxr.view_mut::<TroveCollExtra>()? = c2;
        st.set_slot_extra(pid, COLL_SLOT, cxr)?;
        ids.push(pid);
    }
    if ids.is_empty() {
        Ok(DirtySet::None)
    } else {
        Ok(DirtySet::Positions(ids))
    }
}

fn sp(
    b: &BranchConfig,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    topic0: B256,
) -> Result<DirtySet> {
    if topic0 == events::StabilityPoolBoldBalanceUpdated::SIGNATURE_HASH {
        let ev = decode::<events::StabilityPoolBoldBalanceUpdated>(log)?;
        patch_branch(st, b.market, Some(last_update(log.timestamp)?), |body| {
            body.sp_bold_deposits = narrow_u128(ev.newBalance)?;
            Ok(())
        })?;
        return Ok(DirtySet::MarketAccrual(rows_of(b.market)));
    }
    Ok(DirtySet::None)
}

fn bo(
    b: &BranchConfig,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    topic0: B256,
) -> Result<DirtySet> {
    if topic0 == events::ShutDown::SIGNATURE_HASH {
        let _ = decode::<events::ShutDown>(log)?;
        let lu = last_update(log.timestamp)?;
        patch_branch(st, b.market, Some(lu), |body| {
            body.shutdown_time = lu;
            Ok(())
        })?;
        return Ok(DirtySet::MarketAccrual(rows_of(b.market)));
    }
    Ok(DirtySet::None)
}

fn pf(
    b: &BranchConfig,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    topic0: B256,
) -> Result<DirtySet> {
    if topic0 == events::ShutDownFromOracleFailure::SIGNATURE_HASH {
        let _ = decode::<events::ShutDownFromOracleFailure>(log)?;
        let lu = last_update(log.timestamp)?;
        patch_branch(st, b.market, Some(lu), |body| {
            body.shutdown_time = lu;
            Ok(())
        })?;
        return Ok(DirtySet::MarketAccrual(rows_of(b.market)));
    }
    Ok(DirtySet::None)
}
