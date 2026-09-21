//! GUIDE 16 §4b / §5 — network RTT series and 13A pool seam.
//!
//! Empty window → [`None`] / ABSENT, never a fabricated 0. Paths A / B / C
//! are separate series; so are the three network legs. Per-builder and
//! MEV-Share relay windows are also separate. No rolled-up "e2e" number.
//!
//! Path A/B/C verdicts are never `Pass` from [`RttMonitor::report`]: this
//! crate does not have the full loaded universe (16C forbids that PASS).

use std::fs;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::Duration;

use parking_lot::Mutex;
use serde::Deserialize;
use socket2::{Domain, Protocol, Socket, Type};

use crate::error::{ObsError, Result};

/// GUIDE 16 §4b Path A (ExEx → signed bundle on the wire), nanoseconds.
pub const PATH_A_BUDGET_NS: u64 = 2_800_000;
/// Path B (SSE byte → `mev_sendBundle` sent).
pub const PATH_B_BUDGET_NS: u64 = 2_000_000;
/// Path C (CEX tick → pre-warmed plan). Soft budget.
pub const PATH_C_BUDGET_NS: u64 = 10_000_000;

/// Sliding window capacity. p99 is over these samples, not an invented mean.
pub const WINDOW_CAP: usize = 8192;

/// p99 is a distinct 99th rank only at n ≥ 100 (same honesty as 16C's p99.9 ≥ 1000).
pub const P99_RELIABLE_N: usize = 100;

const CLIENT_TIMEOUT: Duration = Duration::from_secs(3);
const POOL_IDLE: Duration = Duration::from_secs(90);
const TCP_KEEPALIVE: Duration = Duration::from_secs(10);
const POOL_MAX_IDLE_PER_HOST: usize = 8;

/// Nearest-rank percentile. `permille` is 500 (p50), 990 (p99), 999 (p99.9).
///
/// Rank = max(1, ceil(n · permille / 1000)). Returns `None` when `sorted` is
/// empty or `permille > 1000`. Does not interpolate (interpolation invents a
/// nanosecond that was never observed). Same formula as 16C.
#[must_use]
pub fn percentile_nearest_rank(sorted: &[u64], permille: u32) -> Option<u64> {
    if sorted.is_empty() || permille > 1000 {
        return None;
    }
    let n = u128::try_from(sorted.len()).ok()?;
    let p = u128::from(permille);
    let prod = n.checked_mul(p)?;
    let rank = prod.div_ceil(1000).max(1);
    let idx = usize::try_from(rank.saturating_sub(1)).ok()?;
    sorted.get(idx).copied()
}

#[must_use]
pub const fn p99_reliable(n: usize) -> bool {
    n >= P99_RELIABLE_N
}

/// Observed nearest-rank p99. `p99_ns` is a sample that existed in the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RankP99 {
    pub n: usize,
    pub p99_ns: u64,
    pub reliable: bool,
}

#[derive(Clone, Debug)]
struct SampleWindow {
    raw: Vec<u64>,
    head: usize,
    dropped: u64,
    cap: usize,
}

impl SampleWindow {
    fn with_capacity(cap: usize) -> Self {
        Self {
            raw: Vec::with_capacity(cap),
            head: 0,
            dropped: 0,
            cap,
        }
    }

    fn record(&mut self, ns: u64) {
        if self.cap == 0 {
            return;
        }
        if self.raw.len() < self.cap {
            self.raw.push(ns);
            return;
        }
        if let Some(slot) = self.raw.get_mut(self.head) {
            *slot = ns;
        }
        let next = self.head.saturating_add(1);
        self.head = if next >= self.cap { 0 } else { next };
        self.dropped = self.dropped.saturating_add(1);
        metrics::counter!("liq_net_rtt_window_dropped").increment(1);
    }

    fn p99(&self) -> Option<RankP99> {
        if self.raw.is_empty() {
            return None;
        }
        let mut v = self.raw.clone();
        v.sort_unstable();
        let p99_ns = percentile_nearest_rank(&v, 990)?;
        Some(RankP99 {
            n: v.len(),
            p99_ns,
            reliable: p99_reliable(v.len()),
        })
    }
}

