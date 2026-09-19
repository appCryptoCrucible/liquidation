# Rust Conventions

The canonical reference for how this system is written. Every guide assumes it.
Where a guide's layer has a specific concurrency shape, it says so in its own
**Rust: concurrency & safety** section and defers here for the rest.

Read this before GUIDE 00, and re-read §3 before writing any code that crosses a
thread boundary.

---

## 1. The architectural fact that drives everything

**The hot path is single-threaded, synchronous, and owns its data exclusively.**

```
ExEx notification ──► decode ──► state apply ──► engine ──► candidate
                    (all on ONE thread, &mut StateStore, no locks at all)
```

The state store has **exactly one writer**, and every hot-path stage runs on that
writer's thread. It therefore needs no synchronization — not a mutex, not an
atomic, not a lock-free structure. `&mut StateStore` is the whole concurrency
story for the part of the system that must be fastest.

This is not an optimization you apply later. It is a constraint you design to,
and it is why `Arc<Mutex<StateStore>>` does not appear anywhere in this codebase.
The fastest lock is the one you never needed because nothing is shared.

Everything else — telemetry, drift sampling, route solving, submission, PnL — is
*off* that thread and communicates by message passing or published snapshots.

**When you are tempted to share mutable state on the hot path, the answer is
almost always to restructure so you don't, not to pick a cleverer primitive.**

---

## 2. Panics and `.unwrap()`

A panic inside the ExEx future can take down the Ethereum node (GUIDE 03). Treat
every panic as a potential outage.

### 2.1 Lint configuration

In every crate that runs on or near the hot path — `liq-types`, `liq-state`,
`liq-protocol`, `liq-adapters`, `liq-engine`, `liq-flash`, `liq-node`,
`liq-router`:

```rust
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,  // force checked_/saturating_/wrapping_ (D53; `integer_arithmetic` is the deprecated old name)
    clippy::todo,
    clippy::unimplemented,
    clippy::float_arithmetic,     // no floats in money math, ever
)]
#![forbid(unsafe_code)]           // see §7 for the narrow exceptions
```

Tests get `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]`.
Test code may panic freely; that is what a failing test is.

### 2.2 When `expect` is acceptable

Only when the invariant is **locally provable** and the message states it:

```rust
// OK: capacity was reserved at startup; this is a structural invariant.
let row = self.markets.get(idx)
    .expect("market index validated at config load; see MarketId interning");
```

Not acceptable: `expect("should exist")`, `expect("this can't fail")`, or any
message that restates the call rather than the invariant. If you cannot write
the invariant in one clause, you do not have one — return an error.

### 2.3 Indexing

`clippy::indexing_slicing` is denied because `v[i]` panics. On the hot path:

```rust
// Preferred: bounds check once, then iterate.
for (slot, &bal) in pos.supply.iter().enumerate() { ... }

// When you must index: get() and handle None.
let Some(&bal) = self.supply[slot].get(pos) else { return Err(...) };
```

Do **not** reach for `get_unchecked`. The bounds check is a predictable branch
that the CPU will speculate correctly; it is not your bottleneck, and §7 applies.

### 2.4 Arithmetic

Money math never wraps and never silently overflows.

```toml
[profile.release]
overflow-checks = true      # yes, in release. Cheap; catches wrong health factors.
panic = "unwind"            # REQUIRED — catch_unwind cannot contain an abort
lto = "fat"
codegen-units = 1
```

`panic = "abort"` is tempting for performance and is **forbidden here**: it makes
the ExEx panic containment in §2.5 impossible, turning any adapter bug into a
node outage.

Use `checked_*` and propagate, or `saturating_*` where saturation is semantically
correct. Never `wrapping_*` in financial math. Floats are banned outright in
money paths — `clippy::float_arithmetic` enforces it. Floats are fine for
telemetry, latency histograms and the bid model's `α` estimation, which are not
money.

### 2.5 Panic containment at the ExEx boundary

One place in the system catches panics, and it is GUIDE 03's dispatch:

```rust
let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
    adapter.apply_log(&mut store, &decoded)
}));

match result {
    Ok(Ok(dirty))  => acc.merge(dirty),
    Ok(Err(e))     => { risk.halt(HaltScope::Protocol(adapter.id()), e.into()); }
    Err(_panic)    => {
        // An adapter panicked. Halt that protocol; keep the node alive.
        risk.halt(HaltScope::Protocol(adapter.id()), HaltReason::AdapterPanic);
        metrics::counter!("adapter_panic", "protocol" => adapter.name()).increment(1);
    }
}
```

