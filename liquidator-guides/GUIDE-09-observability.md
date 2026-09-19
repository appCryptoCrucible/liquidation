# GUIDE 09 — Observability & Shadow Mode

| | |
|---|---|
| **Crate** | `liq-obs` (completing the GUIDE 00 skeleton) |
| **Prerequisites** | GUIDE 05, 06, 08 (build); **the shadow run additionally needs 10, 11, 12A** |
| **Est. effort** | 1 week, then **4+ weeks of running it** |
| **Blocks** | 13 — **13 must not start before the shadow gate clears**. 12A and 14 do *not* wait for this guide (D49) |
| **Work packages** | **W** (watcher, Step 4a — starts on day 0), **09A** (build), **09B** (start the shadow run — after 12A-2, 11, 10C and the Correctness gate) |

## Objective

Make the system's competitiveness measurable before any capital is at risk, and
build the feedback loop that tells you which layer to optimize next.

This guide is short to implement and long to *run*. The running is the point.

The old prerequisite cycle — 12 listed 09, 09's shadow run needs 12's router —
is resolved by splitting: 09A builds the recorder, the taxonomy and the join
against 08's engine and needs nothing from 12; 09B *starts the run* and is
eligible only when the whole pipeline through `liq-router` and `liq-sim` exists.

## Build vs. buy

Buy `tracing`, `metrics`, a Prometheus exporter and Grafana. The shadow log and
the PnL ledger are JSONL + SQLite (D06, Step 4b) — no ClickHouse. Build the
`TraceId` plumbing and the miss taxonomy.

---

## Step 1 — Complete the trace threading

`TraceId` was introduced in GUIDE 00 and threaded through signatures since. Now
emit at every boundary:

```
price_tick  → candidate      p50 / p99
candidate   → quote          p50 / p99
quote       → route_solved   p50 / p99
route       → sim_verified   p50 / p99
sim         → signed         p50 / p99
signed      → venue_ack      p50 / p99
venue_ack   → inclusion      p50 / p99
──────────────────────────────────────
tick        → inclusion      ← the only number that decides anything
```

Every span carries `ConfigVersion` (GUIDE 00) so a decision from three weeks ago
is reproducible.

## Step 2 — The miss taxonomy

Classify **every** liquidation on every tracked protocol — including ones you
never bid on. This is the core feedback loop of the system.

```rust
pub enum Outcome {
    Won { realized_pnl: I256, bid: U256 },

    /// Never knew the position existed. Adapter or backfill bug.
    NotTracked { position: PositionKey },
    /// Tracked, but computed HF >= 1 when it was liquidatable. MATH BUG.
    HealthWrong { computed: Ray, actual: Ray },
    /// Correct HF, detected after the winner was included.
    DetectedLate { delta_ms: i64 },
    /// Detected in time, rejected on profit. Check route + gas model.
    RejectedUnprofitable { est_net: I256, winner_bid: Option<U256> },
    /// Bid and lost the SVR/MEV-Share auction. Check the bid model.
    Outbid { our_bid: U256, winner_bid: U256 },
    /// Chose to wait for a larger V4 bonus and someone took it first.
    WaitedTooLong { our_target_hf: Ray, actual_hf_at_fill: Ray },
    /// Sim said no, chain said yes.
    SimFalseNegative { reason: SimError },
    /// Won inclusion, tx reverted. Worst kind of loss.
    RevertedOnChain { reason: Bytes },
}
```

Each variant has a different owner and a different fix:

| Variant | Owner | Priority |
|---|---|---|
| `NotTracked`, `HealthWrong` | GUIDE 03/04 | **Must be zero.** Nothing else matters until they are. |
| `DetectedLate` | GUIDE 03/06 | Infrastructure |
| `Outbid` | GUIDE 12 bid model | The main lever under SVR |
| `WaitedTooLong` | GUIDE 12 timing optimizer | V4-specific; tune the competitor model |
| `RejectedUnprofitable` | GUIDE 12 route/gas | Compare `est_net` to `winner_bid` — if the winner paid more than you thought possible, your model is wrong |
| `SimFalseNegative` | GUIDE 11 | Sim state or fork point |
| `RevertedOnChain` | GUIDE 10/12 | Guard logic or stale state |

