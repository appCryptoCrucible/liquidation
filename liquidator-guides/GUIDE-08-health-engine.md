# GUIDE 08 — Health Engine

| | |
|---|---|
| **Crate** | `liq-engine` |
| **Prerequisites** | GUIDE 02, 04, 06 (Steps 2–3), 07; 05's fixtures (05D) for the "no new InScope miss" acceptance |
| **Work packages** | **08A** engine (bands, threshold index, time heap, ticks, candidates); **08B** trigger sources (governance poller in `liq-oracle`, derived-rate fan-out, stale). `Band` is a `liq-types` type (D46) because `StateStore` stores it |
| **Est. effort** | 1.5–2 weeks |
| **Blocks** | 09, 12, 16 |

## Objective

Turn a price tick into the exact set of positions that crossed, in O(log N + k),
and emit prioritized candidates. This crate must not depend on `liq-adapters` —
the GUIDE 00 lint enforces it.

## Build vs. buy

100% build. ~1,500 lines, all of it specific to the problem.

---

## Step 1 — Band manager

```rust
pub enum Band {
    Hot,    // hf < 1.02      recompute every tick; plan + calldata held warm
    Warm,   // 1.02..1.15     recompute on every relevant tick
    Cool,   // 1.15..1.50     recompute on index updates and large moves
    Cold,   // hf >= 1.50     threshold tripwire only
    Dead,   // no debt        excluded; kept for re-entry detection
    /// Debt is not flashloanable at the size required (GUIDE 07).
    /// Tracked cheaply, never promoted — eligibility flips back when
    /// flash liquidity returns, so NEVER delete these.
    Unfundable,
}
```

Bands are a **cost optimization, never the correctness mechanism**. A Cold
position can gap straight through Warm on a 30% move. Correctness lives in the
threshold index; bands only decide how often you re-derive.

Positions whose debt is not flashloanable at the required size are `Unfundable`
and never promoted, however unhealthy they get — under flashloan-only funding
you cannot act on them (GUIDE 07). Re-evaluate eligibility when `FlashIndex`
availability for that asset moves materially, not every block.

Expected population on a large protocol: Cold 95%+, Cool hundreds, Warm tens,
Hot single digits. A full exact recompute of Hot+Warm is a few microseconds — do
it unconditionally on any tick touching those assets.

## Step 2 — Threshold index

```rust
pub struct ThresholdIndex {
    /// One sorted array per GLOBAL AssetId — spans all protocols, which is
    /// why AssetId had to be global (GUIDE 00).
    falling: Vec<SortedVec<(Price, PositionId)>>,   // collateral: crosses on drop
    rising:  Vec<SortedVec<(Price, PositionId)>>,   // debt: crosses on rise
    registered: HashMap<(PositionId, AssetId), Price>,
    dirty: BitSet,
}

pub fn crossed(&self, a: AssetId, old: Price, new: Price)
    -> impl Iterator<Item = PositionId> + '_
{
    if new < old { self.falling[a].range(new..=old).map(|(_, id)| *id) }
    else         { self.rising[a].range(old..=new).map(|(_, id)| *id) }
}
```

**The multi-asset caveat, stated honestly.** A registered threshold assumes all
other prices fixed. When several move together — a crash, exactly when it matters
— registered thresholds are stale. Three mitigations, use all three:

1. **Register with a safety margin** — the price at HF = 1.03, not 1.00. False
   positives cost a recompute, not money.
2. **Correlated-move sweep** — when multiple assets moved in the same window,
   sweep Cool as well as Hot+Warm. Cheap to detect, rare enough to afford.
3. **Lazy re-derivation** — any position recomputed for any reason re-registers
   with fresh thresholds. Active positions self-heal.

## Step 3 — Time-cross heap

For positions that cross from accrual alone: stable collateral, stable debt, the
boring ones nobody watches.

```rust
pub struct TimeCrossHeap {
    heap: BinaryHeap<Reverse<(Timestamp, PositionId, Generation)>>,
    generation: Vec<Generation>,       // bumped on rate change; lazy deletion
}
```

Call `Protocol::time_to_cross()`, push `(t*, id, gen)`. On a rate change, bump
that market's generation; stale entries are discarded on pop, not eagerly
removed — an eager rebuild is O(N) on every rate update.

**Aave V4 note:** `time_to_cross` includes the per-position risk premium
(GUIDE 04, Step 5). A position with a high premium crosses meaningfully sooner
than its market rate implies. If this heap fires late on V4 positions, the
premium term is missing.

