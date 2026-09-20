//! Packed encoder — inverse of `PlanDecoder.sol` / `liq_exec::wire`.

use alloy_primitives::Address;
use liq_exec::wire::LegTail;
use liq_exec::wire::{
    decode_group, decode_liq_leg, decode_swap_leg, Plan, GROUP_HEAD_LEN, HEADER_LEN,
    LEG_TAKE_BALANCE, SWAP_LEG_HEAD_LEN, VENUE_UNIV3_POOL,
};
use liq_protocol::ExecutorAdapter;

use crate::error::{EncodeError, Result};
use crate::types::{BatchPlan, EncodedPlan, FlashGroup, LiqLeg, SwapLeg, ValidateCtx, LIQ_LEG_LEN};
use crate::validate;

impl EncodedPlan {
    /// Validate then pack. Byte-for-bit `PlanDecoder.sol`.
    pub fn encode(p: &BatchPlan, ctx: &ValidateCtx) -> Result<Self> {
        validate::validate(p, ctx)?;
        encode_unchecked(p)
    }
}

/// Pack without validate — tests that assert encode shape of invalid plans
/// must not exist; this is `pub(crate)` for the surplus-leg helper only.
pub(crate) fn encode_unchecked(p: &BatchPlan) -> Result<EncodedPlan> {
    if p.groups.is_empty() {
        return Err(EncodeError::NoGroups);
    }
    let mut b = Vec::with_capacity(size_hint(p)?);
    b.push(p.flags);
    b.extend_from_slice(&p.bid_bps.to_be_bytes());
    b.extend_from_slice(&p.gas_cost_wei.to_be_bytes());
    b.extend_from_slice(&p.min_profit_wei.to_be_bytes());
    if b.len() != HEADER_LEN {
        return Err(EncodeError::NoGroups);
    }
    let n_g = u8::try_from(p.groups.len()).map_err(|_| EncodeError::TooManyGroups)?;
    b.push(n_g);
    for g in &p.groups {
        encode_group(&mut b, g)?;
    }
    let n_p = u8::try_from(p.profit_swaps.len()).map_err(|_| EncodeError::TooManyLegs)?;
    b.push(n_p);
    for s in &p.profit_swaps {
        encode_swap(&mut b, s)?;
    }
    Ok(EncodedPlan(b))
}

fn encode_group(b: &mut Vec<u8>, g: &FlashGroup) -> Result<()> {
    b.push(g.provider as u8);
    b.extend_from_slice(g.flash_source.as_slice());
    b.extend_from_slice(g.debt_asset.as_slice());
    b.extend_from_slice(&g.flash_amount.to_be_bytes());
    let n_l = u8::try_from(g.liqs.len()).map_err(|_| EncodeError::TooManyLegs)?;
    let n_r = u8::try_from(g.repay_swaps.len()).map_err(|_| EncodeError::TooManyLegs)?;
    b.push(n_l);
    b.push(n_r);
    for l in &g.liqs {
        encode_liq(b, l)?;
    }
    for s in &g.repay_swaps {
        encode_swap(b, s)?;
    }
    Ok(())
}

fn encode_liq(b: &mut Vec<u8>, l: &LiqLeg) -> Result<()> {
    b.push(l.adapter as u8);
    b.extend_from_slice(l.market.as_slice());
    b.extend_from_slice(l.borrower.as_slice());
    b.extend_from_slice(l.collateral_asset.as_slice());
    b.extend_from_slice(&l.repay_amount.to_be_bytes());
    match (l.adapter, &l.tail) {
        (ExecutorAdapter::AaveV3, LegTail::None) => {}
        (
            ExecutorAdapter::AaveV4,
            LegTail::AaveV4 {
                collateral_reserve_id,
                debt_reserve_id,
            },
        ) => {
            b.extend_from_slice(&collateral_reserve_id.to_be_bytes());
            b.extend_from_slice(&debt_reserve_id.to_be_bytes());
        }
        (ExecutorAdapter::MorphoBlue, LegTail::Morpho { market_id }) => {
            b.extend_from_slice(market_id.as_slice());
        }
        (ExecutorAdapter::AaveV3, _) => return Err(EncodeError::V3TailShape),
        (ExecutorAdapter::AaveV4, _) => return Err(EncodeError::V4TailShape),
        (ExecutorAdapter::MorphoBlue, _) => return Err(EncodeError::MorphoTailShape),
    }
    Ok(())
}

fn encode_swap(b: &mut Vec<u8>, s: &SwapLeg) -> Result<()> {
    let n = u16::try_from(s.data.len()).map_err(|_| EncodeError::DataTooLong(s.data.len()))?;
    b.push(s.venue);
    b.extend_from_slice(s.token_in.as_slice());
    b.extend_from_slice(s.token_out.as_slice());
    b.push(s.flags);
    b.extend_from_slice(&s.amount.to_be_bytes());
    b.extend_from_slice(&n.to_be_bytes());
    b.extend_from_slice(&s.data);
    Ok(())
}

