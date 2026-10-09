//! Hot reload of the registry's exits while the node runs — no restart.
//!
//! **Not started** (decision 2026-10-07): no exit joins the running bot
//! unreviewed. The daily refresh proposes, a person admits
//! (`tools/registry/admit_reviewed.py`), and a restart takes the batch. The
//! module is kept, tested, for a reviewed hot path should one be wanted.
//!
//! A thread polls `registry.json`'s modification time (the daily discovery
//! job replaces it atomically). On a change it reads the file, diffs it
//! against what the process runs on, and applies only exits for tokens the
//! process already interned:
//! - new UniswapV2 / SushiSwap pairs and Curve / crypto pools join the book
//!   after their tokens and coins are asserted on chain (the boot
//!   assertion, on just the additions). The hot thread is asked to route
//!   their logs; only once it does are V2 pairs seeded and Curve pools
//!   forced stale at the current head, so no trade between the file and the
//!   subscription is missed;
//! - unwraps added or changed replace the book's; removed ones are dropped.
//!
//! Everything else needs a restart and is appended to the review log for a
//! person to decide on: a new or changed token (asset ids are fixed at
//! startup), a protocol, oracle, flash-source or router change, a Uniswap V3
//! pool (Uniswap V3 pools created on chain are already followed live), and
//! pools removed or changed (they stay routed until the restart).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use alloy_primitives::Address;
use liq_config::rpc::{ChainRpc, HttpRpc};
use liq_config::{Intern, PoolVenue, Registry, TokenEntry};
use liq_router::{PoolBook, PoolId};
use parking_lot::RwLock;

/// How often the file's modification time is checked.
const POLL: Duration = Duration::from_secs(15);
/// How long the hot thread may take to route new pools' logs.
const RESUBSCRIBE_WAIT: Duration = Duration::from_secs(120);

/// What the watcher needs from the running process.
pub struct RegistryWatch {
    pub path: PathBuf,
    /// Appended to (one timestamped block per reload with anything to
    /// review).
    pub review_log: PathBuf,
    pub rpc_url: String,
    pub intern: Intern,
    pub hops: crate::gas_model::HopGas,
    pub book: Arc<RwLock<PoolBook>>,
    pub resubscribe: Arc<liq_node::Resubscribe>,
    /// The registry the process started on.
    pub loaded: Registry,
}

/// What changed between the running registry and the file.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Diff {
    /// Pools that can join the book now (not Uniswap V3).
    pub pools_added: Vec<Address>,
    /// Tokens whose unwrap was added or changed.
    pub unwraps_set: Vec<Address>,
    /// Tokens whose unwrap was removed.
    pub unwraps_removed: Vec<Address>,
    /// Changes that need a restart or a person: one line each.
    pub review: Vec<String>,
}

impl Diff {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pools_added.is_empty()
            && self.unwraps_set.is_empty()
            && self.unwraps_removed.is_empty()
            && self.review.is_empty()
    }
}

fn sym(e: &TokenEntry) -> &str {
    e.symbol.as_deref().unwrap_or("?")
}

