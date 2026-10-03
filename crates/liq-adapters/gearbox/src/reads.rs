//! Chain reads Gearbox's logs cannot replace.
//!
//! **Accounts after a multicall.** Inside `CreditFacadeV3._multicall`
//! adapter calls swap the account's tokens and `decreaseDebt` repays through
//! pool `Repay`, which names no account; the facade emits only
//! `StartMultiCall` / `Execute` / `FinishMultiCall`, with no amounts. So
//! `apply_log` marks the account [`AccountExtra::STALE`] and these reads
//! replace its state with the chain's:
//!
//! 1. `ICreditManagerV3.creditAccountInfo(account)` — debt, interest and
//!    quota-interest checkpoints, the enabled-token mask, the borrower.
//! 2. As follow-ups at the same block: `IERC20.balanceOf(account)` per token
//!    the mask enables (the underlying always), and
//!    `IPoolQuotaKeeperV3.getQuota(account, token)` per enabled
//!    non-underlying token — in v3.1 every one is quoted.
//!
//! The account leaves `STALE` only when every one of those answered. A
//! closed account (`borrower == 0`) is zeroed.
//!
//! **Interest, every block.** Debt grows with the pool's base index and each
//! quoted token's quota index; no log carries either. For every pool:
//! `baseInterestIndexLU`, `baseInterestRate`, `lastBaseInterestUpdate`; for
//! every quota keeper: `lastQuotaRateUpdate`; for every quoted token:
//! `getTokenQuotaParams`. Health projects both indexes to its evaluation
//! time with the pin's own formulas (`crate::math`).
//!
//! Tags: account reads `position id << 16 | kind` (kind `0` = account info,
//! `1 + slot` = balance, `0x100 + slot` = quota); interest reads
//! [`INTEREST`]` | index` into [`interest_plan`].

use std::collections::HashMap;

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;
use bytemuck::Zeroable;
use liq_protocol::{
    DirtyPositions, DirtyRows, DirtySet, MarketSlot, PositionRef, ProtocolError, Result,
    StateAnswer, StateRead, StateWriter,
};
use liq_types::{MarketId, PositionId};

use crate::apply::zero_account;
use crate::config::{Config, ManagerConfig};
use crate::events::views::{ICreditManagerV3, IPoolQuotaKeeperV3, IPoolV3, IERC20};
use crate::layout::{AccountExtra, ManagerRow, QuotaExtra, TokenRow, UNDERLYING_SLOT};

const INFO: u64 = 0;
const QUOTA_KIND: u64 = 0x100;
/// High bit of an interest read's tag; account tags stay below bit 48.
const INTEREST: u64 = 1 << 63;

#[inline]
fn tag(pos: PositionId, kind: u64) -> u64 {
    u64::from(pos.0).wrapping_shl(16) | kind
}

#[inline]
fn untag(tag: u64) -> (PositionId, u64) {
    // Built from a `u32` id; a foreign tag maps to an id no store holds.
    (
        PositionId(u32::try_from(tag.wrapping_shr(16)).unwrap_or(u32::MAX)),
        tag & 0xffff,
    )
}

#[inline]
fn balance_kind(slot: u16) -> u64 {
    u64::from(slot).saturating_add(1)
}

#[inline]
fn quota_kind(slot: u16) -> u64 {
    QUOTA_KIND | u64::from(slot)
}

/// Slots whose balance counts for `mask`: the underlying and every enabled
/// token. `None` when the mask names a token past the stored 64-bit mask.
fn counted_slots(m: &ManagerConfig, mask: U256) -> Option<Vec<u16>> {
    let mask = u64::try_from(mask).ok()?;
    let mut out: Vec<u16> = m
        .tokens
        .iter()
        .filter(|t| t.slot == UNDERLYING_SLOT || mask & t.mask != 0)
        .map(|t| t.slot)
        .collect();
    out.sort_unstable();
    out.dedup();
    Some(out)
}

