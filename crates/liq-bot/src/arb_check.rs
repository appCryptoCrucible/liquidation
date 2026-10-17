//! What a simulation of our own backrun is held to, and when a
//! disagreement is the model's fault.
//!
//! Every public-mempool and top-of-block backrun is simulated on the node's
//! state before it is signed. The simulation must gain what the plan
//! claims ([`claim_holds`]): to the wei for a flash-loan plan, at least
//! that for a flash-swap plan (the V3 anchor's exact-output charge can be a
//! unit under the modelled input). A plan that does not is never sent.
//!
//! A pool is quarantined only on a confirmed model error: the first hop
//! whose swap event disagrees with the model ([`first_disagreement`]),
//! with the same amount in, on a pool whose state just before our
//! transaction — read through the pool's own views on the simulated state
//! ([`view_reads`], [`chain_view`]) — is the state the model solved on. A
//! pool that held a different state there is a state difference, which
//! never counts against the pool.

use alloy_primitives::{Address, I256, U256};
use liq_arb::{swap_seen, Hop, LogRef, Opportunity, PoolView, SwapSeen};
use liq_router::{PoolBook, PoolId};
use liq_types::FlashProvider;
use smallvec::SmallVec;

/// Whether a simulation that gained `gained` (WETH, executor and sink)
/// matches what `o` claims.
#[must_use]
pub fn claim_holds(o: &Opportunity, gained: I256) -> bool {
    let Ok(g) = U256::try_from(gained) else {
        return false;
    };
    if o.flash == FlashProvider::UniV3Swap {
        g >= o.kept_wei
    } else {
        g == o.kept_wei
    }
}

/// Where a simulation first disagrees with the plan, hop by hop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disagreement {
    /// Every hop's event matched (the claim differed elsewhere: the bid or
    /// the gas the contract charged).
    None,
    /// A hop left no swap event this decoder reads (a venue without one):
    /// nothing after it can be pinned on a pool.
    Unverified(Hop),
    /// A hop's event shows a different amount in: the difference started
    /// upstream of it.
    Upstream { hop: Hop, seen: SwapSeen },
    /// The same amount went in and a different amount came out: this pool,
    /// or its state.
    Pool { hop: Hop, seen: SwapSeen },
}

/// The first of `o`'s hops that the swap events in `logs` (our call's, in
/// emission order) disagree with. Each hop is matched to the first unused
/// event of the same pool and direction: a flash swap's anchor event is
/// emitted after its callback, so emission order is not hop order.
#[must_use]
pub fn first_disagreement(
    book: &PoolBook,
    o: &Opportunity,
    logs: &[alloy_primitives::Log],
    block: u64,
    timestamp: u64,
) -> Disagreement {
    let mut seen: SmallVec<[(SwapSeen, bool); 16]> = SmallVec::new();
    for l in logs {
        let r = LogRef {
            address: l.address,
            topics: l.data.topics(),
            data: &l.data.data,
        };
        if let Some(s) = swap_seen(book, &r, block, timestamp) {
            seen.push((s, false));
        }
    }
    let a = o.backrun.anchor;
    for &hop in &o.hops {
        let Some((s, used)) = seen
            .iter_mut()
            .find(|(s, used)| !*used && s.pool == hop.pool && s.sold == hop.i && s.bought == hop.j)
        else {
            return Disagreement::Unverified(hop);
        };
        *used = true;
        let s = *s;
        let flash_anchor = o.flash == FlashProvider::UniV3Swap
            && hop.pool == a.pool
            && hop.i == a.i
            && hop.j == a.j;
        let matched = if flash_anchor {
            s.amount_out == hop.amount_out && s.amount_in <= hop.amount_in
        } else {
            s.amount_out == hop.amount_out && s.amount_in == hop.amount_in
        };
        if matched {
            continue;
        }
        if s.amount_in != hop.amount_in && !flash_anchor {
            return Disagreement::Upstream { hop, seen: s };
        }
        return Disagreement::Pool { hop, seen: s };
    }
    Disagreement::None
}

/// `getReserves()`.
const GET_RESERVES: [u8; 4] = [0x09, 0x02, 0xf1, 0xac];
/// `slot0()`.
const SLOT0: [u8; 4] = [0x38, 0x50, 0xc7, 0xbd];
/// `liquidity()`.
const LIQUIDITY: [u8; 4] = [0x1a, 0x68, 0x65, 0x02];

/// View calls, `(callee, calldata)`.
pub type Reads = Vec<(Address, Vec<u8>)>;

