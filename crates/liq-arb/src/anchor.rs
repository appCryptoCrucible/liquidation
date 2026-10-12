//! The anchored backrun: trade the pool a victim moved back toward the
//! market and close the loop through the liquidation exit.
//!
//! An [`Anchor`] is that pool and the direction that undoes the victim's
//! move (coin `i` in, `j` out). Two ways to fund it, each needing a flash-
//! loanable start token:
//!
//! * [`Orientation::StartIn`] — flash `a` (coin `i`), sell it into the
//!   anchor for `b`, exit `b → a` through [`solve_pair`], repay, keep the
//!   rest of `a`.
//! * [`Orientation::StartOut`] — flash `b`, exit `b → a` first, buy `b`
//!   back through the anchor, repay, keep the rest of `b`.
//!
//! The amount is sized by [`maximise`] on profit after gas, each step one
//! exit solve. When the exit routes through the anchor itself, that step is
//! taken in execution order on scratch state (the anchor swapped first, or
//! the exit applied first), so a quote never reads a state the plan will
//! not see. At the chosen amount the exit is refined once
//! ([`refine_exit`]: split routes, a flow for a large one) and the whole
//! plan replayed swap by swap ([`realize`]); one unit of difference refuses
//! it.

use crate::realize::{apply_exit, realize, replayable, uses_pool};
use crate::scratch::{Scratch, EVAL};
use crate::size::maximise;
use crate::ArbError;
use alloy_primitives::{I256, U256};
use liq_router::exact::refine_exit;
use liq_router::{solve_pair, ExitQuote, GasTerms, PoolBook, PoolId, SolveBudget};
use liq_types::AssetId;

/// `1e18` wei.
const WEI_PER_ETH: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

/// The pool a victim moved and the direction that trades it back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub pool: PoolId,
    /// Coin sold into the anchor.
    pub i: u8,
    /// Coin bought from it.
    pub j: u8,
    /// A natural size for the trade, in coin `i` units: what the victim
    /// moved through the pool. The search brackets around it.
    pub scale: U256,
}

/// Which side of the anchor the flash funds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    StartIn,
    StartOut,
}

/// Everything a solve needs besides the book.
#[derive(Clone, Copy, Debug)]
pub struct ArbCtx<'a> {
    /// Base fee of the block the backrun targets (wei / gas).
    pub base_fee_wei: u128,
    /// Raw units of each asset per `1e18` wei, indexed by `AssetId.0`;
    /// zero is unpriced (never a start token).
    pub per_eth: &'a [U256],
    /// Assets a flash loan or flash swap supplies.
    pub flashable: &'a [AssetId],
    /// What profit is measured in on chain (the contract's WETH check). A
    /// start token other than this one pays one more swap to close into it.
    pub weth: AssetId,
    pub budget: SolveBudget,
    /// The arb contract's own gas around its swaps (call, flash, profit
    /// check, sweep).
    pub base_gas: u64,
    /// Exit solves the search may spend per orientation.
    pub max_evals: u16,
}

impl ArbCtx<'_> {
    fn per_eth(&self, a: AssetId) -> Option<U256> {
        self.per_eth
            .get(usize::from(a.0))
            .copied()
            .filter(|p| !p.is_zero())
    }

    fn gas_terms(&self, out: AssetId) -> Option<GasTerms> {
        Some(GasTerms {
            base_fee_wei: self.base_fee_wei,
            priority_fee_wei: 0,
            out_per_eth: self.per_eth(out)?,
        })
    }

    /// Gas of closing a profit in `start` into WETH: the cheapest live pool
    /// between them; `None` when there is none (the plan cannot settle).
    fn closer_gas(&self, book: &PoolBook, start: AssetId) -> Option<u64> {
        if start == self.weth {
            return Some(0);
        }
        book.legs(start, self.weth)
            .iter()
            .filter_map(|l| book.get(l.pool))
            .filter(|p| p.is_live())
            .map(|p| p.hop_gas)
            .min()
    }
}

/// `1e18`, for tests that price both test assets at one ETH.
#[cfg(test)]
pub(crate) fn tests_eth() -> U256 {
    WEI_PER_ETH
}

/// A sized, replayed backrun.
#[derive(Clone, Debug)]
pub struct Backrun {
    pub anchor: Anchor,
    pub orientation: Orientation,
    /// The flashed token, and what profit is kept in before closing.
    pub start: AssetId,
    /// Flashed.
    pub amount_in: U256,
    /// What the exit sells: the anchor's output (`StartIn`) or the flash
    /// itself (`StartOut`).
    pub exit_in: U256,
    pub exit: ExitQuote,
    /// Back in the start token, before repaying the flash.
    pub amount_out: U256,
    /// Total gas: contract, anchor, exit, closing swap.
    pub gas: u64,
    /// `amount_out − amount_in − gas`, start-token units.
    pub profit: U256,
    /// [`Self::profit`] in wei.
    pub net_wei: U256,
    /// Exit solves spent sizing.
    pub evals: u16,
}

