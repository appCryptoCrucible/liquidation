# GUIDE 02 — State Store

| | |
|---|---|
| **Crate** | `liq-state` |
| **Prerequisites** | GUIDE 00, 01 — `liq-state` depends on `liq-protocol` (implements `StateWriter`, lays out its `MarketRow`/`PositionExtraRepr`); never the reverse (D46) |
| **Work packages** | **02A** interner + columnar store + undo ring + views; **02B** WAL + snapshot + snapshot publish + drift detector |
| **Est. effort** | 1–1.5 weeks |
| **Blocks** | 03, 04, 05, 08, 11, 14 |

## Objective

The in-memory mirror of every tracked position, laid out so that recomputing a
few hundred health factors costs microseconds, and so that a reorg unwinds
instead of resyncing.

## Build vs. buy

100% build. No general-purpose store gives you microsecond columnar folds plus
exact reorg unwind. Buy only `memmap2` for snapshots.

---

## Step 1 — Interning

```rust
pub struct Interner {
    map: HashMap<PositionKey, PositionId>,
    keys: Vec<PositionKey>,          // index == PositionId
}
```

IDs are dense, assigned in first-sight order, and **never reused or removed**. A
position that repays to zero becomes `Band::Dead` but keeps its id — it will
borrow again, and a stable id means every downstream structure (threshold index,
heaps, caches) can hold `u32` forever without invalidation.

Do the same for assets: `Interner<Address, AssetId>` populated from config at
startup so `AssetId` is global across protocols (GUIDE 00, Step 3).

## Step 2 — Columnar layout

```rust
pub struct StateStore {
    interner: Interner,

    // ---- per-position columns, all indexed by PositionId ----
    config:    Vec<AssetMask>,       // which slots are active
    supply:    Vec<Vec<u128>>,       // [slot][position]  ← column-major
    debt:      Vec<Vec<u128>>,       // [slot][position]
    extra:     Vec<PositionExtra>,   // fixed-size, no indirection
    band:      Vec<Band>,          // incl. Unfundable (GUIDE 07)
    hf_cache:  Vec<(Ray, BlockNum)>,

    // ---- market table: small, hot, stays in L1/L2 ----
    markets:   Vec<MarketRow>,

    undo: UndoRing,
    wal:  WalWriter,
}
```

**Column-major, not row-major.** `supply[slot]` is a contiguous `Vec<u128>` over
all positions. Sweeping one market's balances is a linear scan; a
`MarketAccrual` dirty event touches one column, not N scattered rows.

`AssetMask` is a `u128` bitmask (or `[u64; 2]` if you exceed 128 slots). The
first thing `health()` does is iterate set bits, which skips 95+ of 100 markets
for a typical position. This single check is worth more than any other
optimization in the hot path.

## Step 3 — `MarketRow` and the Aave V4 two-table problem

Aave V3 has one table: reserves. Aave V4 has two levels — the Hub holds asset
accounting (`assetId`, indices, liquidity, rates), the Spoke holds risk
parameters (`collateralRisk`, flags, caps, `dynamicConfigKey`, liquidation
config). A position's health needs both.

Model it as one flat `MarketRow` per `(protocol, market, slot)` with the hub
fields denormalized into it, plus a `hub_ref` so a hub-level accrual updates
every row that points at it:

```rust
#[repr(C)]
pub struct MarketRow {
    // hub-level (denormalized; updated via hub_ref fan-out)
    // Aave stores indices and rates as uint128 RAY-scaled. Mirror the width:
    // a U256 `Ray` here doubles the row for bits the chain never sets.
    pub supply_index: RayU128,  // V4 addExRate; V3 liquidityIndex           16
    pub debt_index:   RayU128,  // V4 drawnIndex; V3 variableBorrowIndex     16
    pub supply_rate:  RayU128,  //                                           16
    pub debt_rate:    RayU128,  //                                           16
    pub dust_floor:   u128,     //                                           16
    pub last_update:  u32,      //                                            4
    pub hub_ref:      u16,      // u16::MAX for protocols without a hub       2
    pub liq_threshold: u16,     // bps (V4 collateralRisk, V3 liqThreshold)  2
    pub ltv:           u16,     // bps                                        2
    pub price_feed:    FeedId,  // u16                                        2
    pub asset:         AssetId, // GLOBAL id, u16                             2
    pub decimals:      u8,      //                                            1
    pub flags:         MarketFlags,   // frozen | paused | siloed | isolated  1

    // liquidation config (V4: per spoke; V3: per reserve). Governance
    // parameters with ≤ 4 decimals of real precision; bps-scaled u16/u32 is
    // exact for every value governance can set. Confirm the on-chain storage
    // width in GUIDE 04 Step 1 and use THAT — never wider.
    pub target_hf:        u32,  // 1e4-scaled                                 4
    pub max_liq_bonus:    u16,  // bps                                        2
    pub hf_for_max_bonus: u16,  // 1e4-scaled                                 2
    pub liq_bonus_factor: u16,  // 1e4-scaled                                 2
    // padding to 128                                                         6
}
const _: () = assert!(core::mem::size_of::<MarketRow>() <= 128);
```

