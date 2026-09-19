# GUIDE 16 — Baremetal Systems Engineering

| | |
|---|---|
| **Crate** | `liq-bot` (runtime config), `ops/` |
| **Prerequisites** | Steps 0/0b: **none — day 0** (A1, H1, A2). Step 3b: GUIDE 00. Steps 2–6: GUIDE 03, 08, 11 (something to tune) |
| **Work packages** | **A1/A2/A3** (Track A, day 0); **16A** thread map + pinning + allocators (with 00, needed by 03B); **16B** box tuning; **16C** benchmark harness + zero-alloc e2e; **16D** network |
| **Est. effort** | 1–2 weeks |
| **Blocks** | 17 |

## Objective

Convert a large dedicated machine into consistent low latency and the compute
headroom to run the full protocol universe without heuristic shortcuts.

Two things matter, in this order. **Consistency beats peak speed** — a p99 of
2 ms with a p50 of 1.5 ms is better than a p50 of 0.4 ms with a p99 of 12 ms,
because auctions are lost on the tail. And **headroom is only useful if you
spend it deliberately** — on breadth and exhaustiveness, not on a lazy hot path.

## Build vs. buy

Buy the OS tuning knowledge; build the affinity layout and the benchmark
harness. Kernel-bypass networking (DPDK / AF_XDP) is the one item to defer until
the Step 6 harness proves the kernel path is your bottleneck — with colocation
already giving sub-millisecond RTT, the remaining kernel overhead is usually
smaller than the software budget above it. Prove it before spending weeks.

---

## Step 0 — OS and kernel

**Recommendation: Ubuntu 26.04 LTS.** Debian 13 is a defensible second. RHEL 10
only if the team already runs RHEL.

| | Ubuntu 26.04 LTS | Debian 13 "trixie" | RHEL 10 (or Alma/Rocky 10) |
|---|---|---|---|
| Kernel | **7.0** | 6.12 LTS | 6.12 (backported) |
| Released | Apr 2026 | Aug 2025 | May 2025 |
| Support | to Apr 2031 (10y w/ Pro) | ~5y | to 2035 |
| Default noise | snapd, unattended-upgrades, cloud-init | minimal | moderate |
| Latency tooling | TuneD (packaged) | TuneD (packaged) | **TuneD native + RHEL for Real Time** |
| Reth upstream target | **primary** | vanilla, fine | least common |

The deciding factor is the kernel. 7.0 versus 6.12 is a full major version, and
the deltas land exactly where this workload lives: NVMe multi-queue, io_uring,
NUMA balancing, and scheduler work (`sched_ext`). On a new server box, hardware
enablement matters too — a 6.12 kernel may not have optimal drivers for current
NICs and NVMe controllers.

Second factor: **Reth is developed and released against Ubuntu.** Matching
production to upstream's primary target eliminates a class of build and glibc
problems that are pure overhead to debug.

RHEL's genuine advantage is `TuneD` — one versioned, reproducible profile
applying CPU governor, C-states, IRQ affinity, THP and sysctl coherently, plus
first-class real-time documentation. **But TuneD is packaged for Ubuntu and
Debian too**, so you can take Ubuntu's kernel and RHEL's tuning framework. That
combination is the actual recommendation. Use a custom TuneD profile checked into
`ops/` rather than a pile of undocumented sysctl edits.

Debian's advantage is minimalism — fewer daemons means fewer sources of jitter on
isolated cores. It is a real argument, but it is ~15 minutes of work to strip
Ubuntu down, and a full kernel version is not something you can add later.

**Strip these on Ubuntu, before anything else:**

```bash
systemctl disable --now unattended-upgrades apt-daily.timer apt-daily-upgrade.timer
systemctl disable --now snapd snapd.socket        # and purge if unused
systemctl disable --now cloud-init motd-news.timer
```

`unattended-upgrades` is the one that matters most. An automatic package upgrade
that restarts a service or triggers I/O mid-auction is a self-inflicted outage,
and it is on by default. **All patching on this box is scheduled and deliberate**
(GUIDE 17).