`AssertUnwindSafe` is a real assertion, not a formality: after a panic mid-fold
the store may hold a partially-applied log. That is why the handler halts the
protocol rather than continuing — and why the undo ring (GUIDE 02) must be able
to unwind the partial block.

---

## 3. Choosing a concurrency primitive

The decision table. Find your row before writing any code.

| Situation | Use | Crate | Why not something else |
|---|---|---|---|
| Hot path stage → next hot path stage | **Nothing — a function call** | — | Same thread. A channel here is pure overhead. |
| One producer → one consumer, every message matters, bounded | **SPSC ring buffer** | `rtrb` | Wait-free, no contention, no allocation. MPSC pays for producers you don't have. |
| One producer → one consumer, **only the latest matters** | **Triple buffer** | `triple_buffer` | Never blocks, never allocates, reader always gets a complete recent value. A channel would queue stale prices. |
| Many producers → one consumer (CEX feeds, telemetry) | **Bounded MPSC** | `crossbeam-channel` | Handles multiple producers correctly; bounded gives backpressure. |
| One writer → many readers, read-mostly, whole-structure | **RCU pointer swap** | `arc-swap` | Readers never block and never contend. An `RwLock` makes readers contend on the lock word. |
| Read-mostly, small, POD | **Seqlock** or an atomic | `crossbeam` / `std::sync::atomic` | Cheapest possible read. |
| Counters, gauges, rates | **Atomics, `Relaxed`** | `std::sync::atomic` | No ordering needed for statistics. |
| Rare mutation, short critical section, contention is fine | **`Mutex`** | `parking_lot` | Genuinely correct sometimes. See §4. |
| Async I/O edge → sync core | **`crossbeam` bounded, or `flume`** | — | Never block a Tokio worker on a sync channel; never block the hot thread on an async one. |
| Fan out CPU work over cores | **Scoped threads** or `rayon` | `std::thread::scope` / `rayon` | Scoped threads borrow without `Arc`. Prefer them for a fixed, small fan-out. |

### 3.1 SPSC in practice

Used at four boundaries: ExEx → hot path and back (§5.1), the mempool ingest
stream (GUIDE 03), the MEV-Share hint stream (GUIDE 06), and simulation dispatch
and return per worker (GUIDE 11). Every one of them has exactly one producer and
one consumer, which is why MPSC would be paying for contention that does not
exist.

```rust
let (mut producer, mut consumer) = rtrb::RingBuffer::<PendingTx>::new(4096);

// Producer (network thread) — never blocks, drops on full.
if producer.push(tx).is_err() {
    metrics::counter!("mempool_dropped").increment(1);   // correct behavior
}

// Consumer (hot thread) — drain what's there, never wait.
while let Ok(tx) = consumer.pop() { overlay.apply(tx); }
```

Note the drop-on-full. For the mempool, a stale pending transaction is worthless
and blocking ingest is catastrophic — **dropping is the correct policy and must
be counted, not silently swallowed.**

### 3.2 Triple buffer for the price vector

The `PriceVector` is written by the oracle thread and read by the hot path
millions of times. Readers want the newest complete vector, never a queue of old
ones, and must never block.

```rust
let (mut input, mut output) = triple_buffer::TripleBuffer::new(&PriceVector::zeroed()).split();

// Oracle thread: publish a complete vector.
input.write(new_vector);

// Hot thread: always gets the most recent complete write, wait-free.
let px: &PriceVector = output.read();
```

A `Mutex<PriceVector>` would put the hot path behind the oracle thread's writes.
An MPSC channel would build a backlog you'd have to drain to find the latest.
Triple buffer is exactly the right shape and it is worth knowing it exists.

### 3.3 `arc-swap` for read-mostly structures

For the flash liquidity index (GUIDE 07), the warm route cache (GUIDE 12) and
live config: one writer rebuilds, many readers read, readers must never block.

**Do not reach for a `static`.** `ArcSwap::from_pointee` is not `const fn`, so a
plain `static` will not compile — and the two obvious repairs, `LazyLock` and
`OnceLock`, both put an **acquire load and a branch on every single access**. The
branch predicts perfectly after the first hit, so it is small, but it is a cost
paid per access forever on a path budgeted in nanoseconds, and it is entirely
avoidable.

