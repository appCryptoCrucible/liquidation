//! `Protocol::apply_log` for Euler V2 EVK. Journal-before-write is the writer.

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use liq_protocol::{
    DecodedLog, DirtyPositions, DirtyRows, DirtySet, MarketFlags, MarketRow, MarketSlot,
    ProtocolError, Result, StateWriter,
};
use liq_types::fixed::FixedError;
use liq_types::{MarketId, PositionId, PositionKey};

use crate::config::{Config, Emitter};
use crate::events::{self, evc, halt};
use crate::layout::{
    CatalogEntry, CollRow, UserExtra, VaultRow, CATALOG_ASSET, DEBT_SLOT, INITIAL_HOOKED_OPS,
    OP_LIQUIDATE, UNMAPPED_ASSET,
};
use crate::math::{
    accrue_accumulator, addr20, addr_from, assets_to_owed, current_owed, last_update,
    set_vault_acc, to_assets_up, u256_from_limbs, u256_to_limbs, vault_acc, DEFAULT_INTEREST_FEE,
    INITIAL_INTEREST_ACCUMULATOR,
};

type SmallVec8<T> = smallvec::SmallVec<[T; 8]>;

#[inline]
fn decode<E: SolEvent>(log: &DecodedLog<'_>) -> Result<E> {
    E::decode_raw_log(log.topics.iter().copied(), log.data).map_err(|_| ProtocolError::MalformedLog)
}