Use the **HWE / generic kernel, not the low-latency variant**, unless
measurement says otherwise. The low-latency kernel trades throughput for
preemption granularity, which helps interactive audio workloads more than it
helps a pinned, isolated, polling hot path. Measure with the Step 6 harness
before switching; do not assume.

Enable cgroup v2 (default on all three), and keep the root filesystem boring —
ext4 or XFS. Avoid ZFS and btrfs for the node database; copy-on-write plus a
multi-TB append-heavy workload is a latency trap.

## Step 0b — The prune profile is a one-way door

Set this **before the first sync command.** Reth's pruner runs as part of the
pipeline, so whatever the config says to discard is discarded as the node walks
forward. There is no un-prune. Recovering a segment you dropped means a full
re-sync, which is days.

The trap is that `--full` looks like the obvious choice and is wrong for this
build. Its defaults retain account history, storage history **and receipts** for
the last **10,064 blocks** — roughly **33 hours** at 12-second slots. Receipts are
where logs live, so a `--full` node cannot serve `eth_getLogs` beyond about a day
and a half. Every historical liquidation you wanted for the GUIDE 05 replay
archive is already gone, and the day you discover that is the day you need it.

An archive node is the other extreme, and you do not need it either. The replay
harness reconstructs state from **events**, not from historical state tries. So
the segment that must survive is receipts, and the segments you can drop
aggressively are exactly the multi-terabyte ones.

**Do not read the recall gate's independence as permission to skip this.** The
gate is measured forward off the live watcher and blocks on no history at all
(GUIDE 05 Step 3), which removes the deadline but not the reason. Filtered
receipts cost a config line and some disk; they are what turns "a liquidation we
missed last month" from a shrug into a root cause, and they are the only part of
this decision that cannot be made later. Set the filter wide on day 0 precisely
because nothing downstream will remind you.

Reth prunes per segment, and receipts take an address filter:

```toml
# reth.toml — set at provisioning, before sync
[prune]
block_interval = 5

[prune.segments]
sender_recovery   = "full"
account_history   = { distance = 10_064 }
storage_history   = { distance = 10_064 }

# Keep every receipt containing a log from one of these addresses, back to the
# given block; discard all others. This *is* the replay archive.
[prune.segments.receipts_log_filter]
"0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2" = { before = 16291127 }  # Aave V3 Pool
# … one line per tracked market, spoke, and oracle aggregator.
```

`receipts_log_filter` is what makes the single-box plan work. Full log history
for two dozen contracts costs a small fraction of full receipt retention, and
nothing at all next to archive state. Equivalent CLI form:
`--prune.receipts.log-filter <address>:before:<block>`.

Two consequences follow, and both are day-zero:

- **The address list is part of the irreversible decision.** A protocol added to
  the roster in month three has no retained history unless its address was in
  this file on day zero. Put **every** candidate from the GUIDE 15 roster in at
  sync time, including ones you have no intention of building an adapter for. A
  line in this file is free. A re-sync is not.
- **Generate this list from the registry, do not write it.** `REGISTRY.md` §3
  enumerates addresses by walking registry contracts and event logs, because
  "Aave V3 on Ethereum" is several Pool instances and V4 deployed three hubs with
  eleven spokes. A hand-written filter is a transcription of someone's blog post,
  and the thing it omits is gone for good.
- **Oracle aggregators are on the list too.** GUIDE 05's `PriceUpdate` events and
  GUIDE 06's feed history come out of the same receipts. Omit them and you replay
  detection against no prices. List the **proxy and the current underlying
  aggregator** — a feed upgraded mid-window otherwise leaves a hole.
- **Receipt tokens are on the list, and this is the one people miss.** An aToken
  transfer moves a position with **no protocol event** — that is GUIDE 05's
  `collateral_token_transfer` fixture. Leave those addresses out and the entire
  failure class is invisible in replay, which means detection looks clean
  without ever testing it.

### What belongs in the filter, in tiers

This is a log-emitter list, not a list of everything you care about. Addresses
that matter to the *solver* but emit nothing you replay (routers, most token
contracts) belong in `REGISTRY.md`, not here.

