# State — Build Progress

**This file is the handoff between agent sessions.** Read it after
`ORCHESTRATOR.md`; update it before the session ends. It is the only thing that
survives a cold start.

Status values: `todo` · `in_progress` · `blocked` · `done`
A `done` with an empty evidence field is treated as `todo`.

Last updated: 2026-09-19 (Europe/Berlin)

---

## Tracks

| Track | Status | Started | Evidence / note |
|---|---|---|---|
| **A1 — OS installed, stripped, TuneD** | `not_started` | — | GUIDE 16 Step 0. |
| **H1 — Prune profile signed off** | `not_started` | — | **Before the sync.** GUIDE 16 Step 0b. Irreversible; needs a human. Record the address list here. `before = 0` is the safe direction (D52) — check for *missing* lines, not for zeros. |
| **A2 — Reth node up** | `not_started` | — | **Only after H1.** `reth download` with `--with-receipts-since` — hours, not days. Never genesis sync. |
| **A3 — Retention verified** | `not_started` | — | Three known liquidations returned by the local node, one at the far end. |
| **A4 — Liquidation watcher live** | `not_started` | — | WP W, then flip. Starts the moment the node is synced and runs **continuously** from then on. **Starts the Recall clock.** |
| **A5 — Drift observation** | `not_started` | — | WP 04C. 7 days minimum per adapter, adapters concurrent. **Starts the Correctness clock.** |
| **A6 — Shadow mode** | `not_started` | — | WP 09B. ≥ 2 weeks **and** ≥ 200 contested opportunities. **Starts the Shadow clock.** |
| **A7 — Supervised live window** | `not_started` | — | WP 13B after H4. ≥10 watched sessions over ≥2 weeks. Submit flag windowed; box is not. |
| **C1 — Registry discovered** | `not_started` | — | `REGISTRY.md` §3. **Enumerate from chain, never hand-list** — V3 is several Pool instances, V4 is 3 hubs + 11 spokes. Record the counts. Public RPC, no node dependency, **day 0**. |
| **C2 — Registry verified** | `not_started` | — | Independent re-derivation + canonical-list identity check. Record both. |
| **C3 — Prune filter generated** | `draft` | 2026-09-18 | D15's address list, **generated from the committed registry** — gates the sync. Draft from `tools/discover_d15_addresses.py` exists; regenerate from C2's output and add the flash-source and aggregator tiers (`WORK-PACKAGES.md` §1). |
| **C4 — Event extraction** | `not_started` | — | WP 05B, from the local log-filtered node. Produces the parquet archive. |
| **C — Archive RPC rented** | `n/a` | — | Contingency only — needed if the prune profile was wrong and re-sync is worse than backfill. |
| **C5 — Market study** | `not_started` | — | `PRE-FLIGHT.md` §2, Q1–Q6. Feeds **H2** (manual continue/stop, D13). Never blocks Track B. |

---

## Gates

| Gate | Status | Evidence |
|---|---|---|
| **Correctness** (started by 04C; consumed by 09B, 15D) | `open` | *(mismatch rate — must be < 1 bp; sample size — must be ≥ 10k; per adapter, V3 and V4 separately; date range — 7 days)* |
| **Recall** (started by A4; consumed by H4 only) | `open` | *(in-scope observations — must be ≥ 50; in-scope miss rate — must be < 30% aggregate **and per coverage row**; total misses and % classified — must be 100%; coverage matrix rows filled / uncovered; `declined` breakdown classified MARKET/SYSTEM/KNOWN; observation start date)* |
| **Shadow** (started by 09B; consumed by 13A, H4) | `open` | *(% preceding winner — must be > 80%; n — must be ≥ 200 contested; date range — ≥ 2 weeks)* |
| **Supervised** (started by 13B; consumed by H6) | `open` | *(all six GUIDE 17 Step 0 rows: sessions — ≥10 with ≥3 volatile, reverts, miss-alarm count, realized-vs-predicted spread, relay, nonce; date range — ≥ 2 weeks)* |

A gate is `open` until the evidence cell holds a number and a date range.

The recall gate's denominator is **in-scope** liquidations, not all of them —
dust, self-liquidations, keepers and loss-making bots are classified out, not
counted against the detector. Threshold 30% (D40). The floor that does not move
is **100% of misses classified**; an unclassified miss could be anything. Floor
of 50 in-scope observations, and the rate is read per coverage row as well as in
aggregate — an aggregate can hide a slice that is 100% missed.
Measured forward off the live watcher, so it needs no archive node. The scope
classifier must use fork calls, never our own state. GUIDE 05 Step 3a.

