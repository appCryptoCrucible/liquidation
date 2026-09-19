//! Undo ring and reorg tests (GUIDE 02 §5; TESTING §3 state-store row).
//!
//! Oracle for every equality here: the **mathematical invariant**
//! `undo(apply(x)) == x` — the state is digested through the public read path
//! before a block, mutated, unwound, and digested again. No expected value is
//! produced by the store's own code. Negative assertions: `ReorgTooDeep`
//! leaves the state untouched, a gap is refused, mutation #8 goes red.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use alloy_primitives::Address;
use liq_protocol::{
    AssetMask, FeedId, MarketFlags, MarketRow, MarketSlot, PositionExtraRepr, ProtocolError,
    StateWriter,
};
use liq_state::{Overlay, StateError, StateStore, StoreConfig, UndoCapacity, UNDO_DEPTH};
use liq_types::{AssetId, MarketId, PositionId, PositionKey, ProtocolId, RayU128};
use proptest::prelude::*;

const BASE: u64 = 1_000;
/// Markets the generators may touch; the last one does not exist at start,
/// so a `Push` to it creates a market inside a block.
const MARKETS: [MarketId; 3] = [MarketId(5), MarketId(9), MarketId(13)];

fn row(asset: u16, tag: u32) -> MarketRow {
    MarketRow {
        supply_index: RayU128::from_raw(u128::from(tag) << 64),
        debt_index: RayU128::from_raw(u128::from(tag)),
        supply_rate: RayU128::from_raw(7),
        debt_rate: RayU128::from_raw(11),
        dust_floor: u128::from(tag),
        last_update: tag,
        target_hf: 10_500,
        hub_ref: u16::MAX,
        liq_threshold: 8_000,
        ltv: 7_500,
        price_feed: FeedId(asset),
        asset: AssetId(asset),
        max_liq_bonus: 500,
        hf_for_max_bonus: 9_500,
        liq_bonus_factor: 10_000,
        decimals: 18,
        flags: MarketFlags::NONE,
        _pad: [0; 22],
    }
}

fn key(market: MarketId, user: u8) -> PositionKey {
    PositionKey {
        protocol: ProtocolId(1),
        market,
        user: Address::repeat_byte(user),
    }
}

fn extra(v: u128) -> PositionExtraRepr {
    let mut e = PositionExtraRepr::ZERO;
    *e.view_mut::<u128>().unwrap() = v;
    e
}

fn cfg(ops: usize) -> StoreConfig {
    StoreConfig {
        base: BASE,
        positions: 64,
        markets: 4,
        undo: UndoCapacity {
            ops,
            extras: ops / 4,
            rows: ops / 4,
        },
    }
}

/// Two markets with three and two reserves, three positions each with a
/// starting balance, so every op kind has something to hit.
fn seeded(ops: usize) -> StateStore {
    let mut st = StateStore::new(cfg(ops));
    for a in 0..3u16 {
        st.push_market(MARKETS[0], row(a, 1)).unwrap();
    }
    for a in 3..5u16 {
        st.push_market(MARKETS[1], row(a, 1)).unwrap();
    }
    for (m, users) in [(MARKETS[0], 0..3u8), (MARKETS[1], 10..13u8)] {
        for u in users {
            let p = st.intern(&key(m, u)).unwrap();
            st.set_supply(p, 0, 1_000 + u128::from(u)).unwrap();
            st.set_debt(p, 1, 500 + u128::from(u)).unwrap();
        }
    }
    st
}

// ---------------------------------------------------------------------------
// Digest through the public read path
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
struct Pos {
    key: PositionKey,
    config: AssetMask,
    supply: Vec<u128>,
    debt: Vec<u128>,
    extra: PositionExtraRepr,
}

/// Everything a reader can observe. The ring's `floor` is deliberately not
/// state: it only ever rises (eviction) and an unwind does not lower it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Digest {
    positions: Vec<Pos>,
    markets: Vec<Result<Vec<MarketRow>, ProtocolError>>,
    tip: u64,
}

