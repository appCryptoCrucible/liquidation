# What Changed — Reshape for Flashloan-Only, Full Universe, Baremetal

Three constraints were added after the first guide set. This records what moved
and why, so the delta is auditable rather than buried in a rewrite.

| Constraint | Effect |
|---|---|
| **Flashloan-only funding** (Aave + Uniswap required) | New crate `liq-flash` + GUIDE 07. Eligibility becomes a universe filter. Inventory removed from risk, router and contracts. |
| **All Ethereum lending protocols** | GUIDE 14 "second adapter" → GUIDE 15 "coverage program". Trait design pressure increases. |
| **Baremetal server, significant compute** | New GUIDE 16. Headroom spent on parallelism and exhaustiveness, not a lazy hot path. |

---

## Renumbering

Two guides were inserted, so everything after shifted:

| Old | New | Guide |
|---|---|---|
| — | **07** | **Flashloan Sources & Eligibility** (new) |
| 07 | 08 | Health Engine |
| 08 | 09 | Observability |
| 09 | 10 | Contracts |
| 10 | 11 | Simulation |
| 11 | 12 | Router & Bidding |
| 12 | 13 | Execution |
| 13 | 14 | Risk & Treasury |
| 14 | 15 | Protocol Coverage Program |
| — | **16** | **Baremetal Systems Engineering** (new) |
| 15 | 17 | Operations |

Guides 00–06 keep their numbers. All cross-references were updated.

---

## Substantive changes by guide

**GUIDE 01 — Protocol Trait (rewritten).**
`Quote` now carries `repay_options` and `seize_options` as *sets*, because a
multi-debt position may only be fundable on its second-choice debt leg —
surfacing one repay asset silently discards opportunities. Funding moved out of
the trait entirely: `encode()` takes a `FlashRoute` produced by GUIDE 07, so
provider selection is not reimplemented per adapter. Added
`HealthState::SoftLiquidating` so protocols like Curve/LLAMMA are declined
deliberately rather than discovered mid-build. Step 1's variance table expanded
from five protocols to eight.

**GUIDE 07 — Flashloan Sources & Eligibility (new).**
Five sources behind a `FlashSource` trait: Aave (5 bps, deepest), Uniswap V3
(pool fee tier), Uniswap V4 (**fee-free**, singleton depth), Morpho Blue (free),
Balancer (free). A live `FlashIndex` tracks availability per asset per source,
updated from the ExEx log stream, never polled. The eligibility filter gates
which positions are targets at all, checking every repay option. Includes source
ranking by effective cost, a safety haircut tuned from observed liquidity
failures, a fallback chain, and split-source nesting for oversized positions.

**GUIDE 08 — Health Engine.**
Added `Band::Unfundable`. Positions with unfundable debt are tracked cheaply and
never promoted, but **never deleted** — eligibility flips back when liquidity
returns, and re-backfilling costs far more than carrying them.

**GUIDE 10 — Contracts (rewritten).**
Restructured around a `callbacks × adapters` matrix — five callback shapes times
a dozen protocol families is sixty combinations, so one dispatching executor with
combinatorially *generated* fork tests, not sixty contracts. Uniswap V4's
`unlockCallback` singled out as the one that will surprise the implementer
(deltas must net to zero before `unlock()` returns). Inventory custody removed.
New critical invariant: **every callback validates `msg.sender` and the
initiator** — a callback that doesn't is a free-money function for anyone who
finds it. Added the surplus-borrow case, which is sharper now: if you flash for
the requested repay and V4 clamps to less, you hold surplus debt you must still
repay with fee.

**GUIDE 11 — Simulation.**
Added bounded parallel simulation of plan *variants* (alternative flash sources,
repay×seize combinations, two V4 firing points) on dedicated cores. Explicitly
distinguished from search — it is parallelism over a precomputed variant set.

**GUIDE 12 — Router & Bidding (rewritten).**
Sizing now has four ceilings, and the binding one is usually flash depth or route
depth rather than the protocol cap. Added joint optimization over
repay × seize × source — source selection and repay-leg selection must be decided
together, since the cheapest source depends on which debt asset you pick. Added
partial-size fallback (a 20% liquidation at full bonus is real money). Flash fee
is now a permanent line item in the profit model, which creates a
`min_viable_notional` per protocol/asset. Bid model gains a new lever: sourcing
at 0 bps instead of 5 bps puts a competitor $250 behind on a $500k liquidation
before either party bids.

**GUIDE 14 — Risk & Treasury (rewritten).**
Inventory, capital allocation and warehousing removed — a genuinely smaller
attack surface. Replaced by **flash liquidity risk**: provider concentration
monitoring, depth alerts, haircut auto-tuning, and per-provider concurrent caps
so two of your own bundles don't contend for the same pool in one block. Treasury
is now gas balance plus unswept profit, nothing else. Proxy watcher extended to
cover flash providers, not just lending protocols.

**GUIDE 15 — Protocol Coverage Program (rewritten from "Second Adapter").**
Now a program with a prioritized universe table (TVL-sized), a per-adapter
checklist, expected effort per adapter as an architecture health metric, scale
effects at a dozen protocols, and an explicit `DECLINED.md`. Prioritization is by
liquidatable debt **and debt-asset flashloanability** — a high-TVL protocol whose
debt isn't flashloanable is worth less to you than its size suggests.

**GUIDE 16 — Baremetal (new).**
NUMA pinning, core isolation, hugepages, full pre-allocation, zero hot-path
allocation verified end-to-end. A section on **spending headroom deliberately**:
parallel variant simulation, a wider warm route cache, continuous full-universe
revalidation of band membership, deeper route search, more concurrent protocols —
and an explicit warning that headroom multiplies an efficient design rather than
rescuing an inefficient one. Notes that kernel bypass is almost certainly
unnecessary under SVR, and that the single-box failure posture must be decided
explicitly.

**GUIDE 17 — Operations.**
References the GUIDE 16 failure-posture decision instead of assuming a warm
spare. Benchmark comparison against baseline added as a pre-deploy gate alongside
the replay run.

**GUIDE 03 — Ingest.**
The union log filter now includes `FlashSource::subscriptions()`, so flash
liquidity rides the same stream.

---

## Addendum — OS choice and the ExEx single-process consequence

**Recommendation: Ubuntu 26.04 LTS** (kernel 7.0, Apr 2026, supported to 2031).
Debian 13 ships 6.12 LTS; RHEL 10 ships 6.12. A full kernel major version is the
deciding factor for NVMe, io_uring, NUMA and scheduler behaviour on modern server
hardware, and Reth is developed and released against Ubuntu. Take RHEL's tuning
framework anyway — TuneD is packaged for Ubuntu and Debian. Details, comparison
table and the list of default services to disable are in **GUIDE 16 Step 0**.

The larger finding, surfaced by the question: **with the ExEx architecture the
bot is compiled into the Reth binary and runs inside the Reth process.** Not
"same box" — same process. Three consequences now documented:

- **GUIDE 16 Step 1 / Step 2** — pinning is thread-level, not process-level. You
  cannot `taskset` your hot path away from the node's workers; threads must be
  named and pinned from inside the binary.
- **GUIDE 17 Step 1b (new)** — a bot deploy is a node restart, so deploys are
  batched, run behind the spare, and never done during volatility. Config that
  changes often (bid parameters) must be hot-reloadable so it doesn't need a
  rebuild.
- **GUIDE 03** — a panic in an adapter can take the node down. Contain faults at
  the ExEx boundary: an adapter fault degrades to a protocol halt, never a
  process abort.

Also added: **GUIDE 00** CI now builds on the production OS image (glibc parity),
and **GUIDE 17** gains a scheduled OS-patching cadence — automatic upgrades are
disabled on the box, which makes security patching a calendar item rather than
the distro's job.

---

## Addendum — Rust conventions made explicit

Added **`RUST-CONVENTIONS.md`**, the canonical reference every guide now defers
to, plus a **Rust: concurrency & safety** section in each guide where the
primitive choice is layer-specific (00, 02, 03, 06, 07, 08, 11, 12, 13, 16).

**The organizing claim** (§1): the hot path is single-threaded, synchronous, and
owns its data exclusively. `StateStore` has exactly one writer and every hot-path
stage runs on that writer's thread, so it needs no synchronization at all —
`&mut StateStore` is the entire concurrency story for the fastest part of the
system. `Arc<Mutex<StateStore>>` is the most likely wrong turn in the project and
now has a CI grep-assert against it.

**Channel selection is now a table, applied per layer:**

| Layer | Primitive | Why |
|---|---|---|
| Hot stage → hot stage | function call | same thread; a channel is pure overhead |
| Mempool → overlay (GUIDE 03) | `rtrb` SPSC, drop-oldest | one producer; stale pending tx is worthless |
| CEX feeds → fusion (GUIDE 06) | `crossbeam` bounded MPSC | many producers |
| Fusion → hot path (GUIDE 06) | **`triple_buffer`** | engine wants the latest *complete* vector, not a queue of superseded ones |
| Flash index, route cache (07, 12) | **`arc-swap`** | one writer, many readers, readers never block |
| Sim dispatch/return (GUIDE 11) | `rtrb` SPSC per worker | known peer; MPSC would add contention for nothing |
| Nonce allocator (GUIDE 13) | **`parking_lot::Mutex`** | the one place a lock is correct — short section, low contention, async path |

**Panic policy.** `panic = "unwind"` is required in the release profile
(`abort` would make ExEx containment impossible), deny-level
`unwrap_used`/`expect_used`/`panic`/`indexing_slicing` in every hot-path crate,
and `catch_unwind` at the adapter dispatch converting a panic into a protocol
halt. With a dozen adapters planned, an unwrap in adapter #9 must not take down
the Ethereum node.

**`overflow-checks = true` in release**, deliberately. A silent wraparound in
health math produces a confidently wrong answer rather than a crash — the worst
failure mode this system has.

**Floats are banned in money math** (`clippy::float_arithmetic` denied) with one
scoped exception: the bid model's statistics in GUIDE 12, where the result is
converted back to an integer `U256`, rounding down, before it touches a
transaction.

**`await_holding_lock` is deny-level**, backed by using `parking_lot` (whose
guards are `!Send`) rather than `tokio::sync::Mutex`. Two mechanisms for one
rule, because that one causes real outages.

**`#![forbid(unsafe_code)]` everywhere**, currently with zero exceptions —
`bytemuck` covers the columnar layout's `Pod`/`Zeroable` needs safely, and
`get_unchecked` is explicitly rejected as not worth it.

Also added to GUIDE 16: a thread naming convention, a startup-time core map with
fail-closed assertion, the thread placement table, and `mimalloc` as the global
allocator.

---

## Addendum — the executor contract, custody and venue routing

Added **`Executor.sol`** (reference implementation) and **`PLAN-ENCODING.md`**
(the Solidity ⇄ Rust wire format), both owned by GUIDE 10.

**Profit custody — a third option.** Sweeping every win costs ~30k gas on a
350–600k gas liquidation; accumulating leaves a standing balance. The design
now decides **per liquidation, off-chain**: the Rust side tracks the executor's
balance and sets a `SWEEP` flag in the plan when it crosses a threshold. Not
sweeping costs zero extra gas (the flag is already in calldata); sweeping costs
~30k amortized over N wins. What makes accumulation safe is not the threshold
but that `PROFIT_SINK` is immutable and `OPERATOR` can only call `execute()` —
a compromised hot key can waste gas and nothing else. `sweep()` is permissionless
for the same reason.

**Approvals are mostly unnecessary**, which corrects the earlier guidance.
Uniswap V3, V4 and Balancer settle by transfer inside the callback — no approval
in those paths at all. Only Aave and Morpho pull, and an exact-amount approval
self-zeroes when consumed. GUIDE 10 Step 5 now has the per-path table.

**Calldata packing is worth less than previously implied.** EIP-7623's floor
applies only to transactions that are calldata-heavy relative to their
computation; a liquidation's 350–600k gas of EVM work keeps it on the standard
4/16 schedule. Packing saves ~1% of transaction gas — do it, but do not
hand-write fragile assembly for it.

**Bundles are for ordering, not payment** (GUIDE 13). A backrun must land
immediately after a specific transaction; a standalone private transaction,
however high its priority fee, can land before it, after a competitor, or a
block late — the on-chain guard reverts in the first two cases and the position
is gone in the third. `InterestDrift` is the one trigger with no transaction to
sit behind, so it legitimately ships as a plain private transaction with a
modest priority fee. For SVR-triggered Aave liquidations the auction is the
access mechanism, not an optimization.

---

## Addendum — routing, gas and the swap leg

The conversation that produced this section was mostly about the *third* leg of
the cycle, which earlier drafts had treated as a detail. It is not.

**Price impact is usually the largest single cost.** Gas is $2–20 at calm base
fees, flash fee 0–5 bps, swap fee 1–30 bps — and price impact runs 1–5 bps on a
small exit into deep liquidity but 50–300 bps on a large exit into mid liquidity.
Everything below follows from that.

**Pool swap fees were missing from the profit model.** The bid formula being
repeated — `net = gross − gas − flash_fee` — omitted both swap costs. On a 30 bps
route that is the *largest* omitted term. Net is now computed from the exact
quote, which internalizes fee and impact together, and fee tier became a route
*selection* input rather than something discovered afterwards.

**Split routing across N pools** (GUIDE 12 Step 4). Optimal allocation equalizes
marginal `(fee + impact)` — water-filling, not proportional or equal splitting.
Pool-set selection is discrete and gas-bounded: add pools greedily, stop when the
next pool's ~80–120k gas exceeds the impact it saves. Settles at 2–4 pools.

