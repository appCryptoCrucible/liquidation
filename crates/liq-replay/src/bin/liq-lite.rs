//! 05F lite validation smoke. `LIQ_RPC_URL` required. Clears nothing.

use std::path::PathBuf;

use liq_replay::lite::{run, LiteError};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(e) = go().await {
        tracing::error!(error = %e, "liq-lite failed");
        eprintln!("liq-lite: {e}");
        std::process::exit(1);
    }
}

async fn go() -> Result<(), LiteError> {
    let root = PathBuf::from(std::env::var("LIQ_ROOT").unwrap_or_else(|_| ".".into()));
    let report = run(&root).await?;
    eprintln!(
        "05F lite smoke (clears nothing; not the Recall gate)\n\
         instance={} spoke={:#x}\n\
         window={}..={}\n\
         universe={} flags={} liquidations={}\n\
         flagged_and_liquidated={}\n\
         liquidated_never_flagged (outside universe)={}\n\
         flagged_nobody_liquidated={}\n\
         flagged_and_declined={}",
        report.instance,
        report.spoke,
        report.from,
        report.to,
        report.universe,
        report.flags.len(),
        report.liquidations,
        report.matches.flagged_and_liquidated.len(),
        report.matches.liquidated_never_flagged.len(),
        report.matches.flagged_nobody_liquidated.len(),
        report.matches.flagged_and_declined.len(),
    );
    Ok(())
}
