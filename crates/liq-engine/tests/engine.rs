//! GUIDE 08 acceptance, end to end: real `StateStore`, real Aave V4
//! adapter, real Chainlink prices and Aave V3 flash depth at block 26018679
//! (`common`). The oracle for "what must be emitted" is always the adapter
//! and `liq_flash::is_eligible` called directly over the whole universe
//! (`Rig::expected`) — never the engine's own bands or index.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

mod common;

use std::time::{Duration, Instant};

use alloy_primitives::{B256, U256};
use common::*;
use liq_engine::{classify, EngineError, TriggerCause};
use liq_protocol::{DirtySet, MarketSlot, Protocol};
use liq_types::{
    Band, Confidence, FlashProvider, MevShareHint, PositionId, Ray, SourceKind, TriggerKind,
};
use smallvec::smallvec;

/// One position per band at the pinned prices (1 WETH collateral at CF 80 %
/// = 2060.71 USD of borrowing power): 2100 DAI is under water, 2040 Hot,
/// 1900 Warm, 1500 Cool, 1000 and 700 Cold, 0 a supplier (Dead). The lender
/// interns last.
fn bands_universe() -> Vec<Borrower> {
    borrowers(7, |i| {
        [2_100, 2_040, 1_900, 1_500, 1_000, 0, 700][i as usize]
    })
}

const ETH_MINUS_30: u64 = ETH_USD_P8 - ETH_USD_P8 * 30 / 100;
const ETH_MINUS_31: u64 = ETH_USD_P8 - ETH_USD_P8 * 31 / 100;
const ETH_PLUS_10: u64 = ETH_USD_P8 + ETH_USD_P8 / 10;

fn public(tx: u8) -> SourceKind {
    SourceKind::PendingPublic {
        tx: B256::repeat_byte(tx),
        confidence: Confidence::CERTAIN,
    }
}

/// Acceptance: bands narrow the sweep but never eligibility — after a −30 %
/// canonical move every position the adapter says is liquidatable and
/// fundable is emitted (a `Cool` one gaps straight through `Warm` and is
/// caught by the index), and nothing else is. Negative: +10 % emits nothing.
#[test]
fn recall_is_exact_across_bands_including_a_cold_gap() {
    let mut rig = Rig::new(&bands_universe(), pinned_flash(), 64);
    rig.resync(T0);
    let px = pinned_prices();
    for id in (0..8u32).map(PositionId) {
        let hf = rig.hf(id, T0, &px);
        let has_debt = hf != liq_protocol::Health::NO_DEBT_HF;
        assert_eq!(
            rig.engine.band(id),
            Some(classify(hf, has_debt)),
            "band of {id:?} at hf {hf:?}"
        );
    }
    assert_eq!(rig.engine.band(pid(3)), Some(Band::Cool));
    assert_eq!(rig.engine.band(pid(4)), Some(Band::Cold));
    assert_eq!(rig.engine.band(pid(5)), Some(Band::Dead));
    // Already under water on canonical state: surfaced as Stale.
    let first = ids(rig.engine.candidates());
    assert_eq!(first, rig.expected(T0, &px));
    assert_eq!(first, vec![pid(0)]);

    let crash = rig.tick(WETH, ETH_MINUS_30, SourceKind::Canonical);
    rig.with_world(T0, |e, w| e.on_price_tick(w, &crash).unwrap());
    let got = ids(rig.engine.candidates());
    let want = rig.expected(T0, &rig.px_with(WETH, ETH_MINUS_30));
    assert_eq!(got, want);
    assert_eq!(
        got,
        vec![pid(0), pid(1), pid(2), pid(3)],
        "Cool #3 gapped through"
    );
    assert_eq!(rig.engine.prices().0[0].price, ray_of_p8(ETH_MINUS_30));
    assert_eq!(rig.engine.band(pid(3)), Some(Band::Hot));
    // #4 (1000 DAI) is at hf 1.44 now but was not swept: Cold is tripwire
    // only and its threshold (3 % above the hf-1.0 price, 1288 USD) did not
    // trip. Its band is a cadence label, not truth — see the next test.
    assert_eq!(rig.engine.band(pid(4)), Some(Band::Cold));
}

