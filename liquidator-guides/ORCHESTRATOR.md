# Orchestrator — How Agent Sessions Drive This Build

**Every agent session starts here.** Read this file, then `STATE.md`, then
`WORK-PACKAGES.md`, then apply §4 to select work. A first session also reads the
short list in §11 before starting anything.

The guides describe *what* to build. `WORK-PACKAGES.md` cuts the guides into
work packages (WPs) with a dependency graph that has no cycles at the crate
level. This file decides *what happens next* and *who does it*. `STATE.md` is
the only thing that survives between sessions.

---

## 1. Why work packages and not guides

The guides are organised by layer and their header tables form a clean DAG. The
**prose** does not: guide 01 names types that guide 02 defines, guide 03 calls a
halt API that guide 14 defines, guide 04's gate needs prices that guide 06
defines, guide 09's shadow run needs the router that guide 12 defines while
guide 12 lists 09 as a prerequisite, and guide 10's acceptance needs the encoder
that guide 13 defines. An agent dispatched "GUIDE 03" discovers mid-session that
it needs a type from GUIDE 14 and either stalls or invents one.

Work packages fix this with two moves, both recorded as decisions in `STATE.md`:

1. **Cross-cutting types and traits move down to the crate that has no
   dependencies** (`liq-types`, WP 00C) or to the trait crate (`liq-protocol`,
   WP 01). `PriceVector`, `Band`, `HaltScope`/`HaltReason`/`HaltSink`,
   `LogSubscriber`, `Submitter`, `TraceId` live in `liq-types`. `StateWriter`,
   `RouteCache`, `PositionRef`, `MarketRow`, `PositionExtraRepr`, `DirtySet`
   live in `liq-protocol`. Every later crate depends downward only. (D46)
2. **Guides whose steps have different prerequisites are split.** 06 → 06A
   (canonical prices, before 04's gate) and 06B/C (SVR client, aggregator sim,
   after 04). 09 → 09A (build) and 09B (the shadow run, a calendar item). 12 →
   12A (everything that needs no production data) and 12B (fits that do). 15 →
   15A (build + drift, can start once 04A's pattern exists) and 15D (enable,
   after 13/14). 16 → 16-0 (day-0 node), 16A (thread map, with 03), 16B–D
   (tuning, after there is something to tune). The watcher (09 Step 4a) becomes
   its own WP that starts the day the node is synced. (D47)

The guides remain the specification of *what* each WP builds. When a WP row and a
guide disagree on a signature or a type's home, **the WP row wins and the guide is
edited in the same session**, with a `CHANGES.md` line. Guides remain
authoritative on acceptance criteria, rounding, and everything else.

---

## 2. The session protocol

Two kinds of session. A **coordinator** session runs the dispatch loop and may
drive several **builder** sessions in parallel as subagents. A builder session
does exactly one WP.

```
COORDINATOR
1. Read ORCHESTRATOR.md, STATE.md, WORK-PACKAGES.md
2. Expire stale claims (§4.2)
3. Compute the eligible set (§4.1); pick up to LANES of them (§4.3)
4. For each: launch a builder subagent with the WP's model (§5) and the
   builder brief (§6). Builders run in parallel on disjoint owned paths
5. When a builder returns: launch its reviewer subagent (§7)
6. On review PASS: mark the WP done with evidence (§8); on REWORK: back to
   the same builder with the review; on FAIL: blocked, needs_human
7. Repeat from 3 until nothing is eligible and no lane is busy
8. Write what is being waited on. STOP.

BUILDER
1. Read the brief, the WP row, the named guide sections in full,
   RUST-CONVENTIONS.md §2 §3 §5 §6, TESTING.md §2 §3 (the row for this
   layer) §7, and §9 of this file
2. Do the work inside the owned paths only
3. Run the WP's acceptance criteria — against TESTING.md, not just until green
4. Run the PonyTail-HFT post-pass (§9) on your own diff
5. Return: commit SHA, cargo test / forge test summary, each acceptance line
   ticked with HOW, the list of anything you left out and why
6. STOP. Do not start another WP
```

**One WP per builder; a WP may have more than one builder.** A builder that
finishes early spends the remaining context running the acceptance criteria
harder and the mutation checklist — it does not pick up a second WP. A large WP
(04A, 10A, 12A-1, 12A-2) may be handed to a single builder for the whole thing —
the harness compresses context, so running out of it is not the expected failure
— or the coordinator may split it into **sub-briefs with disjoint owned paths
inside the WP's `Owns`** and run them as parallel builders. The WP is still
reviewed and marked `done` as one unit, with one evidence block. What is never
split: one file between two builders, or one acceptance line between two
builders.

