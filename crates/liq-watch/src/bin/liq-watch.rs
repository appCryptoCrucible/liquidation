//! Process entry: `stream` at the tip or `batch FROM TO parquet`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy_provider::{Provider, ProviderBuilder};
use liq_config::{Intern, Registry};
use liq_types::LogSubscriber;
use liq_watch::batch::{extract_range, write_parquet};
use liq_watch::decode::WatchDecoder;
use liq_watch::join::NoEngineJoin;
use liq_watch::source::{LogSource, OwnedBlock, Poll, RpcPoll, DEFAULT_PAGE_BLOCKS};
use liq_watch::stream::StreamSink;
use liq_watch::WatchError;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(e) = run().await {
        tracing::error!(error = %e, "liq-watch failed");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), WatchError> {
    let mut args = std::env::args().skip(1);
    let cmd = args
        .next()
        .ok_or_else(|| WatchError::Config("usage: liq-watch stream|batch".into()))?;
    let root = PathBuf::from(std::env::var("LIQ_ROOT").unwrap_or_else(|_| ".".into()));
    let rpc = rpc_url()?;
    let reg = Registry::from_path(&root.join("registry/registry.json"))
        .map_err(|e| WatchError::Config(e.to_string()))?;
    let intern = Intern::from_registry(&reg).map_err(|e| WatchError::Config(e.to_string()))?;
    let decoder = WatchDecoder::from_registry(&reg, intern)?;
    let url = rpc.parse().map_err(|e| WatchError::Rpc(format!("{e}")))?;
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(url);
    match cmd.as_str() {
        "stream" => stream_tip(provider, decoder, &root).await,
        "batch" => {
            let from: u64 = args
                .next()
                .ok_or_else(|| WatchError::Config("batch FROM".into()))?
                .parse()
                .map_err(|_| WatchError::Config("FROM".into()))?;
            let to: u64 = args
                .next()
                .ok_or_else(|| WatchError::Config("batch TO".into()))?
                .parse()
                .map_err(|_| WatchError::Config("TO".into()))?;
            let out = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join("actual_liquidations.parquet"));
            let (_d, rows) = extract_range(provider, &decoder, from, to).await?;
            write_parquet(&out, &rows)?;
            Ok(())
        }
        other => Err(WatchError::Config(format!("unknown command {other}"))),
    }
}

fn rpc_url() -> Result<String, WatchError> {
    std::env::var("LIQ_RPC_URL")
        .or_else(|_| std::env::var("MAINNET_RPC_URL"))
        .or_else(|_| std::env::var("RPC_URL"))
        .map_err(|_| WatchError::Rpc("set LIQ_RPC_URL or MAINNET_RPC_URL".into()))
}

async fn stream_tip<P: Provider + Clone>(
    provider: P,
    decoder: WatchDecoder,
    root: &Path,
) -> Result<(), WatchError> {
    let mut sink = StreamSink::open(&root.join("watch.sqlite"), &root.join("watch.jsonl"))?;
    let join = NoEngineJoin;
    let head = provider
        .get_block_number()
        .await
        .map_err(|e| WatchError::Rpc(e.to_string()))?;
    let filters = decoder.subscriptions();
    let mut poll = RpcPoll::new(provider, &filters, head, None, DEFAULT_PAGE_BLOCKS);
    let mut buf = OwnedBlock::default();
    loop {
        match poll.fetch_page().await? {
            Poll::Idle => tokio::time::sleep(Duration::from_millis(200)).await,
            Poll::Exhausted => tokio::time::sleep(Duration::from_millis(200)).await,
            Poll::Ready => {
                while matches!(LogSource::poll_block(&mut poll, &mut buf)?, Poll::Ready) {
                    let vol = decoder.coverage_from_oracle_logs(&buf.logs, u32::MAX).1;
                    for log in &buf.logs {
                        let (trig, _) = decoder.coverage_from_oracle_logs(&buf.logs, log.tx_index);
                        if let Some(ev) = decoder.decode_log(log, trig, vol)? {
                            sink.persist(&ev, &join)?;
                        }
                    }
                }
            }
        }
    }
}
