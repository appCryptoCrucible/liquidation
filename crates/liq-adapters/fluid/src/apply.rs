//! `Protocol::apply_log` for Fluid vaults. Journal-before-write is the writer.

use alloy_primitives::{Address, B256, I256, U256};
use alloy_sol_types::SolEvent;
use liq_protocol::{
    DecodedLog, DirtyPositions, DirtyRows, DirtySet, MarketFlags, MarketRow, MarketSlot,
    ProtocolError, Result, StateWriter,
};
use liq_types::fixed::FixedError;
use liq_types::{MarketId, PositionId, PositionKey};

use crate::config::{Config, Emitter, VaultPin};
use crate::events::{admin, factory, halt, vault};
use bytemuck::Zeroable;

use crate::layout::{CatalogEntry, VaultExtra, VaultRow, CATALOG_ASSET, SLOT0, UNMAPPED_ASSET};
use crate::math::{
    addr20, addr_from, market_from_vault_id, pack_penalty_from_event, pack_threshold_from_event,
    tick_from_raw, to_raw, NATIVE_TOKEN, TICK_STATUS_LIQUIDATED, TICK_STATUS_PERFECT,
};

#[inline]
fn decode<E: SolEvent>(log: &DecodedLog<'_>) -> Result<E> {
    E::decode_raw_log(log.topics.iter().copied(), log.data).map_err(|_| ProtocolError::MalformedLog)
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
fn dirty_slot(market: MarketId, slot: u16) -> DirtyRows {
    let mut d = DirtyRows::new();
    d.push(MarketSlot { market, slot });
    d
}

fn intern_vault(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    vault: Address,
) -> Result<PositionId> {
    st.intern(&PositionKey {
        protocol: cfg.protocol,
        market,
        user: vault,
    })
}

fn extra(st: &dyn StateWriter, pos: PositionId) -> Result<VaultExtra> {
    Ok(*st.extra(pos)?.view::<VaultExtra>()?)
}

fn set_extra(st: &mut dyn StateWriter, pos: PositionId, e: VaultExtra) -> Result<()> {
    let mut repr = *st.extra(pos)?;
    *repr.view_mut::<VaultExtra>()? = e;
    st.set_extra(pos, repr)
}

fn lookup_vault(cfg: &Config, st: &dyn StateWriter, vault: Address) -> Result<Option<MarketId>> {
    let rows = match st.markets(cfg.catalog) {
        Ok(r) => r,
        Err(ProtocolError::UnknownMarket(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    for row in rows {
        let e: &CatalogEntry = row.body()?;
        if addr_from(e.vault) == vault {
            return Ok(Some(MarketId(e.market)));
        }
    }
    Ok(None)
}

fn emitter(cfg: &Config, st: &dyn StateWriter, address: Address) -> Result<Option<Emitter>> {
    if address == cfg.factory {
        return Ok(Some(Emitter::Factory));
    }
    if let Some(m) = lookup_vault(cfg, st, address)? {
        return Ok(Some(Emitter::Vault(m)));
    }
    if cfg.is_vault(address) {
        return Ok(Some(Emitter::Pending));
    }
    Ok(None)
}

fn derive_flags(v: &VaultRow) -> MarketFlags {
    if v.flags & VaultRow::PRICED == 0 {
        MarketFlags::UNPRICED
    } else {
        MarketFlags::NONE
    }
}

fn add_i256(cur: u128, delta: I256) -> Result<u128> {
    let cur = I256::try_from(cur).map_err(|_| ProtocolError::Fixed(FixedError::Overflow))?;
    let n = cur
        .checked_add(delta)
        .ok_or(ProtocolError::Fixed(FixedError::Overflow))?;
    if n.is_negative() {
        return Err(ProtocolError::Fixed(FixedError::Underflow));
    }
    u128::try_from(n).map_err(|_| ProtocolError::Fixed(FixedError::Overflow))
}

fn sub_u256(cur: u128, amt: U256) -> Result<u128> {
    let c = U256::from(cur);
    let n = c
        .checked_sub(amt)
        .ok_or(ProtocolError::Fixed(FixedError::Underflow))?;
    u128::try_from(n).map_err(|_| ProtocolError::MalformedLog)
}

type TokenDec = (Address, u8);

fn pin_tokens(pin: &VaultPin) -> Result<(Vec<TokenDec>, u8, u8)> {
    let mut col = vec![(pin.supply0, pin.supply_decimals0)];
    if pin.supply1 != Address::ZERO {
        col.push((pin.supply1, pin.supply_decimals1));
    }
    let mut debt = vec![(pin.borrow0, pin.borrow_decimals0)];
    if pin.borrow1 != Address::ZERO {
        debt.push((pin.borrow1, pin.borrow_decimals1));
    }
    let n_col = u8::try_from(col.len()).map_err(|_| ProtocolError::Internal)?;
    let n_debt = u8::try_from(debt.len()).map_err(|_| ProtocolError::Internal)?;
    col.extend(debt);
    Ok((col, n_col, n_debt))
}

fn priced_tokens(cfg: &Config, tokens: &[TokenDec]) -> bool {
    tokens.iter().all(|(t, _)| {
        *t != Address::ZERO && *t != NATIVE_TOKEN && cfg.asset_by_underlying(*t).is_some()
    })
}

fn vault_body_from_pin(pin: &VaultPin, n_col: u8, n_debt: u8, priced: bool) -> VaultRow {
    let mut v = VaultRow::zeroed(); // bytemuck::Zeroable
    v.vault = addr20(pin.vault);
    v.oracle = addr20(pin.oracle);
    v.supply0 = addr20(pin.supply0);
    v.supply1 = addr20(pin.supply1);
    v.borrow0 = addr20(pin.borrow0);
    v.borrow1 = addr20(pin.borrow1);
    v.vault_id = pin.vault_id;
    v.vault_type = pin.vault_type;
    v.liq_threshold = pin.liq_threshold;
    v.liq_max_limit = pin.liq_max_limit;
    v.liq_penalty = pin.liq_penalty;
    v.n_col = n_col;
    v.n_debt = n_debt;
    v.flags = VaultRow::VIEWED;
    if priced {
        v.flags |= VaultRow::PRICED;
    }
    v
}

fn push_vault_slots(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    pin: &VaultPin,
    ts: u32,
) -> Result<DirtyRows> {
    let (tokens, n_col, n_debt) = pin_tokens(pin)?;
    let priced = priced_tokens(cfg, &tokens);
    let body = vault_body_from_pin(pin, n_col, n_debt, priced);
    let mut dirty = DirtyRows::new();
    for (i, (token, decimals)) in tokens.iter().enumerate() {
        let tok = cfg.asset_by_underlying(*token);
        let (asset, dec, feed) = match tok {
            Some(t) => (t.asset, t.decimals, t.feed),
            None => (UNMAPPED_ASSET, *decimals, liq_protocol::FeedId(0)),
        };
        let mut row = MarketRow::blank(asset, dec);
        row.price_feed = feed;
        row.last_update = ts;
        *row.body_mut::<VaultRow>()? = body;
        row.flags = derive_flags(&body);
        let at = st.push_market(market, row)?;
        if u16::try_from(i).map_err(|_| ProtocolError::Internal)? != at.slot {
            return Err(ProtocolError::SlotMismatch {
                expected: u16::try_from(i).map_err(|_| ProtocolError::Internal)?,
                got: at.slot,
            });
        }
        dirty.push(at);
    }
    Ok(dirty)
}

fn ensure_vault(
    cfg: &Config,
    st: &mut dyn StateWriter,
    vault_addr: Address,
    vault_id: u32,
    ts: u32,
) -> Result<(MarketId, DirtyRows)> {
    if let Some(m) = lookup_vault(cfg, st, vault_addr)? {
        return Ok((m, DirtyRows::new()));
    }
    let market = market_from_vault_id(vault_id)?;
    let n = match st.markets(cfg.catalog) {
        Ok(r) => r.len(),
        Err(ProtocolError::UnknownMarket(_)) => 0,
        Err(e) => return Err(e),
    };
    let cat_slot = u16::try_from(n).map_err(|_| ProtocolError::MalformedLog)?;
    let mut cat = MarketRow::blank(CATALOG_ASSET, 0);
    {
        let e: &mut CatalogEntry = cat.body_mut()?;
        e.vault = addr20(vault_addr);
        e.vault_id = vault_id;
        e.market = market.0;
    }
    st.push_market(cfg.catalog, cat)?;
    let mut dirty = dirty_slot(cfg.catalog, cat_slot);
    if let Some(pin) = cfg.pin_of(vault_addr) {
        if pin.vault_id != vault_id {
            return Err(ProtocolError::OracleSourceMismatch);
        }
        dirty.extend(push_vault_slots(cfg, st, market, pin, ts)?);
    } else {
        let mut row = MarketRow::blank(UNMAPPED_ASSET, 0);
        row.last_update = ts;
        row.flags = MarketFlags::UNPRICED;
        {
            let v: &mut VaultRow = row.body_mut()?;
            v.vault = addr20(vault_addr);
            v.vault_id = vault_id;
        }
        st.push_market(market, row)?;
        dirty.push(MarketSlot {
            market,
            slot: SLOT0,
        });
    }
    let _ = intern_vault(cfg, st, market, vault_addr)?;
    Ok((market, dirty))
}

fn patch_vault(
    st: &mut dyn StateWriter,
    market: MarketId,
    ts: Option<u32>,
    f: impl FnOnce(&mut VaultRow) -> Result<()>,
) -> Result<DirtyRows> {
    let rows = st.markets(market)?;
    let n = u16::try_from(rows.len()).map_err(|_| ProtocolError::Internal)?;
    if n == 0 {
        return Err(ProtocolError::UnknownMarket(market));
    }
    let mut first = *st.market(MarketSlot {
        market,
        slot: SLOT0,
    })?;
    f(first.body_mut::<VaultRow>()?)?;
    if let Some(t) = ts {
        first.last_update = t;
    }
    let body = *first.body::<VaultRow>()?;
    let flags = derive_flags(&body);
    first.flags = flags;
    st.set_market(
        MarketSlot {
            market,
            slot: SLOT0,
        },
        first,
    )?;
    let mut dirty = dirty_slot(market, SLOT0);
    for slot in 1..n {
        let at = MarketSlot { market, slot };
        let mut row = *st.market(at)?;
        *row.body_mut::<VaultRow>()? = body;
        row.flags = flags;
        if let Some(t) = ts {
            row.last_update = t;
        }
        st.set_market(at, row)?;
        dirty.push(at);
    }
    Ok(dirty)
}

fn vault_row(st: &dyn StateWriter, market: MarketId) -> Result<VaultRow> {
    Ok(*st
        .market(MarketSlot {
            market,
            slot: SLOT0,
        })?
        .body::<VaultRow>()?)
}

fn refresh_top_tick(st: &mut dyn StateWriter, pos: PositionId, market: MarketId) -> Result<()> {
    let v = vault_row(st, market)?;
    let mut e = extra(st, pos)?;
    if v.n_nfts != 1 || v.flags & VaultRow::EX_KNOWN == 0 {
        e.flags &= !VaultExtra::TOP_KNOWN;
        return set_extra(st, pos, e);
    }
    let col = st.supply(pos, SLOT0)?;
    let dslot = v.debt_slot();
    let debt = st.debt(pos, dslot)?;
    if col == 0 || debt == 0 {
        e.flags &= !VaultExtra::TOP_KNOWN;
        return set_extra(st, pos, e);
    }
    let col_raw = to_raw(U256::from(col), U256::from(v.supply_ex_price))?;
    let debt_raw = to_raw(U256::from(debt), U256::from(v.borrow_ex_price))?;
    e.top_tick = tick_from_raw(col_raw, debt_raw)?;
    e.tick_status = TICK_STATUS_PERFECT;
    e.flags |= VaultExtra::TOP_KNOWN;
    set_extra(st, pos, e)
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
    let Some(em) = emitter(cfg, st, log.address)? else {
        return Err(ProtocolError::UnexpectedLog);
    };
    let topic0 = *log.topics.first().ok_or(ProtocolError::MalformedLog)?;
    if HALT.contains(&topic0) {
        return if log.block <= cfg.pinned_through {
            Ok(DirtySet::None)
        } else {
            Err(ProtocolError::HaltSignal)
        };
    }
    match em {
        Emitter::Factory => factory_log(cfg, st, log, topic0),
        Emitter::Vault(m) => vault_log(cfg, st, log, m, topic0),
        Emitter::Pending => pending_log(cfg, st, log, topic0),
    }
}

fn pending_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    topic0: B256,
) -> Result<DirtySet> {
    let Some(pin) = cfg.pin_of(log.address) else {
        return Ok(DirtySet::None);
    };
    let ts = last_update(log.timestamp)?;
    let (m, mut d) = ensure_vault(cfg, st, pin.vault, pin.vault_id, ts)?;
    let inner = vault_log(cfg, st, log, m, topic0)?;
    if !d.is_empty() {
        match inner {
            DirtySet::None => Ok(DirtySet::MarketReprice(d)),
            DirtySet::MarketReprice(mut r) => {
                d.append(&mut r);
                Ok(DirtySet::MarketReprice(d))
            }
            DirtySet::MarketAccrual(mut r) => {
                d.append(&mut r);
                Ok(DirtySet::MarketReprice(d))
            }
            other => Ok(other),
        }
    } else {
        Ok(inner)
    }
}

fn factory_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    topic0: B256,
) -> Result<DirtySet> {
    if topic0 == factory::VaultDeployed::SIGNATURE_HASH {
        let ev = decode::<factory::VaultDeployed>(log)?;
        let vault_id = u32::try_from(ev.vaultId).map_err(|_| ProtocolError::MalformedLog)?;
        let ts = last_update(log.timestamp)?;
        let (_, dirty) = ensure_vault(cfg, st, ev.vault, vault_id, ts)?;
        return Ok(DirtySet::MarketReprice(dirty));
    }
    if topic0 == factory::NewPositionMinted::SIGNATURE_HASH {
        let ev = decode::<factory::NewPositionMinted>(log)?;
        let Some(market) = lookup_vault(cfg, st, ev.vault)? else {
            return Ok(DirtySet::None);
        };
        let dirty = patch_vault(st, market, Some(last_update(log.timestamp)?), |v| {
            v.n_nfts = v
                .n_nfts
                .checked_add(1)
                .ok_or(ProtocolError::Fixed(FixedError::Overflow))?;
            Ok(())
        })?;
        let pos = intern_vault(cfg, st, market, ev.vault)?;
        refresh_top_tick(st, pos, market)?;
        return Ok(DirtySet::MarketReprice(dirty));
    }
    if topic0 == factory::Transfer::SIGNATURE_HASH
        || topic0 == factory::LogSetDeployer::SIGNATURE_HASH
        || topic0 == factory::LogSetGlobalAuth::SIGNATURE_HASH
        || topic0 == factory::LogSetVaultAuth::SIGNATURE_HASH
        || topic0 == factory::LogSetVaultDeploymentLogic::SIGNATURE_HASH
    {
        decode_factory_known(log, topic0)?;
        return Ok(DirtySet::None);
    }
    Err(ProtocolError::UnexpectedLog)
}

fn decode_factory_known(log: &DecodedLog<'_>, topic0: B256) -> Result<()> {
    if topic0 == factory::Transfer::SIGNATURE_HASH {
        let _ = decode::<factory::Transfer>(log)?;
        return Ok(());
    }
    if topic0 == factory::LogSetDeployer::SIGNATURE_HASH {
        let _ = decode::<factory::LogSetDeployer>(log)?;
        return Ok(());
    }
    if topic0 == factory::LogSetGlobalAuth::SIGNATURE_HASH {
        let _ = decode::<factory::LogSetGlobalAuth>(log)?;
        return Ok(());
    }
    if topic0 == factory::LogSetVaultAuth::SIGNATURE_HASH {
        let _ = decode::<factory::LogSetVaultAuth>(log)?;
        return Ok(());
    }
    if topic0 == factory::LogSetVaultDeploymentLogic::SIGNATURE_HASH {
        let _ = decode::<factory::LogSetVaultDeploymentLogic>(log)?;
        return Ok(());
    }
    Err(ProtocolError::UnexpectedLog)
}

fn vault_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    market: MarketId,
    topic0: B256,
) -> Result<DirtySet> {
    if topic0 == vault::LogOperate::SIGNATURE_HASH {
        return operate(cfg, st, log, market);
    }
    if topic0 == vault::LogUpdateExchangePrice::SIGNATURE_HASH {
        let ev = decode::<vault::LogUpdateExchangePrice>(log)?;
        let supply = u128::try_from(ev.supplyExPrice_).map_err(|_| ProtocolError::MalformedLog)?;
        let borrow = u128::try_from(ev.borrowExPrice_).map_err(|_| ProtocolError::MalformedLog)?;
        if supply == 0 || borrow == 0 {
            return Err(ProtocolError::MalformedLog);
        }
        let dirty = patch_vault(st, market, Some(last_update(log.timestamp)?), |v| {
            v.supply_ex_price = supply;
            v.borrow_ex_price = borrow;
            v.flags |= VaultRow::EX_KNOWN;
            Ok(())
        })?;
        let v = vault_row(st, market)?;
        let pos = intern_vault(cfg, st, market, addr_from(v.vault))?;
        refresh_top_tick(st, pos, market)?;
        return Ok(DirtySet::MarketAccrual(dirty));
    }
    if topic0 == vault::LogLiquidate::SIGNATURE_HASH {
        return liquidate_log(cfg, st, log, market);
    }
    if topic0 == vault::LogAbsorb::SIGNATURE_HASH {
        let ev = decode::<vault::LogAbsorb>(log)?;
        let v = vault_row(st, market)?;
        let pos = intern_vault(cfg, st, market, addr_from(v.vault))?;
        let mut e = extra(st, pos)?;
        e.absorbed_col_raw =
            u128::try_from(ev.colAbsorbedRaw_).map_err(|_| ProtocolError::MalformedLog)?;
        e.absorbed_debt_raw =
            u128::try_from(ev.debtAbsorbedRaw_).map_err(|_| ProtocolError::MalformedLog)?;
        set_extra(st, pos, e)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == vault::LogRebalance::SIGNATURE_HASH {
        let _ = decode::<vault::LogRebalance>(log)?;
        return Ok(DirtySet::None);
    }
    if topic0 == admin::LogUpdateLiquidationThreshold::SIGNATURE_HASH {
        let ev = decode::<admin::LogUpdateLiquidationThreshold>(log)?;
        let packed = pack_threshold_from_event(ev.liquidationThreshold_)?;
        let dirty = patch_vault(st, market, Some(last_update(log.timestamp)?), |v| {
            v.liq_threshold = packed;
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(dirty));
    }
    if topic0 == admin::LogUpdateLiquidationMaxLimit::SIGNATURE_HASH {
        let ev = decode::<admin::LogUpdateLiquidationMaxLimit>(log)?;
        let packed = pack_threshold_from_event(ev.liquidationMaxLimit_)?;
        let dirty = patch_vault(st, market, Some(last_update(log.timestamp)?), |v| {
            v.liq_max_limit = packed;
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(dirty));
    }
    if topic0 == admin::LogUpdateLiquidationPenalty::SIGNATURE_HASH {
        let ev = decode::<admin::LogUpdateLiquidationPenalty>(log)?;
        let packed = pack_penalty_from_event(ev.liquidationPenalty_)?;
        let dirty = patch_vault(st, market, Some(last_update(log.timestamp)?), |v| {
            v.liq_penalty = packed;
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(dirty));
    }
    if topic0 == admin::LogUpdateOracle::SIGNATURE_HASH {
        if log.block > cfg.pinned_through {
            return Err(ProtocolError::HaltSignal);
        }
        let ev = decode::<admin::LogUpdateOracle>(log)?;
        let dirty = patch_vault(st, market, Some(last_update(log.timestamp)?), |v| {
            v.oracle = addr20(ev.newOracle_);
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(dirty));
    }
    if topic0 == admin::LogUpdateCoreSettings::SIGNATURE_HASH {
        let ev = decode::<admin::LogUpdateCoreSettings>(log)?;
        let thr = pack_threshold_from_event(ev.liquidationThreshold_)?;
        let max = pack_threshold_from_event(ev.liquidationMaxLimit_)?;
        let pen = pack_penalty_from_event(ev.liquidationPenalty_)?;
        let dirty = patch_vault(st, market, Some(last_update(log.timestamp)?), |v| {
            v.liq_threshold = thr;
            v.liq_max_limit = max;
            v.liq_penalty = pen;
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(dirty));
    }
    if is_admin_none(topic0) {
        decode_admin_none(log, topic0)?;
        return Ok(DirtySet::None);
    }
    Err(ProtocolError::UnexpectedLog)
}

fn is_admin_none(topic0: B256) -> bool {
    topic0 == admin::LogUpdateSupplyRateMagnifier::SIGNATURE_HASH
        || topic0 == admin::LogUpdateBorrowRateMagnifier::SIGNATURE_HASH
        || topic0 == admin::LogUpdateCollateralFactor::SIGNATURE_HASH
        || topic0 == admin::LogUpdateWithdrawGap::SIGNATURE_HASH
        || topic0 == admin::LogUpdateBorrowFee::SIGNATURE_HASH
        || topic0 == admin::LogUpdateRebalancer::SIGNATURE_HASH
        || topic0 == admin::LogRescueFunds::SIGNATURE_HASH
        || topic0 == admin::LogAbsorbDustDebt::SIGNATURE_HASH
}

fn decode_admin_none(log: &DecodedLog<'_>, topic0: B256) -> Result<()> {
    if topic0 == admin::LogUpdateSupplyRateMagnifier::SIGNATURE_HASH {
        let _ = decode::<admin::LogUpdateSupplyRateMagnifier>(log)?;
        return Ok(());
    }
    if topic0 == admin::LogUpdateBorrowRateMagnifier::SIGNATURE_HASH {
        let _ = decode::<admin::LogUpdateBorrowRateMagnifier>(log)?;
        return Ok(());
    }
    if topic0 == admin::LogUpdateCollateralFactor::SIGNATURE_HASH {
        let _ = decode::<admin::LogUpdateCollateralFactor>(log)?;
        return Ok(());
    }
    if topic0 == admin::LogUpdateWithdrawGap::SIGNATURE_HASH {
        let _ = decode::<admin::LogUpdateWithdrawGap>(log)?;
        return Ok(());
    }
    if topic0 == admin::LogUpdateBorrowFee::SIGNATURE_HASH {
        let _ = decode::<admin::LogUpdateBorrowFee>(log)?;
        return Ok(());
    }
    if topic0 == admin::LogUpdateRebalancer::SIGNATURE_HASH {
        let _ = decode::<admin::LogUpdateRebalancer>(log)?;
        return Ok(());
    }
    if topic0 == admin::LogRescueFunds::SIGNATURE_HASH {
        let _ = decode::<admin::LogRescueFunds>(log)?;
        return Ok(());
    }
    if topic0 == admin::LogAbsorbDustDebt::SIGNATURE_HASH {
        let _ = decode::<admin::LogAbsorbDustDebt>(log)?;
        return Ok(());
    }
    Err(ProtocolError::UnexpectedLog)
}

fn operate(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    market: MarketId,
) -> Result<DirtySet> {
    let ev = decode::<vault::LogOperate>(log)?;
    let v = vault_row(st, market)?;
    let pos = intern_vault(cfg, st, market, addr_from(v.vault))?;
    let col = add_i256(st.supply(pos, SLOT0)?, ev.colAmt_)?;
    st.set_supply(pos, SLOT0, col)?;
    let dslot = v.debt_slot();
    let debt = add_i256(st.debt(pos, dslot)?, ev.debtAmt_)?;
    st.set_debt(pos, dslot, debt)?;
    refresh_top_tick(st, pos, market)?;
    Ok(positions(&[pos]))
}

fn liquidate_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    market: MarketId,
) -> Result<DirtySet> {
    let ev = decode::<vault::LogLiquidate>(log)?;
    let v = vault_row(st, market)?;
    let pos = intern_vault(cfg, st, market, addr_from(v.vault))?;
    let col = sub_u256(st.supply(pos, SLOT0)?, ev.colAmt_)?;
    st.set_supply(pos, SLOT0, col)?;
    let dslot = v.debt_slot();
    let debt = sub_u256(st.debt(pos, dslot)?, ev.debtAmt_)?;
    st.set_debt(pos, dslot, debt)?;
    let mut e = extra(st, pos)?;
    e.tick_status = TICK_STATUS_LIQUIDATED;
    set_extra(st, pos, e)?;
    refresh_top_tick(st, pos, market)?;
    Ok(positions(&[pos]))
}