| Tier | What | From |
|---|---|---|
| **Essential** | protocol markets, spokes, oracle proxies + aggregators, receipt tokens | deployment block |
| **Useful** | pools you would route through | a shallower block — 6–12 months |
| **Not here** | routers, plain ERC-20s, flash sources you only call | — |

The second tier is a judgement call worth making deliberately. Detection recall
needs protocol and oracle logs only — pool swap events are for reconstructing
what your router *would* have quoted, which trains the bid model (GUIDE 12) but
is not what the recall gate tests. Every swap on mainnet will dwarf everything
else in tier one combined, so take pools shallow and narrow, or skip them and
accept that historical route quality cannot be backtested exactly.

**Verify retention before relying on it.** `eth_getLogs` over wide ranges has
returned silently empty results rather than an error on pruned nodes. Pick three
known historical liquidations — one near the far end of your window — and assert
the logs come back, cross-checked once against a public RPC, while re-syncing is
still a cheap option rather than an expensive one.

### Do not sync from genesis

Genesis sync is days to a week, and **your CPU barely moves the number.** Paradigm
measured ~50 hours to tip on well-specced bare metal and were explicit that "the
dominant factor for performance across hardware is the hard drive," recommending
>16K IOPS. More recent reports on fast hardware with 1 Gbps put it nearer a week
as the chain has grown. Sixty-four threads at 3.6 GHz buys almost none of that
back, because the constraint is random disk I/O and peer throughput, not compute.

Use `reth download` instead. It pulls official public snapshots, with three
profiles (`--minimal`, `--full`, `--archive`) and per-component flags. Public
download runs at roughly 100–300 MB/s, which turns sync into a bandwidth problem
measured in hours rather than a compute problem measured in days.

The component flags are what make this work with the retention plan above. The
log filter governs what the pruner keeps going *forward*; it cannot conjure
history a snapshot never contained:

```bash
reth download \
  --with-receipts-since <EARLIEST_PROTOCOL_DEPLOYMENT_BLOCK> \
  --with-txs-since     <EARLIEST_PROTOCOL_DEPLOYMENT_BLOCK> \
  --with-state-history-distance 10064
```

`--with-receipts-since` is the important one: it puts deep receipt history on disk
at download time, and `receipts_log_filter` takes over from there. The snapshot's
receipts are **range**-based rather than address-filtered, so this is bulkier than
the log-filtered steady state — which is a feature. A protocol added to the roster
later still has history, because you took the whole range rather than only the
addresses you had thought of on day zero.

**Order matters, and getting it wrong is silent.** `reth download --force`
overwrites existing snapshot data "by removing db, rocksdb, static_files, and
reth.toml." Write the prune config first and it is deleted without comment. So:

1. `reth download` with the component flags
2. *then* write `reth.toml` with the prune profile
3. *then* `reth node`

**What the snapshot trades.** You accept someone else's database instead of
re-deriving it from genesis. The trust window is bounded and checkable — verify
the state root at the snapshot block against the canonical chain, and everything
after it is validated normally by your own node. For this system it is a good
trade; make it knowingly rather than by default.

**If you prune retroactively**, `reth prune` has been reported to need running
twice before it actually removes data. Verify the result rather than trusting the
command's exit code.

## Step 1 — Everything is one process, not just one box

The whole stack colocates: Reth, the oracle layer, the engine, the router,
simulation, and execution. But be precise about what "colocated" means here —
with the ExEx architecture (GUIDE 03) **the bot is compiled into the Reth
binary and runs inside the Reth process.** The entire hot path is in-process with
the node — no IPC, no serialization, no loopback.

Three consequences that shape the rest of this guide and GUIDE 17:

1. **Pinning is thread-level, not process-level.** You cannot put "the node" and
   "the bot" in separate cpusets — they are the same process. Core isolation
   works by pinning *threads* (Step 2), which means the code must name and pin
   its own threads explicitly rather than relying on external `taskset`.
2. **A bot deploy is a node restart.** New adapter, new bid parameter, bug fix —
   all of them replace the binary, which stops the node. Reth restarts from disk
   in tens of seconds, not hours, but it is still a window of blindness. Plan
   deploys around it (GUIDE 17) and let the spare hold the lease if you run one.
