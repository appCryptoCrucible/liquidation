//! `Protocol::apply_log` for Gearbox V3. Journal-before-write is the writer.
//! Liquidation entry is the facade; pool `Repay` has no credit account.

use alloy_primitives::{Address, B256, I256, U256};
use alloy_sol_types::SolEvent;
use bytemuck::Zeroable;
use liq_protocol::{
    DecodedLog, DirtyPositions, DirtyRows, DirtySet, MarketFlags, MarketRow, MarketSlot,
    ProtocolError, Result, StateWriter,
};
use liq_types::fixed::FixedError;
use liq_types::{MarketId, PositionId, PositionKey};

use crate::config::{Config, Emitter, ManagerConfig};
use crate::events::{configurator, facade, factory, halt, manager, pool, quota};
use crate::layout::{
    AccountExtra, ManagerRow, QuotaExtra, TokenRow, UNDERLYING_SLOT, UNMAPPED_ASSET,
};
use crate::math::{addr20, PERCENTAGE_FACTOR, STATIC_LT_RAMP_START};

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
fn dirty_slot(market: MarketId, slot: u16) -> DirtyRows {
    let mut d = DirtyRows::new();
    d.push(MarketSlot { market, slot });
    d
}

#[inline]
fn add_u128(cur: u128, d: U256, add: bool) -> Result<u128> {
    let c = U256::from(cur);
    let n = if add {
        c.checked_add(d).ok_or(FixedError::Overflow)?
    } else {
        c.checked_sub(d).ok_or(FixedError::Underflow)?
    };
    narrow(n)
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

fn extra(st: &dyn StateWriter, pos: PositionId) -> Result<AccountExtra> {
    Ok(*st.extra(pos)?.view::<AccountExtra>()?)
}

fn set_extra(st: &mut dyn StateWriter, pos: PositionId, e: AccountExtra) -> Result<()> {
    let mut repr = *st.extra(pos)?;
    *repr.view_mut::<AccountExtra>()? = e;
    st.set_extra(pos, repr)
}

fn quota_at(st: &dyn StateWriter, pos: PositionId, slot: u16) -> Result<QuotaExtra> {
    Ok(*st.slot_extra(pos, slot)?.view::<QuotaExtra>()?)
}

fn set_quota(st: &mut dyn StateWriter, pos: PositionId, slot: u16, q: QuotaExtra) -> Result<()> {
    let mut repr = *st.slot_extra(pos, slot)?;
    *repr.view_mut::<QuotaExtra>()? = q;
    st.set_slot_extra(pos, slot, repr)
}

fn derive_flags(m: &ManagerRow) -> MarketFlags {
    let mut f = 0u8;
    if m.flags & ManagerRow::PRICED == 0 {
        f |= MarketFlags::UNPRICED.0;
    }
    if m.flags & ManagerRow::PAUSED != 0 {
        f |= MarketFlags::PAUSED.0;
    }
    MarketFlags(f)
}

fn token_flags(t: &TokenRow) -> MarketFlags {
    if t.flags & TokenRow::PRICED == 0 {
        MarketFlags::UNPRICED
    } else {
        MarketFlags::NONE
    }
}

fn halt_after(cfg: &Config, log: &DecodedLog<'_>) -> Result<DirtySet> {
    if log.block <= cfg.pinned_through {
        Ok(DirtySet::None)
    } else {
        Err(ProtocolError::HaltSignal)
    }
}

const HALT: &[B256] = &[
    halt::Upgraded::SIGNATURE_HASH,
    halt::AdminChanged::SIGNATURE_HASH,
    halt::Initialized::SIGNATURE_HASH,
];

fn pf_u16() -> Result<u16> {
    u16::try_from(PERCENTAGE_FACTOR).map_err(|_| ProtocolError::Internal)
}

fn i96_to_i256(v: alloy_primitives::aliases::I96) -> Result<I256> {
    let (sign, abs) = v.into_sign_and_abs();
    let mag = I256::try_from(U256::from(abs)).map_err(|_| ProtocolError::MalformedLog)?;
    if sign.is_negative() {
        mag.checked_neg()
            .ok_or(ProtocolError::Fixed(FixedError::Overflow))
    } else {
        Ok(mag)
    }
}

fn ensure_listed(
    _cfg: &Config,
    st: &mut dyn StateWriter,
    m: &ManagerConfig,
    ts: u32,
) -> Result<()> {
    match st.markets(m.market) {
        Ok(rows) if !rows.is_empty() => {
            let n = u16::try_from(rows.len()).map_err(|_| ProtocolError::Internal)?;
            let want = u16::try_from(m.tokens.len()).map_err(|_| ProtocolError::Internal)?;
            if n != want {
                return Err(ProtocolError::SlotMismatch {
                    expected: want,
                    got: n,
                });
            }
            return Ok(());
        }
        Ok(_) | Err(ProtocolError::UnknownMarket(_)) => {}
        Err(e) => return Err(e),
    }
    for t in &m.tokens {
        if t.slot == UNDERLYING_SLOT {
            let mut row = MarketRow::blank(t.asset, t.decimals);
            row.price_feed = t.feed;
            row.last_update = ts;
            {
                let b: &mut ManagerRow = row.body_mut()?;
                b.manager = addr20(m.manager);
                b.facade = addr20(m.facade);
                b.pool = addr20(m.pool);
                b.underlying = addr20(m.underlying);
                b.fee_interest = m.fees.fee_interest;
                b.fee_liquidation = m.fees.fee_liquidation;
                b.liquidation_discount = m.fees.liquidation_discount;
                b.fee_liquidation_expired = m.fees.fee_liquidation_expired;
                b.liquidation_discount_expired = m.fees.liquidation_discount_expired;
                b.lt_underlying = m.lt_underlying;
                b.expiration_date =
                    u32::try_from(m.expiration_date).map_err(|_| ProtocolError::Internal)?;
                b.quoted_tokens_mask = m.quoted_tokens_mask;
                b.min_debt = m.min_debt.to_be_bytes();
                b.flags = ManagerRow::FEES;
                if t.asset != UNMAPPED_ASSET {
                    b.flags |= ManagerRow::PRICED;
                }
                b.expirable = u8::from(m.expirable);
                b.token_count =
                    u8::try_from(m.tokens.len()).map_err(|_| ProtocolError::Internal)?;
            }
            let viewed: &ManagerRow = row.body()?;
            row.flags = derive_flags(viewed);
            let got = st.push_market(m.market, row)?;
            if got.slot != t.slot {
                return Err(ProtocolError::SlotMismatch {
                    expected: t.slot,
                    got: got.slot,
                });
            }
        } else {
            let mut row = MarketRow::blank(t.asset, t.decimals);
            row.price_feed = t.feed;
            row.last_update = ts;
            {
                let b: &mut TokenRow = row.body_mut()?;
                b.token = addr20(t.token);
                b.lt_initial = t.lt_initial;
                b.lt_final = t.lt_final;
                b.ramp_start = t.ramp_start;
                b.ramp_duration = t.ramp_duration;
                b.flags = TokenRow::LISTED;
                if t.asset != UNMAPPED_ASSET {
                    b.flags |= TokenRow::PRICED;
                }
            }
            let viewed: &TokenRow = row.body()?;
            row.flags = token_flags(viewed);
            let got = st.push_market(m.market, row)?;
            if got.slot != t.slot {
                return Err(ProtocolError::SlotMismatch {
                    expected: t.slot,
                    got: got.slot,
                });
            }
        }
    }
    Ok(())
}

fn patch_manager(
    st: &mut dyn StateWriter,
    market: MarketId,
    ts: Option<u32>,
    f: impl FnOnce(&mut ManagerRow) -> Result<()>,
) -> Result<DirtyRows> {
    let at = MarketSlot {
        market,
        slot: UNDERLYING_SLOT,
    };
    let mut row = *st.market(at)?;
    f(row.body_mut::<ManagerRow>()?)?;
    if let Some(t) = ts {
        row.last_update = t;
    }
    let m: &ManagerRow = row.body()?;
    row.flags = derive_flags(m);
    st.set_market(at, row)?;
    Ok(dirty_slot(market, UNDERLYING_SLOT))
}

fn patch_token(
    st: &mut dyn StateWriter,
    market: MarketId,
    slot: u16,
    ts: Option<u32>,
    f: impl FnOnce(&mut TokenRow) -> Result<()>,
) -> Result<DirtyRows> {
    let at = MarketSlot { market, slot };
    let mut row = *st.market(at)?;
    f(row.body_mut::<TokenRow>()?)?;
    if let Some(t) = ts {
        row.last_update = t;
    }
    let t: &TokenRow = row.body()?;
    row.flags = token_flags(t);
    st.set_market(at, row)?;
    Ok(dirty_slot(market, slot))
}

fn enable_bit(ex: &mut AccountExtra, slot: u16) -> Result<()> {
    let bit = 1u64
        .checked_shl(u32::from(slot))
        .ok_or(FixedError::Overflow)?;
    ex.enabled_tokens_mask |= bit;
    Ok(())
}

fn zero_account(st: &mut dyn StateWriter, pos: PositionId, token_count: u8) -> Result<()> {
    let n = u16::from(token_count.max(1));
    for slot in 0u16..n {
        st.set_supply(pos, slot, 0)?;
        st.set_debt(pos, slot, 0)?;
        set_quota(st, pos, slot, QuotaExtra::zeroed())?;
    }
    set_extra(st, pos, AccountExtra::zeroed())
}

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
        return halt_after(cfg, log);
    }
    match em {
        Emitter::Register => Ok(DirtySet::None),
        Emitter::Facade(i) => facade_log(cfg, st, log, i, topic0),
        Emitter::Manager(i) => manager_log(cfg, st, log, i, topic0),
        Emitter::Configurator(i) => configurator_log(cfg, st, log, i, topic0),
        Emitter::Pool(_) => pool_log(cfg, st, log, topic0),
        Emitter::Factory(_) => factory_log(cfg, st, log, topic0),
        Emitter::Quota(_) => quota_log(cfg, st, log, topic0),
    }
}

