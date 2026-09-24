use super::*;
use alloy_primitives::{Address, U256};
use liq_types::{AssetId, ProtocolId};
use liq_watch::types::PositionKeyDto;
use liq_watch::ActualLiquidation;

fn cfg<'a>(
    protos: &'a [ProtocolId],
    assets: &'a [AssetId],
    keepers: &'a [Address],
) -> DeclaredConfig<'a> {
    DeclaredConfig {
        enabled_protocols: protos,
        target_assets: assets,
        keepers,
        exit_venue_declared: true,
        debt_is_exotic: false,
    }
}

fn ev(proto: u16, user: Address, liq: Address, repay: u16, seize: u16) -> EventFields {
    EventFields {
        protocol: ProtocolId(proto),
        user,
        liquidator: liq,
        repay_asset: AssetId(repay),
        repay_amount: U256::from(1u64),
        seize_asset: AssetId(seize),
        seize_amount: U256::from(1u64),
        block: 10,
        tx_index: 5,
    }
}

fn fork_in_scope() -> ForkFacts {
    ForkFacts {
        flash_available_at_size: true,
        exit_quote_exists: true,
        gas_exceeds_bonus: false,
        size_exceeds_routing: false,
        executor_realized_wei: 1,
    }
}

/// TESTING.md §4 mutation #13 — design check (review item).
#[test]
fn mutation_13_classifier_imports_neither_state_flash_nor_router() {
    // A `pub(crate) use` in `mod.rs` plus `super::` in the classifier
    // would not show up in `classifier.rs` alone.
    let files = [
        ("classifier.rs", include_str!("classifier.rs")),
        ("mod.rs", include_str!("mod.rs")),
    ];
    for needle in [
        "liq_state",
        "liq-state",
        "liq_flash",
        "liq-flash",
        "liq_router",
        "liq-router",
    ] {
        for (name, src) in files {
            assert!(
                !src.contains(needle),
                "{name} must not mention {needle} (mutation #13)"
            );
        }
    }
}

#[test]
fn miss_config_only_no_fork() {
    let p = [ProtocolId(1)];
    let a = [AssetId(1), AssetId(2)];
    let k = [];
    let c = cfg(&p, &a, &k);
    let u = Address::repeat_byte(1);
    let miss_proto = ev(9, u, Address::repeat_byte(2), 1, 2);
    assert_eq!(
        classify_miss(&miss_proto, &c, None).unwrap(),
        MissClass::OutOfScopeProtocol
    );
    let miss_asset = ev(1, u, Address::repeat_byte(2), 9, 2);
    assert_eq!(
        classify_miss(&miss_asset, &c, None).unwrap(),
        MissClass::OutOfScopeAsset
    );
    let self_liq = ev(1, u, u, 1, 2);
    assert_eq!(
        classify_miss(&self_liq, &c, None).unwrap(),
        MissClass::SelfOrKeeper
    );
}

#[test]
fn miss_fork_required_fail_closed() {
    let p = [ProtocolId(1)];
    let a = [AssetId(1), AssetId(2)];
    let k = [];
    let c = cfg(&p, &a, &k);
    let e = ev(1, Address::repeat_byte(1), Address::repeat_byte(2), 1, 2);
    assert_eq!(
        classify_miss(&e, &c, None),
        Err(ClassifyError::ForkUnavailable)
    );
    assert_eq!(require_fork(None), Err(ClassifyError::ForkUnavailable));
}

#[test]
fn miss_band_and_in_scope() {
    let p = [ProtocolId(1)];
    let a = [AssetId(1), AssetId(2)];
    let k = [];
    let c = cfg(&p, &a, &k);
    let e = ev(1, Address::repeat_byte(1), Address::repeat_byte(2), 1, 2);
    let mut f = fork_in_scope();
    f.gas_exceeds_bonus = true;
    assert_eq!(
        classify_miss(&e, &c, Some(f)).unwrap(),
        MissClass::BelowBand
    );
    f.gas_exceeds_bonus = false;
    f.size_exceeds_routing = true;
    assert_eq!(
        classify_miss(&e, &c, Some(f)).unwrap(),
        MissClass::AboveBand
    );
    f.size_exceeds_routing = false;
    f.flash_available_at_size = false;
    assert_eq!(
        classify_miss(&e, &c, Some(f)).unwrap(),
        MissClass::NotFlashloanable
    );
    f.flash_available_at_size = true;
    f.executor_realized_wei = 0;
    assert_eq!(
        classify_miss(&e, &c, Some(f)).unwrap(),
        MissClass::ExecutorUnprofitable
    );
    f.executor_realized_wei = 1;
    assert_eq!(classify_miss(&e, &c, Some(f)).unwrap(), MissClass::InScope);
}

#[test]
fn decline_classes_split_band() {
    assert_eq!(DeclineReason::BelowBand.class(), DeclineClass::Market);
    assert_eq!(DeclineReason::AboveBand.class(), DeclineClass::System);
    assert_eq!(
        DeclineReason::ProtocolNotEnabled.class(),
        DeclineClass::Known
    );
    assert_eq!(
        DeclineReason::DebtNotFlashloanable { exotic: false }.class(),
        DeclineClass::System
    );
    assert_eq!(
        DeclineReason::DebtNotFlashloanable { exotic: true }.class(),
        DeclineClass::Market
    );
    assert_eq!(
        DeclineReason::NoCollateralExit { registry_gap: true }.class(),
        DeclineClass::System
    );
    assert_eq!(
        DeclineReason::NoCollateralExit {
            registry_gap: false
        }
        .class(),
        DeclineClass::Market
    );
}