/// Software latency paths (GUIDE 16 §4b). Never merged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LatencyPath {
    /// ExEx notification → signed bundle on the wire.
    A,
    /// MEV-Share SSE byte arrival → `mev_sendBundle` sent.
    B,
    /// CEX tick → pre-warmed plan ready.
    C,
}

/// Network legs (GUIDE 16 §4b). Tuned by geography, not code. Never merged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NetLeg {
    P2pBlockReceipt,
    SseHintDelivery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathVerdict {
    Unmeasured,
    Pass,
    OverBudget,
}

/// Same gate as 16C: PASS requires a reliable percentile **and** the full
/// loaded universe. 16D never supplies that universe.
#[must_use]
pub fn path_verdict(p99: Option<RankP99>, universe_full: bool, budget_ns: u64) -> PathVerdict {
    match p99 {
        Some(r) if r.reliable && universe_full => {
            if r.p99_ns <= budget_ns {
                PathVerdict::Pass
            } else {
                PathVerdict::OverBudget
            }
        }
        _ => PathVerdict::Unmeasured,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuilderTarget {
    pub id: u16,
    pub name: String,
    pub endpoint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetRoster {
    pub builders: Vec<BuilderTarget>,
    pub mevshare_relay: String,
}

#[derive(Deserialize)]
struct File {
    builders: Vec<Row>,
    mevshare: MevShareRow,
}

#[derive(Deserialize)]
struct Row {
    id: u16,
    name: String,
    endpoint: String,
}

#[derive(Deserialize)]
struct MevShareRow {
    relay: String,
}

/// Align with `liq_exec::builders::reject_public_rpc` (13A, read-only).
fn reject_public_rpc(url: &str) -> Result<()> {
    let l = url.to_ascii_lowercase();
    const FORBIDDEN: &[&str] = &[
        "infura.io",
        "alchemy.com",
        "llamarpc.com",
        "publicnode.com",
        "cloudflare-eth.com",
        "eth_sendraw",
        "sendprivatetransaction",
        "sendrawtransaction",
    ];
    for needle in FORBIDDEN {
        if l.contains(needle) {
            tracing::error!(url, needle, "refusing public-RPC / mempool submit URL");
            return Err(ObsError::PublicRpcForbidden);
        }
    }
    Ok(())
}

impl NetRoster {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(|e| ObsError::BuildersToml(e.to_string()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: File = toml::from_str(text).map_err(|e| ObsError::BuildersToml(e.to_string()))?;
        if file.builders.is_empty() {
            tracing::error!("builders.toml builder list is empty");
            return Err(ObsError::EmptyBuilderRoster);
        }
        if file.mevshare.relay.is_empty() {
            tracing::error!("builders.toml mevshare.relay is empty");
            return Err(ObsError::EmptyMevShareRelay);
        }
        reject_public_rpc(&file.mevshare.relay)?;
        let mut builders = Vec::with_capacity(file.builders.len());
        let mut seen = Vec::with_capacity(file.builders.len());
        for row in file.builders {
            reject_public_rpc(&row.endpoint)?;
            if seen.contains(&row.id) {
                return Err(ObsError::DuplicateBuilderId(row.id));
            }
            seen.push(row.id);
            builders.push(BuilderTarget {
                id: row.id,
                name: row.name,
                endpoint: row.endpoint,
            });
        }
        Ok(Self {
            builders,
            mevshare_relay: file.mevshare.relay,
        })
    }
}

/// Persistent reqwest client: default hyper pool + explicit idle + keepalive.
///
/// This is the 16D monitor client. 13A's submit client is a separate object
/// (see [`thirteen_a_http_pool_seam`]); 16D does not edit 13A.
pub fn build_pooled_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(CLIENT_TIMEOUT)
        .pool_idle_timeout(POOL_IDLE)
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
        .tcp_keepalive(TCP_KEEPALIVE)
        .tcp_nodelay(true)
        .no_proxy()
        .build()
        .map_err(|e| {
            tracing::error!(error = %e, "pooled reqwest client build failed");
            ObsError::HttpClient(e.to_string())
        })
}

/// Client that will not keep idle sockets — handshake-counter negative control.
pub fn build_unpooled_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(CLIENT_TIMEOUT)
        .pool_max_idle_per_host(0)
        .tcp_nodelay(true)
        .no_proxy()
        .build()
        .map_err(|e| {
            tracing::error!(error = %e, "unpooled reqwest client build failed");
            ObsError::HttpClient(e.to_string())
        })
}

/// Loopback TCP listen via socket2 (`SO_REUSEADDR`). Used by the handshake test.
pub fn bind_loopback_reuse() -> Result<(TcpListener, SocketAddr)> {
    let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).map_err(|e| {
        tracing::error!(error = %e, "socket2 TCP socket failed");
        ObsError::Io(e.to_string())
    })?;
    sock.set_reuse_address(true)
        .map_err(|e| ObsError::Io(e.to_string()))?;
    sock.set_tcp_nodelay(true)
        .map_err(|e| ObsError::Io(e.to_string()))?;
    let any = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0));
    sock.bind(&any.into())
        .map_err(|e| ObsError::Io(e.to_string()))?;
    sock.listen(128).map_err(|e| ObsError::Io(e.to_string()))?;
    let listener: TcpListener = sock.into();
    listener
        .set_nonblocking(true)
        .map_err(|e| ObsError::Io(e.to_string()))?;
    let addr = listener
        .local_addr()
        .map_err(|e| ObsError::Io(e.to_string()))?;
    Ok((listener, addr))
}