**Why this matters commercially:** these opportunities are uncontested relative
to oracle-driven ones, because winning requires knowing the second in advance
rather than winning an auction. Lower margin, near-100% win rate, and they fund
your infrastructure while you are still learning to bid.

## Step 4 — Tick handling

```rust
pub fn on_price_tick(&mut self, tick: PriceTick) {
    match tick.kind {
        // Pre-warm only. Compute, plan, sign — do not fire.
        SourceKind::Predicted { .. } => {
            self.prewarm(tick.asset, tick.price);
        }
        // Actionable. Fire.
        SourceKind::SvrAnnounced { .. } | SourceKind::PendingPublic { .. } => {
            let crossers = self.index.crossed(tick.asset, self.px[tick.asset], tick.price);
            self.exact_recompute(crossers.chain(self.hot_and_warm(tick.asset)));
        }
        SourceKind::Canonical => self.commit_price(tick),
        SourceKind::Derived { .. } => self.fan_out_derived(tick),
    }
}
```

The `Predicted` / announced split is the contract with GUIDE 06 and it is the
whole reason prediction pays under SVR: by the time the MEV-Share hint lands, the
candidate set, quote and route already exist and you are only confirming and
bidding.

## Step 5 — Candidate emission

```rust
pub struct Candidate {
    pub position: PositionId,
    pub protocol: ProtocolId,
    pub health: Health,
    pub quote: Quote,              // carries BonusCurve — see below
    pub cause: TriggerCause,
    pub est_value: Wad,
    pub deadline: Instant,
    pub trace: TraceId,
}

pub enum TriggerCause {
    // ── auctioned: you bid against other searchers via the protocol's own
    //    recapture mechanism. Margin is compressed by design.
    SvrAuction { hint: B256, deadline: Instant },

    // ── contested but not auctioned: a latency/skill race
    OraclePublic { tx: TxHash },                   // bundle behind the transmit
    OraclePullHeld { payload: SignedUpdate },      // you submit the price
    PoolStateChange { pool: Address },             // TWAP / LP collateral
    UserAction { tx: TxHash },                     // they borrowed into it

    // ── uncontested: won by breadth and correctness, not speed
    DerivedRate { source: AssetId },               // wstETH rate, sDAI chi, LRT
    ParamChange { market: MarketId },              // governance; TIMELOCKED, so
                                                   // schedulable in advance
    InterestDrift,                                 // closed-form crossing time
    Stale { liquidatable_since: BlockNum },        // nobody took it

    // ── not fireable: pre-warm only
    OraclePredicted { conf: f32 },
}
```

`TriggerCause` is not decoration — GUIDE 13 branches on it for venue and bid
strategy. An `InterestDrift` candidate should never pay an auction bid; an
`SvrAuction` candidate must never be sent as a naked public transaction.

**Track every trigger.** The engine is trigger-agnostic: the same threshold
index, bands and recompute serve all of them, so supporting the full set costs
almost nothing marginal. The grouping above is about *where to invest effort and
how to execute*, never about which triggers to detect.

Two of these are easy to omit and are where an entrant's edge plausibly lives
(`PRE-FLIGHT.md` §3):

- **`DerivedRate`** — a wstETH position goes underwater when `stEthPerToken()`
  moves, not only when a Chainlink feed does. A competitor subscribed to
  aggregators alone never sees it. GUIDE 06 §7 makes you model these contracts
  for correctness; register them as price sources and they become triggers too.
- **`ParamChange`** — governance executes behind a timelock, so an LTV or
  liquidation-threshold cut is knowable **weeks ahead**. Subscribe to the
  governance contracts, precompute the exact position set that becomes
  liquidatable at the execution block, and schedule it. Deterministic, and
  nobody is bidding against you for public information.

`Stale` exists so you notice positions that have been liquidatable for several
blocks and were never taken — below other searchers' `min_viable_notional`, on a
protocol nobody watches, or because someone's bot was down. Full-universe
coverage (GUIDE 15) is what turns that into revenue.

**Pass `Quote` through intact, including its `BonusCurve`.** The engine does not
decide when to fire on a `HealthLinear` curve — GUIDE 12 does, with a competitor
model. The engine's job is to surface the candidate with enough information for
that decision.

## Step 5b — Rust: concurrency & safety

