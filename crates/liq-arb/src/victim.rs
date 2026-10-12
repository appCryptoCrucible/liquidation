//! A victim's logs on scratch state, and the anchors they leave.
//!
//! A pending transaction is known by its logs: a MEV-Share hint that shares
//! them, or a simulation of a public-mempool transaction. Folded through the
//! book's own [`PoolBook::apply_log`] they give the exact state after it:
//! a V2 `Sync` carries the reserves, a V3/V4 `Swap` the price, liquidity and
//! tick, a Balancer `Swap` the balances' change. A Curve pool is not folded
//! from logs (the book re-reads it), so a Curve `TokenExchange` is applied
//! through the pool's own model instead, and only when the model pays
//! exactly what the event says it paid: otherwise that pool's state is not
//! known, and it is marked stale on the scratch state so no exit uses it.
//!
//! Every swap event becomes an [`Anchor`]: the pool, the direction that
//! undoes the victim's trade, and the victim's output as the size to
//! bracket around.

use crate::anchor::Anchor;
use crate::scratch::{Scratch, VICTIM};
use alloy_primitives::{b256, Address, B256, I256, U256};
use liq_protocol::DecodedLog;
use liq_router::{PoolBook, PoolId};
use smallvec::SmallVec;

const V2_SWAP: B256 = b256!("d78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822");
const V3_SWAP: B256 = b256!("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");
const PANCAKE_V3_SWAP: B256 =
    b256!("19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83");
const V4_SWAP: B256 = b256!("40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f");
/// `TokenExchange(address,int128,uint256,int128,uint256)` — StableSwap.
const CURVE_EXCHANGE: B256 =
    b256!("8b3e96f2b889fa771c53c981b40daf005f63f637f1869f707052d15a3dd97140");
/// `TokenExchange(address,uint256,uint256,uint256,uint256)` — crypto.
const CRYPTO_EXCHANGE: B256 =
    b256!("b2e76ae99761dc136e598d4a629bb347eccb9532a5f8bbd72e18467c3c34cc98");
/// `TokenExchange(address,uint256,uint256,uint256,uint256,uint256,uint256)`
/// — newer crypto pools (fee, packed price scale appended).
const CRYPTO_EXCHANGE_FEE: B256 =
    b256!("143f1f8e861fbdeddd5b46e844b7d3ac7b86a122f36e8c463859ee6811b1f29c");
/// `Swap(bytes32,address,address,uint256,uint256)` — Balancer V2 Vault.
const BALANCER_SWAP: B256 =
    b256!("2170c741c41531aec20e7c107c24eecfdd15e69c9bb0a8dd37b1840b9e0b207b");

/// Anchors one victim can leave; more pools than this are not backrun.
pub const MAX_ANCHORS: usize = 8;

/// One log, borrowed: what [`apply_victim`] reads.
#[derive(Clone, Copy, Debug)]
pub struct LogRef<'a> {
    pub address: Address,
    pub topics: &'a [B256],
    pub data: &'a [u8],
}

fn word(data: &[u8], k: usize) -> Option<U256> {
    let s = k.checked_mul(32)?;
    data.get(s..s.checked_add(32)?).map(U256::from_be_slice)
}

fn signed(data: &[u8], k: usize) -> Option<I256> {
    word(data, k).map(I256::from_raw)
}

/// Why a victim was not backrun.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// It swapped through no pool in the book.
    NoPool,
    /// More than [`MAX_ANCHORS`] pools.
    TooManyPools,
}