**The allocation solve is per-AMM, not generic.** `g(λ)` is monotone but only
C⁰ — it kinks at every tick boundary — so Newton diverges near kinks and plain
bisection needs ~40 iterations against a ~300 µs routing budget. Better: the
kinks are at *known* locations, so binary search the sorted breakpoint list
(O(log K), exact) and solve closed-form within the smooth interval. Brent, with
its **bisection** fallback — not golden section, which belongs to Brent's
*minimization* method — is the generic fallback where structure is unavailable.
And λ never needs wei precision: solve loosely, assign the residual to the
best-marginal-rate pool.

**`Dex-Math-Core-rs` is a first-class dependency** (`DEPENDENCIES.md` §1). Not
convenience — split optimization on concentrated liquidity is *impossible*
without exact tick-crossing math, and its Curve/Balancer coverage matters because
seized collateral is frequently a stable or LST whose best exit is not Uniswap.
An earlier draft claimed revm made a swap-math library unnecessary; that
conflated verification with optimization. revm gates the plan you chose; it
cannot evaluate the space you chose from.

**The gas oracle became a component** (GUIDE 12). Next block's base fee is
*computable exactly* from the current block under EIP-1559 — a pure function
bounded ±12.5% — so anyone treating it as an estimate carries an error term you
do not have to. Priority fee is the estimated part and only matters on the
standalone private-relay path.

**Bid rule: costs-first, floor and percentage.**
`bid = net − max(floor, 0.01 × net)`. Computing net *after every cost* makes the
remainder positive by construction, which the naive `α × gross` form does not.
The floor is an error budget and a concurrency filter, not loss prevention — the
`minProfit` guard already makes thin margins cost opportunities rather than
money.

**The executor gained split-swap execution** (`Executor.sol`). N legs, the last
absorbing the remainder so allocations sum exactly however the solver rounded,
pool-direct V3 legs settling by transfer in `uniswapV3SwapCallback` with no
approval, and allowlisted router legs for Curve and Balancer. The swap callback
needs its own CREATE2 pool-address verification — storing "the pool we called"
does not generalize to N legs.

**Eligibility became two-sided** (GUIDE 07). Flash fundability is necessary, not
sufficient: a position whose collateral has no exit route at acceptable slippage
is exactly as untargetable as one you cannot fund. `is_eligible` now consults the
route cache as well as the flash index, over every `(repay, seize)` pair.

**Uniswap V4 split in two.** Excluded as a swap venue (hooks make quoting
pool-specific); retained as the preferred flashloan source (hooks do not fire on
`take`/`settle`). Separate config surfaces, because the exclusion propagating
into the flash-source list would silently cost the zero-fee bid advantage.

---

## Addendum — scope, timeline and infrastructure

**One box, not three.** The archive-node suggestion was wrong independent of
budget: archive access is needed exactly once, for the GUIDE 05 extraction. Rent
it for a month, produce the parquet, cancel. Production runs a pruned full node.
Analytics scaled down to JSONL plus SQLite plus a daily digest script — at tens
of liquidations a day the heavy stack buys nothing and its I/O is the p99 jitter
you are tuning against.

**Scope by constraint class, not developer-hours.** With agent-assisted building,
code compresses enormously; calendar (node sync, the 7-day drift gate, 2–4 weeks
of shadow) and data (bid-model calibration) do not. So **build wide** — twelve
adapters, five flash sources, nine triggers — and defer only what is data-gated.
The earlier advice to narrow to three protocols reasoned from the wrong axis.

Realistic schedule: ~1–2 weeks of building, node sync and extraction in parallel,
then the gates. **First live submission ~4–6 weeks out, most of it waiting.**

**The gates survive cheap code, and matter more.** An agent will produce an
adapter that compiles, reads correctly, and has one rounding direction backwards
— invisible at HF 1.3, decisive at HF 0.9999. The risk moved from "will this get
built" to "is the built thing subtly wrong in a way that silently loses money."

**Latency was under-credited.** An earlier draft said bidding matters "not
latency." Under SVR the auction is decided on value rather than arrival order,
but every searcher's clock starts when the hint arrives and the block seals at a
fixed moment — so proximity buys **decision time**, and it remains decisive
outright for the eight non-auctioned trigger classes.

---

## What did not change

GUIDES 00, 02, 04, 05, 06, 09, 13 are substantially as written. The correctness
spine — fixed-point rounding, columnar store with undo ring, the drift gate, the
recall gate, the SVR oracle path, the miss taxonomy, bundle execution — is
unaffected by all three new constraints. That is the intended property: funding
strategy, protocol breadth and hardware are supposed to be swappable above a
correct foundation, and this reshape is evidence the layering held.

---

## Round 3 — validation path, prune profile, supervised window

Prompted by a proposed self-validation plan: replay as far back as a pruned node
allows, accept ≥90% match, then test live during watched hours only.

**One finding invalidated part of the earlier plan.** Reth's `--full` defaults
retain account history, storage history **and receipts** for 10,064 blocks —
about 33 hours. Receipts hold logs, so a `--full` node cannot serve `eth_getLogs`
past roughly a day and a half. D07 previously said "pruned full node in
production; archive rented once," which would have destroyed the replay archive
during the very first thing on the critical path, and surfaced weeks later at the
recall gate.

| Area | Change |
|---|---|
| **GUIDE 16** | New **Step 0b — the prune profile is a one-way door**. `receipts_log_filter` keeps full log history for tracked contracts while pruning state to 10,064 blocks. Storage bullet and Step 7 updated; acceptance criteria gained the profile and a post-sync retention check. |
| **D07 / new D15** | Own log-filtered node *is* the replay archive. Archive rental demoted from required line item to recovery path. D15 records the profile; its address list is **unset and gates the sync**. |
| **GUIDE 05** | Step 1 rewritten: source is the local node, not a rented archive; added the "verify against a second source" warning (pruned `eth_getLogs` has returned silently empty rather than erroring). Step 3 recall reframed — see below. |
| **Recall gate** | No longer a percentage. `never_detected` empty is the bar, with a new `declined`/`DeclineReason` split alongside it. A blended match rate conflates a detection bug with the system correctly declining an unaddressable position, and cannot clear the gate at any value. |
| **GUIDE 09** | New **Step 4a — the live miss alarm**. Continuous watcher, structurally independent of the engine, sharing only ABI decoders; same code as GUIDE 05's ground-truth extractor with two consumers. Alarms on `NotTracked`/`HealthWrong` only. |
| **GUIDE 17** | New **Step 0 — the supervised live window**, and a fourth gate. Window the *submit flag*, never the box or the watcher: hourly instances lose local disk, a box off for 20 hours restarts ~6,000 blocks behind, and windowed observation measures a biased sample of the quiet market. |
| **ORCHESTRATOR / STATE / INDEX / PRE-FLIGHT** | Fourth gate added; Track A gains the prune profile at the front and the supervised window at the end; Track C loses the archive rental; two new human checkpoints (before sync, before unattended); timeline extended to ~7–9 weeks to unattended running. |

**What the proposal got right and is now written down.** The live miss alarm —
"if a liquidation lands on a target protocol and we didn't spot it, something is
wrong" — is a better instrument than a batch recall number, because it catches
the failures that *appear* in a previously clean system: a proxy upgraded behind
you, a new asset listed, a spoke reconfigured. And a supervised live stage
between shadow and unattended running was missing entirely; the guides went
straight from shadow to production.

---

## Round 4 — snapshot sync, and the warm spare removed

**Sync is not the long pole.** Genesis sync is days to a week and is disk-IOPS
bound, not CPU bound — Paradigm: "the dominant factor for performance across
hardware is the hard drive." `reth download` pulls official public snapshots at
~100–300 MB/s, making it hours of bandwidth instead.

| Area | Change |
|---|---|
| **GUIDE 16 Step 0b** | New subsection **"Do not sync from genesis."** `reth download` profiles and component flags, with `--with-receipts-since` as the one that matters — it puts deep receipt history on disk at download time, which `receipts_log_filter` alone cannot do retroactively. Documents the ordering trap (`--force` deletes `reth.toml`), the snapshot trust trade, and the `reth prune`-runs-twice quirk. |
| **ORCHESTRATOR / STATE / INDEX / PRE-FLIGHT** | "Days of wall clock" corrected to hours everywhere. Track A's first item is still first — it is now cheap rather than slow, which is a better reason to do it first, not a reason to defer it. |
| **GUIDE 17 Step 1** | **Warm spare removed from phase 1**, matching D05. Topology diagram redrawn as one box; the lease is retained with one holder as a post-restart safety interlock, not a failover mechanism; spare and lease-handoff deploys relabelled phase 2 throughout. |
| **GUIDE 17 acceptance criteria** | "Warm spare exists, failover < 30 s" removed — it contradicted D05 and would have sent an agent building a second box. Replaced with measuring cold-restart time and rehearsing an off-box restore. |
| **GUIDE 17 runbook** | `Node lag` action no longer says "fail over to spare"; it halts submission, which is the actual single-box answer. |

**Why the spare removal mattered.** D05 said single box; GUIDE 17's diagram,
deploy story, runbook and acceptance criteria all still assumed two. An agent
working from the acceptance criteria would have built toward hardware that was
explicitly declined.

---

## Round 5 — the node's role, and backup posture

| Area | Change |
|---|---|
| **GUIDE 13 Step 1** | New subsection **"Your own node is not the submission path."** Every venue except `PublicMempool` is an HTTPS POST to a relay or builder; the node is not on that wire, and `eth_sendRawTransaction` against it *is* the public mempool by another name. States what colocation actually buys — no RPC in the hot path under ExEx, simulation against exact state, no rate limits, and no query-stream leakage to a third-party provider. RPC server stays on, localhost-bound, ops-only. |
| **GUIDE 16 Step 7** | New **"What actually needs backing up."** Node DB needs no backup — `reth download` is the backup. State store rebuilds from logs, so replication is convenience, deferrable. The unrecoverable part is kilobytes of config and keys and belongs in git today. |
| **GUIDE 17 acceptance criteria** | Backup criterion split accordingly: config/keys day one, state-store replication marked deferrable, node DB dropped entirely. |
| **D17, D18** | Backup posture and submission path recorded as decisions. |

**Why the submission subsection exists.** The guides said "never the public
mempool" but never said what the node's role in submission *is*, which leaves
"colocated private RPC for submissions" as a natural and wrong reading. The
architecture is better than that reading — under ExEx there is no RPC in the hot
path at all — but the correction has to be explicit or the misconception
survives contact with the guides.

---

## Round 6 — submission corrections, batching, and the operating agent

| Area | Change |
|---|---|
| **`AGENT-OPS.md`** (new) | The agent that operates the system once built, framed as an experiment. Hypothesis, the five claims and which two are novel, four phases with authority *tightening* as capital comes online, counterfactual scoring against a frozen config, falsifiable predictions per action, permission-enforced invariants, and what would count as a result. `ORCHESTRATOR.md` governs build agents; this governs the operating one. |
| **GUIDE 13 routing table** | `InterestDrift` and `Stale` move from "private relay, standalone" to **one-tx bundles**. Atomicity is a property of bundles, not transactions — a lone tx has no revert invariant and a builder still earns the priority fee by including it. One-tx bundles give atomic semantics with no ordering constraint and force per-block requoting. |
| **GUIDE 13 — builders vs relays** | You submit to **builder endpoints**, which are public. `relay.flashbots.net` is Flashbots' *builder*, misleadingly named. mev-boost relays are trust escrow between builder and proposer and have nothing to do with searchers — "submitting via relays" is not a thing. Added fan-out prioritisation, curation over spraying, and the note that you can hold the best bid and still not land because your builder lost the slot. |
| **GUIDE 13 — identity key** | `X-Flashbots-Signature` is signed with a **third key**, separate from every transaction-signing key, holding no funds and carrying relay reputation. Rotation resets that reputation, so it is a decision rather than hygiene. |
| **GUIDE 13 → GUIDE 12** | `min_viable_notional` clarified: it answers "if I win, is this profitable after gas," **not** "what does losing cost" — losing costs nothing once everything is a bundle. The naive reading passes on opportunities that are free to attempt, which matters most at the small end. What bounds breadth is nonce slots and solver throughput. |
| **GUIDE 12 Step 4b** (new) | Batching by debt token: what it buys (notably one nonce slot instead of N, worth most during cascades), the four complications (atomicity inversion, joint routing across shared output pools, flash depth forcing a worse source, V4 dynamic close factor), and a co-occurrence query that decides it from the replay archive before any contract work. |
| **D19–D22** | Bundle discipline, the three keys, batching deferred-pending-query, agent operations. |

**The phrasing fix.** The earlier "no RPC in the hot path" read as a global
claim. Corrected: there is no RPC between the bot and the node; submission itself
is very much JSON-RPC, outbound to a third party.

---

## Round 7 — the registry, and a live bug in the Executor

### The bug

`Executor.sol` called every token through the bool-returning ERC-20 interface.
**USDT returns no data** from `transfer`/`approve` — the call succeeds at the EVM
level and then reverts in Solidity's ABI decoder, which expects 32 bytes. It hit
the Aave flashloan repay, the UniV3 / Morpho / Balancer repays, the router
approval, the swap-callback payment, `sweep()`, and — worst — the liquidation
repay approval itself.

USDT is one of the largest debt assets on Aave. As written, every USDT-denominated
liquidation reverted, and it would have presented as "those opportunities never
work" rather than as a bug. Second, separate issue: USDT reverts on a non-zero →
non-zero approve, so any router that failed to consume its allowance exactly
would have left that path permanently stuck.

Fixed with a mandatory `SafeTransfer` library — low-level call, empty return data
accepted, allowance zeroed before every set and after every router leg. All ten
call sites converted; CI should grep for the raw form.

