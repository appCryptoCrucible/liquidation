# GUIDE 03 — Ingest, Reorg & Mempool

| | |
|---|---|
| **Crate** | `liq-node` |
| **Prerequisites** | GUIDE 00, 02; GUIDE 16 Step 3b (thread map, WP 16A) for the hot thread; the node (A2) for Step 6's acceptance only |
| **Work packages** | **03A** sources + router + arena decode + dirty set + backfill (no node needed); **03B** ExEx forwarder + hot thread + reorg + panic containment + mempool ring; **03C** event-coverage audits. `HaltSink` and `LogSubscriber` are `liq-types` traits (D46) |
| **Est. effort** | 1–1.5 weeks (plus node sync time — start the sync on day one) |
| **Blocks** | 04, 05, 06, 07, 11, 16 |

## Objective

Get every relevant log into the state store with sub-millisecond latency and
correct reorg handling, and get pending transactions into a separate overlay that
never contaminates canonical state.

## Build vs. buy

**Buy Reth and run your bot as an ExEx.** This is the highest-leverage vendoring
decision in the project. An Execution Extension compiles into the Reth binary and
runs in-process: no JSON, no IPC, no polling. Reth delivers chain committed /
reverted / reorged notifications directly, which means your reorg signal is free
and exact rather than inferred from block hashes.

Buy `alloy`'s `sol!` macro for codegen'd ABI decode. Build the log router, the
dirty-set dispatch, and the mempool overlay.

---

## Step 0 — The node is a day-0 item, not a GUIDE 03 item

The node comes up on day 0 via `reth download` after the prune profile is signed
off (GUIDE 16 Step 0b; Track A in `STATE.md`; H1 → A2). That is hours, not days,
and it happens before any code in this guide. There is **one** box and one node
(D05, D07): no warm spare, no archive node. What this guide needs from Track A is
only that A2 is `started` by the time WP 03B's acceptance runs.

## Step 1 — ExEx skeleton

An ExEx is a `Future` that runs alongside Reth and receives notifications.

```rust
async fn liquidator_exex<Node: FullNodeComponents>(
    mut ctx: ExExContext<Node>,
    mut store: StateStore,
    router: LogRouter,
) -> eyre::Result<()> {
    while let Some(notification) = ctx.notifications.try_next().await? {
        match &notification {
            ExExNotification::ChainCommitted { new } => {
                apply_chain(&mut store, &router, new)?;
            }
            ExExNotification::ChainReverted { old } => {
                unwind_chain(&mut store, old)?;          // GUIDE 02 undo ring
            }
            ExExNotification::ChainReorged { old, new } => {
                unwind_chain(&mut store, old)?;
                apply_chain(&mut store, &router, new)?;
            }
        }
        if let Some(tip) = notification.committed_chain() {
            ctx.events.send(ExExEvent::FinishedHeight(tip.tip().num_hash()))?;
        }
    }
    Ok(())
}
```

**The bot ships inside the Reth binary.** This is not a bot process talking to a
node process — `cargo build` produces one executable containing both. That is
what buys you sub-100 µs state access, and it costs you: a deploy restarts the
node (GUIDE 17), pinning is thread-level (GUIDE 16), and a panic in an adapter
can kill the node unless you contain it at this boundary. Wrap `apply_chain` so
an adapter fault becomes a protocol halt, never a process abort.

**But do not do the work inside the ExEx future.** The sketch above is shown
inline for clarity; the production shape is different and the difference matters.
The ExEx future is polled by **Reth's own Tokio runtime**. Folding state and
running the engine there means 2–3 ms of blocking work on a worker Reth also
uses, on a thread you cannot pin or isolate because Reth owns that runtime.

The ExEx is therefore a **thin forwarder** — it pushes the notification to a
dedicated, pinned, non-async hot-path thread over an SPSC ring and waits for
confirmation before emitting `FinishedHeight`:

```rust
while let Some(n) = ctx.notifications.try_next().await? {
    to_hot.push(Notification::from(n))?;          // owned payload only
    if let Ok(done) = from_hot.pop() {            // hot thread confirms consistency
        ctx.events.send(ExExEvent::FinishedHeight(done.num_hash))?;
    }
}
```

The round trip costs roughly a microsecond and buys real core isolation
(GUIDE 16), a hot path outside any runtime, and the guarantee that a slow block
never starves Reth's own tasks. `RUST-CONVENTIONS.md` §5.1 has the full pattern
including the hot thread's loop.

