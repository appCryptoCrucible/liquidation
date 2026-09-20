//! FFI entry for Foundry: `liq-diff-health --case 0x…` → ABI `(uint256,uint256,uint256)`.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;

use liq_replay::diff::{adapter_view, decode_case, encode_view};

fn main() -> ExitCode {
    match run() {
        Ok(bytes) => {
            let mut out = io::stdout().lock();
            if write_hex(&mut out, &bytes).is_err() {
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("liq-diff-health: {e}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<Vec<u8>, String> {
    let mut args = env::args().skip(1);
    let mut case = None;
    while let Some(a) = args.next() {
        if a == "--case" {
            case = args.next();
        }
    }
    let raw = case.ok_or_else(|| "usage: liq-diff-health --case <hex>".to_owned())?;
    let c = decode_case(raw.as_bytes()).map_err(|e| e.to_string())?;
    let v = adapter_view(&c).map_err(|e| e.to_string())?;
    Ok(encode_view(
        v.health_factor_wad,
        v.total_collateral_value,
        v.total_debt_value_wad,
    ))
}

fn write_hex(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    write!(out, "0x")?;
    for b in bytes {
        write!(out, "{b:02x}")?;
    }
    Ok(())
}
