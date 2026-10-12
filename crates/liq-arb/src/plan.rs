//! A [`Backrun`] as an `ArbExecutor` plan: one flash group whose swap legs
//! are the whole cycle, then the profit closed into WETH (PLAN-ENCODING,
//! every group with zero liquidation legs; [`EncodedPlan::encode_arb`]).
//!
//! Funding is always flash, never inventory — a flash swap where the cycle
//! has one, a flash loan where it does not:
//!
//! * Flash swap: a `StartIn` backrun whose anchor is a Uniswap V3 pool
//!   contract. The anchor pays the bought token first (an exact-output
//!   swap of what the exit sells), the exit runs in its callback, and the
//!   pool is paid from the exit's output. No loan, and one swap fewer.
//! * Flash loan otherwise: a free flash of the start token (V4 `take`,
//!   Morpho) from [`PlanCtx::sources`], then the anchor and the exit as
//!   swap legs.
//!
//! Swap legs spend set amounts, as the solve allocated them; the last leg
//! on each token takes the whole balance ([`LEG_TAKE_BALANCE`]), so a unit
//! of rounding can never strand or overdraw it.

use crate::anchor::{Backrun, Orientation};
use crate::ArbError;
use alloy_primitives::{Address, U256};
use liq_plan::{
    BatchPlan, EncodedPlan, FlashGroup, SwapLeg, FLAG_SWEEP, LEG_TAKE_BALANCE, VENUE_CHAIN,
};
use liq_router::{venue_bytes, Allocation, ExitQuote, PoolBook, PoolId};
use liq_types::{AssetId, FlashProvider};

/// A free flash source for one asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlashSource {
    pub asset: AssetId,
    /// `UniV4` or `Morpho`.
    pub provider: FlashProvider,
    /// The PoolManager or the Morpho singleton.
    pub source: Address,
    /// What it can lend now.
    pub available: U256,
}

/// What a plan needs besides the backrun.
#[derive(Clone, Copy, Debug)]
pub struct PlanCtx<'a> {
    pub weth: Address,
    pub weth_id: AssetId,
    pub sources: &'a [FlashSource],
    /// Coinbase bid, basis points of realized net.
    pub bid_bps: u16,
    /// What must be kept after the bid, wei.
    pub min_profit_wei: u128,
    pub base_fee_wei: u128,
    /// Sweep WETH to the profit sink after this plan.
    pub sweep: bool,
    /// Fund a `StartIn` backrun on a Uniswap V3 anchor with that anchor's
    /// flash swap. Off forces a flash loan (fork tests that compare the
    /// chain to the model to the unit use that: the V3 pool's exact-output
    /// charge for the bought amount can be a unit under the exact-input
    /// amount the model sized).
    pub flash_swap: bool,
}

/// The plan for `r`, and its encoding.
pub fn build_plan(
    book: &PoolBook,
    r: &Backrun,
    ctx: &PlanCtx<'_>,
) -> Result<(BatchPlan, EncodedPlan), ArbError> {
    let anchor = book
        .get(r.anchor.pool)
        .ok_or(ArbError::Missing("anchor pool"))?;
    let a = token(book, r.anchor.pool, r.anchor.i)?;
    let b = token(book, r.anchor.pool, r.anchor.j)?;
    let start = match r.orientation {
        Orientation::StartIn => a,
        Orientation::StartOut => b,
    };
    let exit = exit_legs(book, &r.exit, b, a)?;
    let anchor_leg = |flags: u8, amount: u128| -> Result<SwapLeg, ArbError> {
        let (venue, data) = venue_bytes(book, r.anchor.pool, r.anchor.i, r.anchor.j)
            .map_err(|_| ArbError::Missing("anchor venue"))?;
        Ok(SwapLeg {
            venue,
            token_in: a,
            token_out: b,
            flags,
            amount,
            data,
        })
    };
    let flash_swap = ctx.flash_swap
        && r.orientation == Orientation::StartIn
        && anchor.is_v3_contract()
        && matches!(&anchor.state, liq_router::PoolState::V3(s) if s.factory == 0);
    let group = if flash_swap {
        FlashGroup {
            provider: FlashProvider::UniV3Swap,
            flash_source: anchor.address,
            debt_asset: b,
            flash_amount: u128_of(r.exit_in)?,
            fee_bps: 0,
            liqs: Vec::new(),
            repay_swaps: exit,
        }
    } else {
        let src = ctx
            .sources
            .iter()
            .filter(|s| s.asset == r.start && s.available >= r.amount_in)
            .find(|s| matches!(s.provider, FlashProvider::UniV4 | FlashProvider::Morpho))
            .ok_or(ArbError::Missing("flash source"))?;
        let amount = u128_of(r.amount_in)?;
        let repay_swaps = match r.orientation {
            Orientation::StartIn => {
                let mut v = Vec::with_capacity(exit.len().saturating_add(1));
                v.push(anchor_leg(0, amount)?);
                v.extend(exit);
                v
            }
            Orientation::StartOut => {
                let mut v = exit;
                v.push(anchor_leg(LEG_TAKE_BALANCE, 0)?);
                v
            }
        };
        FlashGroup {
            provider: src.provider,
            flash_source: src.source,
            debt_asset: start,
            flash_amount: amount,
            fee_bps: 0,
            liqs: Vec::new(),
            repay_swaps,
        }
    };
    let profit_swaps = if r.start == ctx.weth_id {
        Vec::new()
    } else {
        vec![closer(book, r.start, ctx.weth_id, start, ctx.weth)?]
    };
    let gas_cost_wei = u128::from(r.gas)
        .checked_mul(ctx.base_fee_wei)
        .ok_or(ArbError::Math)?;
    let plan = BatchPlan {
        flags: if ctx.sweep { FLAG_SWEEP } else { 0 },
        bid_bps: ctx.bid_bps,
        gas_cost_wei,
        min_profit_wei: ctx.min_profit_wei,
        groups: vec![group],
        profit_swaps,
    };
    let enc = EncodedPlan::encode_arb(&plan, ctx.weth).map_err(|_| ArbError::Unencodable)?;
    Ok((plan, enc))
}