### `REGISTRY.md` (new)

Addresses, decimals, token0/token1 ordering, fee tiers. Split explicitly from
D15's prune filter, which is a *log-emitter* list and irreversible; the registry
is computation data and regenerable.

The argument for giving it its own document: **a decimals error does not fail
safe.** Off by 10^12, and `minProfit` is denominated in the same token computed
at the same wrong scale — so the guard passes, the simulation passes, and every
safety mechanism inherits the error rather than catching it.

Verification is three layers, because they catch different things: independent
re-derivation (not review — a second generation), a canonical-list identity check
(the layer that catches *a valid address for the wrong token* — USDC and USDC.e
both answer `decimals() == 6`), and a boot-time assertion that refuses to start on
any mismatch. Agent generates; machine verifies.

| Area | Change |
|---|---|
| **`Executor.sol`** | `SafeTransfer` library; all ten token call sites converted; router allowance zeroed after each leg |
| **GUIDE 10** | Six new acceptance criteria — no raw ERC-20 calls (grep in CI), full fork matrix **against USDT specifically**, non-zero→non-zero approve test, router allowance assertion, fee-on-transfer via measured deltas |
| **GUIDE 00** | Registry boot assertion wired before startup, plus a test that a corrupted entry refuses to run |
| **GUIDE 16 §0b** | Prune filter clarified as a log-emitter list, in tiers. **Receipt tokens added** — an aToken transfer moves a position with no protocol event, so omitting them makes `collateral_token_transfer` invisible in replay and the recall gate passes without testing it. Oracle proxies *and* underlying aggregators. Pools as a deliberate shallower tier. |
| **ORCHESTRATOR / STATE / INDEX** | Registry generation added to Track C at day 0 — no node dependency, and it is where D15's address list comes from |
| **D23, D24** | Registry; `SafeTransfer` everywhere |

---

## Round 8 — batching specified, Comet deferred

### Batching is now the only path

Previously GUIDE 12 §4b was "deferred; measure before building." It is now
specified and built into the contract, because two situations produce the same
shape and one of them is unavoidable:

- **N positions sharing a debt token** — Alice, Bob and Mary all owe USDC
- **N collaterals on one position** — Aave accounts are cross-collateralised, so
  one borrower may post ETH, wBTC and an LST against a single USDC debt

**A single position is `liqCount == 1`.** There is no separate single-position
path, which is what keeps the batched path exercised by every test rather than
by one rare one.

| Area | Change |
|---|---|
| **`Executor.sol`** | `Plan` split into a shared header plus a per-leg `LiqLeg`. `_core` walks N legs inside `try/catch`, skips failures, reverts only if all fail (`AllLegsFailed`). Failed legs drop their allowance. `_liquidate` → `_liquidateLeg`; `_guard` → `_isLiquidatable` returning bool rather than reverting. `adapter` moved per-leg so a batch can span protocols. |
| **`_swap`** | Each swap leg now carries its own `tokenIn`, because with several collaterals in flight "the remainder" is meaningless without saying remainder *of what*. Positional remainder logic replaced by a `TAKE_BALANCE` flag: the last leg per collateral sweeps that token's whole balance, which cannot disagree with what is held — so it absorbs solver rounding, partial fills, and skipped liquidation legs alike. |
| **`PLAN-ENCODING.md`** | Rewritten. 74-byte shared header, liquidation-leg blob (77 bytes × N), swap-leg blob (40 + data per leg). Blob offsets are derived, never encoded. Round-trip proptest must vary `liqCount` and `swapCount` independently — stride bugs hide behind single-leg fixtures. |
| **Encoder invariant** | The Rust encoder must assert every collateral has exactly one `TAKE_BALANCE` leg. Zero strands it until the next sweep; two makes the second a silent no-op. The contract cannot see this; only the encoder can. |
| **Over-borrow** | `flashAmount` should exceed the sum of repays whenever a leg might be lost. Free with a zero-fee source; a real input to source selection with a fee-charging one. |
| **`minProfit`** | Now explicitly a floor across the whole batch, not an expectation. Encoding the all-legs-succeed outcome makes any partial fill revert — free, but it discards a profitable partial. |

### The joint routing problem

The substantive addition. K collaterals into one debt asset share output pools —
an LST→USDC path passes through ETH/USDC, which is also where ETH→USDC goes.
Solved independently, each solve assumes it has the whole pool, so every quote
understates impact **in the direction that inflates net and makes you overbid**.

Resolved without a joint convex program: swap legs execute sequentially on-chain,
each seeing the state the last one left, so a sequential simulation is *exact*
for a given ordering. Evaluate orderings (exhaustive at K ≤ 4,
largest-notional-first beyond), water-fill each collateral against the
progressively displaced state, keep the cheapest. The inner solve is the existing
Step 4 water-fill, unchanged.

### Seize selection was a minimisation and should be a maximisation

Aave sets `liquidationBonus` **per reserve**, and e-mode changes it again. So on a
multi-collateral position, seizing wBTC at one bonus versus ETH at another differs
in revenue before exit cost enters at all. GUIDE 07's filter ends
`.min_by_key(|r| r.effective_cost())` — cheapest exit, not best trade. On a
position holding thin-but-high-bonus beside deep-but-low-bonus, those give
opposite answers.

### Compound V3 deferred

Excluded from phase 1 as a sequencing decision, recorded in GUIDE 15 Step 1b
rather than `DECLINED.md` — it is deferred, not declined.

Its mechanism is genuinely different: `absorb()` is permissionless, takes the
entire account with no choice of collateral, and pays the caller nothing; the
money is in `buyCollateral()` at a discount. The flashloan cycle inverts to
*borrow base → buy at discount → swap back → repay*.

**The exclusion has a cost worth naming:** Comet was third in the original
ordering precisely because it was the cheapest moment to find an abstraction flaw
in GUIDE 01. The trait now gets one generalization test (Morpho Blue, position
scope) instead of two. A later mechanism-shaped change to `liq-engine` is the
delayed bill for this, not a surprise.

---

## Round 9 — registry discovery

The registry generation step is now explicitly **discovery, not transcription**.

The prompt: "Aave V3 on Ethereum" is not one market — Core plus separate Pool
instances, each its own address — and Aave V4 launched with three hubs and eleven
spokes. An agent assembling that list from documentation produces a snapshot of
what someone wrote down, and the omission lands in the `receipts_log_filter`,
which is the one decision that cannot be revised.

| Area | Change |
|---|---|
| **`REGISTRY.md` §3** (new) | Per-family enumeration: `PoolAddressesProviderRegistry.getAddressesProvidersList()` for V3 instances, `getReservesList()` + `getReserveData()` for reserves (which yields receipt tokens as a by-product rather than as a separate thing to remember), `AaveOracle.getSourceOfAsset()` → `.aggregator()` for feeds, `CreateMarket` events for Morpho, factory calls or `PoolCreated` for pools. V4's spoke enumeration flagged as **confirm against the deployed contract, not the docs**. |
| **Counts as evidence** | Instances, reserves, aggregators and receipt tokens are recorded, so a later discovery run finding *fewer* is visibly wrong rather than quietly incomplete. |
| **Recurring, not one-off** | Discovery re-runs on a schedule and the diff is the alert. Pairs with GUIDE 09's `NoCollateralExit` declines — a governance listing shows up as declines accumulating on one asset *and* as a discovery diff naming it. Two independent signals for something that arrives on someone else's schedule. Good standing job for the operating agent. |
| **§6 sequencing** | D15's prune filter is now explicitly **generated from the committed registry**, not hand-assembled — which is the reason discovery runs before the node sync rather than alongside it. |
| **GUIDE 16 §0b, ORCHESTRATOR, STATE** | Same rule restated where the sync is actually started, since that is where the irreversible mistake gets made. |

---

## Round 10 — protocol families and market admission

`REGISTRY.md` §3 now splits discovery by protocol family, and §3b decides what
gets admitted at all.

**Family A — governed sets.** Aave, Spark, Compound V2 forks, Sky. A registry
contract knows every market: `getAddressesProvidersList()`, `getAllMarkets()`, the
ilk registry. Bounded and slow-changing. *A fork is the same enumeration code with
a different root address* — which is where GUIDE 15's fork multiplier actually
comes from.

**Family B — permissionless sets.** Morpho Blue, Euler V2, Silo, Ajna. No list to
call, only creation events, and the set grows continuously.

### The prune-filter consequence, which runs opposite to intuition

Family B splits on architecture, and the split matters more than the family does:

- **Singleton** (Morpho Blue) — one contract emits every market's events, so it is
  *one* `receipts_log_filter` entry covering thousands of markets. Easier than
  Aave.
- **Per-vault** (Euler V2, Silo) — addresses created permissionlessly over time,
  so **you cannot pre-list addresses that do not exist yet**. A vault created in
  month three has its receipts pruned as they arrive until the filter is
  regenerated.

Live operation is unaffected (the ExEx stream ignores what is stored); the loss is
replay, so a late-discovered vault cannot clear the recall gate. Fixed mostly by
discovery cadence, optionally by a rolling full-receipt window alongside the
filter — with a note to verify the two compose on your own node rather than
assuming.

### Admission (§3b) — and why the reason matters more than the answer

Phase 1 tracks markets with real borrowed volume. But chosen deliberately for
**focus and evidence**, not safety:

**The oracle does not need to be trusted.** Morpho's five params split usefully —
LLTV and IRM are governance-whitelisted, loan and collateral tokens are already
filtered by GUIDE 07, leaving the oracle, whitelisted by nobody. And it does not
matter: the market's oracle determines *liquidatability* (a fact about protocol
state either way), while your own routing math determines *profitability*, enforced
on-chain by `minProfit`. A dishonest oracle creates opportunities at that market's
lenders' expense, not yours — it cannot make an unprofitable liquidation look
profitable, because profit is measured in the swap.

So the real costs are hot-path memory and cycles, and **gate evidence** — enable
3,000 markets where 2,950 never liquidate and the recall gate is measured on the
fifty that mattered anyway.

Which makes the bar **mechanical and movable**: minimum *borrowed* (not TVL) sized
to `min_viable_notional`. Pick a narrow scope for safety reasons and the bar never
moves; pick it for evidence and lowering it later is the long-tail edge.

**One thing stays curated:** the registry is the safety boundary. Market addresses
reach `Executor` from the plan and the batching `try/catch` does not bound gas, so
a market that burns the call's gas takes the batch with it. Generous bar, but a
bar. The same reasoning transfers to DEX pools — filter on depth plus the oracle
cross-price, never vet.

**D27** records the admission rule; its threshold is unset and travels with D11.

---

## Round 11 — the viability band replaces `min_viable_notional`

`min_viable_notional` is deleted. It was wrong in three ways at once, and the
third only became visible after the first two were fixed.

**It was derived, not input.** Exact swap math, exactly-known next base fee, exact
flash fee — so "is this profitable" is a direct computation and the threshold is
just where it crosses zero. Caching it kept a second, staler copy of something
derivable exactly: the same mistake as encoding a blob offset.

**It was keyed wrong.** Bonus rate lives on the *collateral* (per-reserve,
e-mode dependent) and exit liquidity is a *(collateral, debt) path* property.
Keying on debt asset alone dropped both.

**It was one-sided.** The cost terms have different shapes, and that is what
creates the structure:

```
net(s) = s·bonus_rate − s·fee_rate − impact(s) − s·flash_rate − gas
         ├── linear ──────────────┤  └ convex ┘  └─ linear ─┘  └ fixed ┘
```

Gas is fixed and dominates at small `s` — the lower edge. Impact is **convex** and
dominates at large `s` — the upper edge. Fees and flash rate are linear: they tilt
the line and move both edges but create neither. Folding impact in with fees, as
the guide previously did, hid the entire reason an upper edge exists.

| Area | Change |
|---|---|
| **GUIDE 12 Step 4b** (new) | The viability band. Why a stored threshold is the wrong object, the cost-shape decomposition, the ETH numeraire, **base fee is a cost / the bid is a distribution** (which resolves a circularity — net depends on the bid, the bid is a fraction of net), and the band as a *join* over `FlashIndex` + route cache + registry bonus + gas oracle rather than a new subsystem. Emitted by the warm tier. Explicitly **not** called an oracle. |
| **Per-block, not gas regimes** | Next base fee is a pure function of the parent block and moves at most ±12.5%, so recompute during the inter-block window for the fee you already know applies. Regimes are a fallback if the impact walk proves expensive, not a starting design. |
| **Manipulation resistance, sized down** | The band is a pre-filter: a manipulated band leads to an exact computation, and `minProfit` drops a wrong bundle for free. So it cannot cost money — only missed opportunities. `min(spot, twa)` on liquidity and price, conservative in both directions, plus the warning that **a plain TWA degrades exactly during cascades**, when liquidity collapses for real and opportunities are densest. Lean generous; the guard decides. |
| **Three decisions un-conflated** | Execution: exact net, no threshold. Tracking: the band, crude is fine. Slot allocation: an *opportunity-cost* floor that is zero with spare capacity and tracks slot pressure — moved to GUIDE 13 Step 4 where the nonce pool lives. |
| **Step 4b/4c renumbered** | Band is 4b (it follows the profit model), batching is 4c. Cross-references updated in STATE, GUIDE 15 and PLAN-ENCODING. |
| **Propagation** | GUIDE 05 (`DeclineReason::BelowMinNotional` → `OutsideViabilityBand`, now two-sided), GUIDE 07, GUIDE 13, GUIDE 15, PRE-FLIGHT Q5, REGISTRY §3b admission bar. References to *other searchers'* thresholds left alone — that is still a real thing about competitors. |
| **D28** | Records the band; D27's admission bar now sized to its lower edge at a representative gas level rather than to a cached constant. |