struct Inner {
    builders: Vec<(u16, SampleWindow)>,
    mevshare: SampleWindow,
    path_a: SampleWindow,
    path_b: SampleWindow,
    path_c: SampleWindow,
    p2p: SampleWindow,
    sse: SampleWindow,
}

impl Inner {
    fn new(roster: &NetRoster) -> Self {
        let builders = roster
            .builders
            .iter()
            .map(|b| (b.id, SampleWindow::with_capacity(WINDOW_CAP)))
            .collect();
        Self {
            builders,
            mevshare: SampleWindow::with_capacity(WINDOW_CAP),
            path_a: SampleWindow::with_capacity(WINDOW_CAP),
            path_b: SampleWindow::with_capacity(WINDOW_CAP),
            path_c: SampleWindow::with_capacity(WINDOW_CAP),
            p2p: SampleWindow::with_capacity(WINDOW_CAP),
            sse: SampleWindow::with_capacity(WINDOW_CAP),
        }
    }

    fn builder_window(&mut self, id: u16) -> Result<&mut SampleWindow> {
        self.builders
            .iter_mut()
            .find(|(i, _)| *i == id)
            .map(|(_, w)| w)
            .ok_or_else(|| ObsError::UnknownTarget(id.to_string()))
    }

    fn builder_p99(&self, id: u16) -> Result<Option<RankP99>> {
        self.builders
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, w)| w.p99())
            .ok_or_else(|| ObsError::UnknownTarget(id.to_string()))
    }
}

/// RTT monitor: one window per builder, one for the MEV-Share relay, plus
/// Path A/B/C and the p2p / SSE legs. Submit RTT is the per-target windows.
pub struct RttMonitor {
    client: reqwest::blocking::Client,
    roster: NetRoster,
    inner: Mutex<Inner>,
}

impl RttMonitor {
    pub fn from_builders_toml(path: &Path) -> Result<Self> {
        Self::from_roster(NetRoster::load(path)?)
    }

    pub fn from_roster(roster: NetRoster) -> Result<Self> {
        if roster.builders.is_empty() {
            return Err(ObsError::EmptyBuilderRoster);
        }
        let client = build_pooled_client()?;
        let inner = Inner::new(&roster);
        Ok(Self {
            client,
            roster,
            inner: Mutex::new(inner),
        })
    }

    #[must_use]
    pub fn roster(&self) -> &NetRoster {
        &self.roster
    }

    #[must_use]
    pub fn client(&self) -> &reqwest::blocking::Client {
        &self.client
    }

    /// This box does not have the full protocol universe loaded.
    #[must_use]
    pub const fn universe_full() -> bool {
        false
    }

    pub fn record_builder(&self, id: u16, ns: u64) -> Result<()> {
        let mut g = self.inner.lock();
        g.builder_window(id)?.record(ns);
        metrics::counter!("liq_net_rtt_samples", "series" => "builder").increment(1);
        Ok(())
    }