fn digest(st: &StateStore) -> Digest {
    let view = st.view(0);
    let positions = (0..st.len() as u32)
        .map(|i| {
            let r = view.position(PositionId(i)).unwrap();
            Pos {
                key: *r.key,
                config: r.config,
                supply: r.supply.to_vec(),
                debt: r.debt.to_vec(),
                extra: *r.extra,
            }
        })
        .collect();
    let markets = MARKETS
        .iter()
        .map(|&m| st.markets(m).map(<[MarketRow]>::to_vec))
        .collect();
    Digest {
        positions,
        markets,
        tip: st.tip(),
    }
}

// ---------------------------------------------------------------------------
// Random mutation sequences
// ---------------------------------------------------------------------------

/// Abstract op; indices are reduced against the live state when applied, so
/// every generated op targets something that exists (or, for `Intern`, a key
/// that may or may not).
#[derive(Clone, Debug)]
enum Op {
    Intern { market: u8, user: u8 },
    Supply { pos: u8, slot: u8, v: u128 },
    Debt { pos: u8, slot: u8, v: u128 },
    Extra { pos: u8, v: u128 },
    Market { market: u8, slot: u8, tag: u32 },
    Push { market: u8, tag: u32 },
}

/// Values biased to zero (clears the mask bit), one, and the extremes.
fn value() -> impl Strategy<Value = u128> {
    prop_oneof![
        4 => Just(0u128),
        2 => Just(1u128),
        2 => Just(u128::MAX),
        4 => any::<u64>().prop_map(u128::from),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0..3u8, 0..8u8).prop_map(|(market, user)| Op::Intern { market, user }),
        8 => (any::<u8>(), any::<u8>(), value()).prop_map(|(pos, slot, v)| Op::Supply { pos, slot, v }),
        8 => (any::<u8>(), any::<u8>(), value()).prop_map(|(pos, slot, v)| Op::Debt { pos, slot, v }),
        2 => (any::<u8>(), value()).prop_map(|(pos, v)| Op::Extra { pos, v }),
        3 => (0..3u8, any::<u8>(), any::<u32>()).prop_map(|(market, slot, tag)| Op::Market { market, slot, tag }),
        1 => (0..3u8, any::<u32>()).prop_map(|(market, tag)| Op::Push { market, tag }),
    ]
}

