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
use crate::events::{cfg as ccfg, halt, oracle, pool, provider, sentinel, stable, token, v2};
use crate::layout::{
    emode_place, EModeCat, EModeRow, PoolMeta, Reserve, UserExtra, UserReserve, EMODE_ROWS,
    FIRST_RESERVE, META_ASSET,
};
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
        if p.grace_sentinel != Address::ZERO && p.grace_sentinel == address {
            return Ok(Some(Emitter::GraceSentinel(i)));
        }
        if let Ok(rows) = st.markets(p.market) {
            for (slot, row) in rows.iter().enumerate().skip(usize::from(FIRST_RESERVE)) {
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
                if r.s_token != [0u8; 20] && Address::from(r.s_token) == address {
                    return Ok(Some(Emitter::SToken { pool: i, slot }));
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
    for (i, row) in rows.iter().enumerate().skip(usize::from(FIRST_RESERVE)) {
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
    let v2 = cfg.liquidation.version == crate::config::AaveVersion::V2;
    if HALT.contains(&topic0) || (v2 && HALT_V2.contains(&topic0)) {
        if log.block <= cfg.pinned_through {
            return Ok(DirtySet::None);
        }
        return halt_market(cfg, st, em, log.address, topic0);
    }
    if v2 {
        return match em {
            Emitter::Pool(market) => apply_pool_v2(cfg, st, market, topic0, log),
            Emitter::Configurator(i) => apply_cfg_v2(cfg, st, i, topic0, log),
            Emitter::Oracle(i) => apply_oracle(cfg, st, i, topic0, log),
            Emitter::Provider(_) | Emitter::Sentinel(_) | Emitter::Sequencer(_) => {
                Ok(DirtySet::None)
            }
            Emitter::GraceSentinel(i) => apply_grace_v2(cfg, st, i, topic0, log),
            Emitter::AToken { pool, slot } => apply_atoken_v2(cfg, st, pool, slot, topic0, log),
            Emitter::VToken { pool, slot } => apply_vtoken_v2(cfg, st, pool, slot, topic0, log),
            Emitter::SToken { pool, slot } => apply_stoken(cfg, st, pool, slot, topic0, log),
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
        Emitter::GraceSentinel(_) => Ok(DirtySet::None),
        Emitter::AToken { pool, slot } => apply_atoken(cfg, st, pool, slot, topic0, log),
        Emitter::VToken { pool, slot } => apply_vtoken(cfg, st, pool, slot, topic0, log),
        Emitter::SToken { pool, slot } => apply_stoken(cfg, st, pool, slot, topic0, log),
    }
}

/// Aave V2 halt-class logs beside [`HALT`]: the provider's address
/// changes and the stable debt tokens' upgrades.
const HALT_V2: &[alloy_primitives::B256] = &[
    v2::provider::LendingPoolUpdated::SIGNATURE_HASH,
    v2::provider::ConfigurationAdminUpdated::SIGNATURE_HASH,
    v2::provider::EmergencyAdminUpdated::SIGNATURE_HASH,
    v2::provider::LendingPoolConfiguratorUpdated::SIGNATURE_HASH,
    v2::provider::LendingPoolCollateralManagerUpdated::SIGNATURE_HASH,
    v2::provider::PriceOracleUpdated::SIGNATURE_HASH,
    v2::provider::LendingRateOracleUpdated::SIGNATURE_HASH,
    v2::provider::ProxyCreated::SIGNATURE_HASH,
    v2::provider::AddressSet::SIGNATURE_HASH,
    v2::cfg::StableDebtTokenUpgraded::SIGNATURE_HASH,
];

/// The market a halt-class log's emitter belongs to is halted
/// ([`PoolMeta::halted`]): nothing in it is liquidated until a person
/// re-pins the config. The bot and every other market keep running.
fn halt_market(
    cfg: &Config,
    st: &mut dyn StateWriter,
    em: Emitter,
    emitter: Address,
    topic0: alloy_primitives::B256,
) -> Result<DirtySet> {
    let pool = match em {
        Emitter::Pool(m) => cfg.pools.iter().position(|p| p.market == m),
        Emitter::Configurator(i)
        | Emitter::Oracle(i)
        | Emitter::Provider(i)
        | Emitter::Sentinel(i)
        | Emitter::Sequencer(i)
        | Emitter::GraceSentinel(i)
        | Emitter::AToken { pool: i, .. }
        | Emitter::VToken { pool: i, .. }
        | Emitter::SToken { pool: i, .. } => Some(i),
    };
    let Some(market) = pool.and_then(|i| cfg.pools.get(i)).map(|p| p.market) else {
        return Err(ProtocolError::HaltSignal);
    };
    tracing::error!(
        market = market.0,
        %emitter,
        %topic0,
        "aave market emitted a halt-class log after the pin: this market is halted (no liquidations) until the config is re-pinned; other markets continue"
    );
    let at = MarketSlot { market, slot: 0 };
    let Ok(cur) = st.market(at) else {
        // No reserve listed yet: nothing to liquidate, nothing to mark.
        return Ok(DirtySet::None);
    };
    let mut row = *cur;
    row.body_mut::<PoolMeta>()?.halted = 1;
    st.set_market(at, row)?;
    Ok(DirtySet::ProtocolWide)
}

/// Aave V2 pool events. Balances come from the tokens' events (V2's
/// `Repay` does not say which debt it repaid), so the pool's own events
/// only mark the account for re-evaluation, beside the index and
/// collateral-flag events V2 shares with V3.
fn apply_pool_v2(
    cfg: &Config,
    st: &mut dyn StateWriter,
    market: MarketId,
    topic0: alloy_primitives::B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let user = match topic0 {
        pool::ReserveDataUpdated::SIGNATURE_HASH
        | pool::ReserveUsedAsCollateralEnabled::SIGNATURE_HASH
        | pool::ReserveUsedAsCollateralDisabled::SIGNATURE_HASH => {
            return apply_pool(cfg, st, market, topic0, log);
        }
        v2::pool::Paused::SIGNATURE_HASH | v2::pool::Unpaused::SIGNATURE_HASH => {
            let at = MarketSlot { market, slot: 0 };
            let Ok(cur) = st.market(at) else {
                return Ok(DirtySet::None);
            };
            let mut row = *cur;
            row.body_mut::<PoolMeta>()?.pool_paused =
                u8::from(topic0 == v2::pool::Paused::SIGNATURE_HASH);
            st.set_market(at, row)?;
            return Ok(DirtySet::ProtocolWide);
        }
        pool::Withdraw::SIGNATURE_HASH => decode::<pool::Withdraw>(log)?.user,
        pool::LiquidationCall::SIGNATURE_HASH => decode::<pool::LiquidationCall>(log)?.user,
        v2::pool::Deposit::SIGNATURE_HASH => decode::<v2::pool::Deposit>(log)?.onBehalfOf,
        v2::pool::Borrow::SIGNATURE_HASH => decode::<v2::pool::Borrow>(log)?.onBehalfOf,
        v2::pool::Repay::SIGNATURE_HASH => decode::<v2::pool::Repay>(log)?.user,
        v2::pool::Swap::SIGNATURE_HASH => decode::<v2::pool::Swap>(log)?.user,
        v2::pool::RebalanceStableBorrowRate::SIGNATURE_HASH => {
            decode::<v2::pool::RebalanceStableBorrowRate>(log)?.user
        }
        _ => return Ok(DirtySet::None),
    };
    let id = intern(cfg, st, market, user)?;
    Ok(positions(&[id]))
}

/// Aave V2 configurator events: the two V3 shares, and V2's own on/off
/// events for activity, freezing and borrowing.
fn apply_cfg_v2(
    cfg: &Config,
    st: &mut dyn StateWriter,
    idx: usize,
    topic0: alloy_primitives::B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    if topic0 == ccfg::ReserveInitialized::SIGNATURE_HASH
        || topic0 == ccfg::CollateralConfigurationChanged::SIGNATURE_HASH
    {
        return apply_cfg(cfg, st, idx, topic0, log);
    }
    let p = cfg.pools.get(idx).ok_or(ProtocolError::UnexpectedLog)?;
    let (asset, bit, on) = match topic0 {
        v2::cfg::ReserveActivated::SIGNATURE_HASH => (
            decode::<v2::cfg::ReserveActivated>(log)?.asset,
            Reserve::ACTIVE,
            true,
        ),
        v2::cfg::ReserveDeactivated::SIGNATURE_HASH => (
            decode::<v2::cfg::ReserveDeactivated>(log)?.asset,
            Reserve::ACTIVE,
            false,
        ),
        v2::cfg::ReserveFrozen::SIGNATURE_HASH => (
            decode::<v2::cfg::ReserveFrozen>(log)?.asset,
            Reserve::FROZEN,
            true,
        ),
        v2::cfg::ReserveUnfrozen::SIGNATURE_HASH => (
            decode::<v2::cfg::ReserveUnfrozen>(log)?.asset,
            Reserve::FROZEN,
            false,
        ),
        v2::cfg::BorrowingEnabledOnReserve::SIGNATURE_HASH => (
            decode::<v2::cfg::BorrowingEnabledOnReserve>(log)?.asset,
            Reserve::BORROWING,
            true,
        ),
        v2::cfg::BorrowingDisabledOnReserve::SIGNATURE_HASH => (
            decode::<v2::cfg::BorrowingDisabledOnReserve>(log)?.asset,
            Reserve::BORROWING,
            false,
        ),
        _ => return Ok(DirtySet::None),
    };
    let slot = slot_by_underlying(cfg, st, p.market, asset)?;
    let rows = update_reserve(st, p.market, slot, None, |r| {
        if on {
            r.flags |= bit;
        } else {
            r.flags &= !bit;
        }
        Ok(())
    })?;
    Ok(DirtySet::MarketReprice(rows))
}

/// Aave V2's grace sentinel: `GracePeriodSet(asset, until)` is the
/// reserve's grace window, as V3's `LiquidationGracePeriodChanged` is.
fn apply_grace_v2(
    cfg: &Config,
    st: &mut dyn StateWriter,
    idx: usize,
    topic0: alloy_primitives::B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    if topic0 != v2::grace::GracePeriodSet::SIGNATURE_HASH {
        return Ok(DirtySet::None);
    }
    let p = cfg.pools.get(idx).ok_or(ProtocolError::UnexpectedLog)?;
    let ev: v2::grace::GracePeriodSet = decode(log)?;
    let until = u32::try_from(ev.until).map_err(|_| ProtocolError::MalformedLog)?;
    let slot = slot_by_underlying(cfg, st, p.market, ev.asset)?;
    let rows = update_reserve(st, p.market, slot, None, |r| {
        r.grace_until = until;
        Ok(())
    })?;
    Ok(DirtySet::MarketReprice(rows))
}

/// Aave V2 aToken: `Mint` and `Burn` carry the amount and the index the
/// token scaled it by (`amount.rayDiv(index)`, half-up), and V2's
/// `BalanceTransfer` the unscaled amount with its index (V3's carries the
/// scaled one) — `AToken` `0x1c050bca…` lines 114, 139, 332.
fn apply_atoken_v2(
    cfg: &Config,
    st: &mut dyn StateWriter,
    pool_i: usize,
    slot: u16,
    topic0: alloy_primitives::B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let p = cfg.pools.get(pool_i).ok_or(ProtocolError::UnexpectedLog)?;
    let moves: SmallVec<[(Address, U256, bool); 2]> = match topic0 {
        v2::atoken::Mint::SIGNATURE_HASH => {
            let ev: v2::atoken::Mint = decode(log)?;
            SmallVec::from_slice(&[(ev.from, crate::math::ray_div(ev.value, ev.index)?, true)])
        }
        v2::atoken::Burn::SIGNATURE_HASH => {
            let ev: v2::atoken::Burn = decode(log)?;
            SmallVec::from_slice(&[(ev.from, crate::math::ray_div(ev.value, ev.index)?, false)])
        }
        token::BalanceTransfer::SIGNATURE_HASH => {
            let ev: token::BalanceTransfer = decode(log)?;
            let scaled = crate::math::ray_div(ev.value, ev.index)?;
            SmallVec::from_slice(&[(ev.from, scaled, false), (ev.to, scaled, true)])
        }
        _ => return Ok(DirtySet::None),
    };
    let mut ids: SmallVec<[PositionId; 2]> = SmallVec::new();
    for (user, scaled, add) in moves {
        if user == Address::ZERO {
            continue;
        }
        let id = intern(cfg, st, p.market, user)?;
        add_supply(st, id, slot, scaled, add)?;
        ids.push(id);
    }
    Ok(positions(&ids))
}

/// Aave V2 variable debt token: `Mint(from, onBehalfOf, value, index)` and
/// `Burn(user, amount, index)`, scaled as the token did
/// (`VariableDebtToken` `0x1f57cc62…` lines 72, 95).
fn apply_vtoken_v2(
    cfg: &Config,
    st: &mut dyn StateWriter,
    pool_i: usize,
    slot: u16,
    topic0: alloy_primitives::B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let p = cfg.pools.get(pool_i).ok_or(ProtocolError::UnexpectedLog)?;
    let (user, scaled, add) = match topic0 {
        v2::vtoken::Mint::SIGNATURE_HASH => {
            let ev: v2::vtoken::Mint = decode(log)?;
            (
                ev.onBehalfOf,
                crate::math::ray_div(ev.value, ev.index)?,
                true,
            )
        }
        v2::vtoken::Burn::SIGNATURE_HASH => {
            let ev: v2::vtoken::Burn = decode(log)?;
            (ev.user, crate::math::ray_div(ev.amount, ev.index)?, false)
        }
        _ => return Ok(DirtySet::None),
    };
    let id = intern(cfg, st, p.market, user)?;
    add_debt(st, id, slot, scaled, add)?;
    Ok(positions(&[id]))
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
            // With `receiveAToken` the collateral moves by
            // `transferOnLiquidation`, whose `BalanceTransfer` carries the
            // exact scaled amount; debiting it here too would count it twice.
            // Without it the collateral is burned, and only this event says so.
            // The protocol fee always moves by `BalanceTransfer` to the
            // treasury and is not in `liquidatedCollateralAmount`.
            if !ev.receiveAToken {
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
            }
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
                for _ in 0..EMODE_ROWS {
                    st.push_market(market, MarketRow::blank(META_ASSET, 0))?;
                }
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
            body.s_token = ev.stableDebtToken.into();
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
            let at = st.push_market(market, row)?;
            Ok(DirtySet::MarketReprice(row_at(market, at.slot)))
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
        ccfg::ReserveStableRateBorrowing::SIGNATURE_HASH => {
            let ev: ccfg::ReserveStableRateBorrowing = decode(log)?;
            if ev.enabled {
                tracing::warn!(
                    target: "coverage",
                    pool = %p.address,
                    asset = %ev.asset,
                    "stable-rate borrowing enabled — accounts that take stable debt will not be quoted"
                );
            }
            Ok(DirtySet::None)
        }
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
            let id = ev.categoryId;
            let (ltv, liq_threshold, liq_bonus) = (
                narrow(ev.ltv)?,
                narrow(ev.liquidationThreshold)?,
                narrow(ev.liquidationBonus)?,
            );
            set_emode(st, market, id, |c| {
                *c = EModeCat {
                    id,
                    isolated: c.isolated,
                    ltv,
                    liq_threshold,
                    liq_bonus,
                };
            })?;
            reprice(collateral_in(st, market, id)?)
        }
        ccfg::EModeCategoryIsolationChanged::SIGNATURE_HASH => {
            let ev: ccfg::EModeCategoryIsolationChanged = decode(log)?;
            let id = ev.categoryId;
            set_emode(st, market, id, |c| {
                if c.id == id {
                    c.isolated = u8::from(ev.isolated);
                }
            })?;
            reprice(collateral_in(st, market, id)?)
        }
        ccfg::AssetCollateralInEModeChanged::SIGNATURE_HASH => {
            emode_bit(cfg, st, market, log, topic0, |r, id, on| {
                r.emode_coll.set(id, on)
            })
        }
        // Which assets an e-mode user may borrow does not change a
        // liquidation, so the adapter does not keep it.
        ccfg::AssetBorrowableInEModeChanged::SIGNATURE_HASH => Ok(DirtySet::None),
        ccfg::AssetLtvzeroInEModeChanged::SIGNATURE_HASH => {
            emode_bit(cfg, st, market, log, topic0, |r, id, on| {
                r.emode_ltv0.set(id, on)
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
    set: impl FnOnce(&mut Reserve, u8, bool),
) -> Result<DirtySet> {
    let (asset, cat, on) = if topic0 == ccfg::AssetCollateralInEModeChanged::SIGNATURE_HASH {
        let ev: ccfg::AssetCollateralInEModeChanged = decode(log)?;
        (ev.asset, ev.categoryId, ev.collateral)
    } else {
        let ev: ccfg::AssetLtvzeroInEModeChanged = decode(log)?;
        (ev.asset, ev.categoryId, ev.ltvzero)
    };
    // `Pool.configureEModeCategory*Bitmap` refuses category 0.
    if cat == 0 {
        return Err(ProtocolError::MalformedLog);
    }
    let slot = slot_by_underlying(cfg, st, market, asset)?;
    let rows = update_reserve(st, market, slot, None, |r| {
        set(r, cat, on);
        Ok(())
    })?;
    Ok(DirtySet::MarketReprice(rows))
}

/// Rewrite category `id`'s entry in the e-mode rows.
fn set_emode(
    st: &mut dyn StateWriter,
    market: MarketId,
    id: u8,
    f: impl FnOnce(&mut EModeCat),
) -> Result<()> {
    // Aave's category 0 is e-mode off; no event configures it.
    let (slot, i) = emode_place(id).ok_or(ProtocolError::MalformedLog)?;
    let at = MarketSlot { market, slot };
    let mut row = *st.market(at)?;
    f(row
        .body_mut::<EModeRow>()?
        .cats
        .get_mut(i)
        .ok_or(ProtocolError::Internal)?);
    st.set_market(at, row)
}

/// A reprice of `rows`; nothing when no row is named.
fn reprice(rows: DirtyRows) -> Result<DirtySet> {
    Ok(if rows.is_empty() {
        DirtySet::None
    } else {
        DirtySet::MarketReprice(rows)
    })
}

/// The rows of the reserves that are collateral in category `id`: a change
/// to the category moves the health of every account holding one of them.
fn collateral_in(st: &dyn StateWriter, market: MarketId, id: u8) -> Result<DirtyRows> {
    let mut out = DirtyRows::new();
    for (i, row) in st
        .markets(market)?
        .iter()
        .enumerate()
        .skip(usize::from(FIRST_RESERVE))
    {
        if row.body::<Reserve>()?.emode_coll.contains(id) {
            let slot = u16::try_from(i).map_err(|_| ProtocolError::Internal)?;
            out.push(MarketSlot { market, slot });
        }
    }
    Ok(out)
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

/// Stable debt is not modelled (disabled on every Spark reserve). A mint
/// marks the account's slot, which makes the account unquotable; a burn that
/// leaves no stable debt on that reserve clears it. Every mark is an alarm.
fn apply_stoken(
    cfg: &Config,
    st: &mut dyn StateWriter,
    pool_i: usize,
    slot: u16,
    topic0: alloy_primitives::B256,
    log: &DecodedLog<'_>,
) -> Result<DirtySet> {
    let p = cfg.pools.get(pool_i).ok_or(ProtocolError::UnexpectedLog)?;
    let (user, holds) = if topic0 == stable::Mint::SIGNATURE_HASH {
        let ev: stable::Mint = decode(log)?;
        (ev.onBehalfOf, true)
    } else if topic0 == stable::Burn::SIGNATURE_HASH {
        let ev: stable::Burn = decode(log)?;
        // `StableDebtToken.burn`: the event's amount is `amount -
        // balanceIncrease`; what is left is `currentBalance - amount`.
        let left = ev
            .currentBalance
            .checked_sub(ev.amount)
            .and_then(|x| x.checked_sub(ev.balanceIncrease));
        (ev.from, left != Some(U256::ZERO))
    } else {
        return Ok(DirtySet::None);
    };
    let id = intern(cfg, st, p.market, user)?;
    let bit = 1u64.checked_shl(u32::from(slot.min(63))).unwrap_or(1 << 63);
    let mut extra = *st.extra(id)?;
    let e = extra.view_mut::<UserExtra>()?;
    if holds {
        e.stable_slots |= bit;
        tracing::warn!(
            target: "coverage",
            pool = %p.address,
            account = %user,
            slot,
            "account holds stable debt — not quoted (stable debt is not modelled)"
        );
    } else if slot < 63 {
        e.stable_slots &= !bit;
    }
    st.set_extra(id, extra)?;
    Ok(positions(&[id]))
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