Own it instead, construct it once at startup, and hand out a `&'static`:

```rust
pub struct Shared {
    pub routes: ArcSwap<RouteCache>,
    pub flash:  ArcSwap<FlashIndex>,
}

// Startup, once, before any thread is spawned:
let shared: &'static Shared = Box::leak(Box::new(Shared {
    routes: ArcSwap::from_pointee(RouteCache::empty()),
    flash:  ArcSwap::from_pointee(FlashIndex::empty()),
}));

// Background thread: build a new one, publish atomically.
shared.routes.store(Arc::new(rebuilt));

// Hot path: a plain field access, then a cheap load. No init check, ever.
let routes = shared.routes.load();
```

`Box::leak` at startup is the whole trick: the allocation lives for the process,
so `&'static` is honest, and every later access is a field offset the compiler can
keep in a register. Nothing is lazy because nothing needs to be — the process has
exactly one startup and it happens before the first block arrives.

It is also better shape for other reasons: no global mutable state, so a test can
build its own `Shared` and two tests cannot interfere; and the hot path's
dependencies are visible in its signature instead of reaching out to a static.

**The general rule, which matters more than this one case:** a lazily-initialised
global is a per-access check bought to avoid a startup ordering problem. This
process has a startup phase with nothing to race against, so pay the ordering cost
once, at startup, and hand the hot path something already built. If you find
yourself writing `LazyLock`, `OnceLock`, `once_cell` or a `lazy_static` anywhere
reachable from a hot path, the answer is almost always an explicitly constructed
`&'static` instead.

Readers get a consistent snapshot; there is no torn read and no lock. The cost is
that the writer allocates a new structure per publish — fine off the hot path,
which is where all these writers live.

### 3.4 Bounded channels and backpressure

**Every channel is bounded**, with one exception: the canonical block stream,
where dropping is corruption. Bound everything else and decide the overflow
policy explicitly:

| Channel | Bound | On full |
|---|---|---|
| Canonical blocks | unbounded | never drop (ExEx `FinishedHeight` provides real backpressure) |
| Mempool → overlay | 4096 | **drop oldest**, count it |
| Engine → router (candidates) | 1024, priority-ordered | **drop lowest value**, count it |
| Telemetry | 65536 | drop, count it — telemetry must never stall trading |
| Submission results | 256 | block briefly; this path must not lose data |

An unbounded channel is an out-of-memory bug waiting for a volatile day. Dropping
the *lowest-value* candidate under load is correct behavior; dropping the
*newest* is not, which is why that channel is a priority structure rather than a
FIFO.

---

## 4. When a lock is the right answer

Lock-free is not a goal in itself. It is a tool for contended hot paths, and
almost nothing here is both contended and hot.

**Use a `parking_lot::Mutex` when all of these hold:** the critical section is
short and allocation-free, contention is low, and the path is not the
sub-millisecond hot path.

The canonical example is the **nonce allocator** (GUIDE 13):

```rust
// Correct use of a lock. Contention is one-per-submission, the critical
// section is a couple of integer ops, and this is on the async path.
let mut guard = key.state.lock();
let nonce = guard.next;
guard.next += 1;
guard.in_flight.insert(nonce, InFlight::new());
drop(guard);            // explicit: released BEFORE any await
```

**Rules that are not negotiable:**

1. **Never hold a lock across `.await`.** Use `parking_lot` (whose guards are
   `!Send`, so the compiler stops you) rather than `tokio::sync::Mutex`, unless
   you genuinely need to hold across an await — and then question the design.
2. **Never allocate inside a critical section.** Build outside, insert inside.
3. **One lock at a time.** If you need two, you need a redesign; nested locks
   here will deadlock on a volatile day and not before.
4. `parking_lot` over `std::sync` — smaller, faster uncontended, no poisoning.
   Poisoning is a misfeature for this workload: a panic already halts the
   protocol (§2.5); it should not also permanently break the lock.

**Do not hand-roll a lock-free data structure.** If you believe you need one,
first prove the single-writer restructure is impossible, then use a published
crate, then test it under `loom` (§8). Hand-rolled lock-free code that is subtly
wrong fails once a month under load and is nearly impossible to debug from a
production trace.