fn facade_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    i: usize,
    topic0: B256,
) -> Result<DirtySet> {
    let m = cfg.manager(i).ok_or(ProtocolError::Internal)?;
    let ts = last_update(log.timestamp)?;
    ensure_listed(cfg, st, m, ts)?;
    if topic0 == facade::OpenCreditAccount::SIGNATURE_HASH {
        let ev = decode::<facade::OpenCreditAccount>(log)?;
        let pos = intern(cfg, st, m.market, ev.creditAccount)?;
        let mut ex = extra(st, pos)?;
        ex.flags |= AccountExtra::OPEN;
        set_extra(st, pos, ex)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == facade::CloseCreditAccount::SIGNATURE_HASH {
        let ev = decode::<facade::CloseCreditAccount>(log)?;
        let pos = intern(cfg, st, m.market, ev.creditAccount)?;
        let n = st
            .market(MarketSlot {
                market: m.market,
                slot: UNDERLYING_SLOT,
            })?
            .body::<ManagerRow>()?
            .token_count;
        zero_account(st, pos, n)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == facade::LiquidateCreditAccount::SIGNATURE_HASH {
        let ev = decode::<facade::LiquidateCreditAccount>(log)?;
        let pos = intern(cfg, st, m.market, ev.creditAccount)?;
        let n = st
            .market(MarketSlot {
                market: m.market,
                slot: UNDERLYING_SLOT,
            })?
            .body::<ManagerRow>()?
            .token_count;
        zero_account(st, pos, n)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == facade::PartiallyLiquidateCreditAccount::SIGNATURE_HASH {
        let ev = decode::<facade::PartiallyLiquidateCreditAccount>(log)?;
        let pos = intern(cfg, st, m.market, ev.creditAccount)?;
        let tok = m.token(ev.token).ok_or(ProtocolError::MalformedLog)?;
        let d = st.debt(pos, UNDERLYING_SLOT)?;
        st.set_debt(pos, UNDERLYING_SLOT, add_u128(d, ev.repaidDebt, false)?)?;
        let s = st.supply(pos, tok.slot)?;
        st.set_supply(pos, tok.slot, add_u128(s, ev.seizedCollateral, false)?)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == facade::AddCollateral::SIGNATURE_HASH {
        let ev = decode::<facade::AddCollateral>(log)?;
        let pos = intern(cfg, st, m.market, ev.creditAccount)?;
        let tok = m.token(ev.token).ok_or(ProtocolError::MalformedLog)?;
        let s = st.supply(pos, tok.slot)?;
        st.set_supply(pos, tok.slot, add_u128(s, ev.amount, true)?)?;
        let mut ex = extra(st, pos)?;
        enable_bit(&mut ex, tok.slot)?;
        set_extra(st, pos, ex)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == facade::WithdrawCollateral::SIGNATURE_HASH {
        let ev = decode::<facade::WithdrawCollateral>(log)?;
        let pos = intern(cfg, st, m.market, ev.creditAccount)?;
        let tok = m.token(ev.token).ok_or(ProtocolError::MalformedLog)?;
        let s = st.supply(pos, tok.slot)?;
        st.set_supply(pos, tok.slot, add_u128(s, ev.amount, false)?)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == facade::WithdrawPhantomToken::SIGNATURE_HASH {
        let ev = decode::<facade::WithdrawPhantomToken>(log)?;
        let pos = intern(cfg, st, m.market, ev.creditAccount)?;
        let Some(tok) = m.token(ev.token) else {
            return Ok(positions(&[pos]));
        };
        let s = st.supply(pos, tok.slot)?;
        st.set_supply(pos, tok.slot, add_u128(s, ev.amount, false)?)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == facade::Paused::SIGNATURE_HASH {
        let rows = patch_manager(st, m.market, Some(ts), |b| {
            b.flags |= ManagerRow::PAUSED;
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(rows));
    }
    if topic0 == facade::Unpaused::SIGNATURE_HASH {
        let rows = patch_manager(st, m.market, Some(ts), |b| {
            b.flags &= !ManagerRow::PAUSED;
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(rows));
    }
    if topic0 == facade::StartMultiCall::SIGNATURE_HASH
        || topic0 == facade::FinishMultiCall::SIGNATURE_HASH
        || topic0 == facade::Execute::SIGNATURE_HASH
    {
        return Ok(DirtySet::None);
    }
    Ok(DirtySet::None)
}

fn manager_log(
    cfg: &Config,
    _st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    i: usize,
    topic0: B256,
) -> Result<DirtySet> {
    let m = cfg.manager(i).ok_or(ProtocolError::Internal)?;
    if topic0 == manager::SetCreditConfigurator::SIGNATURE_HASH {
        let ev = decode::<manager::SetCreditConfigurator>(log)?;
        if ev.newConfigurator != m.configurator {
            return halt_after(cfg, log);
        }
        return Ok(DirtySet::None);
    }
    Ok(DirtySet::None)
}

fn configurator_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    i: usize,
    topic0: B256,
) -> Result<DirtySet> {
    let m = cfg.manager(i).ok_or(ProtocolError::Internal)?;
    let ts = last_update(log.timestamp)?;
    ensure_listed(cfg, st, m, ts)?;
    if topic0 == configurator::UpdateFees::SIGNATURE_HASH {
        let ev = decode::<configurator::UpdateFees>(log)?;
        let pf = pf_u16()?;
        let disc = pf
            .checked_sub(ev.liquidationPremium)
            .ok_or(ProtocolError::MalformedLog)?;
        let disc_e = pf
            .checked_sub(ev.liquidationPremiumExpired)
            .ok_or(ProtocolError::MalformedLog)?;
        // G2. `ltUnderlying = liquidationDiscount - feeLiquidation` is an
        // immutable identity (`CreditManagerV3.sol:184`, and
        // `CreditConfiguratorV3.sol:424` refuses any `setFees` that would
        // change it) — so it is re-derived here, on every fee update, rather
        // than read from a per-token LT slot the underlying never has.
        let lt_underlying = u16::try_from(
            U256::from(disc)
                .checked_sub(U256::from(ev.feeLiquidation))
                .ok_or(ProtocolError::MalformedLog)?,
        )
        .map_err(|_| ProtocolError::MalformedLog)?;
        let rows = patch_manager(st, m.market, Some(ts), |b| {
            b.fee_liquidation = ev.feeLiquidation;
            b.liquidation_discount = disc;
            b.fee_liquidation_expired = ev.feeLiquidationExpired;
            b.liquidation_discount_expired = disc_e;
            b.lt_underlying = lt_underlying;
            b.flags |= ManagerRow::FEES;
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(rows));
    }
    if topic0 == configurator::SetBorrowingLimits::SIGNATURE_HASH {
        let ev = decode::<configurator::SetBorrowingLimits>(log)?;
        let min = u128::try_from(ev.minDebt).map_err(|_| ProtocolError::MalformedLog)?;
        let rows = patch_manager(st, m.market, Some(ts), |b| {
            b.min_debt = min.to_be_bytes();
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(rows));
    }
    if topic0 == configurator::SetTokenLiquidationThreshold::SIGNATURE_HASH {
        let ev = decode::<configurator::SetTokenLiquidationThreshold>(log)?;
        let Some(tok) = m.token(ev.token) else {
            return Ok(DirtySet::None);
        };
        if tok.slot == UNDERLYING_SLOT {
            // G2. `setCollateralTokenData` (pin `510fc654`) reverts for the
            // underlying, so the real contract cannot emit this event with
            // `token == underlying` — `ev.liquidationThreshold` here would be
            // untrusted data from a call path that should not exist. Never
            // write `lt_underlying` from it; the value is derived only from
            // `fees` (see `UpdateFees` above and the initial listing).
            return Ok(DirtySet::None);
        }
        let rows = patch_token(st, m.market, tok.slot, Some(ts), |b| {
            b.lt_initial = ev.liquidationThreshold;
            b.lt_final = ev.liquidationThreshold;
            b.ramp_start = STATIC_LT_RAMP_START;
            b.ramp_duration = 0;
            b.flags |= TokenRow::LISTED;
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(rows));
    }
    if topic0 == configurator::ScheduleTokenLiquidationThresholdRamp::SIGNATURE_HASH {
        let ev = decode::<configurator::ScheduleTokenLiquidationThresholdRamp>(log)?;
        let Some(tok) = m.token(ev.token) else {
            return Ok(DirtySet::None);
        };
        if tok.slot == UNDERLYING_SLOT {
            return Ok(DirtySet::None);
        }
        if ev.timestampRampEnd < ev.timestampRampStart {
            return Err(ProtocolError::MalformedLog);
        }
        let start = u64::try_from(U256::from(ev.timestampRampStart))
            .map_err(|_| ProtocolError::MalformedLog)?;
        let end = u64::try_from(U256::from(ev.timestampRampEnd))
            .map_err(|_| ProtocolError::MalformedLog)?;
        let dur = end.checked_sub(start).ok_or(ProtocolError::MalformedLog)?;
        let duration = u32::try_from(dur).map_err(|_| ProtocolError::MalformedLog)?;
        let rows = patch_token(st, m.market, tok.slot, Some(ts), |b| {
            b.lt_initial = ev.liquidationThresholdInitial;
            b.lt_final = ev.liquidationThresholdFinal;
            b.ramp_start = start;
            b.ramp_duration = duration;
            b.flags |= TokenRow::LISTED;
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(rows));
    }
    if topic0 == configurator::AddCollateralToken::SIGNATURE_HASH {
        return Ok(DirtySet::None);
    }
    if topic0 == configurator::SetExpirationDate::SIGNATURE_HASH {
        let ev = decode::<configurator::SetExpirationDate>(log)?;
        let exp = u64::try_from(U256::from(ev.expirationDate))
            .map_err(|_| ProtocolError::MalformedLog)?;
        if exp > u64::from(u32::MAX) {
            return Err(ProtocolError::MalformedLog);
        }
        let rows = patch_manager(st, m.market, Some(ts), |b| {
            b.expiration_date = u32::try_from(exp).map_err(|_| ProtocolError::MalformedLog)?;
            Ok(())
        })?;
        return Ok(DirtySet::MarketReprice(rows));
    }
    if topic0 == configurator::SetCreditFacade::SIGNATURE_HASH {
        let ev = decode::<configurator::SetCreditFacade>(log)?;
        if ev.creditFacade != m.facade {
            return halt_after(cfg, log);
        }
        return Ok(DirtySet::None);
    }
    if topic0 == configurator::SetPriceOracle::SIGNATURE_HASH
        || topic0 == configurator::CreditConfiguratorUpgraded::SIGNATURE_HASH
    {
        return halt_after(cfg, log);
    }
    if topic0 == configurator::ForbidToken::SIGNATURE_HASH
        || topic0 == configurator::AllowToken::SIGNATURE_HASH
        || topic0 == configurator::AllowAdapter::SIGNATURE_HASH
        || topic0 == configurator::ForbidAdapter::SIGNATURE_HASH
        || topic0 == configurator::SetMaxDebtPerBlockMultiplier::SIGNATURE_HASH
        || topic0 == configurator::SetLossPolicy::SIGNATURE_HASH
    {
        return Ok(DirtySet::None);
    }
    Ok(DirtySet::None)
}

fn pool_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    topic0: B256,
) -> Result<DirtySet> {
    if topic0 == pool::Borrow::SIGNATURE_HASH {
        let ev = decode::<pool::Borrow>(log)?;
        let Some((_, m)) = cfg.manager_by_addr(ev.creditManager) else {
            return halt_after(cfg, log);
        };
        let ts = last_update(log.timestamp)?;
        ensure_listed(cfg, st, m, ts)?;
        let pos = intern(cfg, st, m.market, ev.creditAccount)?;
        let d = st.debt(pos, UNDERLYING_SLOT)?;
        st.set_debt(pos, UNDERLYING_SLOT, add_u128(d, ev.amount, true)?)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == pool::Repay::SIGNATURE_HASH
        || topic0 == pool::AddCreditManager::SIGNATURE_HASH
        || topic0 == pool::SetInterestRateModel::SIGNATURE_HASH
        || topic0 == pool::SetPoolQuotaKeeper::SIGNATURE_HASH
        || topic0 == pool::SetTotalDebtLimit::SIGNATURE_HASH
        || topic0 == pool::SetCreditManagerDebtLimit::SIGNATURE_HASH
        || topic0 == pool::SetWithdrawFee::SIGNATURE_HASH
        || topic0 == pool::IncurUncoveredLoss::SIGNATURE_HASH
        || topic0 == pool::Refer::SIGNATURE_HASH
    {
        return Ok(DirtySet::None);
    }
    Ok(DirtySet::None)
}

fn factory_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    topic0: B256,
) -> Result<DirtySet> {
    if topic0 == factory::AddCreditManager::SIGNATURE_HASH {
        let ev = decode::<factory::AddCreditManager>(log)?;
        let Some((_, m)) = cfg.manager_by_addr(ev.creditManager) else {
            return halt_after(cfg, log);
        };
        let ts = last_update(log.timestamp)?;
        ensure_listed(cfg, st, m, ts)?;
        return Ok(DirtySet::MarketReprice(dirty_slot(
            m.market,
            UNDERLYING_SLOT,
        )));
    }
    if topic0 == factory::DeployCreditAccount::SIGNATURE_HASH {
        let ev = decode::<factory::DeployCreditAccount>(log)?;
        let Some((_, m)) = cfg.manager_by_addr(ev.creditManager) else {
            return halt_after(cfg, log);
        };
        let ts = last_update(log.timestamp)?;
        ensure_listed(cfg, st, m, ts)?;
        let pos = intern(cfg, st, m.market, ev.creditAccount)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == factory::TakeCreditAccount::SIGNATURE_HASH {
        let ev = decode::<factory::TakeCreditAccount>(log)?;
        let Some((_, m)) = cfg.manager_by_addr(ev.creditManager) else {
            return halt_after(cfg, log);
        };
        let ts = last_update(log.timestamp)?;
        ensure_listed(cfg, st, m, ts)?;
        let pos = intern(cfg, st, m.market, ev.creditAccount)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == factory::ReturnCreditAccount::SIGNATURE_HASH {
        return Ok(DirtySet::None);
    }
    if topic0 == factory::Rescue::SIGNATURE_HASH {
        return halt_after(cfg, log);
    }
    Ok(DirtySet::None)
}

fn quota_log(
    cfg: &Config,
    st: &mut dyn StateWriter,
    log: &DecodedLog<'_>,
    topic0: B256,
) -> Result<DirtySet> {
    if topic0 == quota::UpdateQuota::SIGNATURE_HASH {
        let ev = decode::<quota::UpdateQuota>(log)?;
        let Some((m, tok)) = cfg
            .managers
            .iter()
            .find_map(|mgr| mgr.token(ev.token).map(|t| (mgr, t)))
        else {
            return Ok(DirtySet::None);
        };
        let ts = last_update(log.timestamp)?;
        ensure_listed(cfg, st, m, ts)?;
        let pos = intern(cfg, st, m.market, ev.creditAccount)?;
        let mut q = quota_at(st, pos, tok.slot)?;
        let delta = i96_to_i256(ev.quotaChange)?;
        let cur = I256::try_from(q.quota).map_err(|_| ProtocolError::MalformedLog)?;
        let next = cur
            .checked_add(delta)
            .ok_or(ProtocolError::Fixed(FixedError::Overflow))?;
        let next_u =
            U256::try_from(next).map_err(|_| ProtocolError::Fixed(FixedError::Underflow))?;
        q.quota = u128::try_from(next_u).map_err(|_| ProtocolError::MalformedLog)?;
        q.flags |= QuotaExtra::QUOTED;
        set_quota(st, pos, tok.slot, q)?;
        let mut ex = extra(st, pos)?;
        enable_bit(&mut ex, tok.slot)?;
        set_extra(st, pos, ex)?;
        return Ok(positions(&[pos]));
    }
    if topic0 == quota::UpdateTokenQuotaRate::SIGNATURE_HASH
        || topic0 == quota::SetGauge::SIGNATURE_HASH
        || topic0 == quota::AddCreditManager::SIGNATURE_HASH
        || topic0 == quota::AddQuotaToken::SIGNATURE_HASH
        || topic0 == quota::SetTokenLimit::SIGNATURE_HASH
        || topic0 == quota::SetQuotaIncreaseFee::SIGNATURE_HASH
    {
        return Ok(DirtySet::None);
    }
    Ok(DirtySet::None)
}