**Arithmetic on the layout, because the previous draft got it wrong.** Eight
`U256` fields are 256 bytes on their own — the earlier "keep it under 128 bytes"
with `Ray` everywhere was unsatisfiable, and an agent hitting that would either
drop the assert or drop fields. Sized as above the row is 124 bytes: the four
u128 accounting fields (64) + dust (16) + 12 small fields (24) + liquidation
config (10) + padding. Two cache lines per row, every row starting on a line
boundary (`#[repr(C, align(64))]`). Order fields by access frequency in
`health()`, not by logical grouping — indices, thresholds, `asset`, `flags`
first, liquidation config last, so the common path reads one line of the two.

Widening a field is allowed only with evidence the chain stores it wider; the
`const` assert is what stops the row silently growing to three lines.

## Step 4 — `PositionExtra` and V4's risk premium

Aave V4 charges a per-user risk premium on top of the base borrow rate, settled
as `premiumDelta` on repay. This means **V4 debt accrual is not purely a
per-market multiplier** — it has a per-position term. The store must carry it:

```rust
#[repr(C)]
pub union PositionExtraRepr { /* sized to the largest variant */ }

pub struct AaveV4Extra {
    pub risk_premium: Ray,          // current premium rate
    pub premium_accrued: u128,      // settled at last touch
    pub premium_last_update: u32,
}
pub struct AaveV3Extra   { pub emode_category: u8, pub isolated: bool }
pub struct CompoundV2Extra { pub borrow_index_snapshot: Ray }
```

Size the union from the largest variant and assert it at compile time. Consequence
for GUIDE 08: V4's `time_to_cross` must integrate base rate **and** premium, and
a premium change is a `DirtySet::Positions`, not a market event.

## Step 5 — Undo ring

Every mutation pushes an inverse. This is non-negotiable and it is the thing most
implementations skip.

```rust
pub struct UndoRing { blocks: Box<[BlockUndo; K]>, head: usize }   // K = 128

pub enum UndoOp {
    Supply  { pos: PositionId, slot: u16, prev: u128 },
    Debt    { pos: PositionId, slot: u16, prev: u128 },
    Config  { pos: PositionId, prev: AssetMask },
    Extra   { pos: PositionId, prev: PositionExtraRepr },
    Market  { mkt: u16, prev: MarketRow },
    Created { pos: PositionId },           // drop on unwind
}
```

Unwind is `for op in block.ops.iter().rev() { apply_inverse(op) }` — deterministic,
allocation-free, microseconds. Compare to resyncing from an archive node, which
is seconds of blindness during exactly the volatility that pays.

A reorg deeper than `K` is a **halt condition**, not a recovery path: trip the
halt (GUIDE 14), rebuild from snapshot, alert.

## Step 6 — Pending overlay

Simulation and mempool evaluation must never touch canonical state.

```rust
pub struct StateView<'a> { base: &'a StateStore, overlay: Option<&'a Overlay> }
```

`PositionRef` is always constructed from a `StateView`, so `health()` serves
canonical evaluation, pending evaluation and backtest without branching.

## Step 7 — Snapshot and WAL

- **WAL**: append decoded events per block; fsync on a background thread with
  bounded lag. Never in the hot path.
- **Snapshot**: `memmap2` flat dump of all columns every N blocks. Cold start =
  mmap + replay WAL tail. Target sub-second recovery.
- Analytics go to JSONL + SQLite (D06) off the hot path. Nothing on the hot path
  queries a database, ever.

## Step 7b — Rust: concurrency & safety

See `RUST-CONVENTIONS.md` §1. This crate is where that section's claim is made
concrete.

**Single writer, no synchronization.** `StateStore` is owned by the ExEx thread
and mutated through `&mut`. There is no `Arc`, no `Mutex`, no `RwLock`, and no
atomic anywhere in its definition. The borrow checker is your concurrency proof:
if `apply_log` takes `&mut StateStore`, nothing else can hold a reference while
it runs.

Resist the reflex to wrap it. `Arc<Mutex<StateStore>>` would put the engine
behind the ingest thread's writes and cost you the entire latency budget, and it
is the single most likely wrong turn in this crate.

**Background readers get a published snapshot, not a lock.** The drift sampler
(Step 8), telemetry and the PnL writer all run off-thread and need a consistent
view, not the live one:

```rust
// Owned in `Shared`, built once at startup, handed out as `&'static`.
// NOT a `static` + `LazyLock`: that buys an acquire load and a branch on every
// access to avoid an ordering problem this process does not have.
// RUST-CONVENTIONS.md §3.3.
// `shared: &'static Shared` holds `snapshot: ArcSwap<StoreSnapshot>`.

// Writer thread, end of block: publish a cheap immutable view.
shared.snapshot.store(Arc::new(store.snapshot()));

// Drift sampler, any thread: wait-free, consistent, no contention.
let snap = shared.snapshot.load();
```

`snapshot()` must not deep-copy the columns. Share the underlying `Arc<[u128]>`
buffers and copy only what changed this block, or snapshot at a coarser cadence
(every N blocks) if copying proves measurable.

**Column type.** Prefer one flat `Vec<u128>` per slot with a fixed stride over
`Vec<Vec<u128>>` — it removes a pointer chase per access and makes the whole
column one allocation you make once at startup.

**Fixed-size undo ring.** `Box<[BlockUndo; K]>`, allocated at startup, with each
`BlockUndo.ops` a pre-reserved `Vec` that is `clear()`ed rather than dropped.
Reusing capacity means a block of 10k mutations allocates nothing.

**No indexing, no unwrap.** `clippy::indexing_slicing` is denied in this crate,
so every access is `get()`/`get_mut()` with the `None` case returning
`StateError::UnknownPosition`. On the hot path, bounds-check once by iterating
`AssetMask` set bits and then use the checked accessors — the branch is
predictable and it is not your bottleneck.

**`PositionExtra` as a union without `unsafe`.** Use `bytemuck` with derived
`Pod`/`Zeroable` for the fixed-size representation rather than a raw `union` and
`transmute`. Same layout guarantee, `#![forbid(unsafe_code)]` intact.

## Step 8 — Drift detector scaffold

Lives here (needs raw state access) but is a risk system (GUIDE 14 wires the
halt). Build the sampling loop now so GUIDE 04 can use it immediately:

```rust
pub struct DriftDetector {
    /// Stratified: over-sample Hot/Warm, under-sample Cold.
    strata: [(Band, u32); 4],
    mismatch: HashMap<ProtocolId, Ewma>,
    max_mismatch_bps: u32,
}
```

Each interval: sample K positions, issue `health_probe()` calls, compare to local
`health()`, record the delta distribution. This is how you find missed event
paths, and it is the acceptance criterion for GUIDE 04.

---

## Acceptance criteria

- [ ] `health()` on a 3-market position touches ≤ 16 cache lines (verify with
      `perf stat -e cache-misses` on a synthetic sweep). Budget: per market one
      `MarketRow` line (the hot half), one `supply[slot]` line, one `debt[slot]`
      line = 9; plus the `AssetMask`, the position's `PositionExtra`, and the
      `PriceVector` lines for the three assets (≤ 3, usually 1) — 12 to 15.
      `size_of::<MarketRow>() <= 128` is asserted at compile time.
- [ ] Sweeping 1,000 positions completes in < 50 µs on the target hardware
- [ ] Apply-then-unwind a block of 10k mutations restores byte-identical state
      (property test with random mutation sequences)
- [ ] Unwinding 128 blocks completes in < 5 ms
- [ ] Reorg deeper than `K` returns a distinct error, not a silent partial unwind
- [ ] Snapshot + WAL cold start of a 50k-position store is < 1 s
- [ ] `size_of::<PositionExtraRepr>()` is asserted at compile time
- [ ] No allocation occurs inside `StateView::position()` or `health()`
- [ ] `StateStore` contains no `Arc`, `Mutex`, `RwLock` or atomic in its definition
      (grep-assert it in CI)
- [ ] Background readers use an `arc-swap` snapshot; no reader ever takes a lock
      the writer also takes
- [ ] Crate builds with `#![forbid(unsafe_code)]` and deny-level `unwrap_used`,
      `expect_used`, `indexing_slicing`

## Failure modes

| Symptom | Cause |
|---|---|
| Health factors silently wrong after a reorg | Undo records not pushed for every mutation path — usually the adapter's "fast path" |
| Latency spikes on busy blocks | Row-major layout, or `AssetMask` not checked first |
| V4 positions drift from chain over hours | Risk premium not accrued per-position |
| Cold start takes minutes | Backfilling instead of restoring from snapshot |
| `PositionExtra` heap-allocated | Modelled as a trait object instead of a union |

## Handoff

GUIDE 03 fills this store from the chain. Do not write an adapter (GUIDE 04)
before the undo property test passes — an adapter built on a store that cannot
unwind will need every `apply_log` path revisited.