/// A `Cold` position is never swept; it is caught by its threshold when the
/// move reaches it, and only then re-banded. Negative: a rally emits nothing.
#[test]
fn cold_position_is_caught_by_its_threshold_not_its_band() {
    let mut rig = Rig::new(&bands_universe(), pinned_flash(), 64);
    rig.resync(T0);
    let crash = rig.tick(WETH, ETH_MINUS_30, SourceKind::Canonical);
    rig.with_world(T0, |e, w| e.on_price_tick(w, &crash).unwrap());
    rig.engine.candidates().count();
    assert_eq!(rig.engine.band(pid(4)), Some(Band::Cold), "not swept");
    // −52 %: 1 WETH · 0.48 · 0.80 = 989 USD of power against 999.78 of debt.
    let deeper = rig.tick(
        WETH,
        ETH_USD_P8 - ETH_USD_P8 * 52 / 100,
        SourceKind::Canonical,
    );
    rig.with_world(T0, |e, w| e.on_price_tick(w, &deeper).unwrap());
    let px = rig.px_with(WETH, ETH_USD_P8 - ETH_USD_P8 * 52 / 100);
    let got = ids(rig.engine.candidates());
    assert_eq!(got, rig.expected(T0, &px));
    assert!(got.contains(&pid(4)), "the Cold one crossed via the index");
    assert!(!got.contains(&pid(6)), "700 DAI at hf 1.41 did not");
    assert_eq!(
        rig.engine.band(pid(4)),
        Some(classify(rig.hf(pid(4), T0, &px), true))
    );
    assert_eq!(rig.engine.band(pid(4)), Some(Band::Hot));
    assert_eq!(rig.engine.band(pid(6)), Some(Band::Cold), "still not swept");
    let rally = rig.tick(WETH, ETH_PLUS_10, SourceKind::Canonical);
    rig.with_world(T0, |e, w| e.on_price_tick(w, &rally).unwrap());
    assert!(rig.expected(T0, &rig.px_with(WETH, ETH_PLUS_10)).is_empty());
    assert_eq!(rig.engine.queued(), 0);
    assert_eq!(rig.engine.band(pid(0)), Some(Band::Warm));
}