3. **A bot panic can take down the node.** Catch and contain panics at the ExEx
   boundary. An unwrap in an adapter must degrade to "halt that protocol", never
   to "the node process died."

Provision for the full universe, not the first adapter:

- **Storage**: NVMe. Production runs a **log-filtered pruned node**, not archive
  (Step 0b) — history pruned to 10,064 blocks, receipts retained from deployment
  for the tracked contracts only. A fraction of the multi-TB archive footprint,
  and it still answers every historical query the replay harness asks. Put the
  WAL and snapshots on a separate device from the node database so fsync
  contention does not show up as ingest latency.
- **RAM**: enough for the node's page cache *plus* your entire state store, the
  DEX graph, the flash index and the warm route cache, with room to spare. The
  design assumes everything hot lives in memory.
- **Cores**: enough to dedicate cores to the hot path and still have a pool for
  parallel simulation and background route solving.

## Step 2 — NUMA, CCX and core pinning

On a multi-socket box this is the single largest latency win available.

Because everything is one process (Step 1), this is **thread affinity, not
process affinity**. Name every long-lived thread at spawn and pin it explicitly
from inside the binary — `taskset` on the Reth PID cannot separate your hot path
from the node's own workers.

- **Pin the hot path to one NUMA node.** The ExEx notification handler, state
  store, engine and price vector must share a node. A cross-socket cache miss on
  the position columns costs more than most micro-optimizations in the other
  guides.
- Allocate the state store with a NUMA-aware allocator bound to that node.
  A columnar store spread across two sockets defeats the layout work in GUIDE 02.
- Reserve cores with `isolcpus` at boot, then pin your threads onto them from
  code (`sched_setaffinity` per thread). Keep interrupts off those cores
  (`irqaffinity`).
- Leave Reth's own pools — networking, payload building, database I/O — on the
  non-isolated cores. You are isolating *your* threads from *its* threads inside
  a shared process, which only works if both sides are explicit.
- Put background work — route solving, WAL fsync, telemetry, PnL writes, drift
  sampling — on the *other* node or on non-isolated cores.

### Pin at CCX granularity, not just NUMA node — L3 is not shared node-wide

NUMA-node pinning is necessary and **not sufficient**. On AMD EPYC, L3 is private
to each **CCX** (core complex — typically 8 cores with their own L3 slice). Two
threads on the same NUMA node but different CCXs share no L3 at all, and two
threads on the *same* CCX evict each other's lines freely.

That second half is what matters here, and it is the opposite problem from the
one people usually guard against:

**The hot working set is small. The risk is that it does not stay resident.**
GUIDE 08's bands keep the per-tick recompute to Hot+Warm — roughly 200 positions,
about 25 KB at GUIDE 02's 128-byte rows, which fits in **L1d**. But it is touched
once per block. In the 12 seconds between ticks, the route-cache builder, the
simulation workers, the drift sampler and Reth's own database and networking
threads are all running. Anything sharing that L3 will have evicted 25 KB long
before the next block arrives, so every tick starts cold no matter how compact the
layout is.

Smallness buys nothing if something else owns the cache between uses.

- **Give the hot path its own CCX.** The ExEx handler, state store, engine and
  oracle fusion go on one CCX and nothing else does. Its L3 slice then holds the
  position columns across the inter-block gap because no other thread can touch
  it.
- **Keep the L3-hungry background work on a different CCX** — the route-cache
  builder (which walks the whole pool graph) and the simulation workers (revm
  warm cache) are the two that will flush you.
- Read the topology rather than assuming it: `lscpu -e`, `lstopo`, or
  `/sys/devices/system/cpu/cpu*/cache/index3/shared_cpu_list` gives the exact
  core→L3 grouping. Core numbering is not contiguous per CCX on every part, so a
  guessed core list often straddles two.

Verify with `numactl --hardware` and `lstopo`, and measure, per stage, before and
after. If pinning does not improve p99, something else is dominating and you
should find it before tuning further.

## Step 3 — Memory

- **Hugepages** for the state store and the revm warm cache. Fewer TLB misses on
  a large working set, and the working set is large by design.
