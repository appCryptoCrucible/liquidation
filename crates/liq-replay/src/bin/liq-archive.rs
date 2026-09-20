//! Extract C3-filtered logs + W actuals to `data/archive/`.

use std::path::PathBuf;

use liq_config::{Intern, Registry};
use liq_replay::archive::{
    archive_rpc_url, connect_http, extract_window, load_c3_addresses, ExtractError,
};
use liq_watch::decode::WatchDecoder;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(e) = run().await {
        tracing::error!(error = %e, "liq-archive failed");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), ExtractError> {
    let mut args = std::env::args().skip(1);
    let from: u64 = args
        .next()
        .ok_or_else(|| ExtractError::Filter("usage: liq-archive FROM TO".into()))?
        .parse()
        .map_err(|_| ExtractError::Filter("FROM".into()))?;
    let to: u64 = args
        .next()
        .ok_or_else(|| ExtractError::Filter("TO".into()))?
        .parse()
        .map_err(|_| ExtractError::Filter("TO".into()))?;
    let root = PathBuf::from(std::env::var("LIQ_ROOT").unwrap_or_else(|_| ".".into()));
    let rpc = archive_rpc_url()?;
    let c3 = load_c3_addresses(&root)?;
    let reg = Registry::from_path(&root.join("registry/registry.json"))
        .map_err(|e| ExtractError::Filter(e.to_string()))?;
    let intern = Intern::from_registry(&reg).map_err(|e| ExtractError::Filter(e.to_string()))?;
    let decoder = WatchDecoder::from_registry(&reg, intern.clone())
        .map_err(|e| ExtractError::Watch(e.to_string()))?;
    let provider = connect_http(&rpc)?;
    let out = root.join("data/archive");
    let report = extract_window(provider, &decoder, &intern, &c3, &out, from, to).await?;
    tracing::error!(
        from = report.from,
        to = report.to,
        headers = report.headers,
        events = report.events,
        prices = report.prices,
        actuals = report.actuals,
        reorgs = report.reorgs,
        "archive extract complete"
    );
    Ok(())
}