---

## Round 12 — profit is denominated in ETH

The bid has to be paid in ETH — builders value a bundle by `coinbaseDiff`, the
coinbase's *native balance* delta, so an ERC-20 sent to that address is worth
literally zero to them. At a high `bidBps` the bid is nearly all of net, which
means **the conversion to ETH was happening anyway.** Routing the remaining few
percent along with it is close to free, and it collapses several open problems at
once.

### The swap pass now has exactly two outputs

```
seized collateral ──┬─► debt asset, EXACT OUTPUT = flash_owed   (repay)
                    └─► WETH, take-balance                      (profit)
```

Exact-output on the repay leg means no debt-token dust is stranded — previously
you had to over-provision and keep the remainder. V3 encodes the mode in the sign
of `amountSpecified`, so it is the same call site.

### What it buys

- **One numeraire.** `minProfit` is in wei, gas is in wei, the viability band was
  already in wei. The guard and the cost it guards against are finally the same
  unit.
- **The executor stops being a vault.** One asset between transactions instead of
  an accumulating pile of debt tokens — the proxy posture it was designed for.
- **The bid funds itself.** No ETH float, no top-up loop, no working capital, so
  D02 holds end to end. The alternative — holding an ETH balance for bids — would
  have been inventory by another name.
- **D12 dissolves into arithmetic.** The sweep threshold was a risk budget when
  profit accumulated across several debt tokens. With one asset and a bid paid in
  the same transaction it is just "is the balance worth ~9k gas of transfer",
  `balance > k × transfer_cost`, derivable rather than judged.

**It is often cheaper, not dearer.** Most collateral routes to a stablecoin
*through* ETH, so stopping at ETH saves a hop. The case that genuinely costs is
stablecoin collateral against stablecoin debt — priced honestly in the band
rather than waved away.

| Area | Change |
|---|---|
| **`Executor.sol`** | `WETH` immutable; profit measured as the WETH delta **after `_initiate` returns**, so the flash repay has already left and the debt-asset-is-WETH case needs no special handling. `net = gross − gasCostWei` (underflow is the correct failure). `bid = net × bidBps / 10_000`, unwrap, `call{value:}` to `block.coinbase`. Residual sweeps as WETH. `L_EXACT_OUT` leg flag; `_swapLeg` takes an `exactOut` bool and flips the sign for V3. |
| **`PLAN-ENCODING.md`** | Header 74 → **92 bytes**: adds `bidBps` (u16) and `gasCostWei` (u128); `minProfit` redenominated to **wei**. Swap legs gain `EXACT_OUT`; `tokenOut` is derived from the flag, not encoded. Encoder asserts exact-output legs precede balance-taking legs — an `EXACT_OUT` leg consumes an unknown amount of collateral, so a `TAKE_BALANCE` leg before it would sweep collateral the repay still needs. |
| **GUIDE 12 §4d** (new) | Two outputs, why ETH, the often-cheaper argument, and the deliberate ETH exposure. |
| **GUIDE 12 §4e** (new) | Three bid channels — coinbase transfer (default, tracks realized net), priority fee (a *rate*, keep modest and nonzero), `refundConfig` (SVR, a refund share rather than a payment). The bid model produces a venue-appropriate expression, not one wei amount forced into every channel. |
| **GUIDE 14 §5 / §5b** | Sweep threshold reframed as a gas formula. New disposal section: long ETH on purpose (gas is priced in ETH, so margin in ETH terms is stable — closer to a hedge than a risk), batched CEX disposal with a cadence and floor, and the counterparty risk named. |
| **GUIDE 10** | Six criteria: profit is WETH-only, no debt dust, `call` not `transfer`, a contract fee recipient needing >2300 gas, bid shrinks on an under-delivering swap, and the debt-is-WETH case. |
| **D29–D31** | Profit denomination, bid channel, disposal. **D12 revised** rather than left unset. |

---

## Round 13 — flash groups, candidate selection, and the bid as an optimisation

### Flash groups

One oracle update makes positions liquidatable **across many debt assets**, and
the format assumed one flashloan. A plan now carries N groups, each with its own
source, asset, amount, liquidation legs and repay swaps, executed **sequentially
— never nested**. Profit swaps run once, globally, afterwards.

The ETH-denominated profit decision from Round 12 turns out to be what makes this
tractable: every group repays its own asset, all surplus converges on WETH, and
the guard stays a single wei check. Per-debt-asset profit would have needed one
guard and one threshold per group.

`tokenOut` is now **encoded per swap leg** rather than inferred from the flag —
inference worked while a plan had one debt asset and silently would not now.

Header 92 → 35 bytes; `SKIP_SWAP` and `RECEIVE_ATOKEN` deleted (skipping is zero
legs; aTokens cannot converge on WETH). `_currentGroup` re-walks calldata from a
transient group index, because no provider's callback ABI has room to carry it.

### Candidate selection (GUIDE 12 §4f)

**Eligibility is a gate; ranking is an ordering within it.** Stated first and
twice, because "maximise net-per-gas across candidates" invites an agent to build
a candidate set of *nearly*-liquidatable positions — a richer optimisation over
the wrong set.

It is also **self-punishing**, which is the version worth internalising: a failed
leg still burns gas via `try/catch`, and that gas comes out of realized net, which
is what the on-chain bid is computed from. Padding a bundle shrinks the bid you
can afford.

**No batching threshold.** `expected_net_per_gas = p·net / (p·gas_ok + (1−p)·gas_fail)`
— during a cascade `p` falls, marginal candidates drop out on their own, batches
shrink without configuration. A "batch when N > 10" rule would have reintroduced
two code paths and a discontinuity, against the standing decision that a single
position is just a batch of one.

**SVR forces batching; cascades only reward it.** In an SVR auction you bid for
the right to backrun *one transaction*, so everything must be in that bundle and
aggregate net is what you bid with — the competitive mechanism, not an
optimisation. Separately: two-stage selection under load, crude rank across all
candidates, exact solve for the top K.

### The bid model (GUIDE 12 §6)

Replaced "bid aggressively, parameterise as a floor" with the actual
optimisation: `E[profit|β] = net·F(β)·(1−β)`.

**"More wins" is the `F(β)` term** — not a separate consideration the
optimisation ignores, but the thing being multiplied, by a `(1−β)` that goes to
zero. For a flat 0.99 to be optimal, `F` would have to be a near-step function
exactly there.

Estimator: per ETH-normalised bracket **and contest class**, fitted from
`inferred_bid`, windowed by **sample count not blocks**, with **own wins
excluded** (otherwise it ratchets against itself) and the **untaken fraction** as
a cleaner contest signal than an average over the taken ones. Randomise the
**margin**, not the lookback — noise in the measurement only degrades the
estimate.

Plus: bid high during the learning phase on purpose, because you cannot fit `F`
from losses you never observe. And the objective is stated once — expected
profit, win rate secondary — so a future agent does not reconcile two goals by
guessing.

### Parameterisation (`PRE-FLIGHT.md` §7, new)

Gas limit 30M → 36M → 45M → 60M in eighteen months, Glamsterdam targeting 200M.
**Read it from the block header.** EIP-7732 (ePBS) is flagged as the most exposed
part of this design — it touches the whole submission path — with an explicit
"track it, do not pre-build for it," because guessing the searcher-facing
implications now would bake in worse assumptions than the current ones.

### Wallet-funded bidding (GUIDE 10 §4d)

`payable execute` with `msg.value` as a **ceiling** — the bid is still computed
from realized net, `min(bid, msg.value)` is paid, remainder refunded. Built and
default-off: the executor is immutable, so this is a branch now versus a redeploy
later. The code never reads `address(this).balance`, because `receive()` is open
and a donation would otherwise be bid away.

**D32–D38** record all of it.

---

## Round 14 — `TESTING.md`

The last gap, and it is a specific one: **an agent that writes both the code and
its test derives the expected value from the code.** The test then asserts what
the code does rather than what it should do, and the bug ships with a green
checkmark on it. Coverage does not detect this — a tautological test executes
every line.

### The oracle rule

> Every assertion names where its expected value came from. If the answer is
> "the code under test," the test is void.

Checkable by review, which is what makes it usable. Four legitimate oracles: the
chain, an independent implementation, a mathematical invariant, a recorded
real-world outcome.

### The assertion map (§3)

Per area — what must be true, the oracle, and **the fake**: the test that passes,
looks reasonable and proves nothing. Naming the fake is what stops it being
written. Highlights:

- **Adapter health** is the likeliest tautology in the system, because writing the
  expected value out longhand feels like verification and is not. The chain is
  sitting right there.
- **The encoding proptest** must call the *Solidity* decoder on a fork. A Rust
  reimplementation shares the author's misreading of the layout and agrees with
  the encoder about it.
- **Fuzz generators must concentrate on the boundary.** Uniform inputs sit at
  HF ≈ 1.4 where nothing is at stake. "100k cases, zero failures, first run" is a
  reason to inspect the generator.
- **Mocks that are better-behaved than reality are how bugs survive.** The USDT
  bug from Round 7 would have passed any suite built on a standards-compliant mock
  token. Fork tests, real contracts, for token behaviour and protocol responses.
- The new selection/band/bid assertions are all **negative or adversarial** — a
  non-liquidatable candidate that must not enter the set, a self-win that must not
  move the estimator, a liquidity collapse that must shrink the band. Agents write
  positive assertions by default; these have to be asked for by name.

### The mutation checklist (§4)

Twelve named mutations — flip a rounding direction, off-by-one in the leg stride,
swap token0/token1, remove a guard, use `address(this).balance` for the bid — each
mapped to the test that must catch it. **Every one corresponds to a decision in
`STATE.md`**: if the suite does not catch it, that decision is not enforced, it is
a comment.

It is the only mechanical defence against pass-shaped tests, and the only
checklist item that cannot be satisfied by writing convincing prose.

### And what cannot be tested (§6)

The drift week, six-month recall and shadow mode are **evidence, not tests**. An
agent under time pressure will reach for a synthetic version that passes in
seconds. Named explicitly, and added to the orchestrator's never-do list.

Wired into `INDEX.md`, the `ORCHESTRATOR.md` session protocol and first-session
reading order, GUIDE 00's fuzz criterion, and **D39**.

---

# Round 15 — The recall gate, and what `declined` actually means

Two challenges, both to claims in GUIDE 05 Step 3 that had been asserted without
support.

## 1. The gate no longer depends on a ≥6-month archive

**The claim that failed.** The gate read "`never_detected` empty over a ≥6-month
window." Replaying six months requires the full event stream — every borrow,
repay, supply, withdraw, transfer and oracle update across every tracked
contract, millions of logs. That is the expensive half of Step 1, and stating the
gate that way quietly made an archive-grade extraction a precondition for
progress.

**The separation that fixes it.** Ground truth and replay input are not the same
cost. Ground truth is a handful of event signatures and a few thousand events —
free-RPC fetchable. Replay input is millions of logs. But the ground truth is the
only half the gate needs, because the engine can be compared against liquidations
**as they land** instead of being replayed against history.

**So the gate is measured forward**, off the live watcher that GUIDE 09 Step 4a
already specified. That component now persists every observation with its
coverage dimensions rather than only alarming on the bad ones.

**And cleared by coverage, not calendar.** Six months was always a proxy for
"enough variety that an empty miss list means something," and it never measured
that. A quiet six-month window covering one regime and four collateral types
proves less than three weeks spanning a cascade. The proxy is replaced by the
thing it stood for: every enabled protocol instance, every collateral family,
every trigger class, one top-decile volatility block, and a floor count set
before observing.

Strictly stronger than the calendar version, and it cannot be cleared by
selecting a favourable window — there is no window to select.

**The cost, stated rather than hidden.** Forward observation cannot outpace the
market. A shape that occurs quarterly takes a quarter. Two mitigations: archive
replay counts toward coverage where the archive reaches, and a row that will not
fill is recorded as **uncovered** with what is missing. An uncovered row is a
known blind spot, which is a different thing from a cleared gate.

**GUIDE 05 Step 0 is new** — a lite validation on a free RPC and a bounded
position universe, runnable in an afternoon, months before the node exists. It
catches decoder bugs, inverted comparisons and wrong scalings. It is labelled a
smoke test in four places, because substituting it for the gate is exactly the
failure `TESTING.md` §6 exists to prevent. It also asserts the third direction
most people skip: we flagged it and *nobody* liquidated it, which is the same
adapter bug seen from the other side.

**GUIDE 16 Step 0b survives unchanged, with its rationale repaired.** The
filtered-receipt config is no longer on the gate's critical path — but receipts
not kept are gone forever, and it is still the only irreversible day-0 decision.
Its justification is now "this is what turns a missed liquidation into a root
cause," not "the gate is waiting on it." Removing a deadline is not removing a
reason, and an agent that defers it will not be reminded by anything downstream.

## 2. `declined` was characterised wrongly

**The claim that failed.** "`declined` should be large and its reasons should be
boring," and "most liquidations on mainnet are not addressable by a
flashloan-funded bot." Neither was grounded, and the data already in these
documents argues against both: on Aave V3 Ethereum, WETH, USDT and USDC are about
96% of borrowed value and all three are deeply flashloanable at zero fee. On that
distribution most liquidations *are* addressable, and `DebtNotFlashloanable`
should be rare.

**The structural point that was being missed.** Every entry in `declined` is a
liquidation that actually happened — somebody executed it and paid gas. That
makes it a sample biased toward the addressable, so the default reading of a
decline is suspicion, not reassurance. "We correctly skipped it" has to be argued
against a counterparty who did not skip it.

**Each reason is now classified:**

