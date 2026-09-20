//! Fusion thread: canonical (06A-1) + CEX best-forward + staleness (GUIDE 06 §7b, §8).
//!
//! CEX ticks arrive on a bounded MPSC (drop+count on full). [`AggregatorSim`]
//! state is owned here (`&mut`, no lock). [`PendingUpdate`] is drained for
//! pre-warm only — [`crate::canonical::CanonicalBook`] is never written from CEX.

use crate::aggsim::{AggregatorSim, PendingUpdate};
use crate::canonical::{stale_after, CanonicalBook};
use crate::cex::{parse_source, CexTick, CexVenue, CHANNEL_CAP};
use crate::feeds::FeedSet;
use crate::publish::{split, PricePublish, PriceRead};
use crate::{OracleError, Result};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use liq_protocol::DecodedLog;
use liq_types::{AssetId, Confidence, HaltSink, Price, PriceVector, SourceKind};
use smallvec::SmallVec;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Same bound as [`CHANNEL_CAP`] (GUIDE 06 §7b).
pub const FUSION_CHANNEL_CAP: usize = CHANNEL_CAP;

type Route = BTreeMap<(CexVenue, String), SmallVec<[(u16, u8); 2]>>;

/// Fusion owner: one writer thread. Canonical publish is wait-free for readers.
pub struct Fusion {
    book: CanonicalBook,
    publish: PricePublish,
    sims: Vec<AggregatorSim>,
    route: Route,
    forward: PriceVector,
    pending: Vec<PendingUpdate>,
    rx: Receiver<CexTick>,
    dropped: Arc<AtomicU64>,
}

/// Split writer + CEX sender + hot-path canonical reader + drop counter.
pub fn split_fusion(
    book: CanonicalBook,
) -> Result<(Fusion, Sender<CexTick>, PriceRead, Arc<AtomicU64>)> {
    let (publish, read) = split(book.vector());
    let n = book.vector().0.len();
    let mut forward = Vec::with_capacity(n);
    for p in &book.vector().0 {
        forward.push(Price {
            asset: p.asset,
            price: p.price,
            source: SourceKind::Predicted {
                eta: None,
                confidence: Confidence(0),
            },
            block: 0,
            ts: 0,
        });
    }
    let (sims, route) = bind_sims(book.feeds())?;
    let dropped = Arc::new(AtomicU64::new(0));
    let (tx, rx) = crossbeam_channel::bounded(FUSION_CHANNEL_CAP);
    Ok((
        Fusion {
            book,
            publish,
            sims,
            route,
            forward: PriceVector(forward),
            pending: Vec::new(),
            rx,
            dropped: Arc::clone(&dropped),
        },
        tx,
        read,
        dropped,
    ))
}

fn bind_sims(feeds: &FeedSet) -> Result<(Vec<AggregatorSim>, Route)> {
    let mut sims = Vec::with_capacity(feeds.specs.len());
    let mut route: Route = BTreeMap::new();
    for (i, f) in feeds.specs.iter().enumerate() {
        let idx = u16::try_from(i).map_err(|_| OracleError::Load("too many feeds".into()))?;
        let n = f.spec.sources.len();
        sims.push(AggregatorSim::new(
            f.id,
            f.asset,
            f.spec.deviation_bps,
            f.spec.heartbeat_secs,
            n,
        )?);
        for (slot, raw) in f.spec.sources.iter().enumerate() {
            let (venue, symbol) = parse_source(raw)?;
            let s = u8::try_from(slot).map_err(|_| OracleError::TooManyCexSources)?;
            route.entry((venue, symbol)).or_default().push((idx, s));
        }
    }
    Ok((sims, route))
}

impl Fusion {
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn book(&self) -> &CanonicalBook {
        &self.book
    }

    /// Latest complete canonical vector (06A-1). CEX never mutates this.
    #[must_use]
    pub fn canonical(&self) -> &PriceVector {
        self.book.vector()
    }

    /// CEX-predicted overlay. `ts == 0` means no forward quote yet.
    #[must_use]
    pub fn best_forward(&self) -> &PriceVector {
        &self.forward
    }

    /// GUIDE 06 §8: overlay Predicted slots with `confidence >= min`.
    /// `min >= CERTAIN` returns canonical only.
    #[must_use]
    pub fn vector_at(&self, min: Confidence) -> PriceVector {
        let mut v = self.book.vector().clone();
        if min >= Confidence::CERTAIN {
            return v;
        }
        for (i, f) in self.forward.0.iter().enumerate() {
            if f.ts == 0 {
                continue;
            }
            let SourceKind::Predicted { confidence, .. } = f.source else {
                continue;
            };
            if confidence < min {
                continue;
            }
            if let Some(slot) = v.0.get_mut(i) {
                *slot = f.clone();
            }
        }
        v
    }