/// Diff `old` (running) against `new` (the file).
#[must_use]
pub fn diff(old: &Registry, new: &Registry) -> Diff {
    let mut d = Diff::default();
    if old.chain_id != new.chain_id {
        d.review.push(format!(
            "chain id {} → {}: file refused",
            old.chain_id, new.chain_id
        ));
        return d;
    }
    for (t, e) in &new.tokens {
        match old.tokens.get(t) {
            None => d.review.push(format!(
                "new token {t:#x} ({}, {} decimals) — needs a restart to intern",
                sym(e),
                e.decimals
            )),
            Some(o) => {
                let strip = |x: &TokenEntry| TokenEntry {
                    unwrap: None,
                    ..x.clone()
                };
                if strip(o) != strip(e) {
                    d.review.push(format!(
                        "token {t:#x} ({}) changed — needs a restart",
                        sym(e)
                    ));
                }
                match (&o.unwrap, &e.unwrap) {
                    (a, Some(b)) if a.as_ref() != Some(b) => d.unwraps_set.push(*t),
                    (Some(_), None) => d.unwraps_removed.push(*t),
                    _ => {}
                }
            }
        }
    }
    for (t, e) in &old.tokens {
        if !new.tokens.contains_key(t) {
            d.review.push(format!(
                "token {t:#x} ({}) removed — stays interned until a restart",
                sym(e)
            ));
        }
    }
    let as_json = |m: &BTreeMap<String, liq_config::ProtocolEntry>| {
        m.iter()
            .map(|(k, v)| (k.clone(), serde_json::to_value(v).unwrap_or_default()))
            .collect::<BTreeMap<_, _>>()
    };
    let (op, np) = (as_json(&old.protocols), as_json(&new.protocols));
    for (k, v) in &np {
        match op.get(k) {
            None => d.review.push(format!(
                "new protocol market {k} — needs an adapter bind and a restart"
            )),
            Some(o) if o != v => d
                .review
                .push(format!("protocol market {k} changed — needs a restart")),
            _ => {}
        }
    }
    for k in op.keys() {
        if !np.contains_key(k) {
            d.review.push(format!(
                "protocol market {k} removed — still bound until a restart"
            ));
        }
    }
    if old.oracles != new.oracles {
        d.review
            .push("oracles changed — needs a restart".to_string());
    }
    if old.flash_sources != new.flash_sources || old.routers != new.routers {
        d.review
            .push("flash sources / routers changed — needs a restart".to_string());
    }
    for (a, p) in &new.pools {
        match old.pools.get(a) {
            None if p.venue == PoolVenue::Univ3 => d.review.push(format!(
                "new Uniswap V3 pool {a:#x} — needs a restart (pools created on chain are followed live)"
            )),
            None => d.pools_added.push(*a),
            Some(o) if o != p => d.review.push(format!(
                "pool {a:#x} changed — the running entry stays until a restart"
            )),
            _ => {}
        }
    }
    for a in old.pools.keys() {
        if !new.pools.contains_key(a) {
            d.review.push(format!(
                "pool {a:#x} no longer admitted — stays routed until a restart"
            ));
        }
    }
    d
}

/// The part of `new` the additions touch, for the boot assertion: every
/// token they use and the added pools.
fn additions(new: &Registry, d: &Diff) -> Registry {
    let mut r = new.clone();
    r.protocols.clear();
    r.oracles.clear();
    r.flash_sources.clear();
    r.routers.clear();
    r.asset_ledger = None;
    r.pools.retain(|a, _| d.pools_added.contains(a));
    let mut keep: Vec<Address> = d.unwraps_set.clone();
    for t in &d.unwraps_set {
        if let Some(u) = new.tokens.get(t).and_then(|e| e.unwrap) {
            keep.push(u.into);
        }
    }
    for p in r.pools.values() {
        keep.extend(p.coins.iter().copied());
        keep.push(p.token0);
        keep.push(p.token1);
    }
    r.tokens.retain(|t, _| keep.contains(t));
    r
}

fn append_review(path: &std::path::Path, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::error!(error = %e, dir = %dir.display(), "review log dir not created");
            return;
        }
    }
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut body = format!("== registry reload at unix {stamp}\n");
    for l in lines {
        body.push_str("- ");
        body.push_str(l);
        body.push('\n');
    }
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| f.write_all(body.as_bytes()))
    {
        Ok(()) => tracing::warn!(
            items = lines.len(),
            log = %path.display(),
            "registry changes need review"
        ),
        Err(e) => tracing::error!(error = %e, log = %path.display(), "review log not written"),
    }
}

fn modified(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Start the watcher thread.
pub fn spawn(
    w: RegistryWatch,
    stop: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("liq-bot-registry".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "registry watch runtime refused — no hot reload");
                    return;
                }
            };
            let mut w = w;
            let mut seen = modified(&w.path);
            tracing::info!(path = %w.path.display(), "registry watch started");
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(POLL);
                let now = modified(&w.path);
                if now == seen {
                    continue;
                }
                seen = now;
                rt.block_on(reload(&mut w));
            }
        })
}