- **Pre-allocate everything** at startup: position columns sized for the full
  universe with headroom, undo ring, route cache, flash index. No growth
  reallocation during operation — a `Vec` doubling mid-block is a latency spike
  at exactly the wrong moment.
- **No allocation on the hot path.** This is asserted in GUIDES 01, 02, 03 and 07
  individually; verify it end-to-end here with a counting allocator under a
  replay load.
- Disable swap. A swapped page in the hot path is a lost auction.

## Step 3b — Threads, naming and the allocator

Pinning (Step 2) requires that every long-lived thread is explicitly created,
named and placed. Ad-hoc `thread::spawn` with no name is invisible in `top -H`
and impossible to pin.

```rust
std::thread::Builder::new()
    .name("liq-oracle-fusion".into())
    .spawn(move || { pin_to_core(FUSION_CORE); fusion_loop(); })?;
```

Establish a naming convention (`liq-<crate>-<role>`) and a startup-time core map
in config, so the layout is data rather than scattered constants. Assert at
startup that every expected thread came up on its assigned core, and fail closed
if not — a silently unpinned hot thread is a latency mystery you will spend days
on.

Threads that must exist and be placed:

| Thread | Placement | Notes |
|---|---|---|
| Reth ExEx / hot path | isolated core, node A, **CCX 0** | state, engine, router hot half |
| Oracle fusion | isolated core, node A, **CCX 0** | publishes the price triple buffer |
| CEX feed readers | non-isolated | async, Tokio |
| Simulation workers | isolated cores, node A, **not CCX 0** | one per variant slot (GUIDE 11); revm's warm cache is L3-hungry |
| Route cache builder | non-isolated, node B | background, `arc-swap` publisher; walks the whole pool graph — keep it off the hot CCX |
| Tokio runtime (submission) | non-isolated | GUIDE 13 |
| Telemetry / PnL writer | non-isolated, node B | never blocks trading |
| Drift sampler | non-isolated, node B | reads an `arc-swap` snapshot |

**Global allocator: `mimalloc` or `jemalloc`.** It will not help the hot path,
which does not allocate (`RUST-CONVENTIONS.md` §6), but it measurably helps the
background half — route solving, snapshot publishing, telemetry — and reduces
fragmentation over a process that runs for months.