---

## 5. Async boundaries — the complete mapping

The rule in one line: **the hot path is synchronous and runs on its own pinned
OS thread. Async exists only at I/O edges, on non-isolated cores.**

Because this is the question that produces the most wrong code, here is every
component classified, with no ambiguity.

### 5.1 The ExEx is a thin forwarder, not the hot path

This is the subtlety that decides the rest. An ExEx future is polled by **Reth's
own Tokio runtime**. If you do the work inside the notification handler, your
2–3 ms of state folding and health computation blocks a Tokio worker that Reth
also uses, and you cannot pin or isolate it because Reth owns that runtime.

So the ExEx future does almost nothing:

```rust
async fn liquidator_exex<N: FullNodeComponents>(
    mut ctx: ExExContext<N>,
    mut to_hot: rtrb::Producer<Notification>,      // async  -> sync
    mut from_hot: rtrb::Consumer<FinishedUpTo>,    // sync   -> async
) -> eyre::Result<()> {
    while let Some(n) = ctx.notifications.try_next().await? {
        // Owned/Arc'd payload — nothing borrowed crosses the boundary.
        to_hot.push(Notification::from(n)).map_err(|_| eyre!("hot path stalled"))?;

        // FinishedHeight MUST follow state consistency, so wait for the hot
        // thread to confirm. This await yields the Tokio worker; it does not
        // spin.
        if let Ok(done) = from_hot.pop() {
            ctx.events.send(ExExEvent::FinishedHeight(done.num_hash))?;
        }
    }
    Ok(())
}
```

The hot path is then a plain `std::thread` — nameable, pinnable, isolatable
(GUIDE 16), and outside any runtime:

```rust
std::thread::Builder::new().name("liq-hot".into()).spawn(move || {
    pin_to_core(HOT_CORE);
    loop {
        while let Ok(n) = from_exex.pop() {
            apply_chain(&mut store, &router, &n);   // all sync, no await
            engine.on_block(&mut store);
            to_exex.push(FinishedUpTo { num_hash: n.tip }).ok();
        }
        std::hint::spin_loop();
    }
})?;
```

The extra hop costs roughly a microsecond. It buys determinism, real core
isolation, and the guarantee that a slow block never starves Reth's own tasks.
On a sub-5 ms budget that trade is not close.

**A busy-poll loop is correct here** given a dedicated core. If you would rather
not burn it, park with a futex-based signal, but measure the wakeup latency
before choosing.

### 5.2 Every component, classified

**Synchronous — hot-path thread, no runtime, no `.await` anywhere:**

| Component | Guide |
|---|---|
| Log decode and adapter dispatch | 03 |
| State store apply / undo / reorg unwind | 02 |
| Flash index update and availability lookup | 07 |
| `health()`, `liquidation_price()`, `time_to_cross()` | 01, 04 |
| Threshold index lookup, band reassignment, time-cross heap | 08 |
| Candidate emission | 08 |
| `quote()`, sizing, repay × seize × source search | 12 |
| Route cache read (an `arc-swap` load) | 12 |
| Profit model, bid computation, plan encoding | 12 |
| revm simulation | 11 |
| Transaction signing | 13 |

**Asynchronous — Tokio, non-isolated cores, never touching `StateStore`:**

| Component | Guide |
|---|---|
| MEV-Share SSE stream reader | 06 |
| CEX websocket subscribers | 06 |
| Pull-oracle clients (Hermes, Redstone) | 06 |
| `mev_sendBundle` and builder HTTP submission | 13 |
| Inclusion watching where it needs RPC | 13 |
| Drift-detector probe calls | 02, 04 |
| Backfill and archive reads | 03 |
| Governance contract polling (`ParamChange` triggers) | 08 |
| Prometheus scrape endpoint, config hot-reload | 09 |

**Dedicated sync threads — CPU-bound, not hot path, not async:**

| Component | Guide |
|---|---|
| Route cache builder (publishes via `arc-swap`) | 12 |
| Simulation workers for parallel variants | 11 |
| WAL writer and snapshotter | 02 |
| PnL ledger writer | 14 |

### 5.3 Every boundary crossing