/// Acceptance: `Predicted` ticks never produce a fireable candidate and
/// leave the engine untouched; announced ticks fire with the auction /
/// public cause and still leave canonical state untouched; the canonical
/// tick commits.
#[test]
fn predicted_prewarms_announced_fires_canonical_commits() {
    let mut rig = Rig::new(&bands_universe(), pinned_flash(), 64);
    rig.resync(T0);
    rig.engine.candidates().count();
    let before = *rig.engine.stats();
    let px_new = rig.px_with(WETH, ETH_MINUS_30);
    let want = rig.expected(T0, &px_new);

    let predicted = rig.tick(
        WETH,
        ETH_MINUS_30,
        SourceKind::Predicted {
            eta: None,
            confidence: Confidence(9_000),
        },
    );
    rig.with_world(T0, |e, w| e.on_price_tick(w, &predicted).unwrap());
    let pre: Vec<_> = rig.engine.candidates().collect();
    assert_eq!(
        ids(pre.iter().cloned()),
        want,
        "pre-warm computes the full set"
    );
    for c in &pre {
        assert!(!c.fireable());
        assert_eq!(
            c.cause,
            TriggerCause::OraclePredicted {
                conf: Confidence(9_000)
            }
        );
        assert_eq!(c.cause.kind(), TriggerKind::OraclePredicted);
    }
    assert_eq!(
        rig.engine.prices().0[0].price,
        ray_of_p8(ETH_USD_P8),
        "canonical untouched"
    );
    assert_eq!(rig.engine.band(pid(3)), Some(Band::Cool), "bands untouched");
    assert_eq!(rig.engine.stats().folds, before.folds, "no canonical fold");
    assert!(rig.engine.stats().shadow_folds > before.shadow_folds);

    let deadline = Instant::now() + Duration::from_millis(300);
    let announced = rig.tick(
        WETH,
        ETH_MINUS_30,
        SourceKind::SvrAnnounced {
            hint: MevShareHint {
                hash: B256::repeat_byte(0xaa),
                to: None,
                function_selector: None,
                call_data: None,
                logs: None,
            },
            deadline,
        },
    );
    rig.with_world(T0, |e, w| e.on_price_tick(w, &announced).unwrap());
    let fired: Vec<_> = rig.engine.candidates().collect();
    assert_eq!(ids(fired.iter().cloned()), want);
    for c in &fired {
        assert!(c.fireable());
        assert_eq!(
            c.cause,
            TriggerCause::SvrAuction {
                hint: B256::repeat_byte(0xaa),
                deadline
            }
        );
        assert_eq!(c.deadline, Some(deadline));
    }
    assert_eq!(
        rig.engine.prices().0[0].price,
        ray_of_p8(ETH_USD_P8),
        "still untouched"
    );
    assert_eq!(rig.engine.band(pid(3)), Some(Band::Cool));

    // A second pending price on the same asset: evaluated from canonical,
    // so the crosser set is measured against what the chain believes.
    let deeper = rig.tick(WETH, ETH_MINUS_31, public(0x01));
    rig.with_world(T0, |e, w| e.on_price_tick(w, &deeper).unwrap());
    let pub_c: Vec<_> = rig.engine.candidates().collect();
    assert_eq!(
        ids(pub_c.iter().cloned()),
        rig.expected(T0, &rig.px_with(WETH, ETH_MINUS_31))
    );
    assert!(pub_c.iter().all(|c| c.cause
        == TriggerCause::OraclePublic {
            tx: B256::repeat_byte(1)
        }));

    let landed = rig.tick(WETH, ETH_MINUS_30, SourceKind::Canonical);
    rig.with_world(T0, |e, w| e.on_price_tick(w, &landed).unwrap());
    let committed: Vec<_> = rig.engine.candidates().collect();
    assert_eq!(ids(committed.iter().cloned()), want);
    let tip = rig.st.tip();
    assert!(committed.iter().all(|c| matches!(
        c.cause,
        TriggerCause::Stale { liquidatable_since } if liquidatable_since == tip
    )));
    assert_eq!(rig.engine.prices().0[0].price, ray_of_p8(ETH_MINUS_30));
    assert_eq!(rig.engine.band(pid(3)), Some(Band::Hot));
}