**Reporting a single "win rate" makes the system unimprovable.** Keep these
separate in every dashboard and every review.

## Step 3 — `WaitedTooLong` deserves special handling

V4's variable bonus creates a decision the prior generation of bots never had:
fire now at a small bonus, or wait for a larger one. `WaitedTooLong` is the
measurement of that tradeoff being wrong in one direction.

Log, for every V4 opportunity: the HF at which you would have fired, the HF at
which it actually filled, and the bonus difference. Over weeks this gives you the
empirical competitor-arrival distribution that GUIDE 12's optimizer needs. It
cannot be derived from first principles — it must be measured.

## Step 4 — Shadow mode

Replace the submitter with a logger. The entire pipeline runs; nothing is sent.

```rust
/// `Submitter` and `IntendedSubmission` live in `liq-types` (D46), so this
/// crate does not depend on `liq-exec` — which does not exist yet when the
/// shadow run starts. `liq-exec`'s real submitters implement the same trait.
pub struct ShadowRecorder { sink: JsonlWriter }   // D06: JSONL on disk, rotated daily

impl Submitter for ShadowRecorder {
    fn submit(&self, s: &IntendedSubmission) -> Result<SubmitReceipt> {
        // `IntendedSubmission` = plan bytes, bid, venue, target block, TraceId,
        // and the wall-clock instant the send would have happened. No signing:
        // the shadow run proves timing and price, not key handling.
        self.sink.write(s)?;
        Ok(SubmitReceipt::Shadow)
    }
}
```

Record every candidate, quote, route, simulated outcome and intended bid with
timestamps. Then join against actual on-chain liquidations and classify with the
Step 2 taxonomy.

Shadow mode answers "would I have won, and at what price?" using the real code
path and zero risk. Run it until the gate clears — **≥ 2 weeks and ≥ 200 contested opportunities**,
whichever is later — before GUIDE 13, and keep
running it permanently for protocols you have not enabled.

## Step 4a — The live miss alarm

Shadow mode tells you how you *would* have done, in a report you read later. This
step is the thing that wakes you up, and it is the single highest-value alarm in
the system.

Run a **liquidation watcher**: a component subscribed to the liquidation events of
every tracked protocol, decoding them as they land. For each one, check it against
what the engine did. The invariant is simple and absolute:

> Every liquidation on a tracked protocol corresponds to a position the engine
> had either flagged as liquidatable, or explicitly declined with a reason.
> Anything else is an alarm, immediately.

**Build it once, use it twice.** This is the same decoding as GUIDE 05 Step 2's
ground-truth extractor — identical ABIs, identical output type. Give it two
consumers: a batch one reading the parquet archive, and a streaming one reading
the tip. Writing it twice is how the replay and the live alarm end up disagreeing
about what a liquidation is.

**This watcher is also the recall gate.** GUIDE 05 Step 3 measures recall forward
rather than over a historical window, and this is the instrument that does the
measuring. Every observation it produces is one cell of the coverage matrix, so
it must persist them — protocol instance, collateral family, trigger class, the
block's realized volatility, and the outcome — not just alarm and discard. A
watcher that only alarms leaves the gate with nothing to clear against.

**Persist the raw event, not a summary.** GUIDE 05 Step 3a classifies every miss
by fork call at block *N−1*, and it cannot do that from a log line saying "miss at
block 21400312". Store the decoded event with all its fields plus the block hash,
so the classification is reproducible months later and does not depend on the
watcher having judged correctly in the moment.

It therefore has to run continuously from the moment detection works, including
the hours nobody is watching. `ORCHESTRATOR.md` §7.

**Keep it structurally independent of the engine.** It shares the event decoders
and nothing else — no state store, no health computation, no position index. A
watcher that asks the engine whether the engine knew about a position will agree
with the engine about everything, including the bugs. Its whole value is being a
second opinion built from a different input.

