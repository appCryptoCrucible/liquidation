//! Books scanner. Refuses to start without `EXECUTOR_ADDRESS` and `RPC_URL`.
//! Does not write a row unless every GUIDE 14 component is present.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use liq_books::{parse_address, scan_once};

fn main() -> ExitCode {
    let executor_raw = env::var("EXECUTOR_ADDRESS").unwrap_or_default();
    let rpc = env::var("RPC_URL").unwrap_or_default();
    let Some(executor) = parse_address(executor_raw.trim()) else {
        eprintln!("liq-books: EXECUTOR_ADDRESS unset or zero — not scanning");
        return ExitCode::from(1);
    };
    if rpc.is_empty() {
        eprintln!("liq-books: RPC_URL unset — not scanning");
        return ExitCode::from(1);
    }
    let profit_sink = match env::var("PROFIT_SINK") {
        Err(_) => None,
        Ok(raw) if raw.trim().is_empty() => None,
        Ok(raw) => match parse_address(raw.trim()) {
            Some(a) => Some(a),
            None => {
                eprintln!(
                    "liq-books: PROFIT_SINK is set but not a non-zero address — not scanning"
                );
                return ExitCode::from(1);
            }
        },
    };
    if profit_sink.is_none() {
        eprintln!("liq-books: PROFIT_SINK unset — net_retained_wei cannot be attested; rows will be skipped");
    }
    let books = env::var("BOOKS_DIR").unwrap_or_else(|_| "books".into());
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("liq-books: runtime: {e}");
            return ExitCode::from(1);
        }
    };
    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("liq-books: http client: {e}");
            return ExitCode::from(1);
        }
    };
    match rt.block_on(scan_once(
        &client,
        &rpc,
        &PathBuf::from(books),
        executor,
        profit_sink,
    )) {
        Ok(out) => {
            for skip in out.skipped {
                eprintln!(
                    "liq-books: skip {} missing {}",
                    skip.tx_hash,
                    skip.missing.join(",")
                );
            }
            eprintln!(
                "liq-books: finalized {} appended {}",
                out.finalized, out.appended
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("liq-books: {e}");
            ExitCode::from(1)
        }
    }
}