### Recall coverage matrix

Filled as observations accumulate. A row is `uncovered` until observed; an
uncovered row at gate time is a recorded blind spot, not a pass.

The matrix qualifies the miss rate — it states where the number applies and
where it is unmeasured. An uncovered row is a stated limit, not a failed gate.

| Row | Status | Evidence |
|---|---|---|
| Every enabled protocol instance | `uncovered` | *(instance → first observation block)* |
| Every collateral family | `uncovered` | *(family → block)* |
| Every trigger class | `uncovered` | *(class → block)* |
| Top-decile volatility block | `uncovered` | *(block, realized vol)* |
| ≥ 50 **in-scope** observations (D40) | `uncovered` | *(n in-scope / 50)* |
| Per-row miss rate reported, no all-missed slice | `uncovered` | *(row → misses/observations)* |

---

## Work packages

Dispatch unit is the **work package**, not the guide (`ORCHESTRATOR.md` §1,
`WORK-PACKAGES.md`). Dependencies here are a copy for convenience; **the WP file
is authoritative.** If they disagree, fix this file. `Gate`/`Track` columns name
consumption points that must be `passed`/`started` before the WP is eligible.

`claimed_by` format: `<agent-id> <ISO timestamp>`. A claim older than 6 h with no
commit on the WP's owned paths is expired by the next coordinator (§4.2).

**Parameter `REVIEW_T3`** (D50): `claude-opus-5-thinking-high` — set 2026-09-19.