/// Acceptance: the correlated-move sweep triggers when ≥ 3 assets move in
/// one block — `Cool` joins the sweep on the third mover, once, and the
/// window closes with the block.
#[test]
fn correlated_move_sweeps_cool_once_per_block() {
    let mut rig = Rig::new(&bands_universe(), pinned_flash(), 64);
    rig.resync(T0);
    rig.engine.candidates().count();
    let hot_warm =
        rig.engine.bands().members(Band::Hot).len() + rig.engine.bands().members(Band::Warm).len();
    let cool = rig.engine.bands().members(Band::Cool).len();
    assert_eq!((hot_warm, cool), (3, 1));
    let folds = |rig: &Rig| rig.engine.stats().shadow_folds;

    let t1 = rig.tick(WETH, ETH_USD_P8 - ETH_USD_P8 / 1_000, public(1));
    let t2 = rig.tick(DAI, DAI_USD_P8 + DAI_USD_P8 / 1_000, public(2));
    let t3 = rig.tick(USDC, USDC_USD_P8 + USDC_USD_P8 / 1_000, public(3));
    let f0 = folds(&rig);
    rig.with_world(T0, |e, w| e.on_price_tick(w, &t1).unwrap());
    assert_eq!(
        folds(&rig) - f0,
        hot_warm as u64,
        "one mover: Hot+Warm only"
    );
    let f1 = folds(&rig);
    rig.with_world(T0, |e, w| e.on_price_tick(w, &t2).unwrap());
    assert_eq!(
        folds(&rig) - f1,
        hot_warm as u64,
        "two movers: still Hot+Warm"
    );
    assert_eq!(rig.engine.stats().correlated_sweeps, 0);
    let f2 = folds(&rig);
    rig.with_world(T0, |e, w| e.on_price_tick(w, &t3).unwrap());
    assert_eq!(
        folds(&rig) - f2,
        (hot_warm + cool) as u64,
        "third mover: Cool swept"
    );
    assert_eq!(rig.engine.stats().correlated_sweeps, 1);
    // A prediction is not a move but rides the block's correlated state.
    let f3 = folds(&rig);
    let p = rig.tick(
        WETH,
        ETH_USD_P8 - ETH_USD_P8 / 500,
        SourceKind::Predicted {
            eta: None,
            confidence: Confidence(5_000),
        },
    );
    rig.with_world(T0, |e, w| e.on_price_tick(w, &p).unwrap());
    assert_eq!(folds(&rig) - f3, (hot_warm + cool) as u64);
    assert_eq!(rig.engine.stats().correlated_sweeps, 2);
    // New block: window closed.
    rig.with_world(T0, |e, w| e.on_block(w).unwrap());
    let f4 = folds(&rig);
    rig.with_world(T0, |e, w| e.on_price_tick(w, &t1).unwrap());
    assert_eq!(folds(&rig) - f4, hot_warm as u64);
    assert_eq!(rig.engine.stats().correlated_sweeps, 2);
    rig.engine.candidates().count();
}

/// Acceptance: the time-cross heap fires within one block of the actual
/// crossing for a V4 position with a nonzero risk premium — and sooner
/// than the identical position without one. Oracle: the adapter's own
/// `health()` one block either side of the scheduled instant.
#[test]
fn time_cross_heap_fires_within_one_block_and_premium_brings_it_forward() {
    // 2055 DAI against 2060.71 of borrowing power: hf 1.0029 at T0, so
    // 5 % base (+ 2 % premium) accrual crosses within weeks.
    let bs = vec![
        Borrower {
            user: user(0),
            weth: ONE,
            dai: U256::from(2_055u64) * ONE,
            premium: true,
        },
        Borrower {
            user: user(1),
            weth: ONE,
            dai: U256::from(2_055u64) * ONE,
            premium: false,
        },
    ];
    let mut rig = Rig::new(&bs, pinned_flash(), 64);
    rig.resync(T0);
    assert_eq!(rig.engine.queued(), 0, "both healthy at T0");
    let px = pinned_prices();
    let (a, b) = (pid(0), pid(1));
    let ta = rig
        .engine
        .heap()
        .scheduled(a)
        .expect("premium position scheduled");
    let tb = rig
        .engine
        .heap()
        .scheduled(b)
        .expect("plain position scheduled");
    assert!(ta < tb, "premium crosses first: {ta} < {tb}");
    assert!(
        ta > T0 && ta - T0 < 60 * 86_400,
        "weeks out, not years: {}",
        ta - T0
    );
    // Adapter oracle: one block before t* still healthy, at t* under water.
    assert!(rig.hf(a, ta - 12, &px) >= Ray::ONE);
    assert!(rig.hf(a, ta, &px) < Ray::ONE);
    assert!(rig.hf(b, ta, &px) >= Ray::ONE, "no premium: not yet");

    // Block before the crossing: nothing.
    rig.with_world(ta - 12, |e, w| e.on_block(w).unwrap());
    assert_eq!(rig.engine.queued(), 0);
    // Crossing block: InterestDrift for the premium position only.
    rig.st.begin_block(rig.st.tip() + 1).unwrap();
    rig.with_world(ta, |e, w| e.on_block(w).unwrap());
    let fired: Vec<_> = rig.engine.candidates().collect();
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].position, a);
    assert_eq!(fired[0].cause, TriggerCause::InterestDrift);
    assert_eq!(fired[0].cause.kind(), TriggerKind::InterestDrift);
    assert_eq!(rig.engine.band(a), Some(Band::Hot));
    assert_eq!(
        rig.engine.heap().scheduled(a),
        None,
        "fired entries leave the heap"
    );
    // Next block, still untaken: Stale since the crossing block.
    let since = rig.st.tip();
    rig.st.begin_block(since + 1).unwrap();
    rig.with_world(ta + 12, |e, w| e.on_block(w).unwrap());
    let again: Vec<_> = rig.engine.candidates().collect();
    assert_eq!(again.len(), 1);
    assert_eq!(
        again[0].cause,
        TriggerCause::Stale {
            liquidatable_since: since
        }
    );
    // The plain position fires at its own, later, instant.
    assert!(rig.hf(b, tb, &px) < Ray::ONE);
    rig.st.begin_block(rig.st.tip() + 1).unwrap();
    rig.with_world(tb, |e, w| e.on_block(w).unwrap());
    let both = ids(rig.engine.candidates());
    assert_eq!(both, vec![a, b]);
}

