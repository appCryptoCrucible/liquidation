//! 16D: roster fail-closed, recorded-sample p99, 13A pool seam, handshake reuse, NIC ABSENT.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use liq_obs::net_rtt::{
    bind_loopback_reuse, build_pooled_client, build_unpooled_client, nic_queue_status, ops_net_dir,
    thirteen_a_http_pool_seam, Claim, LatencyPath, NetLeg, NetRoster, NicQueueStatus, PathVerdict,
    ProbeId, RttMonitor,
};
use liq_obs::ObsError;

fn shipped_builders() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/builders.toml")
}

fn spawn_keepalive_http(accepts: Arc<AtomicU64>, stop: Arc<AtomicBool>) -> String {
    let (listener, addr) = bind_loopback_reuse().expect("socket2 loopback");
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    accepts.fetch_add(1, Ordering::SeqCst);
                    thread::spawn(move || serve_http11(stream));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
    });
    format!("http://{addr}/")
}

fn serve_http11(mut stream: TcpStream) {
    // Listener is nonblocking; accepted fds inherit that on Windows.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_nodelay(true);
    let mut hold = Vec::with_capacity(1024);
    let mut tmp = [0u8; 512];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => hold.extend_from_slice(&tmp[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
        while let Some(consumed) = take_one_request(&mut hold) {
            let _ = consumed;
            if stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                )
                .is_err()
            {
                return;
            }
            let _ = stream.flush();
        }
    }
}

fn take_one_request(buf: &mut Vec<u8>) -> Option<usize> {
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let body_at = header_end.saturating_add(4);
    let headers = std::str::from_utf8(&buf[..body_at]).ok()?;
    let mut content_len = 0usize;
    for line in headers.split("\r\n") {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_len = v.trim().parse().ok()?;
        }
    }
    let total = body_at.checked_add(content_len)?;
    if buf.len() < total {
        return None;
    }
    buf.drain(..total);
    Some(total)
}

#[test]
fn empty_builder_list_fails_closed() {
    let err = NetRoster::parse(
        r#"
builders = []
[mevshare]
relay = "https://relay.flashbots.net"
"#,
    )
    .unwrap_err();
    assert!(matches!(err, ObsError::EmptyBuilderRoster));
}

#[test]
fn empty_mevshare_fails_closed() {
    let err = NetRoster::parse(
        r#"
[[builders]]
id = 1
name = "x"
endpoint = "http://127.0.0.1:9/"
[mevshare]
relay = ""
"#,
    )
    .unwrap_err();
    assert!(matches!(err, ObsError::EmptyMevShareRelay));
}

#[test]
fn public_rpc_refused() {
    let err = NetRoster::parse(
        r#"
[[builders]]
id = 1
name = "x"
endpoint = "https://mainnet.infura.io/v3/x"
[mevshare]
relay = "https://relay.flashbots.net"
"#,
    )
    .unwrap_err();
    assert!(matches!(err, ObsError::PublicRpcForbidden));
}

#[test]
fn shipped_toml_has_every_builder_and_relay() {
    let r = NetRoster::load(&shipped_builders()).expect("shipped builders.toml");
    assert_eq!(r.builders.len(), 4);
    assert_eq!(r.mevshare_relay, "https://relay.flashbots.net");
    let names: Vec<_> = r.builders.iter().map(|b| b.name.as_str()).collect();
    assert!(names.contains(&"beaverbuild"));
    assert!(names.contains(&"rsync"));
    assert!(names.contains(&"titan"));
    assert!(names.contains(&"flashbots"));
    let mon = RttMonitor::from_roster(r).unwrap();
    let rep = mon.report();
    assert_eq!(rep.builders.len(), 4);
    for row in &rep.builders {
        assert_eq!(row.n, 0);
        assert!(row.p99_ns.is_none(), "empty window is ABSENT, not 0");
    }
    assert!(rep.mevshare.p99_ns.is_none());
    assert!(rep.p2p.p99_ns.is_none());
    assert!(rep.sse.p99_ns.is_none());
    assert!(rep.path_a.p99_ns.is_none());
    assert!(rep.path_b.p99_ns.is_none());
    assert!(rep.path_c.p99_ns.is_none());
    assert_eq!(rep.path_a.verdict, PathVerdict::Unmeasured);
    assert_eq!(rep.path_b.verdict, PathVerdict::Unmeasured);
    assert_eq!(rep.path_c.verdict, PathVerdict::Unmeasured);
    assert!(!rep.universe_full);
    assert!(!rep.invented_p99());
    assert!(!rep.render().contains("e2e"));
}