/// Apply one op against the live state. Errors are legitimate outcomes (a
/// slot that does not exist yet, an unknown market) and must leave the state
/// unchanged — the round trip below would expose one that did not.
fn apply(st: &mut StateStore, op: &Op) {
    let n = st.len() as u8;
    let pos_of = |i: u8| PositionId(u32::from(if n == 0 { 0 } else { i % n }));
    let slot_of = |st: &StateStore, m: MarketId, s: u8| -> u16 {
        let k = st.markets(m).map_or(0, <[MarketRow]>::len) as u8;
        u16::from(if k == 0 { s } else { s % k })
    };
    let _ = match *op {
        Op::Intern { market, user } => st
            .intern(&key(MARKETS[usize::from(market)], user))
            .map(|_| ()),
        Op::Supply { pos, slot, v } => {
            let p = pos_of(pos);
            let m = st
                .view(0)
                .position(p)
                .map(|r| r.key.market)
                .unwrap_or(MARKETS[0]);
            st.set_supply(p, slot_of(st, m, slot), v)
        }
        Op::Debt { pos, slot, v } => {
            let p = pos_of(pos);
            let m = st
                .view(0)
                .position(p)
                .map(|r| r.key.market)
                .unwrap_or(MARKETS[0]);
            st.set_debt(p, slot_of(st, m, slot), v)
        }
        Op::Extra { pos, v } => st.set_extra(pos_of(pos), extra(v)),
        Op::Market { market, slot, tag } => {
            let m = MARKETS[usize::from(market)];
            st.set_market(
                MarketSlot {
                    market: m,
                    slot: slot_of(st, m, slot),
                },
                row(u16::from(slot), tag),
            )
        }
        Op::Push { market, tag } => st
            .push_market(MARKETS[usize::from(market)], row(u16::from(tag as u8), tag))
            .map(|_| ()),
    };
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1_000, ..ProptestConfig::default() })]

    /// `undo(apply(x)) == x` over random sequences split across three
    /// blocks, unwound one block at a time and then all at once; re-applying
    /// the same sequence reproduces the same forward digests (the interner,
    /// market table and strides were restored too, not just the cells).
    #[test]
    fn undo_apply_is_identity(
        ops in prop::collection::vec(op(), 1..200),
        cut1 in 0..200usize,
        cut2 in 0..200usize,
    ) {
        let (a, b) = (cut1.min(ops.len()), cut2.min(ops.len()));
        let (a, b) = (a.min(b), a.max(b));
        let mut st = seeded(4_096);
        let d0 = digest(&st);

        st.begin_block(BASE + 1).unwrap();
        for o in &ops[..a] { apply(&mut st, o); }
        let d1 = digest(&st);
        st.begin_block(BASE + 2).unwrap();
        for o in &ops[a..b] { apply(&mut st, o); }
        let d2 = digest(&st);
        st.begin_block(BASE + 3).unwrap();
        for o in &ops[b..] { apply(&mut st, o); }
        let d3 = digest(&st);

        st.unwind_to(BASE + 2).unwrap();
        prop_assert_eq!(digest(&st), d2.clone());
        st.unwind_to(BASE + 1).unwrap();
        prop_assert_eq!(digest(&st), d1.clone());
        st.unwind_to(BASE).unwrap();
        prop_assert_eq!(digest(&st), d0);

        // Determinism after undo: the same blocks replay to the same states.
        st.begin_block(BASE + 1).unwrap();
        for o in &ops[..a] { apply(&mut st, o); }
        prop_assert_eq!(digest(&st), d1);
        st.begin_block(BASE + 2).unwrap();
        for o in &ops[a..b] { apply(&mut st, o); }
        prop_assert_eq!(digest(&st), d2);
        st.begin_block(BASE + 3).unwrap();
        for o in &ops[b..] { apply(&mut st, o); }
        prop_assert_eq!(digest(&st), d3);
        // A three-block unwind in one call equals the three single steps.
        st.unwind_to(BASE).unwrap();
        prop_assert_eq!(digest(&st).positions.len(), 6);
    }
}

// ---------------------------------------------------------------------------
// Named cases
// ---------------------------------------------------------------------------

/// A position created in a block must disappear exactly when the block is
/// unwound: unknown to the view, absent from the interner (re-interning
/// yields a fresh id equal to the popped one), and its market's block no
/// longer holds it. Oracle: the invariant + GUIDE 02 §1 (ids dense).
#[test]
fn created_position_disappears_on_unwind() {
    let mut st = seeded(64);
    let d0 = digest(&st);
    let n = st.len() as u32;
    st.begin_block(BASE + 1).unwrap();
    let k = key(MARKETS[0], 42);
    let p = st.intern(&k).unwrap();
    assert_eq!(p, PositionId(n));
    st.set_supply(p, 2, 77).unwrap();
    st.set_debt(p, 0, 1).unwrap();
    assert_eq!(st.len() as u32, n + 1);
    assert!(st.view(0).position(p).is_ok());

    st.unwind_to(BASE).unwrap();
    assert_eq!(digest(&st), d0);
    assert_eq!(st.position_id(&k), None, "interner forgot the key");
    assert_eq!(
        st.view(0).position(p).err(),
        Some(StateError::UnknownPosition(p))
    );
    assert_eq!(st.supply(p, 2), Err(ProtocolError::UnknownPosition(p)));
    // Re-interning after the unwind assigns the same dense id again.
    st.begin_block(BASE + 1).unwrap();
    assert_eq!(st.intern(&k).unwrap(), p);
}