/// The view calls that read `pool`'s [`PoolView`] back from its contract,
/// and the state the plan was solved on; `None` when the plan did not
/// record one (a venue no pool contract's view returns).
#[must_use]
pub fn view_reads(book: &PoolBook, o: &Opportunity, pool: PoolId) -> Option<(PoolView, Reads)> {
    let (_, solved) = o.pre.iter().find(|(p, _)| *p == pool)?;
    let address = book.get(pool)?.address;
    let reads = match solved {
        PoolView::V2 { .. } => vec![(address, GET_RESERVES.to_vec())],
        PoolView::V3 { .. } => vec![(address, SLOT0.to_vec()), (address, LIQUIDITY.to_vec())],
    };
    Some((*solved, reads))
}

fn word(b: &[u8], k: usize) -> Option<U256> {
    let s = k.checked_mul(32)?;
    b.get(s..s.checked_add(32)?).map(U256::from_be_slice)
}

/// The [`PoolView`] the view calls of [`view_reads`] returned, in the shape
/// of `like`; `None` when a call reverted or returned short.
#[must_use]
pub fn chain_view(like: &PoolView, outs: &[Option<alloy_primitives::Bytes>]) -> Option<PoolView> {
    match like {
        PoolView::V2 { .. } => {
            let r = outs.first()?.as_ref()?;
            Some(PoolView::V2 {
                reserve0: word(r, 0)?,
                reserve1: word(r, 1)?,
            })
        }
        PoolView::V3 { .. } => {
            let s = outs.first()?.as_ref()?;
            let l = outs.get(1)?.as_ref()?;
            // `int24 tick`, sign-extended into its word.
            let tick = i32::try_from(I256::from_raw(word(s, 1)?)).ok()?;
            Some(PoolView::V3 {
                sqrt_price_x96: word(s, 0)?,
                tick,
                liquidity: u128::try_from(word(l, 0)?).ok()?,
            })
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::panic
)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use liq_router::PoolState;

    fn w(x: U256) -> [u8; 32] {
        x.to_be_bytes::<32>()
    }

    /// `getReserves()` returns `(uint112, uint112, uint32)`; `slot0()`
    /// returns the price then the tick as a sign-extended `int24`, and
    /// `liquidity()` one word — read back field for field, a negative tick
    /// included.
    #[test]
    fn chain_view_decodes_the_pool_views() {
        let mut r = Vec::new();
        r.extend(w(U256::from(5u8)));
        r.extend(w(U256::from(7u8)));
        r.extend(w(U256::from(1_700_000_000u64)));
        let v2 = PoolView::V2 {
            reserve0: U256::ZERO,
            reserve1: U256::ZERO,
        };
        assert_eq!(
            chain_view(&v2, &[Some(Bytes::from(r))]),
            Some(PoolView::V2 {
                reserve0: U256::from(5u8),
                reserve1: U256::from(7u8),
            })
        );
        let mut s = Vec::new();
        s.extend(w(U256::from(79_228_162_514_264_337_593_543_950_336u128)));
        s.extend(w(I256::try_from(-200_000i64).unwrap().into_raw()));
        for _ in 0..5 {
            s.extend(w(U256::ZERO));
        }
        let l = w(U256::from(123_456u64)).to_vec();
        let v3 = PoolView::V3 {
            sqrt_price_x96: U256::ZERO,
            tick: 0,
            liquidity: 0,
        };
        assert_eq!(
            chain_view(&v3, &[Some(Bytes::from(s)), Some(Bytes::from(l))]),
            Some(PoolView::V3 {
                sqrt_price_x96: U256::from(79_228_162_514_264_337_593_543_950_336u128),
                tick: -200_000,
                liquidity: 123_456,
            })
        );
        assert_eq!(chain_view(&v3, &[None, None]), None, "a reverted view");
    }

    const SYNC: alloy_primitives::B256 =
        alloy_primitives::b256!("1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1");
    const V2_SWAP: alloy_primitives::B256 =
        alloy_primitives::b256!("d78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822");
    const E21: u128 = 1_000_000_000_000_000_000_000;

    fn tok(n: u8) -> Address {
        Address::repeat_byte(n.wrapping_add(0x10))
    }

    fn words(ws: &[U256]) -> Vec<u8> {
        ws.iter().flat_map(|w| w.to_be_bytes::<32>()).collect()
    }

    /// A V2 `Swap` event as pool `at` emits it for a swap of `i` into `j`.
    fn v2_swap(at: Address, i: u8, amount_in: U256, amount_out: U256) -> alloy_primitives::Log {
        let z = U256::ZERO;
        let data = if i == 0 {
            words(&[amount_in, z, z, amount_out])
        } else {
            words(&[z, amount_in, amount_out, z])
        };
        alloy_primitives::Log::new_unchecked(
            at,
            vec![
                V2_SWAP,
                alloy_primitives::B256::ZERO,
                alloy_primitives::B256::ZERO,
            ],
            Bytes::from(data),
        )
    }

    /// A plan from the engine (two V2 pools, a victim selling into the
    /// first), and its book.
    fn planned() -> (liq_arb::Engine, Opportunity) {
        use liq_router::{Pool, PoolState, V2State};
        use liq_types::AssetId;
        let pool = |n: u8, r0: u128, r1: u128| Pool {
            address: Address::repeat_byte(n),
            assets: smallvec::SmallVec::from_slice(&[AssetId(0), AssetId(1)]),
            tokens: smallvec::SmallVec::from_slice(&[tok(0), tok(1)]),
            hop_gas: 90_000,
            state: PoolState::V2(V2State {
                reserve0: U256::from(r0),
                reserve1: U256::from(r1),
                factory: 0,
            }),
        };
        let mut assets = std::collections::HashMap::new();
        assets.insert(tok(0), AssetId(0));
        assets.insert(tok(1), AssetId(1));
        let mut book = PoolBook::new(assets, None, 0);
        book.add(pool(1, E21, E21)).unwrap();
        book.add(pool(2, E21, E21)).unwrap();
        let cfg = liq_arb::EngineCfg {
            weth: tok(0),
            weth_id: AssetId(0),
            flashable: vec![AssetId(0)],
            budget: liq_router::SolveBudget::default(),
            base_gas: 60_000,
            max_evals: 40,
            bid_bps: 500,
            min_net_wei: U256::from(1u8),
            min_profit_wei: 1,
            sweep: false,
            flash_swap: true,
            top_of_block: false,
        };
        let mut e = liq_arb::Engine::new(&book, cfg);
        let eth = U256::from(1_000_000_000_000_000_000u64);
        let src = [liq_arb::FlashSource {
            asset: AssetId(0),
            provider: FlashProvider::UniV4,
            source: Address::repeat_byte(0x44),
            available: U256::MAX,
        }];
        let target = liq_arb::Target {
            number: 2,
            timestamp: 24,
            base_fee_wei: 1_000_000_000,
        };
        e.on_block(&book, target, &[eth, eth], &src);
        let e21 = U256::from(E21);
        let sold = U256::from(50u8) * eth;
        let a = sold * U256::from(997u16);
        let bought = a * e21 / (e21 * U256::from(1000u16) + a);
        let sync = words(&[e21 + sold, e21 - bought]);
        let swap = words(&[sold, U256::ZERO, U256::ZERO, bought]);
        let (ts, tw) = (
            [SYNC],
            [
                V2_SWAP,
                alloy_primitives::B256::ZERO,
                alloy_primitives::B256::ZERO,
            ],
        );
        let at = Address::repeat_byte(1);
        let logs = [
            LogRef {
                address: at,
                topics: &ts,
                data: &sync,
            },
            LogRef {
                address: at,
                topics: &tw,
                data: &swap,
            },
        ];
        let o = e.on_victim(&logs, |_| 500).unwrap().unwrap();
        assert!(o.hops.len() >= 2, "a cycle through both pools");
        (e, o)
    }

    /// Events that report every hop as the model has it disagree nowhere;
    /// one more unit out of a hop pins that pool; a different amount in is
    /// upstream of the hop; a hop with no event leaves the rest unverified.
    /// The plan's recorded states are the book's, readable as V2 reserves.
    #[test]
    fn first_disagreement_pins_the_hop() {
        let (e, o) = planned();
        let book = e.book();
        let at = |h: &Hop| book.get(h.pool).unwrap().address;
        let exact: Vec<_> = o
            .hops
            .iter()
            .map(|h| v2_swap(at(h), h.i, h.amount_in, h.amount_out))
            .collect();
        assert_eq!(
            first_disagreement(book, &o, &exact, 1, 24),
            Disagreement::None
        );

        let last = *o.hops.last().unwrap();
        let mut off = exact.clone();
        *off.last_mut().unwrap() = v2_swap(
            at(&last),
            last.i,
            last.amount_in,
            last.amount_out + U256::from(1u8),
        );
        assert!(matches!(
            first_disagreement(book, &o, &off, 1, 24),
            Disagreement::Pool { hop, .. } if hop == last
        ));

        let mut up = exact.clone();
        *up.last_mut().unwrap() = v2_swap(
            at(&last),
            last.i,
            last.amount_in - U256::from(1u8),
            last.amount_out,
        );
        assert!(matches!(
            first_disagreement(book, &o, &up, 1, 24),
            Disagreement::Upstream { hop, .. } if hop == last
        ));

        let first = o.hops[0];
        assert_eq!(
            first_disagreement(book, &o, &exact[1..], 1, 24),
            Disagreement::Unverified(first)
        );

        let (solved, reads) = view_reads(book, &o, first.pool).unwrap();
        assert_eq!(reads, vec![(at(&first), GET_RESERVES.to_vec())]);
        let PoolState::V2(s) = &book.get(first.pool).unwrap().state else {
            panic!("a V2 pool")
        };
        assert_eq!(
            solved,
            PoolView::V2 {
                reserve0: s.reserve0,
                reserve1: s.reserve1
            },
            "the state the plan was solved on: the book's, after the victim"
        );
    }
}
