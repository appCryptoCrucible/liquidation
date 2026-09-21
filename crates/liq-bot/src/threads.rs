//! Named threads, `cores.toml` topology, and fail-closed pinning.
//!
//! Live `/sys` topology is A1. This module reads the committed seam file only.

use core_affinity::CoreId;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::thread::{Builder, JoinHandle};
use thiserror::Error;

/// Core for `liq-node-hot`. Written once at startup, read by
/// [`crate::exex_install::pin_hot_configured`] (`fn()`, not a closure).
/// `usize::MAX` means unset — pin then fails closed.
pub static HOT_PIN_CORE: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Linux `shared_cpu_list` / `Cpus_allowed_list` grammar (ranges and commas).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuList(Box<[u16]>);

impl CpuList {
    /// Parse `0-7`, `0,2,4-6`, or a single id. Empty / inverted ranges fail.
    pub fn parse(s: &str) -> Result<Self, ThreadError> {
        let t = s.trim();
        if t.is_empty() {
            return Err(ThreadError::BadCpuList {
                text: s.to_owned(),
                why: "empty",
            });
        }
        let mut out: Vec<u16> = Vec::new();
        for part in t.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(ThreadError::BadCpuList {
                    text: s.to_owned(),
                    why: "empty segment",
                });
            }
            if let Some((a, b)) = part.split_once('-') {
                let start: u16 = parse_cpu_id(a, s)?;
                let end: u16 = parse_cpu_id(b, s)?;
                if end < start {
                    return Err(ThreadError::BadCpuList {
                        text: s.to_owned(),
                        why: "inverted range",
                    });
                }
                let mut c = start;
                loop {
                    out.push(c);
                    if c == end {
                        break;
                    }
                    c = c.checked_add(1).ok_or_else(|| ThreadError::BadCpuList {
                        text: s.to_owned(),
                        why: "cpu id overflow",
                    })?;
                }
            } else {
                out.push(parse_cpu_id(part, s)?);
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(Self(out.into_boxed_slice()))
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u16] {
        &self.0
    }

    #[must_use]
    pub fn contains(&self, core: u16) -> bool {
        self.0.binary_search(&core).is_ok()
    }
}

fn parse_cpu_id(tok: &str, full: &str) -> Result<u16, ThreadError> {
    tok.trim()
        .parse::<u16>()
        .map_err(|_| ThreadError::BadCpuList {
            text: full.to_owned(),
            why: "not a u16 cpu id",
        })
}

/// One L3 / CCX grouping from `shared_cpu_list`.
#[derive(Clone, Debug)]
pub struct L3Slice {
    pub shared_cpu_list: CpuList,
    pub numa: u8,
    pub isolated: bool,
    pub exclusive_hot: bool,
}

/// One named, pinned OS thread.
#[derive(Clone, Debug)]
pub struct ThreadSlot {
    pub name: String,
    pub core: u16,
}

