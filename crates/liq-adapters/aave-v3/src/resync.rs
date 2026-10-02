//! Resync: one account's state read back from the pool
//! (`aave-dao/aave-v3-origin` @ `8305565ae`), for the drift check.
//!
//! What this adapter stores per account, and the view that holds the chain's
//! copy of each:
//!
//! | store | view |
//! |---|---|
//! | `supply[slot]` (scaled) | `AToken.scaledBalanceOf(user)` |
//! | `debt[slot]` (scaled variable) | `VariableDebtToken.scaledBalanceOf(user)` |
//! | collateral flag per slot | `Pool.getUserConfiguration(user)`, bit `2·id + 1` |
//! | e-mode | `Pool.getUserEMode(user)` |
//! | stable-debt marks | `StableDebtToken.balanceOf(user)` (pre-3.2 pools) |
//!
//! Every reserve's balances are read in one stage (follow-ups see only the
//! previous answer, not the store, so they cannot map the configuration
//! bitmap to slots). The bitmap is indexed by reserve **id**, which is not
//! `slot - 1` once a reserve has been dropped and its id reused
//! (`PoolLogic.executeInitReserve` takes the first free id): each reserve's
//! id is read once — `UNDERLYING_ASSET_ADDRESS()` on its aToken, then
//! `getReserveData(underlying)`, whose `aTokenAddress` must be the row's —
//! and kept on the row ([`Reserve::id_known`]). An account whose collateral
//! needs an id not read yet stays unsettled and is read again.
//!
//! Tags: account reads `position id << 16 | kind` (`0` configuration, `1`
//! e-mode, `0x100 | slot` aToken, `0x200 | slot` variable debt, `0x300 |
//! slot` stable debt); id reads [`ID`]` | pool << 16 | slot`, the
//! follow-up with [`ID_DATA`] set too.

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{sol, SolCall};
use liq_protocol::{
    DirtyPositions, MarketRows, MarketSlot, PositionRef, Result, StateAnswer, StateRead,
    StateWriter,
};
use liq_types::{MarketId, PositionId};

use crate::config::Config;
use crate::layout::{Reserve, UserExtra, UserReserve};

sol! {
    /// `DataTypes.ReserveDataLegacy` @ `8305565ae`. `configuration` is the
    /// one-word `ReserveConfigurationMap`, encoded inline.
    struct ReserveDataLegacy {
        uint256 configuration;
        uint128 liquidityIndex;
        uint128 currentLiquidityRate;
        uint128 variableBorrowIndex;
        uint128 currentVariableBorrowRate;
        uint128 currentStableBorrowRate;
        uint40 lastUpdateTimestamp;
        uint16 id;
        address aTokenAddress;
        address stableDebtTokenAddress;
        address variableDebtTokenAddress;
        address interestRateStrategyAddress;
        uint128 accruedToTreasury;
        uint128 unbacked;
        uint128 isolationModeTotalDebt;
    }

    interface IPoolAccount {
        /// `UserConfigurationMap` is one word, encoded inline.
        function getUserConfiguration(address user) external view returns (uint256 data);
        function getUserEMode(address user) external view returns (uint256);
        function getReserveData(address asset) external view returns (ReserveDataLegacy memory);
    }

    interface IReserveToken {
        function scaledBalanceOf(address user) external view returns (uint256);
        function balanceOf(address user) external view returns (uint256);
        function UNDERLYING_ASSET_ADDRESS() external view returns (address);
    }
}

/// High bit of a reserve-id read's tag; account tags stay below bit 48.
pub const ID: u64 = 1 << 63;
/// Set with [`ID`] on the second stage (`getReserveData`).
pub const ID_DATA: u64 = 1 << 62;

const CFG: u64 = 0;
const EMODE: u64 = 1;
const A: u64 = 0x100;
const V: u64 = 0x200;
const S: u64 = 0x300;

#[inline]
fn tag(pos: PositionId, kind: u64) -> u64 {
    u64::from(pos.0).wrapping_shl(16) | kind
}

#[inline]
fn untag(tag: u64) -> (PositionId, u64) {
    (
        PositionId(u32::try_from(tag.wrapping_shr(16)).unwrap_or(u32::MAX)),
        tag & 0xffff,
    )
}

fn addr(b: [u8; 20]) -> Address {
    Address::from(b)
}

// ---- reserve ids -----------------------------------------------------------