**Never mark a WP done without evidence.** §8 defines what counts.

---

## 3. Tracks, gates, and what a gate actually blocks

Three tracks run concurrently. Two are wall-clock and start before code.

| Track | What | Who | Starts |
|---|---|---|---|
| **A — Calendar** | OS + prune profile → `reth download` → node up → watcher live → drift week(s) → shadow run → supervised window | human starts, agents check | **hardware ordered day 0; node up before 04A lands** (≈ week 2–3) |
| **B — Code** | the WPs in `WORK-PACKAGES.md` | agent sessions | **day 0** — nothing in B needs the node until 03B's acceptance |
| **C — Data** | registry discovery → verification → prune filter → archive extraction | agent sessions | **day 0, first**; runs on public RPCs |

**Cold-start order:** C1 registry → C2 verify → C3 prune filter → **H1 human
sign-off** → A1 OS → A2 download+node → A4 watcher. Track B starts on day 0 in
parallel (00A, 00B, 00C, 01, 02A do not touch a node). The node does **not**
have to exist on day 0: `reth download --with-receipts-since <earliest
deployment>` backfills the archive, so deferring it loses no data — it only
delays the calendar, because 04C's drift week, 09B's shadow run and 13B's
supervised window cannot start before the node is up. The binding rule is
therefore **node live before 04A is ready for drift**, and the only true day-0
obligation on Track A is ordering hardware with whatever lead time it has.

An RPC substitutes for the node in Track C, in `anvil --fork-url` test
harnesses, and nowhere else. **Never build an RPC-fed ingest path into the bot
to bridge the gap** — it is a second code path that never ships, it violates
D05, and a drift week measured against it validates nothing.

### 3.1 Gates are consumption points, not start barriers

The previous rule — "every upstream gate must be `passed` before a guide starts"
— blocked all of 06–17 on the Recall gate, which accumulates over weeks. That was
wrong: building code during a calendar wait is the whole point of the schedule.
**A gate blocks the thing whose correctness depends on it, and nothing else:**

| Gate | Measured by | Passing is required before |
|---|---|---|
| **Correctness** (per adapter) | 7-day drift, < 1 bp, ≥ 10k positions, V3 and V4 separately | that adapter enters the shadow run (09B) or is enabled (15D). A failure reopens 04A/04B at top priority |
| **Recall** | ≥ 50 in-scope observations, in-scope miss rate < 30% aggregate **and per row**, 100 % of misses classified, `declined` classified | the first live submission (13B / H4). Any SYSTEM decline > 20 % is a defect routed to 07B or 12A first |
| **Shadow** | ≥ 2 weeks **and** ≥ 200 contested opportunities, intended submission precedes winner > 80 % | **WP 13A may not start**. Execution code built on an ungated detector is the most expensive mistake available here |
| **Supervised** | GUIDE 17 Step 0's six rows, ≥ 10 sessions, ≥ 2 weeks, ≥ 3 volatile | unattended running (H6) |

Everything in §4 of the previous orchestrator about *what* each gate requires
still holds verbatim: the recall denominator is in-scope; the classifier may not
touch our state; the coverage matrix qualifies the number; `declined` is a
defect queue, not reassurance; a gate is `open` until a number and a date range
are in `STATE.md`. Those paragraphs now live in GUIDE 05 Step 3/3a and
`TESTING.md` §6, and `STATE.md`'s gate rows carry the thresholds.

**Correctness is per adapter and runs concurrently.** Adapter #2's drift week
runs while #1's does. GUIDE 15 Step 4 says so; the WP graph makes it possible.

### 3.2 The prune profile is still a one-way door

GUIDE 16 Step 0b. The `receipts_log_filter` list is generated from the committed
registry (C3), a human signs it (H1), and only then does the download start.
`before = 0` in that file means *keep everything for this address* — it is the
safe direction and costs nothing, because no receipt before deployment contains
that address's logs. Do not spend a day hunting 2,600 deployment blocks before
sync; spend it checking that every roster protocol, oracle proxy **and its
underlying aggregator**, every receipt token, the Uniswap V4 `PoolManager`, the
Morpho singleton, Sky DSS Flash, and the top exit pools are present. A missing
line is the unrecoverable case; a `before = 0` line is not.

---

## 4. Dispatch

### 4.1 Eligibility

A WP is eligible when **all** hold:

1. Status `todo` in `STATE.md`
2. Every entry in its `Depends on` column is `done`
3. Every gate named in its `Gate` column is `passed`
4. Every calendar/track item named in its row is at least `started`
5. No `in_progress` WP owns an overlapping path (§4.4)

`STATE.md` may disagree with `WORK-PACKAGES.md`; the WP file wins and `STATE.md`
is fixed in the session.

### 4.2 Claims

A coordinator marks a WP `in_progress` with `claimed_by: <agent-id> <ISO ts>`
before launching the builder. A claim older than **6 hours** with no commit
touching its owned paths expires: status back to `todo`, a session-log line says
so. This is what makes parallel sessions safe against a builder that died.

### 4.3 Ordering among eligible WPs

1. Anything that **starts a clock** — a Track A/C row, `04C` (start drift),
   `09B` (start shadow), `A4` (start watcher)
2. Longest remaining path to `13A` (critical path), computed from
   `WORK-PACKAGES.md` §8
3. Lowest WP id

`LANES` defaults to **3** concurrent builders. Raise it only when the eligible
set has that many non-overlapping WPs, or when a large WP is being run as
parallel sub-briefs (§2) — the sub-briefs count as lanes.

### 4.4 Path ownership

Each WP row lists the paths it may create or modify. Two in-flight WPs never
share a path. A builder that needs a change outside its owned paths writes the
change as a one-paragraph request in its return note; the coordinator applies
it as a tiny commit or routes it to the owning WP. This is the rule that keeps
three parallel lanes from producing merge conflicts in `liq-types`.

### 4.5 If nothing is eligible

The build is gate-blocked or waiting on wall clock. Record what is being waited
on and stop. **Do not invent work.** The most common orchestration failure is an
agent finding something to do because the real next step was "wait four days".

---

## 5. Model routing

Three tiers, chosen per WP from what a wrong answer costs and how far the guide
already specifies the answer. The assignment is in each WP row; the rubric below
is for WPs added later (new adapters, new flash sources).

| Tier | Model | Use for | Reviewer |
|---|---|---|---|
| **T1** | **Claude Fable 5.1** (`claude-fable-5-1-thinking-high`) | Anything whose bug is **silent**: rounding and health math, the state store's undo ring, the miss classifier, the plan encoder's test generator, callback authentication, the water-fill and profit model, candidate gating, nonce/async boundaries, the ExEx forwarder and panic containment, trait design | **Claude Opus 5** (`claude-opus-5-thinking-high`), a different model so the review is independent |
| **T2** | **Grok 4.6** (`cursor-grok-4.6-high`) | Implementing a **specified** interface against a documented external surface: provider ABIs, SSE and websocket clients, revm wiring, fixtures, harnesses, adapters that follow an established pattern, ops scripts with a machine check | **Fable 5.1** for anything on Path A/B/C; **Grok 4.6** (fresh session) otherwise |
| **T3** | **Composer 2.5** (`composer-2.5`) | Fast, low-judgement, machine-verified: workspace and CI skeleton, lint config, TOML schemas, generation scripts whose output is re-derived independently, digest/report scripts, CSV writers, docs artifacts | **Claude Opus 5** (`claude-opus-5-thinking-high`) — `REVIEW_T3`, D50. Sonnet was the first choice; no Sonnet slug exists in this workspace |

The T3 review is a polish pass, not a second build: fix what Composer missed or
did poorly, run the acceptance lines, do not redesign. If the reviewer finds it
is rewriting more than it is fixing, the WP was mis-tiered — return `FAIL
<re-tier>` and the coordinator re-dispatches it as T2.

**Routing rules that override the table:**

- A T3 build that touches any path listed in a T1 WP's `Owns` is not a T3 build.
  Re-route.
- Any WP whose acceptance includes a `TESTING.md` §4 mutation item is reviewed
  by T1 regardless of who built it.
- A reviewer is never the same model instance as the builder. Same model,
  fresh session, is acceptable only for T2→T2.
- The Executor (10A) and the plan encoder (10B) additionally get **human /
  external review** (H3). No model review substitutes for it.

---

## 6. The builder brief

The coordinator hands each builder exactly this, filled in:

```
WP:            <id> <name>
Model:         <slug>
Guide §:       <file, steps>            — read these in full
Also read:     RUST-CONVENTIONS.md §2 §3 §5 §6; TESTING.md §2, §3 <row>, §7;
               ORCHESTRATOR.md §9 (PonyTail-HFT)
Owns:          <paths>                  — write nowhere else
Depends on:    <ids, all done>          — read their return notes in STATE.md
Deliver:       <the acceptance lines from the WP row>
Deferred:      <criteria this WP does NOT own, and which WP does>
Decisions:     D01–D54 in STATE.md are given. Diverging is a decision-log
               entry flagged needs_human, not an edit.
Never:         §10 of ORCHESTRATOR.md
Return:        commit SHA; test summary; each acceptance line ticked with how;
               allocations/latency numbers where the row asks; PonyTail
               post-pass log (§9.3); anything left out and why
```

---

## 7. Review protocol

Every WP is reviewed by a different agent before it is `done`. The reviewer
receives the builder's return note and the same brief, and produces one of
`PASS` / `REWORK <list>` / `FAIL <reason>`.

The reviewer **must**:

1. Run the tests; do not trust the summary
2. Apply `TESTING.md` §7 to every test in the diff — every assertion names its
   oracle; none derives from the code under test; at least one negative
   assertion; generators vary each dimension independently and are biased to
   the boundary
3. Apply the `TESTING.md` §4 mutations listed in the WP row: break the code,
   confirm red, restore
4. Check every acceptance line was ticked *with how*, and spot-check two
5. Run the PonyTail-HFT review pass (§9.4) and list every item judged
   unnecessary, removed or kept, with the reason
6. Check nothing outside `Owns` changed
7. For Path A/B/C code: confirm no `.await`, no allocation, no lock, no
   `LazyLock`/`OnceLock`, no `unwrap`, no float, per `RUST-CONVENTIONS.md`

A `REWORK` goes back to the original builder once. A second `REWORK` on the same
WP escalates the builder one tier.

---

## 8. Evidence format

| Task type | Evidence |
|---|---|
| Code WP | Commit SHA + `cargo test`/`forge test` summary + reviewer verdict + the acceptance checklist, each line ticked with how |
| Gate | The metric, its value, the sample size, the date range |
| Calendar item | Start timestamp; on completion the finish timestamp |
| Blocked | What is blocking, what was tried, what would unblock it |

`done` with an empty evidence field is treated as `todo` by the next session.

---

## 9. PonyTail-HFT — the minimal-code discipline, adapted for this system

PonyTail's premise: *write only what the task needs; before writing, climb a
ladder that stops at the first rung that already solves it; after writing, ask
of every piece whether it is necessary and whether it can be smaller.* It
explicitly never cuts validation, error handling or security. Here it also never
cuts hot-path performance or financial exactness. Fewer lines is not the goal.
**The smallest correct, fastest implementation is.**

### 9.1 Before writing — the ladder

Stop at the first rung that solves it.

0. **Is it in this WP's scope?** If no acceptance line or guide step in the
   brief requires it, do not write it. Put it in the return note as a
   suggestion.
1. **Does it need to exist at all?** Name the acceptance criterion, invariant
   or caller. No name → no code.
2. **Is it already in the workspace?** Search `crates/` before writing a
   helper. One `mul_div`, one `SafeTransfer`, one liquidation-event decoder.
3. **Does `std`, `alloy`, `revm`, `reth`, or a pinned dependency do it?**
   `DEPENDENCIES.md` §1: buy everything that touches the chain. `sol!` over a
   hand-written decoder; `arc-swap` over your own RCU; `rtrb` over your own
   ring; `bytemuck` over a union; `Dex-Math-Core-rs` over your own tick math.
4. **Write the smallest implementation that meets the guide's signature and
   RUST-CONVENTIONS.md.** No speculative parameters, no config knob nobody
   reads, no trait with one implementor unless a guide names it for dependency
   inversion (those are required — `StateWriter`, `RouteCache`, `HaltSink`,
   `LogSubscriber`, `Submitter`).

### 9.2 Off the chopping block — never removed to save lines

- A named rounding direction, a `checked_*`, an `overflow-checks`, a `Result`
- An `UndoOp` push, a `FinishedHeight` ordering, a `catch_unwind`
- A callback `msg.sender`/initiator check, a `SafeTransfer`, an allowlist check,
  the `minProfit` guard, the registry boot assertion
- A bounds-checked access, a `None` arm, a counted drop on a full channel
- Pre-allocation, `SmallVec`/`ArrayVec` bounds, the arena, the `AssetMask`
  iteration, column-major layout, thread naming and pinning, `&'static Shared`
- Any test, any assertion, any fixture, any negative test
- Any acceptance criterion, any `STATE.md` decision, any `TESTING.md` oracle
- Structured logging fields that `GUIDE 09`'s taxonomy or `GUIDE 05`'s
  classifier reads