| From | To | Primitive | Notes |
|---|---|---|---|
| ExEx (async) | hot (sync) | `rtrb` SPSC | owned payload only; §5.1 |
| hot (sync) | ExEx (async) | `rtrb` SPSC | `FinishedHeight` confirmation |
| mempool reader (async) | hot (sync) | `rtrb` SPSC | drop-oldest, counted |
| CEX readers (async) | fusion (sync thread) | `crossbeam` bounded MPSC | many producers |
| MEV-Share SSE (async) | hot (sync) | `rtrb` SPSC | one producer |
| fusion (sync) | hot (sync) | `triple_buffer` | latest complete vector |
| route builder (sync) | hot (sync) | `arc-swap` | read-mostly snapshot |
| hot (sync) | sim workers (sync) | `rtrb` per worker | known peer each way |
| **hot (sync)** | **submitter (async)** | **`tokio::sync::mpsc::Sender::try_send`** | see below |
| hot (sync) | telemetry (async) | bounded, drop on full | never stalls trading |
| submitter | inclusion watcher | `tokio::sync::mpsc` | async to async |

**The sync → async crossing is the one people get wrong.** From a non-async
thread, use `try_send`, which is callable outside a runtime and returns
immediately:

```rust
// Hot thread. Correct.
if tx.try_send(bundle).is_err() {
    metrics::counter!("submit_queue_full").increment(1);
}
```

Never `blocking_send` from the hot thread — it parks the thread waiting on the
runtime. Never `Handle::block_on` — same problem with a deeper stack. If the
queue is full you drop and count; the submission path being backed up is a
condition to alert on (GUIDE 14), not one to wait through.

### 5.4 Hard rules

1. **No `.await` in any function reachable from the hot-path thread.** Enforce it
   by making those crates not depend on `tokio` at all — `liq-state`,
   `liq-protocol`, `liq-adapters`, `liq-engine`, `liq-flash`, `liq-router` and
   `liq-sim` should have no async runtime in their dependency tree. The compiler
   then makes the mistake impossible rather than the reviewer catching it.
2. **Async tasks never hold a reference into `StateStore`.** They read an
   `arc-swap` snapshot (GUIDE 02) or receive messages. The store is `!Sync` by
   construction and the borrow checker enforces the rest.
3. **Never hold a lock across `.await`** — `clippy::await_holding_lock` is
   deny-level and `parking_lot` guards are `!Send`, so this is caught twice (§4).
4. **Tokio's runtime threads are pinned to non-isolated cores** (GUIDE 16). An
   async task must never be schedulable onto a hot-path core.
5. **CPU-bound work never runs on a Tokio worker.** Dedicated thread, or
   `spawn_blocking` if it is rare and you do not care about its latency.

## 6. Allocation discipline

**Zero allocation on the hot path**, asserted in tests, not assumed.

```rust
// In liq-bot's test harness.
struct PanicOnAlloc;
unsafe impl GlobalAlloc for PanicOnAlloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if HOT_PATH.with(|f| f.get()) { panic!("allocation on hot path: {l:?}") }
        System.alloc(l)
    }
    /* dealloc … */
}
```

Set the thread-local flag around the hot path in a replay test and run the full
archive through it. This catches the accidental `Vec` or `format!` that a code
review will miss.

Techniques:

- **Pre-allocate everything at startup**, sized for the full protocol universe
  with headroom (GUIDE 16). `Vec::with_capacity`, never let it grow.
- `SmallVec` / `ArrayVec` for bounded collections — `DirtySet::Positions`,
  `Quote::repay_options`, the source list per asset.
- **Arena for per-block scratch**: `bumpalo`, reset each block. Log decoding
  allocates into it and the whole thing is freed with a pointer bump.
- No `format!`, `to_string()`, or `String` in error variants used on the hot path
  — use `&'static str` or a fieldless enum. Structured logging carries the
  context instead.
- `Box<dyn Protocol>` in a startup-built `Vec` is fine: one vtable indirection
  per call, no allocation. `Box`ing *per call* is not.
- Global allocator: `mimalloc` or `jemalloc`. It will not help the hot path
  (which does not allocate) but it measurably helps everything else.

---

## 7. `unsafe`

`#![forbid(unsafe_code)]` in every crate. Exceptions are granted per-crate, in
writing, and currently there should be **zero**.

If a case arises, it requires all of: a `// SAFETY:` comment stating the
invariant and why it holds, a `miri` test covering it, review by someone who did
not write it, and an entry in a checked-in `UNSAFE.md` (a repo artifact you
create on first use, not a spec file).