/// First stage of the id read for every reserve whose id is not known.
pub(crate) fn id_reads(cfg: &Config, rows: &dyn MarketRows) -> Vec<StateRead> {
    let mut out = Vec::new();
    for (pi, p) in cfg.pools.iter().enumerate() {
        let Some(rs) = rows.rows(p.market) else {
            continue;
        };
        for (slot, row) in rs.iter().enumerate().skip(1) {
            let Ok(r) = row.body::<Reserve>() else {
                continue;
            };
            if r.id_known != 0 || r.a_token == [0; 20] {
                continue;
            }
            let (Ok(pi), Ok(slot)) = (u64::try_from(pi), u64::try_from(slot)) else {
                continue;
            };
            out.push(StateRead {
                market: p.market,
                target: addr(r.a_token),
                calldata: Bytes::from(IReserveToken::UNDERLYING_ASSET_ADDRESSCall {}.abi_encode()),
                tag: ID | pi.wrapping_shl(16) | slot,
            });
        }
    }
    out
}

/// The `getReserveData` read an aToken's underlying calls for.
pub(crate) fn follow_ups(cfg: &Config, answer: StateAnswer<'_>) -> Vec<StateRead> {
    let t = answer.read.tag;
    if t & ID == 0 || t & ID_DATA != 0 || !answer.success {
        return Vec::new();
    }
    let Some(pool) = usize::try_from((t & !ID).wrapping_shr(16))
        .ok()
        .and_then(|i| cfg.pools.get(i))
    else {
        return Vec::new();
    };
    let Ok(asset) = IReserveToken::UNDERLYING_ASSET_ADDRESSCall::abi_decode_returns(answer.data)
    else {
        return Vec::new();
    };
    vec![StateRead {
        market: pool.market,
        target: pool.address,
        calldata: Bytes::from(IPoolAccount::getReserveDataCall { asset }.abi_encode()),
        tag: t | ID_DATA,
    }]
}

