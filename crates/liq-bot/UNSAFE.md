# `unsafe` in `liq-bot`

Workspace default is `unsafe_code = "forbid"`. This crate overrides that to
`deny` so a single, reviewed exception can exist.

## Sanctioned site (the only one in this WP)

| Item | Value |
|---|---|
| Symbol | `alloc::PanicOnAlloc` |
| Kind | `unsafe impl GlobalAlloc` |
| Feature | `alloc-assert` only (test/CI). Production uses mimalloc, no `unsafe` in this crate. |
| Callers | Replay / hot-path allocation assertion (RUST-CONVENTIONS §6, GUIDE 16 §3b). 04A `AllocMeter` / 16C e2e zero-alloc. |
| Not used for | Pinning, topology, mimalloc. |

### Rationale

A counting/panic global allocator cannot be expressed in safe Rust. D57 moved
the `health()` allocation-free assertion onto 16A's `PanicOnAlloc` for that
reason. The impl forwards to `std::alloc::System` unless the thread-local
hot-path flag is set; then it panics with the `Layout` (fail-closed, no
fabricated “zero alloc” result).

### Invariants (`// SAFETY:` in source)

- `alloc` / `alloc_zeroed` / `realloc` / `dealloc` are only reached through the
  `GlobalAlloc` contract (aligned `Layout`, pointers from prior `alloc`).
- When the hot-path flag is clear, behavior is `System`’s.
- When the flag is set, `alloc`/`alloc_zeroed`/`realloc` clear the flag then
  panic (never return a pointer). Clearing first lets `catch_unwind` allocate
  during unwind; the panic is the assertion. `dealloc` always forwards to
  `System`.

### Scope

No other `unsafe` in 16A. Thread pinning uses `core_affinity` (library-owned
platform `unsafe`, inherited — same class as D59/`crc32fast`).

### miri

`PanicOnAlloc` is a process-global allocator. miri cannot swap the interpreter
allocator for this type in-crate without `--features alloc-assert` on a binary
that never installs a second `#[global_allocator]`. Coverage is the unit test
`panic_on_hot_path_alloc` (`catch_unwind` around `Vec` growth with the flag set)
plus `system_alloc_outside_hot_path`. Run miri against that module when the
toolchain component is present:

`cargo +nightly miri test -p liq-bot --features alloc-assert --lib panic_on`

Until then the test is the executable contract.