```rust
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

Swap in the panic-on-allocation allocator behind a test feature flag so the
hot-path allocation assertion can run in CI against the replay harness.

## Step 4 — Spending the headroom

This is the part that changes the *design*, not just the deployment. With
significant compute you can replace heuristics with exhaustiveness:

**Parallel simulation.** Simulate several plan variants concurrently and take the
first that passes: alternative flash sources (GUIDE 07's fallback chain),
alternative repay × seize combinations (GUIDE 12 Step 2), and two candidate
firing points for Aave V4's bonus curve. Wall-clock cost is one simulation;
you have the cores. Give each a dedicated core and a pre-warmed revm instance.

**Wider warm route cache.** Cache every `(collateral, debt)` pair across every
tracked protocol at every size bucket, refreshed more aggressively. Memory is not
the constraint.

**Continuous full-universe revalidation.** Instead of trusting band membership
indefinitely, sweep the entire Cold band on a rolling basis in the background.
Bands remain a cost optimization (GUIDE 08), but with headroom you can verify
them continuously rather than trusting the threshold index alone. This is a
correctness win you could not afford on a small machine.

**Deeper route search.** Raise the exact solver's depth bound and time budget,
since the budget is now set by the auction deadline rather than by CPU.

**More protocols concurrently.** The point of GUIDE 15 is breadth; headroom is
what makes running twelve adapters at full recompute frequency viable.

**What not to spend it on:** do not let headroom justify a lazy hot path. The
columnar layout, the `AssetMask` check, the allocation-free health function and
the threshold index all still matter — headroom multiplies an efficient design,
it does not rescue an inefficient one.

## Step 4b — The three latency paths, measured separately

"End-to-end" is ambiguous until you name the endpoints. Three paths matter and
they have different deadlines; instrument each independently or you will optimize
the wrong one.

| Path | From → to | Budget | Dominated by |
|---|---|---|---|
| **A — block-triggered** | ExEx notification → signed bundle on the wire | **≤ 2.8 ms** | decode, state apply, engine, sim |
| **B — SVR / hint-triggered** | MEV-Share SSE byte arrival → `mev_sendBundle` sent | **≤ 2 ms** | quote, route, sim, sign — candidate already pre-warmed |
| **C — prediction** | CEX tick → pre-warmed plan ready | ≤ 10 ms, no hard deadline | aggregator sim, warm route solve |

Plus three network legs, measured as their own series because you tune them with
geography rather than code: p2p block receipt at your node, SSE hint delivery,
and relay/builder submission RTT (p99, not p50).

A sub-5 ms wall-clock target for Path A including network is achievable and is a
reasonable goal. Path B is the one that converts directly into bid quality — see
Step 5.

## Step 5 — Network and colocation

Colocation with the validator, builder and relay campuses is worth real money
here, and for a less obvious reason than queue position.

**Latency buys decision time, even in a sealed-bid auction.** SVR's auction is
decided on value, not arrival order — but every searcher's clock starts when the
MEV-Share hint reaches them, and the block seals at a fixed wall-clock moment. If
you receive the hint 40 ms earlier than a competitor, you have 40 ms more to
size, route, simulate and price the bid, and you can submit later with more
information. Sub-millisecond proximity to the relay converts directly into a
better-informed bid, not merely an earlier one.

The same applies to block receipt. You run your own Reth node, so blocks arrive
by p2p gossip — being in the same campus as a large share of validators means
your Path A clock starts earlier than a remote competitor's, on every single
block.

And for the non-auctioned triggers in `PRE-FLIGHT.md` §3 — public oracle
backruns, pull-oracle races, pending-swap backruns — it is a straight latency
race, and proximity is the whole game.

Practical requirements:

- Low and **stable** RTT to the Flashbots relay and your builders. Measure p99,
  not p50, continuously. Stability matters more than the mean: a 0.4 ms median
  with a 12 ms tail loses the blocks that matter.
- Persistent connections, pre-established, with aggressive keepalive. Never pay
  TCP+TLS handshake in the critical section.
- Separate NICs or at least separate queues for the CEX feed subscriptions
  (GUIDE 06) and submission traffic, so a burst of market data never delays a
  bundle.
- Redundant upstream connectivity. A single-homed box is a single point of
  failure for the whole operation.
- **Kernel bypass (DPDK / AF_XDP) — measure before building.** With colocation
  giving sub-millisecond RTT, the kernel's contribution is typically well under
  the software budget above it. It becomes worth revisiting only once Path A and
  Path B are consistently inside budget and the network legs dominate p99.

## Step 6 — The benchmark harness

Tuning without measurement is superstition. Build a harness that replays a fixed
archive segment (GUIDE 05) and reports per-stage latency distributions:

```
ingest → state → price → engine → router → sim → sign
```

Requirements:

- Same load every run, so changes are attributable
- Report p50 / p99 / p99.9 per stage — **p99.9 is the number that matters**
- Run it in CI on the production box; fail on regression beyond a threshold
- Re-run after every tuning change, every dependency bump, and every new adapter

Record a baseline before tuning. Most of the wins here are unintuitive and some
"optimizations" will make things worse; only the harness will tell you which.

## Step 7 — How many boxes, and split along which axis

The tempting split is "a big node box and a small liquidator box." **Do not split
there.** It throws away the ExEx, which is the highest-leverage decision in the
whole design.

What separating the bot from the node actually costs:

- **Block and log delivery becomes a network hop.** A busy block's receipts and
  logs are hundreds of kilobytes. Serialize, send, deserialize — realistically
  0.5–2 ms added to Path A, on a 2.8 ms budget.
- **Simulation becomes architecturally hard.** `revm` needs random access to
  state. In-process that is a memory read from the node's own database
  (GUIDE 11). Across a network it is an RPC per storage slot, or you keep a full
  state copy on the bot box — which means running a second node there, at which
  point you have not split anything.
- **Reorg notifications stop being free.** `ChainReverted` / `ChainReorged`
  arrive as ExEx notifications today. Split, and you forward them over the wire
  or infer them from block hashes, which is the fragile path GUIDE 03 exists to
  avoid.

The motivations behind the question are real — deploy isolation, blast radius,
not paying for a huge box twice. Split along a different axis.

### The default: one box

For a solo, self-funded operation, **one box is the right answer** and the rest
of this section is a phase-2 menu. Flashloan-only funding means an outage costs
missed opportunities, not stranded capital, so single-box risk is a real and
reasonable trade rather than a corner cut. You keep the deploy-restart window and
you accept hiccups; write that down once and stop revisiting it.

Two things make the single box work without a second machine:

- **Do not run an archive node.** Configure the production node's receipts log
  filter correctly on day zero (Step 0b) and it *is* your archive for the only
  history this system reads — the logs of the contracts you track. Renting an
  archive RPC drops from a required line item to a recovery path for the case
  where the filter list was wrong and you would rather backfill than re-sync.
- **Keep analytics small** (GUIDE 09 Step 4b): JSONL logs and a SQLite ledger
  rather than ClickHouse. At tens of liquidations a day the heavy stack buys
  nothing and its background I/O is exactly the p99 jitter you are trying to
  avoid.

Everything below becomes worth doing once the bot has generated enough profit to
justify it — not before.

### The three-tier layout (phase 2)

**Box A — production.** Reth + bot as an ExEx. Colocated, tuned per this guide,
`isolcpus` and NUMA-pinned. A **full node, not archive**: steady-state operation
needs only recent state, and pruning saves multiple terabytes.

**Box B — warm spare.** Identical build, full pipeline running, holding **no
submit lease**. This is what actually solves the deploy problem: deploy to B,
verify, hand it the lease, then deploy A. A bot deploy stops being a node restart
(GUIDE 17 Step 1b) and becomes a lease handoff with no blind window — a better
answer to "I don't want deploys restarting my node" than splitting the node out
ever was.

**Box C — ops and data.** Cheap, storage-heavy, no latency requirement, no need
to be colocated. Runs the **replay harness and its parquet archive** (the source
is still box A's log-filtered node, not a second archive node), the SQLite
ledger and JSONL digests (D06 — moving them does not mean upgrading them),
Prometheus and Grafana, CI and builds.
Moving these off A matters more than it sounds: analytics writes and nightly
replay runs are exactly the background I/O that shows up as p99 jitter on a box
you are trying to keep deterministic.

Keep the route-cache builder, simulation workers and CEX feed readers **on box
A** — they are latency-adjacent or feed something that is, and you have the
cores. The rule for box C is "nothing here has a deadline."

### When to add each tier

**Box B (warm spare)** earns its cost when the deploy window starts costing you
real opportunities, or when an outage during volatility would hurt more than the
hardware. That is a revenue-dependent judgement, not an architectural one.

**Box C (ops/data)** earns its cost when analytics volume or replay runs start
showing up in your p99, or when you want CI off the production machine. Note it
does **not** need an archive node even then — a correctly log-filtered production
node (Step 0b) remains the source of history permanently.

A *cold* spare is not a failover — a cold Reth sync is days. It is a rebuild
path, and worth labelling as one so nobody mistakes it for redundancy.

### What actually needs backing up

Less than it looks, and the expensive-sounding part needs it least.

**The node database does not need a backup.** `reth download` (Step 0b) *is* the
backup, maintained by someone else. Losing the box costs a snapshot re-download
measured in hours — the same operation you used to build it in the first place.
Replicating 2 TB+ off-box to avoid re-downloading 2 TB is work for nothing.

**Your derived state store does not need one either**, strictly. It rebuilds from
the node's logs. That rebuild is slow enough to be annoying, so replicating
snapshots and the WAL is worth doing *eventually* — it turns a re-backfill into a
file copy — but it is a convenience, not a safety property, and it is reasonable
to defer it until the system runs unattended.

The **accounting books** (GUIDE 14 Step 7) belong in this category too — a month
of them cannot be reconstructed once the chain data behind it is out of reach and
nobody kept the file. A few hundred kilobytes a year, and a legal retention
period attached. Back them up off-box and versioned from the first landed bundle.

**What is genuinely unrecoverable is measured in kilobytes**, and belongs in git
on day one rather than in a backup plan for later:

- `reth.toml` and its `receipts_log_filter` address list
- the TuneD profile and every sysctl you set
- all bot config — thresholds, enabled protocols, flash sources, the bid floor
- deployed contract addresses and constructor arguments
- `STATE.md` and its decision log

Plus the keys, encrypted and off the box. Losing the operator key does not lose
funds — `PROFIT_SINK` is immutable and swept profit is untouched — but it bricks a
deployed `Executor` and costs a redeploy.

The prioritization that falls out: **config and keys today, state-store
replication when you stop watching daily, node database never.**

---

## Acceptance criteria

- [ ] OS installed per Step 0; `unattended-upgrades`, `apt-daily*` timers,
      snapd and cloud-init disabled — verified by a test that asserts none are
      enabled
- [ ] TuneD profile checked into `ops/` and applied; `tuned-adm active` matches
- [ ] Prune profile (Step 0b) checked into `ops/` and applied **before** sync;
      `receipts_log_filter` covers every GUIDE 15 roster candidate and every
      GUIDE 06 oracle aggregator
- [ ] Retention verified after sync: `eth_getLogs` returns a known liquidation
      from the far end of the intended window, cross-checked once against a
      public RPC
- [ ] Every long-lived thread is named at spawn (visible in `top -H`)
- [ ] Panics at the ExEx boundary are caught and degrade to a protocol halt;
      a fault-injection test proves a panicking adapter does not kill the node
- [ ] Hot path pinned to one NUMA node; state store allocated on that node
- [ ] **Hot path owns a CCX exclusively** — core list derived from
      `/sys/devices/system/cpu/cpu*/cache/index3/shared_cpu_list`, not guessed,
      and no simulation worker or route builder shares it
- [ ] **LLC-miss rate on the first recompute after a block arrives** is measured
      and recorded as a baseline (GUIDE 09). This is the number that tells you
      whether the hot band survived the inter-block gap
- [ ] Dedicated cores reserved via `isolcpus` and pinned per-thread from code,
      not via external `taskset`; interrupts steered away
- [ ] Hugepages enabled for the state store and revm warm cache
- [ ] All hot structures pre-allocated for full-universe cardinality; zero
      reallocation under a replay load
- [ ] End-to-end zero-allocation verified on the hot path with a counting
      allocator
- [ ] Swap disabled
- [ ] Benchmark harness reports p50/p99/p99.9 per stage; baseline recorded
- [ ] p99.9 end-to-end within the GUIDE 12 auction deadline with the **full
      protocol universe** loaded, not just Aave
- [ ] Parallel simulation of ≥ 3 plan variants completes in the wall-clock time
      of one
- [ ] RTT to relay and builders monitored continuously at p99
- [ ] Persistent connections pre-established; no handshake in the critical path
- [ ] Failure posture (Step 7) decided, documented, and tested
- [ ] Snapshots and WAL replicated off-box

## Failure modes

| Symptom | Cause |
|---|---|
| Unexplained multi-second stall, once a day | `unattended-upgrades` or an `apt-daily` timer. Disable them first, before any other tuning. |
| Node process dies on a new adapter | Panic not contained at the ExEx boundary |
| `taskset` on the node PID does nothing useful | Pinning attempted at process level; it must be per-thread from inside the binary |
| p99 far worse than p50 | Scheduler migration, cross-NUMA access, or GC-like reallocation |
| Latency degrades as adapters are added | Structures not pre-allocated for full cardinality |
| Tuning changes have no effect | Not measuring; or the real bottleneck is elsewhere |
| Occasional multi-ms spikes | WAL fsync contending with the node DB on one device |
| Market data bursts delay bundles | Shared NIC queue for feeds and submission |
| Weeks lost to kernel bypass | Optimized a path that was never the bottleneck |
| Outage becomes a multi-day rebuild | Snapshots not replicated off-box |

## Handoff

GUIDE 17 turns this box into a supervised, alertable production system. The
benchmark harness built here becomes the pre-deploy gate described there.
