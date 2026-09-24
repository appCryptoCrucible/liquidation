//! Inclusion block feed (GUIDE 13 §5).
//!
//! Receipts and liquidation logs come from `rpc_url`. A failed read returns
//! no observation, so a missed block is not recorded as `Dropped`.
//! A successful receipt is `Included` only when `profit_sink` is set and the
//! receipt contains a WETH transfer to it. Competitor bids use
//! [`liq_watch::batch::attested_inferred_bid`]; a missing component stays
//! `None`.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, I256, U256};
use alloy_provider::{Provider, ProviderBuilder, RootProvider};
use alloy_sol_types::SolEvent;
use crossbeam_channel::{Receiver, Sender};
use liq_config::{Intern, Registry};
use liq_exec::inclusion::Terminal;
use liq_exec::inclusion::{spawn_sourced, BlockObs, BlockSource, InclusionObs, Tracked, WatchCmd};
use liq_risk::{OutcomeRow, PnlLedger};
use liq_watch::batch::attested_inferred_bid;
use liq_watch::source::{LogSource, OwnedBlock, Poll, RpcPoll};
use liq_watch::WatchDecoder;
use tokio::runtime::Runtime;

use crate::bind::REGISTRY_WETH;

alloy_sol_types::sol! {
    event Transfer(address indexed from, address indexed to, uint256 value);
}

pub struct InclusionJoin {
    pub cmd_tx: Sender<WatchCmd>,
    _watch: JoinHandle<()>,
    _outcomes: JoinHandle<()>,
}

struct RpcFeed {
    _rt: Runtime,
    handle: tokio::runtime::Handle,
    provider: RootProvider,
    decoder: WatchDecoder,
    profit_sink: Option<Address>,
    /// Next block to read. `0` means "not started" — the first head is the
    /// start, so history is not back-filled into the watcher.
    cursor: u64,
}

impl BlockSource for RpcFeed {
    fn poll(&mut self, open: &[Tracked]) -> Option<BlockObs> {
        let handle = self.handle.clone();
        match handle.block_on(self.poll_async(open)) {
            Ok(obs) => obs,
            Err(e) => {
                tracing::error!(error = %e, "inclusion feed read failed — block not applied");
                None
            }
        }
    }
}

