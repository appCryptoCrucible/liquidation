//! Global allocator: mimalloc in production; `PanicOnAlloc` behind `alloc-assert`.
//!
//! Hot path must not allocate (RUST-CONVENTIONS §6). The panic allocator is the
//! CI assertion, not a production allocator.

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
        if hot_path_flag() {
            // Clear first so unwind/`catch_unwind` can allocate; the panic is
            // the assertion firing.
            HOT_PATH.with(|c| c.set(false));
            panic!("allocation on hot path: {layout:?}");
        }
        // SAFETY: caller guarantees `layout` as GlobalAlloc::alloc.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if hot_path_flag() {
            HOT_PATH.with(|c| c.set(false));
            panic!("allocation on hot path: {layout:?}");
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if hot_path_flag() {
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

    #[cfg(feature = "alloc-assert")]
    #[test]
    fn system_alloc_outside_hot_path() {
        let s = String::from("ok");
        assert_eq!(s.as_str(), "ok");
    }
}
