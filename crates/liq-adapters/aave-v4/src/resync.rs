//! Resync: one account's state on one spoke read back from chain
//! (`aave/aave-v4` @ `40232a0a`, `Spoke.sol`), for the drift check.
//!
//! | store | view |
//! |---|---|
//! | `supply[slot]`, `debt[slot]`, premium shares and offset, dynamic-config key | `getUserPosition(reserveId, user)` — the stored `UserPosition` itself |
//! | collateral flag | `getUserReserveStatus(reserveId, user).0` |
//! | the key's collateral factor, liquidation fee, max bonus | `getDynamicReserveConfig(reserveId, key)`, a follow-up once the key is read |
//! | risk premium | `getUserLastRiskPremium(user)` |
//!
//! Slot `r + 1` is reserve `r` (slot 0 is the spoke's meta row; reserves
//! are appended by `AddReserve`, never removed). The narrow solidity types
//! (`uint120`, `int200`, `uint16`, `uint32`) are decoded as full words —
//! the ABI encodes each in one sign- or zero-extended word.
//!
//! Tags: `position id << 16 | kind` — `0` risk premium, `0x100 | slot`
//! position, `0x200 | slot` status, `0x300 | slot` dynamic config.

use alloy_primitives::{Bytes, I256, U256};
use alloy_sol_types::{sol, SolCall};
use liq_protocol::{
    DirtyPositions, DirtySet, PositionRef, Result, StateAnswer, StateRead, StateWriter,
};
use liq_types::{MarketId, PositionId};

use crate::config::Config;
use crate::layout::{UserExtra, UserReserve};
use crate::math::split;

sol! {
    /// `ISpoke.UserPosition`, each field one word.
    struct UserPositionWords {
        uint256 drawnShares;
        uint256 premiumShares;
        int256 premiumOffsetRay;
        uint256 suppliedShares;
        uint256 dynamicConfigKey;
    }

    /// `ISpoke.DynamicReserveConfig`, field order as declared.
    struct DynamicConfigWords {
        uint256 collateralFactor;
        uint256 maxLiquidationBonus;
        uint256 liquidationFee;
    }

    interface ISpokeAccount {
        function getUserPosition(uint256 reserveId, address user)
            external view returns (UserPositionWords memory);
        function getUserReserveStatus(uint256 reserveId, address user)
            external view returns (bool collateral, bool borrowing);
        function getDynamicReserveConfig(uint256 reserveId, uint32 dynamicConfigKey)
            external view returns (DynamicConfigWords memory);
        function getUserLastRiskPremium(address user) external view returns (uint256);
    }
}

const RISK: u64 = 0;
const POS: u64 = 0x100;
const STATUS: u64 = 0x200;
const DYN: u64 = 0x300;

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

/// Every read that replaces `pos`'s state on its spoke.
pub(crate) fn resync_reads(cfg: &Config, pos: PositionRef<'_>) -> Vec<StateRead> {
    if pos.key.protocol != cfg.protocol {
        return Vec::new();
    }
    let Some(spoke) = cfg.spoke_by_market(pos.key.market) else {
        return Vec::new();
    };
    let user = pos.key.user;
    let read = |calldata: Vec<u8>, kind: u64| StateRead {
        market: spoke.market,
        target: spoke.address,
        calldata: Bytes::from(calldata),
        tag: tag(pos.id, kind),
    };
    let mut out = vec![read(
        ISpokeAccount::getUserLastRiskPremiumCall { user }.abi_encode(),
        RISK,
    )];
    for slot in 1..pos.markets.len() {
        let (Ok(k), Some(reserve)) = (u64::try_from(slot), slot.checked_sub(1)) else {
            continue;
        };
        let reserve_id = U256::from(reserve);
        out.push(read(
            ISpokeAccount::getUserPositionCall {
                reserveId: reserve_id,
                user,
            }
            .abi_encode(),
            POS | k,
        ));
        out.push(read(
            ISpokeAccount::getUserReserveStatusCall {
                reserveId: reserve_id,
                user,
            }
            .abi_encode(),
            STATUS | k,
        ));
    }
    out
}

