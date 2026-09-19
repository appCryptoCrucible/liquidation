# Build vs. Buy — Dependency Research

**Scope:** Ethereum mainnet only. Flashloan-funded only. Full protocol universe.
Rust. Single baremetal server. Verified September 2026.

---

## 0. Five findings that shape the design

### 0.1 Aave V4 is live on mainnet and its liquidation engine is different

Aave V4 launched on Ethereum mainnet **30 March 2026** with hub-and-spoke
architecture and a redesigned liquidation engine.

| V3 | V4 |
|---|---|
| Fixed close factor (50%, 100% below HF 0.95) | **Dynamic** — repay at most enough to restore a governance-set **Target Health Factor** per Spoke |
| Static bonus per reserve | **Variable bonus scaling with health** — `maxLiquidationBonus`, `healthFactorForMaxBonus`, `liquidationBonusFactor` |
| Dust left behind | Max liquidatable adjusts so sub-threshold remainders clear fully |
| Per-user pooled position | `(Spoke, user)`; assets accounted at the Hub by `assetId` |
| Scaled balance × `liquidityIndex` | **Share-based**: `suppliedShares`/`drawnShares`, `addExRate`/`drawnIndex` |
| Uniform borrow rate | Base rate **plus a per-user Risk Premium**, settled as `premiumDelta` |

**Consequences:** "fire at HF < 1" is no longer optimal — when to fire is an
expected-value problem (GUIDE 12). And the per-user risk premium partially breaks
the axiom that accrual is a pure per-market multiplier, so the store carries
per-position premium state (GUIDE 02) and `time_to_cross` must integrate it
(GUIDE 08).

V3 stays live alongside V4. Build the V3 adapter against **3.5+** semantics —
that release changed rounding and internal scaled accounting.

### 0.2 Aave on mainnet uses Chainlink SVR — the public-mempool backrun is dead