    #[must_use]
    pub fn is_stale(&self, asset: AssetId, now: u64) -> bool {
        let Some(p) = self.book.vector().0.get(usize::from(asset.0)) else {
            return true;
        };
        if p.ts == 0 {
            return true;
        }
        let hb = heartbeat(self.book.feeds(), asset);
        now.saturating_sub(p.ts) > stale_after(hb)
    }

    pub fn check_staleness(&self, now: u64, sink: &dyn HaltSink) {
        self.book.check_staleness(now, sink);
    }

    /// Drain CEX MPSC (never block). Emit [`PendingUpdate`] for pre-warm.
    pub fn drain_cex(&mut self, now: Instant, now_unix: u64) {
        loop {
            match self.rx.try_recv() {
                Ok(tick) => self.ingest_cex(tick, now, now_unix),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    tracing::error!("cex producers disconnected");
                    break;
                }
            }
        }
    }

    fn ingest_cex(&mut self, tick: CexTick, now: Instant, now_unix: u64) {
        let key = (tick.venue, tick.symbol.clone());
        let Some(hits) = self.route.get(&key).cloned() else {
            tracing::debug!(?tick.venue, symbol = %tick.symbol, "cex tick unmatched");
            return;
        };
        for (idx, slot) in hits {
            let Some(sim) = self.sims.get_mut(usize::from(idx)) else {
                tracing::error!(idx, "sim index");
                continue;
            };
            sim.on_quote(slot, &tick);
            if let Some(p) = sim.on_tick(now, now_unix) {
                write_forward(&mut self.forward, &p);
                self.pending.push(p);
            }
        }
    }

    /// Fold on-chain `AnswerUpdated` (canonical). Republish triple-buffer.
    pub fn apply_log(&mut self, log: &DecodedLog<'_>, sink: &dyn HaltSink) -> Result<bool> {
        let changed = self.book.apply_log(log, sink)?;
        if changed {
            self.sync_onchain();
            self.publish.write(self.book.vector());
        }
        Ok(changed)
    }

    fn sync_onchain(&mut self) {
        for sim in &mut self.sims {
            if let Some(p) = self.book.price(sim.asset) {
                sim.set_onchain(p.price, p.ts);
            }
        }
    }

    /// Take pending pre-warm updates (GUIDE 08). Does not fire.
    pub fn take_pending(&mut self) -> Vec<PendingUpdate> {
        core::mem::take(&mut self.pending)
    }
}

fn heartbeat(feeds: &FeedSet, asset: AssetId) -> u32 {
    for f in &feeds.specs {
        if f.asset == asset {
            return f.spec.heartbeat_secs;
        }
    }
    0
}

fn write_forward(forward: &mut PriceVector, p: &PendingUpdate) {
    let i = usize::from(p.asset.0);
    let Some(slot) = forward.0.get_mut(i) else {
        tracing::error!(asset = p.asset.0, "forward slot missing");
        return;
    };
    slot.asset = p.asset;
    slot.price = p.predicted;
    slot.source = SourceKind::Predicted {
        eta: Some(p.eta),
        confidence: p.confidence,
    };
    slot.block = 0;
    slot.ts = 1;
}