**Alarm on `NotTracked` and `HealthWrong` only.** `declined` outcomes go to the
daily digest, not to your phone — not because they are unimportant, but because
their diagnosis is statistical rather than immediate. One decline means nothing;
a reason climbing as a share of declines over a week is the signal, and that is a
digest question. GUIDE 05 Step 3 is where they get classified.

A caveat on that: if `declined` is *numerous*, do not treat it as background. On
the current Aave V3 Ethereum debt distribution most liquidations should be
addressable, so a large decline bucket is more likely an incomplete flash-source
index or a weak router than a fact about the market.

**What this actually catches.** Not the bugs you shipped with — replay finds
those. It catches the ones that *appear* in a system that was previously clean: a
proxy upgraded behind you, a new asset listed with a decoder you do not have, an
e-mode category added, a spoke reconfigured. Those arrive on someone else's
schedule, and the gap between "my detection broke" and "I noticed" is otherwise
however long it is until you next read a report.

This is also what makes leaving the box unattended defensible. "Am I still seeing
everything?" stops being a question you have to remember to ask.

## Step 4b — Sizing the stack to the operation

The stack below assumes a team watching screens. For a solo operator checking in
once a day, most of it is overhead. The **content** — the taxonomy, the trace
threading, the shadow comparison — is load-bearing and stays. The *infrastructure*
should be as small as you can stand:

| Need | Team scale | Solo scale |
|---|---|---|
| Trace + stage timings | ClickHouse | **structured JSONL on disk**, rotated daily |
| PnL ledger | ClickHouse / Postgres | **SQLite**, one file. You will do tens of liquidations a day, not millions of rows. |
| Metrics + dashboards | Prometheus + Grafana | Prometheus + Grafana locally is fine and light at this volume — or skip both |
| Alerting | PagerDuty | **push to your phone**: ntfy, a Telegram bot, or plain email |
| Daily review | a standing meeting | **one digest script** that reads yesterday's JSONL and prints the numbers |

The digest script is the important one, and it is an afternoon of work. It reads
the day's logs and emits: outcome taxonomy counts, `HealthWrong` and `NotTracked`
(must be zero), drift mismatch rate, realized vs. simulated PnL, and any halt
that fired. If that lands in your inbox each morning, daily monitoring is a
two-minute job rather than a dashboard-staring habit you will abandon.

**Halts fail safe, which is what makes daily monitoring viable.** A halt
that trips at 02:00 and waits for you until morning costs missed opportunities,
never money. That bias is deliberate (GUIDE 14) and it is exactly the right
tradeoff for a one-person operation.

## Step 5 — Dashboards that drive decisions

Four panels, not forty:

1. **Correctness** — `NotTracked` + `HealthWrong` counts (target: zero), drift
   detector mismatch rate by protocol
2. **Competitiveness** — distribution of `tick → would-have-been-included` vs.
   actual winner inclusion; the `Outbid` bid gap distribution
3. **Economics** — expected profit per opportunity, not win rate (see GUIDE 12);
   realized vs. simulated PnL
4. **Health** — node lag, oracle staleness, MEV-Share connection uptime, channel
   drop rate, gas spend rate, **hot-path LLC-miss and dTLB-miss rate** (below)

### Cache residency belongs on the health panel

A p99 latency regression has two causes with opposite fixes — the work got
slower, or the data stopped being resident — and a latency histogram alone cannot
tell them apart. One counter separates them.

Sample `LLC-load-misses` and `dTLB-load-misses` for the hot-path thread, via
`perf_event_open` on its own fd rather than a subprocess, and report **the first
recompute after a block arrives** separately from the rest.

That split is the whole point. The hot band is ~25 KB and fits in L1d, but it is
touched once per block, so the number that matters is whether it survived the
12-second gap while everything else on the box ran (GUIDE 16 Step 2). A high
first-touch miss rate with a low steady-state rate means **eviction**, and the fix
is placement — the hot path does not own its CCX. A miss rate that is high
throughout means the working set genuinely grew, which is a different
conversation.

