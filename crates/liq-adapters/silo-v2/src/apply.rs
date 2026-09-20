//! `Protocol::apply_log` for Silo V2. Journal-before-write is the writer.

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolEvent;
use liq_protocol::{
    DecodedLog, DirtyPositions, DirtyRows, DirtySet, MarketFlags, MarketRow, MarketSlot,
    ProtocolError, Result, StateWriter,
};
use liq_types::fixed::FixedError;
use liq_types::{MarketId, PositionId, PositionKey};

use crate::config::{Config, Emitter, ShareKind, ShareLoc};
use crate::events::{factory, halt, hook, silo};
use crate::layout::{SiloRow, UserExtra, SLOT0, SLOT1};

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
        slot: SLOT0,
    });
    d.push(MarketSlot {
        market,
        slot: SLOT1,
    });
    d
}

fn intern(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    user: Address,
) -> Result<PositionId> {
    let pos = st.intern(&PositionKey {
        protocol: cfg.protocol,
        market,
        user,
    })?;
    // Fresh extra is zeroed; slot 0 would otherwise look like "collateral is
    // silo0" before `Borrow` / `onDebtTransfer` writes `borrowerCollateralSilo`.
    let e = extra(st, pos)?;
    if e.collateral_slot == 0 && e.protected_0 == 0 && e.protected_1 == 0 {
        let d0 = st.debt(pos, SLOT0)?;
        let d1 = st.debt(pos, SLOT1)?;
        if d0 == 0 && d1 == 0 {
            let mut e = e;
            e.collateral_slot = UserExtra::UNSET;
            set_extra(st, pos, e)?;
        }
    }
    Ok(pos)
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

fn extra(st: &dyn StateWriter, pos: PositionId) -> Result<UserExtra> {
    Ok(*st.extra(pos)?.view::<UserExtra>()?)
}

fn set_extra(st: &mut dyn StateWriter, pos: PositionId, e: UserExtra) -> Result<()> {
    let mut repr = *st.extra(pos)?;
    *repr.view_mut::<UserExtra>()? = e;
    st.set_extra(pos, repr)
}

fn patch_row(
    st: &mut dyn StateWriter,
    market: MarketId,
    slot: u16,
    ts: Option<u32>,
    f: impl FnOnce(&mut SiloRow) -> Result<()>,
) -> Result<()> {
    let at = MarketSlot { market, slot };
    let mut row = *st.market(at)?;
    f(row.body_mut::<SiloRow>()?)?;
    if let Some(t) = ts {
        row.last_update = t;
    }
    let viewed = row.body::<SiloRow>()?.flags;
    row.flags = if viewed & SiloRow::PRICED == 0 {
        MarketFlags::UNPRICED
    } else {
        MarketFlags::NONE
    };
    st.set_market(at, row)
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
    let Some(em) = cfg.emitter(log.address) else {
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
        Emitter::Factory(_) => factory_log(cfg, st, log, topic0),
        Emitter::Hook(i) => hook_log(cfg, st, log, i, topic0),
        Emitter::Silo { pair, slot } => silo_log(cfg, st, log, pair, slot, topic0),
        Emitter::Share(loc) => share_log(cfg, st, log, loc, topic0),
        Emitter::SiloConfig(_) => Ok(DirtySet::None),
    }
}

fn factory_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    topic0: B256,
) -> Result<DirtySet> {
    if topic0 == factory::NewSilo::SIGNATURE_HASH {
        return new_silo(cfg, st, log);
    }
    if topic0 == factory::NewSiloShareTokens::SIGNATURE_HASH
        || topic0 == factory::NewSiloHook::SIGNATURE_HASH
    {
        if topic0 == factory::NewSiloHook::SIGNATURE_HASH {
            let ev = decode::<factory::NewSiloHook>(log)?;
            if let Some((_, p)) = cfg
                .pairs
                .iter()
                .enumerate()
                .find(|(_, p)| p.silo0.silo == ev.silo || p.silo1.silo == ev.silo)
            {
                if ev.hook != p.hook_receiver && log.block > cfg.pinned_through {
                    return Err(ProtocolError::HaltSignal);
                }
            }
        }
        return Ok(DirtySet::None);
    }
    Ok(DirtySet::None)
}

