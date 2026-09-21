//! Hot-reload of `submit_enabled` (GUIDE 17 §1b). SIGHUP (unix) + file watch.
//! Reloads only the flag — does not re-assert the registry.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{Builder, JoinHandle};
use std::time::Duration;

use liq_exec::submit::SubmitEnabled;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReloadError {
    #[error("reload io: {0}")]
    Io(#[from] std::io::Error),
    #[error("reload toml: {0}")]
    Toml(String),
    #[error("reload watch: {0}")]
    Watch(String),
}

#[derive(Deserialize)]
struct FlagFile {
    #[serde(default)]
    submit_enabled: bool,
}

/// Read `submit_enabled` from TOML and [`SubmitEnabled::set`]. Default false.
pub fn apply_file(path: &Path, flag: &SubmitEnabled) -> Result<bool, ReloadError> {
    let raw = std::fs::read_to_string(path)?;
    let parsed: FlagFile = toml::from_str(&raw).map_err(|e| ReloadError::Toml(e.to_string()))?;
    flag.set(parsed.submit_enabled);
    tracing::info!(
        path = %path.display(),
        submit_enabled = parsed.submit_enabled,
        "submit_enabled reloaded"
    );
    Ok(parsed.submit_enabled)
}

/// Watch `path` (the file). On write/create, re-read and `set`.
pub fn spawn_file_watch(
    path: PathBuf,
    flag: Arc<SubmitEnabled>,
) -> Result<JoinHandle<()>, ReloadError> {
    let watch_path = path.clone();
    let handle = Builder::new()
        .name("liq-bot-reload".into())
        .spawn(move || {
            if let Err(e) = watch_loop(watch_path, flag) {
                tracing::error!(?e, "submit_enabled file watch exited");
            }
        })
        .map_err(ReloadError::Io)?;
    Ok(handle)
}

fn watch_loop(path: PathBuf, flag: Arc<SubmitEnabled>) -> Result<(), ReloadError> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        tx,
        notify::Config::default().with_poll_interval(Duration::from_secs(1)),
    )
    .map_err(|e| ReloadError::Watch(e.to_string()))?;
    let parent = path
        .parent()
        .ok_or_else(|| ReloadError::Watch("submit_enabled path has no parent".into()))?;
    watcher
        .watch(parent, RecursiveMode::NonRecursive)
        .map_err(|e| ReloadError::Watch(e.to_string()))?;
    for ev in rx {
        let Ok(ev) = ev else {
            continue;
        };
        let hit = ev.paths.iter().any(|p| p == &path);
        if !hit {
            continue;
        }
        match apply_file(&path, &flag) {
            Ok(_) => {}
            Err(e) => tracing::error!(?e, "submit_enabled reload refused"),
        }
    }
    Ok(())
}

/// Unix SIGHUP → re-read `path` and `set`. No-op compile on Windows.
#[cfg(unix)]
pub fn spawn_sighup(
    path: PathBuf,
    flag: Arc<SubmitEnabled>,
) -> Result<Option<JoinHandle<()>>, ReloadError> {
    let handle = Builder::new()
        .name("liq-bot-sighup".into())
        .spawn(move || {
            let mut signals =
                match signal_hook::iterator::Signals::new([signal_hook::consts::SIGHUP]) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(?e, "SIGHUP iterator failed");
                        return;
                    }
                };
            for _ in &mut signals {
                match apply_file(&path, &flag) {
                    Ok(_) => {}
                    Err(e) => tracing::error!(?e, "SIGHUP submit_enabled reload refused"),
                }
            }
        })
        .map_err(ReloadError::Io)?;
    Ok(Some(handle))
}

#[cfg(not(unix))]
pub fn spawn_sighup(
    _path: PathBuf,
    _flag: Arc<SubmitEnabled>,
) -> Result<Option<JoinHandle<()>>, ReloadError> {
    tracing::info!("SIGHUP reload is unix-only; file-watch is the Windows path");
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering as AtomicOrdering;

    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn tmp() -> PathBuf {
        let n = N.fetch_add(1, AtomicOrdering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("liq-17a-reload-{}-{}.toml", std::process::id(), n));
        p
    }

    #[test]
    fn apply_file_defaults_false_and_flips() {
        let path = tmp();
        std::fs::write(&path, "chain_id = 1\n").unwrap();
        let flag = SubmitEnabled::new(true);
        assert!(!apply_file(&path, &flag).unwrap());
        assert!(!flag.get());
        std::fs::write(&path, "submit_enabled = true\n").unwrap();
        assert!(apply_file(&path, &flag).unwrap());
        assert!(flag.get());
        std::fs::write(&path, "submit_enabled = false\n").unwrap();
        assert!(!apply_file(&path, &flag).unwrap());
        assert!(!flag.get());
        let _ = std::fs::remove_file(&path);
    }
}