/// The dynamic config one position answer's key calls for.
pub(crate) fn follow_ups(answer: StateAnswer<'_>) -> Vec<StateRead> {
    let (pos, kind) = untag(answer.read.tag);
    if kind & 0xff00 != POS || !answer.success {
        return Vec::new();
    }
    let (Ok(call), Ok(p)) = (
        ISpokeAccount::getUserPositionCall::abi_decode(&answer.read.calldata),
        ISpokeAccount::getUserPositionCall::abi_decode_returns(answer.data),
    ) else {
        return Vec::new();
    };
    let Ok(key) = u32::try_from(p.dynamicConfigKey) else {
        return Vec::new();
    };
    vec![StateRead {
        market: answer.read.market,
        target: answer.read.target,
        calldata: Bytes::from(
            ISpokeAccount::getDynamicReserveConfigCall {
                reserveId: call.reserveId,
                dynamicConfigKey: key,
            }
            .abi_encode(),
        ),
        tag: tag(pos, DYN | (kind & 0xff)),
    }]
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

struct Slot {
    slot: u16,
    supplied: u128,
    drawn: u128,
    user: UserReserve,
}

fn settle(
    st: &dyn StateWriter,
    market: MarketId,
    pos: PositionId,
    answers: &[StateAnswer<'_>],
) -> Option<Vec<Slot>> {
    let rows = st.markets(market).ok()?;
    let mut out = Vec::with_capacity(rows.len());
    for i in 1..rows.len() {
        let slot = u16::try_from(i).ok()?;
        let k = u64::from(slot);
        let p = answer(answers, market, tag(pos, POS | k))?;
        let p = ISpokeAccount::getUserPositionCall::abi_decode_returns(p.data).ok()?;
        let status = answer(answers, market, tag(pos, STATUS | k))?;
        let status =
            ISpokeAccount::getUserReserveStatusCall::abi_decode_returns(status.data).ok()?;
        let dyn_cfg = answer(answers, market, tag(pos, DYN | k))?;
        let dyn_cfg =
            ISpokeAccount::getDynamicReserveConfigCall::abi_decode_returns(dyn_cfg.data).ok()?;
        let (lo, hi) = split(I256::into_raw(p.premiumOffsetRay));
        let user = UserReserve {
            premium_shares: u128::try_from(p.premiumShares).ok()?,
            premium_offset_lo: lo,
            premium_offset_hi: hi,
            collateral_factor: u16::try_from(dyn_cfg.collateralFactor).ok()?,
            liquidation_fee: u16::try_from(dyn_cfg.liquidationFee).ok()?,
            max_liquidation_bonus: u32::try_from(dyn_cfg.maxLiquidationBonus).ok()?,
            dyn_key: u32::try_from(p.dynamicConfigKey).ok()?,
            flags: if status.collateral {
                UserReserve::USING_AS_COLLATERAL
            } else {
                0
            },
            _pad: [0; 3],
        };
        out.push(Slot {
            slot,
            supplied: u128::try_from(p.suppliedShares).ok()?,
            drawn: u128::try_from(p.drawnShares).ok()?,
            user,
        });
    }
    Some(out)
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
        if kind != RISK || !a.success {
            continue;
        }
        let Some(spoke) = cfg.spoke_by_market(a.read.market) else {
            continue;
        };
        // The tag's id must still name the account the read asked about.
        let Ok(call) = ISpokeAccount::getUserLastRiskPremiumCall::abi_decode(&a.read.calldata)
        else {
            continue;
        };
        let Ok(key) = st.position_key(pos).copied() else {
            continue;
        };
        if key.protocol != cfg.protocol || key.market != spoke.market || key.user != call.user {
            continue;
        }
        let (Ok(risk), Some(slots)) = (
            ISpokeAccount::getUserLastRiskPremiumCall::abi_decode_returns(a.data),
            settle(st, spoke.market, pos, answers),
        ) else {
            tracing::warn!(target: "drift", account = %key.user, "aave v4 account resync incomplete — read again");
            continue;
        };
        let Ok(risk) = u32::try_from(risk) else {
            continue;
        };
        for s in &slots {
            st.set_supply(pos, s.slot, s.supplied)?;
            st.set_debt(pos, s.slot, s.drawn)?;
            let mut repr = *st.slot_extra(pos, s.slot)?;
            *repr.view_mut::<UserReserve>()? = s.user;
            st.set_slot_extra(pos, s.slot, repr)?;
        }
        let mut extra = *st.extra(pos)?;
        extra.view_mut::<UserExtra>()?.risk_premium = risk;
        st.set_extra(pos, extra)?;
        dirty.push(pos);
    }
    Ok(if dirty.is_empty() {
        Vec::new()
    } else {
        vec![DirtySet::Positions(dirty)]
    })
}