/// Loaded `config/cores.toml`. Owned; no `Arc<Mutex<_>>`.
#[derive(Clone, Debug)]
pub struct CoreMap {
    pub schema: u32,
    l3: Box<[L3Slice]>,
    threads: Box<[ThreadSlot]>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    schema: u32,
    l3: Vec<L3Raw>,
    thread: Vec<ThreadRaw>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct L3Raw {
    shared_cpu_list: String,
    numa: u8,
    isolated: bool,
    exclusive_hot: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadRaw {
    name: String,
    core: u16,
}

#[derive(Debug, Error)]
pub enum ThreadError {
    #[error("cores.toml io: {0}")]
    Io(#[from] std::io::Error),
    #[error("cores.toml parse: {0}")]
    Toml(String),
    #[error("cpu list {text:?}: {why}")]
    BadCpuList { text: String, why: &'static str },
    #[error("cores.toml schema must be 1, got {0}")]
    BadSchema(u32),
    #[error("cores.toml has no [[l3]] slices")]
    NoL3,
    #[error("cores.toml has no [[thread]] pins")]
    NoThreads,
    #[error("no exclusive_hot L3 slice (hot CCX)")]
    NoHotCcx,
    #[error("duplicate thread name {0}")]
    DuplicateName(String),
    #[error("duplicate core {0} in thread pin map")]
    DuplicateCore(u16),
    #[error("thread name {0:?} is not liq-<crate>-<role>")]
    BadName(String),
    #[error("unknown thread {0}")]
    UnknownThread(String),
    #[error("core {core} is not in any shared_cpu_list")]
    CoreNotInTopology { core: u16 },
    #[error("hot thread {name} core {core} is not on exclusive_hot CCX")]
    HotThreadOffExclusiveCcx { name: String, core: u16 },
    #[error("non-hot thread {name} core {core} contaminates exclusive_hot CCX")]
    HotCcxContaminated { name: String, core: u16 },
    #[error("sim worker {name} core {core} is not isolated off the hot CCX")]
    SimOffIsolated { name: String, core: u16 },
    #[error("core {core} is not in this process affinity set")]
    CoreUnavailable { core: usize },
    #[error("pin_to_core({core}) returned false")]
    PinFailed { core: usize },
    #[error("affinity mismatch: expected core {expected}, allowed {actual}")]
    AffinityMismatch { expected: usize, actual: String },
}

impl CoreMap {
    /// Load and validate `cores.toml`. Missing file / bad topology fail closed.
    pub fn load(path: &Path) -> Result<Self, ThreadError> {
        let raw = std::fs::read_to_string(path)?;
        Self::parse_toml(&raw)
    }

    pub fn parse_toml(raw: &str) -> Result<Self, ThreadError> {
        let file: File = toml::from_str(raw).map_err(|e| ThreadError::Toml(e.to_string()))?;
        if file.schema != 1 {
            return Err(ThreadError::BadSchema(file.schema));
        }
        if file.l3.is_empty() {
            return Err(ThreadError::NoL3);
        }
        if file.thread.is_empty() {
            return Err(ThreadError::NoThreads);
        }
        let mut l3 = Vec::with_capacity(file.l3.len());
        let mut hot_ccx = 0usize;
        for s in file.l3 {
            let list = CpuList::parse(&s.shared_cpu_list)?;
            if s.exclusive_hot {
                hot_ccx = hot_ccx.saturating_add(1);
                if !s.isolated {
                    return Err(ThreadError::Toml(
                        "exclusive_hot L3 must be isolated".into(),
                    ));
                }
            }
            l3.push(L3Slice {
                shared_cpu_list: list,
                numa: s.numa,
                isolated: s.isolated,
                exclusive_hot: s.exclusive_hot,
            });
        }
        if hot_ccx != 1 {
            return Err(ThreadError::NoHotCcx);
        }
        let mut names = HashSet::new();
        let mut cores = HashSet::new();
        let mut threads = Vec::with_capacity(file.thread.len());
        for t in file.thread {
            if !is_liq_thread_name(&t.name) {
                return Err(ThreadError::BadName(t.name));
            }
            if !names.insert(t.name.clone()) {
                return Err(ThreadError::DuplicateName(t.name));
            }
            if !cores.insert(t.core) {
                return Err(ThreadError::DuplicateCore(t.core));
            }
            let slice = l3
                .iter()
                .find(|s| s.shared_cpu_list.contains(t.core))
                .ok_or(ThreadError::CoreNotInTopology { core: t.core })?;
            let hot_name = is_hot_thread(&t.name);
            let sim = is_sim_worker(&t.name);
            if hot_name && !slice.exclusive_hot {
                return Err(ThreadError::HotThreadOffExclusiveCcx {
                    name: t.name,
                    core: t.core,
                });
            }
            if !hot_name && slice.exclusive_hot {
                return Err(ThreadError::HotCcxContaminated {
                    name: t.name,
                    core: t.core,
                });
            }
            if sim && (slice.exclusive_hot || !slice.isolated) {
                return Err(ThreadError::SimOffIsolated {
                    name: t.name,
                    core: t.core,
                });
            }
            threads.push(ThreadSlot {
                name: t.name,
                core: t.core,
            });
        }
        Ok(Self {
            schema: file.schema,
            l3: l3.into_boxed_slice(),
            threads: threads.into_boxed_slice(),
        })
    }

    #[must_use]
    pub fn l3(&self) -> &[L3Slice] {
        &self.l3
    }

    #[must_use]
    pub fn threads(&self) -> &[ThreadSlot] {
        &self.threads
    }

    pub fn slot(&self, name: &str) -> Result<&ThreadSlot, ThreadError> {
        self.threads
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| ThreadError::UnknownThread(name.to_owned()))
    }
}

/// Record the hot-thread core before spawn. Not a skip of the pin itself.
pub fn configure_hot_pin(core: u16) {
    HOT_PIN_CORE.store(usize::from(core), std::sync::atomic::Ordering::Release);
}

/// `liq-<crate>-<role>` with crate a single kebab-free token.
#[must_use]
pub fn is_liq_thread_name(name: &str) -> bool {
    let mut parts = name.split('-');
    if parts.next() != Some("liq") {
        return false;
    }
    match parts.next() {
        Some(c) if !c.is_empty() && c.bytes().all(|b| b.is_ascii_lowercase()) => {}
        _ => return false,
    }
    let mut n_role = 0usize;
    for p in parts {
        if p.is_empty()
            || !p
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return false;
        }
        n_role = n_role.saturating_add(1);
    }
    n_role > 0
}

/// Build `liq-<crate>-<role>`.
pub fn thread_name(crate_name: &str, role: &str) -> Result<String, ThreadError> {
    let n = format!("liq-{crate_name}-{role}");
    if !is_liq_thread_name(&n) {
        return Err(ThreadError::BadName(n));
    }
    Ok(n)
}

fn is_hot_thread(name: &str) -> bool {
    matches!(
        name,
        "liq-node-hot" | "liq-oracle-fusion" | "liq-engine-recompute"
    )
}

fn is_sim_worker(name: &str) -> bool {
    name.split_once("liq-sim-")
        .is_some_and(|(_, r)| r.starts_with("worker-"))
}

/// Pin the current thread to `core`. Fail closed if the core is missing or
/// `set_for_current` fails, then assert the thread is on that core.
pub fn pin_to_core(core: usize) -> Result<(), ThreadError> {
    let ids = core_affinity::get_core_ids().ok_or(ThreadError::CoreUnavailable { core })?;
    let id = ids
        .iter()
        .copied()
        .find(|c: &CoreId| c.id == core)
        .ok_or(ThreadError::CoreUnavailable { core })?;
    if !core_affinity::set_for_current(id) {
        tracing::error!(core, "pin_to_core failed");
        return Err(ThreadError::PinFailed { core });
    }
    assert_on_core(core)
}

/// Startup assertion: current thread affinity is exactly `core`.
pub fn assert_on_core(core: usize) -> Result<(), ThreadError> {
    #[cfg(target_os = "linux")]
    {
        let allowed = read_cpus_allowed_list()?;
        let want = u16::try_from(core).map_err(|_| ThreadError::CoreUnavailable { core })?;
        if allowed.as_slice() == [want] {
            return Ok(());
        }
        tracing::error!(
            expected = core,
            actual = ?allowed.as_slice(),
            "thread not on assigned core"
        );
        return Err(ThreadError::AffinityMismatch {
            expected: core,
            actual: format!("{:?}", allowed.as_slice()),
        });
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Production box is Linux (A1). Here pin already succeeded and `core`
        // was in the process set. Thread-mask readback without extra `unsafe`
        // is Linux `/proc/thread-self`.
        let ids = core_affinity::get_core_ids().ok_or(ThreadError::CoreUnavailable { core })?;
        if ids.iter().any(|c| c.id == core) {
            return Ok(());
        }
        Err(ThreadError::AffinityMismatch {
            expected: core,
            actual: "process affinity no longer contains core".into(),
        })
    }
}

#[cfg(target_os = "linux")]
fn read_cpus_allowed_list() -> Result<CpuList, ThreadError> {
    let status = std::fs::read_to_string("/proc/thread-self/status")?;
    for line in status.lines() {
        if let Some(v) = line.strip_prefix("Cpus_allowed_list:") {
            return CpuList::parse(v.trim());
        }
    }
    Err(ThreadError::Toml(
        "/proc/thread-self/status missing Cpus_allowed_list".into(),
    ))
}

/// Owned handle. Not `Arc<Mutex<Thread>>`.
pub struct PinnedThread {
    pub name: String,
    pub core: u16,
    handle: JoinHandle<()>,
}

impl PinnedThread {
    #[must_use]
    pub fn handle(&self) -> &JoinHandle<()> {
        &self.handle
    }

    pub fn join(self) -> std::thread::Result<()> {
        self.handle.join()
    }
}

/// Spawn `name` from the map, pin, assert, then run `f`. Pin failure is an
/// `Err` to the caller (thread exits without running `f`).
pub fn spawn_pinned<F>(map: &CoreMap, name: &str, f: F) -> Result<PinnedThread, ThreadError>
where
    F: FnOnce() + Send + 'static,
{
    let slot = map.slot(name)?;
    let core_u = usize::from(slot.core);
    let name_owned = slot.name.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let handle = Builder::new().name(name_owned.clone()).spawn(move || {
        let pin = pin_to_core(core_u);
        let ok = pin.is_ok();
        let _ = tx.send(pin);
        if ok {
            f();
        }
    })?;
    match rx.recv() {
        Ok(Ok(())) => Ok(PinnedThread {
            name: name_owned,
            core: slot.core,
            handle,
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(ThreadError::PinFailed { core: core_u }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn committed_toml() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/cores.toml")
    }

    #[test]
    fn cpu_list_parse() {
        assert_eq!(CpuList::parse("0-3").unwrap().as_slice(), &[0, 1, 2, 3]);
        assert_eq!(CpuList::parse("0,8,16").unwrap().as_slice(), &[0, 8, 16]);
        assert_eq!(CpuList::parse("0-1,8-9").unwrap().as_slice(), &[0, 1, 8, 9]);
        assert!(CpuList::parse("").is_err());
        assert!(CpuList::parse("3-1").is_err());
    }

    #[test]
    fn names() {
        assert!(is_liq_thread_name("liq-node-hot"));
        assert!(is_liq_thread_name("liq-engine-recompute"));
        assert!(is_liq_thread_name("liq-sim-worker-0"));
        assert!(!is_liq_thread_name("node-hot"));
        assert!(!is_liq_thread_name("liq-hot"));
        assert_eq!(thread_name("node", "hot").unwrap().as_str(), "liq-node-hot");
    }

    #[test]
    fn load_committed_placeholder() {
        let map = CoreMap::load(&committed_toml()).expect("committed cores.toml");
        assert_eq!(map.schema, 1);
        assert!(map.l3().iter().any(|s| s.exclusive_hot));
        assert_eq!(map.slot("liq-node-hot").unwrap().core, 0);
        assert_eq!(map.slot("liq-oracle-fusion").unwrap().core, 1);
        assert_eq!(map.slot("liq-engine-recompute").unwrap().core, 2);
        assert_eq!(map.slot("liq-sim-worker-0").unwrap().core, 8);
        assert!(map.slot("nope").is_err());
    }

    #[test]
    fn hot_thread_off_ccx_fails() {
        let toml = r#"
schema = 1
[[l3]]
shared_cpu_list = "0-7"
numa = 0
isolated = true
exclusive_hot = true
[[l3]]
shared_cpu_list = "8-15"
numa = 0
isolated = true
exclusive_hot = false
[[thread]]
name = "liq-node-hot"
core = 8
"#;
        match CoreMap::parse_toml(toml) {
            Err(ThreadError::HotThreadOffExclusiveCcx { .. }) => {}
            other => panic!("expected HotThreadOffExclusiveCcx, got {other:?}"),
        }
    }

    #[test]
    fn pin_missing_core_fails_closed() {
        let n = core_affinity::get_core_ids()
            .expect("process affinity")
            .iter()
            .map(|c| c.id)
            .max()
            .expect("at least one core")
            .saturating_add(64);
        match pin_to_core(n) {
            Err(ThreadError::CoreUnavailable { core }) if core == n => {}
            other => panic!("expected CoreUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn pin_live_core_and_assert() {
        let id = core_affinity::get_core_ids().expect("process affinity")[0];
        pin_to_core(id.id).expect("pin live core");
        assert_on_core(id.id).expect("assert after pin");
    }

    #[test]
    fn spawn_pinned_live_core() {
        let id = core_affinity::get_core_ids().expect("process affinity")[0];
        let toml = format!(
            r#"
schema = 1
[[l3]]
shared_cpu_list = "{0}-{0}"
numa = 0
isolated = true
exclusive_hot = true
[[thread]]
name = "liq-node-hot"
core = {0}
"#,
            id.id
        );
        let map = CoreMap::parse_toml(&toml).expect("map for live core");
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let t = spawn_pinned(&map, "liq-node-hot", move || {
            tx.send(std::thread::current().name().map(str::to_owned))
                .expect("send");
        })
        .expect("spawn");
        let got = rx.recv().expect("name");
        t.join().expect("join");
        assert_eq!(got.as_deref(), Some("liq-node-hot"));
    }
}