Specifically **not** reasons to use unsafe here:

- `get_unchecked` to skip a bounds check — predictable branch, not your bottleneck
- Uninitialized buffers — use `Vec::with_capacity` + `resize`, measure before caring
- Transmuting for the columnar store — `bytemuck` with derived `Pod`/`Zeroable`
  gives you the same layout guarantees safely

The performance in this system comes from data layout and not sharing state.
It does not come from `unsafe`, and reaching for it is usually a sign that a
layout problem is being papered over.

---

## 8. Testing the concurrent parts

| Tool | For | When |
|---|---|---|
| `proptest` | fixed-point math, undo/redo round-trips | every commit |
| `loom` | any structure crossing threads that you did not get from a crate | every commit, small models |
| `miri` | any `unsafe` (so: nothing, currently) | nightly CI |
| `criterion` | per-stage microbenchmarks | every commit, regression-gated |
| counting allocator | hot-path allocation assertions | every commit, on the replay harness |
| `cargo-deny` | licenses, advisories, duplicate versions | every commit |
| Fault injection | adapter panic → protocol halt, not node death | every commit |

`loom` deserves emphasis: it exhaustively explores thread interleavings for a
small model. If you write anything with atomics beyond a `Relaxed` counter, model
it in `loom` or replace it with a crate that already did.

---

## 9. Memory ordering, briefly

If you find yourself thinking hard about this on the hot path, re-read §1 —
the hot path should not be sharing anything.

- `Relaxed` — counters and statistics. No ordering implied, and none needed.
- `Release` on the write / `Acquire` on the read — publishing data another thread
  will then read. The standard publish-consume pair.
- `AcqRel` — read-modify-write that both publishes and consumes.
- `SeqCst` — only when you need a single total order across multiple locations.
  Genuinely rare. Reaching for it "to be safe" is a sign the design is unclear,
  and it is not actually safer, just slower.

Prefer `arc-swap`, `triple_buffer`, `rtrb` and `crossbeam` — all of which have
already solved this correctly — over your own atomics.

---

## 10. Error types

```rust
// Library crates: typed, allocation-free, no String on hot paths.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("unknown market {0:?}")]
    UnknownMarket(MarketId),
    #[error("reorg depth {depth} exceeds undo ring capacity {cap}")]
    ReorgTooDeep { depth: u32, cap: u32 },
}
```

- `thiserror` in libraries, `eyre` only in `liq-bot` at the top level.
- `Option` means "not applicable"; `Result` means "attempted and failed".
  `liquidation_price()` returns `Option` — an asset that doesn't affect the
  position is not an error.
- Errors that halt a protocol carry enough context to fill a `HaltReason`
  (GUIDE 14) without a string allocation.
- `#[must_use]` on anything returning a `Result` you could plausibly ignore.

---

## 11. Workspace lint configuration

Set once in the workspace so no crate can silently opt out:

```toml
[workspace.lints.rust]
unsafe_code = "forbid"
missing_debug_implementations = "warn"
unreachable_pub = "warn"

[workspace.lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
indexing_slicing = "deny"
arithmetic_side_effects = "deny"     # D53 — not `integer_arithmetic` (renamed, deprecated)
float_arithmetic = "deny"
await_holding_lock = "deny"          # catches the §4 rule mechanically
large_futures = "warn"
mutex_atomic = "warn"
rc_buffer = "warn"
```

Then `[lints] workspace = true` in every member crate. CI runs
`cargo clippy --workspace --all-targets -- -D warnings`.

`await_holding_lock` is the one that will save you a real outage. Leave it on.

---

## 12. Quick reference

```
Hot path stage to stage?            → function call, one thread, &mut
Latest value, one reader?           → triple_buffer
Every message, 1→1?                 → rtrb (SPSC)
Many senders?                       → crossbeam bounded MPSC
One writer, many readers, whole?    → arc-swap
Just a counter?                     → AtomicU64, Relaxed
Short, rare, off hot path?          → parking_lot::Mutex
Tempted to hand-roll lock-free?     → restructure to single-writer instead
Tempted by LazyLock / OnceLock?     → build it at startup, hand out &'static (§3.3)
Tempted by unsafe?                  → fix the data layout instead
Tempted by .unwrap()?               → is the invariant provable in one clause?
                                      No → return an error.
```