#[inline]
fn narrow<T: TryFrom<U256>>(v: U256) -> Result<T> {
    T::try_from(v).map_err(|_| ProtocolError::MalformedLog)
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

fn discovered_market(cfg: &Config, st: &dyn StateWriter, vault: Address) -> Result<MarketId> {
    if let Some(id) = cfg.interned_id(vault) {
        return Ok(id);
    }
    let mut n = 0u32;
    match st.markets(cfg.catalog) {
        Ok(rows) => {
            for row in rows {
                let e: &CatalogEntry = row.body()?;
                if cfg.interned_id(addr_from(e.vault)).is_none() {
                    n = n.checked_add(1).ok_or(FixedError::Overflow)?;
                }
            }
        }
        Err(ProtocolError::UnknownMarket(_)) => {}
        Err(e) => return Err(e),
    }
    Ok(MarketId(
        cfg.first_market
            .0
            .checked_add(n)
            .ok_or(FixedError::Overflow)?,
    ))
}

fn emitter(cfg: &Config, st: &dyn StateWriter, address: Address) -> Result<Option<Emitter>> {
    if address == cfg.factory {
        return Ok(Some(Emitter::Factory));
    }
    if address == cfg.evc {
        return Ok(Some(Emitter::Evc));
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
    let mut f = 0u8;
    if v.flags & VaultRow::PRICED == 0 {
        f |= MarketFlags::UNPRICED.0;
    }
    if v.flags & VaultRow::HOOKS_KNOWN != 0 && v.hooked_ops & OP_LIQUIDATE != 0 {
        f |= MarketFlags::PAUSED.0;
    }
    MarketFlags(f)
}

fn trailing3(data: &[u8]) -> Result<(Address, Address, Address)> {
    let a: [u8; 20] = data
        .get(..20)
        .and_then(|s| s.try_into().ok())
        .ok_or(ProtocolError::MalformedLog)?;
    let o: [u8; 20] = data
        .get(20..40)
        .and_then(|s| s.try_into().ok())
        .ok_or(ProtocolError::MalformedLog)?;
    let u: [u8; 20] = data
        .get(40..60)
        .and_then(|s| s.try_into().ok())
        .ok_or(ProtocolError::MalformedLog)?;
    Ok((Address::from(a), Address::from(o), Address::from(u)))
}

fn ensure_vault(
    cfg: &Config,
    st: &mut dyn StateWriter,
    vault: Address,
    underlying: Address,
    oracle: Address,
    unit: Address,
    ts: u32,
) -> Result<(MarketId, DirtyRows)> {
    if let Some(m) = lookup_vault(cfg, st, vault)? {
        let at = MarketSlot {
            market: m,
            slot: DEBT_SLOT,
        };
        let mut row = *st.market(at)?;
        {
            let v: &mut VaultRow = row.body_mut()?;
            if underlying != Address::ZERO {
                v.underlying = addr20(underlying);
            }
            if oracle != Address::ZERO {
                v.oracle = addr20(oracle);
            }
            if unit != Address::ZERO {
                v.unit_of_account = addr20(unit);
            }
            let priced = cfg.token(addr_from(v.underlying)).is_some()
                && cfg.oracle_pinned(addr_from(v.oracle));
            if priced {
                v.flags |= VaultRow::PRICED;
            } else {
                v.flags &= !VaultRow::PRICED;
            }
            if let Some(tok) = cfg.token(addr_from(v.underlying)) {
                row.asset = tok.asset;
                row.decimals = tok.decimals;
                row.price_feed = tok.feed;
            }
        }
        let v: &VaultRow = row.body()?;
        row.flags = derive_flags(v);
        st.set_market(at, row)?;
        return Ok((m, dirty_slot(m, DEBT_SLOT)));
    }
    let n = match st.markets(cfg.catalog) {
        Ok(r) => r.len(),
        Err(ProtocolError::UnknownMarket(_)) => 0,
        Err(e) => return Err(e),
    };
    let cat_slot = u16::try_from(n).map_err(|_| ProtocolError::MalformedLog)?;
    let market = discovered_market(cfg, st, vault)?;
    let mut cat = MarketRow::blank(CATALOG_ASSET, 0);
    {
        let e: &mut CatalogEntry = cat.body_mut()?;
        e.vault = addr20(vault);
        e.market = market.0;
    }
    st.push_market(cfg.catalog, cat)?;

    let tok = cfg.token(underlying);
    let priced = tok.is_some() && oracle != Address::ZERO && cfg.oracle_pinned(oracle);
    let (asset, decimals, feed) = match tok {
        Some(t) => (t.asset, t.decimals, t.feed),
        None => (UNMAPPED_ASSET, 0, liq_protocol::FeedId(0)),
    };
    let mut row = MarketRow::blank(asset, decimals);
    row.price_feed = feed;
    row.last_update = ts;
    {
        let v: &mut VaultRow = row.body_mut()?;
        v.vault = addr20(vault);
        v.underlying = addr20(underlying);
        v.oracle = addr20(oracle);
        v.unit_of_account = addr20(unit);
        v.max_liquidation_discount = 0;
        v.liquidation_cool_off = 0;
        v.interest_fee = DEFAULT_INTEREST_FEE;
        v.hooked_ops = INITIAL_HOOKED_OPS;
        v.flags = VaultRow::ACC_KNOWN
            | VaultRow::DISCOUNT_KNOWN
            | VaultRow::COOL_OFF_KNOWN
            | VaultRow::HOOKS_KNOWN;
        if priced {
            v.flags |= VaultRow::PRICED;
        }
        set_vault_acc(v, INITIAL_INTEREST_ACCUMULATOR)?;
    }
    let v: &VaultRow = row.body()?;
    row.flags = derive_flags(v);
    st.push_market(market, row)?;
    let mut d = dirty_slot(cfg.catalog, cat_slot);
    d.extend(dirty_slot(market, DEBT_SLOT));
    Ok((market, d))
}

fn patch_vault(
    st: &mut dyn StateWriter,
    market: MarketId,
    ts: Option<u32>,
    f: impl FnOnce(&mut VaultRow) -> Result<()>,
) -> Result<DirtyRows> {
    let at = MarketSlot {
        market,
        slot: DEBT_SLOT,
    };
    let mut row = *st.market(at)?;
    f(row.body_mut::<VaultRow>()?)?;
    if let Some(t) = ts {
        row.last_update = t;
    }
    let v: &VaultRow = row.body()?;
    row.flags = derive_flags(v);
    st.set_market(at, row)?;
    Ok(dirty_slot(market, DEBT_SLOT))
}

fn accrue_to(st: &mut dyn StateWriter, market: MarketId, ts: u64) -> Result<U256> {
    let at = MarketSlot {
        market,
        slot: DEBT_SLOT,
    };
    let mut row = *st.market(at)?;
    let last = u64::from(row.last_update);
    if ts < last {
        return Err(ProtocolError::TimestampBeforeUpdate);
    }
    let v: &VaultRow = row.body()?;
    if v.flags & VaultRow::ACC_KNOWN == 0 {
        return Err(ProtocolError::Internal);
    }
    let (acc, _) = accrue_accumulator(
        vault_acc(v),
        U256::ZERO,
        U256::from(v.interest_rate),
        U256::from(ts.checked_sub(last).ok_or(FixedError::Underflow)?),
    )?;
    set_vault_acc(row.body_mut::<VaultRow>()?, acc)?;
    row.last_update = last_update(ts)?;
    let v: &VaultRow = row.body()?;
    row.flags = derive_flags(v);
    st.set_market(at, row)?;
    Ok(acc)
}

fn write_extra(
    st: &mut dyn StateWriter,
    pos: PositionId,
    f: impl FnOnce(&mut UserExtra) -> Result<()>,
) -> Result<()> {
    let mut extra = *st.extra(pos)?;
    f(extra.view_mut()?)?;
    st.set_extra(pos, extra)
}

fn coll_slots(
    cfg: &Config,
    st: &dyn StateWriter,
    coll_vault: Address,
) -> Result<SmallVec8<(MarketId, u16)>> {
    let mut out = SmallVec8::new();
    let cat = match st.markets(cfg.catalog) {
        Ok(r) => r,
        Err(ProtocolError::UnknownMarket(_)) => return Ok(out),
        Err(e) => return Err(e),
    };
    for crow in cat {
        let e: &CatalogEntry = crow.body()?;
        let market = MarketId(e.market);
        let rows = match st.markets(market) {
            Ok(r) => r,
            Err(ProtocolError::UnknownMarket(_)) => continue,
            Err(err) => return Err(err),
        };
        for (i, row) in rows.iter().enumerate() {
            if i == 0 {
                continue;
            }
            let slot = u16::try_from(i).map_err(|_| ProtocolError::MalformedLog)?;
            let c: &CollRow = row.body()?;
            if addr_from(c.vault) == coll_vault {
                out.push((market, slot));
            }
        }
    }
    Ok(out)
}

fn bump_supply(
    st: &mut dyn StateWriter,
    pos: PositionId,
    slot: u16,
    amount: U256,
    add: bool,
) -> Result<()> {
    let cur = U256::from(st.supply(pos, slot)?);
    let n = if add {
        cur.checked_add(amount).ok_or(FixedError::Overflow)?
    } else {
        cur.checked_sub(amount).ok_or(FixedError::Underflow)?
    };
    st.set_supply(pos, slot, narrow(n)?)
}

const HALT: &[B256] = &[
    halt::Upgraded::SIGNATURE_HASH,
    halt::AdminChanged::SIGNATURE_HASH,
    halt::Initialized::SIGNATURE_HASH,
    events::SetImplementation::SIGNATURE_HASH,
    events::SetUpgradeAdmin::SIGNATURE_HASH,
    events::GovSetGovernorAdmin::SIGNATURE_HASH,
];

pub(crate) fn apply_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let topic0 = *log.topics.first().ok_or(ProtocolError::MalformedLog)?;
    let Some(em) = emitter(cfg, st, log.address)? else {
        return Err(ProtocolError::UnexpectedLog);
    };
    if HALT.contains(&topic0) {
        return if log.block <= cfg.pinned_through {
            Ok(DirtySet::None)
        } else {
            Err(ProtocolError::HaltSignal)
        };
    }
    match em {
        Emitter::Factory => apply_factory(cfg, st, topic0, log),
        Emitter::Evc => apply_evc(cfg, st, topic0, log),
        Emitter::Vault(m) => apply_vault(cfg, st, m, topic0, log),
        Emitter::Pending => apply_pending(cfg, st, topic0, log),
    }
}

fn apply_factory(
    cfg: &Config,
    st: &mut dyn StateWriter,
    topic0: B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    if topic0 == events::ProxyCreated::SIGNATURE_HASH {
        let ev: events::ProxyCreated = decode(log)?;
        let (asset, oracle, unit) = trailing3(ev.trailingData.as_ref())?;
        let ts = last_update(log.timestamp)?;
        let (_, rows) = ensure_vault(cfg, st, ev.proxy, asset, oracle, unit, ts)?;
        return Ok(DirtySet::MarketReprice(rows));
    }
    if topic0 == events::Genesis::SIGNATURE_HASH {
        return Ok(DirtySet::None);
    }
    Ok(DirtySet::None)
}

fn apply_evc(
    cfg: &Config,
    st: &mut dyn StateWriter,
    topic0: B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    if topic0 == evc::CollateralStatus::SIGNATURE_HASH {
        let ev: evc::CollateralStatus = decode(log)?;
        let slots = coll_slots(cfg, st, ev.collateral)?;
        let mut ids = DirtyPositions::new();
        for (market, slot) in slots {
            let pos = intern(cfg, st, market, ev.account)?;
            write_extra(st, pos, |u| {
                let bit = 1u128
                    .checked_shl(u32::from(slot))
                    .ok_or(FixedError::Overflow)?;
                if ev.enabled {
                    u.enabled_mask |= bit;
                } else {
                    u.enabled_mask &= !bit;
                }
                Ok(())
            })?;
            ids.push(pos);
        }
        return Ok(if ids.is_empty() {
            DirtySet::None
        } else {
            DirtySet::Positions(ids)
        });
    }
    if topic0 == evc::ControllerStatus::SIGNATURE_HASH {
        let ev: evc::ControllerStatus = decode(log)?;
        let Some(market) = lookup_vault(cfg, st, ev.controller)? else {
            return Ok(DirtySet::None);
        };
        if !ev.enabled {
            return Ok(DirtySet::None);
        }
        let pos = intern(cfg, st, market, ev.account)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == evc::AccountStatusCheck::SIGNATURE_HASH {
        let ev: evc::AccountStatusCheck = decode(log)?;
        let Some(market) = lookup_vault(cfg, st, ev.controller)? else {
            return Ok(DirtySet::None);
        };
        let pos = intern(cfg, st, market, ev.account)?;
        let ts = last_update(log.timestamp)?;
        write_extra(st, pos, |u| {
            u.last_status_check = ts;
            Ok(())
        })?;
        return Ok(positions(&[pos]));
    }
    Ok(DirtySet::None)
}

fn apply_pending(
    cfg: &Config,
    st: &mut dyn StateWriter,
    topic0: B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    if topic0 == events::EVaultCreated::SIGNATURE_HASH {
        let ev: events::EVaultCreated = decode(log)?;
        let ts = last_update(log.timestamp)?;
        let (_, rows) = ensure_vault(
            cfg,
            st,
            log.address,
            ev.asset,
            Address::ZERO,
            Address::ZERO,
            ts,
        )?;
        return Ok(DirtySet::MarketReprice(rows));
    }
    Err(ProtocolError::UnexpectedLog)
}

fn apply_vault(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    topic0: B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    if topic0 == events::EVaultCreated::SIGNATURE_HASH {
        let ev: events::EVaultCreated = decode(log)?;
        let ts = last_update(log.timestamp)?;
        let (_, rows) = ensure_vault(
            cfg,
            st,
            log.address,
            ev.asset,
            Address::ZERO,
            Address::ZERO,
            ts,
        )?;
        return Ok(DirtySet::MarketReprice(rows));
    }
    if topic0 == events::VaultStatus::SIGNATURE_HASH {
        return vault_status(st, market, log);
    }
    if topic0 == events::GovSetLTV::SIGNATURE_HASH {
        return gov_ltv(cfg, st, market, log);
    }
    if topic0 == events::GovSetMaxLiquidationDiscount::SIGNATURE_HASH {
        let ev: events::GovSetMaxLiquidationDiscount = decode(log)?;
        let rows = patch_vault(st, market, None, |v| {
            v.max_liquidation_discount = ev.newDiscount;
            v.flags |= VaultRow::DISCOUNT_KNOWN;
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(rows));
    }
    if topic0 == events::GovSetLiquidationCoolOffTime::SIGNATURE_HASH {
        let ev: events::GovSetLiquidationCoolOffTime = decode(log)?;
        let rows = patch_vault(st, market, None, |v| {
            v.liquidation_cool_off = ev.newCoolOffTime;
            v.flags |= VaultRow::COOL_OFF_KNOWN;
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(rows));
    }
    if topic0 == events::GovSetHookConfig::SIGNATURE_HASH {
        let ev: events::GovSetHookConfig = decode(log)?;
        let rows = patch_vault(st, market, None, |v| {
            v.hooked_ops = ev.newHookedOps;
            v.flags |= VaultRow::HOOKS_KNOWN;
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(rows));
    }
    if topic0 == events::GovSetConfigFlags::SIGNATURE_HASH {
        let ev: events::GovSetConfigFlags = decode(log)?;
        let rows = patch_vault(st, market, None, |v| {
            v.config_flags = ev.newConfigFlags;
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(rows));
    }
    if topic0 == events::GovSetInterestFee::SIGNATURE_HASH {
        let ev: events::GovSetInterestFee = decode(log)?;
        let rows = patch_vault(st, market, None, |v| {
            v.interest_fee = ev.newFee;
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(rows));
    }
    if topic0 == events::Transfer::SIGNATURE_HASH {
        return transfer(cfg, st, log);
    }
    if topic0 == events::Deposit::SIGNATURE_HASH {
        return deposit(cfg, st, log, true);
    }
    if topic0 == events::Withdraw::SIGNATURE_HASH {
        return deposit(cfg, st, log, false);
    }
    if topic0 == events::Borrow::SIGNATURE_HASH {
        return borrow(cfg, st, market, log, true);
    }
    if topic0 == events::Repay::SIGNATURE_HASH {
        return borrow(cfg, st, market, log, false);
    }
    if topic0 == events::Liquidate::SIGNATURE_HASH {
        return liquidate(cfg, st, market, log);
    }
    if topic0 == events::PullDebt::SIGNATURE_HASH {
        let ev: events::PullDebt = decode(log)?;
        let a = intern(cfg, st, market, ev.from)?;
        let b = intern(cfg, st, market, ev.to)?;
        return Ok(positions(&[a, b]));
    }
    if topic0 == events::DebtSocialized::SIGNATURE_HASH {
        let ev: events::DebtSocialized = decode(log)?;
        let p = intern(cfg, st, market, ev.account)?;
        return Ok(positions(&[p]));
    }
    Ok(DirtySet::None)
}

fn vault_status(
    st: &mut dyn StateWriter,
    market: MarketId,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let ev: events::VaultStatus = decode(log)?;
    let ts = last_update(narrow(ev.timestamp)?)?;
    let acc = ev.interestAccumulator;
    let rate: u128 = narrow(ev.interestRate)?;
    let rows = patch_vault(st, market, Some(ts), |v| {
        set_vault_acc(v, acc)?;
        v.interest_rate = rate;
        v.flags |= VaultRow::ACC_KNOWN;
        Ok(())
    })?;
    Ok(DirtySet::MarketAccrual(rows))
}

fn gov_ltv(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let ev: events::GovSetLTV = decode(log)?;
    let target = u64::try_from(ev.targetTimestamp).map_err(|_| ProtocolError::MalformedLog)?;
    let tok = cfg.token(ev.collateral);
    let (asset, decimals, feed, priced) = match tok {
        Some(t) => {
            let debt = st.market(MarketSlot {
                market,
                slot: DEBT_SLOT,
            })?;
            let v: &VaultRow = debt.body()?;
            (t.asset, t.decimals, t.feed, v.flags & VaultRow::PRICED != 0)
        }
        None => (UNMAPPED_ASSET, 0, liq_protocol::FeedId(0), false),
    };
    let found = {
        let rows = st.markets(market)?;
        let mut found = None;
        for (i, row) in rows.iter().enumerate().skip(1) {
            let c: &CollRow = row.body()?;
            if addr_from(c.vault) == ev.collateral {
                found = Some(u16::try_from(i).map_err(|_| ProtocolError::MalformedLog)?);
                break;
            }
        }
        found
    };
    let slot = match found {
        Some(s) => s,
        None => {
            let mut row = MarketRow::blank(asset, decimals);
            row.price_feed = feed;
            st.push_market(market, row)?.slot
        }
    };
    let at = MarketSlot { market, slot };
    let mut row = *st.market(at)?;
    row.asset = asset;
    row.decimals = decimals;
    row.price_feed = feed;
    row.flags = if priced {
        MarketFlags::NONE
    } else {
        MarketFlags::UNPRICED
    };
    {
        let c: &mut CollRow = row.body_mut()?;
        c.vault = addr20(ev.collateral);
        c.borrow_ltv = ev.borrowLTV;
        c.liquidation_ltv = ev.liquidationLTV;
        c.initial_liquidation_ltv = ev.initialLiquidationLTV;
        c.ramp_duration = ev.rampDuration;
        c.target_timestamp = target;
        c.share_decimals = decimals;
        c.share_asset = asset.0;
        if target != 0 {
            c.flags |= CollRow::RECOGNIZED;
        }
    }
    st.set_market(at, row)?;
    Ok(DirtySet::MarketReprice(dirty_slot(market, slot)))
}

fn transfer(cfg: &Config, st: &mut dyn StateWriter, log: &DecodedLog<'_>) -> Result<DirtySet> {
    let ev: events::Transfer = decode(log)?;
    if ev.from == Address::ZERO || ev.to == Address::ZERO {
        return Ok(DirtySet::None);
    }
    share_move(cfg, st, log.address, ev.from, ev.to, ev.value)
}

fn deposit(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    is_deposit: bool,
) -> Result<DirtySet> {
    if is_deposit {
        let ev: events::Deposit = decode(log)?;
        share_move(cfg, st, log.address, Address::ZERO, ev.owner, ev.shares)
    } else {
        let ev: events::Withdraw = decode(log)?;
        share_move(cfg, st, log.address, ev.owner, Address::ZERO, ev.shares)
    }
}

fn share_move(
    cfg: &Config,
    st: &mut dyn StateWriter,
    coll_vault: Address,
    from: Address,
    to: Address,
    amount: U256,
) -> Result<DirtySet> {
    let slots = coll_slots(cfg, st, coll_vault)?;
    let mut ids = DirtyPositions::new();
    for (market, slot) in slots {
        if from != Address::ZERO {
            let p = intern(cfg, st, market, from)?;
            bump_supply(st, p, slot, amount, false)?;
            ids.push(p);
        }
        if to != Address::ZERO {
            let p = intern(cfg, st, market, to)?;
            bump_supply(st, p, slot, amount, true)?;
            ids.push(p);
        }
    }
    Ok(if ids.is_empty() {
        DirtySet::None
    } else {
        DirtySet::Positions(ids)
    })
}

fn borrow(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    log: &DecodedLog<'_>,
    is_borrow: bool,
) -> Result<DirtySet> {
    let (account, assets) = if is_borrow {
        let ev: events::Borrow = decode(log)?;
        (ev.account, ev.assets)
    } else {
        let ev: events::Repay = decode(log)?;
        (ev.account, ev.assets)
    };
    let pos = intern(cfg, st, market, account)?;
    let acc = accrue_to(st, market, log.timestamp)?;
    let extra = *st.extra(pos)?;
    let user: &UserExtra = extra.view()?;
    let stored = U256::from(st.debt(pos, DEBT_SLOT)?);
    let user_acc = u256_from_limbs(user.user_accumulator_lo, user.user_accumulator_hi);
    let current = if stored.is_zero() {
        U256::ZERO
    } else if user.flags & UserExtra::ACC_KNOWN == 0 {
        return Err(ProtocolError::Internal);
    } else {
        current_owed(stored, acc, user_acc)?
    };
    let next = if is_borrow {
        current
            .checked_add(assets_to_owed(assets)?)
            .ok_or(FixedError::Overflow)?
    } else {
        let owed_assets = to_assets_up(current)?;
        if assets > owed_assets {
            return Err(ProtocolError::MalformedLog);
        }
        let remain = owed_assets
            .checked_sub(assets)
            .ok_or(FixedError::Underflow)?;
        assets_to_owed(remain)?
    };
    st.set_debt(pos, DEBT_SLOT, narrow(next)?)?;
    let (lo, hi) = u256_to_limbs(acc)?;
    write_extra(st, pos, |u| {
        u.user_accumulator_lo = lo;
        u.user_accumulator_hi = hi;
        u.flags |= UserExtra::ACC_KNOWN;
        Ok(())
    })?;
    Ok(positions(&[pos]))
}

fn liquidate(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let ev: events::Liquidate = decode(log)?;
    let v = intern(cfg, st, market, ev.violator)?;
    let l = intern(cfg, st, market, ev.liquidator)?;
    Ok(positions(&[v, l]))
}