| WP | Name | Depends on | Gate / Track | Tier | Status | claimed_by | Evidence |
|---|---|---|---|---|---|---|---|
| C1 | Registry discovery | — | — | T3 | `todo` | | |
| C2 | Registry re-derivation + identity check | C1 | — | T2 | `todo` | | |
| C3 | Prune filter from committed registry | C2 | — | T3 | `todo` | | draft exists: `d15_receipts_log_filter.toml` (1533 addrs) — regenerate from C2's registry, add flash sources + aggregators |
| H1 | **Human:** prune profile sign-off | C3 | — | human | `todo` | | |
| A1 | OS install, strip, TuneD | — | — | human+T2 | `todo` | | |
| A2 | `reth download` → `reth.toml` → node | H1, A1 | — | human+T2 | `todo` | | |
| A3 | Retention verification | A2 | — | T3 | `todo` | | |
| 00A | Workspace, lints, CI, dependency lint | — | — | T3 | `todo` | | |
| 00B | Fixed-point Ray/Wad/mul_div | 00A | — | T1 | `todo` | | |
| 00C | Identities + shared types (D46) | 00A | — | T2 | `todo` | | |
| 00D | liq-config + registry boot assertion | 00C, C2 | — | T2 | `todo` | | |
| 16A | Thread map, pinning, allocators | 00A, A1 | — | T2 | `todo` | | |
| 01 | Protocol trait + storage contract + conformance | 00B, 00C | — | T1 | `todo` | | |
| 02A | State store core + undo ring | 01 | — | T1 | `todo` | | |
| 02B | WAL, snapshot, drift scaffold | 02A | — | T2 | `todo` | | |
| 03A | Ingest core (sources, router, decode, dirty, backfill) | 02A, 00C | — | T2 | `todo` | | |
| 03B | ExEx forwarder, hot thread, reorg, containment, mempool | 03A, 02B, 16A | A2 started | T1 | `todo` | | |
| 03C | Event coverage audits (V4, V3) | 00A | — | T2 | `todo` | | |
| W | Liquidation watcher + ground-truth decoder | 00C, 00D | A2 started | T2 | `todo` | | |
| A4 | Watcher live (start clock: Recall) | W | — | T3 flip | `todo` | | |
| 06A-1 | Feed registry + canonical PriceVector | 00C, 00D, 03A | — | T2 | `todo` | | |
| 06A-2 | Derived pricing adapters | 06A-1 | — | T1 | `todo` | | |
| 04A | Aave V4 adapter | 01, 02A, 03A, 03C, 06A-1 | — | T1 | `todo` | | |
| 04B | Aave V3 adapter | 04A, 03C | — | T2 | `todo` | | |
| 05A | Differential fuzz harness | 01, 04A | — | T1 | `todo` | | |
| 04C | **Start drift** (clock: Correctness) | 04A, 04B, 02B, 06A-1 | A2 synced | T3 flip | `todo` | | |
| 05F | Lite validation (smoke) | 04A, 03A, W | — | T3 | `todo` | | |
| 05B | Archive extraction + ground truth (Track C4) | W, 03A | A3 done | T2 | `todo` | | |
| 05C | Recall harness, miss classifier, declines, timing | 05B, 04A, 03A | — | T1 | `todo` | | |
| 05D | Named fixtures, CI, determinism | 05C, 04B | — | T2 | `todo` | | |
| 05E | Swap-math parity | 05B, 00A | — | T2 | `todo` | | |
| 06B | MEV-Share client (stream, matcher, signing, history) | 06A-1, 00C | — | T2 | `todo` | | |
| 06C | CEX feeds, aggregator sim, fusion, ETA | 06A-1, 05B | — | T2 | `todo` | | |
| 06D | Public mempool transmit decoder | 03B, 06A-1 | — | T3 | `todo` | | |
| 07A | Five flash sources | 01, 00C, 03A | — | T2 | `todo` | | |
| 07B | FlashIndex, eligibility, selection, cascade | 07A | — | T1 | `todo` | | |
| 08A | Health engine | 02A, 01, 00C, 07B, 05D | — | T1 | `todo` | | |
| 08B | Trigger sources (governance, derived, stale) | 08A, 06A-2 | — | T2 | `todo` | | |
| 09A | Observability build (recorder, join, alarm, digest, perf) | 08A, W, 05C | — | T2/T3 | `todo` | | |
| 10A | Executor finalisation + callbacks + on-chain adapters | 07A, 04A, 04B | — | T1 | `todo` | | |
| 10B | liq-plan encoder + round-trip proptest | 10A, 00C | — | T2 | `todo` | | |
| 10C | Fork matrix, invariants, gas snapshots | 10A, 10B | — | T2 | `todo` | | |
| H3 | **Human:** Executor review + deploy | 10A, 10B, 10C | — | human | `todo` | | |
| 11 | Simulation | 03B, 02A, 10A, 10B, 05B | — | T2 | `todo` | | |
| 12A-1 | Routing: solver, tiers, band, real RouteCache | 07B, 05E, 03A, 06A-1 | — | T1 | `todo` | | |
| 12A-2 | Profit, selection, assembly, gas oracle, bid structure | 12A-1, 10B, 11, 08A | — | T1 | `todo` | | |
| 09B | **Start shadow** (clock: Shadow) | 09A, 12A-2, 11, 10C | G-C passed per adapter | T3 flip | `todo` | | |
| 12B | Data-gated fits (F(β), p, timing) | 09B ≥ 2 w, 05B | — | T1 | `todo` | | |
| 14A | Risk gate, halt matrix, proxy watcher, caps, treasury, ledger | 07B, 02B, 09A | — | T2 | `todo` | | |
| 14B | Accounting books scanner | H3, 00C | — | T3 (+T1 spot) | `todo` | | |
| 13A | Execution | H3, 12A-2, 06B, 14A, W | **G-S passed** | T1 | `todo` | | **do not start with Shadow open** |
| H4 | **Human:** first live submission | 13A | **G-R passed, G-S passed** | human | `todo` | | |
| 13B | Staged rollout (Track A7) | H4 | — | human+T2 | `todo` | | |
| 15A-1 | Morpho Blue adapter + drift | 04A, 01, 03A, 05A | — | T2 | `todo` | | |
| 15A-2 | Spark (config-only) | 04B | — | T3 | `todo` | | |
| 15B | Mechanism reviews, flashloanability, DECLINED.md | 07A | — | T2 | `todo` | | |
| 15C-* | Tier 2/3 adapters (one row each, add per 15B) | 15B, 15A-1 | — | T2 | `todo` | | |
| 15D | Enable in production (per adapter/source/trigger) | 13B, 14A | G-C for it, recall row, shadow for it | H5 | `todo` | | |
| 16B | Box tuning | A1, 16A | — | T2 | `todo` | | |
| 16C | Benchmark harness + zero-alloc e2e | 05D, 08A, 11, 12A-2, 16A, 09A | — | T2 | `todo` | | |
| 16D | Network | 09A, 13A | — | T2 | `todo` | | |
| 17A | Wiring, supervision, lease, hot-reload | 13A, 14A, 16A, 02B, 00D | — | T2 | `todo` | | |
| 17B | Runbook, drills, keys, backups, reports | 14A, 17A | — | T3 | `todo` | | |
| H6 | **Human:** unattended running | — | **G-U passed** | human | `todo` | | |