| Class | Meaning |
|---|---|
| MARKET | A fact about the position. Nothing to build |
| SYSTEM | Our deficiency wearing a decline's label. **A work queue** |
| KNOWN | Our own config, deliberately |

**`OutsideViabilityBand` is split into `BelowBand` and `AboveBand`,** because the
two edges have opposite meanings and one variant hid that. Below the lower edge is
MARKET — gas exceeds the bonus, dust is dust. Above the upper edge is **SYSTEM** —
"too large for my routing" is our router being weak, and a better route converts
it into a win. `NoCollateralExit` must state which class it is: a registry gap
and genuine illiquidity look identical in the enum and are not the same finding.

The gate now requires the **classified breakdown**, with any reason above 20% of
declines explained in `STATE.md` and no SYSTEM reason above that left unaddressed.
Not a target size — a size target would have been the same unexamined assumption
pointed the other way.

This is where the `PRE-FLIGHT.md` §3 edge argument actually cashes out: the
SYSTEM declines *are* the addressable-but-unaddressed opportunity, enumerated,
with block numbers attached.

**Files:** GUIDE 05 (Step 0 new, Step 1, Step 3 rewritten, Step 7, acceptance,
failure modes, handoff), GUIDE 08, GUIDE 09 Step 4a + acceptance, GUIDE 15,
GUIDE 16 Step 0b, ORCHESTRATOR §2/§4/§7, TESTING §3/§5/§6, INDEX, REGISTRY,
PRE-FLIGHT, STATE (gate row, new coverage matrix, D40, D41).

---

# Round 16 — The denominator, and the classifier that must not be ours

## The correction

`never_detected` empty was too blunt. Not every liquidation on a tracked protocol
was one we wanted: a $4 dust seizure, a borrower self-liquidating to avoid
something worse, a protocol keeper doing its job, a bot running at a loss. Held
against the detector, those produce a metric that can never reach zero and is
therefore never read — the same failure as the blended match rate it replaced,
one level down.

**The denominator is now in-scope liquidations**, and every miss is decoded
individually into a `MissClass` (GUIDE 05 Step 3a): `InScope`,
`OutOfScopeProtocol`, `OutOfScopeAsset`, `BelowBand`, `AboveBand`,
`NotFlashloanable`, `ExecutorUnprofitable`, `SelfOrKeeper`.

**Threshold: in-scope miss rate < 30% to proceed** (D40). Above it, the systemic
cause is found before continuing. Below it, proceed and keep tuning as
observations accumulate.

Thirty percent is defensible for a stated reason rather than as a round number:
**misses cluster by cause.** A decoder that mishandles one collateral type misses
every position holding it; a forgotten event path misses every position that
moved through it. A third of in-scope liquidations missing is one or two bugs
with names, not three hundred edge cases — precisely the case where "stop and find
it" is correct. Below that, the residue is better attacked with more observations
than with more staring.

**The floor is classification, not the rate.** 30% moves as the detector improves;
"every miss classified" does not. An unclassified miss could be anything, and a
pile of them is exactly how a systemic blind spot hides inside an
acceptable-looking ratio. `never_detected` is now typed
`Vec<(ActualLiquidation, MissClass)>` so an unclassified entry cannot be
constructed.

## The hardening this needed

Something has to decide scope for a position we **never detected** — and the
obvious implementation reconstructs the position and asks our adapter. That is
the tautology from `TESTING.md` §2 in its most dangerous form: **the decoder bug
that caused the miss also mis-classifies it as out-of-scope.** Both errors share a
root. A buggy system asked to grade its own failures reports that they did not
matter, and the gate certifies the bug.

The escape is that none of it requires our state. Debt and collateral assets,
size and protocol instance come from the event's own fields; target-set
membership is our declared config, not a computed value; flashloanability is an
`eth_call` at block *N−1*, not the `FlashIndex` cache; the exit path is a
fork-quote, not the router's cached pools; the band comes from oracle prices at
that block and the block's own base fee; the executor's economics come from their
receipt. Every input is the chain.

This is now a documented requirement in GUIDE 05 Step 3a with the full input
table, a review item in `TESTING.md` §3, and **mutation #13** — the only entry on
that list flagged as a design check no test can catch, because the wrong version
looks more thorough than the right one and passes everything.

GUIDE 09's watcher must now persist the **decoded event with all fields plus the
block hash**, not a summary line, so classification is reproducible months later
and does not depend on the watcher having judged correctly in the moment.

## Reconciled with Round 15

The coverage matrix is demoted from a second gate to a **qualifier on the
number**. An 8% in-scope miss rate measured over a period containing no volatile
block and no LST collateral is an 8% figure about the easy half of the market. An
uncovered row states where the measurement does not apply — which is the honest
thing to carry into the next stage, not a reason to stop. This also removes an
inconsistency: requiring every row filled while accepting a 30% miss rate was two
different levels of strictness in the same gate.

**Files:** GUIDE 05 (`RecallReport`, Step 3 lead, Step 3a new, window section,
acceptance, failure modes, handoff), GUIDE 08, GUIDE 09 Step 4a, GUIDE 15 (×2),
ORCHESTRATOR §4, TESTING §3 + mutation #13, INDEX (×3), STATE (gate row, gate
note, matrix note, D40).

**Still unset:** D40's floor observation count and the tighter miss rate for
unattended running (pairs with D13), D11, D12, D13, D27, D34, D35.

---

# Round 17 — D40 set: 50 in-scope observations, 30% flat

Both knobs closed.

## Floor: 50 **in-scope** observations

In-scope, not total. Most observed liquidations classify out of scope, so a run
of 50 liquidations might carry a dozen in the denominator; the count that matters
is the one the rate is computed over. Stated explicitly in GUIDE 05 Step 3a
because the looser reading is the easy mistake.

What 50 actually buys, now documented rather than assumed:

| True in-scope miss rate | Passes a 50-observation check |
|---|---|
| 30% | 57% |
| 35% | 28% |
| 40% | 10% |
| 45% | 2% |

It catches a broken system reliably and discriminates poorly at the line. That is
the right trade, for a stated reason: the miss rate is an **opportunity** metric,
not a safety one. A borderline pass costs some foregone profit and more data
arriving anyway — not capital.

## The per-slice rule — this is what 50 needed

The justification for 30% is that misses cluster by cause. That argument cuts
both ways, and the first version of this gate only read it in one direction.

One collateral type missed 3-for-3, inside a run of 50 where everything else is
clean, is a **6% aggregate and a 100% failure on that asset.** It passes
comfortably, and it is precisely the signature the threshold was written to
catch. An aggregate over a clustered failure is the wrong statistic.

So the rate is now reported **per coverage row** as well as in aggregate, and:

> Any slice where every in-scope observation was missed is root-caused before
> proceeding, regardless of its n — including n = 2.

Not a statistical claim and it does not need to be one. Low n means you cannot
*conclude* the slice is broken; it does not mean you should not *look*. The
confidence threshold governs whether you proceed, not whether you investigate,
and looking costs a handful of positions and a fork call each.

## Unattended rate: 30%, flat — and recorded as deliberate (D40b)

No ratchet between "proceed to the next stage" and "run unattended." Monitoring
is intermittent, not absent, and more importantly a miss costs the liquidation
and nothing else — it does not compound and it does not endanger capital. The
failure modes that *do* compound while nobody is watching are a bug submitting
losing bundles repeatedly, a stuck nonce, a drained gas balance. Those live in
GUIDE 17 Step 0 and the D13 kill criterion.

D40b carries an explicit **"do not tighten this"** note. A future agent reading
two identical thresholds will otherwise read it as an oversight and propose a
stricter recall number — which would substitute an opportunity metric for the
safety criteria that actually govern unattended running.

## Also added

A failure-mode row pointing at the market study (Q5/Q6): convert the liquidation
arrival rate into a calendar estimate for 50 in-scope observations early, so the
gate's duration is planned rather than discovered three weeks in.

**Files:** GUIDE 05 (Step 3a, window bullet, acceptance ×4, failure modes ×3),
ORCHESTRATOR §4, STATE (gate note, matrix rows, D40 set, D40b new).

**Still unset:** D11 (bid floor), D12 (sweep `k`), **D13 (kill criterion —
recommended before any code)**, D27 (admission threshold), D34 (`p`),
D35 (bid cap/jitter).

---

# Round 18 — D13 set: manual only, and the two things that follow from it

## The decision

**No mechanical abandonment rule.** The shadow gate still produces the numbers; a
human reads them and decides.

The standard argument for pre-committing a threshold is that you will not write
one once invested. That argument assumes the decision is time-critical — that by
the time you notice, something irreversible has happened. **Here it is not.**
Capital is protected per transaction, on-chain: `minProfit` is checked inside
`Executor.execute` against realized amounts, and a bundle that fails it reverts
for free. A losing trade is not reachable. So the only thing at stake in deciding
late is time, and time spent is already spent when you notice.

`PRE-FLIGHT.md` §4 is rewritten from "fill in your own numbers" to **the inputs
the decision requires** — profit per opportunity with its spread, win rate against
observed `F(β)`, liquidator concentration, the in-scope miss rate and coverage
matrix (a profit figure means little without knowing which slice it came from),
infrastructure cost, and the `declined` SYSTEM breakdown, which is a reason to
*continue* and belongs in the same view. One discipline survives: record the
reasoning, not just the outcome, so the next review compares against it rather
than re-arguing from scratch.

## Consequence 1 — an agent must not read this as "remove the RiskGate"

"Manual kill only" and GUIDE 14's "kill switch matrix" were one keyword apart,
and an agent reading D13 could reasonably have torn out the automatic halts.

**D13b records that they stay**, and GUIDE 14 Step 1 is renamed the **halt
matrix**, with every stray "kill switch" across seven files renamed to "halt" so
the two concepts no longer share a word.

The matrix also gets a reframe that changes how to tune it: **these are not
capital controls.** No row prevents a losing trade, because a losing trade is not
reachable. What they protect is **builder reputation** (a sender submitting
consistently reverting bundles gets deprioritised — the real cost of not halting,
and one the PnL ledger never shows), signal quality, and attribution. So tune for
noise suppression, which means a halt firing slightly too often is cheap, and
"nothing bad happened anyway" is a misreading of what it is for.

One exception is named: **the PnL drawdown row is the only one measuring
something `minProfit` does not already prevent.** Per-transaction profitability
does not imply per-month profitability — gas on any non-bundle send, the box, the
operator's time all sit outside the contract's check. That row is the
system-level version, which is why it requires manual re-arm.

## Consequence 2 — D12 inherited D13's job

With no automatic capital control, **the Executor's standing balance is the
entire at-risk amount**, and the only thing bounding it is the sweep cadence.

D12 was downgraded in an earlier round to "a gas question, not a risk budget"
once profit became ETH-only. That is no longer true. It is a risk budget again,
and `k` and the max age should be set with that in mind rather than for transfer
efficiency alone. Recorded on D12 and in the unset-decisions note.

## Consequence 3 — the manual control must be exercised

A manual stop that has never been pulled is not a control. GUIDE 14 acceptance
now requires it drilled **from the phone, timed, on a normal operating day** —
not from the console — with the duration recorded. D13 makes this the one control
with no automatic equivalent.

**Files:** PRE-FLIGHT §4 + §5, GUIDE 14 (Step 1 header + reframe, acceptance),
GUIDE 09 (acceptance ×2, halt wording), GUIDE 02 ×2, GUIDE 06, GUIDE 11,
GUIDE 13, GUIDE 17, ORCHESTRATOR ×2, INDEX ×2, STATE (D12, D13 set, D13b new,
unset-decisions note).

**Still unset:** D11 (bid floor), D12 (sweep `k` — now load-bearing), D15's
address list (gates the sync), D27 (admission threshold), D34 (`p`), D35 (bid
cap/jitter).

---

# Round 19 — Halts narrowed to three classes; bundle-only made absolute

## Bundle-only, no fallback

D19 already said "never the public mempool." One place softened it: GUIDE 13's
"honest fallback" paragraph described broadcasting through your own node if every
builder endpoint were unreachable, advised against it, and left it on the table.

Now closed. **Do not build the path, including as a flag that defaults off** — a
disabled path is one config mistake away from an enabled one. The correct
behaviour when no builder is reachable is to miss the opportunity; an alert on
total unreachability is the whole response.

The reasoning is recorded on D19 because it is load-bearing for everything else:
free reverts are the foundation the safety model rests on. A bundle that loses
its race costs nothing; a broadcast transaction that loses costs gas. Wiring a
public path forfeits the property that makes halts unnecessary as capital
controls — which is the argument the rest of this round runs on.

## Halts: kept, but they now have to earn their place

Removed the phone-drill acceptance criterion (rejected). The matrix itself stays,
reorganised around three admission tests (D13c):

1. **Scoped as narrowly as the fault** — blast radius is the first question
2. **Self-clearing unless code must change** — if the world caused it, the world
   ends it
3. **Cannot fire on a market condition** — a rolling metric crossing a threshold
   in a volatile hour is exactly when the opportunity is largest

Failing test 3 means alert, not halt.

The framing that drives all three: **a halt is not a safety device here.** Funds
are protected on-chain per transaction, so no row prevents a loss. What a halt
actually does is stop us acting on distrusted state — and every halt is code,
code has bugs, and a mis-thresholded halt is a silent machine for losing
opportunities. Nothing alarms when you are not trading.

**Class A — scoped, auto-clearing** (8 rows): node lag, oracle staleness,
MEV-Share disconnect, flash liquidity collapse, provider revert streak, deep
reorg, gas wallet floor, reserve frozen. Node lag is the only global halt, and it
is Class A — it recovers by itself when the node catches up and must never need a
human.