/// The best backrun on `anchor` over both orientations, replayed; `None`
/// when neither pays after gas or neither start token can be flashed.
pub fn best_backrun(
    book: &mut PoolBook,
    scratch: &mut Scratch,
    ctx: &ArbCtx<'_>,
    anchor: Anchor,
) -> Result<Option<Backrun>, ArbError> {
    let (a, b) = {
        let p = book
            .get(anchor.pool)
            .ok_or(ArbError::Missing("anchor pool"))?;
        (
            *p.assets
                .get(usize::from(anchor.i))
                .ok_or(ArbError::Missing("anchor coin i"))?,
            *p.assets
                .get(usize::from(anchor.j))
                .ok_or(ArbError::Missing("anchor coin j"))?,
        )
    };
    let mut best: Option<Backrun> = None;
    for (o, start) in [(Orientation::StartIn, a), (Orientation::StartOut, b)] {
        if !ctx.flashable.contains(&start) {
            continue;
        }
        if let Some(r) = solve(book, scratch, ctx, anchor, o, a, b)? {
            if best.as_ref().is_none_or(|w| r.net_wei > w.net_wei) {
                best = Some(r);
            }
        }
    }
    Ok(best)
}

/// Profit after gas of one orientation at `x` flashed, start-token units,
/// with the exit it used. Quotes against the book as it stands; when the
/// exit routes through the anchor, takes the step in execution order on
/// scratch state instead.
#[allow(clippy::too_many_arguments)]
fn value_at(
    book: &mut PoolBook,
    scratch: &mut Scratch,
    ctx: &ArbCtx<'_>,
    anchor: Anchor,
    o: Orientation,
    a: AssetId,
    b: AssetId,
    x: U256,
    fixed_gas: u64,
) -> Option<(I256, ExitQuote, U256, U256)> {
    let gas_a = ctx.gas_terms(a)?;
    let hop = book.get(anchor.pool)?.hop_gas;
    match o {
        Orientation::StartIn => {
            let mid = book
                .get(anchor.pool)?
                .quote_exact_in(anchor.i, anchor.j, x)
                .ok()?;
            if mid.is_zero() {
                return None;
            }
            let mut q = solve_pair(book, b, a, mid, &gas_a, &ctx.budget).ok()?;
            if uses_pool(&q, anchor.pool) {
                // The exit trades the anchor too: solve it on the state our
                // anchor swap leaves.
                scratch.save(EVAL, book, anchor.pool);
                let swapped = book
                    .get_mut(anchor.pool)?
                    .apply_exact_in(anchor.i, anchor.j, x)
                    .ok();
                let after =
                    swapped.and_then(|_| solve_pair(book, b, a, mid, &gas_a, &ctx.budget).ok());
                scratch.restore(EVAL, book);
                q = after?;
            }
            let gas = fixed_gas.checked_add(hop)?.checked_add(q.hop_gas)?;
            let cost = gas_a.cost_in_out(gas).ok()?;
            let v = signed(q.amount_out)?
                .checked_sub(signed(x)?)?
                .checked_sub(signed(cost)?)?;
            let out = q.amount_out;
            Some((v, q, mid, out))
        }
        Orientation::StartOut => {
            let gas_b = ctx.gas_terms(b)?;
            let q = solve_pair(book, b, a, x, &gas_a, &ctx.budget).ok()?;
            let back = if uses_pool(&q, anchor.pool) {
                scratch.save(EVAL, book, anchor.pool);
                crate::realize::for_each_pool(&q, |id| scratch.save(EVAL, book, id));
                let r = apply_exit(book, &q, x).ok().and_then(|mid| {
                    book.get(anchor.pool)?
                        .quote_exact_in(anchor.i, anchor.j, mid)
                        .ok()
                });
                scratch.restore(EVAL, book);
                r?
            } else {
                book.get(anchor.pool)?
                    .quote_exact_in(anchor.i, anchor.j, q.amount_out)
                    .ok()?
            };
            let gas = fixed_gas.checked_add(hop)?.checked_add(q.hop_gas)?;
            let cost = gas_b.cost_in_out(gas).ok()?;
            let v = signed(back)?
                .checked_sub(signed(x)?)?
                .checked_sub(signed(cost)?)?;
            Some((v, q, x, back))
        }
    }
}

fn signed(v: U256) -> Option<I256> {
    I256::try_from(v).ok()
}

