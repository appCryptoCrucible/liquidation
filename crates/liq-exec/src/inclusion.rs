//! Inclusion watcher (GUIDE 13 §5). Single-owner `&mut` map; outcomes on a channel.
//!
//! `LostToCompetitor` is matched on W (`liq-watch`) liquidation events by
//! `(protocol, market, user)`. Live resolution rate is **ABSENT** — no live
//! stream is measured here. Fixture tests cover the matcher.

use crate::error::{ExecError, Result};
use alloy_primitives::{Address, Bytes, B256, I256, U256};
use crossbeam_channel::{Receiver, Sender};
use liq_types::{PositionKey, TraceId};
use liq_watch::types::DecodedLiquidation;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Live `LostToCompetitor` resolution percent. Not measured (no live W stream).
pub const LOST_TO_COMPETITOR_LIVE_PCT: Option<u8> = None;

/// Terminal state for one submission (GUIDE 13 §5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Terminal {
    Included { profit: I256, gas: u64 },
    Dropped,
    Reverted { reason: Bytes },
    LostToCompetitor { tx: B256, their_bid: Option<U256> },
}

/// Tracked submission. Profit/gas for [`Terminal::Included`] come from
/// attested [`InclusionObs`] only — never guessed.
#[derive(Clone, Debug)]
pub struct Tracked {
    pub trace: TraceId,
    pub tx_hash: B256,
    pub position: PositionKey,
    pub operator: Address,
    pub min_block: u64,
    pub max_block: u64,
}

/// Attested inclusion of our tx. `profit` is required; missing profit is
/// not filled in.
#[derive(Clone, Debug)]
pub struct InclusionObs {
    pub tx_hash: B256,
    pub block: u64,
    pub gas_used: u64,
    pub profit: I256,
    pub reverted: bool,
    pub revert_reason: Bytes,
}

/// One block of observations fed to the watcher.
#[derive(Clone, Debug, Default)]
pub struct BlockObs {
    pub block: u64,
    pub inclusions: Vec<InclusionObs>,
    pub liquidations: Vec<DecodedLiquidation>,
    /// Attested competitor bids keyed by the winner's tx hash.
    pub inferred_bids: Vec<(B256, Option<U256>)>,
    /// A read succeeded but an outcome was withheld (no profit sink, no
    /// attested WETH transfer). The block must not become `Dropped`.
    pub incomplete: bool,
}

/// Commands for the single watcher task.
#[derive(Clone, Debug)]
pub enum WatchCmd {
    Track(Tracked),
    Observe(BlockObs),
    Shutdown,
}

/// Single-task watcher. Other components only see outcomes on a channel.
pub struct InclusionWatch {
    map: HashMap<TraceId, Tracked>,
}

impl Default for InclusionWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl InclusionWatch {
    #[must_use]
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub fn track(&mut self, t: Tracked) {
        self.map.insert(t.trace, t);
    }

    #[must_use]
    pub fn unresolved(&self) -> usize {
        self.map.len()
    }

    #[must_use]
    pub fn open_tracks(&self) -> Vec<Tracked> {
        self.map.values().cloned().collect()
    }

    /// Apply one block. Resolved traces are removed and returned.
    pub fn observe(&mut self, obs: &BlockObs) -> Result<Vec<(TraceId, Terminal)>> {
        let mut out = Vec::new();
        let mut resolved = Vec::new();
        for (trace, t) in &self.map {
            if let Some(term) = resolve_one(t, obs)? {
                out.push((*trace, term));
                resolved.push(*trace);
            } else if !obs.incomplete && obs.block > t.max_block {
                out.push((*trace, Terminal::Dropped));
                resolved.push(*trace);
            }
        }
        for k in resolved {
            self.map.remove(&k);
        }
        Ok(out)
    }
}

fn resolve_one(t: &Tracked, obs: &BlockObs) -> Result<Option<Terminal>> {
    if obs.block < t.min_block {
        return Ok(None);
    }
    for inc in &obs.inclusions {
        if inc.tx_hash != t.tx_hash {
            continue;
        }
        if inc.reverted {
            return Ok(Some(Terminal::Reverted {
                reason: inc.revert_reason.clone(),
            }));
        }
        return Ok(Some(Terminal::Included {
            profit: inc.profit,
            gas: inc.gas_used,
        }));
    }
    for ev in &obs.liquidations {
        if let Some(term) = match_competitor(t, ev, &obs.inferred_bids)? {
            return Ok(Some(term));
        }
    }
    Ok(None)
}