fn u128_of(x: U256) -> Result<u128, ArbError> {
    u128::try_from(x).map_err(|_| ArbError::Math)
}

fn token(book: &PoolBook, pool: PoolId, coin: u8) -> Result<Address, ArbError> {
    book.get(pool)
        .and_then(|p| p.tokens.get(usize::from(coin)).copied())
        .ok_or(ArbError::Missing("pool token"))
}

/// Sell all of `tin` for `tout` across `allocs`: set amounts on every pool
/// but the largest, which takes the balance and runs last.
fn sell_legs(
    book: &PoolBook,
    allocs: &[Allocation],
    tin: Address,
    tout: Address,
    out: &mut Vec<SwapLeg>,
) -> Result<(), ArbError> {
    let sold = || allocs.iter().filter(|x| !x.amount_in.is_zero());
    let close = sold()
        .enumerate()
        .max_by_key(|(_, x)| x.amount_in)
        .map(|(k, _)| k)
        .ok_or(ArbError::Missing("exit allocation"))?;
    let mut closer = None;
    for (k, x) in sold().enumerate() {
        let (venue, data) = venue_bytes(book, x.leg.pool, x.leg.i, x.leg.j)
            .map_err(|_| ArbError::Missing("exit venue"))?;
        let leg = SwapLeg {
            venue,
            token_in: tin,
            token_out: tout,
            flags: 0,
            amount: u128_of(x.amount_in)?,
            data,
        };
        if k == close {
            closer = Some(SwapLeg {
                flags: LEG_TAKE_BALANCE,
                amount: 0,
                ..leg
            });
        } else {
            out.push(leg);
        }
    }
    out.extend(closer);
    Ok(())
}

/// The exit `q` (selling `b` for `a`) as swap legs: a lone chain is one
/// chain leg on the whole balance; otherwise the hub's pools (b → hub),
/// then the direct pools (hub or b → a).
fn exit_legs(
    book: &PoolBook,
    q: &ExitQuote,
    b: Address,
    a: Address,
) -> Result<Vec<SwapLeg>, ArbError> {
    if !crate::realize::replayable(q) {
        return Err(ArbError::Unreplayable);
    }
    let mut legs = Vec::with_capacity(
        q.allocs
            .len()
            .saturating_add(q.hub.as_ref().map_or(0, |h| h.allocs.len())),
    );
    if let Some(c) = &q.chain {
        legs.push(SwapLeg {
            venue: VENUE_CHAIN,
            token_in: b,
            token_out: a,
            flags: LEG_TAKE_BALANCE,
            amount: 0,
            data: c.data.clone(),
        });
        return Ok(legs);
    }
    let mut tin = b;
    if let Some(h) = &q.hub {
        let first = h
            .allocs
            .iter()
            .find(|x| !x.amount_in.is_zero())
            .ok_or(ArbError::Missing("hub allocation"))?;
        let hub = token(book, first.leg.pool, first.leg.j)?;
        sell_legs(book, &h.allocs, b, hub, &mut legs)?;
        tin = hub;
    }
    sell_legs(book, &q.allocs, tin, a, &mut legs)?;
    Ok(legs)
}