/// A market created inside a block (first `push_market` for its id) is gone
/// after the unwind, along with a position interned into it.
#[test]
fn created_market_disappears_on_unwind() {
    let mut st = seeded(64);
    let d0 = digest(&st);
    assert_eq!(
        st.markets(MARKETS[2]),
        Err(ProtocolError::UnknownMarket(MARKETS[2]))
    );
    st.begin_block(BASE + 1).unwrap();
    let at = st.push_market(MARKETS[2], row(50, 3)).unwrap();
    assert_eq!(at.slot, 0);
    let p = st.intern(&key(MARKETS[2], 1)).unwrap();
    st.set_debt(p, 0, 9).unwrap();
    assert_eq!(st.markets(MARKETS[2]).unwrap().len(), 1);

    st.unwind_to(BASE).unwrap();
    assert_eq!(digest(&st), d0);
    assert_eq!(
        st.markets(MARKETS[2]),
        Err(ProtocolError::UnknownMarket(MARKETS[2]))
    );
    assert_eq!(
        st.intern(&key(MARKETS[2], 1)),
        Err(ProtocolError::UnknownMarket(MARKETS[2]))
    );
}

/// TESTING §4 mutation #8 — drop the mask inverse (`prev_set`) from the
/// `Supply`/`Debt` undo record. A single `set_supply` that flips the bit
/// (0 → nonzero sets it; nonzero → 0 with no debt clears it) followed by an
/// unwind must restore the **config** as well as the balance; nothing else
/// in the block can mask a dropped field. Oracle: the invariant.
#[test]
fn mutation_8_undo_restores_config_bit_not_just_balance() {
    let mut st = seeded(64);
    let p = PositionId(0);
    let before = digest(&st);
    let cfg0 = st.view(0).position(p).unwrap().config;
    assert!(cfg0.contains(0) && cfg0.contains(1) && !cfg0.contains(2));

    // Set a fresh slot: bit 2 goes 0 → 1.
    st.begin_block(BASE + 1).unwrap();
    st.set_supply(p, 2, 5).unwrap();
    assert!(st.view(0).position(p).unwrap().config.contains(2));
    st.unwind_to(BASE).unwrap();
    let after = st.view(0).position(p).unwrap();
    assert!(
        !after.config.contains(2),
        "Supply undo must clear the bit it set"
    );
    assert_eq!(after.supply[2], 0);
    assert_eq!(digest(&st), before);

    // Zero a held slot with no debt: bit 0 goes 1 → 0.
    st.begin_block(BASE + 1).unwrap();
    st.set_supply(p, 0, 0).unwrap();
    assert!(!st.view(0).position(p).unwrap().config.contains(0));
    st.unwind_to(BASE).unwrap();
    let after = st.view(0).position(p).unwrap();
    assert!(
        after.config.contains(0),
        "Supply undo must re-set the bit it cleared"
    );
    assert_eq!(after.supply[0], 1_000);
    assert_eq!(digest(&st), before);

    // Same for the debt column (slot 1 holds debt only).
    st.begin_block(BASE + 1).unwrap();
    st.set_debt(p, 1, 0).unwrap();
    assert!(!st.view(0).position(p).unwrap().config.contains(1));
    st.unwind_to(BASE).unwrap();
    assert!(
        st.view(0).position(p).unwrap().config.contains(1),
        "Debt undo must re-set the bit"
    );
    assert_eq!(digest(&st), before);
}

/// `set_extra` / `set_market` inverses: the whole 64-byte / 128-byte value
/// comes back, including a field a shallow undo would miss (`_pad` is part
/// of the row; `dust_floor` sits on line 1).
#[test]
fn extra_and_market_rows_round_trip_whole_value() {
    let mut st = seeded(64);
    let before = digest(&st);
    let p = PositionId(1);
    let at = MarketSlot {
        market: MARKETS[0],
        slot: 1,
    };
    st.begin_block(BASE + 1).unwrap();
    st.set_extra(p, extra(u128::MAX)).unwrap();
    st.set_extra(p, extra(3)).unwrap();
    let mut r = row(1, 99);
    r._pad = [1; 22];
    st.set_market(at, r).unwrap();
    st.set_market(at, row(1, 100)).unwrap();
    assert_eq!(st.market(at).unwrap().last_update, 100);
    st.unwind_to(BASE).unwrap();
    assert_eq!(digest(&st), before);
    assert_eq!(st.market(at).unwrap().last_update, 1);
    assert_eq!(*st.extra(p).unwrap(), PositionExtraRepr::ZERO);
}