**Class B — scoped, manual, because clearing requires a code change** (3 rows):
proxy implementation changed, drift mismatch, sim-pass/chain-revert divergence.
**Every one halts a single protocol.** A proxy upgrade on one Aave V4 spoke does
not stop Morpho. The test for manual is not "is this serious" but "does clearing
it require code to change?"

**Class C — alert only** (2 rows): gas spend rate anomaly, PnL drawdown. **Both
were global threshold halts and are now neither global nor halts.** This reverses
Round 18, which called drawdown the one row measuring something `minProfit` does
not prevent — true, but it fails test 3, and a rolling-PnL global halt is the
archetype of the bug that shuts the system down during the hour it should be
busiest. They are diagnostics about whether the business works: a question for a
person over a week, not a gate over a block.

## The action-required log (D13d)

Class B halts get their own stream, separate from alerts.

Alerts carry things that happened and mostly resolve themselves. The
action-required log carries only what is **waiting on a change from you**, and
nothing auto-clears out of it — an entry leaves when the code ships.

The reason is triage under load: during a volatile hour the alert stream is busy
and transient, and a proxy upgrade landing in the middle of it is the one thing
that will still be broken tomorrow and the one most likely to scroll past. An
empty action-required log means nothing is waiting on code — checkable in ten
seconds from a phone.

**Files:** GUIDE 13 §1 (fallback closed), GUIDE 14 (Step 1 rewritten, Step 1b
new, Step 2 rewritten, acceptance), STATE (D13b rewritten, D13c and D13d new,
D19 strengthened).

**Still unset:** D11, D12 (sweep `k` — load-bearing), D15's address list,
D27, D34, D35.

---

# Round 20 — GUIDE 14 Step 7: the accounting books

A separate, immutable record of landed bundles for German bookkeeping. Explicitly
**not** the Step 6 PnL ledger: that is mutable analytics, re-derived whenever a
model changes, and correct for that. This must still be readable and unchanged in
eight years. They share no storage, no schema and no code path — merging them
means a schema change made to tune the bid model rewrites a tax record.

## Built as an independent scanner

Per Tucker's suggestion: a separate process watching the Executor's own
transactions on chain, not the searcher emitting rows as it trades. Zero hot-path
cost, and — the better reason — **the record of what the system earned is not
produced by the system that earned it.** Same independence principle as the miss
classifier in GUIDE 05 Step 3a. A searcher-side accounting bug would otherwise
write itself into the books; a scanner reading receipts either agrees or produces
a finding. Reconciled monthly.

## Written at finality

Two epochs, not inclusion. A finalized block cannot be reorged, so the file needs
no reversing entries and stays strictly append-only — which is the simplest
possible way to satisfy GoBD's **Unveränderbarkeit**. A file with negated entries
is defensible in double-entry terms but harder to explain to an auditor, and a
naive sum over it is wrong.

## Two CSVs, monthly, hash-chained

`bundles-YYYY-MM.csv` and `liquidations-YYYY-MM.csv`, joined on `tx_hash`,
children summing to the parent. CSV is flat and a cascade bundle carries ten
liquidations.

**Components, not just net** — gross bonus, flash fee, swap cost, gas used/price/
cost, coinbase bid, net retained. Gas and the builder bid are operating expenses
of the business; a net-only log cannot show they ever existed, and a net figure
can never be decomposed later. Child rows carry raw amounts **and** decimals,
because a raw integer without its decimals is unreadable in five years.

Each row carries `row_hash = sha256(prev_row_hash || fields)`, each closed month a
`.sha256` sidecar. A row altered after the fact breaks every hash after it, so the
file demonstrates its integrity rather than asserting it.

## The EUR rate — there is no Chainlink ETH/EUR feed on mainnet

Checked. Only ETH/USD (`0x5f4eC3Df…5c5b8419`) and EUR/USD
(`0xb49f6779…2C1734C1`) exist there, so the rate is derived as their quotient,
both read at the liquidation's block.

**Both feeds' `answer`, `roundId` and `updatedAt` are recorded, not just the
derived rate.** The derivation is then reproducible by anyone from public data,
and `updatedAt` makes staleness visible — Chainlink posts on a heartbeat plus a
deviation threshold, so "the price at block N" is the last posted price and can be
hours old. Deterministic and verifiable, but it should be visible rather than
folded silently into one number.

**The ECB daily reference rate is recorded alongside it.** Free, one HTTP call a
day, and the conventional source for EUR conversion in German books. Which one the
books use is a Steuerberater question — but only one of those choices can be made
retroactively, and it is the one where both were kept.

## Retention and the Verfahrensdokumentation

**BEG IV**, in force 1 Jan 2025, cut Buchungsbelege retention from ten years to
**eight** — but **Verfahrensdokumentation under GoBD stays at ten**. So the CSVs
are the 8-year artifact and the document describing how they are produced is the
10-year one, and GUIDE 14 Step 7 plus `REGISTRY.md` and the GUIDE 13 submission
path are the technical core of it. The March 2025 BMF Schreiben on crypto is
pointed that software-generated tax reports are no longer accepted uncritically,
which argues for capturing more rather than less.

The books join GUIDE 16 §7's genuinely-unrecoverable set: the node re-syncs, the
state store rebuilds, the archive re-extracts — a month of books does not come
back. A few hundred kilobytes a year with a legal retention period attached.

Every engineering call here runs in the direction of *keeping more, verifiably*,
which leaves the tax questions open for someone qualified to answer them. **None
of it is tax advice.**

**Files:** GUIDE 14 (Step 7 new, acceptance ×6), GUIDE 16 §7, INDEX, STATE (D42).

**Still unset:** D11, D12 (sweep `k` — load-bearing), D15's address list, D27,
D34, D35.

---

# Round 21 — Drift repair: stale references and half-applied edits

Twenty rounds of revision left dangling references and edits applied in one file
but not its neighbours. This round fixes what is mechanically determinable from
the changelog. It adds no design.

## Half-applied edits from later rounds

| Round | What changed | What was left behind |
|---|---|---|
| 18 | D12 became a risk budget again (D13 made the executor balance the only capital control) | GUIDE 14 §5's heading still read *"stopped being a risk budget"*; GUIDE 10 §4 still argued the opposite. Both now say it is one, and GUIDE 10 explains that D29 removed only the *multi-asset* half |
| 19 | Bundle-only made absolute (D19) | `PLAN-ENCODING.md` still said `InterestDrift` *"ships as a plain private transaction"*, three lines below its own table saying one-tx bundle. Now: having nothing to backrun changes the bundle's contents, not its channel |
| earlier | GUIDE 12 identified `min_by_key(effective_cost)` as picking the cheapest exit rather than the best trade; D26 settled it | GUIDE 07 was never corrected. Now `max_by_key(net_of_bonus)` with the D26 reference inline |
| earlier | `CallbackShape` grew to five variants | GUIDE 01's acceptance still said "all four". Now asserts exhaustively via `match` rather than a number that goes stale |

## The repay-blob layout, settled

`PLAN-ENCODING.md` §1c said both swap blobs carry a leading `legCount` byte; §1,
§1b and §2's encoder said the repay blob does not. `Executor.sol` implemented both
readings in different functions — `_swap` per §1c, `_skipSwapLegs` per §1/§1b — one
byte apart.

**EVM execution settled it:** with the §1/§1b/§2 encoding, repay swaps silently
execute nothing (31 of 43 observed groups) or revert (12). §1c is the outlier and
is now corrected, with the failure mode named so it is not re-introduced — it is
silent, and the first leg's `venue` byte decides which way it fails.

## Gates: one name, one duration, one criterion set

The **Shadow** gate had three names (Shadow / Competitiveness / "Stage 2 gate")
and three durations (2–4 weeks / ≥4 weeks / ≥2 weeks) across five files.

Resolved by applying the principle already established for recall in D40: **a
calendar window is a proxy for sample size, so state the sample size.** The bar is
now **≥ 2 weeks and ≥ 200 contested opportunities**, the count being the binding
one — and GUIDE 09 already carried a ≥200 target for the competitor-arrival fit,
so nothing new was invented. Propagated to ORCHESTRATOR, GUIDE 09, GUIDE 15,
PRE-FLIGHT, INDEX and STATE.

Also: INDEX listed **three** gates and omitted Supervised (added in a later round);
it now lists four under the same names ORCHESTRATOR uses. Clearing criteria now
match across all four gates in all four files — previously Correctness's
per-adapter split lived only in GUIDE 04, Recall's ≥50 floor was missing from INDEX
and STATE's gate row, and Supervised's volatile-period requirement appeared only in
GUIDE 17.

STATE's gate rows carried evidence *placeholders* with no thresholds. They now
carry the thresholds, so a row cannot be filled in without meeting a stated bar.

## The prerequisite graph, rebuilt from its own headers

`Blocks` rows were wrong in **13 of 18** guides — GUIDE 05's said "08, 15" while
GUIDE 09 lists 05 as a prerequisite and was absent from it. All 18 regenerated as
the exact inverse of the Prerequisites rows, and verified as such.

INDEX's mermaid diagram had **2 edges that were not prerequisite relations** and
**11 missing**. Rebuilt from the same source, 52 edges.

## The 07 ↔ 12 loop: sequenced, not broken

Not a dependency cycle — the header DAG is clean — but the prose looped and the
orchestration never said how to proceed. `ORCHESTRATOR.md` §3 now carries the
order: GUIDE 07 defines the one-method `RouteCache` interface it needs
(`has_exit`) plus a conservative stub answering from flash-source depth alone,
builds everything that does not consult a route, and **defers its two
route-dependent acceptance criteria to GUIDE 12's checklist.** GUIDE 07 opens with
a note saying so, so the loop cannot be rediscovered as a blocker. The stub
under-admits, which is the safe direction.

## Dispatch

§2 and §3 selected different first tasks and neither was marked as overriding, so
a cold start had four defensible answers. §2 is now explicitly first, with the
cold-start order written out (registry → prune sign-off → sync → GUIDE 00). §3's
condition 3 previously read "no gate between it and the current position is open",
undefined at position zero; it now reads "every gate **upstream** of it is passed",
which is vacuous at GUIDE 00 rather than blocking.

ORCHESTRATOR also tested two fields `STATE.md` does not have — `track_a:
not_started` and `gate_shadow == passed`. Both now name the actual rows, and the
Track-row vocabulary (`not_started`, `n/a`) is explicitly distinguished from the
guide vocabulary.

The opening "Nothing else" contradicted §8's first-session reading list. Now
carries the exception.

## Smaller drift

- **`ArcSwap::from_pointee` is not `const fn`** — the pattern did not compile and
  had been copied into four files. All now use `LazyLock`, with the reason inline
- **Duplicate section numbers:** `PRE-FLIGHT` had two §7 (D38's citation was
  ambiguous — the first is now §6b); GUIDE 05 had two Step 6b (the first is now 6a)
- **Counts that drifted as items were added:** "Three places where an agent stops"
  listed five; "four research findings" listed five
- **`REGISTRY.md §4c`** did not exist — a renumbering survivor referenced from
  `Executor.sol` and GUIDE 00. Both now point at §4
- **`DECLINED.md` and `UNSAFE.md`** are referenced but are repo artifacts the build
  creates, not spec files. Both references now say so
- **STATE's unset-decisions note** listed three and omitted five (D23, D27, D30,
  D34, D35). It now lists all eight and notes that D27, D30 and D35 are all keyed
  to D11, so setting D11 unblocks three
- **STATE's decision table** was out of ID order in five places. Sorted
- **INDEX's file table** listed 10 of 30 files, omitting `PLAN-ENCODING.md` and
  `Executor.sol` — the two that govern the immutable contract. Both added

## Verified after

Fences balanced, every markdown link resolves, `Blocks` rows confirmed as the
exact inverse of `Prerequisites`, 30 files.

**Not addressed here** — these need decisions, not repair: multi-pool flash
sourcing (`AUDIT.md` §1), the GUIDE 12 mathematics (§3), Balancer (§4.1), the five
missing availability formulas (§4.2), and the six protocols with no enumeration
recipe (§6).

---

# Round 21b — The `LazyLock` fix was wrong for a latency system

Round 21 repaired a non-compiling pattern — `ArcSwap::from_pointee` is not
`const fn`, so `static X: ArcSwap<T> = ...` never built — by reaching for
`LazyLock`. That compiles, and it is the wrong answer here.

`LazyLock` and `OnceLock` both put an **acquire load and a branch on every
access**. The branch predicts perfectly after the first hit, so the cost is small
— but it is paid per access, forever, on a path budgeted in nanoseconds, to solve
a startup-ordering problem this process does not have. There is exactly one
startup and it happens before the first block arrives.

**Replaced with an owned `Shared` struct, built once at startup and leaked to
`&'static`:**

```rust
pub struct Shared {
    pub routes: ArcSwap<RouteCache>,
    pub flash:  ArcSwap<FlashIndex>,
}

let shared: &'static Shared = Box::leak(Box::new(Shared { /* ... */ }));

// Hot path: a field offset the compiler keeps in a register, then one atomic
// load. No init check, ever.
let routes = shared.routes.load();
```

Better on three counts, not one: no per-access check; no global mutable state, so
a test builds its own `Shared` and two tests cannot interfere; and the hot path's
dependencies appear in its signature instead of reaching out to a static.

Applied to all four sites — `RUST-CONVENTIONS.md` §3.3, GUIDE 02, GUIDE 07,
GUIDE 12 — and the three guide snippets, which Round 21 left half-rewritten with
the old static names still below the new comment, now read coherently.

## The standing rule, which is the point

§3.3 now carries it in general terms: **a lazily-initialised global is a
per-access check bought to avoid a startup ordering problem.** This process has a
startup phase with nothing to race against, so pay the ordering cost once and hand
the hot path something already built. `LazyLock`, `OnceLock`, `once_cell` or
`lazy_static` anywhere reachable from a hot path means an explicitly constructed
`&'static` was the answer.

