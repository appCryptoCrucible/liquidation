# GUIDE 11 — Simulation

| | |
|---|---|
| **Crate** | `liq-sim` |
| **Prerequisites** | GUIDE 02, 03 (Step 6 — the ExEx provider), 10A/10B (bytecode + encoder; **not** the deploy — pre-H3 the Executor bytecode is inserted into `CacheDB` at the planned address), 05B for the 100-liquidation replay |
| **Work package** | **11** |
| **Est. effort** | 1 week |
| **Blocks** | 12, 13, 16 |

## Objective

Verify a candidate bundle against real pending state in under a millisecond,
in-process, with no RPC.

## Build vs. buy

**~85% buy.** `revm` 43.0.2 does the execution. You supply the state provider,
the warm cache policy, and the bundle assembly. This is the highest
buy-to-build ratio in the project — do not write an EVM.

---

## Step 1 — State provider from the node's own database

Because the bot runs as a Reth ExEx (GUIDE 03), you have direct access to the
node's `StateProviderFactory`. Use it.

```rust
pub struct Simulator<P: StateProviderFactory> {
    provider: Arc<P>,
    db: CacheDB<StateProviderDatabase<P::StateProvider>>,
    warm: WarmSet,
}
```

`eth_call` against even a local node costs a round-trip plus serialization;
against a remote node it costs the race. The whole point of the ExEx architecture
is that this step is a memory read.

## Step 2 — Warm the account cache

Your executor, the routers, the tokens, the Aave Hub and Spokes, the pools — the
same addresses every single time. Keeping them resident cuts simulation from
~1 ms to ~150 µs.

```rust
impl Simulator {
    /// Clear transient state between sims, KEEP the warm accounts.
    fn reset(&mut self) { self.db.clear_except(&self.warm); }
}
```

Populate `WarmSet` from config at startup and refresh it when the route cache
(GUIDE 12) discovers a new pool that recurs.

## Step 3 — Simulate the bundle, not the transaction

This is the mistake that makes simulation useless. The profit depends on the
oracle update landing first; simulating your liquidation against *current* state
tells you nothing.

```rust
pub fn verify(&mut self, bundle: &Bundle, at: BlockEnv) -> Result<SimOutcome, SimError> {
    self.reset();
    // 1. Apply the triggering transaction:
    //    - SVR: reconstruct the oracle update from the MEV-Share hint.
    //      If the hint omitted callData, substitute the PREDICTED price from
    //      the aggregator simulator and mark the outcome low-confidence.
    //    - Public: the decoded transmit() tx.
    //    - InterestDrift: nothing — just advance the timestamp.
    // 2. Apply your calls in order.
    // 3. Read back balances; compute net profit.
    // 4. Return ACTUAL gas used.
}
```

**The partial-hint case matters.** MEV-Share senders choose what to share, so
`callData` may be absent. Simulating on a predicted price is legitimate — flag
the outcome's confidence and let GUIDE 12 shade the bid accordingly rather than
refusing to bid.

## Step 4 — Use simulated gas, never an estimate

Return actual gas used and feed it to the bid model. On mainnet, gas is a
first-order term in the bid:

- Overestimating inflates your cost model → you bid low → you lose auctions you
  should win
- Underestimating → reverts, or bids you cannot afford

## Step 4b — Parallel variant simulation (baremetal)

With dedicated cores (GUIDE 16) the single-variant rule below relaxes in one
specific, bounded way: simulate plan *variants* concurrently and take the first
that passes.

- Alternative flash sources from GUIDE 07's fallback chain
- Alternative repay × seize combinations from GUIDE 12 Step 2
- **Top split-routing candidates from the water-fill** (GUIDE 12 Step 4) — the
  exact math ranks them, simulation confirms the best 2–3
- Two candidate firing points on Aave V4's bonus curve

Give each variant a dedicated core and a pre-warmed revm instance. Wall-clock
cost is one simulation. This is bounded parallelism over a *precomputed* variant
set — it is not a search, which is the distinction the next step draws.

## Step 4c — Rust: concurrency & safety

**Thread-per-core, share nothing.** Each simulation worker owns its own `revm`
instance and its own `CacheDB`. No worker touches another's state, so there is
no lock, no atomic and no lock-free structure in this crate.

