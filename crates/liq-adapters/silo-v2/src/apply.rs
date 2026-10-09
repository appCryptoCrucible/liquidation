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
use crate::math::rate_ray;

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

/// Patch one silo's row. With a timestamp, the totals first grow to it at
/// the read rates (Silo accrues interest before every state change), then
/// `f` applies the change and `last_update` moves to `ts`.
fn patch_row(
    st: &mut dyn StateWriter,
    market: MarketId,
    slot: u16,
    ts: Option<u32>,
    f: impl FnOnce(&mut SiloRow) -> Result<()>,
) -> Result<()> {
    let at = MarketSlot { market, slot };
    let mut row = *st.market(at)?;
    if let Some(t) = ts {
        let dt = u64::from(t.saturating_sub(row.last_update));
        accrue(row.body_mut::<SiloRow>()?, dt)?;
    }
    f(row.body_mut::<SiloRow>()?)?;
    if let Some(t) = ts {
        row.last_update = t;
    }
    row.flags = row_flags(row.body::<SiloRow>()?);
    st.set_market(at, row)
}

/// `b`'s totals grown by `dt` seconds of the read interest rate: the debt
/// by the interest, the collateral by the interest less its fees
/// (`SiloMathLib.getCollateralAmountsWithInterest`: `accrued − accrued ×
/// (daoFee + deployerFee) / 1e18`).
pub(crate) fn accrue(b: &mut SiloRow, dt: u64) -> Result<()> {
    if b.flags & SiloRow::GROWTH_KNOWN == 0 {
        return Ok(());
    }
    let debt = crate::math::grown(b.total_debt_assets, b.debt_rate_ray, dt)?;
    let accrued = U256::from(debt.saturating_sub(b.total_debt_assets));
    let fees =
        crate::math::mul_div_down(accrued, U256::from(b.interest_fee), crate::math::PRECISION)?;
    b.total_debt_assets = debt;
    b.total_collateral_assets = add_u128(
        b.total_collateral_assets,
        accrued.saturating_sub(fees),
        true,
    )?;
    Ok(())
}

/// The row flags a body implies: unpriced without its pinned oracle,
/// paused once halted.
pub(crate) fn row_flags(b: &SiloRow) -> MarketFlags {
    let mut f = MarketFlags::NONE;
    if b.flags & SiloRow::PRICED == 0 {
        f = MarketFlags(f.0 | MarketFlags::UNPRICED.0);
    }
    if b.flags & SiloRow::HALTED != 0 {
        f = MarketFlags(f.0 | MarketFlags::PAUSED.0);
    }
    f
}