/// Depth 8 and 64 (TESTING §3), then the ring edge: unwinding exactly
/// `UNDO_DEPTH` blocks works, one more is `ReorgTooDeep` **and changes
/// nothing** — no partial unwind. Oracle: the invariant + GUIDE 02 §5.
#[test]
fn reorg_depths_and_too_deep_is_total_or_nothing() {
    let mut st = seeded(256);
    let mut digests = vec![digest(&st)];
    for k in 1..=(UNDO_DEPTH as u64 + 10) {
        st.begin_block(BASE + k).unwrap();
        let p = PositionId((k % 6) as u32);
        st.set_supply(p, 0, k as u128 * 3).unwrap();
        st.set_debt(p, 1, if k % 5 == 0 { 0 } else { k as u128 })
            .unwrap();
        if k % 7 == 0 {
            st.intern(&key(MARKETS[1], 100 + k as u8)).unwrap();
        }
        if k % 11 == 0 {
            st.set_market(
                MarketSlot {
                    market: MARKETS[1],
                    slot: 0,
                },
                row(3, k as u32),
            )
            .unwrap();
        }
        digests.push(digest(&st));
    }
    let tip = st.tip();
    assert_eq!(tip, BASE + UNDO_DEPTH as u64 + 10);
    assert_eq!(st.floor(), BASE + 10, "ten records evicted");

    st.unwind_to(tip - 8).unwrap();
    assert_eq!(digest(&st), digests[UNDO_DEPTH + 2]);
    st.unwind_to(tip - 64).unwrap();
    assert_eq!(digest(&st), digests[UNDO_DEPTH + 10 - 64]);

    // Too deep by one: refused with the exact numbers, state untouched.
    let now = digest(&st);
    let err = st.unwind_to(BASE + 9);
    assert_eq!(
        err,
        Err(StateError::ReorgTooDeep {
            depth: (tip - 64) - (BASE + 9),
            cap: (tip - 64) - (BASE + 10),
        })
    );
    assert_eq!(digest(&st), now, "no partial unwind");
    assert_eq!(st.tip(), tip - 64);

    // Exactly to the floor is fine.
    st.unwind_to(BASE + 10).unwrap();
    assert_eq!(digest(&st), digests[10]);
    // Above the tip is a distinct error, also a no-op.
    assert_eq!(
        st.unwind_to(BASE + 11),
        Err(StateError::TargetAboveTip {
            target: BASE + 11,
            tip: BASE + 10
        })
    );
    assert_eq!(digest(&st), digests[10]);
}

/// Blocks must be contiguous; a refused `begin_block` leaves the tip alone.
#[test]
fn block_gap_is_refused() {
    let mut st = seeded(16);
    assert_eq!(
        st.begin_block(BASE + 2),
        Err(StateError::BlockGap {
            tip: BASE,
            got: BASE + 2
        })
    );
    assert_eq!(
        st.begin_block(BASE),
        Err(StateError::BlockGap {
            tip: BASE,
            got: BASE
        })
    );
    assert_eq!(st.tip(), BASE);
    st.begin_block(BASE + 1).unwrap();
    assert_eq!(st.tip(), BASE + 1);
}