/// Fold `logs` (one victim, in emission order) into `book`, saving every
/// pool they touch at [`VICTIM`] first, and return the anchors its swaps
/// leave. On `Err` the book is already restored. The caller restores
/// [`VICTIM`] after solving.
pub fn apply_victim(
    book: &mut PoolBook,
    scratch: &mut Scratch,
    logs: &[LogRef<'_>],
    block: u64,
    timestamp: u64,
) -> Result<SmallVec<[Anchor; MAX_ANCHORS]>, Skip> {
    let mut anchors: SmallVec<[Anchor; MAX_ANCHORS]> = SmallVec::new();
    let mut touched: SmallVec<[PoolId; 4]> = SmallVec::new();
    for l in logs {
        let Some(&t0) = l.topics.first() else {
            continue;
        };
        let d = DecodedLog {
            address: l.address,
            topics: l.topics,
            data: l.data,
            block,
            timestamp,
        };
        touched.clear();
        book.pools_for_log(&d, &mut touched);
        for &id in &touched {
            scratch.save(VICTIM, book, id);
        }
        let Some(&pool) = touched.first() else {
            continue;
        };
        let anchor = if t0 == CURVE_EXCHANGE || t0 == CRYPTO_EXCHANGE || t0 == CRYPTO_EXCHANGE_FEE {
            // Not folded from logs: apply the exchange through the model and
            // hold it to what the event says it paid. When it does not pay
            // that, the pool's state is unknown: stale on this scratch
            // state (restored with the victim), so no exit reads it.
            let a = curve(book, pool, l.data);
            if a.is_none() {
                book.mark_stale(pool, block);
            }
            a
        } else {
            book.apply_log(&d);
            swap_anchor(book, pool, t0, l)
        };
        if let Some(a) = anchor {
            // Several swaps through one pool the same way are one anchor.
            if let Some(same) = anchors
                .iter_mut()
                .find(|x| x.pool == a.pool && x.i == a.i && x.j == a.j)
            {
                same.scale = same.scale.saturating_add(a.scale);
            } else if anchors.len() == MAX_ANCHORS {
                scratch.restore(VICTIM, book);
                return Err(Skip::TooManyPools);
            } else {
                anchors.push(a);
            }
        }
    }
    if anchors.is_empty() {
        scratch.restore(VICTIM, book);
        return Err(Skip::NoPool);
    }
    Ok(anchors)
}

/// A Curve exchange applied through the pool's model; `None` unless the
/// model pays exactly `tokens_bought`.
fn curve(book: &mut PoolBook, pool: PoolId, data: &[u8]) -> Option<Anchor> {
    let i = u8::try_from(word(data, 0)?).ok()?;
    let sold = word(data, 1)?;
    let j = u8::try_from(word(data, 2)?).ok()?;
    let bought = word(data, 3)?;
    let got = book.get_mut(pool)?.apply_exact_in(i, j, sold).ok()?;
    (got == bought).then_some(Anchor {
        pool,
        i: j,
        j: i,
        scale: bought,
    })
}

/// The anchor a folded swap event leaves: the reverse of the victim's
/// direction, sized by what it bought.
fn swap_anchor(book: &PoolBook, pool: PoolId, t0: B256, l: &LogRef<'_>) -> Option<Anchor> {
    // (coin the victim sold, coin it bought, amount bought)
    let (sold, bought, out) = if t0 == V2_SWAP {
        let (a0in, a1in, a0out, a1out) = (
            word(l.data, 0)?,
            word(l.data, 1)?,
            word(l.data, 2)?,
            word(l.data, 3)?,
        );
        if !a0in.is_zero() && a1in.is_zero() {
            (0u8, 1u8, a1out)
        } else if !a1in.is_zero() && a0in.is_zero() {
            (1, 0, a0out)
        } else {
            return None;
        }
    } else if t0 == V3_SWAP || t0 == PANCAKE_V3_SWAP {
        // Pool deltas: positive went in.
        let (a0, a1) = (signed(l.data, 0)?, signed(l.data, 1)?);
        if a0.is_positive() && a1.is_negative() {
            (0, 1, a1.unsigned_abs())
        } else if a1.is_positive() && a0.is_negative() {
            (1, 0, a0.unsigned_abs())
        } else {
            return None;
        }
    } else if t0 == V4_SWAP {
        // The swapper's deltas: negative is what it paid.
        let (a0, a1) = (signed(l.data, 0)?, signed(l.data, 1)?);
        if a0.is_negative() && a1.is_positive() {
            (0, 1, a1.unsigned_abs())
        } else if a1.is_negative() && a0.is_positive() {
            (1, 0, a0.unsigned_abs())
        } else {
            return None;
        }
    } else if t0 == BALANCER_SWAP {
        let p = book.get(pool)?;
        let tin = Address::from_word(*l.topics.get(2)?);
        let tout = Address::from_word(*l.topics.get(3)?);
        let ci = p.tokens.iter().position(|t| *t == tin)?;
        let co = p.tokens.iter().position(|t| *t == tout)?;
        (
            u8::try_from(ci).ok()?,
            u8::try_from(co).ok()?,
            word(l.data, 1)?,
        )
    } else {
        return None;
    };
    (!out.is_zero()).then_some(Anchor {
        pool,
        i: bought,
        j: sold,
        scale: out,
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::testbook::*;
    use crate::{best_backrun, Orientation};
    use alloy_primitives::b256;
    use std::collections::HashMap;

    const SYNC: B256 = b256!("1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1");

    fn level_book() -> PoolBook {
        let mut assets = HashMap::new();
        assets.insert(tok(0), A);
        assets.insert(tok(1), B);
        let mut b = PoolBook::new(assets, None, 0);
        b.add(v2(1, E21, E21)).unwrap();
        b.add(v2(2, E21, E21)).unwrap();
        b
    }

    fn words(ws: &[U256]) -> Vec<u8> {
        ws.iter().flat_map(|w| w.to_be_bytes::<32>()).collect()
    }

    /// A victim sells 50 A into pool 0 (Sync then Swap, as a V2 pair emits
    /// them). Folded: pool 0's reserves are the Sync's. The anchor is pool 0
    /// the other way (sell B back), sized by the B the victim bought. The
    /// backrun pays, and restoring the victim level gives the book back.
    #[test]
    fn v2_victim_leaves_the_reverse_anchor() {
        let mut b = level_book();
        let before: Vec<_> = b.pools().iter().map(|p| p.state.clone()).collect();
        let mut s = Scratch::new(&b);
        let sold = U256::from(50u8) * U256::from(10u64).pow(U256::from(18u8));
        let e21 = U256::from(E21);
        let bought = v2_out(sold, e21, e21);
        let sync = words(&[e21 + sold, e21 - bought]);
        let swap = words(&[sold, U256::ZERO, U256::ZERO, bought]);
        let pool = Address::repeat_byte(1);
        let t_sync = [SYNC];
        let t_swap = [V2_SWAP, B256::ZERO, B256::ZERO];
        let logs = [
            LogRef {
                address: pool,
                topics: &t_sync,
                data: &sync,
            },
            LogRef {
                address: pool,
                topics: &t_swap,
                data: &swap,
            },
        ];
        let anchors = apply_victim(&mut b, &mut s, &logs, 1, 1).unwrap();
        assert_eq!(
            reserves(&b, PoolId(0)),
            (e21 + sold, e21 - bought),
            "folded to the Sync"
        );
        assert_eq!(anchors.len(), 1);
        let a = anchors[0];
        assert_eq!((a.pool, a.i, a.j, a.scale), (PoolId(0), 1, 0, bought));

        let per = [crate::anchor::tests_eth(), crate::anchor::tests_eth()];
        let flash = [A, B];
        let r = best_backrun(&mut b, &mut s, &ctx(&per, &flash), a)
            .unwrap()
            .unwrap();
        assert!(!r.profit.is_zero());
        assert!(matches!(
            r.orientation,
            Orientation::StartIn | Orientation::StartOut
        ));

        s.restore(VICTIM, &mut b);
        let after: Vec<_> = b.pools().iter().map(|p| p.state.clone()).collect();
        assert_eq!(after, before, "victim level restored");
    }

    /// Logs of pools not in the book leave nothing and change nothing.
    #[test]
    fn unknown_pool_is_skipped() {
        let mut b = level_book();
        let mut s = Scratch::new(&b);
        let swap = words(&[U256::from(1u8), U256::ZERO, U256::ZERO, U256::from(1u8)]);
        let t = [V2_SWAP];
        let logs = [LogRef {
            address: Address::repeat_byte(9),
            topics: &t,
            data: &swap,
        }];
        assert_eq!(
            apply_victim(&mut b, &mut s, &logs, 1, 1).unwrap_err(),
            Skip::NoPool
        );
    }
}