/// Mark pair `pair_i` halted ([`SiloRow::HALTED`] on both silos): it refuses
/// liquidation until the config is re-pinned; other pairs continue. State,
/// not a flag in memory: a reorg that drops the log unwinds it.
fn halt_pair(
    cfg: &Config,
    st: &mut dyn StateWriter,
    pair_i: usize,
    emitter: Address,
    topic0: B256,
) -> Result<DirtySet> {
    let pair = cfg.pair(pair_i).ok_or(ProtocolError::Internal)?;
    tracing::error!(
        market = pair.market.0,
        %emitter,
        %topic0,
        "silo pair emitted a halt-class log after the pin: this pair is halted (no liquidations) until the config is re-pinned; other pairs continue"
    );
    match st.markets(pair.market) {
        Ok(rows) if rows.len() == usize::from(crate::layout::PAIR_SLOTS) => {}
        // Not listed yet: nothing to liquidate, and `NewSilo` precedes the
        // pin for every configured pair.
        Ok(_) | Err(ProtocolError::UnknownMarket(_)) => return Ok(DirtySet::None),
        Err(e) => return Err(e),
    }
    for slot in [SLOT0, SLOT1] {
        patch_row(st, pair.market, slot, None, |b| {
            b.flags |= SiloRow::HALTED;
            Ok(())
        })?;
    }
    Ok(DirtySet::MarketReprice(rows_of(pair.market)))
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
        if log.block <= cfg.pinned_through {
            return Ok(DirtySet::None);
        }
        // One pair's silo, hook, share token or config: halt that pair, not
        // the bot. The factories are shared by every pair, so theirs stop
        // the protocol's ingest as before.
        return match em {
            Emitter::Factory(_) => Err(ProtocolError::HaltSignal),
            Emitter::Hook(i)
            | Emitter::SiloConfig(i)
            | Emitter::Silo { pair: i, .. }
            | Emitter::Share(ShareLoc { pair: i, .. }) => {
                halt_pair(cfg, st, i, log.address, topic0)
            }
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
            if let Some((i, p)) = cfg
                .pairs
                .iter()
                .enumerate()
                .find(|(_, p)| p.silo0.silo == ev.silo || p.silo1.silo == ev.silo)
            {
                if ev.hook != p.hook_receiver && log.block > cfg.pinned_through {
                    return halt_pair(cfg, st, i, log.address, topic0);
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
        // FeedId(0) is the first interned Aave oracle, not unset. Silo solvency
        // oracles are not in registry.oracles — do not invent FeedId::NONE.
        // Do not join `row.price_feed` to ticks; health prices via AssetId /
        // PriceVector. Documented collision (coverage silo-v2.md).
        row.price_feed = tok.feed;
        row.last_update = ts;
        {
            let b: &mut SiloRow = row.body_mut()?;
            let wad = |v: u128| u64::try_from(v).map_err(|_| ProtocolError::AmountTooLarge);
            b.lt = wad(side.lt)?;
            b.liquidation_fee = wad(side.liquidation_fee)?;
            b.liquidation_target_ltv = wad(side.liquidation_target_ltv)?;
            b.interest_fee = wad(side.interest_fee)?;
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
        // `Silo.sol:825` emits `Δ totalAssets[Debt]` for this accrual (see
        // the rename in `events.rs`).
        //
        // Once a state read has measured the silo's growth, `patch_row`
        // already grew both totals to `ts` (debt by the interest, collateral
        // by the interest net of fees), and adding the event's amount again
        // would count it twice; each block's read then sets both totals to
        // the chain's own `getDebtAssets()` / `getCollateralAssets()`.
        // Before the first read (backfill), the event is all there is: debt
        // takes it, and collateral stays put (LTV biased upward, the
        // fail-safe direction) until the read.
        let ev = decode::<silo::AccruedInterest>(log)?;
        patch_row(st, market, slot, Some(ts), |b| {
            if b.flags & SiloRow::GROWTH_KNOWN == 0 {
                b.total_debt_assets = add_u128(b.total_debt_assets, ev.accruedInterest, true)?;
            }
            Ok(())
        })?;
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

/// Debt units of growth that measure a rate to a percent.
const RESOLVED_GROWTH: u128 = 100;
/// The oldest accrual whose storage totals start a measurement (seconds).
const AVERAGE_WINDOW: u64 = 3_600;

/// Fold one block's totals reads ([`crate::SiloV2`]'s `state_reads`): per
/// silo, both totals become the chain's with-interest values at
/// `timestamp`, and the debt's growth rate is re-measured.
///
/// The rate is the debt's growth from a base point (`base_debt` at
/// `base_at`) to now, taken once it has grown by [`RESOLVED_GROWTH`] units
/// (the integer growth then resolves it to a percent), after which now is
/// the next base. A busy silo is measured block to block, the rate now; a
/// quiet one over as many blocks as its growth needs. Between accruals only
/// interest moves the totals, so the growth is the rate. Silo's rate model
/// moves the rate while utilization stays away from its target, so a long
/// average is not the rate now: a silo unaccrued for 71 days at 100 %
/// utilization grew at twice its average, another at an eighteenth of it.
///
/// An accrual (a new `interestRateTimestamp`) restarts the base: at the
/// accrual itself (its storage totals) when it is under
/// [`AVERAGE_WINDOW`] old, else now. Until a rate is measured the totals
/// are the chain's at each read and do not grow between reads (short by
/// under [`RESOLVED_GROWTH`] units). An accrual changes utilization, but
/// the rate model is continuous through it, so a measured rate stays until
/// the next one.
///
/// A silo missing any of its four answers, or whose read target is not the
/// row's silo, is left as it was.
pub(crate) fn apply_totals(
    st: &mut dyn StateWriter,
    timestamp: u64,
    answers: &[liq_protocol::StateAnswer<'_>],
) -> Result<Vec<DirtySet>> {
    #[derive(Default)]
    struct Words {
        target: Address,
        debt: Option<U256>,
        coll: Option<U256>,
        stored: Option<(U256, U256)>,
        accrued_at: Option<u64>,
    }
    let mut by_slot: Vec<(MarketSlot, Words)> = Vec::new();
    for a in answers {
        if !a.success {
            continue;
        }
        let Ok(slot) = u16::try_from(a.read.tag & 0xffff) else {
            continue;
        };
        let at = MarketSlot {
            market: a.read.market,
            slot,
        };
        let i = match by_slot.iter().position(|(s, _)| *s == at) {
            Some(i) => i,
            None => {
                by_slot.push((
                    at,
                    Words {
                        target: a.read.target,
                        ..Words::default()
                    },
                ));
                by_slot.len().saturating_sub(1)
            }
        };
        let Some((_, w)) = by_slot.get_mut(i) else {
            continue;
        };
        let word = |k: usize| {
            a.data
                .get(k.saturating_mul(32)..k.saturating_add(1).saturating_mul(32))
                .map(U256::from_be_slice)
        };
        match a.read.tag >> 16 {
            0 => w.debt = word(0),
            1 => w.coll = word(0),
            2 => w.stored = word(0).zip(word(1)),
            3 => w.accrued_at = word(2).and_then(|t| u64::try_from(t).ok()),
            _ => {}
        }
    }
    let ts = last_update(timestamp)?;
    let mut rows = DirtyRows::new();
    for (at, w) in by_slot {
        let (Some(debt), Some(coll), Some((_, stored_debt)), Some(accrued_at)) =
            (w.debt, w.coll, w.stored, w.accrued_at)
        else {
            continue;
        };
        let Ok(cur) = st.market(at) else { continue };
        let mut row = *cur;
        let before = row;
        let b: &mut SiloRow = row.body_mut()?;
        if Address::from(b.silo) != w.target {
            continue;
        }
        if b.flags & SiloRow::READ_BASE == 0 || b.accrued_at != accrued_at {
            let since_accrual = timestamp.saturating_sub(accrued_at);
            if since_accrual > 0 && since_accrual <= AVERAGE_WINDOW {
                b.base_debt = narrow(stored_debt)?;
                b.base_at = last_update(accrued_at)?;
            } else {
                b.base_debt = narrow(debt)?;
                b.base_at = ts;
            }
        }
        let window = timestamp.saturating_sub(u64::from(b.base_at));
        let resolved = U256::from(b.base_debt)
            .checked_add(U256::from(RESOLVED_GROWTH))
            .is_some_and(|t| debt >= t);
        if window > 0 && resolved {
            b.debt_rate_ray = rate_ray(U256::from(b.base_debt), debt, window)?;
            b.flags |= SiloRow::GROWTH_KNOWN;
            b.base_debt = narrow(debt)?;
            b.base_at = ts;
        }
        b.total_debt_assets = narrow(debt)?;
        b.total_collateral_assets = narrow(coll)?;
        b.accrued_at = accrued_at;
        b.flags |= SiloRow::READ_BASE;
        row.last_update = ts;
        row.flags = row_flags(row.body::<SiloRow>()?);
        if row != before {
            st.set_market(at, row)?;
            rows.push(at);
        }
    }
    Ok(if rows.is_empty() {
        Vec::new()
    } else {
        vec![DirtySet::MarketAccrual(rows)]
    })
}