    pub fn record_mevshare(&self, ns: u64) {
        self.inner.lock().mevshare.record(ns);
        metrics::counter!("liq_net_rtt_samples", "series" => "mevshare").increment(1);
    }

    pub fn record_path(&self, path: LatencyPath, ns: u64) {
        let mut g = self.inner.lock();
        match path {
            LatencyPath::A => g.path_a.record(ns),
            LatencyPath::B => g.path_b.record(ns),
            LatencyPath::C => g.path_c.record(ns),
        }
        metrics::counter!("liq_net_rtt_samples", "series" => "path").increment(1);
    }

    pub fn record_net_leg(&self, leg: NetLeg, ns: u64) {
        let mut g = self.inner.lock();
        match leg {
            NetLeg::P2pBlockReceipt => g.p2p.record(ns),
            NetLeg::SseHintDelivery => g.sse.record(ns),
        }
        metrics::counter!("liq_net_rtt_samples", "series" => "net_leg").increment(1);
    }

    pub fn p99_builder(&self, id: u16) -> Result<Option<RankP99>> {
        self.inner.lock().builder_p99(id)
    }

    pub fn p99_mevshare(&self) -> Option<RankP99> {
        self.inner.lock().mevshare.p99()
    }

    pub fn p99_path(&self, path: LatencyPath) -> Option<RankP99> {
        let g = self.inner.lock();
        match path {
            LatencyPath::A => g.path_a.p99(),
            LatencyPath::B => g.path_b.p99(),
            LatencyPath::C => g.path_c.p99(),
        }
    }

    pub fn p99_net_leg(&self, leg: NetLeg) -> Option<RankP99> {
        let g = self.inner.lock();
        match leg {
            NetLeg::P2pBlockReceipt => g.p2p.p99(),
            NetLeg::SseHintDelivery => g.sse.p99(),
        }
    }

    /// POST to establish the idle socket. **Not** recorded — handshake RTT
    /// must not enter the p99 window.
    pub fn warm_http(&self, url: &str) -> Result<()> {
        reject_public_rpc(url)?;
        let resp = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .body("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_chainId\",\"params\":[]}")
            .send()
            .map_err(|e| {
                tracing::error!(error = %e, url, "warmup POST failed");
                ObsError::HttpClient(e.to_string())
            })?;
        let _status = resp.status();
        resp.bytes().map_err(|e| {
            tracing::error!(error = %e, url, "warmup body read failed");
            ObsError::HttpClient(e.to_string())
        })?;
        Ok(())
    }

    /// POST after warmup. Records wall-clock of **this** request only.
    pub fn probe_http(&self, id: ProbeId, url: &str) -> Result<u64> {
        reject_public_rpc(url)?;
        let start = std::time::Instant::now();
        let resp = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .body("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_chainId\",\"params\":[]}")
            .send()
            .map_err(|e| {
                tracing::error!(error = %e, url, "probe POST failed");
                ObsError::HttpClient(e.to_string())
            })?;
        let _status = resp.status();
        resp.bytes().map_err(|e| {
            tracing::error!(error = %e, url, "probe body read failed");
            ObsError::HttpClient(e.to_string())
        })?;
        let ns = u64::try_from(start.elapsed().as_nanos()).map_err(|_| {
            tracing::error!("probe duration does not fit u64 ns");
            ObsError::HttpClient("duration overflow".into())
        })?;
        match id {
            ProbeId::Builder(b) => self.record_builder(b, ns)?,
            ProbeId::MevShare => self.record_mevshare(ns),
        }
        Ok(ns)
    }