### Adapter roster (GUIDE 15)

Each row needs its own drift week before going live.

| Adapter | Built | Drift 7d | Recall | Shadow 2w | Live |
|---|---|---|---|---|---|
| Aave V4 | `todo` | `todo` | `todo` | `todo` | `no` |
| Aave V3 | `todo` | `todo` | `todo` | `todo` | `no` |
| Morpho Blue | `todo` | `todo` | `todo` | `todo` | `no` |
| Spark | `todo` | `todo` | `todo` | `todo` | `no` |
| ~~Compound V3~~ | `deferred` | — | — | — | `no` | 
| *(add rows per GUIDE 15 Step 1)* | | | | | |

---

## Decisions

Settled choices that are **not** re-derivable from the guides. An agent reads
these as given; changing one is a new entry, not an edit.

| # | Decision | Value | Set | Rationale |
|---|---|---|---|---|
| D01 | Chain | Ethereum mainnet only | — | — |
| D02 | Funding | Flashloan only, no inventory | — | — |
| D03 | Language | Rust; contracts in Solidity via Foundry | — | — |
| D04 | OS | Ubuntu 26.04 LTS, generic kernel, TuneD profile | — | GUIDE 16 §0 |
| D05 | Topology | Single box; no warm spare, no archive node | — | Solo operation; outage costs opportunity, not capital |
| D06 | Analytics | JSONL + SQLite + daily digest; no ClickHouse | — | GUIDE 09 §4b |
| D07 | Node | **Log-filtered pruned node**; own node is the replay archive; rented archive is a contingency only | — | GUIDE 16 §0b — supersedes "pruned full node, archive rented once" |
| D08 | Swap venues | V3, Curve (+ Kyber Elastic where quoted). **Balancer excluded**. **V4 excluded** (hooks) | **set 2026-09-18** | GUIDE 12 §3 — Balancer wound down |
| D09 | Flash sources | Aave, UniV3, **UniV4**, Morpho, **Sky DSS Flash** (5 arenas). Provider id `4` = Sky DSS. Balancer out | **set 2026-09-18** | V4 flash kept; Sky DSS is DAI-only ERC-3156 mint; id 4 reclaimed (docs-only) |
| D10 | Swap quoting | `Dex-Math-Core-rs` | — | Exact tick math; split routing needs it |
| D11 | Bid rule | `net − max(floor, 0.01 × net)` | floor = *(set this)* | GUIDE 12 §6 |
| D12 | Sweep threshold | `balance > k × transfer_gas_cost`, k ≈ 20–50, plus a max age | k = *(set this)* | GUIDE 14 §5. **Load-bearing since D13.** With no automatic capital control, the Executor's standing balance is the entire at-risk amount — this is now a risk budget again, not only a gas question |
| D13 | Project abandonment | **Manual only.** No mechanical rule, no threshold that stops the build by itself. The shadow gate still produces the numbers; a human reads them and decides | **set** | `PRE-FLIGHT.md` §4. Capital is protected per-transaction on-chain (`minProfit` + bundle atomicity), so nothing about abandonment is time-critical. Lost opportunity is diagnosable over time |
| D13b | Operational halts | **Kept but narrowed.** D13 is about abandoning the project, NOT about the RiskGate. Halts stay automatic, in three classes (GUIDE 14 Step 1): **A** scoped + auto-clearing, **B** scoped + manual *only because code must change*, **C** alert only. Gas-spend anomaly and PnL drawdown moved to Class C | **set** | An agent reading "manual kill only" must not tear out the RiskGate. A halt is not a safety device — funds are protected on-chain — so each one must earn its place against blast radius, self-clearing, and whether a market condition can trigger it |
| D13c | Halt admission tests | A halt is allowed only if it is (1) scoped as narrowly as the fault, (2) self-clearing unless code must change, (3) incapable of firing on a market condition. Fails (3) → alert, not halt. `Global` scope reserved for node lag | **set** | GUIDE 14 Step 1/2. Every halt is code, and a mis-thresholded halt is a silent machine for losing opportunities — the cost nothing alarms on |
| D13d | Action-required log | Class B halts go to their own log, separate from alerts. Nothing auto-clears out of it; an entry leaves when the code ships | **set** | GUIDE 14 Step 1b. Triage under load — the transient alert stream is exactly where a proxy upgrade scrolls past |
| D14 | Scope | All triggers, all protocols; auction machinery deferred | — | Defer only what is data-gated |
| D15 | Prune profile | history `distance = 10_064`; receipts via `receipts_log_filter` from deployment | address list = **draft** (`D15-ADDRESSES.md` / `d15_*.{json,toml}`) — human review before sync | GUIDE 16 §0b. **Irreversible** — `--full` leaves ~33h of receipts |
| D16 | Live staging | Supervised window before unattended; submit flag windowed, box and watcher continuous | — | GUIDE 17 §0 |
| D17 | Backup posture | Config + keys in git from day one; state-store replication deferred to unattended running; **node DB never backed up** | — | `reth download` is the node backup. GUIDE 16 §7 |
| D18 | Submission path | Direct to **builder** endpoints. The node is **not** on the submission wire; RPC server localhost-bound, ops-only | — | GUIDE 13 §1 |
| D19 | Bundle discipline | Everything ships as a bundle — a single liquidation and a cascade batch of ten alike. Never `eth_sendPrivateTransaction`, **never the public mempool under any condition, and no fallback path is built** (not even flag-gated). Never the liquidation leg in `revertingTxHashes` | — | GUIDE 13 §1. Free reverts are the foundation the whole safety model rests on: a bundle that loses its race costs nothing, a broadcast tx that loses costs gas |
| D20 | Keys | Three: operator, profit-sink, **relay identity** (`X-Flashbots-Signature`). Identity key holds no funds; rotating it resets reputation | — | GUIDE 13 §1, GUIDE 17 §5 |
| D21 | Batching | **Specified, not deferred.** One flashloan, N liquidation legs, per-leg failure tolerance, joint route solve. Single position is `liqCount == 1` | — | GUIDE 12 §4c, `PLAN-ENCODING.md` §1b |
| D22 | Agent operations | Agentic operator run as an experiment; authority tightens as capital comes online | — | `AGENT-OPS.md` |
| D23 | Registry | Derived on-chain, verified three ways, asserted at every boot | *(generate day 0)* | `REGISTRY.md` — distinct from D15; reversible |
| D24 | Token handling | `SafeTransfer` everywhere; no raw ERC-20 `transfer`/`approve` | — | USDT returns no data; `Executor.sol` |
| D25 | Compound V3 | **Deferred**, not declined. Costs the second abstraction test — accepted knowingly | — | GUIDE 15 §1b |
| D26 | Seize selection | Maximise `bonus − exit cost`, not minimise exit cost. Bonus is per-reserve and e-mode dependent | — | GUIDE 12 §4c |
| D27 | Market admission | Permissionless markets admitted on **minimum borrowed**, sized to the viability band's lower edge at a representative gas level — a number, not an allowlist. Bar is meant to fall | threshold = *(set with D11)* | `REGISTRY.md` §3b. Chosen for focus/evidence, **not** safety — `minProfit` already covers the oracle |
| D28 | Viability band | **No cached `min_viable_notional`.** Two-sided band per `(protocol, collateral, debt)`, recomputed per block from the exactly-known next base fee, emitted by the warm tier. `min(spot, twa)` on liquidity and price | — | GUIDE 12 §4b. Base fee is a cost; the bid is a distribution |
| D29 | Profit denomination | **ETH, always.** Exact-output repay leg to the debt asset, everything else to WETH. `minProfit` in wei | — | GUIDE 12 §4d. The bid must be ETH regardless, so the conversion happens anyway |
| D30 | Bid channel | `block.coinbase` transfer computed from **realized** net (`bidBps` on-chain); modest nonzero priority fee alongside; `refundConfig` for SVR | bps = *(set with D11)* | GUIDE 12 §4e. Priority fee is a rate committed pre-swap and cannot absorb a bad quote |
| D31 | Disposal | Batched to a CEX on a cadence + floor; long ETH in between, deliberately | — | GUIDE 14 §5b |
| D32 | Flash groups | **N sequential flash groups**, never nested. Same debtAsset may repeat across ≤3 groups for multi-source cascade (99% planable buffer) | **set 2026-09-18** | Sibling groups, not nested callbacks. `PLAN-ENCODING.md` §1b′ |
| D33 | Candidate selection | **Eligibility is a gate; net-per-gas is an ordering within it.** Ranking never expands the set | — | GUIDE 12 §4f. Failed legs burn gas, which shrinks the bid — self-punishing |
| D34 | Batch sizing | Expected net-per-gas with `p` folded in. **No count threshold** — batches self-regulate as `p` falls | `p` = *(fit from data; start at 1)* | GUIDE 12 §4f |
| D35 | Bid model | `argmax F(β)(1−β)` per ETH bracket **and contest class**, randomised margin, capped below 1. Own wins excluded | cap, jitter = *(set with D11)* | GUIDE 12 §6 |
| D36 | Objective | **Expected profit.** Win rate reported, labelled secondary | — | Stated once so it is not reconciled by guessing |
| D37 | Wallet-funded bid | `payable execute`, `msg.value` as a **ceiling** with refund. Built, default off | — | GUIDE 10 §4d — immutability makes later addition a redeploy |
| D38 | Protocol params | Gas limit from the block header; nothing validator-movable hard-coded | — | `PRE-FLIGHT.md` §7. Glamsterdam is a watch item, not a pre-build |
| D39 | Test discipline | Every assertion names its oracle; four legitimate sources; mutation checklist per guide | — | `TESTING.md`. Defends against tests derived from the code under test |
| D40 | Recall gate shape | Measured **forward** off the live watcher. Denominator is **in-scope** liquidations (GUIDE 05 Step 3a `MissClass`). **In-scope miss rate ≤ 30%** (integer test `misses·10 ≤ 3·n`; at n = 50, `misses ≤ 15` — the comparator the binomial table in GUIDE 05 is computed for), floor **50 in-scope observations**, rate reported **per coverage row** and any all-missed slice root-caused at any n. **100% classified** is a floor that does not move. Classifier uses fork calls only, never our state | **set** | GUIDE 05 Step 3/3a. 30% is defensible because misses cluster by cause; 50 catches a broken system (45% passes 2% of the time) and discriminates poorly at the line, which is the right trade for an opportunity metric |
| D40b | Unattended miss rate | **Same 30%** — no ratchet between proceeding and unattended running | **set** | Monitoring is intermittent, not absent. A miss costs the liquidation and nothing else; it does not compound or endanger capital. The failure modes that *do* compound unattended are in GUIDE 17 Step 0 and D13, not here. **Do not "helpfully" tighten this** — the flatness is deliberate |
| D41 | Decline classification | Every `DeclineReason` carries MARKET / SYSTEM / KNOWN. Band split into `BelowBand` (market) and `AboveBand` (system) | reason-share threshold = 20% | GUIDE 05 Step 3. Declines are liquidations others executed — SYSTEM ones are a defect queue |
| D43 | Cache placement | Hot path owns a **CCX exclusively**, not merely a NUMA node — L3 is per-CCX on EPYC. Sim workers and the route builder kept off it. Hot-path LLC/dTLB miss rates collected, **first-touch-after-block split from steady state**. Pre-warming deferred until that counter justifies it | **set** | GUIDE 16 Step 2, GUIDE 09 Step 5. The hot band is ~25 KB and fits L1d — the risk is eviction across the 12s gap, not capacity |
| D42 | Accounting books | Separate **independent on-chain scanner**, not searcher-emitted. CSV, monthly, append-only, written at **finality**. Hash-chained rows + per-month `.sha256`. Components logged, not just net. ETH/EUR derived from Chainlink ETH/USD ÷ EUR/USD (no direct mainnet feed) with both `roundId`/`updatedAt`, plus the ECB daily rate as a second source | **set** | GUIDE 14 Step 7. Independence: the record of what was earned is not produced by the system that earned it. 8-year retention (BEG IV) for the CSVs, 10 for the Verfahrensdokumentation |