#[test]
fn timing_block_and_intra() {
    let e = ev(1, Address::repeat_byte(1), Address::repeat_byte(2), 1, 2);
    let hit = DetectionHit {
        protocol: ProtocolId(1),
        user: e.user,
        first_block: 8,
        first_tx_index: 0,
    };
    let t = timing(&e, &hit).unwrap();
    assert_eq!(t.block_delta, 2);
    assert_eq!(t.intra_block_delta, None);
    let same = DetectionHit {
        first_block: 10,
        first_tx_index: 2,
        ..hit
    };
    let t2 = timing(&e, &same).unwrap();
    assert_eq!(t2.block_delta, 0);
    assert_eq!(t2.intra_block_delta, Some(3));
}

#[test]
fn coverage_markdown_matches_state_header() {
    let md = matrix_markdown(&uncovered_matrix());
    assert!(md.starts_with(MATRIX_HEADER));
    assert!(md.contains(ROW_INSTANCE));
    assert!(md.contains(ROW_VOL));
    assert!(md.contains("`uncovered`"));
    assert_eq!(
        in_scope_gate(0, 10),
        InScopeGate::Insufficient { in_scope: 10 }
    );
    assert_eq!(
        in_scope_gate(15, 50),
        InScopeGate::Pass {
            misses: 15,
            in_scope: 50
        }
    );
    assert_eq!(
        in_scope_gate(16, 50),
        InScopeGate::Fail {
            misses: 16,
            in_scope: 50
        }
    );
}

#[test]
fn report_empty_is_zero_not_fabricated() {
    let p = [ProtocolId(1)];
    let a = [AssetId(1)];
    let k = [];
    let c = cfg(&p, &a, &k);
    let r = build_report(&[], &[], &c).unwrap();
    assert_eq!(r.total_actual, 0);
    assert_eq!(r.detected_before, 0);
    assert!(matches!(
        r.gate(),
        InScopeGate::Insufficient { in_scope: 0 }
    ));
}

#[test]
fn report_miss_and_decline_and_before() {
    let p = [ProtocolId(1)];
    let a = [AssetId(1), AssetId(2)];
    let k = [];
    let c = cfg(&p, &a, &k);
    let u = Address::repeat_byte(3);
    let e_miss = ev(1, u, Address::repeat_byte(4), 1, 2);
    let e_hit = EventFields {
        user: Address::repeat_byte(5),
        block: 20,
        ..e_miss
    };
    let e_dec = EventFields {
        protocol: ProtocolId(9),
        user: Address::repeat_byte(6),
        ..e_miss
    };
    let obs = [
        Observed {
            event: e_miss,
            instance: "aave-v3:core".into(),
            collateral_family: "lst".into(),
            trigger: "oracle_public".into(),
            fork: Some(fork_in_scope()),
        },
        Observed {
            event: e_hit,
            instance: "aave-v3:core".into(),
            collateral_family: "volatile".into(),
            trigger: "user_action".into(),
            fork: Some(fork_in_scope()),
        },
        Observed {
            event: e_dec,
            instance: "off".into(),
            collateral_family: "stable".into(),
            trigger: "unobserved".into(),
            fork: None,
        },
    ];
    let hits = [
        DetectionHit {
            protocol: e_hit.protocol,
            user: e_hit.user,
            first_block: 19,
            first_tx_index: 0,
        },
        DetectionHit {
            protocol: e_dec.protocol,
            user: e_dec.user,
            first_block: 9,
            first_tx_index: 0,
        },
    ];
    let r = build_report(&obs, &hits, &c).unwrap();
    assert_eq!(r.total_actual, 3);
    assert_eq!(r.detected_before, 1);
    assert_eq!(r.never_detected.len(), 1);
    assert_eq!(r.never_detected[0].1, MissClass::InScope);
    assert_eq!(r.declined.len(), 1);
    assert_eq!(r.declined[0].1, DeclineReason::ProtocolNotEnabled);
    assert!(r.slices.iter().any(|s| s.key == "lst" && s.all_missed()));
    assert!(r
        .slices
        .iter()
        .any(|s| s.key == "volatile" && s.in_scope == 1 && s.in_scope_misses == 0));
}

#[test]
fn event_from_actual_parses_or_fails() {
    let row = ActualLiquidation {
        block: 1,
        tx_index: 0,
        position: PositionKeyDto {
            protocol: 1,
            market: 0,
            user: Address::repeat_byte(1),
        },
        liquidator: Address::repeat_byte(2),
        repay_asset: 1,
        repay_amount: "100".into(),
        seize_asset: 2,
        seize_amount: "not-a-number".into(),
        inferred_bid: None,
        oracle_backrun: None,
    };
    assert!(matches!(
        event_from_actual(&row),
        Err(RecallError::Amount(_))
    ));
}