impl RpcFeed {
    async fn poll_async(&mut self, open: &[Tracked]) -> Result<Option<BlockObs>, String> {
        let head = self
            .provider
            .get_block_number()
            .await
            .map_err(|e| e.to_string())?;
        if head == 0 {
            return Ok(None);
        }
        if self.cursor > 1 && head < self.cursor.saturating_sub(1) {
            tracing::error!(head, cursor = self.cursor, "inclusion head moved backwards");
            self.cursor = head;
            return Ok(None);
        }
        let from = if self.cursor == 0 { head } else { self.cursor };
        let mut obs = BlockObs {
            block: head,
            ..BlockObs::default()
        };
        for t in open {
            match self.receipt_obs(t).await {
                Ok(ReceiptRead::Observed(inc)) => obs.inclusions.push(inc),
                Ok(ReceiptRead::NotMined) => {}
                Ok(ReceiptRead::Withheld) => obs.incomplete = true,
                Err(e) => {
                    tracing::error!(error = %e, tx = %t.tx_hash, "receipt unread — block not applied");
                    return Ok(None);
                }
            }
        }
        if head >= from {
            let filters = self.decoder.subscriptions();
            let mut poll = RpcPoll::new(self.provider.clone(), &filters, from, Some(head), 8);
            let mut buf = OwnedBlock::default();
            loop {
                match poll.fetch_page().await {
                    Ok(Poll::Exhausted) => break,
                    Ok(Poll::Idle) => {
                        if poll.cursor() > head {
                            break;
                        }
                    }
                    Ok(Poll::Ready) => {
                        while matches!(LogSource::poll_block(&mut poll, &mut buf), Ok(Poll::Ready))
                        {
                            self.push_liquidations(&buf, &mut obs).await;
                        }
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
            self.cursor = head.saturating_add(1);
        }
        Ok(Some(obs))
    }

    async fn push_liquidations(&self, buf: &OwnedBlock, obs: &mut BlockObs) {
        for log in &buf.logs {
            let (trig, vol) = self
                .decoder
                .coverage_from_oracle_logs(&buf.logs, log.tx_index);
            let ev = match self.decoder.decode_log(log, trig, vol) {
                Ok(Some(ev)) => ev,
                Ok(None) => continue,
                Err(e) => {
                    tracing::error!(error = %e, "liquidation log undecoded");
                    continue;
                }
            };
            let bid = match attested_inferred_bid(&self.provider, ev.tx_hash, ev.block).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(error = %e, tx = %ev.tx_hash, "competitor bid withheld");
                    None
                }
            };
            if let Some(b) = bid {
                obs.inferred_bids.push((ev.tx_hash, Some(b)));
            }
            obs.liquidations.push(ev);
        }
    }

    async fn receipt_obs(&self, t: &Tracked) -> Result<ReceiptRead, String> {
        let receipt = self
            .provider
            .get_transaction_receipt(t.tx_hash)
            .await
            .map_err(|e| e.to_string())?;
        let Some(receipt) = receipt else {
            return Ok(ReceiptRead::NotMined);
        };
        let gas = receipt.gas_used;
        let Some(block) = receipt.block_number else {
            tracing::error!(tx = %t.tx_hash, "receipt missing block number");
            return Ok(ReceiptRead::Withheld);
        };
        if !receipt.status() {
            tracing::error!(
                tx = %t.tx_hash,
                "receipt reverted; revert payload is not in the receipt"
            );
            return Ok(ReceiptRead::Observed(InclusionObs {
                tx_hash: t.tx_hash,
                block,
                gas_used: gas,
                profit: I256::ZERO,
                reverted: true,
                revert_reason: alloy_primitives::Bytes::new(),
            }));
        }
        let Some(sink) = self.profit_sink else {
            tracing::error!(tx = %t.tx_hash, "profit sink unset — successful receipt not marked Included");
            return Ok(ReceiptRead::Withheld);
        };
        let Some(profit) = weth_to_sink(&receipt, sink) else {
            tracing::error!(tx = %t.tx_hash, "no WETH transfer to profit sink — Included withheld");
            return Ok(ReceiptRead::Withheld);
        };
        Ok(ReceiptRead::Observed(InclusionObs {
            tx_hash: t.tx_hash,
            block,
            gas_used: gas,
            profit,
            reverted: false,
            revert_reason: alloy_primitives::Bytes::new(),
        }))
    }
}

enum ReceiptRead {
    Observed(InclusionObs),
    NotMined,
    Withheld,
}

fn weth_to_sink(receipt: &alloy_rpc_types_eth::TransactionReceipt, sink: Address) -> Option<I256> {
    let topic = Transfer::SIGNATURE_HASH;
    let mut sum = U256::ZERO;
    let mut any = false;
    for log in receipt.logs() {
        if log.address() != REGISTRY_WETH {
            continue;
        }
        let topics = log.topics();
        if topics.first() != Some(&topic) {
            continue;
        }
        let Ok(ev) = Transfer::decode_raw_log(topics.iter().copied(), &log.data().data) else {
            continue;
        };
        if ev.to != sink {
            continue;
        }
        sum = sum.checked_add(ev.value)?;
        any = true;
    }
    if !any || sum.bit(255) {
        return None;
    }
    Some(I256::from_raw(sum))
}

/// `None` when the decoder or the runtime cannot be built. The caller then
/// leaves the watch channel unset.
///
/// `ledger_path`: SQLite file the resolved-outcome rows are appended to,
/// off the hot path (this fn's own `liq-bot-inclusion` thread, never
/// `liq-node-hot`). `None` — including a path that fails to open — logs and
/// runs the watcher without a ledger; a missing ledger never blocks
/// inclusion tracking.
pub fn start(
    rpc_url: &str,
    reg: &Registry,
    intern: Intern,
    profit_sink: Option<Address>,
    ledger_path: Option<&str>,
) -> Option<InclusionJoin> {
    if rpc_url.is_empty() {
        tracing::error!("inclusion feed refused — empty rpc_url");
        return None;
    }
    let url = match rpc_url.parse() {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "inclusion feed refused — rpc_url");
            return None;
        }
    };
    let decoder = match WatchDecoder::from_registry(reg, intern) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!(error = %e, "inclusion feed refused — decoder");
            return None;
        }
    };
    let rt = match Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "inclusion feed refused — runtime");
            return None;
        }
    };
    let handle = rt.handle().clone();
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(url);
    let feed = RpcFeed {
        _rt: rt,
        handle,
        provider,
        decoder,
        profit_sink: profit_sink.filter(|a| !a.is_zero()),
        cursor: 0,
    };
    if feed.profit_sink.is_none() {
        tracing::error!(
            "profit sink unset — successful receipts stay unresolved until one is configured"
        );
    }
    let (cmd_tx, cmd_rx) = crossbeam_channel::bounded(1024);
    let (out_tx, out_rx) = crossbeam_channel::bounded(1024);
    let full = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let watch = match spawn_sourced(cmd_rx, out_tx, Arc::clone(&full), Some(Box::new(feed))) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "inclusion watcher not started");
            return None;
        }
    };
    let ledger = ledger_path.and_then(|p| match PnlLedger::open(p) {
        Ok(l) => Some(l),
        Err(e) => {
            tracing::error!(error = %e, path = p, "PnL ledger not opened — outcomes logged only");
            None
        }
    });
    let outcomes = match std::thread::Builder::new()
        .name("liq-bot-inclusion".into())
        .spawn(move || log_outcomes(out_rx, full, ledger))
    {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "inclusion outcome thread not started");
            return None;
        }
    };
    Some(InclusionJoin {
        cmd_tx,
        _watch: watch,
        _outcomes: outcomes,
    })
}

