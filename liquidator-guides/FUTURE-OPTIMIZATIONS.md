# Future Optimizations

Deferred speed optimizations surfaced during the build. Each is **not blocking** — the code that surfaced it passes its acceptance — but is recorded here so a future profiling pass can pick them up in priority order. Correctness items stay in `STATE.md` carry-forward; this file is for **speed only**.

Priority is by **win-per-rework-cost**: zero-rework wins first, then layout changes, then policy-gated ones.

---

## P1 — Zero-rework (do first)

### F1. 08A: sort the candidate batch by `PositionId` before folding
- **Win:** recovers the hardware prefetcher on the hot sweep. The 02A 1k-sweep bench walks positions in id order (the prefetcher's best case in a position-major layout, 24 µs). `ThresholdIndex` / `TimeCrossHeap` emit candidates in *value* order — scattered ids, prefetcher defeated. A sort of a few hundred `u32`s before the fold recovers the 24 µs line.
- **Cost:** one sort in 08A. Zero rework elsewhere. Zero `unsafe`.
- **Surfaced by:** 02A Opus review.
- **Revisit:** when 08A is built; fold the sort into the candidate batch before the health sweep.

### F2. 08A/12: `RayU128` for `Quote` / `BonusCurve` values
- **Win:** `size_of::<Quote>() == 1776 B` (28 cache lines) because `SeizeOption` is 192 B (112 B `BonusCurve`) at inline capacity 8. A 1k-deep `Candidate` channel is ~1.8 MB against the ~25 kB hot band D43 assumes stays resident. Every value in `BonusCurve` and `SeizeOption.bonus` is governance-derived and bounded well under `u128`; `RayU128` exists for exactly this (GUIDE-02 §3 already makes the argument for `MarketRow`). Roughly **halves** the `Quote` type.
- **Cost:** a WP 01 type change (`BonusCurve` / `SeizeOption` fields `u128 → RayU128`) + 04A populates them + 08A reads them. Blast radius: 01, 04A, 08A. No `unsafe`.
- **Surfaced by:** 01 Opus review.
- **Revisit:** when 08A is built and the `Candidate` channel depth is set; if the channel exceeds the hot band, do this first.

---

## P2 — Layout changes (re-open a committed type)

### F3. Interleaved balances: 15 → 12 cache lines on the hot sweep
- **Win:** if the two `u128` balances for a `(position, slot)` were adjacent (one array of 32-byte supply/debt pairs), slots land on **3** lines instead of 6 → data = 12 (the exact WORK-PACKAGES target). `set_column`'s "is the other column nonzero" read becomes free (same line) instead of a second line touch. **20% cache-line reduction** on the sweep.
- **Blocker:** `PositionRef` (WP 01) carries `supply: &[u128]` and `debt: &[u128]` as two separate slot-indexed slices. The 02A per-position + per-market-flat layout is *forced* by this contract.
- **Blast radius:** WP 01 (`PositionRef` shape), 02A (store arrays), 04A (`health()` iterates the slices), 02B (snapshot captures the columns).
- **Cost:** a 01-fix WP + 02A rework + re-review both + 04A/02B build on the new layout.
- **Decision:** **deferred** (user, 2026-09-19). The store passes with 2× headroom (24 µs vs 50 µs); the bigger win is F1 (zero rework). Cheapest now, before 02B/04A build on the current layout — but not worth the rework while the path passes.
- **Revisit:** if a target-hardware profiling pass shows the sweep is the bottleneck, or if the `Candidate` channel / hot-band pressure (F2) makes every cache line count. Do F1 and F2 first.

### F4. Stride padding below 4 reserves (15A / 15C)
- **Win:** `next_multiple_of(4)` doubles the column footprint for a 2-reserve market (Morpho Blue has thousands of markets) and gains nothing — a 32-byte row never straddles a cache line. The rule should become "next power of two below 4 cells, next multiple of 4 at or above."
- **Cost:** one line in 02A's stride calculation. No market in scope today has fewer than 4 reserves, so no effect now.
- **Surfaced by:** 02A Opus review.
- **Revisit:** when 15A/15C onboards a non-Aave family (Morpho Blue, Spark) with sub-4-reserve markets.

---

## P3 — Policy-gated (needs a guardrail decision)

### F5. `prefetcht0` on the state-store sweep
- **Win:** pulls the balance lines into L1 before the sweep touches them. Helps on any hardware (L3 is still slower than L1).
- **Blocker:** `unsafe_code = "forbid"` is workspace-wide. Rust 1.95 requires `#[target_feature(enable = "sse")]` for the `core::arch::x86_64::_mm_prefetch` intrinsic, which is in the `unsafe_code` lint category. `prefetcht0` is memory-safe (a non-faulting hint, never dereferences), but `target_feature` is an `unsafe` attribute by Rust convention.
- **Evidence against (now):** the 02A Opus reviewer measured the sweep at **23.8–31.4 µs** (5 runs, ~24 µs median) on the same machine the builder reported 33–56 µs on. The 50 µs budget is met with 2× headroom; the "L3-bound" diagnosis did not reproduce. Spending the project's first-ever `unsafe` exception on a path with 2× headroom is a bad trade.
- **Decision:** **declined** (user, 2026-09-19; D58). Keep `unsafe_code = "forbid"`.
- **Revisit:** only if a target-hardware measurement shows the sweep L3-bound. Ordered options before this: (a) F1 (08A visit-order sort, zero `unsafe`, larger win); (b) F3 (interleaved balances, zero `unsafe`, 20% line cut); (c) only then a one-function helper crate with `unsafe_code = "deny"` and a single audited `#[allow]` + `// SAFETY:` + miri test + `UNSAFE.md` entry (RUST-CONVENTIONS §7).

---

## How to use this file

- When a WP lands a speed carry-forward, add it here in the right priority tier.
- Before a profiling pass, read this file top-to-bottom; the P1 items are the cheapest wins.
- A deferred item that becomes blocking (a budget is missed on target hardware) moves up to P1 and is done in the WP that owns the hot path.