SVR uses a dual-aggregator design: a **standard feed** via the public mempool as
fallback, and an **SVR feed** routed privately through **Flashbots MEV-Share**,
where the right to backrun the update is auctioned to searchers. Builders take
the highest bid and include it in the same block. If nobody bids, the update
publishes with no backrun. Recaptured value splits with the protocol (Aave's
launch terms were 65/35 in Aave's favour for six months).

**Consequence, and it is large:** for SVR feeds, watching the public mempool for
`transmit()` does not win. The path is: subscribe to the MEV-Share SSE stream,
match hints against the SVR aggregator address, and submit a bundle via
`mev_sendBundle` referencing the hinted transaction by hash. The auction is
decided on **value, not arrival order**, so the bid model becomes a core
competency (GUIDES 06, 12, 13).

That does not make latency irrelevant, and an earlier draft of this document
overstated it. Every searcher's clock starts when the hint reaches them and the
block seals at a fixed moment, so proximity to the relay buys **decision time** —
more milliseconds to size, route, simulate and price the bid before the deadline.
Colocated infrastructure converts directly into a better-informed bid. And for
the eight non-auctioned trigger classes (`PRE-FLIGHT.md` §3), it is a
straightforward latency race where proximity is the whole game.

### 0.3 Uniswap V4 gives fee-free flash loans, and they are usually your best source

Flashloan-only funding makes the flash fee an unavoidable line item on every
liquidation. Sources are not interchangeable:

| Source | Fee | Availability | Callback |
|---|---|---|---|
| **Uniswap V4 PoolManager** | **0** | `IERC20(asset).balanceOf(PoolManager)` | `unlockCallback` + `take`/`settle` |
| **Morpho Blue** | **0** (documented as free) | `IERC20(asset).balanceOf(Morpho)` | `onMorphoFlashLoan` |
| **Sky DSS Flash** | **0** today (`flashFee` → 0; runtime) | `maxFlashLoan(DAI)` == governance `max` [wad]; **DAI only** | ERC-3156 `onFlashLoan` |
| Uniswap V3 pool | pool fee tier: 5 / 30 / 100 bps | `IERC20(asset).balanceOf(pool)` | `uniswapV3FlashCallback` |
| Aave V3 / V4 | **5 bps** (`FLASHLOAN_PREMIUM_TOTAL`, 4 bps to treasury) | `IERC20(asset).balanceOf(aToken)` if flash enabled | `executeOperation` |

**Five arenas.** Balancer excluded (D08/D09). Sky DSS is the Dai flash-mint module
(`0x60744434d6339a6B27d73d9Eda62b6F66a0a04FA` on mainnet) — not a pool balance.

Uniswap V4's flash accounting: call `unlock()`, then inside `unlockCallback` use
`take()` to pull tokens and `settle()` to return them. Deltas live in transient
storage and `unlock()` reverts unless every delta nets to zero. A pure
borrow-and-return pays nothing — you pay swap fees only if you swap.

Aave note: `flashLoanSimple()` is the gas-efficient single-reserve path with **no
fee waiver**; the array-form `flashLoan()` is eligible for the fee waiver granted
to approved `flashBorrowers` via `ACLManager`. Make the fee a runtime lookup, not
a constant, so the waiver path costs nothing to support.

**Consequence:** source selection is a bid advantage. A competitor paying 5 bps on
a $500k liquidation is $250 behind you before either of you bids. This is why
flash sourcing is its own crate (GUIDE 07) rather than a router sub-step.

### 0.4 Uniswap V4: flashloan source, not a swap venue

The same contract, two opposite conclusions, and conflating them is expensive
either way.

**As a swap venue: excluded.** V4 hooks fire on `beforeSwap`/`afterSwap` and can
alter amounts, impose fees, or replace the curve entirely
(`beforeSwapReturnDelta`). There is no generic V4 quote — only per-hook ones — so
a base-math quote against a hooked pool is wrong in an unpredictable direction.
Route exits through V3 and Curve (Kyber Elastic where quoted), where quoting is deterministic from
pool state. Balancer is out of scope.

**As a flashloan source: preferred.** Hooks do not fire on `take`/`settle`, which
are PoolManager-level accounting. A borrow-and-return through `unlock` never
touches hook logic, so V4 remains fee-free and deep.

Keep these as separate config surfaces. "Avoid V4" propagating from the router
into the flash-source list silently costs the zero-fee advantage the entire bid
model rests on.

### 0.5 The mainnet lending universe, sized

Approximate TVL, mid-2026 — prioritize by liquidatable debt and **debt-asset
flashloanability**, not headline TVL:

Aave V3 $14.6B · Morpho Blue $11.8B · Sky Lending $5.6B · Spark $3.2B ·
Compound V3 $1.8B · Fluid $1B · Euler V2 $880M · Silo $410M · plus Aave V4
(new), Compound V2 forks, Liquity V2 / trove forks, Gearbox, Ajna.

**Fork multiplier:** Spark is an Aave V3 fork; many mid-tier protocols are
Compound V2 forks. One adapter per *family* plus a TOML file covers each fork —
which is why "everything protocol-specific is data" (GUIDE 00) is load-bearing.

**Decline on purpose:** Curve lending / LLAMMA continuously rebalances positions
across price bands rather than liquidating them. There is no event to win.
Mis-modelling this costs months (GUIDE 15 Step 5).

---

## 1. Dependency matrix

| Need | Decision | What to use | Why |
|---|---|---|---|
| Ethereum types, ABI, RPC, signing | **BUY** | `alloy` 2.4.2 (MSRV 1.94.1) | Ecosystem standard. `sol!` gives codegen'd ABI decode for the hot path. `ethers-rs` is dead. |
| EVM execution / simulation | **BUY** | `revm` 43.0.2 | Only serious option. `ExecuteEvm`, `Context`, `Database`/`DatabaseRef`, `CacheDB`, `Inspector`. |
| Node, state access, reorg feed | **BUY** | `reth`, as an **ExEx** | Runs in-process with the node, delivers chain committed/reverted/reorged notifications, back-pressures via `ExExEvent::FinishedHeight`. Replaces an entire ingest subsystem *and* gives reorg handling free. |
| Contract dev, fork tests, fuzzing | **BUY** | Foundry | Fork + invariant + differential fuzzing is the correctness backbone. |
| MEV-Share stream + bundles | **BUY protocol, BUILD client** | SSE + `mev_sendBundle` | TS reference client exists; no maintained Rust one. It is an SSE reader plus one JSON-RPC method — do not take a critical-path dependency. |
| Flash loan sources | **BUILD** | `alloy::sol!` against each provider ABI | Five providers (0–4; id 4 = Sky DSS), one liquidity index. Core IP (GUIDE 07). |
| AMM pool discovery + state sync | **BUY, then wrap** | `amms-rs` (`amms` crate) | Pool discovery and state sync from the log stream. Wrap behind your own `RouteSolver`. |
| **Exact swap quoting** | **OWN** | **`Dex-Math-Core-rs`** | V2/V3/V4, Curve StableSwap, Kyber Elastic. Balancer weighted may exist in the crate — **do not route**. Deterministic integer math, fail-closed errors. |
| MEV bot framework | **BUILD** | — (read `paradigmxyz/artemis`) | ~29 commits, built on deprecated `ethers-rs`. Copy the Collector/Strategy/Executor pattern by hand. |
| Health factor math | **BUILD** | — | This is the product. Exact rounding per protocol, differentially fuzzed. |
| State store | **BUILD** | — | Columnar + undo ring + dirty-set. No database gives microsecond folds plus reorg unwind. |
| Threshold index / bands | **BUILD** | — | ~400 lines, specific to the problem. |
| Config / serialization | **BUY** | `serde`, `toml`, `figment` | Config is a correctness surface, not a novel one. |
| Metrics / tracing | **BUY** | `tracing`, `metrics` + Prometheus | Thread your own `TraceId` through it; don't build transport. |
| Persistence | **BUY primitive, BUILD policy** | `memmap2` + append log | A database on the hot path is an anti-pattern. |
| Analytics / PnL | **BUY** | `rusqlite` (bundled) + JSONL via `serde_json` | Off the hot path. D06 — no ClickHouse/Postgres at tens of rows a day. |
| Fixed-point math | **BUILD on** `alloy-primitives::U256` | — | Ray/Wad newtypes with explicit rounding direction. Correctness, not style. |
| CEX market data | **BUILD** | raw websockets | Exchange SDKs add latency and dependencies for a few hundred lines of work. |
| NUMA / allocator control | **BUY** | `libnuma`, hugepages, `cpuset` | Standard Linux tuning (GUIDE 16). |
| Kernel-bypass networking | **SKIP** unless proven | — | Under SVR the kernel path is not your bottleneck. Measure before spending weeks. |
| Solana / L2 support | **SKIP** | — | Mainnet only. |
| Inventory / capital management | **SKIP** | — | Flashloan-only removes it entirely. |

### Swap math: `Dex-Math-Core-rs`

Use **[`Dex-Math-Core-rs`](https://github.com/appCryptoCrucible/Dex-Math-Core-rs)**
as the exact swap-math layer. It covers Uniswap V2 constant-product, V3 and V4
concentrated liquidity with tick crossing, Curve StableSwap variants, and Kyber
Elastic — deterministic integer arithmetic throughout,
with a uniform `pool snapshot + swap params → quote` interface and fail-closed
`Result<_, DexError>` rather than guessed fallbacks.

Two reasons it earns its place over `amms-rs` alone:

**Curve coverage.** Liquidation collateral is frequently a
stablecoin or an LST, where the best exit is a Curve StableSwap pool rather than
a Uniswap route. `amms-rs` is thin there. Since seized collateral determines the
swap leg and collateral mix is not your choice, coverage gaps translate directly
into opportunities you cannot price.

**Simulation verifies one plan; exact math is what lets you choose it.** An
earlier draft of this document claimed revm made a swap-math library unnecessary.
That was wrong in an important way. revm is the final gate on the plan you
picked, but you cannot simulate the search space — multi-hop routes, split
allocations across pools, size optimization — inside the latency budget. The
optimizer needs a fast, exact model; simulation confirms its answer. Without one
you are choosing routes on approximations and discovering the cost after you have
already bid.

That matters more at near-maximal bids than anywhere else: **bid accuracy is swap
accuracy.** Bidding `net − floor` means a 0.1% error in predicted output on an
$8,000 gross is $8 — a sixth of a $50 floor. Approximate routing math shows up
directly as underbidding or reverted bundles.

Keep `amms-rs` for what it is good at — pool discovery and state sync from the
log stream — and use `Dex-Math-Core-rs` for quoting. Wrap both behind your own
`RouteSolver` trait (GUIDE 12).

**Its V4 coverage is base math only, which is the right scope.** Uniswap V4 swap
behaviour is pool-dependent: hooks fire on `beforeSwap`/`afterSwap` and can alter
amounts, impose fees or replace the curve (`beforeSwapReturnDelta`). There is no
generic V4 quote to implement — only per-hook ones. So **V4 is excluded as a swap
venue** (GUIDE 12 Step 3); route exits through V3 and Curve where
quoting is deterministic from pool state.

That exclusion is about routing only. V4 remains the preferred **flashloan
source**, because hooks do not fire on `take`/`settle` — a borrow-and-return
through `unlock` never touches hook logic (GUIDE 07).

**Parity-test the rest before live capital**, per the library's own README. Fuzz
quotes against revm executing the real pool over randomized states and sizes,
biased to the notionals you actually trade. Same pattern as the health-math
differential fuzz, second target (GUIDE 05 Step 6b). A systematically optimistic
quote does not fail loudly — it produces bids that cannot be honoured, which
reads as losing races rather than as a math bug.

**None of this helps with protocol health math**, which is where rounding decides
whether to fire at all. That is reimplemented from each protocol's source per
GUIDE 04 and certified by differential fuzzing per GUIDE 05 — no library covers
it, because it is the product.

---

## 2. Build-to-buy ratio by crate

```
liq-types      100% build   thin, but yours
liq-config      80% build   serde/figment parse
liq-obs         20% build   tracing works; TraceId plumbing is yours
liq-node        30% build   reth ExEx does ingest + reorg; decode/dispatch yours
liq-state      100% build   ← core IP
liq-protocol   100% build   ← core IP
liq-adapters   100% build   ← core IP, bulk of ongoing work (GUIDE 15)
liq-flash       90% build   ← core IP; alloy only for ABIs
liq-oracle      70% build   alloy reads; SVR client + agg-sim are yours
liq-engine     100% build   ← core IP
liq-router      50% build   amms-rs for pool math; sizing/bid model yours
liq-sim         15% build   revm does it; you supply state provider + warm cache
liq-exec        60% build   alloy signs; MEV-Share client, nonce pool, fanout yours
liq-risk       100% build
liq-replay      80% build
contracts/     100% build   Foundry is the toolchain, not the code
```

**The pattern: buy everything that touches the chain, build everything that
decides.** A dependency between you and a decision is a latency and correctness
liability. A dependency between you and a byte format saves you a month.

---

## 3. Version pinning policy

```toml
[workspace.dependencies]
alloy = { version = "=2.4.2", features = ["full", "pubsub", "sol-types"] }
revm  = "=43.0.2"
reth-exex     = { git = "https://github.com/paradigmxyz/reth", tag = "<pinned>" }
reth-node-api = { git = "https://github.com/paradigmxyz/reth", tag = "<pinned>" }
amms  = "<pinned>"
```

Reth crates are not published on a stable cadence — pin to a git tag and treat a
bump as requiring a full replay run (GUIDE 05) plus a benchmark comparison
(GUIDE 16). An ExEx API change is the most likely source of a silent ingest
regression.

---

## Sources

- [Aave V4 is Live on Ethereum](https://aave.com/blog/aave-v4-live-ethereum)
- [Aave V4's New Liquidation Engine](https://aave.com/blog/aave-v4-liquidations)
- [Aave v4 Overview](https://aave.com/docs/aave-v4) · [Spokes](https://aave.com/docs/aave-v4/liquidity/spokes) · [Changelog](https://aave.com/docs/resources/changelog)
- [Anatomy of the Aave v4 contracts](https://jeancvllr.medium.com/anatomy-of-the-aave-v4-contracts-364fa3189d04) · [aave/aave-v4 DeepWiki](https://deepwiki.com/aave/aave-v4)
- [Aave Flash Loans — docs](https://aave.com/docs/aave-v3/guides/flash-loans) · [Aave flashloan fees — governance](https://governance.aave.com/t/aave-flashloan-fees/21149)
- [Uniswap V4 flash accounting deep dive](https://www.cyfrin.io/blog/uniswap-v4-swap-deep-dive-into-execution-and-accounting) · [PoolManager flash loan exercise](https://updraft.cyfrin.io/courses/uniswap-v4/pool-manager/exercise-flash-loan)
- [Morpho Blue Free Flash Loans](https://docs.morpho.org/contracts/morpho-blue/guides/advanced-features/free-flash-loans/)
- [Chainlink SVR Feeds](https://docs.chain.link/data-feeds/svr-feeds) · [Introducing SVR](https://chain.link/blog/chainlink-smart-value-recapture-svr)
- [MEV-Share Event Stream](https://docs.flashbots.net/flashbots-mev-share/searchers/event-stream) · [Sending Bundles](https://docs.flashbots.net/flashbots-mev-share/searchers/sending-bundles)
- [Reth ExEx overview](https://reth.rs/exex/overview/) · [How ExExes work](https://reth.rs/exex/how-it-works/)
- [revm docs](https://docs.rs/revm/latest/revm/) · [alloy docs](https://docs.rs/alloy/latest/alloy/)
- [paradigmxyz/artemis](https://github.com/paradigmxyz/artemis) · [darkforestry/amms-rs](https://github.com/darkforestry/amms-rs)
- [DeFi lending protocols compared, 2026](https://eco.com/support/en/articles/15254000-best-defi-lending-protocols-2026-tvl-rates-risk-compared)