fn new_silo(cfg: &Config, st: &mut dyn StateWriter, log: &DecodedLog<'_>) -> Result<DirtySet> {
    let ev = decode::<factory::NewSilo>(log)?;
    let Some((_, pair)) = cfg.pair_by_config(ev.siloConfig) else {
        return Ok(DirtySet::None);
    };
    if ev.silo0 != pair.silo0.silo || ev.silo1 != pair.silo1.silo {
        return Err(ProtocolError::OracleSourceMismatch);
    }
    match st.markets(pair.market) {
        Ok(rows) if !rows.is_empty() => {
            return Err(ProtocolError::SlotMismatch {
                expected: 0,
                got: u16::try_from(rows.len()).unwrap_or(u16::MAX),
            });
        }
        Ok(_) | Err(ProtocolError::UnknownMarket(_)) => {}
        Err(e) => return Err(e),
    }
    let ts = last_update(log.timestamp)?;
    for slot in [SLOT0, SLOT1] {
        let side = pair.side(slot).ok_or(ProtocolError::Internal)?;
        let tok = cfg
            .asset_by_underlying(side.token)
            .ok_or(ProtocolError::OracleSourceMismatch)?;
        let mut row = MarketRow::blank(tok.asset, tok.decimals);
        row.price_feed = tok.feed;
        row.last_update = ts;
        {
            let b: &mut SiloRow = row.body_mut()?;
            b.lt = side.lt;
            b.liquidation_fee = side.liquidation_fee;
            b.liquidation_target_ltv = side.liquidation_target_ltv;
            b.hook = *pair.hook_receiver.as_ref();
            b.silo = *side.silo.as_ref();
            b.config = *pair.silo_config.as_ref();
            b.flags = SiloRow::VIEWED | SiloRow::PRICED;
        }
        row.flags = MarketFlags::NONE;
        let got = st.push_market(pair.market, row)?;
        if got.slot != slot {
            return Err(ProtocolError::SlotMismatch {
                expected: slot,
                got: got.slot,
            });
        }
    }
    Ok(DirtySet::MarketReprice(rows_of(pair.market)))
}

fn hook_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    pair_i: usize,
    topic0: B256,
) -> Result<DirtySet> {
    if topic0 == hook::LiquidationCall::SIGNATURE_HASH {
        let ev = decode::<hook::LiquidationCall>(log)?;
        let pair = cfg.pair(pair_i).ok_or(ProtocolError::Internal)?;
        let pos = intern(cfg, st, pair.market, ev.borrower)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == hook::LiquidationStart::SIGNATURE_HASH {
        return Ok(DirtySet::None);
    }
    Ok(DirtySet::None)
}

fn silo_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    pair_i: usize,
    slot: u16,
    topic0: B256,
) -> Result<DirtySet> {
    let pair = cfg.pair(pair_i).ok_or(ProtocolError::Internal)?;
    let market = pair.market;
    let ts = last_update(log.timestamp)?;
    if topic0 == silo::Deposit::SIGNATURE_HASH {
        let ev = decode::<silo::Deposit>(log)?;
        return coll_delta(
            cfg,
            st,
            market,
            slot,
            CollDelta {
                owner: ev.owner,
                assets: ev.assets,
                shares: ev.shares,
                add: true,
                protected: false,
                ts,
            },
        );
    }
    if topic0 == silo::DepositProtected::SIGNATURE_HASH {
        let ev = decode::<silo::DepositProtected>(log)?;
        return coll_delta(
            cfg,
            st,
            market,
            slot,
            CollDelta {
                owner: ev.owner,
                assets: ev.assets,
                shares: ev.shares,
                add: true,
                protected: true,
                ts,
            },
        );
    }
    if topic0 == silo::Withdraw::SIGNATURE_HASH {
        let ev = decode::<silo::Withdraw>(log)?;
        return coll_delta(
            cfg,
            st,
            market,
            slot,
            CollDelta {
                owner: ev.owner,
                assets: ev.assets,
                shares: ev.shares,
                add: false,
                protected: false,
                ts,
            },
        );
    }
    if topic0 == silo::WithdrawProtected::SIGNATURE_HASH {
        let ev = decode::<silo::WithdrawProtected>(log)?;
        return coll_delta(
            cfg,
            st,
            market,
            slot,
            CollDelta {
                owner: ev.owner,
                assets: ev.assets,
                shares: ev.shares,
                add: false,
                protected: true,
                ts,
            },
        );
    }
    if topic0 == silo::Borrow::SIGNATURE_HASH {
        let ev = decode::<silo::Borrow>(log)?;
        let pos = intern(cfg, st, market, ev.owner)?;
        let cur = st.debt(pos, slot)?;
        st.set_debt(pos, slot, add_u128(cur, ev.shares, true)?)?;
        let mut ex = extra(st, pos)?;
        if ex.collateral_slot == UserExtra::UNSET {
            ex.collateral_slot = u8::try_from(pair.other(slot).ok_or(ProtocolError::Internal)?)
                .map_err(|_| ProtocolError::Internal)?;
            set_extra(st, pos, ex)?;
        }
        patch_row(st, market, slot, Some(ts), |b| {
            b.total_debt_assets = add_u128(b.total_debt_assets, ev.assets, true)?;
            b.total_debt_shares = add_u128(b.total_debt_shares, ev.shares, true)?;
            Ok(())
        })?;
        return Ok(positions(&[pos]));
    }
    if topic0 == silo::Repay::SIGNATURE_HASH {
        let ev = decode::<silo::Repay>(log)?;
        let pos = intern(cfg, st, market, ev.owner)?;
        let cur = st.debt(pos, slot)?;
        st.set_debt(pos, slot, add_u128(cur, ev.shares, false)?)?;
        patch_row(st, market, slot, Some(ts), |b| {
            b.total_debt_assets = add_u128(b.total_debt_assets, ev.assets, false)?;
            b.total_debt_shares = add_u128(b.total_debt_shares, ev.shares, false)?;
            Ok(())
        })?;
        return Ok(positions(&[pos]));
    }
    if topic0 == silo::CollateralTypeChanged::SIGNATURE_HASH {
        let ev = decode::<silo::CollateralTypeChanged>(log)?;
        let pos = intern(cfg, st, market, ev.borrower)?;
        let mut ex = extra(st, pos)?;
        ex.collateral_slot = u8::try_from(slot).map_err(|_| ProtocolError::Internal)?;
        set_extra(st, pos, ex)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == silo::AccruedInterest::SIGNATURE_HASH {
        patch_row(st, market, slot, Some(ts), |_| Ok(()))?;
        return Ok(DirtySet::MarketAccrual(rows_of(market)));
    }
    if topic0 == silo::Transfer::SIGNATURE_HASH {
        return share_transfer(
            cfg,
            st,
            log,
            ShareLoc {
                pair: pair_i,
                slot,
                kind: ShareKind::Collateral,
            },
        );
    }
    if topic0 == silo::FlashLoan::SIGNATURE_HASH
        || topic0 == silo::HooksUpdated::SIGNATURE_HASH
        || topic0 == silo::WithdrawnFees::SIGNATURE_HASH
        || topic0 == silo::DeployerFeesRedirected::SIGNATURE_HASH
    {
        return Ok(DirtySet::None);
    }
    Ok(DirtySet::None)
}

struct CollDelta {
    owner: Address,
    assets: U256,
    shares: U256,
    add: bool,
    protected: bool,
    ts: u32,
}

fn coll_delta(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    slot: u16,
    d: CollDelta,
) -> Result<DirtySet> {
    let pos = intern(cfg, st, market, d.owner)?;
    if d.protected {
        let mut ex = extra(st, pos)?;
        let cur = ex.protected(slot);
        ex.set_protected(slot, add_u128(cur, d.shares, d.add)?);
        set_extra(st, pos, ex)?;
    } else {
        let cur = st.supply(pos, slot)?;
        st.set_supply(pos, slot, add_u128(cur, d.shares, d.add)?)?;
    }
    patch_row(st, market, slot, Some(d.ts), |b| {
        if d.protected {
            b.total_protected_assets = add_u128(b.total_protected_assets, d.assets, d.add)?;
            b.total_protected_shares = add_u128(b.total_protected_shares, d.shares, d.add)?;
        } else {
            b.total_collateral_assets = add_u128(b.total_collateral_assets, d.assets, d.add)?;
            b.total_collateral_shares = add_u128(b.total_collateral_shares, d.shares, d.add)?;
        }
        Ok(())
    })?;
    Ok(positions(&[pos]))
}

fn share_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    loc: ShareLoc,
    topic0: B256,
) -> Result<DirtySet> {
    if topic0 == silo::Transfer::SIGNATURE_HASH {
        return share_transfer(cfg, st, log, loc);
    }
    Ok(DirtySet::None)
}