/// Venues + symbols to spawn: one websocket per [`CexVenue`].
pub fn venues_to_spawn(feeds: &FeedSet) -> Result<BTreeMap<CexVenue, Vec<String>>> {
    let mut m: BTreeMap<CexVenue, Vec<String>> = BTreeMap::new();
    for f in &feeds.specs {
        for raw in &f.spec.sources {
            let (v, s) = parse_source(raw)?;
            let e = m.entry(v).or_default();
            if !e.iter().any(|x| x == &s) {
                e.push(s);
            }
        }
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::{split_fusion, Fusion};
    use crate::canonical::{answer_to_ray, encode_answer_updated, CanonicalBook};
    use crate::cex::{decimal_to_ray, CexTick, CexVenue};
    use crate::feeds::{FeedSet, FeedsConfig};
    use crate::OracleError;
    use alloy_primitives::{address, Address, I256, U256};
    use liq_config::{Intern, Registry};
    use liq_node::{DecodeArena, LogRouter, Route};
    use liq_types::{Confidence, HaltReason, HaltScope, HaltSink, LogSubscriber, SourceKind};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Instant;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    }

    const WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    const WETH_AGG: Address = address!("0x7c7FdFCa295a787DED12Bb5c1A49A8d2Cc20E3f8");

    struct Rec(Mutex<Vec<(HaltScope, HaltReason)>>);
    impl HaltSink for Rec {
        fn halt(&self, scope: HaltScope, reason: HaltReason) {
            self.0.lock().unwrap().push((scope, reason));
        }
    }

    fn fusion_live() -> (Fusion, crossbeam_channel::Sender<CexTick>, AssetIdWrap) {
        let root = workspace_root();
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let mut feeds = FeedsConfig::load(&root.join("config/feeds")).unwrap();
        for f in &mut feeds.feeds {
            if f.asset == WETH {
                f.sources = vec![
                    "binance:ETHUSDT".into(),
                    "coinbase:ETH-USD".into(),
                    "kraken:ETH/USD".into(),
                    "okx:ETH-USDT".into(),
                ];
            }
        }
        let intern = Intern::from_registry(&reg).unwrap();
        let set = FeedSet::bind(&feeds, &intern, &reg).unwrap();
        let weth = intern.asset(WETH).unwrap();
        let (book, _) = CanonicalBook::new(set, &intern, &reg).unwrap();
        let (f, tx, _r, _) = split_fusion(book).unwrap();
        (f, tx, AssetIdWrap(weth))
    }

    struct AssetIdWrap(liq_types::AssetId);

    fn apply_weth(f: &mut Fusion, answer: i64, ts: u64) {
        let filters = f.book().feeds().subscriptions();
        struct Sub(Vec<liq_types::LogFilter>);
        impl LogSubscriber for Sub {
            fn subscriptions(&self) -> Vec<liq_types::LogFilter> {
                self.0.clone()
            }
        }
        let router = LogRouter::from_subscribers(&[&Sub(filters) as &dyn LogSubscriber]).unwrap();
        let log = encode_answer_updated(
            WETH_AGG,
            I256::try_from(answer).unwrap(),
            U256::from(1u64),
            U256::from(ts),
            18_000_000,
            ts,
        );
        let arena = DecodeArena::with_capacity(4096);
        match router.route(&arena, &log).unwrap() {
            Route::Hit { decoded, .. } => {
                let rec = Rec(Mutex::new(Vec::new()));
                assert!(f.apply_log(&decoded, &rec).unwrap());
            }
            other => panic!("{other:?}"),
        }
    }

    /// Oracle: CEX does not write Canonical; Predicted is pre-warm.
    #[test]
    fn cex_is_forward_not_canonical() {
        let (mut f, tx, w) = fusion_live();
        apply_weth(&mut f, 100_000_000_000, 1_700_000_000); // $1000 @ 8dp
        let before = f.canonical().0[usize::from(w.0 .0)].clone();
        assert!(matches!(before.source, SourceKind::Canonical));
        let now = Instant::now();
        let px = decimal_to_ray("1005").unwrap();
        crate::cex::push_cex(
            &tx,
            CexTick {
                venue: CexVenue::Binance,
                symbol: "ETHUSDT".into(),
                bid: px,
                ask: px,
                mid: px,
            },
            &std::sync::atomic::AtomicU64::new(0),
        );
        // drop counter on Fusion is separate; send on tx from split
        f.drain_cex(now, 1_700_000_000);
        let after = f.canonical().0[usize::from(w.0 .0)].clone();
        assert_eq!(after.price, before.price);
        assert!(matches!(after.source, SourceKind::Canonical));
        let pend = f.take_pending();
        assert!(
            !pend.is_empty(),
            "0.5% vs $1000 is 50 bps on 50-bps WETH feeds; got {pend:?}"
        );
        assert_ne!(pend[0].confidence, Confidence::CERTAIN);
        let fwd = f.best_forward().0[usize::from(w.0 .0)].clone();
        assert!(matches!(fwd.source, SourceKind::Predicted { .. }));
        let over = f.vector_at(Confidence(1));
        assert!(matches!(
            over.0[usize::from(w.0 .0)].source,
            SourceKind::Predicted { .. }
        ));
        let cert = f.vector_at(Confidence::CERTAIN);
        assert!(matches!(
            cert.0[usize::from(w.0 .0)].source,
            SourceKind::Canonical
        ));
        assert!(!f.is_stale(w.0, 1_700_000_000));
        assert!(f.is_stale(w.0, 1_700_000_000 + 5401));
        let _ = answer_to_ray(I256::try_from(100_000_000_000i64).unwrap(), 8).unwrap();
    }

    /// Oracle: unknown venue fails at bind, not silently skipped.
    #[test]
    fn unknown_venue_fails_bind() {
        let root = workspace_root();
        let reg = Registry::from_path(&root.join("registry/registry.json")).unwrap();
        let mut feeds = FeedsConfig::load(&root.join("config/feeds")).unwrap();
        feeds.feeds[0].sources = vec!["ftx:ETHUSDT".into()];
        let intern = Intern::from_registry(&reg).unwrap();
        let set = FeedSet::bind(&feeds, &intern, &reg).unwrap();
        let (book, _) = CanonicalBook::new(set, &intern, &reg).unwrap();
        assert!(matches!(
            split_fusion(book),
            Err(OracleError::UnknownCexVenue(_))
        ));
    }
}
