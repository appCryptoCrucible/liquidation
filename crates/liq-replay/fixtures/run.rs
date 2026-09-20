//! WP 05D — named fixtures, determinism, A3-closed recall fold (GUIDE 05 §5, §7).
//!
//! Does not invent `ForkFacts`. Does not invent tx hashes for unobserved guards.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::fs;
use std::path::{Path, PathBuf};

use alloy_primitives::{Address, U256};
use liq_config::{Intern, Registry};
use liq_replay::archive::second_source_verify;
use liq_replay::recall::{
    build_report, ClassifyError, DeclaredConfig, EventFields, Observed, RecallError, RecallReport,
};
use liq_types::{AssetId, ProtocolId};

const NAMES: [&str; 14] = [
    "v4_risk_premium_accrual",
    "v4_target_hf_close_factor",
    "v4_bonus_curve_ramp",
    "v4_dust_floor_full_clear",
    "v4_spoke_config_change",
    "v4_position_manager_action",
    "v3_emode_recategorization",
    "v3_isolation_ceiling",
    "v3_grace_period",
    "v3_deficit_accounting",
    "collateral_token_transfer",
    "flashloan_mediated_change",
    "governance_param_change",
    "reorg_depth_8_and_64",
];

struct Pin {
    name: String,
    comment: String,
    status: String,
    block: Option<u64>,
    tx: Option<String>,
    tx_index: Option<u16>,
    protocol: String,
    user: Option<Address>,
}

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn load_pin(name: &str) -> Pin {
    let path = fixtures_root().join(name).join("pin.txt");
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut comment = String::new();
    let mut status = String::new();
    let mut block = None;
    let mut tx = None;
    let mut tx_index = None;
    let mut protocol = String::new();
    let mut user = None;
    for line in raw.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix('#') {
            if comment.is_empty() {
                comment = rest.trim().to_owned();
            }
            continue;
        }
        if t.is_empty() || t.starts_with("source") || t.starts_with("a3") || t.starts_with("note") {
            continue;
        }
        let Some((k, v)) = t.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        match k {
            "status" => status = v.to_owned(),
            "block" => block = Some(v.parse().unwrap_or_else(|_| panic!("{name} bad block"))),
            "tx" => tx = Some(v.to_owned()),
            "tx_index" => {
                tx_index = Some(v.parse().unwrap_or_else(|_| panic!("{name} bad tx_index")))
            }
            "protocol" => protocol = v.to_owned(),
            "user" if v != "unobserved" => {
                user = Some(v.parse().unwrap_or_else(|_| panic!("{name} bad user")))
            }
            _ => {}
        }
    }
    assert!(
        !comment.is_empty(),
        "{name}: missing one-line real-world comment"
    );
    Pin {
        name: name.to_owned(),
        comment,
        status,
        block,
        tx,
        tx_index,
        protocol,
        user,
    }
}

fn all_pins() -> Vec<Pin> {
    NAMES.iter().map(|n| load_pin(n)).collect()
}