Cost is a counter read per block, off the measured span. Read it *after* the
recompute, never between stages.

**Do not pre-warm the cache until this counter says to.** Touching the Hot+Warm
columns during the inter-block window is a real option — a few microseconds inside
a twelve-second gap, and you know when the next block is due. It is also
speculative optimization: if CCX isolation already keeps the band resident, a warm
pass costs work and buys nothing, and you will not know which world you are in
without the measurement. Measure, then decide. `TESTING.md` §2's rule applies to
optimizations as much as to tests — know where the number came from.

Alert on: any nonzero `HealthWrong`, drift mismatch above threshold, MEV-Share
disconnection > 60s, node lag > 2 blocks, and any proxy implementation change on
a tracked protocol.

---

## Acceptance criteria

- [ ] `TraceId` present in every stage span; a single trace can be followed
      end-to-end in the log store
- [ ] Every historical liquidation in a 30-day window classified into the
      taxonomy, with **zero** `NotTracked` and zero `HealthWrong`
- [ ] Liquidation watcher (Step 4a) running continuously since the node synced,
      structurally independent of the engine, sharing only the ABI decoders
- [ ] Watcher and GUIDE 05 ground-truth extractor are the same code with two
      consumers, not two implementations
- [ ] Watcher **persists** every observation with its coverage dimensions
      (protocol instance, collateral family, trigger class, block volatility),
      not just the alarming ones — GUIDE 05 Step 3 clears against this
- [ ] Miss alarm fires on `NotTracked` / `HealthWrong` only; `declined` outcomes
      route to the daily digest. Verified by injecting one of each.
- [ ] Shadow mode has run **≥ 2 weeks continuously** and accumulated
      **≥ 200 contested opportunities** — the count is the bar, the fortnight is
      the floor. Same principle as the recall gate (D40): a calendar window is a
      proxy for sample size, so state the sample size
- [ ] Shadow data shows the intended-submission timestamp preceding the actual
      winner's inclusion for **> 80% of opportunities** — the **Shadow gate**
- [ ] `WaitedTooLong` data collected for V4 with enough samples to fit a
      competitor-arrival distribution (target ≥ 200 observations)
- [ ] Four dashboards live; alerts fire in a drill
- [ ] **Hot-path LLC-miss and dTLB-miss rates collected**, with the first
      recompute after block arrival reported separately from steady state — the
      split that distinguishes eviction from working-set growth
- [ ] Counter collection adds nothing to the measured span (read after the
      recompute, via `perf_event_open` on the thread's own fd)
- [ ] Overhead of instrumentation < 5% of hot-path latency (measure with
      tracing disabled vs. enabled)
- [ ] **Every input in `PRE-FLIGHT.md` §4 is produced here**, against shadow
      data, before any capital is committed. The abandonment decision is manual
      (D13), which makes the *inputs* the deliverable — a human cannot decide
      well against a single blended profit number.
- [ ] The continue/stop decision is **recorded with its reasoning**, not just its
      outcome. Manual does not mean undocumented; the next review compares
      against this, rather than re-arguing from scratch.

## Failure modes

| Symptom | Cause |
|---|---|
| Cannot tell why you're losing | Single win-rate metric instead of the taxonomy |
| Instrumentation slows the hot path | Synchronous writes, or string formatting in spans — use structured fields and an async sink |
| Shadow says you'd win; live says otherwise | Shadow didn't model the bid, or didn't account for auction competition arriving *because* you bid |
| No `WaitedTooLong` data when GUIDE 12 needs it | Timing decision not logged during shadow; retrofit costs weeks of calendar time |

## Handoff

GUIDE 10 (contracts) and GUIDE 11 (simulation) can proceed while shadow mode
accumulates data. That is deliberate: those are multi-week builds and the shadow
clock runs in parallel. Do **not** proceed to GUIDE 13 until the 80% gate is met.