/// Match a W liquidation onto our tracked position. Same user/market/protocol,
/// different liquidator → [`Terminal::LostToCompetitor`].
pub fn match_competitor(
    t: &Tracked,
    ev: &DecodedLiquidation,
    inferred_bids: &[(B256, Option<U256>)],
) -> Result<Option<Terminal>> {
    if ev.protocol != t.position.protocol.0 {
        return Ok(None);
    }
    if ev.market != t.position.market.0 {
        return Ok(None);
    }
    if ev.user != t.position.user {
        return Ok(None);
    }
    if ev.liquidator == t.operator {
        return Ok(None);
    }
    if ev.block < t.min_block || ev.block > t.max_block {
        return Ok(None);
    }
    let their_bid = inferred_bids
        .iter()
        .find(|(h, _)| *h == ev.tx_hash)
        .and_then(|(_, b)| *b);
    Ok(Some(Terminal::LostToCompetitor {
        tx: ev.tx_hash,
        their_bid,
    }))
}

/// Parse an attested `inferred_bid` decimal string. Missing → `Ok(None)`.
pub fn parse_inferred_bid(s: Option<&str>) -> Result<Option<U256>> {
    let Some(s) = s else {
        return Ok(None);
    };
    s.parse::<U256>()
        .map(Some)
        .map_err(|_| ExecError::BadInferredBid)
}

/// Off-thread block reader. `None` means this poll failed or saw nothing —
/// the watcher must not advance, or a missed block becomes a false `Dropped`.
pub trait BlockSource: Send {
    fn poll(&mut self, open: &[Tracked]) -> Option<BlockObs>;
}

/// Single watcher thread. Outcomes are `try_send`; a full channel is counted.
pub fn spawn_watch(
    cmds: Receiver<WatchCmd>,
    outcomes: Sender<(TraceId, Terminal)>,
    outcome_full: std::sync::Arc<AtomicU64>,
) -> Result<std::thread::JoinHandle<()>> {
    spawn_sourced(cmds, outcomes, outcome_full, None)
}

/// Same watcher. `source` is polled when the command channel is idle.
/// A source that returns `None` does not resolve anything.
pub fn spawn_sourced(
    cmds: Receiver<WatchCmd>,
    outcomes: Sender<(TraceId, Terminal)>,
    outcome_full: std::sync::Arc<AtomicU64>,
    mut source: Option<Box<dyn BlockSource>>,
) -> Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("liq-exec-inclusion".into())
        .spawn(move || {
            let mut watch = InclusionWatch::new();
            if source.is_none() {
                tracing::error!("inclusion block feed unwired — outcome counts are not a win rate");
            }
            loop {
                let cmd = match cmds.recv_timeout(Duration::from_secs(2)) {
                    Ok(c) => Some(c),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                };
                if let Some(cmd) = cmd {
                    match cmd {
                        WatchCmd::Track(t) => watch.track(t),
                        WatchCmd::Observe(obs) => emit(&mut watch, &obs, &outcomes, &outcome_full),
                        WatchCmd::Shutdown => break,
                    }
                }
                if let Some(src) = source.as_mut() {
                    let open = watch.open_tracks();
                    if let Some(obs) = src.poll(&open) {
                        emit(&mut watch, &obs, &outcomes, &outcome_full);
                    }
                }
            }
        })
        .map_err(|e| ExecError::Config(e.to_string()))
}

