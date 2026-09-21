//! Process entry. Startup order is fail-closed (GUIDE 00 / 17). `submit_enabled`
//! defaults false. Does not flip H4, start shadow clocks, or invent keys.

use std::path::PathBuf;
use std::process::ExitCode;

use liq_bot::lease::StatePaths;
use liq_bot::reload;
use liq_bot::shared::PROD_ALLOW_UNPINNED;
use liq_bot::startup;

fn main() -> ExitCode {
    let _ = liq_bot::alloc::hot_path_flag();
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => match rt.block_on(entry()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("liq-bot startup refused: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("liq-bot runtime refused: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn entry() -> Result<(), startup::StartupError> {
    let root = workspace_root();
    let config_dir = root.join("config");
    let cores = config_dir.join("cores.toml");
    let data = std::env::var("LIQ_STATE_DIR").map_or_else(|_| root.join("data"), PathBuf::from);
    let state = StatePaths {
        snapshot: data.join("snapshot.bin"),
        wal: data.join("wal.log"),
    };
    let started = startup::run(&config_dir, &cores, &state, PROD_ALLOW_UNPINNED).await?;
    let flag_file = config_dir.join("node.toml");
    if let Err(e) = reload::spawn_file_watch(
        flag_file.clone(),
        Arc::clone(&started.shared.submit_enabled),
    ) {
        tracing::error!(?e, "file-watch reload not armed");
        return Err(startup::StartupError::Other(e.to_string()));
    }
    if let Err(e) = reload::spawn_sighup(flag_file, Arc::clone(&started.shared.submit_enabled)) {
        tracing::error!(?e, "SIGHUP reload not armed");
        return Err(startup::StartupError::Other(e.to_string()));
    }
    tracing::info!(
        submit_enabled = started.shared.submit_enabled.get(),
        lease_held = started.shared.lease.held(),
        live_permitted = started
            .shared
            .lease
            .live_send_permitted(started.shared.submit_enabled.get()),
        cold_restart_p99 = ?liq_bot::shared::COLD_RESTART_P99,
        "liq-bot running (H4 not flipped; nonce resync ABSENT)"
    );
    let _ = started.forwarder;
    started
        .hot
        .join()
        .map_err(|_| startup::StartupError::Other("hot thread join".into()))?
        .map_err(startup::StartupError::Ingest)?;
    Ok(())
}

fn workspace_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

use std::sync::Arc;