fn size_hint(p: &BatchPlan) -> Result<usize> {
    let mut n = HEADER_LEN
        .checked_add(1)
        .ok_or(EncodeError::TooManyGroups)?;
    for g in &p.groups {
        n = n
            .checked_add(GROUP_HEAD_LEN)
            .ok_or(EncodeError::TooManyGroups)?;
        for l in &g.liqs {
            let t = l.adapter.tail_len();
            n = n
                .checked_add(LIQ_LEG_LEN)
                .and_then(|x| x.checked_add(t))
                .ok_or(EncodeError::TooManyLegs)?;
        }
        for s in &g.repay_swaps {
            n = n
                .checked_add(SWAP_LEG_HEAD_LEN)
                .and_then(|x| x.checked_add(s.data.len()))
                .ok_or(EncodeError::TooManyLegs)?;
        }
    }
    n = n.checked_add(1).ok_or(EncodeError::TooManyLegs)?;
    for s in &p.profit_swaps {
        n = n
            .checked_add(SWAP_LEG_HEAD_LEN)
            .and_then(|x| x.checked_add(s.data.len()))
            .ok_or(EncodeError::TooManyLegs)?;
    }
    Ok(n)
}

/// Rust-decode: walk with `liq_exec::wire` (the PlanDecoder mirror).
pub fn decode_batch(bytes: &[u8]) -> Result<BatchPlan> {
    let plan = Plan::parse(bytes)?;
    let h = plan.header();
    let mut groups = Vec::new();
    let mut o = HEADER_LEN
        .checked_add(1)
        .ok_or(EncodeError::TooManyGroups)?;
    for _ in 0..h.group_count {
        let head = decode_group(bytes, o)?;
        let mut liqs = Vec::new();
        let mut lo = head.liq_offset;
        for _ in 0..head.liq_count {
            let (leg, next) = decode_liq_leg(bytes, lo)?;
            liqs.push(LiqLeg {
                adapter: leg.adapter,
                market: leg.market,
                borrower: leg.borrower,
                collateral_asset: leg.collateral_asset,
                repay_amount: leg.repay_amount,
                tail: leg.tail,
                protocol_pull: 0,
            });
            lo = next;
        }
        let mut repay_swaps = Vec::new();
        let mut so = head.repay_swap_offset;
        for _ in 0..head.repay_swap_count {
            let (s, next) = decode_swap_leg(bytes, so)?;
            repay_swaps.push(swap_owned(s));
            so = next;
        }
        groups.push(FlashGroup {
            provider: head.provider,
            flash_source: head.flash_source,
            debt_asset: head.debt_asset,
            flash_amount: head.flash_amount,
            liqs,
            repay_swaps,
        });
        o = head.next;
    }
    let mut profit_swaps = Vec::new();
    let po = h.profit_swap_offset;
    let n_p =
        bytes
            .get(po)
            .copied()
            .ok_or(EncodeError::Wire(liq_exec::wire::WireError::Truncated {
                need: po.saturating_add(1),
                have: bytes.len(),
            }))?;
    let mut so = po.checked_add(1).ok_or(EncodeError::TooManyLegs)?;
    for _ in 0..n_p {
        let (s, next) = decode_swap_leg(bytes, so)?;
        profit_swaps.push(swap_owned(s));
        so = next;
    }
    Ok(BatchPlan {
        flags: h.flags,
        bid_bps: h.bid_bps,
        gas_cost_wei: h.gas_cost_wei,
        min_profit_wei: h.min_profit,
        groups,
        profit_swaps,
    })
}

fn swap_owned(s: liq_exec::wire::SwapLeg<'_>) -> SwapLeg {
    SwapLeg {
        venue: s.venue,
        token_in: s.token_in,
        token_out: s.token_out,
        flags: s.flags,
        amount: s.amount,
        data: s.data.to_vec(),
    }
}

/// Insert a `TAKE_BALANCE` profit leg `debtAsset → WETH` for every group
/// whose flash exceeds protocol pull, if one is not already present.
///
/// Carry-forward from 10A: V3/V4 clamp leaves surplus debt token with no
/// route to WETH unless this leg exists.
pub fn ensure_surplus_borrow_profit_legs(plan: &mut BatchPlan, weth: Address, univ3_pool: Address) {
    for g in &plan.groups {
        let pull: u128 = g
            .liqs
            .iter()
            .map(|l| l.protocol_pull)
            .fold(0, u128::saturating_add);
        if g.debt_asset == weth || g.flash_amount <= pull {
            continue;
        }
        let has = plan.profit_swaps.iter().any(|s| {
            s.token_in == g.debt_asset && s.token_out == weth && s.flags & LEG_TAKE_BALANCE != 0
        });
        if has {
            continue;
        }
        plan.profit_swaps.push(SwapLeg {
            venue: VENUE_UNIV3_POOL,
            token_in: g.debt_asset,
            token_out: weth,
            flags: LEG_TAKE_BALANCE,
            amount: 0,
            data: univ3_pool.to_vec(),
        });
    }
}