#[test]
fn p99_from_recorded_samples_not_wall_clock() {
    let roster = NetRoster::parse(
        r#"
[[builders]]
id = 1
name = "mock-a"
endpoint = "http://127.0.0.1:1/"
[[builders]]
id = 2
name = "mock-b"
endpoint = "http://127.0.0.1:2/"
[mevshare]
relay = "http://127.0.0.1:3/"
"#,
    )
    .unwrap();
    let mon = RttMonitor::from_roster(roster).unwrap();
    for ns in [10_u64, 20, 30, 40, 50] {
        mon.record_builder(1, ns).unwrap();
    }
    mon.record_builder(2, 7).unwrap();
    mon.record_mevshare(9);
    mon.record_path(LatencyPath::A, 100);
    mon.record_path(LatencyPath::B, 200);
    mon.record_net_leg(NetLeg::P2pBlockReceipt, 300);

    let a = mon.p99_builder(1).unwrap().unwrap();
    assert_eq!(a.n, 5);
    assert_eq!(
        a.p99_ns, 50,
        "nearest-rank of injected samples, not Instant"
    );
    assert!(!a.reliable);

    let b = mon.p99_builder(2).unwrap().unwrap();
    assert_eq!(b.n, 1);
    assert_eq!(b.p99_ns, 7);

    assert_eq!(mon.p99_mevshare().unwrap().p99_ns, 9);
    assert_eq!(mon.p99_path(LatencyPath::A).unwrap().p99_ns, 100);
    assert_eq!(mon.p99_path(LatencyPath::B).unwrap().p99_ns, 200);
    assert!(mon.p99_path(LatencyPath::C).is_none());
    assert_eq!(
        mon.p99_net_leg(NetLeg::P2pBlockReceipt).unwrap().p99_ns,
        300
    );
    assert!(mon.p99_net_leg(NetLeg::SseHintDelivery).is_none());

    let rep = mon.report();
    assert_eq!(rep.path_a.verdict, PathVerdict::Unmeasured);
    assert_eq!(rep.path_b.verdict, PathVerdict::Unmeasured);
    assert_eq!(rep.path_c.verdict, PathVerdict::Unmeasured);
    assert!(!rep.invented_p99());
    assert!(mon.p99_builder(99).is_err());
}

#[test]
fn thirteen_a_keepalive_prewarm_present_handshake_still_absent() {
    let s = thirteen_a_http_pool_seam();
    assert!(s.shared_client, "ExecPath stores one reqwest::Client");
    assert!(s.cloned_for_joinset, "JoinSet clones the same client");
    assert!(
        s.explicit_tcp_keepalive,
        "13A ExecPath client must set tcp_keepalive (17C)"
    );
    assert!(s.explicit_pool_idle);
    assert!(s.explicit_pool_max_idle);
    assert!(s.prewarm, "13A must name warm_http / prewarm in source");
    assert_eq!(
        s.handshake_free_critical,
        Claim::Absent,
        "keepalive/prewarm is not a measured handshake-free proof"
    );
}

#[test]
fn pooled_client_source_declares_pool_and_keepalive() {
    let src = include_str!("../src/net_rtt.rs");
    assert!(src.contains("pool_idle_timeout"));
    assert!(src.contains("pool_max_idle_per_host"));
    assert!(src.contains("tcp_keepalive"));
    assert!(src.contains("tcp_nodelay"));
    assert!(src.contains("no_proxy"));
}

#[test]
fn first_post_after_warmup_reuses_tcp_accept() {
    let accepts = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let url = spawn_keepalive_http(accepts.clone(), stop.clone());
    thread::sleep(Duration::from_millis(20));

    let roster = NetRoster::parse(&format!(
        r#"
[[builders]]
id = 1
name = "mock"
endpoint = "{url}"
[mevshare]
relay = "{url}"
"#
    ))
    .unwrap();
    let mon = RttMonitor::from_roster(roster).unwrap();
    mon.warm_http(&url).expect("warmup");
    let after_warm = accepts.load(Ordering::SeqCst);
    assert!(after_warm >= 1, "warmup must accept at least once");
    mon.probe_http(ProbeId::Builder(1), &url)
        .expect("first POST after warmup");
    let after_probe = accepts.load(Ordering::SeqCst);
    assert_eq!(
        after_probe, after_warm,
        "pooled+keepalive client must not open a new TCP socket after warmup"
    );
    stop.store(true, Ordering::Relaxed);
}

#[test]
fn unpooled_client_opens_a_new_accept_per_post() {
    let accepts = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let url = spawn_keepalive_http(accepts.clone(), stop.clone());
    thread::sleep(Duration::from_millis(20));
    let client = build_unpooled_client().unwrap();
    let body = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_chainId\",\"params\":[]}";
    client.post(&url).body(body).send().unwrap();
    let a = accepts.load(Ordering::SeqCst);
    client.post(&url).body(body).send().unwrap();
    let b = accepts.load(Ordering::SeqCst);
    assert!(b > a, "accept counter must move when the pool is disabled");
    let _ = build_pooled_client().unwrap();
    stop.store(true, Ordering::Relaxed);
}

#[test]
fn nic_sys_absent_is_absent_not_pass() {
    let status = nic_queue_status(&ops_net_dir());
    if !Path::new("/sys").is_dir() {
        assert_eq!(status, NicQueueStatus::Absent);
    } else {
        assert_ne!(
            status,
            NicQueueStatus::Pass,
            "this workspace does not ship ops/net/state.applied"
        );
    }
    let applied = ops_net_dir().join("state.applied");
    assert!(
        !applied.exists(),
        "state.applied must not be committed; verify is ABSENT/FAIL until apply"
    );
}

#[test]
fn dep_contract_liq_types_and_liq_watch_only() {
    let toml = include_str!("../Cargo.toml");
    let deps = toml
        .split("[dependencies]")
        .nth(1)
        .unwrap()
        .split("[target.")
        .next()
        .unwrap()
        .split("[dev-dependencies]")
        .next()
        .unwrap();
    let mut path_crates = Vec::new();
    for line in deps.lines() {
        let t = line.trim();
        if t.starts_with('#') || t.is_empty() {
            continue;
        }
        if t.contains("path =") {
            let name = t.split('=').next().unwrap().trim();
            path_crates.push(name);
        }
    }
    assert_eq!(
        path_crates,
        ["liq-types", "liq-watch"],
        "09A contract: adding liq-exec/engine/state creates 00→09→08"
    );
    for forbidden in [
        "liq-exec",
        "liq-engine",
        "liq-state",
        "liq-router",
        "liq-bot",
        "liq-node",
    ] {
        assert!(
            !deps.contains(forbidden),
            "liq-obs must not depend on {forbidden}"
        );
    }
}
