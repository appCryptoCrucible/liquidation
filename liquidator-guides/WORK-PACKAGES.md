# Work Packages — the build graph

The guides say what to build. This file cuts them into work packages (WPs) whose
dependency graph is acyclic **at the crate level**, assigns each a model tier and
a reviewer (`ORCHESTRATOR.md` §5), and states which acceptance criteria each WP
owns — including criteria a guide lists but a later WP must satisfy.

Conventions: `T1` Fable 5.1 → Opus 5 review · `T2` Grok 4.6 → Fable review on
Path A/B/C code, Grok review otherwise · `T3` Composer 2.5 → `REVIEW_T3`
(D50). `G-C/G-R/G-S/G-U` = Correctness / Recall / Shadow / Supervised gates.
`A*`, `C*` = Track A / Track C rows in `STATE.md`. `H*` = human checkpoints.
"Owns" lists the only paths the builder may touch.

---

## 0. The crate map (D46, D51)

Nineteen crates (D56: the original "18" count omitted `liq-obs`, which WP 09A owns and the dep-lint references). Arrows point at dependencies; every arrow points downward.

```
liq-types        ── ids · Ray/Wad · PriceVector · Band · Halt{Scope,Reason,Sink}
                    LogFilter/LogSubscriber · Submitter/IntendedSubmission · TraceId
liq-config       ── figment load · Validate · ConfigVersion · registry boot assert
liq-protocol     ── Protocol · Health · Quote · BonusCurve · DirtySet · PositionRef
                    MarketRow · PositionExtraRepr · StateWriter · RouteCache
                    FlashRoute/CallbackShape · Archive · conformance harness
liq-state        ── StateStore: StateWriter · undo ring · StateView · WAL/snapshot · DriftDetector
liq-flash        ── FlashSource ×5 · FlashIndex · eligibility · cascade planner
liq-oracle       ── feeds · canonical PriceVector · derived · MEV-Share SSE · CEX · aggsim · governance
liq-engine       ── bands · threshold index · time heap · Candidate/TriggerCause      (no liq-adapters, liq-oracle or liq-obs, ever)
liq-adapters/*   ── one crate per protocol family
liq-node         ── LogSource{ExEx,RpcPoll} · LogRouter · arena decode · DirtyAccumulator · hot thread · mempool overlay
liq-plan         ── BatchPlan encoder · validate() · round-trip proptest          (NEW — was liq-exec/src/plan.rs)
liq-sim          ── revm workers · WarmSet · verify(bundle)
liq-router       ── RouteSolver · warm/exact tiers · band · profit · selection · assembly · gas oracle · bid
liq-exec         ── Submitter impls · templates · NonceAllocator · inclusion watcher
liq-risk         ── RiskGate: HaltSink · halt matrix · proxy watcher · caps · treasury · PnL ledger (SQLite)
liq-watch        ── liquidation-event decoder + streaming/batch consumers            (NEW — GUIDE 05 §2 ≡ GUIDE 09 §4a)
liq-obs          ── tracing sinks · digest · dashboards · perf counters                (depends on liq-types + liq-watch only — D46; omitted from the count, D56)
liq-replay       ── archive · recall/classifier · fixtures · differential fuzz · parity · bench
liq-books        ── accounting scanner, separate binary, finality, CSV hash chain    (NEW — D42 independence)
liq-bot          ── startup wiring · threads/pinning · lease · hot-reload · ExEx registration
```

