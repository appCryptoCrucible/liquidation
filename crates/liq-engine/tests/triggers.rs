//! 08B fan-out against the real 08A engine (derived tick, stale N, param-change).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

mod common;

use common::*;
use liq_engine::{
    attach_crossing, fire_param_change, on_derived_tick, take_ripe, StaleConfig, TriggerCause,
    TriggerError,
};
use liq_types::{PositionId, ScheduledParamChange, SourceKind, TraceId, TriggerKind};
use smallvec::smallvec;

fn ids(cs: &[liq_engine::Candidate]) -> Vec<PositionId> {
    let mut v: Vec<_> = cs.iter().map(|c| c.position).collect();
    v.sort_unstable();
    v
}

/// Oracle: a derived tick uses 08A `on_price_tick` and the cause is
/// `DerivedRate` via `kind()`. Negative: a canonical tick is refused.
#[test]
fn derived_tick_fans_out_to_derived_rate_cause() {
    let mut rig = Rig::new(&borrowers(1, |_| 2_100), pinned_flash(), 16);
    rig.resync(T0);
    let _ = ids(&rig.engine.candidates().collect::<Vec<_>>());
    let tick = rig.tick(
        WETH,
        ETH_USD_P8 - ETH_USD_P8 * 30 / 100,
        SourceKind::Derived { deps: smallvec![] },
    );
    rig.with_world(T0, |e, w| on_derived_tick(e, w, &tick).unwrap());
    let got: Vec<_> = rig.engine.candidates().collect();
    assert!(!got.is_empty());
    assert!(got
        .iter()
        .all(|c| c.cause.kind() == TriggerKind::DerivedRate));
    let canon = rig.tick(WETH, ETH_USD_P8, SourceKind::Canonical);
    rig.with_world(T0, |e, w| {
        assert_eq!(on_derived_tick(e, w, &canon), Err(TriggerError::NotDerived));
    });
}

/// Oracle: 08A emits Stale on first sight; 08B only forwards after N=3.
#[test]
fn stale_gate_holds_until_n_blocks() {
    let mut rig = Rig::new(&borrowers(1, |_| 2_100), pinned_flash(), 16);
    rig.resync(T0);
    let n = StaleConfig::new(3).unwrap();
    let tip = rig.with_world(T0, |_, w| w.view.tip());
    let held = take_ripe(&mut rig.engine, tip, n);
    assert!(
        held.is_empty(),
        "first-sight Stale must not pass N=3 at the same tip"
    );
    assert!(n.ripe(tip.saturating_add(3), tip));
}

/// ParamChange fires only at execution_block, with the index crossing set,
/// and `kind()` is ParamChange.
#[test]
fn param_change_fires_at_execution_block_with_crossing() {
    let mut rig = Rig::new(
        &borrowers(3, |i| [2_100, 1_500, 0][i as usize]),
        pinned_flash(),
        16,
    );
    rig.resync(T0);
    let _ = rig.engine.candidates().count();
    let ev = rig.with_world(T0, |e, w| {
        let tip = w.view.tip();
        let mut ev = ScheduledParamChange {
            protocol: PROTOCOL,
            market: SPOKE_MARKET,
            execution_block: tip,
            crossing: Vec::new(),
            trace: TraceId::from_raw(1),
        };
        let px = e.prices();
        let weth = px.0[0].price;
        ev = attach_crossing(ev, e.index(), WETH, weth, weth);
        fire_param_change(e, w, &ev).unwrap();
        ev
    });
    let tip = ev.execution_block;
    assert!(tip > 0);
    rig.with_world(T0, |e, w| {
        assert_eq!(w.view.tip(), tip);
        let mut late = ev.clone();
        late.execution_block = tip.saturating_add(1);
        assert!(matches!(
            fire_param_change(e, w, &late),
            Err(TriggerError::NotExecutionBlock { .. })
        ));
    });
    let got: Vec<_> = rig.engine.candidates().collect();
    assert!(got
        .iter()
        .all(|c| matches!(c.cause, TriggerCause::ParamChange { .. })));
}