/// Read the file and apply what can be applied. The running registry moves
/// to the file's, so each change is reviewed once.
pub async fn reload(w: &mut RegistryWatch) {
    let next = match Registry::from_path(&w.path) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "registry file unreadable — running registry kept");
            return;
        }
    };
    let d = diff(&w.loaded, &next);
    if d.is_empty() {
        return;
    }
    let mut review = d.review.clone();
    let rpc = match HttpRpc::connect(&w.rpc_url) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "registry reload: RPC connect failed — retried on the next change");
            return;
        }
    };
    let mini = additions(&next, &d);
    if !mini.tokens.is_empty() {
        if let Err(e) = liq_config::assert_registry(&mini, &rpc).await {
            review.push(format!(
                "additions refused by the on-chain assertion ({e}) — nothing applied"
            ));
            append_review(&w.review_log, &review);
            w.loaded = next;
            return;
        }
    }
    let mut v2 = Vec::new();
    let mut curve = Vec::new();
    let mut unwraps = 0usize;
    {
        let mut book = w.book.write();
        for a in &d.pools_added {
            let Some(entry) = next.pools.get(a) else {
                continue;
            };
            let pool = match crate::index::build_pool(&w.intern, &next, w.hops, *a, entry) {
                Ok(p) => p,
                Err(why) => {
                    review.push(format!("pool {a:#x} not added: {why}"));
                    continue;
                }
            };
            let added = if entry.venue == PoolVenue::Univ2 {
                book.add_pending_v2(pool).map(|id| v2.push((id, *a)))
            } else {
                book.add(pool).map(|id| curve.push(id))
            };
            if let Err(e) = added {
                review.push(format!("pool {a:#x} not added: {e:?}"));
            }
        }
        for t in &d.unwraps_set {
            let Some(entry) = next.tokens.get(t) else {
                continue;
            };
            match crate::index::build_unwrap(&w.intern, &next, w.hops, *t, entry) {
                Ok(Some(u)) => {
                    book.add_unwrap(u);
                    unwraps = unwraps.saturating_add(1);
                }
                Ok(None) => {}
                Err(why) => review.push(format!("unwrap {t:#x} not added: {why}")),
            }
        }
        for t in &d.unwraps_removed {
            if let Some(id) = w.intern.asset(*t) {
                book.remove_unwrap(id);
            }
        }
    }
    if !v2.is_empty() || !curve.is_empty() {
        let epoch = w.resubscribe.request();
        if !w.resubscribe.wait_applied(epoch, RESUBSCRIBE_WAIT) {
            review.push(
                "log router not rebuilt in time — added pools stay unseeded until it is".into(),
            );
        } else {
            seed_added(w, &rpc, &v2, &curve).await;
        }
    }
    tracing::info!(
        pools = v2.len().saturating_add(curve.len()),
        unwraps,
        unwraps_removed = d.unwraps_removed.len(),
        review = review.len(),
        "registry reloaded"
    );
    append_review(&w.review_log, &review);
    w.loaded = next;
}