/// Runs off the hot path on its own thread (`liq-bot-inclusion`) — the
/// SQLite write below never touches `liq-node-hot`.
fn log_outcomes(
    rx: Receiver<(Tracked, Terminal)>,
    full: Arc<std::sync::atomic::AtomicU64>,
    ledger: Option<PnlLedger>,
) {
    let ts_unix = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    };
    while let Ok((tracked, term)) = rx.recv() {
        let trace = tracked.trace;
        let (outcome, net_wei, gas_used) = match &term {
            Terminal::Included { profit, gas } => {
                tracing::info!(trace = trace.raw(), %profit, gas, "inclusion WON");
                ("Won", Some(*profit), Some(*gas))
            }
            Terminal::Dropped => {
                tracing::info!(trace = trace.raw(), "inclusion DROPPED");
                ("Dropped", None, None)
            }
            Terminal::Reverted { .. } => {
                tracing::info!(trace = trace.raw(), "inclusion REVERTED");
                ("Reverted", None, None)
            }
            Terminal::LostToCompetitor { tx, their_bid } => {
                tracing::info!(trace = trace.raw(), %tx, ?their_bid, "inclusion LOST");
                ("LostToCompetitor", None, None)
            }
        };
        if let Some(l) = &ledger {
            if let Err(e) = l.insert_outcome(&OutcomeRow {
                trace,
                ts_unix: ts_unix(),
                protocol: tracked.position.protocol,
                outcome: outcome.into(),
                net_wei,
                gas_used,
            }) {
                tracing::error!(error = %e, trace = trace.raw(), "PnL outcome row not written");
            }
        }
        let n = full.load(std::sync::atomic::Ordering::Relaxed);
        if n != 0 {
            tracing::error!(dropped = n, "inclusion outcome channel full");
        }
    }
}