/// Size, refine and replay one orientation.
#[allow(clippy::too_many_arguments)]
fn solve(
    book: &mut PoolBook,
    scratch: &mut Scratch,
    ctx: &ArbCtx<'_>,
    anchor: Anchor,
    o: Orientation,
    a: AssetId,
    b: AssetId,
) -> Result<Option<Backrun>, ArbError> {
    let start = match o {
        Orientation::StartIn => a,
        Orientation::StartOut => b,
    };
    let Some(per) = ctx.per_eth(start) else {
        return Ok(None);
    };
    let Some(closer) = ctx.closer_gas(book, start) else {
        return Ok(None);
    };
    let fixed_gas = ctx.base_gas.checked_add(closer).ok_or(ArbError::Math)?;
    // The bracket's natural size, in the flashed token: the victim's own
    // amount for `StartIn`; for `StartOut`, what that amount buys from
    // the anchor now.
    let scale = match o {
        Orientation::StartIn => anchor.scale,
        Orientation::StartOut => book
            .get(anchor.pool)
            .and_then(|p| p.quote_exact_in(anchor.i, anchor.j, anchor.scale).ok())
            .unwrap_or_default(),
    };
    if scale.is_zero() {
        return Ok(None);
    }
    let found = maximise(scale, ctx.max_evals, |x| {
        value_at(book, scratch, ctx, anchor, o, a, b, x, fixed_gas).map(|r| r.0)
    });
    let Some(m) = found else {
        return Ok(None);
    };
    // The exit at the chosen amount, refined once (split routes; a flow for
    // a large sale). Refining needs the state the exit will see: for
    // `StartIn` that is after the anchor swap when the exit trades the
    // anchor, which `value_at` already solved on.
    let Some((_, q, exit_in, _)) = value_at(book, scratch, ctx, anchor, o, a, b, m.x, fixed_gas)
    else {
        return Ok(None);
    };
    let gas_a = ctx.gas_terms(a).ok_or(ArbError::Missing("exit price"))?;
    // Refining against the pre-anchor state is only exact while the exit
    // keeps off the anchor; a refined split onto it keeps the plain exit.
    let exit = if uses_pool(&q, anchor.pool) {
        q
    } else {
        let refined = refine_exit(book, b, a, &gas_a, &ctx.budget, q.clone())?;
        if uses_pool(&refined, anchor.pool) || !replayable(&refined) {
            q
        } else {
            refined
        }
    };
    let hop = book
        .get(anchor.pool)
        .ok_or(ArbError::Missing("anchor pool"))?
        .hop_gas;
    let gas = fixed_gas
        .checked_add(hop)
        .and_then(|g| g.checked_add(exit.hop_gas))
        .ok_or(ArbError::Math)?;
    // Claimed output on the refined exit (replay checks it below).
    let amount_out = match o {
        Orientation::StartIn => exit.amount_out,
        Orientation::StartOut => {
            if uses_pool(&exit, anchor.pool) {
                scratch.save(EVAL, book, anchor.pool);
                crate::realize::for_each_pool(&exit, |id| scratch.save(EVAL, book, id));
                let r = apply_exit(book, &exit, m.x).and_then(|mid| {
                    Ok(book
                        .get(anchor.pool)
                        .ok_or(ArbError::Missing("anchor pool"))?
                        .quote_exact_in(anchor.i, anchor.j, mid)?)
                });
                scratch.restore(EVAL, book);
                r?
            } else {
                book.get(anchor.pool)
                    .ok_or(ArbError::Missing("anchor pool"))?
                    .quote_exact_in(anchor.i, anchor.j, exit.amount_out)?
            }
        }
    };
    let cost = ctx
        .gas_terms(start)
        .ok_or(ArbError::Missing("start price"))?
        .cost_in_out(gas)?;
    let Some(profit) = amount_out
        .checked_sub(m.x)
        .and_then(|g| g.checked_sub(cost))
        .filter(|p| !p.is_zero())
    else {
        return Ok(None);
    };
    let net_wei = profit
        .checked_mul(WEI_PER_ETH)
        .ok_or(ArbError::Math)?
        .checked_div(per)
        .ok_or(ArbError::Math)?;
    let plan = Backrun {
        anchor,
        orientation: o,
        start,
        amount_in: m.x,
        exit_in,
        exit,
        amount_out,
        gas,
        profit,
        net_wei,
        evals: m.evals,
    };
    realize(book, scratch, &plan)?;
    Ok(Some(plan))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;
    use crate::testbook::*;
    use std::collections::HashMap;

    /// StartIn: flash A, buy B in the anchor, sell it in pool 1. Output
    /// equals the V2 formula on both pools at the plan's amount; the amount
    /// is the scan's argmax of the same cycle to within the search's
    /// tolerance; the books are left untouched.
    #[test]
    fn start_in_matches_the_v2_oracle_and_the_scan() {
        let (mut b, p0, p1) = book();
        let before = (reserves(&b, p0), reserves(&b, p1));
        let mut s = Scratch::new(&b);
        let per = [WEI_PER_ETH, WEI_PER_ETH];
        let flash = [A];
        let anchor = Anchor {
            pool: p0,
            i: 0,
            j: 1,
            scale: U256::from(E21 / 100),
        };
        let r = best_backrun(&mut b, &mut s, &ctx(&per, &flash), anchor)
            .unwrap()
            .unwrap();
        assert_eq!(r.orientation, Orientation::StartIn);
        assert_eq!(
            (reserves(&b, p0), reserves(&b, p1)),
            before,
            "book restored"
        );

        let ((a0, b0), (a1, b1)) = before;
        let mid = v2_out(r.amount_in, a0, b0);
        assert_eq!(r.exit_in, mid);
        let out = v2_out(mid, b1, a1);
        assert_eq!(r.amount_out, out, "exit output vs V2 oracle");

        // Gas: contract + anchor + one exit pool, at 1 gwei, WETH start.
        assert_eq!(r.gas, BASE + HOP + HOP);
        let cost = U256::from(r.gas) * U256::from(1_000_000_000u64);
        assert_eq!(r.profit, out - r.amount_in - cost);
        assert_eq!(r.net_wei, r.profit);

        // Scan oracle over the cycle's gross (gas is constant here).
        let mut best = (U256::ZERO, U256::ZERO);
        let step = U256::from(E21 / 100_000);
        let mut x = step;
        while x < U256::from(E21 / 10) {
            let g = v2_out(v2_out(x, a0, b0), b1, a1).saturating_sub(x);
            if g > best.1 {
                best = (x, g);
            }
            x += step;
        }
        let diff = if r.amount_in > best.0 {
            r.amount_in - best.0
        } else {
            best.0 - r.amount_in
        };
        assert!(
            diff <= best.0 / U256::from(500u32),
            "x {} vs scan {}",
            r.amount_in,
            best.0
        );
        assert!(
            r.profit + cost + step >= best.1,
            "gross {} vs scan {}",
            r.profit + cost,
            best.1
        );
    }

    /// StartOut: only B is flashable. Flash B, sell it in pool 1 for A, buy
    /// B back in the anchor. Checked the same way, in B units.
    #[test]
    fn start_out_matches_the_v2_oracle() {
        let (mut b, p0, p1) = book();
        let mut s = Scratch::new(&b);
        let per = [WEI_PER_ETH, WEI_PER_ETH];
        let flash = [B];
        let anchor = Anchor {
            pool: p0,
            i: 0,
            j: 1,
            scale: U256::from(E21 / 100),
        };
        let r = best_backrun(&mut b, &mut s, &ctx(&per, &flash), anchor)
            .unwrap()
            .unwrap();
        assert_eq!(r.orientation, Orientation::StartOut);
        assert_eq!(r.start, B);
        let ((a0, b0), (a1, b1)) = (reserves(&b, p0), reserves(&b, p1));
        let mid = v2_out(r.amount_in, b1, a1);
        assert_eq!(
            r.amount_out,
            v2_out(mid, a0, b0),
            "anchor buy-back vs V2 oracle"
        );
        // A B start closes its profit into WETH: one more hop.
        assert_eq!(r.gas, BASE + HOP + HOP + HOP);
    }

    /// No dislocation (pools at one price): nothing pays, in either
    /// orientation.
    #[test]
    fn level_pools_give_nothing() {
        let mut assets = HashMap::new();
        assets.insert(tok(0), A);
        assets.insert(tok(1), B);
        let mut b = PoolBook::new(assets, None, 0);
        let p0 = b.add(v2(1, E21, E21)).unwrap();
        b.add(v2(2, E21, E21)).unwrap();
        let mut s = Scratch::new(&b);
        let per = [WEI_PER_ETH, WEI_PER_ETH];
        let flash = [A, B];
        let anchor = Anchor {
            pool: p0,
            i: 0,
            j: 1,
            scale: U256::from(E21 / 100),
        };
        assert!(best_backrun(&mut b, &mut s, &ctx(&per, &flash), anchor)
            .unwrap()
            .is_none());
    }

    /// The replay refuses a plan whose claim is off by one unit.
    #[test]
    fn realize_refuses_a_claim_off_by_one() {
        let (mut b, p0, _) = book();
        let mut s = Scratch::new(&b);
        let per = [WEI_PER_ETH, WEI_PER_ETH];
        let flash = [A];
        let anchor = Anchor {
            pool: p0,
            i: 0,
            j: 1,
            scale: U256::from(E21 / 100),
        };
        let mut r = best_backrun(&mut b, &mut s, &ctx(&per, &flash), anchor)
            .unwrap()
            .unwrap();
        r.amount_out += U256::from(1u8);
        assert!(matches!(
            realize(&mut b, &mut s, &r),
            Err(ArbError::Mismatch { .. })
        ));
    }
}