    /// Snapshot. Path verdicts are Unmeasured (`universe_full == false`).
    #[must_use]
    pub fn report(&self) -> NetReport {
        let g = self.inner.lock();
        let builders = self
            .roster
            .builders
            .iter()
            .map(|t| {
                let p99 = g
                    .builders
                    .iter()
                    .find(|(id, _)| *id == t.id)
                    .and_then(|(_, w)| w.p99());
                SeriesRow {
                    name: format!("builder.{}:{}", t.id, t.name),
                    n: p99.map(|r| r.n).unwrap_or(0),
                    p99_ns: p99.map(|r| r.p99_ns),
                    reliable: p99.map(|r| r.reliable).unwrap_or(false),
                }
            })
            .collect();
        let ms = g.mevshare.p99();
        let pa = g.path_a.p99();
        let pb = g.path_b.p99();
        let pc = g.path_c.p99();
        let p2p = g.p2p.p99();
        let sse = g.sse.p99();
        drop(g);
        NetReport {
            builders,
            mevshare: SeriesRow {
                name: "mevshare.relay".into(),
                n: ms.map(|r| r.n).unwrap_or(0),
                p99_ns: ms.map(|r| r.p99_ns),
                reliable: ms.map(|r| r.reliable).unwrap_or(false),
            },
            path_a: PathRow {
                id: "A",
                budget_ns: PATH_A_BUDGET_NS,
                n: pa.map(|r| r.n).unwrap_or(0),
                p99_ns: pa.map(|r| r.p99_ns),
                verdict: path_verdict(pa, Self::universe_full(), PATH_A_BUDGET_NS),
            },
            path_b: PathRow {
                id: "B",
                budget_ns: PATH_B_BUDGET_NS,
                n: pb.map(|r| r.n).unwrap_or(0),
                p99_ns: pb.map(|r| r.p99_ns),
                verdict: path_verdict(pb, Self::universe_full(), PATH_B_BUDGET_NS),
            },
            path_c: PathRow {
                id: "C",
                budget_ns: PATH_C_BUDGET_NS,
                n: pc.map(|r| r.n).unwrap_or(0),
                p99_ns: pc.map(|r| r.p99_ns),
                verdict: path_verdict(pc, Self::universe_full(), PATH_C_BUDGET_NS),
            },
            p2p: SeriesRow {
                name: "net.p2p_block_receipt".into(),
                n: p2p.map(|r| r.n).unwrap_or(0),
                p99_ns: p2p.map(|r| r.p99_ns),
                reliable: p2p.map(|r| r.reliable).unwrap_or(false),
            },
            sse: SeriesRow {
                name: "net.sse_hint_delivery".into(),
                n: sse.map(|r| r.n).unwrap_or(0),
                p99_ns: sse.map(|r| r.p99_ns),
                reliable: sse.map(|r| r.reliable).unwrap_or(false),
            },
            thirteen_a: thirteen_a_http_pool_seam(),
            nic: nic_queue_status(&ops_net_dir()),
            universe_full: Self::universe_full(),
        }
    }
}