/// The pool the profit in `start` closes into WETH through: the cheapest
/// live one between them (the gas `anchor` charged for it).
fn closer_leg(
    book: &PoolBook,
    start: AssetId,
    weth_id: AssetId,
) -> Result<liq_router::Leg, ArbError> {
    book.legs(start, weth_id)
        .iter()
        .filter(|l| book.get(l.pool).is_some_and(|p| p.is_live()))
        .min_by_key(|l| book.get(l.pool).map_or(u64::MAX, |p| p.hop_gas))
        .copied()
        .ok_or(ArbError::Missing("profit closer"))
}

/// Close the profit in `start` into WETH, whole balance.
fn closer(
    book: &PoolBook,
    start: AssetId,
    weth_id: AssetId,
    start_addr: Address,
    weth: Address,
) -> Result<SwapLeg, ArbError> {
    let leg = closer_leg(book, start, weth_id)?;
    let (venue, data) =
        venue_bytes(book, leg.pool, leg.i, leg.j).map_err(|_| ArbError::Missing("closer venue"))?;
    Ok(SwapLeg {
        venue,
        token_in: start_addr,
        token_out: weth,
        flags: LEG_TAKE_BALANCE,
        amount: 0,
        data,
    })
}

/// What a flash-loan-funded plan for `r` leaves the executor in WETH, by
/// the models: the cycle replayed swap by swap, then the profit closed into
/// WETH on the state the cycle left (restored after). The fork tests hold
/// the chain to this, to the unit.
pub fn expected_weth_gross(
    book: &mut PoolBook,
    scratch: &mut crate::scratch::Scratch,
    r: &Backrun,
    weth_id: AssetId,
) -> Result<U256, ArbError> {
    use crate::scratch::EVAL;
    let closer = if r.start == weth_id {
        None
    } else {
        Some(closer_leg(book, r.start, weth_id)?)
    };
    scratch.save(EVAL, book, r.anchor.pool);
    crate::realize::for_each_pool(&r.exit, |id| scratch.save(EVAL, book, id));
    if let Some(l) = closer {
        scratch.save(EVAL, book, l.pool);
    }
    let out = (|| {
        let back = crate::realize::replay(book, r)?;
        let profit = back.checked_sub(r.amount_in).ok_or(ArbError::Math)?;
        match closer {
            None => Ok(profit),
            Some(l) => Ok(book
                .get_mut(l.pool)
                .ok_or(ArbError::Missing("closer pool"))?
                .apply_exact_in(l.i, l.j, profit)?),
        }
    })();
    scratch.restore(EVAL, book);
    out
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::scratch::Scratch;
    use crate::testbook::*;
    use crate::{best_backrun, Anchor};
    use liq_plan::{decode_batch, VENUE_UNIV2_POOL};

    const ETH: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
    const PM: Address = Address::repeat_byte(0x44);

    fn plan_ctx(sources: &[FlashSource]) -> PlanCtx<'_> {
        PlanCtx {
            weth: tok(0),
            weth_id: A,
            sources,
            bid_bps: 500,
            min_profit_wei: 1,
            base_fee_wei: 1_000_000_000,
            sweep: false,
            flash_swap: true,
        }
    }

    fn solve(flash: &[AssetId]) -> (PoolBook, Backrun) {
        let (mut b, p0, _) = book();
        let mut s = Scratch::new(&b);
        let per = [ETH, ETH];
        let r = best_backrun(
            &mut b,
            &mut s,
            &ctx(&per, flash),
            Anchor {
                pool: p0,
                i: 0,
                j: 1,
                scale: U256::from(E21 / 100),
            },
        )
        .unwrap()
        .unwrap();
        (b, r)
    }

    /// StartIn from WETH on a V4 flash: anchor leg (set amount), then the
    /// exit closing B on its whole balance; no profit legs (WETH start).
    /// The bytes decode (the Solidity decoder's Rust mirror) to the plan.
    #[test]
    fn start_in_plan_round_trips() {
        let (b, r) = solve(&[A]);
        let src = [FlashSource {
            asset: A,
            provider: FlashProvider::UniV4,
            source: PM,
            available: U256::MAX,
        }];
        let (plan, enc) = build_plan(&b, &r, &plan_ctx(&src)).unwrap();
        assert_eq!(decode_batch(enc.as_bytes()).unwrap(), plan);
        let g = &plan.groups[0];
        assert_eq!(
            (g.provider, g.flash_source, g.debt_asset),
            (FlashProvider::UniV4, PM, tok(0))
        );
        assert_eq!(U256::from(g.flash_amount), r.amount_in);
        assert!(g.liqs.is_empty());
        let legs: Vec<_> = g
            .repay_swaps
            .iter()
            .map(|l| {
                (
                    l.venue,
                    l.token_in,
                    l.token_out,
                    l.flags,
                    U256::from(l.amount),
                )
            })
            .collect();
        assert_eq!(
            legs,
            vec![
                (VENUE_UNIV2_POOL, tok(0), tok(1), 0, r.amount_in),
                (
                    VENUE_UNIV2_POOL,
                    tok(1),
                    tok(0),
                    LEG_TAKE_BALANCE,
                    U256::ZERO
                ),
            ]
        );
        assert!(plan.profit_swaps.is_empty());
        assert_eq!(plan.gas_cost_wei, u128::from(r.gas) * 1_000_000_000);
    }

    /// StartOut from B: the exit first (closing the flashed B), the anchor
    /// last on the whole A balance, and the B profit closed into WETH.
    #[test]
    fn start_out_plan_closes_profit_into_weth() {
        let (b, r) = solve(&[B]);
        let src = [FlashSource {
            asset: B,
            provider: FlashProvider::Morpho,
            source: PM,
            available: U256::MAX,
        }];
        let (plan, enc) = build_plan(&b, &r, &plan_ctx(&src)).unwrap();
        assert_eq!(decode_batch(enc.as_bytes()).unwrap(), plan);
        let g = &plan.groups[0];
        assert_eq!((g.provider, g.debt_asset), (FlashProvider::Morpho, tok(1)));
        let legs: Vec<_> = g
            .repay_swaps
            .iter()
            .map(|l| (l.token_in, l.token_out, l.flags))
            .collect();
        assert_eq!(
            legs,
            vec![
                (tok(1), tok(0), LEG_TAKE_BALANCE),
                (tok(0), tok(1), LEG_TAKE_BALANCE)
            ]
        );
        let p = &plan.profit_swaps;
        assert_eq!(p.len(), 1);
        assert_eq!(
            (p[0].token_in, p[0].token_out, p[0].flags),
            (tok(1), tok(0), LEG_TAKE_BALANCE)
        );
    }

    /// WETH start: the WETH the plan leaves is the cycle's output less
    /// its input (no closer), and the book comes back untouched.
    #[test]
    fn expected_gross_of_a_weth_cycle() {
        let (mut b, r) = solve(&[A]);
        let before: Vec<_> = b.pools().iter().map(|p| p.state.clone()).collect();
        let mut s = Scratch::new(&b);
        assert_eq!(
            expected_weth_gross(&mut b, &mut s, &r, A).unwrap(),
            r.amount_out - r.amount_in
        );
        assert!(b.pools().iter().zip(&before).all(|(p, st)| &p.state == st));
    }

    /// B start: the B profit closes into WETH through the cheapest pool on
    /// the state the cycle left — the V2 oracle on those reserves.
    #[test]
    fn expected_gross_closes_a_token_profit() {
        let (mut b, r) = solve(&[B]);
        let mut s = Scratch::new(&b);
        let got = expected_weth_gross(&mut b, &mut s, &r, A).unwrap();
        // Replay by hand: exit (pool 1, B→A), anchor (pool 0, A→B), then
        // close the B profit through the closer (both pools cost the same
        // gas; the first listed wins), each on the reserves the last left.
        let (mut r0, mut r1) = (
            reserves(&b, crate::testbook::book().1),
            reserves(&b, crate::testbook::book().2),
        );
        let mid = v2_out(r.amount_in, r1.1, r1.0);
        r1 = (r1.0 - mid, r1.1 + r.amount_in);
        let back = v2_out(mid, r0.0, r0.1);
        r0 = (r0.0 + mid, r0.1 - back);
        let profit = back - r.amount_in;
        let leg = b.legs(B, A)[0];
        let (rin, rout) = if leg.pool == crate::testbook::book().1 {
            (r0.1, r0.0)
        } else {
            (r1.1, r1.0)
        };
        assert_eq!(got, v2_out(profit, rin, rout));
    }

    /// No free source that can lend the amount: no plan, not a guess.
    #[test]
    fn no_source_no_plan() {
        let (b, r) = solve(&[A]);
        let src = [FlashSource {
            asset: A,
            provider: FlashProvider::UniV4,
            source: PM,
            available: r.amount_in - U256::from(1u8),
        }];
        assert_eq!(
            build_plan(&b, &r, &plan_ctx(&src)).unwrap_err(),
            ArbError::Missing("flash source")
        );
    }
}
