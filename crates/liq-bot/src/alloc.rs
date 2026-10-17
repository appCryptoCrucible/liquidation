//! Global allocator: mimalloc in production; `PanicOnAlloc` behind `alloc-assert`.
//!
//! Hot path must not allocate (RUST-CONVENTIONS §6). The panic allocator is the
//! CI assertion, not a production allocator. `survey_hot_path` is its audit
//! mode: instead of panicking at the first hot-path allocation it records
//! every one (where, how often, how many bytes), so a whole path's
//! allocations are listed in one run.

#[cfg(feature = "alloc-assert")]
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static HOT_PATH: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` with the hot-path allocation flag set.
///
/// With `--features alloc-assert`, any allocation inside `f` panics. Without
/// the feature this is a no-op around `f` (mimalloc remains the global
/// allocator).
#[inline]
pub fn with_hot_path<R>(f: impl FnOnce() -> R) -> R {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            HOT_PATH.with(|c| c.set(false));
        }
    }
    HOT_PATH.with(|c| c.set(true));
    let _g = Guard;
    f()
}

/// One hot-path allocation site found by [`survey_hot_path`]: the first
/// frame in this workspace's crates that allocated, and its caller.
#[cfg(feature = "alloc-assert")]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllocSite {
    pub count: u64,
    pub bytes: u64,
}

#[cfg(feature = "alloc-assert")]
thread_local! {
    static SURVEY: Cell<bool> = const { Cell::new(false) };
    /// Every how many allocations the current survey takes a backtrace
    /// (the others are counted under [`UNTRACED`]), and the countdown to
    /// the next.
    static EVERY: Cell<u32> = const { Cell::new(1) };
    static UNTIL: Cell<u32> = const { Cell::new(0) };
    static SITES: std::cell::RefCell<std::collections::HashMap<String, AllocSite>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// The key allocations between sampled ones are counted under.
#[cfg(feature = "alloc-assert")]
pub const UNTRACED: &str = "(not sampled)";

/// Run `f` as [`with_hot_path`] does, but record every allocation inside it
/// instead of panicking, and return them by site (`"fn (file:line) <- caller"`).
/// Audit only: every `every`-th allocation takes a backtrace (1: all of
/// them) — an unbiased sample across the whole call, so a site's count
/// times `every` estimates its share; the rest are counted under
/// [`UNTRACED`], and all counts together are the call's allocations.
#[cfg(feature = "alloc-assert")]
pub fn survey_hot_path<R>(
    every: u32,
    f: impl FnOnce() -> R,
) -> (R, std::collections::HashMap<String, AllocSite>) {
    SITES.with(|s| s.borrow_mut().clear());
    EVERY.with(|e| e.set(every.max(1)));
    UNTIL.with(|u| u.set(0));
    SURVEY.with(|s| s.set(true));
    let r = with_hot_path(f);
    SURVEY.with(|s| s.set(false));
    let sites = SITES.with(|s| std::mem::take(&mut *s.borrow_mut()));
    (r, sites)
}

/// The site of the allocation in progress: the first two frames in a
/// `liq_` crate outside this module.
#[cfg(feature = "alloc-assert")]
fn site_of(bt: &std::backtrace::Backtrace) -> String {
    let text = bt.to_string();
    let mut found: Vec<String> = Vec::with_capacity(2);
    let mut lines = text.lines().peekable();
    while let Some(l) = lines.next() {
        let name = l.trim_start().split_once(": ").map_or("", |(_, n)| n);
        // This module's frames (the allocator, its shims, the survey) are
        // the harness, not the site; its tests are callers like any other.
        let harness =
            name.starts_with("liq_bot::alloc::") && !name.starts_with("liq_bot::alloc::tests::");
        if !name.starts_with("liq_") || harness {
            continue;
        }
        let at = lines
            .peek()
            .map(|n| n.trim())
            .filter(|n| n.starts_with("at "))
            .map(|n| n.rsplit(['\\', '/']).next().unwrap_or(n).to_string())
            .unwrap_or_default();
        found.push(format!("{name} ({at})"));
        if found.len() == 2 {
            break;
        }
    }
    if found.is_empty() {
        return "(no liq_ frame)".to_string();
    }
    found.join(" <- ")
}

/// Record one hot-path allocation of `size` bytes when a survey is running;
/// `false` when none is (the caller panics).
#[cfg(feature = "alloc-assert")]
fn surveyed(size: usize) -> bool {
    if !SURVEY.with(Cell::get) {
        return false;
    }
    // Off the hot path while recording: the record allocates.
    HOT_PATH.with(|c| c.set(false));
    let every = EVERY.with(Cell::get);
    let sampled = UNTIL.with(|u| {
        let left = u.get();
        u.set(if left == 0 {
            every.saturating_sub(1)
        } else {
            left.saturating_sub(1)
        });
        left == 0
    });
    let key = if sampled {
        site_of(&std::backtrace::Backtrace::force_capture())
    } else {
        UNTRACED.to_string()
    };
    SITES.with(|s| {
        let mut m = s.borrow_mut();
        let e = m.entry(key).or_default();
        e.count = e.count.saturating_add(1);
        e.bytes = e
            .bytes
            .saturating_add(u64::try_from(size).unwrap_or(u64::MAX));
    });
    HOT_PATH.with(|c| c.set(true));
    true
}

/// True if this thread is currently inside [`with_hot_path`].
#[inline]
#[must_use]
pub fn hot_path_flag() -> bool {
    HOT_PATH.with(Cell::get)
}

#[cfg(not(feature = "alloc-assert"))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Test-only global allocator: panic on hot-path allocation, else `System`.
///
/// Recorded in `crates/liq-bot/UNSAFE.md`. The only `unsafe impl` in WP 16A.
#[cfg(feature = "alloc-assert")]
pub struct PanicOnAlloc;

#[cfg(feature = "alloc-assert")]
// SAFETY: see UNSAFE.md — pointers/layouts are the GlobalAlloc contract;
// hot-path alloc never returns a pointer.
#[allow(unsafe_code, clippy::panic)]
unsafe impl GlobalAlloc for PanicOnAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if hot_path_flag() && !surveyed(layout.size()) {
            // Clear first so unwind/`catch_unwind` can allocate; the panic is
            // the assertion firing.
            HOT_PATH.with(|c| c.set(false));
            panic!("allocation on hot path: {layout:?}");
        }
        // SAFETY: caller guarantees `layout` as GlobalAlloc::alloc.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if hot_path_flag() && !surveyed(layout.size()) {
            HOT_PATH.with(|c| c.set(false));
            panic!("allocation on hot path: {layout:?}");
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if hot_path_flag() && !surveyed(new_size) {
            HOT_PATH.with(|c| c.set(false));
            panic!("allocation on hot path: {layout:?} -> {new_size}");
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[cfg(feature = "alloc-assert")]
#[global_allocator]
static ALLOC: PanicOnAlloc = PanicOnAlloc;

#[cfg(test)]
mod tests {
    use super::with_hot_path;

    #[test]
    fn with_hot_path_sets_and_clears_flag() {
        assert!(!super::hot_path_flag());
        with_hot_path(|| {
            assert!(super::hot_path_flag());
        });
        assert!(!super::hot_path_flag());
    }

    #[cfg(not(feature = "alloc-assert"))]
    #[test]
    fn with_hot_path_clears_flag_on_panic() {
        let _ = std::panic::catch_unwind(|| {
            with_hot_path(|| panic!("unwind"));
        });
        assert!(!super::hot_path_flag());
    }

    #[cfg(feature = "alloc-assert")]
    #[test]
    #[allow(clippy::vec_init_then_push)]
    fn panic_on_hot_path_alloc() {
        let r = std::panic::catch_unwind(|| {
            with_hot_path(|| {
                let mut v = Vec::<u8>::new();
                v.push(1);
            });
        });
        assert!(
            r.is_err(),
            "hot-path Vec growth must panic under alloc-assert"
        );
    }

    /// The survey lists every hot-path allocation by site instead of
    /// panicking: a `Vec` grown three times is one site counted three
    /// times, at this test's own line; nothing outside the survey is listed.
    #[cfg(feature = "alloc-assert")]
    #[test]
    fn survey_lists_allocations_by_site() {
        let before = Vec::<u8>::with_capacity(1);
        let ((), sites) = super::survey_hot_path(1, || {
            let mut v: Vec<u64> = Vec::new();
            for k in 0..9u64 {
                v.push(k);
            }
            assert_eq!(v.len(), 9);
        });
        drop(before);
        assert_eq!(sites.len(), 1, "{sites:?}");
        let (site, n) = sites.iter().next().unwrap();
        assert!(site.contains("alloc.rs"), "{site}");
        assert_eq!(n.count, 3, "capacity 4, then 8, then 16");
        assert!(!super::hot_path_flag());
    }

    #[cfg(feature = "alloc-assert")]
    #[test]
    fn system_alloc_outside_hot_path() {
        let s = String::from("ok");
        assert_eq!(s.as_str(), "ok");
    }
}