`liq-watch`, `liq-books` and `liq-plan` are new because each guide requires them
to be **structurally independent** of the thing they check (watcher vs engine,
books vs searcher, encoder vs `liq-exec`'s runtime). Three small crates are the
cost of that independence being enforced by the compiler.

**Type homes that changed (D46)** — the cycle breakers:

| Type / trait | Was | Now | Why |
|---|---|---|---|
| `PriceVector`, `Price`, `SourceKind` | liq-oracle (06) | `liq-types` | 04's `health(pos, &PriceVector)` needs it before 06 exists |
| `Band` | liq-engine (08) | `liq-types` | `StateStore.band: Vec<Band>` in 02 |
| `HaltScope`, `HaltReason`, `trait HaltSink { fn halt(&self, scope, reason) }` | liq-risk (14) | `liq-types` | 03's `catch_unwind` handler and 02's deep-reorg path call it; `liq-risk` implements it |
| `LogFilter`, `trait LogSubscriber { fn subscriptions(&self) -> … }` | implicit in 01/07 | `liq-types` | 03's `LogRouter` is built from *any* subscriber: protocols, flash sources, feeds, rate contracts |
| `trait Submitter`, `IntendedSubmission`, `SubmitReceipt` | liq-exec (13) | `liq-types` | 09's `ShadowRecorder` implements it before 13 exists |
| `TraceId`, `stage()` | liq-obs (00/09) | `liq-types` | every hot-path signature carries it; `liq-obs` provides the sinks |
| `trait StateWriter` | — | `liq-protocol` | `Protocol::apply_log(&self, st: &mut dyn StateWriter, …)`; `liq-state` implements. Removes 01 → 02 |
| `MarketRow`, `PositionExtraRepr`, `PositionRef` | liq-state (02) | `liq-protocol` | `PositionRef` borrows them; 02 depends on 01, not the reverse |
| `trait RouteCache { fn has_exit(&self, coll, size) -> bool }` | liq-router (12) | `liq-protocol` | 07 consumes it; 12 implements it; 07's `DepthOnlyRouteCache` is the interim implementor |
| `BatchPlan` + encoder | liq-exec/src/plan.rs | `liq-plan` | 10's round-trip proptest is the Executor's acceptance; the encoder must exist before 13 |
| liquidation-event decoder | liq-replay + liq-obs | `liq-watch` | one decoder, two consumers, no engine dependency |

---

## 1. Day 0 — Tracks C and A (before any Rust)

| ID | Name | Spec | Owns | Depends on | Tier → Review | Deliverable |
|---|---|---|---|---|---|---|
| **C1** | Registry discovery | REGISTRY §3, §3b, §3c | `tools/registry/discover.py`, `registry/registry.json` | — | T3 → T2 | Every Family A root walked, every Family B factory/event enumerated (recipes in §3), admission on borrowed volume (D27 threshold, or the generous interim), per-address fields derived from chain. **Counts recorded** per protocol: instances, reserves, receipt tokens, aggregators |
| **C2** | Independent re-derivation + identity check | REGISTRY §4a, §4b | `tools/registry/rederive.py`, `registry/verify-report.md` | C1 | **T2 (must not read C1's output before generating)** → T1 | Second generation from chain, diff is empty; every token checked against a canonical list, every market against the protocol's published deployments; report committed |
| **C3** | Prune filter from the committed registry | GUIDE 16 §0b, D15, `D15-ADDRESSES.md` "Completeness" | `ops/reth/reth.toml`, `tools/d15/` | C2 | T3 → T2 | `receipts_log_filter` generated, not hand-listed, and **complete per the 15-class checklist in `D15-ADDRESSES.md`** — the two scripts (`discover_d15_addresses.py`, `d15_complete.py`) are merged into one regenerator under `tools/d15/` and re-run against the committed registry. Acceptance: every oracle source resolves to ≥ 1 `AnswerUpdated`-emitting aggregator **or** is listed under failures with a reason; every proxy's historical phases present; V4 `getReserveSource` enumerated per spoke oracle; Sky Dog + all Clippers + medianizers; five flash-source arenas; UniV3/Curve/Kyber pools for tracked asset × hub; rate providers. Fluid oracles resolved via `VaultResolver` ABI (the completion pass could not). `before = 0` everywhere unless a deploy block is known — the safe direction (D52). Diff against the `*.complete.*` files explained line by line; a class with zero rows is a bug, not a result |
| **H1** | **Human: prune profile sign-off** | ORCH §3.2 | — | C3 | human | Address list reviewed against the GUIDE 15 roster incl. Compound V2 forks and Capo/custom oracle sources; signed in `STATE.md` |
| **A1** | OS install, strip, TuneD, cgroup v2, ext4/XFS, swap off | GUIDE 16 §0 | box | — | human + T2 assist | Verification script from 16B run later; timers disabled now |
| **A2** | `reth download` → write `reth.toml` → `reth node` | GUIDE 16 §0b | box | H1, A1 | human + T2 assist | Order is download → toml → node. `--with-receipts-since <earliest deployment>`. Start ts in `STATE.md` |
| **A3** | Retention verification | GUIDE 16 §0b | `ops/reth/verify-retention.sh` | A2 synced | T3 → T2 | Three known liquidations (one at the far end) returned by `eth_getLogs` on the local node, cross-checked once against a public RPC |
| **C4** | Archive extraction | GUIDE 05 §1 | → WP 05B | A3, W | — | (listed here as a track row; built by 05B) |
| **C5** | Market study | PRE-FLIGHT §2 | `docs/market/` | C4 optional | human / T2 | Non-blocking side track. Feeds H2 only. Never gates Track B |

---

## 2. Foundations (no node needed)

| ID | Name | Spec | Owns | Depends on | Tier → Review | Deliverable / acceptance owned |
|---|---|---|---|---|---|---|
| **00A** | Workspace skeleton, lints, profile, CI, dependency lint | GUIDE 00 §1, §5 (lint), §5b, §6; RUST-CONV §2.1, §11 | `Cargo.toml`, `rust-toolchain.toml`, `deny.toml`, `.github/workflows/ci.yml`, every `crates/*/Cargo.toml`, stub `lib.rs` | — | T3 → REVIEW_T3 | 19 crates stub-build (D56: §0's "18" omitted liq-obs); `[lints] workspace = true`; `arithmetic_side_effects` (not the renamed-away `integer_arithmetic`, D53); `panic = "unwind"`, `overflow-checks = true`; CI: fmt, clippy `-D warnings`, test, deny; the **full forbidden-edge lint from GUIDE 00 §5** (`forbid.txt`: engine→adapters/oracle/obs/router/exec, protocol→state, types→anything, obs→engine/state/router, flash→router, plan→exec, watch→state/engine, books→exec/risk/state) plus the no-tokio assert on hot-path crates, with a scratch-branch job proving the lint **goes red**; CI builds in the D04 OS container |
| **00B** | Fixed-point `Ray`/`Wad`/`mul_div` | GUIDE 00 §2; TESTING §3 fixed-point | `crates/liq-types/src/fixed/` | 00A | **T1 → Opus** | No un-rounded op in the public API; 512-bit `mul_div(a,b,denom,Rounding)`; all `checked_*`; proptests 10k **biased to HF ∈ [0.995, 1.005]**, the four properties in §2; mutation #1 goes red |
| **00C** | Identities + shared contract types | GUIDE 00 §3, §5 (`TraceId`); D46 table above; GUIDE 06 §1 (`SourceKind`), GUIDE 08 §1 (`Band`), GUIDE 14 §2 (`HaltScope`), GUIDE 13 §1 (`Submitter`) | `crates/liq-types/src/{ids,price,band,halt,subscribe,submit,trace}.rs` | 00A | T2 → T1 | `AssetId` global (test: WETH resolves to one id from two protocol configs); `PriceVector` flat `Vec<Price>` by `AssetId`, cheap `Clone`; `PriceTick { asset, price, source: SourceKind, block, ts }` and `ScheduledParamChange` (the two event types `liq-oracle` emits into the hot path, so `liq-engine` never depends on `liq-oracle`); `Band` incl. `Unfundable`; `HaltSink` trait; `LogSubscriber` trait; `Submitter` trait taking `&IntendedSubmission` (plan bytes, bid, venue, deadline, `TraceId`) — no signing type; `TraceId` + `stage()`. No logic beyond constructors |
| **00D** | `liq-config` + registry boot assertion | GUIDE 00 §4; REGISTRY §2, §4c, §5 | `crates/liq-config/`, `config/` layout, `registry/schema.json` | 00C, C2 | T2 → T2 | `Validate` trait; `ConfigVersion` hash logged; figment env overrides; **boot assertion re-reads `decimals/symbol/token0/token1/fee` for every entry and refuses to start on any mismatch**; test corrupting one token's decimals fails startup; token quirks loaded |
| **16A** | Thread map, pinning, allocators | GUIDE 16 §3b; RUST-CONV §6 | `crates/liq-bot/src/{threads,alloc}.rs`, `config/cores.toml` | 00A, A1 (topology read) | T2 → T2 | `liq-<crate>-<role>` naming; core map from `shared_cpu_list`, not guessed; `pin_to_core`; startup assertion every thread on its core, fail closed; `mimalloc` global; `PanicOnAlloc` behind `--features alloc-assert` (the one sanctioned `unsafe impl GlobalAlloc`, test-only, recorded in `UNSAFE.md`) |

---

## 3. The correctness spine

| ID | Name | Spec | Owns | Depends on | Tier → Review | Deliverable / acceptance owned |
|---|---|---|---|---|---|---|
| **01** | Protocol trait + storage contract + conformance | GUIDE 01 all; GUIDE 02 §3, §4 (types only); GUIDE 07 §1 (`CallbackShape`); D46 | `crates/liq-protocol/` | 00B, 00C | **T1 → Opus** | Trait per §2 with `apply_log(&self, &mut dyn StateWriter, …)`; `Health`/`HealthState` incl. `SoftLiquidating`; `Quote` with `repay_options`/`seize_options` sets; `BonusCurve` never scalar; `DirtySet`; `PositionRef`, `MarketRow` (< 128 B, `#[repr(C)]`, field order by access frequency), `PositionExtraRepr` via `bytemuck` with `const` size assert; `FlashRoute`, `CallbackShape` (5 variants, matched exhaustively); `StateWriter`; `RouteCache { has_exit }`; `Archive`; conformance harness (10 checks) passing against a **real minimal implementor** (the V4 adapter's skeleton is the first — no fabricated adapter). All GUIDE 01 acceptance lines |
| **02A** | State store core | GUIDE 02 §1, §2, §5, §6, §7b | `crates/liq-state/src/{interner,store,undo,view}.rs` | 01 | **T1 → Opus** | Per-position columns (`AssetMask` first) + per-market flat `Vec<u128>` with stride (deviation accepted — forced by `PositionRef` slice contract, not chosen); `impl StateWriter`; undo ring `Box<[BlockUndo; K]>` pre-reserved; `undo(apply(x)) == x` proptest over random sequences incl. `Created`; deep-reorg returns `ReorgTooDeep`, never partial; `StateView` + `Overlay`; grep-assert no `Arc/Mutex/RwLock/atomic` in the definition; ≤ 16 cache lines (re-derived floor 15; GUIDE-02 headline is ≤16, the ≤12 was the bottom of the 12–15 derivation and is superseded) / 1,000 positions < 50 µs (measured 24 µs, 2× headroom) / 128-block unwind < 5 ms (measured 1.8 ms) measured; mutation #8 red |
| **02B** | WAL, snapshot, snapshot publish, drift scaffold | GUIDE 02 §7, §7b (snapshot), §8 | `crates/liq-state/src/{wal,snapshot,drift}.rs` | 02A | T2 → T1 | WAL fsync off-thread bounded lag; `memmap2` snapshot, cold start 50k positions < 1 s; `Shared.snapshot: ArcSwap<StoreSnapshot>` (built at startup, `Box::leak`, never `LazyLock`); `DriftDetector` stratified sampling issuing `health_probe()` off-thread against a snapshot, EWMA per protocol, emits `HaltReason::DriftMismatch` to `HaltSink` |
| **03A** | Ingest core: sources, router, decode, dirty set, backfill | GUIDE 03 §2, §4, §4b (arena, accumulator), §5 | `crates/liq-node/src/{source,router,decode,dirty,apply,backfill}.rs` | 02A, 00C | T2 → T1 | `trait LogSource` with two **real** impls: `ExEx` (03B) and `RpcPoll` (`eth_getLogs`, used by 05F lite validation, backfill and 05B extraction — the same fold, different feed); `LogRouter` from all `LogSubscriber`s, hash + jump table, `sol!` decoders into a `bumpalo` arena; `DirtyAccumulator` collapsing Reprice ⊃ Accrual ⊃ Positions, buffers reused; zero allocation per log (counting allocator); backfill → pinned block → snapshot |
| **03B** | ExEx forwarder, hot thread, reorg, containment, mempool | GUIDE 03 §1, §4b, §6; RUST-CONV §2.5, §5.1 | `crates/liq-node/src/{exex,hot,reorg,mempool}.rs`, `crates/liq-bot/src/exex_install.rs` | 03A, 02B, 16A | **T1 → Opus** | Thin ExEx; `rtrb` both ways, owned payload; `FinishedHeight` only after the hot thread confirms; hot thread pinned via 16A; `ChainReverted/Reorged` → undo ring; `catch_unwind(AssertUnwindSafe)` → unwind partial block → `HaltSink::halt(Protocol)`; mempool `rtrb` 4096 drop-oldest counted; **acceptance needs A2**: survives sync to tip, injected reorg depth 1/8/64 byte-identical, kill -9 mid-block no gap/no double-apply, panicking adapter keeps the node up, p99 block→consistent < 500 µs |
| **03C** | Event coverage audits — Aave V4, Aave V3 | GUIDE 03 §3; GUIDE 04 §1 | `docs/coverage/aave-v4.md`, `docs/coverage/aave-v3.md` | 00A | T2 → T1 | Every state-changing path enumerated **from the contract source** (`aave/aave-v4`, `aave-v3-origin` ≥ 3.5) incl. receipt-token `Transfer`, risk-premium updates, position-manager actions, proxy upgrades; each row names the `DirtySet` it maps to; referenced by CI |
| **W** | Liquidation watcher + ground-truth decoder | GUIDE 09 §4a; GUIDE 05 §2 | `crates/liq-watch/` | 00C, 00D, A2 | T2 → T1 | One decoder for every tracked protocol's liquidation events (from the registry); **two consumers**: streaming (localhost RPC subscription at the tip → SQLite/JSONL row per event with **raw decoded fields + block hash** and coverage dims: instance, collateral family, trigger class, block realized-vol) and batch (→ parquet `ActualLiquidation` incl. `inferred_bid`, `oracle_backrun`); depends on **no** `liq-state`/`liq-engine`; runs as its own process from A2 onward (**A4**); the engine join and alarm are added by 09A |
| **06A-1** | Feed registry + canonical `PriceVector` | GUIDE 06 §2, §3, §7b (fusion publish only), staleness | `crates/liq-oracle/src/{feeds,canonical,publish}.rs`, `config/feeds/` | 00C, 00D, 03A | T2 → T1 | Per `(protocol, market, asset)` TOML incl. `svr_aggregator`/`standard_aggregator`; **startup validation fails closed** when the protocol's configured oracle ≠ registry (test by mutation); aggregator `AnswerUpdated` via `LogSubscriber`; `PriceVector` published by `triple_buffer` (hot-thread reads wait-free); staleness at `heartbeat × 1.5` → `HaltSink` asset-scoped |
| **06A-2** | Derived pricing adapters | GUIDE 06 §7 | `crates/liq-oracle/src/derived/` | 06A-1 | **T1 → Opus** | wstETH (`stEthPerToken`), sDAI (`chi`), LRT rate providers, capped oracles, LP fair value, cross-rate composition with the protocol's rounding order; each subscribed to its rate contract via the log stream; emits `SourceKind::Derived`; **≤ 1 bp vs the protocol's oracle contract on a fork for every LST/LP in the registry** |
| **04A** | Aave V4 adapter | GUIDE 04 §1–§7 | `crates/liq-adapters/aave-v4/`, `docs/coverage/aave-v4-rounding.md` | 01, 02A, 03A, 03C, 06A-1 | **T1 → Opus** | Rounding table per arithmetic step **from source**; hub/spoke → `MarketRow` with `hub_ref` fan-out; `health()` iterates `AssetMask`, includes accrued premium, allocation-free, p99 < 2 µs; `quote()` target-HF solve (closed form or ≤ 3 Newton) + dust rule; `BonusCurve::HealthLinear` populated, never evaluated; `liquidation_price` round-trip 1 ulp / 10k; `time_to_cross` integrates base + premium; `apply_log` + `UndoOp` for every path in 03C incl. position-manager flows; `health_probe`; conformance suite passes. **Gate criteria (drift week, 30-day HealthWrong = 0, 100k differential) belong to 04C and 05A/05C, not this WP** |
| **04B** | Aave V3 (3.5+) adapter | GUIDE 04 §8 | `crates/liq-adapters/aave-v3/` | 04A, 03C | T2 → T1 | e-mode, isolation + ceilings, siloed borrowing, grace period, 3.3+ deficit; nothing hard-coded that Spark (15A-2) would need to change; conformance passes |
| **05A** | Differential fuzz harness | GUIDE 05 §6; TESTING §3 adapter health | `crates/liq-replay/src/diff/`, `contracts/test/differential/` | 01, 04A | **T1 → Opus** | Foundry state generator **biased to HF ∈ [0.995, 1.005]**, per spoke config / per e-mode and isolation; Rust `health()` vs on-chain view via FFI or revm; 100k cases zero mismatches is **04's** acceptance run here; "passes first time on uniform inputs" is a generator bug by definition |
| **04C** | **Start the drift observation** (Track A5) | GUIDE 04 §7; GUIDE 02 §8 | `config/drift.toml` | 04A, 04B, 02B, 06A-1, A2 | T3 flips on; human notified | Read-only, continuous, V4 and V3 sampled separately, stratified; start ts in `STATE.md`. **G-C** = 7 days, < 1 bp, ≥ 10k positions per adapter. A breach reopens 04A/04B as top priority; the week restarts |
| **05F** | Lite validation (smoke) | GUIDE 05 §0 | `crates/liq-replay/src/lite.rs` | 04A, 03A (`RpcPoll`), W | T3 run → T2 read | Bounded universe over a public RPC, forward, bidirectional match against W; every "liquidated, never flagged" root-caused; **clears nothing**; may run before A2 |

---

## 4. Replay, recall, and the calendar instruments

| ID | Name | Spec | Owns | Depends on | Tier → Review | Deliverable / acceptance owned |
|---|---|---|---|---|---|---|
| **05B** | Archive extraction + ground truth (Track C4) | GUIDE 05 §1, §2 | `crates/liq-replay/src/archive/`, `data/archive/` | W, A3, 03A | T2 → T2 | Local `eth_getLogs` over the filter set → partitioned parquet of **decoded** events + `PriceUpdate` + reorgs; verified against a second source on two known liquidations (far end + middle); `ActualLiquidation` set via W's batch consumer with `inferred_bid`; one month replays in < 10 min |
| **05C** | Recall harness, miss classifier, decline classes, timing | GUIDE 05 §3, §3a, §4; TESTING §3 recall | `crates/liq-replay/src/recall/` | 05B, 04A, 03A | **T1 → Opus** | `RecallReport`; `MissClass` decided **only** from the event's fields + fork calls at N−1 + declared config — a grep/test asserts the classifier module imports neither `liq-state`, `liq-flash` nor `liq-router`; `DeclineReason` with MARKET/SYSTEM/KNOWN and `BelowBand`/`AboveBand` split; per-coverage-row rate; coverage matrix rows persisted to `STATE.md`'s table format; timing harness (block delta, intra-block delta). Mutation #13 is a review item here |
| **05D** | Named fixtures, CI, determinism | GUIDE 05 §5, §7 | `crates/liq-replay/fixtures/`, `.github/workflows/replay.yml` | 05C, 04B | T2 → T2 | All 14 fixtures pinned with one-line real-world comments; replay bit-identical twice; fixtures every commit; full replay nightly and before any dependency bump; `RecallReport` published as artifact |
| **05E** | Swap-math parity | GUIDE 05 §6b | `crates/liq-replay/src/parity/` | 05B, 00A | T2 → T1 | `Dex-Math-Core-rs` vs revm on real pool bytecode for V2, V3, Curve StableSwap, Kyber Elastic, over recorded states, **biased to traded notionals**; wei divergence = bug until proven; V4 excluded (no hook is generic) |

**G-R (Recall)** is measured forward by W + 05C from A4 onward and is consumed
only by H4/13B. It is not a prerequisite of anything in §5–§6.

---

## 5. Detection

| ID | Name | Spec | Owns | Depends on | Tier → Review | Deliverable / acceptance owned |
|---|---|---|---|---|---|---|
| **06B** | MEV-Share client (stream, matcher, request signing, history) | GUIDE 06 §4 | `crates/liq-oracle/src/mevshare/` | 06A-1, 00C | T2 → T1 | SSE reader with backoff, `:ping`, ≥ 24 h soak; matcher works with **partial hints** (`to` first, selector `6fadcf72` when present, price from `callData`/`logs` else predicted); `mev_sendBundle` body **serialisation + `X-Flashbots-Signature` signing**, validated end-to-end on Sepolia; `/api/v1/history` reader joined to W's ground truth over ≥ 30 days. Sending on mainnet is 13A's, not this WP's |
| **06C** | CEX feeds, aggregator simulator, fusion, ETA calibration | GUIDE 06 §6, §7b, §8 | `crates/liq-oracle/src/{cex,aggsim,fusion}.rs` | 06A-1, 05B | T2 → T1 | Raw websockets, one per venue; `crossbeam` bounded MPSC → fusion thread (drop + count on full); `AggregatorSim` per feed, `&mut`, thread-local; `estimate_eta` fitted from archive `transmit` history, not a constant; `PendingUpdate` → pre-warm only; ≥ 90 % of `transmit`s predicted within one block on 30 days of replay; `PriceOracle` fusion serving canonical + best-forward + staleness |
| **06D** | Public mempool `transmit()` decoder | GUIDE 06 §5 | `crates/liq-oracle/src/mempool_oracle.rs` | 03B, 06A-1 | T3 → REVIEW_T3 | OCR calldata → median observation → `SourceKind::PendingPublic`, consuming 03B's ring |
| **07A** | Five flash sources | GUIDE 07 §1–§3a | `crates/liq-flash/src/sources/` | 01, 00C, 03A | T2 → T1 | `FlashSource` for Aave V3/V4 pools (per Pool instance, `FLASHLOAN_PREMIUM_TOTAL` runtime), UniV3 (per pool, fee tier), UniV4 `PoolManager`, Morpho singleton, Sky DSS (DAI only, `maxFlashLoan`, `flashFee` runtime); availability formulas from §3a; `subscriptions()` for every update trigger; **no RPC polling** (provider that panics on network access); availability within 1 % over 1,000 sampled blocks per source, sampled live; adding a source touches one file |
| **07B** | FlashIndex, eligibility, selection, cascade planner | GUIDE 07 §4–§7; D26, D44 | `crates/liq-flash/src/{index,eligibility,select,cascade}.rs` | 07A | **T1 → Opus** | Flat `by_asset` `SmallVec<[SourceEntry; 6]>`, `available()` < 100 ns no alloc; `Shared.flash: ArcSwap<FlashIndex>` republished only on material change; **`DepthOnlyRouteCache: RouteCache`** — answers `has_exit` from flash-source depth of the collateral alone (a stated conservative rule, under-admits; replaced by 12A-1); eligibility over **all** `repay_options × seize_options`, two-sided, `max_by_key(bonus − exit_cost)`; `Unfundable` bitset, mark never delete, flips back (fixture); ranking by effective total cost (V4 over Aave at equal depth, Aave over shallow V4); cascade planner emitting ≤ 3 sibling groups, `floor(avail·99/100)`, never nested. **Deferred out of this WP**: route-depth fixture + re-evaluate-on-route-change → 12A-1; `gas_overhead` measured per provider → 10C; fallback re-encode on sim liquidity failure → 12A-2 |
| **08A** | Health engine | GUIDE 08 §1–§6 | `crates/liq-engine/` | 02A, 01, 00C, 07B, 05D | **T1 → Opus** | Bands as cost optimisation only; `ThresholdIndex` sorted `Vec`s per global asset, margin at HF 1.03, correlated-move sweep at ≥ 3 assets, lazy re-registration; `TimeCrossHeap` with generations; `on_price_tick` — `Predicted` pre-warms only, announced fires; `Candidate` carries `Quote` intact with `BonusCurve`; `TriggerCause` full set; bounded priority channel dropping lowest value, counted; `crossed()` borrowing iterator; O(log N + k) benchmark at 10k/100k/1M; Hot+Warm recompute < 20 µs; tick → candidate p99 < 400 µs; **replay (05D) shows no new `InScope` miss with bands on**; no `Arc<Mutex<_>>` anywhere in the crate |
| **08B** | Trigger sources: governance, derived-rate, stale | GUIDE 08 §5 (`ParamChange`, `DerivedRate`, `Stale`) | `crates/liq-oracle/src/governance.rs`, `crates/liq-engine/src/triggers.rs` | 08A, 06A-2 | T2 → T1 | Timelock poller (async, `liq-oracle`) → scheduled `ParamChange` with the precomputed crossing set at the execution block; `DerivedRate` fan-out from 06A-2 ticks; `Stale { liquidatable_since }` after N blocks untaken |
| **09A** | Observability build | GUIDE 09 §1–§3, §4 (recorder), §4a (join + alarm), §4b, §5 | `crates/liq-obs/`, `ops/digest/`, `ops/dashboards/` | 08A, W, 05C | T2 (digest/dashboards T3) → T1 | **`liq-obs` depends on `liq-types` and `liq-watch` only** — hot-path crates emit through `tracing` macros and `liq-types::stage()`, never through `liq-obs`, otherwise 00 → 09 → 08 → 00 is a cycle; the engine join consumes the engine's emitted records, not its types. `stage()` spans at every boundary with `ConfigVersion`; `Outcome` taxonomy; watcher ↔ engine join → alarm on `NotTracked`/`HealthWrong` **only**, `declined` → digest (inject one of each); `ShadowRecorder: Submitter` writing `IntendedSubmission` + intended timestamp to JSONL (D06 — no ClickHouse); digest script; ntfy/Telegram alerts; four panels; `perf_event_open` LLC/dTLB on the hot thread, **first-touch-after-block split from steady state**, read after the recompute; instrumentation overhead < 5 %; the PRE-FLIGHT §4 inputs report |

---

## 6. Contracts, simulation, routing

| ID | Name | Spec | Owns | Depends on | Tier → Review | Deliverable / acceptance owned |
|---|---|---|---|---|---|---|
| **10A** | Executor finalisation + callbacks + on-chain adapters | GUIDE 10 §1–§6; PLAN-ENCODING; `Executor.sol` | `contracts/src/`, `contracts/test/unit/` | 07A, 04A, 04B | **T1 → Opus, then H3** | `Executor.sol` reconciled with PLAN-ENCODING (constants already agree: 35/59/77/60); five callbacks with transient-storage arming and `msg.sender` + initiator checks; `uniswapV3SwapCallback` authenticated by CREATE2 derivation; split swap with `TAKE_BALANCE`/`EXACT_OUT`; **on-chain adapters for Aave V3, Aave V4 (read back actual repaid/seized), Morpho Blue** (so Tier 1 needs no early redeploy, D48); `SafeTransfer` everywhere (CI grep); immutable `PROFIT_SINK`, permissionless `sweep()`, payable ceiling bid with refund, never `address(this).balance`; `ExecutorHarness` with `debugDecode*` test-only. Mutations #5, #6, #9, #11 |
| **10B** | `liq-plan` encoder + round-trip proptest | PLAN-ENCODING §2, §2a, §4 | `crates/liq-plan/`, `contracts/test/encoding/` | 10A, 00C | T2 → T1 | `BatchPlan` types; `encode()` with `u128_be` erroring on overflow; `validate()` invariants 1–5 incl. cascade rule; **generator varies groups, legs/group, repay swaps, profit swaps and `data` length independently including minimums**; proptest against the Solidity decoder **on a fork**, field-for-field incl. walked offsets. Mutation #2 |
| **10E** | Pre-H3 Executor: fold passed 15C ABIs | GUIDE 10 §6; D63 | `contracts/src/Executor.sol`, PlanDecoder, `liq-plan`, 10C matrix | 10C, 15C-* PASS set | T1 → Grok (D62) | One WP: on-chain adapters + tails + fork matrix for every 15C family that PASSed before H3 (Euler/Silo/Liquity + wave-2). Not Ajna/Sky/Comet. Then H3. After H3, further ABIs are `10R-n` |
| **10C** | Fork matrix, invariants, gas snapshots | GUIDE 10 §7, §8 | `contracts/test/{fork,invariant}/`, `contracts/gas/`, `config/flash-gas.toml` | 10A, 10B | T2 → T1 | Generated matrix adapters × callbacks × {DAI, WETH, **USDT**}; invariants ≥ 10k runs (zero balances except WETH, zero allowances, `net ≥ minProfit` or revert, every flash repaid exactly, no un-allowlisted call); multi-group with group-1-all-fail; three-group re-walk; fee-on-transfer via measured deltas; under-settle V4 reverts; **gas per provider recorded to config** (closes 07's deferred criterion); `forge snapshot` gate at 2 % |
| **H3** | **Human: Executor review + mainnet deploy** | GUIDE 10 §9; PRE-FLIGHT §6 | `config/venues.toml` | 10A, 10B, 10C | human + external reviewers | Callback auth, approvals, reachable calls, compromised-key blast radius; deploy; address + constructor args committed |
| **11** | Simulation | GUIDE 11 all | `crates/liq-sim/` | 03B, 02A, 10A, 10B, 05B | T2 → T1 | revm workers thread-per-core, `rtrb` per worker each way, `Arc<dyn StateProviderFactory>` shared read-only; `WarmSet` + `clear_except`; `verify()` applies the **trigger tx first** (hint → reconstructed update, or predicted price flagged low-confidence; public transmit; drift → timestamp only); actual gas out; `SimError` classes, `Revert` alerted; scoped-thread variants; `catch_unwind` per worker; zero RPC (panicking provider); p99 < 1 ms warm; **100 historical liquidations within 1 % profit and gas** (uses 05B). Pre-H3: Executor bytecode inserted into `CacheDB` at the planned address — real bytecode, real state; re-baseline gas after H3 |
| **12A-1** | Routing: solver, warm/exact tiers, band, real `RouteCache` | GUIDE 12 §3, §3b, §4 (allocation), §4b, §4c (ordering) | `crates/liq-router/src/{solver,warm,exact,band,cache}.rs` | 07B, 05E, 03A, 06A-1 | **T1 → Opus** | `RouteSolver` over `amms-rs` (discovery + state from the log stream, a `LogSubscriber`) and `Dex-Math-Core-rs`; V4 never a venue, Balancer never; warm builder thread → `Shared.routes: ArcSwap<RouteCache>` with pool **sets** per pair × bucket, `min(spot, twa)`; exact tier water-fill on marginal `(fee + impact)`: V2 closed form, V3 tick-walk (breakpoints, binary search, closed form/2 Newton within interval), Curve Newton, **Brent with bisection fallback** elsewhere; λ loose, residual to best pool; greedy pool-set stopping on hop gas; K-collateral sequential ordering (exhaustive ≤ 4); `ViabilityBand` per `(protocol, coll, debt)` per block on the exact next base fee, base fee only; **implements `RouteCache::has_exit` and replaces 07B's stub; runs 07's two deferred criteria**; root-at-tick-boundary fixture; 6-pool solve inside budget; no HTTP on the hot path |
| **12A-2** | Profit, selection, batching, assembly, gas oracle, bid structure | GUIDE 12 §1, §2, §4 (model), §4c–§4f, §6 (gas oracle, learning-phase bid, channels); D33–D37, D45 | `crates/liq-router/src/{profit,select,assemble,gas,bid}.rs` | 12A-1, 10B, 11, 08A | **T1 → Opus** | `size = min4(…)`, partial beats skip; joint repay × seize × source with scoped threads; profit model charging fee + impact on **seized**, flash fee on `s`, gas at plan level; **eligibility gate separate from ranking in code** (non-liquidatable high-net candidate never enters — test); expected contribution-per-gas with pre-gas numerator and `p` (start 1); group by debt asset, cascade groups from 07B, truncate on Δnet ≤ 0 / header gas limit / nonce slots; over-borrow; `minProfit` as worst-acceptable-partial floor; `BatchPlan` assembly satisfying `liq-plan::validate()`; `next_base_fee` exact, priority percentile; bid = `min(cap, target + jitter)` where target is the **learning-phase near-cap** until 12B exists, expressed per channel (coinbase bps / priority / `refundConfig`); **fallback re-encode against the next source on `InsufficientLiquidity`** (07's deferred criterion); profit within 2 % on 100 replays. Mutation #10 |
| **09B** | **Start the shadow run** (Track A6) | GUIDE 09 §4 | `config/shadow.toml` | 09A, 12A-2, 11, 10C, **G-C passed for each adapter included** | T3 flips on; human notified | Whole pipeline through `ShadowRecorder`; start ts; runs permanently. **G-S** = ≥ 2 weeks and ≥ 200 contested, > 80 % preceding the winner. Also produces `WaitedTooLong` samples and the PRE-FLIGHT §4 inputs for H2 |
| **12B** | Data-gated fits | GUIDE 12 §5, §6 (estimator, argmax, own-win exclusion) | `crates/liq-router/src/fit/` | 09B (≥ 2 weeks of data), 05B | **T1 → Opus** | `F(β)` per ETH-normalised bracket × contest class, windowed by count, **own address excluded** (test: a self-win does not move it); `argmax F(β)(1−β)` + jitter, cap < 1; back-fit winnable historical auctions; `p` per bracket from the taxonomy; V4 timing optimiser behind a flag, default off, gated on ≥ 200 `WaitedTooLong`. Mutation #12 |

---

## 7. Execution, risk, coverage, operations

| ID | Name | Spec | Owns | Depends on | Tier → Review | Deliverable / acceptance owned |
|---|---|---|---|---|---|---|
| **14A** | Risk gate, halt matrix, proxy watcher, caps, treasury, ledger | GUIDE 14 §1–§6; D13b–d | `crates/liq-risk/` | 07B, 02B, 09A | T2 → T1 | `liq-risk` depends on `liq-types`, `liq-flash`, `liq-state` only; the sim-pass/chain-revert and outcome counters it reads arrive as `liq-types` events from 11/13, not as crate dependencies. `RiskGate: HaltSink` with `allow(TraceId)`; every matrix row has metric, threshold, tested action **and class**; no Class B is global; nothing rolling-metric halts (Class C alerts); action-required log separate; EIP-1967 watcher over every protocol **and flash provider** address in config (test enumerates); concentration / depth / haircut auto-tune; caps incl. per-provider concurrent; treasury: gas floor per key, sweep flag `balance > k × transfer_gas`, max age, disposal params; PnL ledger **SQLite** (D06) with provider per liquidation, daily on-chain reconcile within 1 %. Before 13A there is no submitter to gate — halts land in the logs, which is the real behaviour |
| **14B** | Accounting books scanner | GUIDE 14 §7; D42 | `crates/liq-books/`, `books/` | H3, 00C | T3 → REVIEW_T3 **+ T1 spot review of finality and hash chain** | Separate binary reading chain; rows only at finality; two monthly CSVs joined on `tx_hash`; `row_hash = sha256(prev ‖ fields)`, monthly `.sha256`; both Chainlink feeds' `answer/roundId/updatedAt` at the block + ECB daily; raw amounts **and** decimals; backed up off-box from the first row |
| **13A** | Execution | GUIDE 13 §1–§5; D18–D20; D63 | `crates/liq-exec/`, `config/builders.toml` | 12A-2, 06B, 14A, W (H3 deploy **not** a build dep; G-S gates **H4 flip** only) | **T1 → Grok (D62)** | Same path as shadow. `Submitter` impls: `MevShare` (body `[{hash}, {tx}]`, `inclusion` spanning 2–3 blocks, `refundConfig`) and `BuilderBundle` (curated endpoints, `JoinSet` fan-out); **no `PublicMempool`** (D54), including behind `submit_enabled`. Live send is `submit_enabled` default **false** (17A hot-reload). Identity-key header signing from 06B; presigned templates; `NonceAllocator` — `parking_lot::Mutex` per key, guard dropped before any `.await`, gap filler; fee fields from the gas oracle; hot → exec by `try_send`, counted; `RiskGate.allow()` before every send; inclusion watcher → `Terminal`, `LostToCompetitor` via W ≥ 90 %; 20-slot synthetic cascade; sign + submit p99 < 500 µs |
| **H4** | **Human: first live submission** | ORCH §12 | — | 13A reviewed, **G-R passed**, G-S passed | human | Opens the supervised window |
| **13B** | Staged rollout (Track A7) | GUIDE 13 §6; GUIDE 17 §0 | `config/rollout.toml` | H4 | human + T2 | Sepolia end-to-end first; then interest-drift only, small notional, one protocol; then public-oracle on one protocol; then SVR at the learning-phase bid; each stage ≥ 1 week with its review; every revert root-caused before the next session; realized vs predicted tolerance **written down before** the window |
| **15A-1** | Morpho Blue adapter (Rust) + coverage + fixtures + drift | GUIDE 15 §2; GUIDE 04 pattern | `crates/liq-adapters/morpho-blue/`, `docs/coverage/morpho-blue.md`, fixtures | 04A, 01, 03A, 05A | T2 → T1 | Position scope inverse of Aave (many markets × few users); `CreateMarket` discovery from the registry; conformance; own drift week starts on completion, **concurrent** with Aave's; if `liq-engine` needs a change → blocked, route to 01 |
| **15A-2** | Spark (config-only fork) | GUIDE 15 §2 | `config/protocols/spark.toml`, `docs/coverage/spark.md` | 04B | T3 → REVIEW_T3 | Must be a TOML file and a registry section. If it is not, 04B hard-coded something → REWORK 04B |
| **15B** | Mechanism reviews, flashloanability survey, `DECLINED.md` | GUIDE 15 §1, §3 (first three lines), §5 | `DECLINED.md`, `docs/mechanism/*.md` | 07A | T2 → T1 | One page per Tier 2/3 protocol: hard vs soft liquidation, debt assets vs flash depth, enumeration recipe (REGISTRY §3) — **before any adapter code**; Curve/LLAMMA declined with reason; Compound V3 recorded as deferred (D25) |
| **15C-\<proto\>** | One WP per Tier 2/3 adapter | GUIDE 15 §3 checklist | `crates/liq-adapters/<proto>/`, `docs/coverage/<proto>.md`, fixtures, `config/protocols/<proto>.toml` | 15B, 15A-1 | T2 → T1 | Full per-adapter checklist through drift. `encode` stays `ExecutorUnwired` in the adapter WP. **Before H3**, passed new ABIs batch into **10E** (first deploy, D63). **After H3**, a new ABI opens `10R-n` (D48) |
| **15D** | Enable an adapter / source / trigger in production | GUIDE 15 §3 (last line); ORCH §10 | `config/protocols/*.toml` (`enabled`), `STATE.md` roster | 13B live, 14A, that adapter's G-C, recall row filled, shadow ≥ 2 w / ≥ 200 for it, on-chain adapter deployed | **H5** | Config widening is its own session with its own evidence, never part of a build WP |
| **16B** | Box tuning | GUIDE 16 §0 (verify), §2, §3 | `ops/tuned/`, `ops/sysctl/`, `ops/verify.sh` | A1, 16A | T2 → T2 | TuneD profile checked in and active; `isolcpus`, `irqaffinity`, hugepages, swap off, NUMA + **CCX-exclusive** hot cores from `shared_cpu_list`; WAL/snapshot device separate from the node DB; verification script asserts all of it and the disabled timers |
| **16C** | Benchmark harness + end-to-end zero-alloc | GUIDE 16 §6, §3; RUST-CONV §6 | `crates/liq-replay/src/bench/`, `.github/workflows/bench.yml` | 05D, 08A, 11, 12A-2, 16A, 09A | T2 → T1 | Fixed archive segment through ingest → state → price → engine → router → sim → sign; p50/p99/p99.9 per stage; baseline recorded **before** tuning; CI regression gate on the box; `PanicOnAlloc` over the full replay; LLC first-touch baseline; p99.9 inside GUIDE 16 §4b budgets **with the full loaded universe** |
| **16D** | Network | GUIDE 16 §4b, §5 | `ops/net/`, `crates/liq-obs/src/net_rtt.rs` | 09A, 13A | T2 → T2 | RTT p99 monitors to every builder and the relay; persistent pre-established connections (13A's pool) verified — no handshake in the critical section; NIC queue separation feeds vs submission; Paths A/B/C reported as separate series |
| **17A** | Wiring, supervision, lease, hot-reload | GUIDE 17 §1b, §2; GUIDE 00 (startup order) | `crates/liq-bot/src/{main,startup,lease,reload}.rs`, `ops/systemd/` | 13A, 14A, 16A, 02B, 00D | T2 → T1 | Startup: registry assert → config `Validate` → `Shared` built and leaked → threads pinned and asserted → ExEx registered; **post-restart drift check gates the submit lease** (corrupt snapshot → lease refused); `submit_enabled` hot-reloadable (drill: toggled without restart); cold-restart time measured and written down |
| **17C** | Process join leftovers (D63) | GUIDE 17 §1b; GUIDE 12 §4; GUIDE 16 §4b; D63 | `crates/liq-bot/src/{startup,main,exec_bind}.rs`, `crates/liq-exec/src/path.rs` | 17A, 16D, 12A-1, 10E | T2 → Grok (D62) | Attach the leftover **code paths** 17A accepted as D60 residuals: `run()` holds `ExecPath` via `exec_bind::bind`; warm-builder thread; process `AssembleView` + empty `TailPins` fail-closed; `RttMonitor` tick (empty → ABSENT); 13A HTTP keepalive/prewarm; `mark_dropped`+gap-fill on deny/send-fail. No invented keys. No `nonce_resync` store-true. No `submit_enabled` flip. No PublicMempool. |
| **17D** | Hot drain join (D63) | GUIDE 08 §5–§6; GUIDE 12 §4; GUIDE 11; GUIDE 13 §1; D63 | `crates/liq-bot/src/` drain + `liq-node` after-block seam | 17C, 08A, 12A-2, 11, 13A | T2 → Grok (D62) | Same path as shadow. After-block: dirty → `Engine::on_dirty`/`on_block` → `CandidateQueue::drain` → `select` → `assemble` → sim verify or fail-closed skip → `ExecInbox::try_send` → `submit_path`. `OraclePredicted` never becomes a job. Empty pins/intern/state/exec → log, no invented plan. Hot thread never `block_on`. Three-way live gate unchanged. No PublicMempool. |
| **17E** | Drain config bind (D63) | GUIDE 15; GUIDE 12 §4; 10C gas_overhead; D63 | `crates/liq-bot/src/{startup,drain}.rs`, `liq-node` header gas | 17D, 04B, 15C-*, 10C, 00D | T2 → Grok (D62) | Replace `live_noop` emptiness with **observed** inputs: load TOML adapters (fail-closed if intern/registry unasserted); intern from registry; `SelectReady` only when header `gas_limit` is on the committed block (add the field — do not invent 30M); wrap/`gas_failed` from 10C snapshots; fee only from a real gas window. Sim stays `None` without a provider. TailPins from candidate+config, not guessed. Three-way gate unchanged. |
| **17F** | Ingest + fee observe bind (D63) | GUIDE 03 §4; GUIDE 12 §4b; D63 | `crates/liq-bot/src/{startup,bind}.rs`, `liq-node` header base fee | 17E, 03A, 06C | T2 → Grok (D62) | Leaked loaded adapters are `LogRouter` subscribers + `LogHandler` → `Protocol::apply_log`; `register_exex` gets those protocol ids (not `[]`). `GasOracle` is kept; `observe_parent` only when header `base_fee_per_gas` is present (`0` = absent). Do **not** invent `SelectReady`, `BidConfig` (D11/D30/D35 unset), or `gas_failed`. Three-way gate unchanged. |
| **17G** | Index subscribers (D63) | GUIDE 07; GUIDE 12 §3; GUIDE 06 §2; D63 | `crates/liq-bot/src/{startup,bind,routes,drain}.rs` | 17F, 07A, 12A-1, 06A-1 | T2 → Grok (D62) | Same hot `LogRouter` also folds 07A `FlashSource`s, 12A-1 `PoolBook`, and 06A-1 `FeedSet`/`DerivedBook`. One shared book/index with drain + warm thread. Addresses from committed registry/config only; missing → omit, log, empty publish. Do **not** invent `SelectReady`, Bid (D11), pools, or prices. Three-way gate unchanged. |
| **17B** | Runbook, drills, keys, backups, reports | GUIDE 17 §3–§5; GUIDE 16 §7 | `ops/runbook/`, `ops/keys.md`, `ops/backup.md`, `ops/report/` | 14A, 17A, 17C, 17D, 17E, 17F, 17G | T3 → REVIEW_T3; keys with a human | One page per GUIDE 14 alert; every alert fired in a drill; three keys (operator per slot, profit-sink, relay identity) generated, backed up encrypted off-box, rotation rehearsed; the kilobytes in git (`reth.toml`, TuneD, config, addresses, `STATE.md`); daily report delivered; weekly review template |
| **H6** | **Human: unattended running** | GUIDE 17 §0 | — | **G-U passed** | human | All six Step 0 rows with numbers and dates in `STATE.md` |

---

## 8. Critical path and the parallel lanes

Code before the first clock can start (drift):

```
00A ─┬─ 00B ─── 01 ─── 02A ─┬─ 02B ──┐
     ├─ 00C ────────────────┤        │
     ├─ 00D (needs C2) ─────┤        ├─ 03B (needs 16A, A2)
     └─ 16A (needs A1) ─────┘        │
                     03A ────────────┘
                     03C ─────────── 04A ─── 04B ─┐
                     06A-1 ────────────────────────┼─ 04C ▶ G-C (7 d, per adapter)
                     W (needs A2) ─── A4 ▶ G-R accumulates from here
```

Everything below runs **during** the drift week and the shadow weeks. Suggested
lane packing at `LANES = 3` (the coordinator recomputes from eligibility; this
is the expected shape, not a schedule):

| While waiting on | Lane 1 (T1) | Lane 2 (T2) | Lane 3 (T2/T3) |
|---|---|---|---|
| A2 download | 00B → 01 → 02A | 00C → 03A → 03C | 00A → 00D → 16A |
| G-C (drift) | 04A → 05A → 05C → 07B → 08A | 04B → 06A-2 → 07A → 06B → 06C → 15A-1 | W → 05B → 05D → 05E → 06D → 15A-2 → 15B |
| G-C continued | 10A → 10B(review) → 12A-1 → 12A-2 | 09A → 11 → 10C → 14A → 16B | 16C → 14B(after H3) → 15C-… |
| G-S (shadow) | 12B (once data) → 13A (**only after G-S**) | 16D prep, 15C-… | 17B docs, 10R batching |
| G-U (supervised) | — | 17A → 16D | 17B |

Longest chain to the first live submission is calendar, not code:
`A2 (hours) → 04C (7 d) → 09B (≥ 2 w, ≥ 200 contested) → H4 → 13B (≥ 2 w supervised) → H6`.

---

## 9. Adding a WP later

Every new adapter, flash source or venue gets a row here **before** a builder is
launched, with: spec section, owned paths, dependencies, tier chosen by
`ORCHESTRATOR.md` §5's rubric, the acceptance lines it owns, and any criteria it
defers to another WP by name. A WP without a row is not dispatchable.