fn emit(
    watch: &mut InclusionWatch,
    obs: &BlockObs,
    outcomes: &Sender<(TraceId, Terminal)>,
    outcome_full: &std::sync::Arc<AtomicU64>,
) {
    match watch.observe(obs) {
        Ok(rows) => {
            for row in rows {
                if outcomes.try_send(row).is_err() {
                    outcome_full.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Err(e) => {
            tracing::error!(err = %e, "inclusion observe failed");
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256};
    use liq_types::{MarketId, PositionKey, ProtocolId};
    use liq_watch::types::{CoverageDims, TriggerClass};

    fn tracked() -> Tracked {
        Tracked {
            trace: TraceId::from_raw(1),
            tx_hash: b256!("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            position: PositionKey {
                protocol: ProtocolId(1),
                market: MarketId(2),
                user: address!("0x1111111111111111111111111111111111111111"),
            },
            operator: address!("0x2222222222222222222222222222222222222222"),
            min_block: 100,
            max_block: 102,
        }
    }

    fn liq(liquidator: Address, user: Address, tx: B256, block: u64) -> DecodedLiquidation {
        DecodedLiquidation {
            family: "fixture".into(),
            instance: "core".into(),
            protocol: 1,
            market: 2,
            block,
            block_hash: B256::ZERO,
            tx_hash: tx,
            tx_index: 1,
            log_index: 0,
            user,
            liquidator,
            repay_asset: Address::ZERO,
            repay_amount: "1".into(),
            seize_asset: Address::ZERO,
            seize_amount: "1".into(),
            raw: serde_json::json!({}),
            coverage: CoverageDims {
                instance: "core".into(),
                collateral_family: "weth".into(),
                trigger_class: TriggerClass::Unobserved,
                realized_vol_ray: "0".into(),
            },
        }
    }

    #[test]
    fn lost_to_competitor_on_fixture() {
        let t = tracked();
        let win = b256!("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let ev = liq(
            address!("0x3333333333333333333333333333333333333333"),
            t.position.user,
            win,
            101,
        );
        let bid = U256::from(42u64);
        let term = match_competitor(&t, &ev, &[(win, Some(bid))])
            .unwrap()
            .unwrap();
        assert_eq!(
            term,
            Terminal::LostToCompetitor {
                tx: win,
                their_bid: Some(bid)
            }
        );
        assert!(
            LOST_TO_COMPETITOR_LIVE_PCT.is_none(),
            "live % is ABSENT — do not invent"
        );
    }

    #[test]
    fn our_own_liquidation_is_not_competitor() {
        let t = tracked();
        let ev = liq(t.operator, t.position.user, t.tx_hash, 101);
        assert!(match_competitor(&t, &ev, &[]).unwrap().is_none());
    }

    #[test]
    fn included_and_dropped_and_reverted() {
        let mut w = InclusionWatch::new();
        w.track(tracked());
        let inc = InclusionObs {
            tx_hash: tracked().tx_hash,
            block: 101,
            gas_used: 180_000,
            profit: I256::try_from(5i64).unwrap(),
            reverted: false,
            revert_reason: Bytes::new(),
        };
        let out = w
            .observe(&BlockObs {
                block: 101,
                inclusions: vec![inc],
                ..BlockObs::default()
            })
            .unwrap();
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].1, Terminal::Included { gas: 180_000, .. }));
        assert_eq!(w.unresolved(), 0);

        w.track(tracked());
        let out = w
            .observe(&BlockObs {
                block: 103,
                ..BlockObs::default()
            })
            .unwrap();
        assert_eq!(out[0].1, Terminal::Dropped);

        let mut t = tracked();
        t.trace = TraceId::from_raw(9);
        w.track(t.clone());
        let out = w
            .observe(&BlockObs {
                block: 101,
                inclusions: vec![InclusionObs {
                    tx_hash: t.tx_hash,
                    block: 101,
                    gas_used: 1,
                    profit: I256::ZERO,
                    reverted: true,
                    revert_reason: Bytes::from_static(&[0x01]),
                }],
                ..BlockObs::default()
            })
            .unwrap();
        assert!(matches!(out[0].1, Terminal::Reverted { .. }));
    }

    #[test]
    fn incomplete_observation_is_not_a_drop() {
        let mut w = InclusionWatch::new();
        w.track(tracked());
        let out = w
            .observe(&BlockObs {
                block: 103,
                incomplete: true,
                ..BlockObs::default()
            })
            .unwrap();
        assert!(out.is_empty());
        assert_eq!(w.unresolved(), 1);
    }

    #[test]
    fn inferred_bid_missing_is_none_not_guessed() {
        assert_eq!(parse_inferred_bid(None).unwrap(), None);
        assert_eq!(
            parse_inferred_bid(Some("99")).unwrap(),
            Some(U256::from(99u64))
        );
        assert!(parse_inferred_bid(Some("nope")).is_err());
    }
}