/// Acceptance: positions with unfundable debt are marked `Unfundable`, not
/// deleted, emit nothing, and are promoted again when flash liquidity
/// returns. Zero drops: the queue never saw them.
#[test]
fn unfundable_is_marked_not_deleted_and_promoted_when_liquidity_returns() {
    let mut rig = Rig::new(&bands_universe(), no_flash(), 64);
    rig.resync(T0);
    assert_eq!(rig.engine.band(pid(0)), Some(Band::Unfundable));
    assert_eq!(rig.engine.queued(), 0);
    let crash = rig.tick(WETH, ETH_MINUS_30, SourceKind::Canonical);
    rig.with_world(T0, |e, w| e.on_price_tick(w, &crash).unwrap());
    assert_eq!(rig.engine.queued(), 0);
    assert_eq!(rig.engine.dropped(), 0);
    for id in 0..4 {
        assert_eq!(rig.engine.band(pid(id)), Some(Band::Unfundable), "{id}");
    }
    assert_eq!(rig.engine.bands().members(Band::Unfundable).len(), 4);
    assert!(rig
        .expected(T0, &rig.px_with(WETH, ETH_MINUS_30))
        .is_empty());

    rig.flash = pinned_flash();
    rig.with_world(T0, |e, w| e.on_flash_change(w).unwrap());
    let got: Vec<_> = rig.engine.candidates().collect();
    assert_eq!(
        ids(got.iter().cloned()),
        rig.expected(T0, &rig.px_with(WETH, ETH_MINUS_30))
    );
    assert_eq!(got.len(), 4);
    for c in &got {
        assert_eq!(c.funding.provider, FlashProvider::Aave);
        assert_eq!(c.funding.fee_bps, AAVE_V3_FLASH_PREMIUM_BPS);
        assert_eq!(rig.engine.band(c.position), Some(Band::Hot));
    }
    assert!(rig.engine.bands().members(Band::Unfundable).is_empty());
}

/// Acceptance: under a synthetic cascade the bounded queue keeps the
/// highest-value candidates, drops the rest, and counts every drop.
/// Oracle: the same cascade through an unbounded queue, sorted by value.
#[test]
fn bounded_queue_keeps_top_values_and_counts_drops() {
    let cascade = borrowers(40, |i| 1_450 + u64::from(i) * 10);
    let mut full = Rig::new(&cascade, pinned_flash(), 1024);
    full.resync(T0);
    full.engine.candidates().count();
    let crash = full.tick(WETH, ETH_MINUS_30, SourceKind::Canonical);
    full.with_world(T0, |e, w| e.on_price_tick(w, &crash).unwrap());
    let mut all: Vec<_> = full.engine.candidates().collect();
    assert_eq!(all.len(), 40, "every borrower is under water at −30 %");
    assert_eq!(full.engine.dropped(), 0);
    all.sort_by_key(|c| std::cmp::Reverse(c.est_value));
    let top8: Vec<PositionId> = all.iter().take(8).map(|c| c.position).collect();
    assert!(
        all.windows(2).all(|w| w[0].est_value > w[1].est_value),
        "distinct values"
    );

    let mut small = Rig::new(&cascade, pinned_flash(), 8);
    small.resync(T0);
    small.engine.candidates().count();
    small.with_world(T0, |e, w| e.on_price_tick(w, &crash).unwrap());
    assert_eq!(small.engine.queued(), 8);
    assert_eq!(small.engine.dropped(), 32, "40 emitted, 8 kept, 32 counted");
    assert_eq!(small.engine.stats().emitted, 40);
    let kept: Vec<_> = small.engine.candidates().collect();
    assert!(
        kept.windows(2).all(|w| w[0].est_value >= w[1].est_value),
        "drained highest first"
    );
    assert_eq!(kept.iter().map(|c| c.position).collect::<Vec<_>>(), top8);
}