Added to §12's quick reference, and to `ORCHESTRATOR.md` §7's never-do list as a
general obligation: **do not fix a compile error with something that costs
hot-path latency.** The first idiom that compiles is not automatically the right
one. If a correctness fix lands on Path A, B or C, the session note says what it
cost and why nothing cheaper worked. GUIDE 16 §5's budgets are not advisory.

Reviewed the rest of Round 21 for the same failure: everything else was
documentation, graph regeneration or gate wording with no runtime effect. The
`min_by_key` → `max_by_key` correction is the same complexity, and GUIDE 07's
`RouteCache` stub is cheaper than the real query it stands in for.

---

# Round 22 — Cache residency: the problem was eviction, not capacity

Started from a proposal to bound the hot working set to L3, then to tile the
universe into L3-sized batches. **Both were solving a problem this architecture
does not have**, and checking that produced the real one.

## Why neither idea applied

**Cache blocking needs reuse.** Tiling is what you do when each element is touched
many times and you want it resident on the second touch — GEMM blocks because every
element of A is used N times. A health recompute is a single pass. No second touch,
so no tile size improves it; the hardware prefetcher already handles a sequential
walk at DRAM bandwidth rather than random-access latency.

**And the working set was never near L3.** GUIDE 08's bands already prevent it:

| Band | Population | Bytes at GUIDE 02's 128-byte rows | Fits |
|---|---|---|---|
| Hot + Warm | ~200 | **~25 KB** | **L1d** |
| + Cool, correlated sweep | ~700 | ~90 KB | L2 |
| Full universe | ~500k | ~64 MB | never scanned per tick |

Cold is 95%+ of the universe and gets a threshold tripwire, not a recompute. A
capacity guard would have been dead code, and a tiling pass would have added work
to something that already fits in L1.

## The actual risk, which is the opposite shape

**The hot band is small. Nothing keeps it resident.** It is touched once per
block; in the 12 seconds between ticks the route-cache builder, simulation
workers, drift sampler and Reth's own database and networking threads all run.
Anything sharing that L3 evicts 25 KB long before the next block arrives, so every
tick starts cold however compact the layout is.

Smallness buys nothing if something else owns the cache between uses. Nothing in
the set guarded against this, and no existing criterion would have surfaced it.

## Three changes