**`FinishedHeight` is load-bearing.** It tells Reth what it may prune, and it is
the backpressure mechanism — you only receive notifications for blocks above your
last reported height. Emit it after the store is consistent, never before. Emit
it too early and a crash loses blocks you claimed to have processed.

## Step 2 — Log router

One union filter across all adapters, decoded once, dispatched by address.

```rust
pub struct LogRouter {
    by_address: HashMap<Address, SmallVec<[AdapterIdx; 2]>>,
    sigs: HashMap<B256, EventKind>,     // topic0 -> jump table index
}
```

Build it from `Protocol::subscriptions()` **and `FlashSource::subscriptions()`**
(GUIDE 07) at startup — flash liquidity is tracked from the same log stream, never
polled. Dispatch must be a hash
lookup plus a jump table, never a linear match over event names.

Use `alloy::sol!` to generate decoders. Decode into a reusable arena — do not
allocate per log. On a busy block Aave alone emits hundreds of logs and the
allocator will show up in your flamegraph.

## Step 3 — The event coverage audit

This step is where silent drift is prevented, and it deserves more care than its
size suggests. For each protocol, enumerate **every** way a position's state can
change, not just the obvious user actions:

- Supply / Withdraw / Borrow / Repay
- **Collateral-token and debt-token `Transfer`** — positions move with no
  protocol event. This is the single most common cause of drift.
