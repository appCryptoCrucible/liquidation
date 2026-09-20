//! GUIDE 16 §6 harness binary: print the report, gate vs baseline, optional file.

fn main() -> Result<(), i32> {
    let r = liq_replay::bench::run();
    if let Err(e) = liq_replay::bench::maybe_write(&r) {
        eprintln!("LIQ_BENCH_REPORT write failed: {e}");
        return Err(1);
    }
    print!("{}", liq_replay::bench::encode(&r));
    match liq_replay::bench::check(&r, liq_replay::bench::BASELINE_TEXT) {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("bench gate: {e:?}");
            Err(1)
        }
    }
}
