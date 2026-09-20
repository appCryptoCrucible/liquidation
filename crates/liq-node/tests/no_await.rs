//! GUIDE 03 §4b: the hot fold is synchronous. `apply.rs`, `router.rs`,
//! `decode.rs` and `dirty.rs` must contain no `.await` and no `async fn` at
//! all; `source.rs` holds the async `RpcPoll` (backfill / 05F / 05B) but its
//! `LogSource` impls — what the hot thread drains — must stay sync.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::Path;

/// Source with line comments removed, so a doc comment that *mentions*
/// `.await` cannot satisfy or trip the check. Line numbers are preserved.
fn code_lines(text: &str) -> Vec<(usize, String)> {
    text.lines()
        .enumerate()
        .map(|(i, l)| {
            let code = match l.find("//") {
                Some(at) => l.get(..at).unwrap_or(""),
                None => l,
            };
            (i.saturating_add(1), code.to_owned())
        })
        .collect()
}

fn awaits(lines: &[(usize, String)]) -> Vec<usize> {
    lines
        .iter()
        .filter(|(_, l)| l.contains(".await"))
        .map(|(n, _)| *n)
        .collect()
}

fn read(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The lines of every `impl <trait> for` block named by `trait_name`,
/// brace-matched from the `impl` line.
fn impl_blocks(lines: &[(usize, String)], trait_name: &str) -> Vec<(usize, String)> {
    // `impl LogSource for X` and `impl<P> LogSource for X<P>` both count.
    let needle = format!("{trait_name} for ");
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut inside = false;
    for (n, l) in lines {
        if !inside && l.trim_start().starts_with("impl") && l.contains(&needle) {
            inside = true;
            depth = 0;
        }
        if inside {
            out.push((*n, l.clone()));
            depth = depth
                .saturating_add(i32::try_from(l.matches('{').count()).unwrap_or(0))
                .saturating_sub(i32::try_from(l.matches('}').count()).unwrap_or(0));
            if depth <= 0 && l.contains('}') {
                inside = false;
            }
        }
    }
    out
}

/// Oracle: GUIDE 03 §4b / the WP 03A acceptance line "no `.await` anywhere
/// inside `apply_chain`". Negative: `must_detect_an_injected_await` proves
/// the detector is not vacuous.
#[test]
fn sync_ingest_modules_have_no_await() {
    for name in [
        "apply.rs",
        "router.rs",
        "decode.rs",
        "dirty.rs",
        "exex.rs",
        "hot.rs",
        "reorg.rs",
        "mempool.rs",
    ] {
        let lines = code_lines(&read(name));
        let hits = awaits(&lines);
        assert!(
            hits.is_empty(),
            "{name} is on the hot path and contains .await at lines {hits:?}"
        );
        let asyncs: Vec<usize> = lines
            .iter()
            .filter(|(_, l)| {
                l.contains("async fn") || l.contains("async move") || l.contains("async {")
            })
            .map(|(n, _)| *n)
            .collect();
        assert!(
            asyncs.is_empty(),
            "{name} declares an async item at lines {asyncs:?}; the fold is sync"
        );
    }
}

/// Oracle: GUIDE 03 §2/§5 — `source.rs` legitimately holds the async
/// `RpcPoll` (backfill, 05F, 05B), but `LogSource::poll_block` is what the
/// pinned hot thread calls, so no `LogSource` impl may await. Negative: the
/// file as a whole *does* contain `.await`, so a blanket file grep here
/// would be the wrong assertion and is deliberately not made.
#[test]
fn log_source_impls_in_source_rs_are_sync() {
    let lines = code_lines(&read("source.rs"));
    assert!(
        !awaits(&lines).is_empty(),
        "source.rs no longer has any .await — re-check that RpcPoll still does its own I/O"
    );
    let impls = impl_blocks(&lines, "LogSource");
    assert!(
        impls.len() >= 2,
        "expected the ExExSource and RpcPoll LogSource impls, found {}",
        impls.len()
    );
    let hits = awaits(&impls);
    assert!(
        hits.is_empty(),
        "a LogSource impl awaits at lines {hits:?}; poll_block must stay sync"
    );
    let async_in_impl: Vec<usize> = impls
        .iter()
        .filter(|(_, l)| l.contains("async"))
        .map(|(n, _)| *n)
        .collect();
    assert!(
        async_in_impl.is_empty(),
        "a LogSource impl declares an async item at lines {async_in_impl:?}"
    );
}

/// Oracle: the detector itself. A grep-assert that cannot go red is worth
/// nothing, so run the same functions over an injected sample. Negative: the
/// commented-out await must not be counted.
#[test]
fn must_detect_an_injected_await() {
    let injected = "impl LogSource for Fake {\n    fn poll_block(&mut self) {\n        self.rx.recv().await;\n    }\n}\n";
    let lines = code_lines(injected);
    assert_eq!(awaits(&lines), vec![3]);
    assert_eq!(awaits(&impl_blocks(&lines, "LogSource")), vec![3]);

    let commented = "fn f() {\n    // self.rx.recv().await;\n}\n";
    assert!(awaits(&code_lines(commented)).is_empty());
}