fn share_transfer(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    loc: ShareLoc,
) -> Result<DirtySet> {
    let ev = decode::<silo::Transfer>(log)?;
    if ev.from == Address::ZERO || ev.to == Address::ZERO {
        return Ok(DirtySet::None);
    }
    let pair = cfg.pair(loc.pair).ok_or(ProtocolError::Internal)?;
    let market = pair.market;
    let mut dirty: [PositionId; 2] = [PositionId(0), PositionId(0)];
    let mut n = 0usize;
    for user in [ev.from, ev.to] {
        let pos = intern(cfg, st, market, user)?;
        let add = user == ev.to;
        match loc.kind {
            ShareKind::Collateral => {
                let cur = st.supply(pos, loc.slot)?;
                st.set_supply(pos, loc.slot, add_u128(cur, ev.value, add)?)?;
            }
            ShareKind::Protected => {
                let mut ex = extra(st, pos)?;
                let cur = ex.protected(loc.slot);
                ex.set_protected(loc.slot, add_u128(cur, ev.value, add)?);
                set_extra(st, pos, ex)?;
            }
            ShareKind::Debt => {
                let cur = st.debt(pos, loc.slot)?;
                st.set_debt(pos, loc.slot, add_u128(cur, ev.value, add)?)?;
                if add {
                    let mut ex = extra(st, pos)?;
                    if ex.collateral_slot == UserExtra::UNSET {
                        let src = intern(cfg, st, market, ev.from)?;
                        ex.collateral_slot = extra(st, src)?.collateral_slot;
                        set_extra(st, pos, ex)?;
                    }
                }
            }
        }
        if let Some(slot) = dirty.get_mut(n) {
            *slot = pos;
            n = n.saturating_add(1);
        }
    }
    let ids = dirty.get(..n).ok_or(ProtocolError::Internal)?;
    Ok(positions(ids))
}