/// Which per-target window a probe writes. Not a combined series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeId {
    Builder(u16),
    MevShare,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesRow {
    pub name: String,
    pub n: usize,
    pub p99_ns: Option<u64>,
    pub reliable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathRow {
    pub id: &'static str,
    pub budget_ns: u64,
    pub n: usize,
    pub p99_ns: Option<u64>,
    pub verdict: PathVerdict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetReport {
    pub builders: Vec<SeriesRow>,
    pub mevshare: SeriesRow,
    pub path_a: PathRow,
    pub path_b: PathRow,
    pub path_c: PathRow,
    pub p2p: SeriesRow,
    pub sse: SeriesRow,
    pub thirteen_a: ThirteenAHttpPool,
    pub nic: NicQueueStatus,
    pub universe_full: bool,
}

impl NetReport {
    /// `n == 0` with a numeric p99 is a fabricated cell.
    #[must_use]
    pub fn invented_p99(&self) -> bool {
        let invented_series = self.builders.iter().any(|r| r.n == 0 && r.p99_ns.is_some())
            || (self.mevshare.n == 0 && self.mevshare.p99_ns.is_some())
            || (self.p2p.n == 0 && self.p2p.p99_ns.is_some())
            || (self.sse.n == 0 && self.sse.p99_ns.is_some());
        if invented_series {
            return true;
        }
        for p in [&self.path_a, &self.path_b, &self.path_c] {
            if p.n == 0 && p.p99_ns.is_some() {
                return true;
            }
            if matches!(p.verdict, PathVerdict::Pass) && !self.universe_full {
                return true;
            }
        }
        false
    }

    #[must_use]
    pub fn render(&self) -> String {
        let mut s = String::from("# 16D network series (nearest-rank p99; empty = ABSENT)\n");
        s.push_str(&format!("universe_full={}\n", self.universe_full));
        for p in [&self.path_a, &self.path_b, &self.path_c] {
            s.push_str(&format!(
                "path.{}.p99_ns={}\npath.{}.n={}\npath.{}.verdict={}\n",
                p.id,
                cell(p.p99_ns),
                p.id,
                p.n,
                p.id,
                verdict_tag(p.verdict),
            ));
        }
        s.push_str(&format!(
            "{}.p99_ns={}\n{}.n={}\n",
            self.p2p.name,
            cell(self.p2p.p99_ns),
            self.p2p.name,
            self.p2p.n
        ));
        s.push_str(&format!(
            "{}.p99_ns={}\n{}.n={}\n",
            self.sse.name,
            cell(self.sse.p99_ns),
            self.sse.name,
            self.sse.n
        ));
        for b in &self.builders {
            s.push_str(&format!(
                "{}.p99_ns={}\n{}.n={}\n",
                b.name,
                cell(b.p99_ns),
                b.name,
                b.n
            ));
        }
        s.push_str(&format!(
            "{}.p99_ns={}\n{}.n={}\n",
            self.mevshare.name,
            cell(self.mevshare.p99_ns),
            self.mevshare.name,
            self.mevshare.n
        ));
        s.push_str(&format!(
            "thirteen_a.shared_client={}\nthirteen_a.cloned_for_fanout={}\nthirteen_a.tcp_keepalive={}\nthirteen_a.prewarm={}\nthirteen_a.handshake_free_critical={}\n",
            yn(self.thirteen_a.shared_client),
            yn(self.thirteen_a.cloned_for_joinset),
            absent_or_yes(self.thirteen_a.explicit_tcp_keepalive),
            absent_or_yes(self.thirteen_a.prewarm),
            self.thirteen_a.handshake_free_critical.as_str(),
        ));
        s.push_str(&format!("nic.queues={}\n", self.nic.as_str()));
        s
    }
}

fn cell(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "ABSENT".into(),
    }
}

fn verdict_tag(v: PathVerdict) -> &'static str {
    match v {
        PathVerdict::Unmeasured => "UNMEASURED",
        PathVerdict::Pass => "PASS",
        PathVerdict::OverBudget => "OVER_BUDGET",
    }
}

fn yn(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}

fn absent_or_yes(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "ABSENT"
    }
}

/// What 13A actually ships (source inspection, read-only). Not a live handshake proof.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThirteenAHttpPool {
    pub shared_client: bool,
    pub cloned_for_joinset: bool,
    pub explicit_tcp_keepalive: bool,
    pub explicit_pool_idle: bool,
    pub explicit_pool_max_idle: bool,
    pub prewarm: bool,
    pub handshake_free_critical: Claim,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    /// Proven by a test against **this** crate's pooled client + accept counter.
    ProvenOnMonitorClient,
    /// 13A: keepalive/prewarm may be present on the submit client; this
    /// claim stays Absent until a handshake counter is measured on **that**
    /// object (16D monitor proof is not 13A proof).
    Absent,
}

impl Claim {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProvenOnMonitorClient => "PROVEN_MONITOR_CLIENT",
            Self::Absent => "ABSENT",
        }
    }
}

fn prod_src(text: &str) -> &str {
    text.split("#[cfg(test)]").next().unwrap_or(text)
}