// ---- accounts ----------------------------------------------------------

/// The account-info read for a stale account of this adapter, or nothing.
pub(crate) fn position_reads(cfg: &Config, pos: PositionRef<'_>) -> Vec<StateRead> {
    let Ok(extra) = pos.extra.view::<AccountExtra>() else {
        return Vec::new();
    };
    if extra.flags & AccountExtra::STALE == 0 {
        return Vec::new();
    }
    resync_reads(cfg, pos)
}

/// The account-info read for any account of this adapter: its follow-ups
/// and fold replace the account's whole state, stale or not.
pub(crate) fn resync_reads(cfg: &Config, pos: PositionRef<'_>) -> Vec<StateRead> {
    if pos.key.protocol != cfg.protocol {
        return Vec::new();
    }
    let Some((_, m)) = cfg.manager_by_market(pos.key.market) else {
        return Vec::new();
    };
    vec![StateRead {
        market: m.market,
        target: m.manager,
        calldata: Bytes::from(
            ICreditManagerV3::creditAccountInfoCall {
                creditAccount: pos.key.user,
            }
            .abi_encode(),
        ),
        tag: tag(pos.id, INFO),
    }]
}

/// The balance and quota reads one account-info answer calls for.
pub(crate) fn follow_ups(cfg: &Config, answer: StateAnswer<'_>) -> Vec<StateRead> {
    if answer.read.tag & INTEREST != 0 {
        return Vec::new();
    }
    let (pos, kind) = untag(answer.read.tag);
    if kind != INFO || !answer.success {
        return Vec::new();
    }
    let Some((_, m)) = cfg.manager_by_market(answer.read.market) else {
        return Vec::new();
    };
    let (Ok(call), Ok(info)) = (
        ICreditManagerV3::creditAccountInfoCall::abi_decode(&answer.read.calldata),
        ICreditManagerV3::creditAccountInfoCall::abi_decode_returns(answer.data),
    ) else {
        return Vec::new();
    };
    if info.borrower == Address::ZERO {
        return Vec::new();
    }
    let Some(slots) = counted_slots(m, info.enabledTokensMask) else {
        return Vec::new();
    };
    let account = call.creditAccount;
    let mut out = Vec::with_capacity(slots.len().saturating_mul(2));
    for t in slots
        .into_iter()
        .filter_map(|slot| m.tokens.iter().find(|t| t.slot == slot))
    {
        out.push(StateRead {
            market: m.market,
            target: t.token,
            calldata: Bytes::from(IERC20::balanceOfCall { account }.abi_encode()),
            tag: tag(pos, balance_kind(t.slot)),
        });
        if t.slot != UNDERLYING_SLOT {
            out.push(StateRead {
                market: m.market,
                target: m.quota_keeper,
                calldata: Bytes::from(
                    IPoolQuotaKeeperV3::getQuotaCall {
                        creditAccount: account,
                        token: t.token,
                    }
                    .abi_encode(),
                ),
                tag: tag(pos, quota_kind(t.slot)),
            });
        }
    }
    out
}

/// One account's settled state, decoded and narrowed before any write.
struct Settled {
    debt: u128,
    balances: Vec<(u16, u128)>,
    /// `(slot, quota, cumulativeIndexLU)` per enabled non-underlying token.
    quotas: Vec<(u16, u128, u128)>,
    index: u128,
    quota_interest: u128,
    quota_fees: u128,
    mask: u64,
    last_debt_update: u32,
}

fn answer<'a>(
    answers: &'a [StateAnswer<'a>],
    market: MarketId,
    want: u64,
) -> Option<&'a StateAnswer<'a>> {
    answers
        .iter()
        .find(|b| b.read.tag == want && b.read.market == market && b.success)
}

