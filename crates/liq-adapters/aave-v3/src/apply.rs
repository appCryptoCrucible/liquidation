//! `Protocol::apply_log` for Aave V3. Journal-before-write is the writer's
//! contract. Known-address / unknown-topic → `DirtySet::None`.

use alloy_primitives::{Address, U256};
use alloy_sol_types::SolEvent;
use bytemuck::Zeroable;
use liq_protocol::{
    DecodedLog, DirtyPositions, DirtyRows, DirtySet, MarketFlags, MarketRow, MarketSlot,
    ProtocolError, Result, StateWriter,
};
use liq_types::fixed::FixedError;
use liq_types::{MarketId, PositionId, PositionKey};
use smallvec::SmallVec;

use crate::config::{Config, Emitter};
use crate::events::{cfg as ccfg, halt, oracle, pool, provider, sentinel, token};
use crate::layout::{EModeCat, PoolMeta, Reserve, UserExtra, UserReserve, META_ASSET};
use crate::math::{debt_burn_scaled, debt_mint_scaled, supply_burn_scaled, supply_mint_scaled};

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
fn row_at(market: MarketId, slot: u16) -> DirtyRows {
    let mut d = DirtyRows::new();
    d.push(MarketSlot { market, slot });
    d
}

fn derive_flags(r: &Reserve) -> MarketFlags {
    let mut f = 0u8;
    if r.flags & Reserve::PAUSED != 0 {
        f |= MarketFlags::PAUSED.0;
    }
    if r.flags & Reserve::FROZEN != 0 {
        f |= MarketFlags::FROZEN.0;
    }
    if r.flags & Reserve::SILOED != 0 {
        f |= MarketFlags::SILOED.0;
    }
    if r.flags & Reserve::ISOLATED != 0 {
        f |= MarketFlags::ISOLATED.0;
    }
    if r.flags & Reserve::PRICED == 0 {
        f |= MarketFlags::UNPRICED.0;
    }
    MarketFlags(f)
}

fn emitter(cfg: &Config, st: &dyn StateWriter, address: Address) -> Result<Option<Emitter>> {
    for (i, p) in cfg.pools.iter().enumerate() {
        if p.address == address {
            return Ok(Some(Emitter::Pool(p.market)));
        }
        if p.configurator == address {
            return Ok(Some(Emitter::Configurator(i)));
        }
        if p.oracle == address {
            return Ok(Some(Emitter::Oracle(i)));
        }
        if p.provider == address {
            return Ok(Some(Emitter::Provider(i)));
        }
        if p.sentinel != Address::ZERO && p.sentinel == address {
            return Ok(Some(Emitter::Sentinel(i)));
        }
        if p.sequencer_oracle != Address::ZERO && p.sequencer_oracle == address {
            return Ok(Some(Emitter::Sequencer(i)));
        }
        if let Ok(rows) = st.markets(p.market) {
            for (slot, row) in rows.iter().enumerate() {
                if slot == 0 {
                    continue;
                }
                let Ok(r) = row.body::<Reserve>() else {
                    continue;
                };
                let slot = u16::try_from(slot).map_err(|_| ProtocolError::MalformedLog)?;
                if Address::from(r.a_token) == address {
                    return Ok(Some(Emitter::AToken { pool: i, slot }));
                }
                if Address::from(r.v_token) == address {
                    return Ok(Some(Emitter::VToken { pool: i, slot }));
                }
            }
        }
    }
    Ok(None)
}