- Liquidation events (yours and competitors')
- Collateral enable/disable toggles
- E-mode / category changes (V3), Spoke config changes (V4)
- **Risk premium updates (V4)** — per-position, easy to miss
- Rate/index updates (→ `DirtySet::MarketAccrual`)
- Governance parameter changes (→ `DirtySet::MarketReprice`)
- Reserve freeze/pause/unpause
- Deficit / bad-debt events
- Proxy implementation changes (→ halt; GUIDE 14)

Write this list into a checked-in document per adapter, and have GUIDE 04's
drift detector prove it is complete. An adapter is not done because it compiles;
it is done because the drift detector is quiet for a week.

## Step 4 — Dirty-set application

```rust
fn apply_chain(st: &mut StateStore, r: &LogRouter, chain: &Chain) -> Result<()> {
    let mut dirty = DirtyAccumulator::new();
    for (_, receipt) in chain.receipts_with_attachment() {
        for log in &receipt.logs {
            if let Some(adapters) = r.by_address.get(&log.address) {
                let decoded = r.decode(log)?;
                for a in adapters {
                    dirty.merge(a.apply_log(st, &decoded)?);
                }
            }
        }
    }
    dirty.flush_to_engine();     // GUIDE 08 consumes this
    Ok(())
}
```

`DirtyAccumulator` must collapse: a `MarketReprice` subsumes a `MarketAccrual`
for the same market, which subsumes individual `Positions` in it. Do this
merging before dispatch, not after.

## Step 4b — Rust: concurrency & safety

**The ExEx future is async; the hot path is a separate, pinned, non-async
thread** (Step 1). That `.await` on `ctx.notifications` is the only await in the
ingest path, and it lives in the forwarder — never in `apply_chain`. Enforce it
structurally: `liq-state`, `liq-protocol`, `liq-adapters` and `liq-engine` must
have no async runtime in their dependency tree at all, so the mistake cannot
compile. `RUST-CONVENTIONS.md` §5.2 has the full classification.

**Panic containment is implemented here** (`RUST-CONVENTIONS.md` §2.5). Wrap each
adapter dispatch in `catch_unwind(AssertUnwindSafe(..))` and convert a panic into
a protocol halt. This requires `panic = "unwind"` in the release profile —
`panic = "abort"` makes containment impossible and turns any adapter bug into a
node outage.

After catching a panic mid-fold the store may hold a partially applied block.
Unwind it with the undo ring before halting, or the next block folds onto
corrupt state.

**Mempool stream: SPSC ring, drop-oldest.**

```rust
let (mut prod, mut cons) = rtrb::RingBuffer::<PendingTx>::new(4096);

// Network thread — never blocks.
if prod.push(tx).is_err() { metrics::counter!("mempool_dropped").increment(1); }

// Hot thread, start of block — drain, never wait.
while let Ok(tx) = cons.pop() { overlay.apply(tx); }
```

A bounded `crossbeam` MPSC would work but pays for producers you do not have;
this stream has exactly one. Dropping is correct — a stale pending transaction is
worthless — but it must be **counted**, never silently swallowed.

**Decode into an arena.** `bumpalo`, reset at the top of each block. A busy block
emits hundreds of logs and per-log allocation will appear in your flamegraph.
Decoded structs borrow from the arena, so they cannot outlive the block — which
is exactly the lifetime you want and the borrow checker enforces it for free.

**`DirtyAccumulator` reuses its buffers.** `clear()` between blocks, never
reallocate. Its sets are `SmallVec` sized for the common case.

## Step 5 — Backfill

Two strategies; pick per protocol by cost.

- **Log replay** — scan from the deployment block. Correct by construction,
  slow, parallelizable by block range with a merge step.
- **Storage scan** — read position mapping slots directly at a pinned block.
  Much faster where the layout is simple (Morpho Blue; Aave V4's `_userPositions`
  is 3 slots per position and tractable).

Either way: backfill to a pinned block, **snapshot immediately**, then replay
forward. The snapshot is your cold-start artifact — you should never backfill
twice.

**Universe reduction:** only positions with nonzero debt can be liquidated.
Filter hard. This is what turns millions of addresses into a few thousand live
borrowers and makes the hot path fit in cache.

## Step 6 — Mempool overlay

Separate actor, separate channel, **never writes canonical state**.

```rust
pub struct Overlay {
    deltas: HashMap<PositionId, PositionDelta>,
    pending_prices: HashMap<FeedId, (Price, TxHash)>,
    pending_swaps: Vec<PendingSwap>,      // for TWAP/LP-priced collateral
}
```

Cleared every block. Backpressure policy differs by stream and this matters:

- **Canonical stream**: unbounded, never drop. Dropping is corruption.
- **Mempool stream**: bounded, **drop-oldest**. A stale pending tx is worthless,
  and blocking ingest on a full queue loses you a block.

**Mainnet note.** For Aave's SVR-protected feeds the oracle update does *not*
appear in the public mempool in a useful way — it is routed through Flashbots
MEV-Share (see `DEPENDENCIES.md` §0.2, and GUIDE 06). The public mempool watcher
still earns its place for non-SVR feeds, user transactions that push positions
underwater, and pool-state changes for TWAP/LP collateral. Build it, but do not
expect it to win Aave races.

---

## Acceptance criteria

- [ ] ExEx runs in-process with Reth and survives a full sync to tip
- [ ] Block-to-store-consistent latency p99 < 500 µs on a busy block, measured
      from `ChainCommitted` receipt to dirty-set flush
- [ ] Injected reorg test: fork the chain at depth 1, 8 and 64, assert state is
      byte-identical to a fresh replay of the canonical branch
- [ ] Reorg deeper than the undo ring returns the halt error, not a partial unwind
- [ ] Event coverage document exists per adapter and is referenced in CI
- [ ] Zero allocations per log in the decode path (verify with a counting allocator)
- [ ] `FinishedHeight` is emitted only after store consistency; kill -9 mid-block
      and restart produces no gap and no double-apply
- [ ] Mempool queue full does not stall canonical ingest (load test)
- [ ] Fault injection: a deliberately panicking adapter produces a protocol halt
      and the node stays up
- [ ] Release profile has `panic = "unwind"`; a CI check rejects `panic = "abort"`
- [ ] Mempool drops are counted, not silent; the counter is on a dashboard
- [ ] No `.await` anywhere inside `apply_chain` (grep-assert)

## Failure modes

| Symptom | Cause |
|---|---|
| State drifts over days, no errors | Missing event path — almost always token `Transfer` |
| Gap in state after a crash | `FinishedHeight` emitted before the store was consistent |
| Latency spikes correlate with volatile blocks | Per-log allocation, or linear dispatch |
| Reorg leaves ghost positions | `UndoOp::Created` not pushed on first sight |
| Engine recomputes everything every block | `DirtyAccumulator` not collapsing, or adapter over-reporting |
| Sync blocks the project for a week | Node sync not started on day one |

## Handoff

GUIDE 04 writes the first adapter against this pipeline. Do not proceed until
the injected-reorg test passes at depth 64 — every adapter written before then
will have `apply_log` paths that lack undo records.