Replacing an optimised structure with a shorter, slower one is forbidden by the
project ethos regardless of line count.

### 9.3 After writing — the builder's post-pass

For every new `fn`, `struct`, `enum`, `impl`, module and dependency in the diff:

| Question | If yes |
|---|---|
| Is it unreferenced, or referenced only by its own test? | Delete it |
| Does a library already pinned in the workspace do this? | Use the library |
| Can it be expressed in fewer lines with **byte-identical** behaviour and equal or better p99.9 on the WP's benchmark? | Do it |
| Is it a wrapper, adapter or "utils" that exists to be tidy? | Inline it |
| Is it a parameter, flag or variant nothing reads yet? | Remove it and note it as a suggestion |

Log the pass in the return note: `PonyTail: removed <n> items (<list>), kept <m>
flagged items (<list, reason>)`. An empty log on a non-trivial diff is a review
finding.

**Acceptance of a reduction:** all tests unchanged and green; clippy clean; the
WP's stated latency/allocation numbers unchanged or better; acceptance lines
unchanged. Removing a test is never a reduction.

### 9.4 The reviewer's pass

The reviewer repeats §9.3 on the diff independently and lists disagreements.
Anything the reviewer removes must pass the same acceptance rule. The reviewer
also asks the one question the builder cannot: *does this WP now contain
something a later WP will duplicate?* If so, the return note names the later WP
so its brief says "already exists in `<path>`".

---

## 10. Things an agent must never do

- **Skip or self-certify a gate.** §3.1.
- **Mark a protocol adapter live without its own 7-day drift run.** GUIDE 15
  Step 3 applies to adapter #12 as to #1.
- **Change `liq-engine` to accommodate an adapter.** The trait is wrong. Record
  as blocked and route to WP 01.
- **Relax an acceptance criterion to make it pass.** Decision-log entry.
- **Fix a compile error with something that costs hot-path latency.**
  `RUST-CONVENTIONS.md` §3.3. If a fix lands on Path A/B/C, say what it cost.
- **Write a test whose expected value came from the code under test.**
  `TESTING.md` §2.
- **Substitute a synthetic test for a gate.** `TESTING.md` §6.
- **Enable a new protocol, flash source or trigger in production config** as
  part of a build WP. Widening is its own session (15D) with its own evidence.
- **Deploy during volatility.** GUIDE 17 Step 1b.
- **Start the Reth sync before H1.** §3.2.
- **Window observation to watched hours.** GUIDE 17 Step 0.
- **Write outside your owned paths.** §4.4.
- **Add a crate, a dependency, or a trait** not named in `WORK-PACKAGES.md`
  without a decision-log entry.
- **Build a fallback submission path**, flag-gated or otherwise (D19).
- **Add an Executor adapter for a new liquidation ABI without a redeploy WP**
  (D48). The contract is immutable; a Solidity change is a new address.

---

## 11. Reading order for a first session

1. `PRE-FLIGHT.md` §6–§7 — scope by calendar, parameterise protocol constants
2. `RUST-CONVENTIONS.md` — how everything is written
3. `TESTING.md` — what tests must prove
4. `DEPENDENCIES.md` — bought vs. built
5. `WORK-PACKAGES.md` — the graph
6. `INDEX.md` — the map
7. Then Track C, Track A, and only then WP 00A

Later sessions: `STATE.md` → `WORK-PACKAGES.md` → §4. Re-read
`RUST-CONVENTIONS.md` §3 and §5 before any code that crosses a thread boundary.

---

## 12. Human checkpoints

| # | When | Decides |
|---|---|---|
| **H1** | after C3, before A2 | Prune profile and address list signed off. Irreversible |
| **H2** | after C4 / market study, any time before 13B | Continue / stop, against `PRE-FLIGHT.md` §4 inputs. Manual (D13); record the reasoning. **Never blocks Track B** |
| **H3** | after 10A–10C | Executor independent/external review; mainnet deploy; address into config |
| **H4** | Shadow + Recall gates passed, 13A reviewed | First live submission — opens the supervised window (13B) |
| **H5** | each 15D | Widening production config: enable an adapter, flash source or trigger |
| **H6** | Supervised gate passed | Unattended running |
| **H7** | any `needs_human` entry | Design divergence, failing gate, disputed criterion, or an unset parameter (D11, D12, D27, D30, D34, D35, D50) |

Everything else runs unattended.