fn slot_by_underlying(
    cfg: &Config,
    st: &dyn StateWriter,
    market: MarketId,
    underlying: Address,
) -> Result<u16> {
    let ac = cfg
        .asset_by_underlying(underlying)
        .ok_or(ProtocolError::UnknownMarket(market))?;
    let rows = st.markets(market)?;
    for (i, row) in rows.iter().enumerate().skip(1) {
        if row.asset == ac.asset {
            return u16::try_from(i).map_err(|_| ProtocolError::MalformedLog);
        }
    }
    Err(ProtocolError::UnknownMarket(market))
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

fn set_user_flag(st: &mut dyn StateWriter, pos: PositionId, slot: u16, on: bool) -> Result<()> {
    let mut extra = *st.slot_extra(pos, slot)?;
    {
        let u: &mut UserReserve = extra.view_mut()?;
        if on {
            u.flags |= UserReserve::USING_AS_COLLATERAL;
        } else {
            u.flags &= !UserReserve::USING_AS_COLLATERAL;
        }
    }
    st.set_slot_extra(pos, slot, extra)
}

fn add_supply(
    st: &mut dyn StateWriter,
    pos: PositionId,
    slot: u16,
    d: U256,
    add: bool,
) -> Result<()> {
    let cur = U256::from(st.supply(pos, slot)?);
    let next = if add {
        cur.checked_add(d).ok_or(FixedError::Overflow)?
    } else {
        cur.checked_sub(d).ok_or(FixedError::Underflow)?
    };
    st.set_supply(pos, slot, narrow(next)?)
}

fn add_debt(
    st: &mut dyn StateWriter,
    pos: PositionId,
    slot: u16,
    d: U256,
    add: bool,
) -> Result<()> {
    let cur = U256::from(st.debt(pos, slot)?);
    let next = if add {
        cur.checked_add(d).ok_or(FixedError::Overflow)?
    } else {
        cur.checked_sub(d).ok_or(FixedError::Underflow)?
    };
    st.set_debt(pos, slot, narrow(next)?)
}

fn update_reserve(
    st: &mut dyn StateWriter,
    market: MarketId,
    slot: u16,
    ts: Option<u32>,
    f: impl FnOnce(&mut Reserve) -> Result<()>,
) -> Result<DirtyRows> {
    let at = MarketSlot { market, slot };
    let mut row = *st.market(at)?;
    f(row.body_mut::<Reserve>()?)?;
    if let Some(t) = ts {
        row.last_update = t;
    }
    row.flags = derive_flags(row.body::<Reserve>()?);
    st.set_market(at, row)?;
    Ok(row_at(market, slot))
}

const HALT: &[alloy_primitives::B256] = &[
    halt::Upgraded::SIGNATURE_HASH,
    halt::AdminChanged::SIGNATURE_HASH,
    halt::Initialized::SIGNATURE_HASH,
    halt::AuthorityUpdated::SIGNATURE_HASH,
    provider::PoolUpdated::SIGNATURE_HASH,
    provider::PoolConfiguratorUpdated::SIGNATURE_HASH,
    provider::PriceOracleUpdated::SIGNATURE_HASH,
    provider::ACLManagerUpdated::SIGNATURE_HASH,
    provider::ACLAdminUpdated::SIGNATURE_HASH,
    provider::PriceOracleSentinelUpdated::SIGNATURE_HASH,
    provider::ProxyCreated::SIGNATURE_HASH,
    provider::AddressSet::SIGNATURE_HASH,
    provider::AddressSetAsProxy::SIGNATURE_HASH,
    ccfg::ATokenUpgraded::SIGNATURE_HASH,
    ccfg::VariableDebtTokenUpgraded::SIGNATURE_HASH,
    oracle::FallbackOracleUpdated::SIGNATURE_HASH,
    oracle::BaseCurrencySet::SIGNATURE_HASH,
    sentinel::SequencerOracleUpdated::SIGNATURE_HASH,
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
    // Known-addr / unknown-topic (vToken BorrowAllowanceDelegated, aAAVE DelegateChanged).
    match em {
        Emitter::Pool(market) => apply_pool(cfg, st, market, topic0, log),
        Emitter::Configurator(i) => apply_cfg(cfg, st, i, topic0, log),
        Emitter::Oracle(i) => apply_oracle(cfg, st, i, topic0, log),
        Emitter::Provider(_) => Ok(DirtySet::None),
        Emitter::Sentinel(i) => apply_sentinel(cfg, st, i, topic0, log),
        Emitter::Sequencer(i) => apply_sequencer(cfg, st, i, topic0, log),
        Emitter::AToken { pool, slot } => apply_atoken(cfg, st, pool, slot, topic0, log),
        Emitter::VToken { pool, slot } => apply_vtoken(cfg, st, pool, slot, topic0, log),
    }
}

fn apply_pool(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    topic0: alloy_primitives::B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    match topic0 {
        pool::ReserveDataUpdated::SIGNATURE_HASH => {
            let ev: pool::ReserveDataUpdated = decode(log)?;
            let slot = slot_by_underlying(cfg, st, market, ev.reserve)?;
            let rows = update_reserve(st, market, slot, Some(last_update(log.timestamp)?), |r| {
                r.liquidity_rate = narrow(ev.liquidityRate)?;
                r.variable_borrow_rate = narrow(ev.variableBorrowRate)?;
                r.liquidity_index = narrow(ev.liquidityIndex)?;
                r.variable_borrow_index = narrow(ev.variableBorrowIndex)?;
                Ok(())
            })?;
            Ok(DirtySet::MarketAccrual(rows))
        }
        pool::Supply::SIGNATURE_HASH => {
            let ev: pool::Supply = decode(log)?;
            let slot = slot_by_underlying(cfg, st, market, ev.reserve)?;
            let idx = U256::from(
                st.market(MarketSlot { market, slot })?
                    .body::<Reserve>()?
                    .liquidity_index,
            );
            let scaled = supply_mint_scaled(cfg.liquidation.balance_model, ev.amount, idx)?;
            let id = intern(cfg, st, market, ev.onBehalfOf)?;
            add_supply(st, id, slot, scaled, true)?;
            Ok(positions(&[id]))
        }
        pool::Withdraw::SIGNATURE_HASH => {
            let ev: pool::Withdraw = decode(log)?;
            let slot = slot_by_underlying(cfg, st, market, ev.reserve)?;
            let idx = U256::from(
                st.market(MarketSlot { market, slot })?
                    .body::<Reserve>()?
                    .liquidity_index,
            );
            let scaled = supply_burn_scaled(cfg.liquidation.balance_model, ev.amount, idx)?;
            let id = intern(cfg, st, market, ev.user)?;
            add_supply(st, id, slot, scaled, false)?;
            Ok(positions(&[id]))
        }
        pool::Borrow::SIGNATURE_HASH => {
            let ev: pool::Borrow = decode(log)?;
            let slot = slot_by_underlying(cfg, st, market, ev.reserve)?;
            let idx = U256::from(
                st.market(MarketSlot { market, slot })?
                    .body::<Reserve>()?
                    .variable_borrow_index,
            );
            let scaled = debt_mint_scaled(cfg.liquidation.balance_model, ev.amount, idx)?;
            let id = intern(cfg, st, market, ev.onBehalfOf)?;
            add_debt(st, id, slot, scaled, true)?;
            Ok(positions(&[id]))
        }
        pool::Repay::SIGNATURE_HASH => {
            let ev: pool::Repay = decode(log)?;
            let slot = slot_by_underlying(cfg, st, market, ev.reserve)?;
            let idx = U256::from(
                st.market(MarketSlot { market, slot })?
                    .body::<Reserve>()?
                    .variable_borrow_index,
            );
            let scaled = debt_burn_scaled(cfg.liquidation.balance_model, ev.amount, idx)?;
            let id = intern(cfg, st, market, ev.user)?;
            add_debt(st, id, slot, scaled, false)?;
            if ev.useATokens {
                let li = U256::from(
                    st.market(MarketSlot { market, slot })?
                        .body::<Reserve>()?
                        .liquidity_index,
                );
                add_supply(
                    st,
                    id,
                    slot,
                    supply_burn_scaled(cfg.liquidation.balance_model, ev.amount, li)?,
                    false,
                )?;
            }
            Ok(positions(&[id]))
        }
        pool::LiquidationCall::SIGNATURE_HASH => {
            let ev: pool::LiquidationCall = decode(log)?;
            let cslot = slot_by_underlying(cfg, st, market, ev.collateralAsset)?;
            let dslot = slot_by_underlying(cfg, st, market, ev.debtAsset)?;
            let crow = st.market(MarketSlot {
                market,
                slot: cslot,
            })?;
            let drow = st.market(MarketSlot {
                market,
                slot: dslot,
            })?;
            let li = U256::from(crow.body::<Reserve>()?.liquidity_index);
            let di = U256::from(drow.body::<Reserve>()?.variable_borrow_index);
            let id = intern(cfg, st, market, ev.user)?;
            add_supply(
                st,
                id,
                cslot,
                supply_burn_scaled(
                    cfg.liquidation.balance_model,
                    ev.liquidatedCollateralAmount,
                    li,
                )?,
                false,
            )?;
            add_debt(
                st,
                id,
                dslot,
                debt_burn_scaled(cfg.liquidation.balance_model, ev.debtToCover, di)?,
                false,
            )?;
            Ok(positions(&[id]))
        }
        pool::UserEModeSet::SIGNATURE_HASH => {
            let ev: pool::UserEModeSet = decode(log)?;
            let id = intern(cfg, st, market, ev.user)?;
            let mut extra = *st.extra(id)?;
            extra.view_mut::<UserExtra>()?.emode = ev.categoryId;
            st.set_extra(id, extra)?;
            Ok(positions(&[id]))
        }
        pool::ReserveUsedAsCollateralEnabled::SIGNATURE_HASH => {
            let ev: pool::ReserveUsedAsCollateralEnabled = decode(log)?;
            let slot = slot_by_underlying(cfg, st, market, ev.reserve)?;
            let id = intern(cfg, st, market, ev.user)?;
            set_user_flag(st, id, slot, true)?;
            Ok(positions(&[id]))
        }
        pool::ReserveUsedAsCollateralDisabled::SIGNATURE_HASH => {
            let ev: pool::ReserveUsedAsCollateralDisabled = decode(log)?;
            let slot = slot_by_underlying(cfg, st, market, ev.reserve)?;
            let id = intern(cfg, st, market, ev.user)?;
            set_user_flag(st, id, slot, false)?;
            Ok(positions(&[id]))
        }
        pool::DeficitCreated::SIGNATURE_HASH => {
            // A7. `LiquidationLogic._burnBadDebt` (pin 8305565ae) burns the
            // user's ENTIRE remaining debt on this reserve and books it as
            // protocol deficit:
            //
            // ```solidity
            // uint256 userDebt = IVariableDebtToken(...)
            //     .scaledBalanceOf(user).rayMul(reserveCache.nextVariableBorrowIndex);
            // _burnDebtTokens(..., userDebt, ...);
            // reserve.deficit += userDebt.toUint128();
            // emit DeficitCreated(user, reserveAddress, userDebt);
            // ```
            //
            // Only the reserve side was recorded here, so the borrower kept a
            // debt row the pool had already written off — phantom debt that
            // made the position read liquidatable forever and produced quotes
            // against a reserve with nothing left to repay. `amountCreated` is
            // the whole balance by construction, so the user's scaled debt on
            // this slot goes to zero.
            let ev: pool::DeficitCreated = decode(log)?;
            let slot = slot_by_underlying(cfg, st, market, ev.debtAsset)?;
            let id = intern(cfg, st, market, ev.user)?;
            st.set_debt(id, slot, 0)?;
            let rows = update_reserve(st, market, slot, None, |r| {
                r.deficit = r
                    .deficit
                    .checked_add(narrow(ev.amountCreated)?)
                    .ok_or(FixedError::Overflow)?;
                Ok(())
            })?;
            // `MarketAccrual` re-projects every position holding this slot,
            // which includes `id` and so picks up the zeroed debt. It is the
            // wider of the two scopes this log touches, so reporting it alone
            // cannot under-report.
            Ok(DirtySet::MarketAccrual(rows))
        }
        pool::DeficitCovered::SIGNATURE_HASH => {
            let ev: pool::DeficitCovered = decode(log)?;
            let slot = slot_by_underlying(cfg, st, market, ev.reserve)?;
            let rows = update_reserve(st, market, slot, None, |r| {
                r.deficit = r
                    .deficit
                    .checked_sub(narrow(ev.amountCovered)?)
                    .ok_or(FixedError::Underflow)?;
                Ok(())
            })?;
            Ok(DirtySet::MarketAccrual(rows))
        }
        pool::MintedToTreasury::SIGNATURE_HASH
        | pool::FlashLoan::SIGNATURE_HASH
        | pool::PositionManagerApproved::SIGNATURE_HASH
        | pool::PositionManagerRevoked::SIGNATURE_HASH => Ok(DirtySet::None),
        _ => Ok(DirtySet::None),
    }
}

fn apply_cfg(
    cfg: &Config,
    st: &mut dyn StateWriter,
    idx: usize,
    topic0: alloy_primitives::B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let p = cfg.pools.get(idx).ok_or(ProtocolError::UnexpectedLog)?;
    let market = p.market;
    match topic0 {
        ccfg::ReserveInitialized::SIGNATURE_HASH => {
            let ev: ccfg::ReserveInitialized = decode(log)?;
            let have = u16::try_from(st.markets(market).map(|r| r.len()).unwrap_or(0))
                .map_err(|_| ProtocolError::MalformedLog)?;
            if have == 0 {
                let mut meta = MarketRow::blank(META_ASSET, 0);
                *meta.body_mut::<PoolMeta>()? = PoolMeta {
                    sentinel_present: u8::from(p.sentinel != Address::ZERO),
                    ..bytemuck::Zeroable::zeroed()
                };
                st.push_market(market, meta)?;
            }
            let ac = cfg.asset_by_underlying(ev.asset);
            let mut row = match ac {
                Some(a) => {
                    let mut r = MarketRow::blank(a.asset, a.decimals);
                    r.price_feed = a.feed;
                    r
                }
                None => {
                    let mut r = MarketRow::blank(crate::layout::UNMAPPED_ASSET, 18);
                    r.flags = MarketFlags::UNPRICED;
                    r
                }
            };
            let mut body = Reserve::zeroed();
            body.liquidity_index = {
                let ray = liq_types::fixed::RAY;
                u128::try_from(ray).map_err(|_| ProtocolError::MalformedLog)?
            };
            body.variable_borrow_index = body.liquidity_index;
            body.a_token = ev.aToken.into();
            body.v_token = ev.variableDebtToken.into();
            body.flags = Reserve::ACTIVE | Reserve::FLASH;
            if let Some(a) = ac {
                body.debt_ceiling = a.debt_ceiling;
                if a.siloed {
                    body.flags |= Reserve::SILOED;
                }
                if a.isolated {
                    body.flags |= Reserve::ISOLATED;
                }
            }
            if cfg.pinned_source(p.address, ev.asset).is_some() {
                body.flags |= Reserve::PRICED;
            }
            *row.body_mut::<Reserve>()? = body;
            row.flags = derive_flags(&body);
            row.last_update = last_update(log.timestamp)?;
            st.push_market(market, row)?;
            Ok(DirtySet::MarketReprice(row_at(market, have.max(1))))
        }
        ccfg::CollateralConfigurationChanged::SIGNATURE_HASH => {
            let ev: ccfg::CollateralConfigurationChanged = decode(log)?;
            let slot = slot_by_underlying(cfg, st, market, ev.asset)?;
            let rows = update_reserve(st, market, slot, None, |r| {
                r.ltv = narrow(ev.ltv)?;
                r.liq_threshold = narrow(ev.liquidationThreshold)?;
                r.liq_bonus = narrow(ev.liquidationBonus)?;
                Ok(())
            })?;
            Ok(DirtySet::MarketReprice(rows))
        }
        ccfg::ReservePaused::SIGNATURE_HASH => flag(cfg, st, market, log, topic0, |r, on| {
            if on {
                r.flags |= Reserve::PAUSED;
            } else {
                r.flags &= !Reserve::PAUSED;
            }
            Ok(())
        }),
        ccfg::ReserveFrozen::SIGNATURE_HASH => flag(cfg, st, market, log, topic0, |r, on| {
            if on {
                r.flags |= Reserve::FROZEN;
            } else {
                r.flags &= !Reserve::FROZEN;
            }
            Ok(())
        }),
        ccfg::ReserveActive::SIGNATURE_HASH => flag(cfg, st, market, log, topic0, |r, on| {
            if on {
                r.flags |= Reserve::ACTIVE;
            } else {
                r.flags &= !Reserve::ACTIVE;
            }
            Ok(())
        }),
        ccfg::ReserveBorrowing::SIGNATURE_HASH => flag(cfg, st, market, log, topic0, |r, on| {
            if on {
                r.flags |= Reserve::BORROWING;
            } else {
                r.flags &= !Reserve::BORROWING;
            }
            Ok(())
        }),
        ccfg::ReserveFlashLoaning::SIGNATURE_HASH => flag(cfg, st, market, log, topic0, |r, on| {
            if on {
                r.flags |= Reserve::FLASH;
            } else {
                r.flags &= !Reserve::FLASH;
            }
            Ok(())
        }),
        ccfg::LiquidationProtocolFeeChanged::SIGNATURE_HASH => {
            let ev: ccfg::LiquidationProtocolFeeChanged = decode(log)?;
            let slot = slot_by_underlying(cfg, st, market, ev.asset)?;
            let rows = update_reserve(st, market, slot, None, |r| {
                r.liq_protocol_fee = narrow(ev.newFee)?;
                Ok(())
            })?;
            Ok(DirtySet::MarketReprice(rows))
        }
        ccfg::LiquidationGracePeriodChanged::SIGNATURE_HASH => {
            let ev: ccfg::LiquidationGracePeriodChanged = decode(log)?;
            let slot = slot_by_underlying(cfg, st, market, ev.asset)?;
            let rows = update_reserve(st, market, slot, None, |r| {
                r.grace_until = u32::try_from(ev.gracePeriodUntil.to::<u64>())
                    .map_err(|_| ProtocolError::MalformedLog)?;
                Ok(())
            })?;
            Ok(DirtySet::MarketReprice(rows))
        }
        ccfg::LiquidationGracePeriodDisabled::SIGNATURE_HASH => {
            let ev: ccfg::LiquidationGracePeriodDisabled = decode(log)?;
            let slot = slot_by_underlying(cfg, st, market, ev.asset)?;
            let rows = update_reserve(st, market, slot, None, |r| {
                r.grace_until = 0;
                Ok(())
            })?;
            Ok(DirtySet::MarketReprice(rows))
        }
        ccfg::EModeCategoryAdded::SIGNATURE_HASH => {
            let ev: ccfg::EModeCategoryAdded = decode(log)?;
            let at = MarketSlot { market, slot: 0 };
            let mut row = *st.market(at)?;
            {
                let meta: &mut PoolMeta = row.body_mut()?;
                let slot = meta
                    .emode_slot_for(ev.categoryId)
                    .ok_or(ProtocolError::TableFull {
                        table: "aave-v3 PoolMeta::emode",
                        cap: PoolMeta::EMODE_CAP,
                    })?;
                if let Some(c) = meta.emode.get_mut(slot) {
                    *c = EModeCat {
                        id: ev.categoryId,
                        isolated: c.isolated,
                        ltv: narrow(ev.ltv)?,
                        liq_threshold: narrow(ev.liquidationThreshold)?,
                        liq_bonus: narrow(ev.liquidationBonus)?,
                    };
                }
            }
            st.set_market(at, row)?;
            Ok(DirtySet::MarketReprice(row_at(market, 0)))
        }
        ccfg::EModeCategoryIsolationChanged::SIGNATURE_HASH => {
            let ev: ccfg::EModeCategoryIsolationChanged = decode(log)?;
            let at = MarketSlot { market, slot: 0 };
            let mut row = *st.market(at)?;
            {
                let meta: &mut PoolMeta = row.body_mut()?;
                if let Some(c) = meta.emode.iter_mut().find(|c| c.id == ev.categoryId) {
                    c.isolated = u8::from(ev.isolated);
                }
            }
            st.set_market(at, row)?;
            Ok(DirtySet::MarketReprice(row_at(market, 0)))
        }
        ccfg::AssetCollateralInEModeChanged::SIGNATURE_HASH => {
            emode_bit(cfg, st, market, log, topic0, |r, mask, on| {
                if on {
                    r.emode_coll |= mask;
                } else {
                    r.emode_coll &= !mask;
                }
            })
        }
        ccfg::AssetBorrowableInEModeChanged::SIGNATURE_HASH => {
            emode_bit(cfg, st, market, log, topic0, |r, mask, on| {
                if on {
                    r.emode_borrow |= mask;
                } else {
                    r.emode_borrow &= !mask;
                }
            })
        }
        ccfg::AssetLtvzeroInEModeChanged::SIGNATURE_HASH => {
            emode_bit(cfg, st, market, log, topic0, |r, mask, on| {
                if on {
                    r.emode_ltv0 |= mask;
                } else {
                    r.emode_ltv0 &= !mask;
                }
            })
        }
        ccfg::FlashloanPremiumTotalUpdated::SIGNATURE_HASH => {
            let ev: ccfg::FlashloanPremiumTotalUpdated = decode(log)?;
            let at = MarketSlot { market, slot: 0 };
            let mut row = *st.market(at)?;
            row.body_mut::<PoolMeta>()?.flashloan_premium_total =
                narrow(U256::from(ev.newFlashloanPremiumTotal))?;
            st.set_market(at, row)?;
            Ok(DirtySet::None)
        }
        ccfg::ReserveFactorChanged::SIGNATURE_HASH
        | ccfg::BorrowCapChanged::SIGNATURE_HASH
        | ccfg::SupplyCapChanged::SIGNATURE_HASH
        | ccfg::PendingLtvChanged::SIGNATURE_HASH
        | ccfg::ReserveInterestRateDataChanged::SIGNATURE_HASH => {
            Ok(DirtySet::MarketReprice(DirtyRows::new()))
        }
        _ => Ok(DirtySet::None),
    }
}

fn flag(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    log: &DecodedLog<'_>,
    topic0: alloy_primitives::B256,
    f: impl FnOnce(&mut Reserve, bool) -> Result<()>,
) -> Result<DirtySet> {
    let (asset, on) = if topic0 == ccfg::ReservePaused::SIGNATURE_HASH {
        let ev: ccfg::ReservePaused = decode(log)?;
        (ev.asset, ev.paused)
    } else if topic0 == ccfg::ReserveFrozen::SIGNATURE_HASH {
        let ev: ccfg::ReserveFrozen = decode(log)?;
        (ev.asset, ev.frozen)
    } else if topic0 == ccfg::ReserveActive::SIGNATURE_HASH {
        let ev: ccfg::ReserveActive = decode(log)?;
        (ev.asset, ev.active)
    } else if topic0 == ccfg::ReserveBorrowing::SIGNATURE_HASH {
        let ev: ccfg::ReserveBorrowing = decode(log)?;
        (ev.asset, ev.enabled)
    } else {
        let ev: ccfg::ReserveFlashLoaning = decode(log)?;
        (ev.asset, ev.enabled)
    };
    let slot = slot_by_underlying(cfg, st, market, asset)?;
    let rows = update_reserve(st, market, slot, None, |r| f(r, on))?;
    Ok(DirtySet::MarketReprice(rows))
}

fn emode_bit(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    log: &DecodedLog<'_>,
    topic0: alloy_primitives::B256,
    set: impl FnOnce(&mut Reserve, crate::layout::EModeBits, bool),
) -> Result<DirtySet> {
    let (asset, cat, on) = if topic0 == ccfg::AssetCollateralInEModeChanged::SIGNATURE_HASH {
        let ev: ccfg::AssetCollateralInEModeChanged = decode(log)?;
        (ev.asset, ev.categoryId, ev.collateral)
    } else if topic0 == ccfg::AssetBorrowableInEModeChanged::SIGNATURE_HASH {
        let ev: ccfg::AssetBorrowableInEModeChanged = decode(log)?;
        (ev.asset, ev.categoryId, ev.borrowable)
    } else {
        let ev: ccfg::AssetLtvzeroInEModeChanged = decode(log)?;
        (ev.asset, ev.categoryId, ev.ltvzero)
    };
    let meta: PoolMeta = *st.market(MarketSlot { market, slot: 0 })?.body()?;
    // The category must already be in the table: Aave emits
    // `EModeCategoryAdded` before it can flag any asset into the category.
    let i = meta.emode_index(cat).ok_or(ProtocolError::Internal)?;
    let mask = PoolMeta::emode_mask(i).ok_or(ProtocolError::TableFull {
        table: "aave-v3 PoolMeta::emode",
        cap: PoolMeta::EMODE_CAP,
    })?;
    let slot = slot_by_underlying(cfg, st, market, asset)?;
    let rows = update_reserve(st, market, slot, None, |r| {
        set(r, mask, on);
        Ok(())
    })?;
    Ok(DirtySet::MarketReprice(rows))
}

fn apply_oracle(
    cfg: &Config,
    st: &mut dyn StateWriter,
    idx: usize,
    topic0: alloy_primitives::B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    if topic0 != oracle::AssetSourceUpdated::SIGNATURE_HASH {
        return Ok(DirtySet::None);
    }
    let ev: oracle::AssetSourceUpdated = decode(log)?;
    let p = cfg.pools.get(idx).ok_or(ProtocolError::UnexpectedLog)?;
    let Ok(slot) = slot_by_underlying(cfg, st, p.market, ev.asset) else {
        return Ok(DirtySet::None);
    };
    let pin = cfg.pinned_source(p.address, ev.asset);
    let rows = update_reserve(st, p.market, slot, None, |r| {
        if pin == Some(ev.source) {
            r.flags |= Reserve::PRICED;
        } else {
            r.flags &= !Reserve::PRICED;
        }
        Ok(())
    })?;
    Ok(DirtySet::MarketReprice(rows))
}

fn apply_sentinel(
    cfg: &Config,
    st: &mut dyn StateWriter,
    idx: usize,
    topic0: alloy_primitives::B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let p = cfg.pools.get(idx).ok_or(ProtocolError::UnexpectedLog)?;
    if topic0 == sentinel::GracePeriodUpdated::SIGNATURE_HASH {
        let ev: sentinel::GracePeriodUpdated = decode(log)?;
        let at = MarketSlot {
            market: p.market,
            slot: 0,
        };
        let mut row = *st.market(at)?;
        row.body_mut::<PoolMeta>()?.sentinel_grace = narrow(ev.newGracePeriod)?;
        st.set_market(at, row)?;
        return Ok(DirtySet::ProtocolWide);
    }
    Ok(DirtySet::None)
}

fn apply_sequencer(
    cfg: &Config,
    st: &mut dyn StateWriter,
    idx: usize,
    topic0: alloy_primitives::B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    if topic0 != sentinel::AnswerUpdated::SIGNATURE_HASH {
        return Ok(DirtySet::None);
    }
    let ev: sentinel::AnswerUpdated = decode(log)?;
    let p = cfg.pools.get(idx).ok_or(ProtocolError::UnexpectedLog)?;
    let at = MarketSlot {
        market: p.market,
        slot: 0,
    };
    let mut row = *st.market(at)?;
    {
        let m: &mut PoolMeta = row.body_mut()?;
        let ans = if ev.current.is_zero() { 0i8 } else { 1i8 };
        m.sequencer_answer = ans;
        m.sequencer_updated_at = narrow(ev.updatedAt)?;
    }
    st.set_market(at, row)?;
    Ok(DirtySet::ProtocolWide)
}

fn apply_atoken(
    cfg: &Config,
    st: &mut dyn StateWriter,
    pool_i: usize,
    slot: u16,
    topic0: alloy_primitives::B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let p = cfg.pools.get(pool_i).ok_or(ProtocolError::UnexpectedLog)?;
    if topic0 == token::BorrowAllowanceDelegated::SIGNATURE_HASH
        || topic0 == token::DelegateChanged::SIGNATURE_HASH
        || topic0 == token::Mint::SIGNATURE_HASH
        || topic0 == token::Burn::SIGNATURE_HASH
        || topic0 == token::Transfer::SIGNATURE_HASH
    {
        return Ok(DirtySet::None);
    }
    if topic0 != token::BalanceTransfer::SIGNATURE_HASH {
        return Ok(DirtySet::None);
    }
    let ev: token::BalanceTransfer = decode(log)?;
    // `BalanceTransfer.value` IS the scaled amount. `AToken._transfer` emits
    //     emit BalanceTransfer(from, to, amount.rayDiv(index), index);
    // so the division by the liquidity index has already happened on-chain —
    // `docs/coverage/aave-v3.md:43` records this as "scaledAmount param".
    //
    // Scaling it a second time moved a balance short by `index / RAY` on every
    // aToken transfer, which silently drains supply from the sender's row and
    // under-credits the receiver's on every collateral move.
    let scaled = ev.value;
    let mut ids: SmallVec<[PositionId; 2]> = SmallVec::new();
    if ev.from != Address::ZERO {
        let id = intern(cfg, st, p.market, ev.from)?;
        add_supply(st, id, slot, scaled, false)?;
        ids.push(id);
    }
    if ev.to != Address::ZERO {
        let id = intern(cfg, st, p.market, ev.to)?;
        add_supply(st, id, slot, scaled, true)?;
        ids.push(id);
    }
    Ok(positions(&ids))
}

fn apply_vtoken(
    _cfg: &Config,
    _st: &mut dyn StateWriter,
    _pool_i: usize,
    _slot: u16,
    topic0: alloy_primitives::B256,
    _log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    if topic0 == token::BorrowAllowanceDelegated::SIGNATURE_HASH
        || topic0 == token::DelegateChanged::SIGNATURE_HASH
        || topic0 == token::Mint::SIGNATURE_HASH
        || topic0 == token::Burn::SIGNATURE_HASH
        || topic0 == token::Transfer::SIGNATURE_HASH
    {
        return Ok(DirtySet::None);
    }
    Ok(DirtySet::None)
}