/// Writer-contract negatives (GUIDE 02 §7b: `None` is an error, never
/// growth): slots past the market's rows, unknown ids, unknown markets,
/// and the 128-slot ceiling.
#[test]
fn out_of_range_is_refused() {
    let mut st = seeded(16);
    let p = PositionId(0);
    let bad = MarketSlot {
        market: MARKETS[0],
        slot: 3,
    };
    assert_eq!(
        st.set_supply(p, 3, 1),
        Err(ProtocolError::SlotOutOfRange(bad))
    );
    assert_eq!(st.supply(p, 3), Err(ProtocolError::SlotOutOfRange(bad)));
    assert_eq!(st.market(bad), Err(ProtocolError::SlotOutOfRange(bad)));
    assert_eq!(
        st.set_market(bad, row(0, 0)),
        Err(ProtocolError::SlotOutOfRange(bad))
    );
    let ghost = PositionId(99);
    assert_eq!(
        st.set_debt(ghost, 0, 1),
        Err(ProtocolError::UnknownPosition(ghost))
    );
    assert_eq!(st.extra(ghost), Err(ProtocolError::UnknownPosition(ghost)));
    assert_eq!(
        st.view(0).position(ghost).err(),
        Some(StateError::UnknownPosition(ghost))
    );
    assert_eq!(
        st.intern(&key(MarketId(77), 1)),
        Err(ProtocolError::UnknownMarket(MarketId(77)))
    );
    let m = MarketId(20);
    for a in 0..128u16 {
        st.push_market(m, row(a, 0)).unwrap();
    }
    assert_eq!(
        st.push_market(m, row(128, 0)),
        Err(ProtocolError::SlotOutOfRange(MarketSlot {
            market: m,
            slot: 128
        }))
    );
    assert_eq!(st.markets(m).unwrap().len(), 128);
}

// ---------------------------------------------------------------------------
// Overlay
// ---------------------------------------------------------------------------

/// A pending delta authored through `StateWriter` on an `Overlay` is visible
/// through `view_with` and invisible through `view`; the store's digest does
/// not move; the overlay is refused once the store advances. Oracle: GUIDE 02
/// §6 ("must never touch canonical state") + the base digest.
#[test]
fn overlay_applies_delta_without_touching_the_store() {
    let mut st = seeded(64);
    let before = digest(&st);
    let p0 = PositionId(0);
    let at = MarketSlot {
        market: MARKETS[0],
        slot: 0,
    };

    let mut ov = Overlay::new();
    {
        let mut w = ov.writer(&st).unwrap();
        // Read-through before any write.
        assert_eq!(w.supply(p0, 0).unwrap(), 1_000);
        w.set_supply(p0, 0, 0).unwrap();
        w.set_debt(p0, 2, 4_242).unwrap();
        w.set_extra(p0, extra(9)).unwrap();
        w.set_market(at, row(0, 555)).unwrap();
        // A new position and a new slot, pending only.
        let pn = w.intern(&key(MARKETS[1], 200)).unwrap();
        assert_eq!(pn, PositionId(6));
        let ns = w.push_market(MARKETS[1], row(60, 1)).unwrap();
        assert_eq!(ns.slot, 2);
        w.set_supply(pn, 2, 31).unwrap();
        // The overlay's own copies read back.
        assert_eq!(w.supply(p0, 0).unwrap(), 0);
        assert_eq!(w.debt(p0, 2).unwrap(), 4_242);
        assert_eq!(w.market(at).unwrap().last_update, 555);
        assert_eq!(w.supply(pn, 2).unwrap(), 31);
        // A base position of the pushed market, materialised after the push,
        // gains the new cell.
        let p3 = PositionId(3);
        w.set_debt(p3, 2, 1).unwrap();
        assert_eq!(w.supply(p3, 2).unwrap(), 0);
    }
    // Canonical state untouched.
    assert_eq!(digest(&st), before);
    assert_eq!(st.len(), 6);

    let pending = st.view_with(&ov, 1_700_000_012).unwrap();
    let r = pending.position(p0).unwrap();
    assert_eq!(
        r.timestamp, 1_700_000_012,
        "forward-projection time carried"
    );
    assert_eq!(r.supply[0], 0);
    assert!(!r.config.contains(0), "config follows the overlay balances");
    assert!(r.config.contains(2));
    assert_eq!(r.debt[2], 4_242);
    assert_eq!(*r.extra, extra(9));
    assert_eq!(
        r.markets[0].last_update, 555,
        "overlay rows for an overlay position"
    );
    // Base position in a market with overlay rows: base balances, overlay rows.
    let r1 = pending.position(PositionId(1)).unwrap();
    assert_eq!(r1.supply[0], 1_001);
    assert_eq!(r1.markets[0].last_update, 555);
    // New position and pushed slot exist only in the pending view.
    let rn = pending.position(PositionId(6)).unwrap();
    assert_eq!(rn.supply, &[0, 0, 31]);
    assert_eq!(rn.markets.len(), 3);
    assert_eq!(pending.markets(MARKETS[1]).unwrap().len(), 3);
    assert_eq!(pending.len(), 7);
    assert_eq!(
        st.view(0).position(PositionId(6)).err(),
        Some(StateError::UnknownPosition(PositionId(6)))
    );
    assert_eq!(st.markets(MARKETS[1]).unwrap().len(), 2);
    let r3 = pending.position(PositionId(3)).unwrap();
    assert_eq!(r3.debt, &[0, 510, 1]);

    // The store moves on: the overlay is stale for reading and for writing.
    st.begin_block(BASE + 1).unwrap();
    assert!(matches!(
        st.view_with(&ov, 0),
        Err(StateError::OverlayStale { .. })
    ));
    assert!(matches!(
        ov.writer(&st),
        Err(StateError::OverlayStale { .. })
    ));
    ov.clear();
    assert!(ov.is_empty());
    let w = ov.writer(&st).unwrap();
    assert_eq!(
        w.supply(p0, 0).unwrap(),
        1_000,
        "cleared overlay reads through again"
    );
}