/// After the router follows the new pools: V2 pairs from `getReserves` and
/// Curve pools re-read, both at a head no older than the subscription.
async fn seed_added(w: &RegistryWatch, rpc: &HttpRpc, v2: &[(PoolId, Address)], curve: &[PoolId]) {
    let head = match rpc.block_number().await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "registry reload: head unavailable — added pools unseeded");
            return;
        }
    };
    {
        let mut book = w.book.write();
        for &id in curve {
            book.mark_stale(id, head);
        }
    }
    if v2.is_empty() {
        return;
    }
    let pairs: Vec<Address> = v2.iter().map(|&(_, a)| a).collect();
    let reads = crate::pool_seed::read_v2_reserves(rpc, &pairs, head).await;
    let mut book = w.book.write();
    for (&(id, a), r) in v2.iter().zip(reads) {
        match r {
            Some((r0, r1)) => {
                let seeded = book.seed_pending_v2(id, r0, r1, head);
                let live = book.get(id).is_some_and(|p| p.is_live());
                tracing::info!(pair = %a, seeded, live, "V2 pair added at runtime");
            }
            None => {
                tracing::error!(pair = %a, "V2 reserves unreadable — pair waits for its first Sync")
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use liq_router::PoolState;

    fn committed() -> Registry {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        Registry::from_path(&root.join("registry/registry.json")).unwrap()
    }

    #[test]
    fn identical_registries_differ_in_nothing() {
        let r = committed();
        assert!(diff(&r, &r.clone()).is_empty());
    }

    /// Exits for known tokens apply; new tokens, protocol markets and V3
    /// pools go to review; removals are reported, not applied.
    #[test]
    fn exits_apply_and_everything_else_is_reviewed() {
        let old = committed();
        let mut new = old.clone();
        // A removed V2 pair comes back as an addition.
        let (&v2, _) = old
            .pools
            .iter()
            .find(|(_, p)| p.venue == PoolVenue::Univ2)
            .unwrap();
        let mut old2 = old.clone();
        old2.pools.remove(&v2);
        // An unwrap is removed, another changed.
        let mut with_unwrap = old
            .tokens
            .iter()
            .filter(|(_, e)| e.unwrap.is_some())
            .map(|(t, _)| *t);
        let gone = with_unwrap.next().unwrap();
        let changed = with_unwrap.next().unwrap();
        new.tokens.get_mut(&gone).unwrap().unwrap = None;
        let into = new.tokens[&changed].unwrap.unwrap().into;
        new.tokens
            .get_mut(&changed)
            .unwrap()
            .unwrap
            .as_mut()
            .unwrap()
            .into = if into == Address::ZERO {
            Address::repeat_byte(1)
        } else {
            Address::ZERO
        };
        // A brand-new token and a new V3 pool.
        let fresh = Address::repeat_byte(0xAB);
        new.tokens
            .insert(fresh, new.tokens.values().next().unwrap().clone());
        let (&v3, v3e) = old
            .pools
            .iter()
            .find(|(_, p)| p.venue == PoolVenue::Univ3)
            .unwrap();
        let v3e = v3e.clone();
        let mut old3 = old2.clone();
        old3.pools.remove(&v3);
        new.pools.insert(v3, v3e);
        let d = diff(&old3, &new);
        assert_eq!(d.pools_added, vec![v2]);
        assert_eq!(d.unwraps_removed, vec![gone]);
        assert_eq!(d.unwraps_set, vec![changed]);
        assert!(d
            .review
            .iter()
            .any(|l| l.contains("new token") && l.contains(&format!("{fresh:#x}"))));
        assert!(d
            .review
            .iter()
            .any(|l| l.contains("Uniswap V3") && l.contains(&format!("{v3:#x}"))));
        // A pool dropped from the file is reported, not removed.
        let d2 = diff(&old, &old2);
        assert!(d2.pools_added.is_empty());
        assert!(d2.review.iter().any(|l| l.contains("no longer admitted")));
        // A protocol market added is reviewed.
        let mut new3 = old.clone();
        let (k, v) = old.protocols.iter().next().unwrap();
        new3.protocols.insert(format!("{k}-copy"), v.clone());
        let d3 = diff(&old, &new3);
        assert!(d3
            .review
            .iter()
            .any(|l| l.starts_with("new protocol market")));
        assert!(d3.pools_added.is_empty() && d3.unwraps_set.is_empty());
    }

    /// Live (`MAINNET_RPC_URL`): a process running on the committed registry
    /// less a V2 pair, a Curve NG pool and an unwrap sees the full file and
    /// takes all three without a restart — the pair seeded from
    /// `getReserves` at the head once its logs are routed, the pool stale at
    /// that head for the reseed, the unwrap back in the book.
    #[tokio::test]
    #[ignore = "needs MAINNET_RPC_URL"]
    async fn reload_adds_exits_live() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = root.join("registry/registry.json");
        let full = Registry::from_path(&path).unwrap();
        let intern = Intern::from_registry(&full).unwrap();
        let mut running = full.clone();
        let (&v2, _) = full
            .pools
            .iter()
            .find(|(_, p)| p.venue == PoolVenue::Univ2)
            .unwrap();
        let (&ng, _) = full
            .pools
            .iter()
            .find(|(_, p)| p.venue == PoolVenue::CurveNg)
            .unwrap();
        let (&wrapped, _) = full
            .tokens
            .iter()
            .find(|(_, e)| {
                e.unwrap
                    .is_some_and(|u| u.kind == liq_config::UnwrapKind::Erc4626)
            })
            .unwrap();
        running.pools.remove(&v2);
        running.pools.remove(&ng);
        running.tokens.get_mut(&wrapped).unwrap().unwrap = None;
        let load = crate::index::load_index(&root.join("config"), &intern, &running);
        let book = Arc::new(RwLock::new(load.book));
        assert!(book.read().by_address(v2).is_none());
        let resubscribe = Arc::new(liq_node::Resubscribe::new());
        // Stand-in for the hot thread: acknowledge every request.
        let stop = Arc::new(AtomicBool::new(false));
        let (r2, s2) = (Arc::clone(&resubscribe), Arc::clone(&stop));
        let ack = std::thread::spawn(move || {
            while !s2.load(Ordering::Relaxed) {
                let want = r2.requested();
                if want != r2.applied() {
                    r2.publish(std::collections::HashSet::new(), want);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let review_log = std::env::temp_dir().join("liq-registry-watch-test.log");
        let _ = std::fs::remove_file(&review_log);
        let mut w = RegistryWatch {
            path,
            review_log: review_log.clone(),
            rpc_url: std::env::var("MAINNET_RPC_URL").expect("MAINNET_RPC_URL"),
            intern: intern.clone(),
            hops: crate::gas_model::GasModel::load(&root.join("config/liq-gas.toml"))
                .unwrap()
                .hop,
            book: Arc::clone(&book),
            resubscribe: Arc::clone(&resubscribe),
            loaded: running,
        };
        reload(&mut w).await;
        stop.store(true, Ordering::Relaxed);
        ack.join().unwrap();
        let b = book.read();
        let id = b.by_address(v2).expect("pair added");
        assert!(b.get(id).unwrap().is_live(), "pair seeded from getReserves");
        let nid = b.by_address(ng).expect("NG pool added");
        let PoolState::Curve(c) = &b.get(nid).unwrap().state else {
            panic!()
        };
        assert!(
            c.stale && c.stale_block > 0,
            "NG pool waits for its re-read"
        );
        assert!(
            b.unwrap_of(intern.asset(wrapped).unwrap()).is_some(),
            "unwrap back"
        );
        assert!(
            resubscribe.applied() >= 1,
            "the router was asked to follow them"
        );
        assert!(!review_log.exists(), "nothing needed review");
        assert!(
            diff(&w.loaded, &full).is_empty(),
            "running registry is the file's"
        );
    }

    /// The additions registry asserts only what was added.
    #[test]
    fn additions_hold_only_the_added_pools_and_their_tokens() {
        let old = committed();
        let (&a, p) = old
            .pools
            .iter()
            .find(|(_, p)| p.venue == PoolVenue::CurveNg)
            .unwrap();
        let d = Diff {
            pools_added: vec![a],
            ..Diff::default()
        };
        let mini = additions(&old, &d);
        assert_eq!(mini.pools.len(), 1);
        assert!(mini.protocols.is_empty() && mini.oracles.is_empty());
        for c in &p.coins {
            assert!(mini.tokens.contains_key(c));
        }
        assert!(mini.tokens.len() <= p.coins.len() + 2);
    }
}