```rust
// Fixed pool, built at startup, pinned to dedicated cores (GUIDE 16).
struct SimWorker {
    evm: MainnetEvm<CacheDB<StateProviderDatabase<P>>>,   // owned outright
    warm: WarmSet,
    inbox: rtrb::Consumer<SimRequest>,                    // SPSC in
    outbox: rtrb::Producer<SimOutcome>,                   // SPSC out
}
```

One SPSC ring per worker in each direction. The dispatcher knows which worker it
sent to, so a shared MPSC return channel would add contention for nothing.

**Prefer `std::thread::scope` over `rayon`** for the bounded parallel variant
fan-out (Step 4b). The variant set is small and fixed, the workers are pinned,
and scoped threads borrow the plan without `Arc`:

```rust
std::thread::scope(|s| {
    for variant in &variants {                  // borrowed, not cloned
        s.spawn(|| worker.verify(variant));
    }
});
```

`rayon`'s work-stealing scheduler fights your core pinning and adds a global
thread pool you do not control. Use it for background batch work if you like;
not here.

**The state provider is shared, read-only.** `Arc<dyn StateProviderFactory>` is
cloned per worker. Reads are concurrent and immutable, so no synchronization is
needed beyond the `Arc` itself.

**`clear_except(warm)`, never `clear()`.** Reusing the warm accounts is the
optimization in Step 2, and it also means the per-simulation allocation is
near-zero. The `CacheDB`'s maps keep their capacity across simulations.

**Panics in revm.** A malformed plan should return `SimError`, not panic. Wrap
worker bodies in `catch_unwind` anyway — this crate runs inside the Reth process,
and a panicking worker thread must not take the node with it.

## Step 5 — Simulation is a gate, not a search

Simulate **once**, for the plan you already chose. If you find yourself
simulating ten variants to pick one, the router is underspecified and you have
blown the latency budget on search that belongs in the warm path.

The one legitimate exception: for Aave V4's variable bonus, GUIDE 12 may want to
evaluate two firing points. Two is fine. Ten is a design smell.

## Step 6 — Failure classification

```rust
pub enum SimError {
    GuardRejected { hf: Ray },        // position recovered; drop quietly
    InsufficientLiquidity { pool: Address },
    SlippageExceeded { expected: U256, actual: U256 },
    ProfitBelowFloor { net: I256 },
    Revert { reason: Bytes },         // ← investigate every one of these
    StateUnavailable,                 // node lag; feeds GUIDE 14
}
```

`Revert` with an unexpected reason is a bug somewhere, not a market condition.
Alert on it; it is how you find contract and encoding errors before they cost
a won auction.

---

## Acceptance criteria

- [ ] Simulation runs fully in-process; zero RPC calls in the hot path (assert
      with a provider that panics on network access)
- [ ] p99 simulation latency < 1 ms with a warm cache; < 200 µs typical
- [ ] Cold-cache simulation is measurably slower, proving the warm set works
- [ ] Replaying 100 historical liquidations through `verify()` reproduces the
      actual on-chain profit within 1% for each
- [ ] Gas returned matches actual on-chain gas within 1% on those replays
- [ ] Partial-hint path (no `callData`) produces a low-confidence outcome rather
      than an error
- [ ] `SimFalseNegative` rate measured against shadow data is < 1%
- [ ] Simulation correctly rejects a bundle whose position recovered between
      candidate emission and simulation

## Failure modes

| Symptom | Cause |
|---|---|
| Sim passes, chain reverts | Simulated at the wrong fork point, or didn't apply the oracle tx first |
| Sim rejects profitable opportunities | Stale route cache, or gas model inflated by an estimate rather than sim output |
| Latency 10× expected | Cache cleared completely between sims instead of `clear_except(warm)` |
| Occasional `StateUnavailable` | Node lag — surface to the halt matrix, don't retry blindly |
| Every SVR candidate rejected | Treating an absent `callData` as an error instead of substituting the predicted price |

## Handoff

GUIDE 12 consumes `SimOutcome.gas_used` and the confidence flag for the bid
model. Both are load-bearing — do not stub them.