/// Acceptance: candidates carry the adapter's `Quote` intact — every repay
/// and seize option with its `BonusCurve` — plus a fundable leg pair whose
/// route funds the repay asset at the real premium.
#[test]
fn candidate_carries_quote_intact_with_fundable_legs() {
    let mut rig = Rig::new(&bands_universe(), pinned_flash(), 64);
    rig.resync(T0);
    rig.engine.candidates().count();
    let crash = rig.tick(WETH, ETH_MINUS_30, SourceKind::Canonical);
    rig.with_world(T0, |e, w| e.on_price_tick(w, &crash).unwrap());
    let px = rig.px_with(WETH, ETH_MINUS_30);
    let view = rig.st.view(T0);
    let cands: Vec<_> = rig.engine.candidates().collect();
    assert_eq!(cands.len(), 4);
    for c in &cands {
        let pos = view.position(c.position).unwrap();
        let want = rig.p.quote(pos, &px).unwrap().expect("liquidatable quotes");
        assert_eq!(c.quote, want, "quote passed through untouched");
        assert!(!c.quote.repay_options.is_empty());
        assert!(!c.quote.seize_options.is_empty());
        let repay = &c.quote.repay_options[usize::from(c.legs.repay)];
        let seize = &c.quote.seize_options[usize::from(c.legs.seize)];
        assert_eq!(c.funding.asset, repay.asset);
        assert_eq!(c.funding.amount, repay.max_repay);
        assert_eq!(c.funding.provider, FlashProvider::Aave);
        assert_eq!(c.funding.source, AAVE_V3_POOL);
        assert_eq!(c.funding.fee_bps, AAVE_V3_FLASH_PREMIUM_BPS);
        assert_eq!(c.health, rig.p.health(pos, &px).unwrap());
        assert!(c.health.hf < Ray::ONE);
        assert_eq!(c.protocol, PROTOCOL);
        assert_eq!(c.cause.kind(), TriggerKind::Stale);
        assert!(c.deadline.is_none());
        // est_value = min(debt, collateral) · bonus, restated.
        let notional = c
            .health
            .debt_value
            .raw()
            .min(c.health.collateral_value.raw());
        let want_est = notional * seize.bonus.raw() / liq_types::fixed::RAY;
        assert_eq!(c.est_value.raw(), want_est);
        assert!(!c.est_value.raw().is_zero());
    }
    let traces: std::collections::BTreeSet<_> = cands.iter().map(|c| c.trace).collect();
    assert_eq!(traces.len(), 4, "one trace per candidate");
}