fn apply_ids(st: &mut dyn StateWriter, answers: &[StateAnswer<'_>]) -> Result<()> {
    for a in answers {
        let t = a.read.tag;
        if t & ID == 0 || t & ID_DATA == 0 || !a.success {
            continue;
        }
        let Ok(slot) = u16::try_from(t & 0xffff) else {
            continue;
        };
        let Ok(data) = IPoolAccount::getReserveDataCall::abi_decode_returns(a.data) else {
            continue;
        };
        let at = MarketSlot {
            market: a.read.market,
            slot,
        };
        let Ok(row) = st.market(at) else {
            continue;
        };
        let mut row = *row;
        let r = row.body_mut::<Reserve>()?;
        // The reserve this id names must be the row's own.
        if addr(r.a_token) != data.aTokenAddress {
            tracing::warn!(target: "coverage", slot, "aave reserve id read names another aToken — not stored");
            continue;
        }
        r.reserve_id = data.id;
        r.id_known = 1;
        st.set_market(at, row)?;
    }
    Ok(())
}

// ---- accounts ----------------------------------------------------------------

/// Every read that replaces `pos`'s state: the account's configuration and
/// e-mode, and each reserve's balances.
pub(crate) fn resync_reads(cfg: &Config, pos: PositionRef<'_>) -> Vec<StateRead> {
    if pos.key.protocol != cfg.protocol {
        return Vec::new();
    }
    let Some(pool) = cfg.pool_by_market(pos.key.market) else {
        return Vec::new();
    };
    let user = pos.key.user;
    let read = |target: Address, calldata: Vec<u8>, kind: u64| StateRead {
        market: pool.market,
        target,
        calldata: Bytes::from(calldata),
        tag: tag(pos.id, kind),
    };
    let mut out = vec![
        read(
            pool.address,
            IPoolAccount::getUserConfigurationCall { user }.abi_encode(),
            CFG,
        ),
        read(pool.address, IPoolAccount::getUserEModeCall { user }.abi_encode(), EMODE),
    ];
    for (slot, row) in pos.markets.iter().enumerate().skip(1) {
        let (Ok(r), Ok(slot)) = (row.body::<Reserve>(), u64::try_from(slot)) else {
            continue;
        };
        if r.a_token != [0; 20] {
            out.push(read(
                addr(r.a_token),
                IReserveToken::scaledBalanceOfCall { user }.abi_encode(),
                A | slot,
            ));
        }
        if r.v_token != [0; 20] {
            out.push(read(
                addr(r.v_token),
                IReserveToken::scaledBalanceOfCall { user }.abi_encode(),
                V | slot,
            ));
        }
        if r.s_token != [0; 20] {
            out.push(read(
                addr(r.s_token),
                IReserveToken::balanceOfCall { user }.abi_encode(),
                S | slot,
            ));
        }
    }
    out
}

/// One slot's settled values.
struct Slot {
    slot: u16,
    supply: u128,
    debt: u128,
    collateral: bool,
    stable: bool,
}

fn word(answers: &[StateAnswer<'_>], market: MarketId, want: u64) -> Option<U256> {
    let a = answers
        .iter()
        .find(|a| a.read.tag == want && a.read.market == market && a.success)?;
    a.data.get(..32).map(U256::from_be_slice)
}

/// `None` when an answer is missing, does not fit, or a collateral slot's
/// reserve id is not known yet: the account is read again.
fn settle(
    st: &dyn StateWriter,
    market: MarketId,
    pos: PositionId,
    config: U256,
    answers: &[StateAnswer<'_>],
) -> Option<(u8, Vec<Slot>)> {
    let emode = u8::try_from(word(answers, market, tag(pos, EMODE))?).ok()?;
    let rows = st.markets(market).ok()?;
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate().skip(1) {
        let r = row.body::<Reserve>().ok()?;
        let slot = u16::try_from(i).ok()?;
        let k = u64::from(slot);
        let get = |token: [u8; 20], kind: u64| -> Option<u128> {
            if token == [0; 20] {
                return Some(0);
            }
            u128::try_from(word(answers, market, tag(pos, kind | k))?).ok()
        };
        let supply = get(r.a_token, A)?;
        let debt = get(r.v_token, V)?;
        let stable = get(r.s_token, S)? != 0;
        let collateral = if supply == 0 {
            false
        } else if r.id_known != 0 {
            let bit = u32::from(r.reserve_id).checked_mul(2)?.checked_add(1)?;
            config.bit(usize::try_from(bit).ok()?)
        } else {
            return None;
        };
        out.push(Slot {
            slot,
            supply,
            debt,
            collateral,
            stable,
        });
    }
    Some((emode, out))
}

fn apply_accounts(
    cfg: &Config,
    st: &mut dyn StateWriter,
    answers: &[StateAnswer<'_>],
) -> Result<DirtyPositions> {
    let mut dirty = DirtyPositions::new();
    for a in answers {
        if a.read.tag & ID != 0 {
            continue;
        }
        let (pos, kind) = untag(a.read.tag);
        if kind != CFG || !a.success {
            continue;
        }
        let Some(pool) = cfg.pool_by_market(a.read.market) else {
            continue;
        };
        // The tag's id must still name the account the read asked about.
        let Ok(call) = IPoolAccount::getUserConfigurationCall::abi_decode(&a.read.calldata) else {
            continue;
        };
        let Ok(key) = st.position_key(pos).copied() else {
            continue;
        };
        if key.protocol != cfg.protocol || key.market != pool.market || key.user != call.user {
            continue;
        }
        let Ok(config) = IPoolAccount::getUserConfigurationCall::abi_decode_returns(a.data) else {
            continue;
        };
        let Some((emode, slots)) = settle(st, pool.market, pos, config, answers) else {
            tracing::warn!(target: "drift", account = %key.user, "aave account resync incomplete — read again");
            continue;
        };
        let mut stable_slots = 0u64;
        for s in &slots {
            st.set_supply(pos, s.slot, s.supply)?;
            st.set_debt(pos, s.slot, s.debt)?;
            let mut repr = *st.slot_extra(pos, s.slot)?;
            {
                let u: &mut UserReserve = repr.view_mut()?;
                if s.collateral {
                    u.flags |= UserReserve::USING_AS_COLLATERAL;
                } else {
                    u.flags &= !UserReserve::USING_AS_COLLATERAL;
                }
            }
            st.set_slot_extra(pos, s.slot, repr)?;
            if s.stable {
                stable_slots |= 1u64.checked_shl(u32::from(s.slot.min(63))).unwrap_or(1 << 63);
            }
        }
        let mut extra = *st.extra(pos)?;
        {
            let e: &mut UserExtra = extra.view_mut()?;
            e.emode = emode;
            e.stable_slots = stable_slots;
        }
        st.set_extra(pos, extra)?;
        dirty.push(pos);
    }
    Ok(dirty)
}

/// Fold one block's answers: reserve ids, then resynced accounts.
pub(crate) fn apply(
    cfg: &Config,
    st: &mut dyn StateWriter,
    answers: &[StateAnswer<'_>],
) -> Result<Vec<liq_protocol::DirtySet>> {
    apply_ids(st, answers)?;
    let accounts = apply_accounts(cfg, st, answers)?;
    Ok(if accounts.is_empty() {
        Vec::new()
    } else {
        vec![liq_protocol::DirtySet::Positions(accounts)]
    })
}
