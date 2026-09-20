//! `--quick` (1k) / `--full` (100k) Foundry differential gate.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::env;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let full = env::args().any(|a| a == "--full");
    let runs = if full { "100000" } else { "1000" };
    let status = Command::new("forge")
        .args([
            "test",
            "--match-path",
            "test/differential/HealthDiff.t.sol",
            "--fuzz-runs",
            runs,
            "-vv",
        ])
        .current_dir("contracts")
        .env(
            "LIQ_DIFF_BIN",
            env::var("LIQ_DIFF_BIN").unwrap_or_else(|_| default_bin()),
        )
        .status();
    match status {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        Ok(s) => {
            eprintln!("forge exited {s}");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("forge not runnable: {e}");
            ExitCode::from(1)
        }
    }
}

fn default_bin() -> String {
    let exe = if cfg!(windows) {
        "liq-diff-health.exe"
    } else {
        "liq-diff-health"
    };
    format!("../target/debug/{exe}")
}