| D44 | Multi-source flash | Cascade only when one source lacks planable liquidity; cheapest-first; take `floor(avail·99/100)` each; **≤3 sources**; encode as sequential sibling groups | **set 2026-09-18** | GUIDE 07 Step 7, PLAN-ENCODING §1b′. Executor already supports; no nesting |
| D45 | Profit objective | Maximise **`net_bundle_profit`**; searcher net = `net_bundle_profit − bid`. Exit fee/impact charged on seized (`s·(1+bonus)`), not on `s`. Ranking uses pre-gas contribution-per-gas | **set 2026-09-18** | GUIDE 12 §4 / §4f |
| D46 | Type homes | Cross-cutting types move **down**: `PriceVector`/`Price`/`SourceKind`, `Band`, `HaltScope`/`HaltReason`/`HaltSink`, `LogFilter`/`LogSubscriber`, `Submitter`/`IntendedSubmission`, `TraceId` → `liq-types`; `StateWriter`, `RouteCache`, `PositionRef`, `MarketRow`, `PositionExtraRepr` → `liq-protocol`. `Protocol::apply_log` takes `&mut dyn StateWriter`. Every crate depends downward only; the dependency lint enforces it | **set 2026-09-19** | `WORK-PACKAGES.md` §0. The guide prose had 01↔02, 03→14, 04→06, 07↔12, 09↔12/13 cycles; each is broken by moving the shared type to the crate with no dependencies |
| D47 | Dispatch unit | **Work packages, not guides.** 06→06A-1/06A-2/06B/06C/06D; 09→09A/09B; 12→12A-1/12A-2/12B; 15→15A-1/15A-2/15B/15C-*/15D; 16→16A–16D; 10→10A/10B/10C; 05→05A–05F; watcher is its own WP (W). A WP row wins over guide prose on signatures and type homes; the guide is edited in the same session | **set 2026-09-19** | `ORCHESTRATOR.md` §1 |
| D48 | Executor ABI policy | `Executor.sol` is immutable. The first deploy carries on-chain adapters for **Aave V3, Aave V4, Morpho Blue**. Any later protocol with a new liquidation ABI opens a redeploy WP (`10R-n`: 10A–10C + H3 again). Batch new families per redeploy | **set 2026-09-19** | GUIDE 10 §6; avoids an early redeploy for the Tier 1 roster |
| D49 | Gates | A gate blocks the **consumption point** whose correctness depends on it, not code construction: Correctness → 09B/15D per adapter; Recall → H4 only; Shadow → 13A and H4; Supervised → H6. Code is built during calendar waits | **set 2026-09-19** | `ORCHESTRATOR.md` §3.1. The old rule blocked 06–17 on a weeks-long clock, contradicting the calendar in PRE-FLIGHT §6 |
| D50 | `REVIEW_T3` | Every Composer 2.5 build is reviewed by **Claude Opus 5** (`claude-opus-5-thinking-high`). Sonnet was the first choice; no Sonnet slug exists in this workspace, and the user chose Opus over a same-model or Grok review | **set 2026-09-19** | `ORCHESTRATOR.md` §5. Opus reviewing a T3 build is over-powered for the code and exactly right for the failure mode — a T3 build's bugs are the ones a fast model *does not notice*, not the ones it cannot fix |
| D51 | New crates | `liq-plan` (encoder, out of `liq-exec`), `liq-watch` (liquidation decoder + watcher, out of `liq-replay`/`liq-obs`), `liq-books` (accounting scanner). 18 crates total | **set 2026-09-19** | Each guide requires the component to be structurally independent of what it checks; a separate crate makes the compiler enforce it. GUIDE 00 §1's list is updated |
| D52 | `before = 0` in `receipts_log_filter` | Means *keep every receipt containing this address's logs from genesis*. It is the **safe** direction, not a blocker — no receipt before deployment can match. H1 checks for **missing** addresses (aggregators, flash sources, PoolManager, Morpho singleton, Sky DSS, exit pools), not for zeros | **set 2026-09-19** | Supersedes the "blocker" wording in `D15-ADDRESSES.md`. Deployment blocks may be filled in later for disk savings; never at the cost of delaying the sync |
| D53 | Lint name | `clippy::arithmetic_side_effects` (the lint `integer_arithmetic` was renamed and is deprecated on current toolchains). `-D warnings` on a deprecated lint name is a CI failure | **set 2026-09-19** | GUIDE 00 §5, RUST-CONVENTIONS §2.1 |
| D54 | `Venue` enum | `PublicMempool` variant **does not exist** in `liq-exec`. Two variants: `MevShare`, `BuilderBundle`. Matches D19 (no fallback path, not even flag-gated) | **set 2026-09-19** | GUIDE 13 §1 previously listed a third variant with a caveat; the enum and D19 now agree |
| D55 | Executor upgradeability | **Proposed, needs human.** Options with numbers: **(A) immutable + `10R-n` redeploy** (status quo, D48). **(B) UUPS behind ERC-1967, upgrade authority = `PROFIT_SINK` (the cold key that already receives every wei; no new key, no timelock).** **(C) generic liquidation legs** (`target, calldata`) against a `PROFIT_SINK`-settable market allowlist, so a new protocol is a Rust-only change. Facts that decide it: (1) **an upgrade does not save deploy gas** — the new implementation is deployed either way (~3–4 M gas ≈ 0.015–0.02 ETH at 5 gwei); `upgradeTo` adds ~30 k on top of that. The saving is address churn only (config, registry, sim baseline — nothing external holds our address or approvals). (2) **a proxy costs ~5 k gas on every tx forever** (cold impl SLOAD 2 100 + cold DELEGATECALL 2 600 + copies) — 0.6–1.4 % of a 350–900 k gas liquidation, ≈ 0.2 % of searcher margin at a 98 % bid. Small, permanent, on the bid. (3) **the Executor has zero storage**, so the classic proxy failure (layout collision on upgrade) does not exist here; `immutable`s, transient storage and the CREATE2 callback checks all work under `delegatecall`. (4) **a new protocol still needs 10A–10C + H3 under B** — upgradeability changes who pays for the deploy, not the review. Only C removes the on-chain review for new protocols, and C costs a contract redesign before 10A plus a mutable allowlist (needed anyway for Euler/Silo's hundreds of vaults; blast radius of a rogue allowlist entry = standing WETH balance, because flash repayment and `minProfit` still bound the tx) and gives up the protocol-specific `_isLiquidatable` precheck (try/catch on the real call costs about the same). Recommendation: **A**, because redeploys are off every critical path (a new adapter waits a drift week regardless), cost ≈ one liquidation's gas, and immutability is what makes the hot `OPERATOR` key's blast radius "wasted gas". **B is acceptable** if a stable address is wanted — its trust assumption is one the system already carries — and is the variant to pick if the user wants upgradeability. C only if the roster grows past what `10R-n` batching handles | **set 2026-09-19: A** (user decision). `Executor.sol` stays immutable, no proxy, no allowlist setter; D48's `10R-n` redeploy path is the upgrade mechanism | Executor.sol header; GUIDE 10 §6; D48. The WP 10A brief states "immutable, no proxy (D55)" so a builder does not add one |

**Unset: D11 (bid floor), D12 (sweep `k`), D15's address list (Essential draft 2,653 → completion pass 8,434 addresses, `d15_receipts_log_filter.complete.toml`; H1 review before sync), D23 (registry,
generate day 0), D27 (admission threshold), D30 (bid bps), D34 (`p`), D35 (bid
cap/jitter).** D27, D30 and D35 are all keyed to D11, so setting D11 unblocks three.
None of the unset values blocks a build WP except D23/C2 (00D).

