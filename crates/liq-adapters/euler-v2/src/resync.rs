//! Resync: one account's state on one debt vault read back from chain
//! (`euler-xyz/euler-vault-kit` @ `bfb325a6`, the EVC), for the drift check.
//!
//! | store | view |
//! |---|---|
//! | `debt[0]` (owed, `assets << 31`) + user accumulator | debt vault `debtOfExact(account)` and `interestAccumulator()` at the same block |
//! | `supply[slot]` (collateral shares) | collateral vault `balanceOf(account)` |
//! | EVC enable bit per slot | `EVC.isCollateralEnabled(account, collateral)` |
//!
//! `debtOfExact` is the owed amount accrued to the read's block, so it is
//! stored against that block's accumulator: health's `owed · acc(t) /
//! acc(read)` then grows it from there, as the vault does (one floor in the
//! rebase, below an owed unit — `2^-31` of an asset).
//!
//! Tags: `position id << 16 | kind` — `0` debt, `1` accumulator, `0x100 |
//! slot` collateral shares, `0x200 | slot` collateral enabled.

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{sol, SolCall};
use liq_protocol::{
    DirtyPositions, DirtySet, PositionRef, Result, StateAnswer, StateRead, StateWriter,
};
use liq_types::{MarketId, PositionId};

use crate::config::Config;
use crate::layout::{CollRow, UserExtra, VaultRow, DEBT_SLOT};
use crate::math::{addr_from, u256_to_limbs};

sol! {
    interface IEVaultAccount {
        function debtOfExact(address account) external view returns (uint256);
        function interestAccumulator() external view returns (uint256);
        function balanceOf(address account) external view returns (uint256);
    }

    interface IEVCAccount {
        function isCollateralEnabled(address account, address vault) external view returns (bool);
    }
}

const DEBT: u64 = 0;
const ACC: u64 = 1;
const SHARES: u64 = 0x100;
const ENABLED: u64 = 0x200;

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

/// Every read that replaces `pos`'s state on its debt vault.
pub(crate) fn resync_reads(cfg: &Config, pos: PositionRef<'_>) -> Vec<StateRead> {
    if pos.key.protocol != cfg.protocol {
        return Vec::new();
    }
    let Some(debt_vault) = pos
        .markets
        .get(usize::from(DEBT_SLOT))
        .and_then(|r| r.body::<VaultRow>().ok())
        .map(|v| addr_from(v.vault))
        .filter(|v| !v.is_zero())
    else {
        return Vec::new();
    };
    let account = pos.key.user;
    let read = |target: Address, calldata: Vec<u8>, kind: u64| StateRead {
        market: pos.key.market,
        target,
        calldata: Bytes::from(calldata),
        tag: tag(pos.id, kind),
    };
    let mut out = vec![
        read(
            debt_vault,
            IEVaultAccount::debtOfExactCall { account }.abi_encode(),
            DEBT,
        ),
        read(
            debt_vault,
            IEVaultAccount::interestAccumulatorCall {}.abi_encode(),
            ACC,
        ),
    ];
    for (slot, row) in pos.markets.iter().enumerate().skip(1) {
        let (Ok(c), Ok(k)) = (row.body::<CollRow>(), u64::try_from(slot)) else {
            continue;
        };
        let vault = addr_from(c.vault);
        if vault.is_zero() {
            continue;
        }
        out.push(read(
            vault,
            IEVaultAccount::balanceOfCall { account }.abi_encode(),
            SHARES | k,
        ));
        out.push(read(
            cfg.evc,
            IEVCAccount::isCollateralEnabledCall { account, vault }.abi_encode(),
            ENABLED | k,
        ));
    }
    out
}

fn answer<'a>(
    answers: &'a [StateAnswer<'a>],
    market: MarketId,
    want: u64,
) -> Option<&'a StateAnswer<'a>> {
    answers
        .iter()
        .find(|a| a.read.tag == want && a.read.market == market && a.success)
}

/// One account's settled state, decoded before any write.
struct Settled {
    owed: u128,
    acc: U256,
    slots: Vec<(u16, u128, bool)>,
}

fn settle(
    st: &dyn StateWriter,
    market: MarketId,
    pos: PositionId,
    debt: &StateAnswer<'_>,
    answers: &[StateAnswer<'_>],
) -> Option<Settled> {
    let owed = IEVaultAccount::debtOfExactCall::abi_decode_returns(debt.data).ok()?;
    let acc = answer(answers, market, tag(pos, ACC))?;
    let acc = IEVaultAccount::interestAccumulatorCall::abi_decode_returns(acc.data).ok()?;
    let rows = st.markets(market).ok()?;
    let mut slots = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate().skip(1) {
        let c = row.body::<CollRow>().ok()?;
        if c.vault == [0; 20] {
            continue;
        }
        let slot = u16::try_from(i).ok()?;
        let k = u64::from(slot);
        let shares = answer(answers, market, tag(pos, SHARES | k))?;
        let shares = IEVaultAccount::balanceOfCall::abi_decode_returns(shares.data).ok()?;
        let on = answer(answers, market, tag(pos, ENABLED | k))?;
        let on = IEVCAccount::isCollateralEnabledCall::abi_decode_returns(on.data).ok()?;
        slots.push((slot, u128::try_from(shares).ok()?, on));
    }
    Some(Settled {
        owed: u128::try_from(owed).ok()?,
        acc,
        slots,
    })
}

/// Fold the resynced accounts in one block's answers.
pub(crate) fn apply(
    cfg: &Config,
    st: &mut dyn StateWriter,
    answers: &[StateAnswer<'_>],
) -> Result<Vec<DirtySet>> {
    let mut dirty = DirtyPositions::new();
    for a in answers {
        let (pos, kind) = untag(a.read.tag);
        if kind != DEBT || !a.success {
            continue;
        }
        // The tag's id must still name the account the read asked about.
        let Ok(call) = IEVaultAccount::debtOfExactCall::abi_decode(&a.read.calldata) else {
            continue;
        };
        let Ok(key) = st.position_key(pos).copied() else {
            continue;
        };
        if key.protocol != cfg.protocol || key.market != a.read.market || key.user != call.account {
            continue;
        }
        let Some(s) = settle(st, key.market, pos, a, answers) else {
            tracing::warn!(target: "drift", account = %key.user, "euler account resync incomplete — read again");
            continue;
        };
        st.set_debt(pos, DEBT_SLOT, s.owed)?;
        let mut mask = 0u128;
        for &(slot, shares, on) in &s.slots {
            st.set_supply(pos, slot, shares)?;
            if on {
                mask |= 1u128.checked_shl(u32::from(slot)).unwrap_or(0);
            }
        }
        let (lo, hi) = u256_to_limbs(s.acc)?;
        let mut extra = *st.extra(pos)?;
        {
            let u: &mut UserExtra = extra.view_mut()?;
            u.user_accumulator_lo = lo;
            u.user_accumulator_hi = hi;
            u.enabled_mask = mask;
            u.flags |= UserExtra::ACC_KNOWN;
        }
        st.set_extra(pos, extra)?;
        dirty.push(pos);
    }
    Ok(if dirty.is_empty() {
        Vec::new()
    } else {
        vec![DirtySet::Positions(dirty)]
    })
}