/// `None` when an answer is missing, failed, or does not fit the store: the
/// account stays stale.
fn settle(
    m: &ManagerConfig,
    pos: PositionId,
    info: &ICreditManagerV3::creditAccountInfoReturn,
    answers: &[StateAnswer<'_>],
) -> Option<Settled> {
    let slots = counted_slots(m, info.enabledTokensMask)?;
    let mut balances = Vec::with_capacity(slots.len());
    let mut quotas = Vec::with_capacity(slots.len());
    for slot in slots {
        let b = answer(answers, m.market, tag(pos, balance_kind(slot)))?;
        let v = IERC20::balanceOfCall::abi_decode_returns(b.data).ok()?;
        balances.push((slot, u128::try_from(v).ok()?));
        if slot != UNDERLYING_SLOT {
            let q = answer(answers, m.market, tag(pos, quota_kind(slot)))?;
            let q = IPoolQuotaKeeperV3::getQuotaCall::abi_decode_returns(q.data).ok()?;
            quotas.push((
                slot,
                u128::try_from(q.quota).ok()?,
                u128::try_from(q.cumulativeIndexLU).ok()?,
            ));
        }
    }
    Some(Settled {
        debt: u128::try_from(info.debt).ok()?,
        balances,
        quotas,
        index: u128::try_from(info.cumulativeIndexLastUpdate).ok()?,
        // `CreditManagerV3` keeps a 1-wei sentinel in the stored value
        // (`_calcDebtAndCollateral`: `cumulativeQuotaInterest - 1`).
        quota_interest: info.cumulativeQuotaInterest.saturating_sub(1),
        quota_fees: info.quotaFees,
        mask: u64::try_from(info.enabledTokensMask).ok()?,
        last_debt_update: u32::try_from(info.lastDebtUpdate).ok()?,
    })
}

fn apply_accounts(
    cfg: &Config,
    st: &mut dyn StateWriter,
    answers: &[StateAnswer<'_>],
) -> Result<DirtyPositions> {
    let mut dirty = DirtyPositions::new();
    for a in answers {
        if a.read.tag & INTEREST != 0 {
            continue;
        }
        let (pos, kind) = untag(a.read.tag);
        if kind != INFO || !a.success {
            continue;
        }
        let Some((_, m)) = cfg.manager_by_market(a.read.market) else {
            continue;
        };
        // The tag's id must still name the account the read asked about: a
        // reorg can unwind the id's creation and hand it to another account.
        let Ok(call) = ICreditManagerV3::creditAccountInfoCall::abi_decode(&a.read.calldata) else {
            continue;
        };
        let Ok(key) = st.position_key(pos).copied() else {
            continue;
        };
        if key.protocol != cfg.protocol || key.market != m.market || key.user != call.creditAccount
        {
            continue;
        }
        let Ok(info) = ICreditManagerV3::creditAccountInfoCall::abi_decode_returns(a.data) else {
            tracing::error!(target: "coverage", account = %key.user, "gearbox creditAccountInfo undecodable — account stays unread");
            continue;
        };
        if info.borrower == Address::ZERO {
            let count = u8::try_from(m.tokens.len()).map_err(|_| ProtocolError::Internal)?;
            zero_account(st, pos, count)?;
            dirty.push(pos);
            continue;
        }
        let Some(s) = settle(m, pos, &info, answers) else {
            tracing::warn!(target: "coverage", account = %key.user, "gearbox account reads incomplete — read again next block");
            continue;
        };
        st.set_debt(pos, UNDERLYING_SLOT, s.debt)?;
        for t in &m.tokens {
            let bal = s
                .balances
                .iter()
                .find(|(slot, _)| *slot == t.slot)
                .map_or(0, |(_, b)| *b);
            st.set_supply(pos, t.slot, bal)?;
            if t.slot == UNDERLYING_SLOT {
                continue;
            }
            let mut q = QuotaExtra::zeroed();
            if let Some((_, quota, index_lu)) = s.quotas.iter().find(|(slot, ..)| *slot == t.slot) {
                q.quota = *quota;
                q.index_lu = *index_lu;
                q.flags = QuotaExtra::QUOTED;
            }
            let mut repr = *st.slot_extra(pos, t.slot)?;
            *repr.view_mut::<QuotaExtra>()? = q;
            st.set_slot_extra(pos, t.slot, repr)?;
        }
        let mut repr = *st.extra(pos)?;
        let ex = repr.view_mut::<AccountExtra>()?;
        ex.cumulative_index_last_update = s.index;
        ex.cumulative_quota_interest = s.quota_interest;
        ex.quota_fees = s.quota_fees;
        ex.enabled_tokens_mask = s.mask;
        ex.last_debt_update = s.last_debt_update;
        ex.flags = (ex.flags | AccountExtra::OPEN) & !AccountExtra::STALE;
        st.set_extra(pos, repr)?;
        dirty.push(pos);
    }
    Ok(dirty)
}

// ---- interest ------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    PoolIndex(Address),
    PoolRate(Address),
    PoolTime(Address),
    KeeperTime(Address),
    TokenQuota { keeper: Address, token: Address },
}

/// Every interest read, deduplicated across managers that share a pool or
/// keeper, in a fixed order derived from `cfg` alone (the tag is the index).
fn interest_plan(cfg: &Config) -> Vec<(MarketId, Want)> {
    let mut out: Vec<(MarketId, Want)> = Vec::new();
    for m in &cfg.managers {
        for w in [
            Want::PoolIndex(m.pool),
            Want::PoolRate(m.pool),
            Want::PoolTime(m.pool),
            Want::KeeperTime(m.quota_keeper),
        ] {
            if !out.iter().any(|(_, x)| *x == w) {
                out.push((m.market, w));
            }
        }
        for t in m.tokens.iter().filter(|t| t.slot != UNDERLYING_SLOT) {
            let w = Want::TokenQuota {
                keeper: m.quota_keeper,
                token: t.token,
            };
            if !out.iter().any(|(_, x)| *x == w) {
                out.push((m.market, w));
            }
        }
    }
    out
}

/// The per-block interest reads.
pub(crate) fn interest_reads(cfg: &Config) -> Vec<StateRead> {
    interest_plan(cfg)
        .into_iter()
        .enumerate()
        .map(|(i, (market, w))| {
            let (target, calldata) = match w {
                Want::PoolIndex(p) => (p, IPoolV3::baseInterestIndexLUCall {}.abi_encode()),
                Want::PoolRate(p) => (p, IPoolV3::baseInterestRateCall {}.abi_encode()),
                Want::PoolTime(p) => (p, IPoolV3::lastBaseInterestUpdateCall {}.abi_encode()),
                Want::KeeperTime(k) => (
                    k,
                    IPoolQuotaKeeperV3::lastQuotaRateUpdateCall {}.abi_encode(),
                ),
                Want::TokenQuota { keeper, token } => (
                    keeper,
                    IPoolQuotaKeeperV3::getTokenQuotaParamsCall { token }.abi_encode(),
                ),
            };
            StateRead {
                market,
                target,
                calldata: Bytes::from(calldata),
                tag: INTEREST | u64::try_from(i).unwrap_or(u64::MAX >> 1),
            }
        })
        .collect()
}

#[derive(Default)]
struct PoolRead {
    index: Option<u128>,
    rate: Option<u128>,
    time: Option<u64>,
}

fn apply_interest(
    cfg: &Config,
    st: &mut dyn StateWriter,
    answers: &[StateAnswer<'_>],
) -> Result<DirtyRows> {
    let plan = interest_plan(cfg);
    let mut pools: HashMap<Address, PoolRead> = HashMap::new();
    let mut keepers: HashMap<Address, u64> = HashMap::new();
    let mut quotas: HashMap<(Address, Address), (u16, u128)> = HashMap::new();
    for a in answers
        .iter()
        .filter(|a| a.success && a.read.tag & INTEREST != 0)
    {
        let Some((_, w)) = usize::try_from(a.read.tag & !INTEREST)
            .ok()
            .and_then(|i| plan.get(i))
        else {
            continue;
        };
        match *w {
            Want::PoolIndex(p) => {
                pools.entry(p).or_default().index =
                    IPoolV3::baseInterestIndexLUCall::abi_decode_returns(a.data)
                        .ok()
                        .and_then(|v| u128::try_from(v).ok());
            }
            Want::PoolRate(p) => {
                pools.entry(p).or_default().rate =
                    IPoolV3::baseInterestRateCall::abi_decode_returns(a.data)
                        .ok()
                        .and_then(|v| u128::try_from(v).ok());
            }
            Want::PoolTime(p) => {
                pools.entry(p).or_default().time =
                    IPoolV3::lastBaseInterestUpdateCall::abi_decode_returns(a.data)
                        .ok()
                        .map(|v| v.to::<u64>());
            }
            Want::KeeperTime(k) => {
                if let Ok(v) =
                    IPoolQuotaKeeperV3::lastQuotaRateUpdateCall::abi_decode_returns(a.data)
                {
                    keepers.insert(k, v.to::<u64>());
                }
            }
            Want::TokenQuota { keeper, token } => {
                if let Some(q) =
                    IPoolQuotaKeeperV3::getTokenQuotaParamsCall::abi_decode_returns(a.data)
                        .ok()
                        .and_then(|q| Some((q.rate, u128::try_from(q.cumulativeIndexLU).ok()?)))
                {
                    quotas.insert((keeper, token), q);
                }
            }
        }
    }
    let mut dirty = DirtyRows::new();
    for m in &cfg.managers {
        let at = MarketSlot {
            market: m.market,
            slot: UNDERLYING_SLOT,
        };
        let Ok(row) = st.market(at) else {
            continue; // not listed yet
        };
        let mut row = *row;
        let before = *row.body::<ManagerRow>()?;
        let b = row.body_mut::<ManagerRow>()?;
        if let Some(PoolRead {
            index: Some(index),
            rate: Some(rate),
            time: Some(time),
        }) = pools.get(&m.pool)
        {
            b.base_index_lu = *index;
            b.base_rate = *rate;
            b.base_last_update = *time;
            b.flags |= ManagerRow::BASE_KNOWN;
        }
        if let Some(t) = keepers.get(&m.quota_keeper) {
            b.quota_last_update = *t;
            b.flags |= ManagerRow::QUOTA_TIME_KNOWN;
        }
        if *b != before {
            st.set_market(at, row)?;
            dirty.push(at);
        }
        for t in m.tokens.iter().filter(|t| t.slot != UNDERLYING_SLOT) {
            let Some((rate, index)) = quotas.get(&(m.quota_keeper, t.token)) else {
                continue;
            };
            let at = MarketSlot {
                market: m.market,
                slot: t.slot,
            };
            let Ok(row) = st.market(at) else {
                continue;
            };
            let mut row = *row;
            let tb = row.body_mut::<TokenRow>()?;
            let before = *tb;
            tb.quota_rate = *rate;
            tb.quota_index_lu = *index;
            tb.flags |= TokenRow::QUOTA_KNOWN;
            if *tb != before {
                st.set_market(at, row)?;
                dirty.push(at);
            }
        }
    }
    Ok(dirty)
}

/// Fold one block's answers: interest rows, then settled accounts.
/// Accounts whose reads are incomplete stay stale and are read again at the
/// next block.
pub(crate) fn apply(
    cfg: &Config,
    st: &mut dyn StateWriter,
    answers: &[StateAnswer<'_>],
) -> Result<Vec<DirtySet>> {
    let rows = apply_interest(cfg, st, answers)?;
    let accounts = apply_accounts(cfg, st, answers)?;
    let mut out = Vec::with_capacity(2);
    if !rows.is_empty() {
        out.push(DirtySet::MarketAccrual(rows));
    }
    if !accounts.is_empty() {
        out.push(DirtySet::Positions(accounts));
    }
    Ok(out)
}