/// Dirty sets fold with the caller's cause; row sets reach exactly the
/// positions holding the row (walked bands for accrual, every registered
/// threshold for a reprice); `ProtocolWide` folds the universe.
#[test]
fn dirty_sets_fold_the_right_positions_with_the_given_cause() {
    let mut rig = Rig::new(&bands_universe(), pinned_flash(), 64);
    rig.resync(T0);
    let crash = rig.tick(WETH, ETH_MINUS_30, SourceKind::Canonical);
    rig.with_world(T0, |e, w| e.on_price_tick(w, &crash).unwrap());
    rig.engine.candidates().count();
    let folds = |rig: &Rig| rig.engine.stats().folds;

    let tx = B256::repeat_byte(0x77);
    let cause = TriggerCause::UserAction { tx };
    let f = folds(&rig);
    rig.with_world(T0, |e, w| {
        e.on_dirty(w, PROTOCOL, &DirtySet::Positions(smallvec![pid(3)]), &cause)
            .unwrap()
    });
    assert_eq!(folds(&rig) - f, 1);
    let got: Vec<_> = rig.engine.candidates().collect();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].position, pid(3));
    assert_eq!(got[0].cause, TriggerCause::UserAction { tx });

    // Accrual on the DAI row: Hot (0..=3) + Warm/Cool holders of DAI debt.
    let dai_row = MarketSlot {
        market: SPOKE_MARKET,
        slot: v4::DAI_SLOT,
    };
    let walked = [Band::Hot, Band::Warm, Band::Cool]
        .iter()
        .map(|b| rig.engine.bands().members(*b).len())
        .sum::<usize>();
    let f = folds(&rig);
    rig.with_world(T0, |e, w| {
        e.on_dirty(
            w,
            PROTOCOL,
            &DirtySet::MarketAccrual(smallvec![dai_row]),
            &TriggerCause::InterestDrift,
        )
        .unwrap()
    });
    assert_eq!(
        folds(&rig) - f,
        walked as u64,
        "every walked holder of the row"
    );
    let got: Vec<_> = rig.engine.candidates().collect();
    assert_eq!(
        ids(got.iter().cloned()),
        vec![pid(0), pid(1), pid(2), pid(3)]
    );
    assert!(got.iter().all(|c| c.cause == TriggerCause::InterestDrift));

    // Reprice on the WETH row: every position with a live WETH threshold —
    // the Cold ones (#4, #6) included; not the supplier, not the lender.
    let weth_row = MarketSlot {
        market: SPOKE_MARKET,
        slot: v4::WETH_SLOT,
    };
    let f = folds(&rig);
    rig.with_world(T0, |e, w| {
        e.on_dirty(
            w,
            PROTOCOL,
            &DirtySet::MarketReprice(smallvec![weth_row]),
            &TriggerCause::ParamChange {
                market: SPOKE_MARKET,
            },
        )
        .unwrap()
    });
    assert_eq!(folds(&rig) - f, 6, "#0–#4 and #6 hold WETH against debt");
    let got: Vec<_> = rig.engine.candidates().collect();
    assert!(got.iter().all(|c| c.cause
        == TriggerCause::ParamChange {
            market: SPOKE_MARKET
        }));

    let f = folds(&rig);
    rig.with_world(T0, |e, w| {
        e.on_dirty(
            w,
            PROTOCOL,
            &DirtySet::ProtocolWide,
            &TriggerCause::InterestDrift,
        )
        .unwrap()
    });
    assert_eq!(folds(&rig) - f, 8, "the whole universe");
    rig.engine.candidates().count();
    let f = folds(&rig);
    rig.with_world(T0, |e, w| {
        e.on_dirty(w, PROTOCOL, &DirtySet::None, &TriggerCause::InterestDrift)
            .unwrap()
    });
    assert_eq!(folds(&rig), f);
    assert_eq!(rig.engine.stats().fold_errors, 0);
}

/// A price vector not indexed by global asset id is refused, and a tick
/// for an asset outside the universe is an error, never a silent skip.
#[test]
fn misindexed_prices_and_unknown_assets_are_errors() {
    let mut rig = Rig::new(&bands_universe(), pinned_flash(), 64);
    let mut bad = pinned_prices();
    bad.0.swap(0, 1);
    assert_eq!(
        rig.engine.load_prices(&bad),
        Err(EngineError::PriceLayout {
            slot: 0,
            found: DAI
        })
    );
    rig.resync(T0);
    let stray = rig.tick(liq_types::AssetId(7), 1, SourceKind::Canonical);
    let r = rig.with_world(T0, |e, w| e.on_price_tick(w, &stray));
    assert_eq!(r, Err(EngineError::UnknownAsset(liq_types::AssetId(7))));
}