/// Read 13A `path.rs` / `submit.rs`. Does not modify them.
#[must_use]
pub fn thirteen_a_http_pool_seam() -> ThirteenAHttpPool {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../liq-exec/src");
    let path_src = fs::read_to_string(root.join("path.rs")).unwrap_or_default();
    let submit_src = fs::read_to_string(root.join("submit.rs")).unwrap_or_default();
    let path = prod_src(&path_src);
    let submit = prod_src(&submit_src);
    let shared_client = path.contains("reqwest::Client::builder()")
        && path.contains("pub http: reqwest::Client")
        && path.contains(".timeout(");
    let cloned_for_joinset =
        submit.contains("let client = client.clone()") && submit.contains("set.spawn");
    let both = format!("{path}\n{submit}");
    let explicit_tcp_keepalive = both.contains("tcp_keepalive");
    let explicit_pool_idle = both.contains("pool_idle_timeout");
    let explicit_pool_max_idle = both.contains("pool_max_idle_per_host");
    let prewarm =
        both.contains("warm_http") || both.contains("prewarm") || both.contains("pre_warm");
    ThirteenAHttpPool {
        shared_client,
        cloned_for_joinset,
        explicit_tcp_keepalive,
        explicit_pool_idle,
        explicit_pool_max_idle,
        prewarm,
        handshake_free_critical: Claim::Absent,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NicQueueStatus {
    /// `/sys` missing (this Windows box). Not a pass.
    Absent,
    /// Linux sysfs present but queues / `state.applied` not in place.
    Fail,
    /// Applied marker + sysfs match. Never claimed without those files.
    Pass,
}

impl NicQueueStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "ABSENT",
            Self::Fail => "FAIL",
            Self::Pass => "PASS",
        }
    }
}

#[must_use]
pub fn ops_net_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ops/net")
}

/// Fail closed: no `/sys` → ABSENT. No `state.applied` on Linux → FAIL. Never
/// invent PASS.
#[must_use]
pub fn nic_queue_status(ops_net: &Path) -> NicQueueStatus {
    if !Path::new("/sys").is_dir() {
        return NicQueueStatus::Absent;
    }
    if !Path::new("/sys/class/net").is_dir() {
        return NicQueueStatus::Absent;
    }
    let applied = ops_net.join("state.applied");
    if !applied.is_file() {
        return NicQueueStatus::Fail;
    }
    let text = match fs::read_to_string(&applied) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "state.applied unreadable");
            return NicQueueStatus::Fail;
        }
    };
    let iface = text.lines().find_map(|l| {
        let t = l.trim();
        t.strip_prefix("iface=").map(str::to_owned)
    });
    let Some(iface) = iface else {
        return NicQueueStatus::Fail;
    };
    if iface.is_empty() || iface == "CHANGE_ME" {
        return NicQueueStatus::Fail;
    }
    let dev = PathBuf::from("/sys/class/net").join(&iface);
    if !dev.is_dir() {
        return NicQueueStatus::Fail;
    }
    let queues = dev.join("queues");
    if !queues.is_dir() {
        return NicQueueStatus::Fail;
    }
    NicQueueStatus::Pass
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_none_not_zero() {
        assert_eq!(percentile_nearest_rank(&[], 500), None);
        assert_eq!(percentile_nearest_rank(&[], 990), None);
        assert!(SampleWindow::with_capacity(8).p99().is_none());
    }

    #[test]
    fn one_sample_is_nearest_rank_not_a_lie() {
        let mut w = SampleWindow::with_capacity(8);
        w.record(42);
        let r = w.p99().expect("one sample has a rank");
        assert_eq!(r.n, 1);
        assert_eq!(r.p99_ns, 42);
        assert!(!r.reliable, "n=1 is not a reliable p99");
        assert_eq!(percentile_nearest_rank(&[42], 990), Some(42));
    }

    #[test]
    fn known_five_samples_match_16c() {
        let s = [10_u64, 20, 30, 40, 50];
        assert_eq!(percentile_nearest_rank(&s, 500), Some(30));
        assert_eq!(percentile_nearest_rank(&s, 990), Some(50));
    }

    #[test]
    fn hundred_samples_p99_is_rank_99() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile_nearest_rank(&v, 990), Some(99));
        assert!(p99_reliable(100));
        assert!(!p99_reliable(99));
    }

    #[test]
    fn path_pass_forbidden_without_universe() {
        let r = RankP99 {
            n: 100,
            p99_ns: 1,
            reliable: true,
        };
        assert_eq!(
            path_verdict(Some(r), false, PATH_A_BUDGET_NS),
            PathVerdict::Unmeasured
        );
        assert_eq!(
            path_verdict(Some(r), true, PATH_A_BUDGET_NS),
            PathVerdict::Pass
        );
        assert_eq!(
            path_verdict(None, true, PATH_A_BUDGET_NS),
            PathVerdict::Unmeasured
        );
    }
}