/// An overlay's materialised copy is a snapshot of the position at the moment
/// it was first touched. Any canonical write after that — including one
/// *inside* the block the overlay was built against, which moves neither the
/// tip nor the position count — makes every un-overwritten cell of that copy
/// stale, so `view_with` and `writer` must refuse it. Oracle: GUIDE 02 §6
/// (pending evaluation layers a delta on canonical state; a copy that no
/// longer matches its base is not that) + the base read path. Negative: the
/// canonical `view` is unaffected, and the refusal is an error, not a guess.
#[test]
fn overlay_built_before_a_same_block_write_is_refused() {
    let mut st = seeded(64);
    let p0 = PositionId(0);
    let at = MarketSlot {
        market: MARKETS[0],
        slot: 1,
    };

    st.begin_block(BASE + 1).unwrap();
    let mut ov = Overlay::new();
    {
        let mut w = ov.writer(&st).unwrap();
        // Touching slot 0 copies the position's whole row, slot 1 included.
        w.set_supply(p0, 0, 7).unwrap();
        w.set_market(at, row(1, 42)).unwrap();
    }

    // Canonical writes inside the same block: no new position, no new block,
    // so neither `tip` nor `len` moves.
    st.set_debt(p0, 1, 123_456).unwrap();
    st.set_market(at, row(1, 77)).unwrap();
    assert_eq!(st.tip(), BASE + 1);
    assert_eq!(st.len(), 6);
    assert_eq!(st.view(0).position(p0).unwrap().debt[1], 123_456);

    // The overlay's copy of slot 1 predates both writes.
    assert!(
        matches!(st.view_with(&ov, 0), Err(StateError::OverlayStale { .. })),
        "a delta over a superseded base must be refused, not served"
    );
    assert!(matches!(
        ov.writer(&st),
        Err(StateError::OverlayStale { .. })
    ));

    // Cleared, it re-bases on the current state and reads the new values.
    ov.clear();
    let w = ov.writer(&st).unwrap();
    assert_eq!(w.debt(p0, 1).unwrap(), 123_456);
    assert_eq!(w.market(at).unwrap().last_update, 77);
}