- **D15 is the urgent one** — it gates the Reth sync, which gates everything, and
  it is the only decision here that cannot be revised later.
- **D12 inherited D13's job.** Manual-only abandonment means the sweep cadence
  is the only thing bounding funds at risk. Set `k` and the max age with that in
  mind, not just transfer-gas efficiency.
- *(D13 set: manual only.)* `PRE-FLIGHT.md` §4 explains
  why.

---

## Blocked / needs human

*(empty)*

Entries here stop the build. Format:

```
- [needs_human] WP <id> — what is blocking, what was tried, what would unblock it
```

---

## Session log

Append one line per session. Newest last.

```
| date | guide/task | outcome | evidence |
|------|------------|---------|----------|
| 2026-09-18 | Docs: multi-pool cascade, GUIDE-12 math, Balancer out, avail formulas, enum recipes | done | CHANGES Round 23; D08/D09/D32/D44/D45 |
| 2026-09-18 | Docs: Sky DSS Flash as 5th arena (provider id 4) | done | CHANGES Round 24; D09 |
| 2026-09-18 | D15 discovery script + first Essential dump | draft | tools/discover_d15_addresses.py; D15-ADDRESSES.md; d15_addresses.json (1533); d15_receipts_log_filter.toml — human review before sync |
| 2026-09-19 | Orchestration repair: WPs, DAG, model tiers, PonyTail-HFT, gates as consumption points | done | ORCHESTRATOR.md rewrite; WORK-PACKAGES.md new; D46–D54; CHANGES Round 25; guide edits listed there |
```