**The engine runs on the ExEx thread.** It is invoked synchronously from the
dirty-set flush at the end of `apply_chain` (GUIDE 03). It therefore holds
`&mut` to its own structures and `&` to the store, and needs **no
synchronization whatsoever** — no `Arc`, no lock, no atomic.

This is the payoff of `RUST-CONVENTIONS.md` §1. The most latency-sensitive code
in the system is also the code with the least concurrency machinery in it, and
that is not a coincidence.

**Prices arrive by triple buffer, not by channel.** The oracle thread publishes
complete `PriceVector`s (GUIDE 06); the engine reads the latest wait-free at the
top of each evaluation. It never waits on the oracle and never drains a backlog
of superseded prices.

**The threshold index is plain sorted `Vec`s.** `SortedVec<(Price, PositionId)>`
per asset, and `range()` lookups are binary searches over contiguous memory. No
`BTreeMap` — the pointer chasing costs more than the occasional insertion shuffle
at these sizes, and the linear scan over a matched range is cache-friendly.

Re-sorting on threshold re-derivation: collect the changed entries, then do one
pass rather than N individual inserts. Amortize over the block.

**The time-cross heap is a plain `BinaryHeap`** with lazy deletion via
generation counters. Single-threaded, so nothing exotic is warranted.

**Allocation discipline.** Every structure is sized at startup for the full
position universe (GUIDE 16). `crossed()` returns an iterator borrowing the
index, never a collected `Vec` — the caller consumes it directly.

```rust
// Borrows; allocates nothing.
pub fn crossed(&self, a: AssetId, old: Price, new: Price)
    -> impl Iterator<Item = PositionId> + '_
```

**No indexing, no unwrap.** `clippy::indexing_slicing` is denied. Band lookups
and threshold arrays are accessed through `get()` with the `None` arm returning
an error that halts the protocol — never a panic, because a panic here kills the
node.

## Step 6 — Bounded priority channel

Output into a **bounded** priority channel ordered by `est_value`. Under a
market-wide crash you will produce thousands of candidates in one block and you
want the most valuable, not the first. Dropping low-value candidates under load
is correct; blocking the engine is not.

Instrument the drop rate — a nonzero drop rate during calm markets means the
channel is undersized or downstream is too slow.

---

## Acceptance criteria

- [ ] `liq-engine` has zero dependency on `liq-adapters` (GUIDE 00 lint green)
- [ ] Threshold lookup is O(log N + k): benchmark at 10k, 100k and 1M registered
      thresholds and confirm sub-linear scaling
- [ ] Full exact recompute of Hot+Warm (200 positions) < 20 µs
- [ ] Correlated-move sweep triggers when ≥ 3 assets move in one block
- [ ] Replay (GUIDE 05) shows **no new `InScope` miss** with bands and the
      threshold index enabled — this is the test that proves the optimization is
      safe, and it is the reason the bands are allowed to exist at all
- [ ] Time-cross heap fires within one block of the actual crossing for a V4
      position with a nonzero risk premium (fixture)
- [ ] `Predicted` ticks never produce a fireable candidate
- [ ] Bounded channel drops lowest-value candidates under synthetic cascade load,
      and the drop is counted
- [ ] Positions with unfundable debt are marked `Unfundable`, not deleted, and
      are promoted again when flash liquidity returns (fixture)
- [ ] Candidates carry `Quote.repay_options` intact so GUIDE 12 can pick a
      fundable debt leg
- [ ] End-to-end tick → candidate p99 < 400 µs
- [ ] Engine holds no lock and no atomic; it runs on the ExEx thread and the
      types prove it (no `Arc<Mutex<_>>` anywhere in the crate)
- [ ] `crossed()` returns a borrowing iterator, not a collected `Vec`

## Failure modes

| Symptom | Cause |
|---|---|
| Recall drops when bands are enabled | Thresholds registered at exactly HF = 1.0 with no margin; or Cold positions not registered at all |
| Missed liquidations during crashes only | No correlated-move sweep; single-asset thresholds stale |
| Latency fine in tests, terrible live | Recomputing on `Predicted` ticks as if they were real |
| V4 interest-driven crossings always late | Risk premium missing from `time_to_cross` |
| Engine stalls during volatility | Unbounded channel, or a blocking send |
| Candidates fire at HF = 0.9999 and under-earn on V4 | `BonusCurve` collapsed before reaching GUIDE 12 |

## Handoff

GUIDE 09 instruments this so shadow mode can measure it. Build observability
*before* execution — you want weeks of shadow data on candidate quality and
timing before any capital is at risk.
