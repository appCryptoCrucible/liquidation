//! A plan replayed swap by swap, in the order the contract runs it, on
//! scratch state: what it really returns. The solve quotes the anchor and
//! the exit against the state each saw; this applies every swap to the
//! state the previous one left. A plan is sent only when the two agree to
//! the unit ([`crate::ArbError::Mismatch`] otherwise).

use crate::anchor::{Backrun, Orientation};
use crate::scratch::{Scratch, EVAL};
use crate::ArbError;
use alloy_primitives::U256;
use liq_router::{ExitQuote, PoolBook, PoolId};

/// Call `f` with every pool `q` swaps through.
pub fn for_each_pool(q: &ExitQuote, mut f: impl FnMut(PoolId)) {
    if let Some(h) = &q.hub {
        for a in &h.allocs {
            f(a.leg.pool);
        }
    }
    for a in &q.allocs {
        f(a.leg.pool);
    }
    if let Some(c) = &q.chain {
        for l in &c.hops {
            f(l.pool);
        }
        for w in &c.with {
            for l in &w.hops {
                f(l.pool);
            }
        }
    }
}

/// Whether `q` swaps through `pool`.
#[must_use]
pub fn uses_pool(q: &ExitQuote, pool: PoolId) -> bool {
    let mut hit = false;
    for_each_pool(q, |p| hit |= p == pool);
    hit
}

/// Whether [`apply_exit`] follows `q`: no unwrap, and a chain only when it
/// is the whole exit. A refined exit split across chains (whose parts are
/// sized as exact outputs for a repay) is not, and the plain exit stands.
#[must_use]
pub fn replayable(q: &ExitQuote) -> bool {
    q.unwrap.is_none()
        && q.chain
            .as_ref()
            .is_none_or(|c| c.with.is_empty() && q.allocs.is_empty() && q.hub.is_none())
}

/// Apply `q`'s swaps to `book` in execution order — the hub's pools, then
/// the direct pools; or a lone chain hop by hop on the whole `total` — and
/// return what they pay. Mutates `book`: callers save first.
pub fn apply_exit(book: &mut PoolBook, q: &ExitQuote, total: U256) -> Result<U256, ArbError> {
    if !replayable(q) {
        return Err(ArbError::Unreplayable);
    }
    if let Some(c) = &q.chain {
        let mut amt = total;
        for l in &c.hops {
            amt = book
                .get_mut(l.pool)
                .ok_or(ArbError::Missing("chain pool"))?
                .apply_exact_in(l.i, l.j, amt)?;
        }
        return Ok(amt);
    }
    if let Some(h) = &q.hub {
        let mut hub_out = U256::ZERO;
        for a in h.allocs.iter().filter(|a| !a.amount_in.is_zero()) {
            let o = book
                .get_mut(a.leg.pool)
                .ok_or(ArbError::Missing("hub pool"))?
                .apply_exact_in(a.leg.i, a.leg.j, a.amount_in)?;
            hub_out = hub_out.checked_add(o).ok_or(ArbError::Math)?;
        }
        if hub_out != h.amount_out {
            return Err(ArbError::Mismatch {
                claimed: h.amount_out,
                realized: hub_out,
            });
        }
    }
    let mut out = U256::ZERO;
    for a in q.allocs.iter().filter(|a| !a.amount_in.is_zero()) {
        let o = book
            .get_mut(a.leg.pool)
            .ok_or(ArbError::Missing("exit pool"))?
            .apply_exact_in(a.leg.i, a.leg.j, a.amount_in)?;
        out = out.checked_add(o).ok_or(ArbError::Math)?;
    }
    Ok(out)
}

/// Replay `plan` on `book` (restored after) and return what it really
/// returns in its start token; [`ArbError::Mismatch`] when that is not
/// exactly what it claimed.
pub fn realize(
    book: &mut PoolBook,
    scratch: &mut Scratch,
    plan: &Backrun,
) -> Result<U256, ArbError> {
    let p = plan.anchor.pool;
    scratch.save(EVAL, book, p);
    for_each_pool(&plan.exit, |id| scratch.save(EVAL, book, id));
    let r = replay(book, plan);
    scratch.restore(EVAL, book);
    let realized = r?;
    if realized != plan.amount_out {
        return Err(ArbError::Mismatch {
            claimed: plan.amount_out,
            realized,
        });
    }
    Ok(realized)
}

pub(crate) fn replay(book: &mut PoolBook, plan: &Backrun) -> Result<U256, ArbError> {
    let a = plan.anchor;
    let anchor = |book: &mut PoolBook, amt: U256| -> Result<U256, ArbError> {
        Ok(book
            .get_mut(a.pool)
            .ok_or(ArbError::Missing("anchor pool"))?
            .apply_exact_in(a.i, a.j, amt)?)
    };
    match plan.orientation {
        Orientation::StartIn => {
            let mid = anchor(book, plan.amount_in)?;
            if mid != plan.exit_in {
                return Err(ArbError::Mismatch {
                    claimed: plan.exit_in,
                    realized: mid,
                });
            }
            apply_exit(book, &plan.exit, mid)
        }
        Orientation::StartOut => {
            let mid = apply_exit(book, &plan.exit, plan.amount_in)?;
            anchor(book, mid)
        }
    }
}