**1. CCX-granular pinning (GUIDE 16 Step 2).** NUMA-node pinning was necessary and
not sufficient: on EPYC, L3 is private per **CCX** (~8 cores), so two threads on
one node but different CCXs share no L3, and two on the same CCX evict each other
freely. The hot path now owns a CCX exclusively — ExEx handler, state store, engine
and oracle fusion, nothing else — and the two L3-hungry background consumers (the
route builder walking the pool graph, the sim workers' revm warm cache) are kept
off it. Core lists come from
`/sys/devices/system/cpu/cpu*/cache/index3/shared_cpu_list`, not from guessing:
core numbering is not contiguous per CCX on every part, so a guessed list often
straddles two. The thread-placement table and acceptance criteria updated.

**2. Cache-residency counters (GUIDE 09 Step 5).** `LLC-load-misses` and
`dTLB-load-misses` for the hot-path thread, via `perf_event_open` on its own fd,
with **the first recompute after block arrival reported separately from steady
state.** That split is the whole value: high first-touch with low steady-state
means eviction and the fix is placement; high throughout means the working set
genuinely grew, a different conversation. A latency histogram alone cannot
distinguish them, and they have opposite fixes.

**3. Pre-warming deferred, deliberately.** Touching Hot+Warm during the
inter-block window is cheap and plausible — microseconds inside a twelve-second
gap, and the next block's timing is known. It is also speculative: if CCX
isolation already keeps the band resident it costs work and buys nothing, and
without the counter you cannot tell which world you are in. Recorded as an option
to measure, not to build. `TESTING.md` §2's rule extends to optimizations — know
where the number came from.

Recorded as **D43** so the placement is not re-litigated, and so the deferral is
visible as a decision rather than an oversight.

## Not changed

No working-set ceiling was added. The existing allocation discipline stands
unaltered — zero allocation on the hot path enforced by a panicking allocator,
startup preallocation sized for the full universe, `SmallVec`/`ArrayVec`, the bump
arena, hugepages, the 128-byte row and GUIDE 02's ≤12-cache-line criterion. That
work was already correct; the gap was residency, not layout or allocation.

---

# Round 23 — Multi-pool cascade, GUIDE-12 math, Balancer out, availability formulas, enumeration recipes

User decisions from the pre-build audit backlog (Round 21 "not addressed"): multi-pool
flash sourcing, GUIDE 12 cost/net-per-gas mathematics, Balancer exclusion, missing
availability formulas, and six protocol enumeration recipes.

## Decisions recorded

| ID | Change |
|---|---|
| D08 | Swap venues: Balancer **excluded**; V3 + Curve (+ Kyber where quoted); V4 still excluded as venue |
| D09 | Flash sources: **Aave, UniV3, UniV4, Morpho** (4 arenas). Provider id `4` reserved/reverting. Fifth arena **TBD** |
| D32 | Clarified: same `debtAsset` may repeat across ≤3 sequential sibling groups for cascade |
| D44 | Multi-source cascade rules (cheapest-first, 99% planable buffer, ≤3, sibling groups only) |
| D45 | Maximise `net_bundle_profit`; fee/impact on seized; pre-gas contribution-per-gas ranking |

## Multi-pool flash sourcing

- **Encoding:** sequential sibling `FlashGroup`s with the same `debtAsset`, different
  `provider`/`flashSource`. Split legs / `repayAmount` across groups.
- **PLAN-ENCODING rule 5 relaxed:** repeated debt assets allowed for cascade only;
  ≤3 groups per debt; duplicate `(provider, flashSource)` for same debt is an error.
  Repay swaps remain per-group (invariant 3). D32 "never nested" unchanged.
- **Executor:** already runs groups in a synchronous for-loop (`T_GROUP` /
  `T_EXPECTED_CALLER` per iteration). **No contract change required** for cascade.
  Balancer path now reverts; id `4` kept so ids `0–3` do not shift.
- GUIDE 07 Step 7 rewritten away from nested callbacks.

## GUIDE 12 mathematics

- Exit **fee and impact charged on seized** ≈ `s·(1+bonus_rate)`, not on repay `s`.
- Objective: maximise `net_bundle_profit`; `searcher_net = net_bundle_profit − bid`.
- Ranking/truncation uses **pre-gas `contribution = swap_out − flash_owed`** in the
  per-gas numerator — fixes double-charging gas that truncated profitable legs.
- Lower band remains per `(ProtocolId, collateral, debt)` from costs vs gas.

## Balancer

Removed/marked out of scope across GUIDE 07/10/12, DEPENDENCIES, INDEX, REGISTRY,
PLAN-ENCODING, Executor.sol, PRE-FLIGHT, STATE. Not a flash source and not a route
venue.

## Availability formulas (GUIDE 07 Step 3a)

- Aave: `balanceOf(aToken)` if flash-enabled/active/unpaused
- UniV3: `balanceOf(pool)`; fee from `pool.fee()`
- UniV4: `balanceOf(PoolManager)`; fee 0
- Morpho: `balanceOf(Morpho)`; fee 0
- Planable split: `floor(avail · 99/100)`; cascade ≤3

## Enumeration recipes (`REGISTRY.md` §3)

Concrete recipes for **Fluid, Euler V2, Silo, Liquity V2, Gearbox, Ajna**, plus a
fleshed Sky/`IlkRegistry` row. GUIDE 15 checklist now requires the recipe.

## Files touched

`PLAN-ENCODING.md`, `GUIDE-07`, `GUIDE-12`, `GUIDE-10`, `GUIDE-01`, `GUIDE-15`,
`GUIDE-05`, `Executor.sol`, `REGISTRY.md`, `DEPENDENCIES.md`, `INDEX.md`,
`PRE-FLIGHT.md`, `STATE.md`, `GUIDE-06` (SVR permissionless + selector note), `RESEARCH-NOTES.md` (new), `CHANGES.md`.

## Still needs human

Fifth flash arena after Balancer removal (stay at 4 vs add Maker/Sky DSS Flash or
other) — listed under STATE blocked.

---

# Round 24 — Sky DSS Flash as fifth flash arena

User decision: add **Maker/Sky DSS Flash** as the fifth flash arena after Balancer
removal (closes the Round 23 `[needs_human]` on D09).

## Provider id

**Id `4` = Sky DSS Flash** (`P_SKY_DSS`). Reclaimed from the Balancer reservation.
Rationale: docs-only stage — no deployed Executor bytecode depended on
revert-on-4; keeping a dead id and introducing `5` would waste a byte and leave
a trap. Balancer stays out of scope with **no** provider id.

## Interface (facts)

- Mainnet Flash: `0x60744434d6339a6B27d73d9Eda62b6F66a0a04FA` (Sky MCD Flash)
- Path used: ERC-3156 `flashLoan(receiver, token, amount, data)` — **DAI only**
- Availability: `maxFlashLoan(dai) == max` [wad] when unlocked / vat live; else 0
- Fee: `flashFee(token, amount)` → **0** on current deployment; runtime lookup
- Callback: `onFlashLoan` → return `keccak256("ERC3156FlashBorrower.onFlashLoan")`;
  approve Flash for `amount+fee` (pull repay)
- `vatDaiFlashLoan` (rad / internal vat) **out of scope** for this bot

## Cascade

D44 unchanged: ≤3 combined sources, cheapest-first, `floor(avail·99/100)`.

## Files

GUIDE-07, PLAN-ENCODING, Executor.sol, GUIDE-10, GUIDE-01, GUIDE-12, REGISTRY,
DEPENDENCIES, INDEX, PRE-FLIGHT, STATE (D09 + blocked cleared), RESEARCH-NOTES,
CHANGES.

---

# Round 25 — Orchestration repair: work packages, acyclic graph, model tiers, PonyTail-HFT

Full read of all 31 files against each other. The build order at guide
granularity was not executable by agents: the header tables form a DAG but the
prose does not, and the previous gate rule blocked code construction on
calendar-length observations. This round replaces the dispatch unit and fixes the
contradictions found on the way. No market commentary.

## What was wrong

**Circular prose dependencies (crate level).** 01's `apply_log` took
`&mut StateStore` (02) while 02 required 01; 03's `catch_unwind` called
`HaltSink` (14); 04's `health()` took `PriceVector` (06) while 06 required 04;
07's eligibility called `RouteCache` (12) while 12 required 07; 09's
`ShadowSubmitter` implemented 13's `Submitter` while 13 required 09's gate; 09's
shadow run needed 12's router while 12 listed 09 as a prerequisite; 10's
acceptance needed 13's encoder; 02's `StateStore.band` was 08's type. An agent
dispatched "GUIDE 03" would stall or invent a type.

**Gates as start barriers.** "Every upstream gate `passed` before a guide
starts" put 06–17 behind the Recall gate, which accumulates over weeks — the
opposite of PRE-FLIGHT §6's schedule where code is built during the waits.

**Stale text surviving earlier rounds.** `clippy::integer_arithmetic` (renamed
upstream; `-D warnings` would fail CI); `Venue::PublicMempool` "for
completeness" vs D19's no-fallback-path; ClickHouse in GUIDE 02/09/14/16 and
DEPENDENCIES vs D06; a warm spare in GUIDE 03 Step 0 vs D05; a "Vault cap" in
GUIDE 17 that no design has; `α` in 12/13/14/17 for a bid model that is `F(β)`;
GUIDE 12 still quoting 07's `min_by_key` as current after Round 21b fixed it;
`D15-ADDRESSES.md` treating `before = 0` as a sync blocker when it is Reth's
keep-everything setting.

## Decisions recorded (STATE.md)

- **D46** Type homes: `PriceVector`/`SourceKind`, `Band`, `Halt*`/`HaltSink`,
  `LogFilter`/`LogSubscriber`, `Submitter`/`IntendedSubmission`, `TraceId` →
  `liq-types`; `StateWriter`, `RouteCache`, `PositionRef`, `MarketRow`,
  `PositionExtraRepr` → `liq-protocol`. `Protocol::apply_log(&mut dyn StateWriter)`.
- **D47** Dispatch unit is the work package. Splits: 00→A–D, 02→A/B, 03→A/B/C,
  04→A/B/C, 05→A–F + W, 06→A-1/A-2/B/C/D, 07→A/B, 08→A/B, 09→A/B, 10→A/B/C,
  12→A-1/A-2/B, 13→A/B, 14→A/B, 15→A-1/A-2/B/C-*/D, 16→A–D, 17→A/B.
- **D48** Executor ships with Aave V3 + V4 + Morpho Blue on-chain adapters; a new
  liquidation ABI later is a `10R-n` redeploy WP.
- **D49** Gates are consumption points: Correctness → 09B/15D per adapter;
  Recall → H4 only; Shadow → 13A and H4; Supervised → H6.
- **D50** `REVIEW_T3` — Sonnet-class reviewer for every Composer build; slug is
  an unset parameter because no Sonnet model is in the workspace's subagent
  list; the coordinator asks, never substitutes.
- **D51** New crates `liq-plan`, `liq-watch`, `liq-books` (18 total) — each
  guide already required the component to be structurally independent.
- **D52** `before = 0` is the safe direction in `receipts_log_filter`; H1 checks
  for missing addresses, not zeros.
- **D53** `clippy::arithmetic_side_effects`, not `integer_arithmetic`.
- **D54** `Venue` has two variants. No `PublicMempool`.

## New / rewritten

| File | Change |
|---|---|
| **`ORCHESTRATOR.md`** | Rewritten. Coordinator/builder session protocol; tracks and gates-as-consumption-points (§3); eligibility, 6-hour claim expiry, ordering by clock-starters then critical path, path ownership, `LANES = 3` (§4); model routing T1 Fable 5.1 → Opus 5 review / T2 Grok 4.6 → Fable review on hot-path code / T3 Composer 2.5 → `REVIEW_T3`, with override rules (§5); builder brief (§6); review protocol with TESTING §7 and mutation checks (§7); evidence format (§8); **PonyTail-HFT** — pre-write ladder, off-the-chopping-block list, builder post-pass, reviewer pass (§9); never-do list (§10); first-session reading order (§11); human checkpoints H1–H7 (§12) |
| **`WORK-PACKAGES.md`** | New. Crate map with the D46 type-home table (§0); Track A/C rows (§1); every WP with spec sections, owned paths, dependencies, tier → reviewer, and the acceptance lines it owns **including criteria deferred from another guide by name** (§2–§7); critical path and lane packing (§8); rule for adding a WP (§9) |
| **`STATE.md`** | Guides table → work-package table with `claimed_by`; tracks renamed to A1–A7 / C1–C5 with the clock each starts; gate rows name their start WP and consumers; D46–D54; `REVIEW_T3` parameter; `[needs_human]` for D50; session log line |

## Guide edits (surgical)

| File | Change |
|---|---|
| GUIDE-00 §1 | Crate list → 18 crates (D51); paragraph on `liq-types` hosting the D46 types; header gains WP row; `arithmetic_side_effects` |
| GUIDE-01 | `apply_log`/`backfill` take `&mut dyn StateWriter`; objective states no `liq-state` dependency; WP row |
| GUIDE-02 | Prereq row states the direction of the 01↔02 dependency; analytics → JSONL + SQLite; WP row |
| GUIDE-03 | Step 0 rewritten: node is a Track A day-0 item, one box, no warm spare (D05/D07); header WP row |
| GUIDE-04 | Prereqs add 06 Steps 2–3 and the coverage audit; header states which acceptance lines belong to 05A/04C |
| GUIDE-05 | WP row incl. `liq-watch` for the watcher |
| GUIDE-06 | Prereqs split: Steps 2–3 before 04, Steps 4–8 after; WP row |
| GUIDE-07 | WP row naming the interim `DepthOnlyRouteCache` and the three criteria owned by 12A-1 / 10C / 12A-2 |
| GUIDE-08 | Prereqs add 05D for the replay acceptance; WP row; `Band` home |
| GUIDE-09 | Header: 12A and 14 do not wait for this guide; the 09↔12 cycle and its resolution stated; `ShadowSubmitter{ClickHouseWriter}` → `ShadowRecorder{JsonlWriter}` implementing the `liq-types` `Submitter` over `IntendedSubmission` (sync, no signing); build-vs-buy drops ClickHouse |
| GUIDE-10 | WP row incl. Morpho Blue on-chain adapter (D48) and `10R-n` |
| GUIDE-11 | Prereqs: 10A/10B not the deploy; pre-H3 bytecode in `CacheDB`; 05B for the replay run |
| GUIDE-12 | Header: 09 only for Step 5/6 fits; WP row; `min_by_key` paragraph now states the corrected `max_by_key(net_of_bonus)` as current; `α` → `F(β)` ×2 |
| GUIDE-13 | `Venue::PublicMempool` removed from the enum and the prose (D54); prereqs add 14A and W; WP row; rollout stage 3 → learning-phase bid / `F(β)` |
| GUIDE-14 | Prereqs: 13 consumes, is not a prerequisite; WP row; ledger → SQLite; `α` → `F(β)` |
| GUIDE-15 | Prereqs: 04 pattern to build, 13/14 live only to enable (D49); WP row |
| GUIDE-16 | Prereqs per step; WP row; phase-2 box C paragraph drops ClickHouse |
| GUIDE-17 | "Vault cap" → the actual blast radius (standing balance between sweeps + gas float); `α` → `F(β)` ×3 |
| RUST-CONVENTIONS §2.1, §11 | `arithmetic_side_effects` |
| DEPENDENCIES §1 | Analytics row → `rusqlite` + JSONL |
| D15-ADDRESSES | `before = 0` reclassified as safe (D52); the real pre-H1 blockers named |
| INDEX | Points at `WORK-PACKAGES.md`; build-order note on lanes and gates; tracks rewritten with H1/A1–A2 ordering; file count 33 |

## Not changed, deliberately

- Acceptance criteria, rounding rules, thresholds (D40/D40b/D41), the halt
  matrix, the bid model, the encoding layout. Only *where* a criterion is
  satisfied moved, never *whether*.
- Effort estimates in the guide headers — they remain developer-hour estimates;
  the WP file does not add a schedule because the binding constraint is the
  calendar (PRE-FLIGHT §6), which the WP graph makes explicit rather than
  re-estimating.
- `AGENT-OPS.md`, `TESTING.md`, `PRE-FLIGHT.md`, `REGISTRY.md`,
  `PLAN-ENCODING.md`, `Executor.sol`: consistent with the new structure as
  written.

## Open for the human

- **D50**: no Sonnet slug available; set `REVIEW_T3` or approve a substitute.
  *(Resolved in Round 26: Opus.)*
- **H1 checklist** additions in `WORK-PACKAGES.md` §1 row C3 (aggregators
  behind proxies, flash sources, `PoolManager`, Morpho singleton, Sky DSS Flash,
  top exit pools) before the sync — a missing line is the unrecoverable case.
  *(Executed in Round 26: `tools/d15_complete.py`.)*

---

# Round 26 — Math re-validation, D15 completion pass, orchestration parameters

User feedback on Round 25: set `REVIEW_T3` to Opus; allow several builders on one
WP; re-validate every formula and threshold (the math was written by a weaker
model); make the dependency lint assert the full forbidden-edge list; make the
D15 filter complete rather than "roughly 2,600 addresses"; and a question on
Executor upgradeability.

## Math — what was wrong

Every formula, constant and acceptance threshold in GUIDES 00–16, PLAN-ENCODING
and TESTING was re-derived. Most held: plan-encoding strides (35/59/77/60), the
bid table (`F(β)(1−β)`), the water-fill bounds, the D40 binomial table, the
EIP-1559 bound, the `$2,435 / $2,460` flash-fee example, the `net(s)` band
decomposition, the availability formulas, the cache-line and L1d arithmetic.
Six did not:

| Where | Was | Problem | Now |
|---|---|---|---|
| GUIDE 00 Step 2 property test | `ray_div_up(ray_mul_down(x,y),y) >= x` | **Inverted.** `ceil(floor(xy/R)·R/y) ≤ x` for integer `x`; fails at `x = 1, y = 1` (gives 0). A builder would "fix" the math to pass it | `div_down(mul_up(x,y),y) >= x` and `div_up(mul_down(x,y),y) <= x`; `U512` cross-check; overflow is `Err` |
| GUIDE 04 Step 4 target-HF solve | `(C − R·(1+b)·price_ratio)/(D − R) = t`, bonus as a fixed point in `R` | **Missing the liquidation-threshold weight** — seized collateral leaves the numerator at `R·(1+b)·LT`, not `R·(1+b)`; result under-liquidates every V4 position. No `price_ratio` belongs there (both sides are already base-currency). Bonus is evaluated at `hf₀`, not post-liquidation `hf` — not a fixed point unless the source says so | Closed form `R = (t·D − C)/(t − (1+b)·LT)`, sign/denominator asserts, dust rule on both sides; `liquidation_price` given in rational form for assets that are both collateral and debt |
| GUIDE 02 Step 3 `MarketRow` | 8 × `Ray` (U256) + 13 small fields, "keep under 128 bytes" | **Unsatisfiable**: ≈ 290 bytes. Two `Ray`s already exceed the whole budget | u128 indices/rates (Aave's storage width), bps-scaled liquidation config; 124 bytes; `const` size assert; cache-line acceptance 12 → ≤ 16 with the count shown |
| GUIDE 12 Step 3b combination search | `std::thread::scope` spawning one thread per combination on the hot path | Thread creation is 15–40 µs; twelve spawns cost more than evaluating twelve combinations serially and break the pinning model | Serial on the hot thread; if 16C shows a need, the pre-spawned pinned sim-worker pool. CI grep-asserts no `spawn`/`scope`/`rayon` in `liq-router` |
| GUIDE 14 Step 5 sweep rule | "~9k gas of a transfer" | That is a native-ETH `CALL{value}`; the residual is **WETH** and the transfer is an ERC-20 write: ~30k in-plan, ~50k standalone. Constant wrong by 3–5× | Both figures stated; rule now `balance > k × transfer_gas × base_fee` |
| GUIDE 13 Step 4b `maxFeePerGas` | flat "+12.5 %" headroom when spanning `maxBlock` | Compounds per block: `1.125^k`; a 3-block span needs +42.4 % or the bundle is invalid in the later blocks | Compounded, rounded up |

Three smaller corrections: GUIDE 07 omitted the UniV3 **100 tier (1 bp)** as a
flash source and V3's `mulDivRoundingUp` on the flash fee; GUIDE 06's sample
feed config had ETH/USD at `heartbeat_secs = 86400` (mainnet is 3600 — a 24×
error that would suppress every heartbeat-driven prediction on the busiest
feed); GUIDE 05 / D40 said "< 30 %" while the binomial table was computed for
"≤ 30 %" (`misses ≤ 15` at n = 50) — the comparator is now stated as the integer
test the code runs, and the table is labelled with it.

`TESTING.md` §4 gains mutations 14–18, one per correction above, so the
regressions are caught by a test rather than a reader.

## D15 — completion pass (`tools/d15_complete.py`)

The Essential draft (2,653 addresses) enumerated markets and receipt tokens and
stopped at the first oracle proxy. Audit of the JSON by class: Aave V3 had 65
sources and **15** aggregators (Capo / synchronicity / SVR wrappers have no
`aggregator()`); Aave V4 had **zero** aggregators; Compound, Morpho, Euler,
Liquity, Gearbox, Silo and Fluid had **no oracle sources at all**; no historical
phase aggregators anywhere; Sky had no Dog, no Clippers, no medianizers; no
flash sources beyond Aave/Morpho; no exit pools; no rate providers.

The completion pass resolves oracle sources **recursively to the OCR aggregator
that emits `AnswerUpdated`**, adds **every historical phase** (`phaseAggregators`
and `FeedRegistry.getPhaseFeed`), enumerates V4 sources through
`getReserveSource(uint256)` (selector verified on-chain — the V4 SpokeOracle is
keyed by reserve id, not asset), all Compound comets and feeds, Morpho market
oracles and IRMs, Euler router adapters per (asset, unitOfAccount) including
collaterals, Gearbox and Silo feeds, Liquity underlying aggregators, Sky's core
+ Dog + Clippers + medianizers + gems, the flash-source singletons, UniV3 /
Curve / Kyber pools for every tracked asset against the hub assets, rate
providers, Pyth. Multicall3-batched; `eth_getLogs` via an RPC that serves it
(publicnode does not). Outputs `*.complete.*` files and `D15-COMPLETION.md`;
the draft files are untouched. **Result: 2,653 → 8,434 addresses, zero
unresolved failures.** Sampled aggregator rows verify as
`AccessControlledOffchainAggregator` / `OCR2Aggregator` on-chain. Headline
deltas: Aave V3 aggregators 15 → 99 (31 current + 44 phases + 24 wrapper
sources), Morpho 1 → 2,094 rows (1,350 market oracles, 616 feeds/phases, 126
vault rate providers), Euler +737 oracle rows, Compound comets 3 → 6 with 74
feed rows, Sky +16 Clippers + Dog/Vat/Jug/Pot/Spotter/Flash, 981 UniV3 + 526
Curve + 20 Kyber pools, 837 tracked ERC-20s. `D15-ADDRESSES.md` now carries the 15-class
definition of "complete" and WP C3 is acceptance-tested against it.

## Orchestration

- **D50 set**: `REVIEW_T3 = claude-opus-5-thinking-high`. The T3 review is a
  polish pass; a reviewer that finds itself rewriting returns `FAIL <re-tier>`.
- **§2 / §4.3**: one WP per builder still holds; a WP may have **several
  builders** as parallel sub-briefs with disjoint owned paths, reviewed as one
  unit. Never one file or one acceptance line across two builders.
- **GUIDE 00 Step 5 / WP 00A**: the dependency lint is now the **full
  forbidden-edge list** (19 edges) plus the no-tokio assert, with a scratch-branch
  job proving the lint goes red. `TraceId` / `stage()` are stated to live in
  `liq-types` (D46).

## D55 — Executor upgradeability (needs human)

Recorded in `STATE.md` with the numbers. Short form: an upgrade **does not save
deploy gas** (the implementation is deployed either way); a proxy costs ~5k gas
on every tx forever; the Executor has zero storage so the usual proxy hazard
does not apply; a new protocol still needs 10A–10C + H3 under a proxy. Options
A (immutable, as is), B (UUPS with authority = `PROFIT_SINK`, no new key), C
(generic legs + settable allowlist). Recommendation A; B acceptable. Nothing
blocks until 10A is dispatched.

## Files

GUIDE-00, GUIDE-02, GUIDE-04, GUIDE-05, GUIDE-06, GUIDE-07, GUIDE-12, GUIDE-13,
GUIDE-14, TESTING, ORCHESTRATOR, WORK-PACKAGES (00A, C3), STATE (D40, D50, D55,
blocked), D15-ADDRESSES, `tools/d15_complete.py` (new), CHANGES.