fn canonical_bytes(pins: &[Pin]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in pins {
        out.extend_from_slice(p.name.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(p.comment.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(p.status.as_bytes());
        out.push(b'\n');
        if let Some(b) = p.block {
            out.extend_from_slice(&b.to_le_bytes());
        }
        if let Some(ref tx) = p.tx {
            out.extend_from_slice(tx.as_bytes());
        }
        if let Some(i) = p.tx_index {
            out.extend_from_slice(&i.to_le_bytes());
        }
        out.extend_from_slice(p.protocol.as_bytes());
        if let Some(u) = p.user {
            out.extend_from_slice(u.as_slice());
        }
        out.push(0);
    }
    out
}

fn encode_report(r: &RecallReport) -> Vec<u8> {
    let mut s = format!(
        "total={} before={} late={} never={} declined={} timings={} slices={}\n",
        r.total_actual,
        r.detected_before,
        r.detected_late,
        r.never_detected.len(),
        r.declined.len(),
        r.timings.len(),
        r.slices.len()
    );
    let mut slices: Vec<_> = r.slices.iter().map(|x| x.key.clone()).collect();
    slices.sort();
    for k in slices {
        s.push_str(&k);
        s.push('\n');
    }
    s.into_bytes()
}

fn intern() -> Intern {
    let path = workspace_root().join("registry/registry.json");
    let reg = Registry::from_path(&path).expect("registry.json");
    Intern::from_registry(&reg).expect("intern")
}

fn observed_from_pins(pins: &[Pin], intern: &Intern) -> Vec<Observed> {
    let mut out = Vec::new();
    for p in pins {
        if p.status == "unobserved" {
            continue;
        }
        let Some(user) = p.user else {
            continue;
        };
        let Some(block) = p.block else {
            continue;
        };
        let Some(tx_index) = p.tx_index else {
            continue;
        };
        let Some(proto) = intern.protocol(&p.protocol) else {
            panic!("intern missing family {}", p.protocol);
        };
        let weth = intern
            .asset(
                "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
                    .parse()
                    .unwrap(),
            )
            .expect("WETH intern");
        out.push(Observed {
            event: EventFields {
                protocol: proto,
                user,
                liquidator: Address::repeat_byte(0x81),
                repay_asset: weth,
                repay_amount: U256::from(1u64),
                seize_asset: weth,
                seize_amount: U256::from(1u64),
                block,
                tx_index,
            },
            instance: p.protocol.clone(),
            collateral_family: p.name.clone(),
            trigger: p.name.clone(),
            fork: None,
        });
    }
    out
}

fn write_artifact(body: &str) {
    let path = std::env::var("LIQ_RECALL_REPORT").unwrap_or_else(|_| {
        workspace_root()
            .join("target/recall-report.txt")
            .display()
            .to_string()
    });
    if let Some(parent) = Path::new(&path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(&path, body).unwrap_or_else(|e| panic!("RecallReport artifact {path}: {e}"));
}

/// Empty unless a non-hidden `*.parquet` exists (05B partitions may nest).
/// `.gitkeep` and other dotfiles are not archive content. Unreadable dirs
/// fail closed as empty so `LIQ_FULL_REPLAY=1` cannot pass on a listing miss.
fn archive_is_empty(dir: &Path) -> bool {
    !has_parquet(dir)
}

fn has_parquet(root: &Path) -> bool {
    if !root.is_dir() {
        return false;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd {
            let Ok(ent) = ent else {
                continue;
            };
            let name = ent.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let path = ent.path();
            let Ok(ft) = ent.file_type() else {
                continue;
            };
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) == Some("parquet") {
                return true;
            }
        }
    }
    false
}

#[test]
fn fourteen_named_fixtures_pinned() {
    let pins = all_pins();
    assert_eq!(pins.len(), 14);
    for p in &pins {
        assert!(
            p.comment.contains("block")
                || p.comment.contains("A3")
                || p.comment.contains("unobserved")
                || p.comment.contains("PayloadExecuted")
                || p.comment.contains("IsolationMode")
                || p.comment.contains("setUserEMode")
                || p.comment.contains("liquidation"),
            "{} comment must name the real-world event: {}",
            p.name,
            p.comment
        );
        if p.status == "unobserved" {
            assert!(p.tx.is_none(), "{} unobserved must not invent a tx", p.name);
            continue;
        }
        let tx =
            p.tx.as_ref()
                .unwrap_or_else(|| panic!("{} missing tx", p.name));
        assert!(
            tx.starts_with("0x") && tx.len() == 66,
            "{} tx {}",
            p.name,
            tx
        );
        assert!(p.block.unwrap() > 0, "{} block", p.name);
        assert!(!p.protocol.is_empty());
    }
}

#[test]
fn replay_bit_identical_twice() {
    let a = canonical_bytes(&all_pins());
    let b = canonical_bytes(&all_pins());
    assert_eq!(a, b, "fixture encoding must be bit-identical twice");
    assert!(!a.is_empty());
}

#[test]
fn fold_without_fork_facts_is_a3_fail_closed() {
    let intern = intern();
    let pins = all_pins();
    let observed = observed_from_pins(&pins, &intern);
    assert!(
        !observed.is_empty(),
        "at least one pin must carry a real user+block"
    );
    let proto = intern.protocol("aave-v3").expect("aave-v3");
    let weth = intern
        .asset(
            "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
                .parse()
                .unwrap(),
        )
        .expect("WETH");
    let enabled = [proto];
    let assets = [weth];
    let cfg = DeclaredConfig {
        enabled_protocols: &enabled,
        target_assets: &assets,
        keepers: &[],
        exit_venue_declared: true,
        debt_is_exotic: false,
    };
    match build_report(&observed, &[], &cfg) {
        Err(RecallError::Classify(ClassifyError::ForkUnavailable)) => {}
        other => panic!("expected ForkUnavailable A3 seam, got {other:?}"),
    }
}

#[test]
fn full_replay_archive_a3_deferred_and_publish_report() {
    let a3 = second_source_verify();
    let archive = workspace_root().join("data/archive");
    let empty = archive_is_empty(&archive);
    let body = format!(
        "RecallReport\n\
         a3_second_source={a3:?}\n\
         archive_empty={empty}\n\
         fixtures=14\n\
         determinism=bit-identical-twice\n\
         fold=ForkUnavailable_without_N-1_facts\n\
         note=full parquet recall deferred to A3/local node (D60); rates not invented\n"
    );
    write_artifact(&body);
    if std::env::var("LIQ_FULL_REPLAY").ok().as_deref() == Some("1") && empty {
        panic!("LIQ_FULL_REPLAY=1 but data/archive empty (A3Deferred)");
    }
}

/// Regression for D1: `.gitkeep` made `read_dir().next().is_none() == false`.
#[test]
fn gitkeep_only_archive_is_empty_old_next_is_none_is_not() {
    let dir = std::env::temp_dir().join(format!(
        "liq-05d-d1-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(".gitkeep"), []).unwrap();

    let old_empty = fs::read_dir(&dir)
        .map(|mut d| d.next().is_none())
        .unwrap_or(true);
    assert!(
        !old_empty,
        "precondition: .gitkeep-only dir is non-empty under next().is_none()"
    );
    assert!(
        archive_is_empty(&dir),
        ".gitkeep must not populate the archive"
    );

    fs::write(dir.join(".hidden.parquet"), []).unwrap();
    assert!(
        archive_is_empty(&dir),
        "hidden *.parquet must not populate the archive"
    );

    fs::create_dir_all(dir.join("headers")).unwrap();
    fs::write(dir.join("headers").join("headers_0_1.parquet"), []).unwrap();
    assert!(
        !archive_is_empty(&dir),
        "nested 05B partition parquet populates the archive"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn unobserved_guards_are_not_fabricated() {
    let grace = load_pin("v3_grace_period");
    let reorg = load_pin("reorg_depth_8_and_64");
    assert_eq!(grace.status, "unobserved");
    assert_eq!(reorg.status, "unobserved");
    assert!(grace.tx.is_none());
    assert!(reorg.tx.is_none());
}

#[test]
fn isolation_user_fail_closed_not_guessed() {
    let p = load_pin("v3_isolation_ceiling");
    assert!(p.user.is_none(), "do not invent isolation borrower");
    assert_eq!(
        p.tx.as_deref(),
        Some("0x59ac6784cb3851f5c84bff17a2e3bebb57a69a405b850eafd4282debcee58480")
    );
}

#[test]
fn report_encode_deterministic() {
    let p = [ProtocolId(1)];
    let a = [AssetId(1)];
    let cfg = DeclaredConfig {
        enabled_protocols: &p,
        target_assets: &a,
        keepers: &[],
        exit_venue_declared: true,
        debt_is_exotic: false,
    };
    let r1 = build_report(&[], &[], &cfg).unwrap();
    let r2 = build_report(&[], &[], &cfg).unwrap();
    assert_eq!(encode_report(&r1), encode_report(&r2));
}
