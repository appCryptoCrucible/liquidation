# GUIDE 07 — Flashloan Sources & Position Eligibility

| | |
|---|---|
| **Crate** | `liq-flash` |
| **Prerequisites** | GUIDE 00, 01, 03 |
| **Work packages** | **07A** the five `FlashSource`s; **07B** `FlashIndex` + eligibility + selection + cascade planner, using the `RouteCache` trait from `liq-protocol` with a **depth-only interim implementor** until 12A-1 provides the real one. Three acceptance lines are owned elsewhere: route-depth fixtures → 12A-1, measured `gas_overhead` → 10C, re-encode on sim liquidity failure → 12A-2 |
| **Est. effort** | 1.5–2 weeks |
| **Blocks** | 08, 10, 12, 14 |

> **Route dependency — read before starting.** Step 5's eligibility filter needs
> `RouteCache::has_exit`, which GUIDE 12 builds. Do **not** pull GUIDE 12 forward.
> Define the one-method interface here, ship a conservative stub that answers from
> flash-source depth alone, and **defer the two route-dependent acceptance criteria
> to GUIDE 12's checklist** (`ORCHESTRATOR.md` §3). The stub under-admits, which is
> the safe direction.

## Objective

Track, in real time, how much of every asset can be flash-borrowed from every
source — and use that to gate which positions are even worth tracking as
targets.

This layer exists because of a hard constraint: **every liquidation is funded by
a flash loan.** No inventory, no warehousing. That turns flashloan liquidity from
an implementation detail into a **first-class filter on the opportunity universe**,
and it deserves its own crate.

The full cycle every liquidation must close:

```
flash-borrow DEBT  →  repay debt, seize COLLATERAL  →  swap COLLATERAL→DEBT
                   →  repay flash + fee  →  keep the remainder
```

A position is a target only if all four legs are viable. Leg 1 is this guide;
leg 3 is the router (GUIDE 12).

## Build vs. buy

100% build. Use `alloy::sol!` for the provider ABIs. The pool/liquidity state
comes from the ExEx log stream you already have (GUIDE 03) — do not poll.

---

## Step 1 — The provider abstraction

```rust
pub trait FlashSource: Send + Sync {
    fn provider(&self) -> FlashProvider;
    /// Max borrowable of `asset` right now, from live state. Hot path.
    fn available(&self, asset: AssetId) -> U256;
    fn fee_bps(&self, asset: AssetId, amount: U256) -> u16;
    fn callback(&self) -> CallbackShape;
    /// Gas overhead of this provider's wrapping, measured not estimated.
    fn gas_overhead(&self) -> u64;
    /// Which logs change this source's availability.
    fn subscriptions(&self) -> Vec<LogFilter>;
    fn apply_log(&self, log: &DecodedLog);
}

pub enum CallbackShape {
    /// Aave: executeOperation(assets, amounts, premiums, initiator, params)
    AaveExecuteOperation,
    /// Uniswap V3: uniswapV3FlashCallback(fee0, fee1, data)
    UniV3FlashCallback,
    /// Uniswap V4: unlockCallback(data) — take() then settle() inside
    UniV4UnlockCallback,
    /// Morpho: onMorphoFlashLoan(assets, data)
    MorphoFlashCallback,
    /// Sky DSS Flash (ERC-3156): onFlashLoan(initiator, token, amount, fee, data)
    SkyDssOnFlashLoan,
    // BalancerReceiveFlashLoan — OUT OF SCOPE (D08/D09). Do not add.
}
```

## Step 2 — Required sources

**Aave** (required). Two entry points with different properties:

- `flashLoanSimple(receiver, asset, amount, params, referralCode)` — single
  reserve, gas-efficient, **no fee waiver**
- `flashLoan(receiver, assets[], amounts[], interestRateModes[], onBehalfOf, params, referralCode)`
  — multi-reserve, and **eligible for the fee waiver** granted to approved
  `flashBorrowers` via `ACLManager`

Fee: **5 bps** total premium (`FLASHLOAN_PREMIUM_TOTAL`, set at deployment,
governance-adjustable), of which 4 bps currently goes to the protocol treasury.

Default to `flashLoanSimple` for the single-asset case on gas. Note the waiver
path exists — approved `flashBorrowers` pay zero. Whether a liquidation bot can
obtain that status is a governance question, not an engineering one, but the
code should support it: make the fee a runtime lookup, not a constant, so the
day it becomes zero nothing needs rebuilding.

Support both Aave V3 and V4 pools as sources.

**Uniswap** (required). Two very different mechanisms:

- **V3** — `IUniswapV3Pool.flash(recipient, amount0, amount1, data)`, repaid in
  `uniswapV3FlashCallback`. Fee is the **pool's swap fee tier** (500 / 3000 /
  10000 = 5 / 30 / 100 bps). Availability formula: Step 3a.
  Always prefer the 500 tier when it holds enough.
- **V4** — the PoolManager singleton with **flash accounting**. Call `unlock()`,
  and inside `unlockCallback` use `take()` to pull tokens and `settle()` to
  return them. Deltas are tracked in transient storage; `unlock()` reverts unless
  every delta nets to zero before it exits.

  **Uniswap V4 is fee-free for a pure borrow-and-return.** You pay swap fees only
  if you swap. Availability formula: Step 3a (PoolManager ERC-20 balance).
  Typically far deeper than any single V3 pool. On both axes this is usually your best source.

  **Hooks do not apply here.** V4 hooks fire on swap and liquidity operations, not
  on `take`/`settle`, which are PoolManager-level accounting. A borrow-and-return
  through `unlock` never invokes hook logic, so V4 is a deterministic flash source
  even though it is *not* a safe generic swap venue (GUIDE 12 Step 3). Keep those
  two decisions separate — excluding V4 from routing must not exclude it from
  funding, or you lose the zero-fee advantage the bid model rests on.

## Step 3 — Flash-source set (five arenas; Balancer excluded)

Aave and Uniswap are the required sources. Morpho and **Sky DSS Flash** are free
(or currently zero-fee) and cheap to add. **Balancer is out of scope** as both a
flash source and a swap venue (D08/D09). Do not implement `receiveFlashLoan`.

| Source | Fee | Availability (formula below) | Callback |
|---|---|---|---|
| **Uniswap V4 PoolManager** | **0** | PoolManager ERC-20 balance | `unlockCallback` |
| **Morpho Blue** | **0** (documented free) | Morpho singleton ERC-20 balance | `onMorphoFlashLoan` |
| **Sky DSS Flash** | **0** today (`flashFee` → 0; runtime lookup) | governance `max` ceiling, **DAI only** | ERC-3156 `onFlashLoan` |
| Uniswap V3 pool | pool fee tier (1 / 5 / 30 / 100 bps) | that pool's ERC-20 balance | `uniswapV3FlashCallback` |
| Aave V3 / V4 | 5 bps (0 if waived) | aToken underlying balance | `executeOperation` |

**Count: five arenas.** Balancer is not among them. Sky DSS is **DAI-denominated
only** — `available(asset) = 0` for every non-DAI debt asset. It still earns its
slot: DAI is a high-volume liquidation debt token, the fee is zero, and depth is a
governance ceiling rather than pool inventory.

On a $500k liquidation, 5 bps is $250 — routinely the difference between winning
and losing a bid (GUIDE 12). Implement the zero-fee sources; keep Aave for depth
when nothing else holds enough.

Build all five behind `FlashSource` and let the selector decide. Adding a source
must be one file. Cascade rules unchanged: ≤3 sources combined, 99% planable
buffer (D44).

### Step 3a — Availability formulas (implementable)

Each `FlashSource::available(asset)` returns the on-chain ceiling before haircuts.
Multi-source planning applies an additional **1% buffer**: `planable = floor(available · 99/100)` (PLAN-ENCODING §1b′). Eligibility may apply a separate global haircut (Step 4).

**Aave V3 / V4 (per Pool instance):**

```
reserve = Pool.getReserveData(asset)           // aTokenAddress, configuration
enabled = configuration.flashLoanEnabled
          && reserve active && not paused
raw     = IERC20(asset).balanceOf(reserve.aTokenAddress)
available_aave(asset) = enabled ? raw : 0
fee_bps = Pool.FLASHLOAN_PREMIUM_TOTAL()       // runtime lookup, not a constant
```

Track every admitted Pool (Core, Prime, … and V4 spokes that expose flash). Summing
across pools is **not** one source — each Pool is its own `flashSource` address.

**Uniswap V3 (per pool address):**

```
// Prefer the lowest fee tier that can fund the size (100 → 500 → 3000 → 10000).
// The 100 tier (1 bp) is where stable/stable and LST/ETH depth lives; leaving it
// out pays 5× the fee on exactly the debt assets liquidated most.
available_univ3(pool, asset) = IERC20(asset).balanceOf(pool)
fee_bps = pool.fee() / 100                     // 100 → 1 bp, 500 → 5 bps, 3000 → 30 bps
flash_fee = ceil(amount · pool.fee() / 1e6)    // V3 rounds the fee UP (FullMath.mulDivRoundingUp)
```

Only pools where `token0 == asset || token1 == asset`. Index the deepest pool per
`(asset, fee)` from the registry; `available` is that pool's balance, not a sum
across tiers unless you emit separate `SourceEntry` rows (allowed).

**Uniswap V4 PoolManager (singleton):**

```
// take() pulls from the manager's own ERC-20 balance; hooks do not apply.
available_univ4(asset) = IERC20(asset).balanceOf(PoolManager)
fee_bps = 0
```

Native ETH is `CurrencyLibrary.ADDRESS_ZERO` — treat WETH as the debt asset the
bot actually flashes; do not flash native ETH unless the plan explicitly unwraps.

**Morpho Blue (singleton):**

```
// Docs: maxFlashLoan ≡ token balance of Morpho; fee ≡ 0.
// Balance includes market liquidity, posted collateral, and donations.
available_morpho(asset) = IERC20(asset).balanceOf(Morpho)
fee_bps = 0
```

**Combined planable depth** for eligibility / cascade:

```
planable_total(A) = Σ_i floor( available_i(A) · 99/100 )   // at most 3 sources in one plan
```

A position needing more than `planable_total` is unfundable at that size even with
cascade. Prefer single-source when `planable_best ≥ need`.

## Step 4 — The liquidity index

The hot-path structure. One question, asked millions of times per second: *how
much of asset X can I borrow right now, and at what cost?*

```rust
pub struct FlashIndex {
    /// Per asset, sources sorted by effective cost at a reference size.
    /// Rebuilt incrementally from the log stream, never polled.
    by_asset: Vec<SmallVec<[SourceEntry; 6]>>,
    /// Cached max across all sources, for the fast eligibility check.
    max_available: Vec<U256>,
}

pub struct SourceEntry {
    provider: FlashProvider,
    available: U256,
    fee_bps: u16,
    gas_overhead: u64,
}
```

Update triggers, all from logs you already receive:

- Aave: supply / withdraw / borrow / repay / liquidation on any reserve
- Uniswap V3: mint / burn / swap / flash on tracked pools
- Uniswap V4: every `Swap` / `ModifyLiquidity` / settle affecting the singleton
- Morpho: supply / withdraw / liquidate / flash on the singleton
- Sky DSS Flash: `File` on `max`; Vat live/cage

Keep a **safety haircut** on eligibility — never treat 100% of reported
availability as durable. Another searcher's flash loan can land ahead of yours in
the same block and drain the source. 80–90% is a reasonable starting *eligibility*
haircut; tune it from observed `InsufficientLiquidity` simulation failures.

Separately, multi-source **amount splitting** always uses the fixed **1% buffer**
(`planable = floor(available · 99/100)`, PLAN-ENCODING §1b′). Do not collapse the
two into one number: the eligibility haircut gates the universe; the 1% buffer
sizes cascade legs.

## Step 4b — Rust: concurrency & safety

**`FlashIndex` is read-mostly: written by the ExEx thread, read everywhere.**

Because updates arrive as logs on the ingest thread, the writer is the same
thread as the hot path — so hot-path reads are just `&FlashIndex`, no
synchronization at all. Off-thread readers (the router's background warm-cache
builder, risk monitoring, telemetry) get a published snapshot:

```rust
// Owned in `Shared`, built once at startup, handed out as `&'static`.
// NOT a `static` + `LazyLock`: that buys an acquire load and a branch on every
// access to avoid an ordering problem this process does not have.
// RUST-CONVENTIONS.md §3.3.
// `shared: &'static Shared` holds `flash: ArcSwap<FlashIndex>`.

// Writer: republish at end of block if anything moved.
if dirty { shared.flash.store(Arc::new(index.clone_cheap())); }
// Any reader: wait-free.
let idx = shared.flash.load();
```

Republish only when availability actually moved past a threshold, not every
block — otherwise you allocate a new index 7,200 times a day for nothing.

**Availability lookup must be allocation-free and branch-light.** `by_asset` is a
flat `Vec<SmallVec<[SourceEntry; 6]>>` indexed by the global `AssetId`. Six
inline entries covers all five sources with room, so the common case never
touches the heap.

**`FlashSource` is a trait object, built once.** `Vec<Box<dyn FlashSource>>`
constructed at startup: one vtable indirection per call, zero allocation. Do not
box per call, and do not use generics here — the set is runtime-configured.

**Eligibility bitset.** `fixedbitset` or a hand-rolled `Vec<u64>`, sized for the
full position universe at startup. Flipping a position's eligibility is a bit
operation, and the `Unfundable` band check (GUIDE 08) is a single load and mask.

**No `unwrap` on availability.** A source that has not been initialized returns
`U256::ZERO`, not a panic. Missing configuration must degrade to "cannot fund
from here", never to a node outage — this crate runs inside the Reth process
like everything else.

## Step 5 — The eligibility filter

This is the part that changes the whole system's shape. A position is a **target**
only if its debt is flashloanable at the size you would need.

```rust
pub struct Eligibility {
    /// Per position: can any repay option be funded, and by whom?
    /// Recomputed when flash liquidity moves materially, not every block.
    pub fundable: BitSet,
    /// Cheapest viable route per (asset, size bucket).
    pub best: HashMap<(AssetId, SizeBucket), FlashRoute>,
}

pub fn is_eligible(
    q: &Quote, idx: &FlashIndex, routes: &RouteCache, haircut: Ratio
) -> Option<FlashRoute> {
    // Every (repay, seize) pair: the debt leg must be fundable AND the
    // collateral leg must have an exit. A multi-debt position may be viable
    // only on its second-choice debt asset.
    q.repay_options.iter().flat_map(|&debt| q.seize_options.iter().map(move |&c| (debt, c)))
        .filter(|&(_, coll)| routes.has_exit(coll, q.size_hint()))   // leg 3
        .filter_map(|(debt, _)| idx.best_route(debt, q.max_repay_for(debt), haircut))
        .max_by_key(|r| r.net_of_bonus())   // D26: maximise bonus − exit cost,
                                           // NOT minimise exit cost. See below.
}
```

**Eligibility is two-sided.** Flash fundability is necessary, not sufficient. The
cycle has two market-facing legs and a position is only a target if *both* work:

```
leg 1  flash-borrow DEBT      ← this crate
leg 3  swap COLLATERAL → DEBT ← GUIDE 12's route depth
```

A position with perfectly flashloanable debt but collateral that has no exit
route at acceptable slippage is just as untargetable as one you cannot fund. So
`is_eligible` must consult the route cache as well as the flash index, and a
position is `Unfundable` if *either* side fails.

The two sides also fail independently and for different reasons — flash depth
moves with other searchers' borrowing, route depth with pool liquidity — so
re-evaluate on either signal, not just on flash liquidity changes.

Three further consequences worth stating plainly:

1. **The tracked universe shrinks.** Positions whose debt is an illiquid
   long-tail asset are not targets, however unhealthy they get. GUIDE 08's band
   manager should deprioritize them rather than spending recompute budget on
   positions you can never act on.
2. **Eligibility is dynamic.** A position can become eligible because someone
   deposited into a Uniswap V4 pool. Treat flash liquidity as an input to the
   opportunity set, not a static property.
3. **`repay_options` earns its place.** Multi-debt positions are frequently
   fundable on a smaller, more liquid debt leg when the largest one is not.
   Checking only the biggest debt silently discards real opportunities.

**Do not hard-delete ineligible positions from the store.** Mark them
`Band::Unfundable` and keep tracking cheaply. Eligibility flips back, and
re-backfilling a position you dropped is far more expensive than carrying it.

## Step 6 — Source selection

Rank by **effective total cost**, not headline fee:

```
effective_cost = notional · fee_bps
               + gas_overhead · gas_price
               + failure_risk_premium
```

- `gas_overhead` measured per provider from real transactions (GUIDE 10's gas
  snapshots), not estimated. Uniswap V4's unlock/settle dance is cheap; Aave's
  `flashLoan` array form is not.
- `failure_risk_premium` prices the chance the source is drained before
  inclusion. Empirical, per source, from your own `InsufficientLiquidity` rate.

Keep a **fallback chain**, not a single choice. Encode the plan with the primary
source; if simulation (GUIDE 11) fails on liquidity, re-encode against the next
source. On a baremetal box (GUIDE 16) you can simulate two or three source
variants in parallel and take the first that passes — you have the cores.

## Step 7 — Multi-source cascade (sequential sibling groups, never nested)

For very large positions no single source may hold enough. **Do not nest
callbacks.** `Executor.sol` already runs flash groups sequentially: each group
borrows, liquidates, swaps to repay, and settles before the next starts
(PLAN-ENCODING §1b′, D32). Multi-source funding is therefore **sibling groups
sharing a debt asset**, not a call-stack nest.

```
group 0: UniV4.unlock → take(DEBT, x) → liquidate (partial) → settle(x)
group 1: Aave.flashLoanSimple(DEBT, y) → liquidate (rest) → repay Aave
group 2: (optional third source) …
```

Rules (authoritative):

1. Cascade **only** when the cheapest single source's `planable` < need.
2. Sort sources by effective cost; from each, take `min(planable_i, remaining)`.
3. At most **3** sources / groups for one debt asset in one plan.
4. Split `repayAmount` (or legs) across groups so each group's flash covers its
   own liquidations. Same borrower may appear in multiple sibling groups.
5. Prefer a smaller single-source liquidation when cascade economics are close —
   a partial that lands beats a full plan that reverts.
6. Aave's array-form `flashLoan` (several *assets* at once) is unrelated — do not
   confuse it with multi-*provider* cascade.

Acceptance: a fork fixture where need exceeds every single source but fits two
sources under the 99% buffer must encode two sibling groups and succeed without
nested callbacks.

---

## Acceptance criteria

- [ ] All five in-scope sources implemented behind `FlashSource`; adding another touches
      one file. Balancer is not implemented. Sky DSS returns 0 available for non-DAI.
- [ ] `FlashIndex.available()` is a flat indexed lookup, no allocation, < 100 ns
- [ ] Index updates incrementally from the ExEx log stream; **zero RPC polling**
      (assert with a provider that panics on network access)
- [ ] Reported availability matches on-chain reality within 1% across 1,000
      sampled blocks, for every source
- [ ] Eligibility filter checks **all** `repay_options`, proven by a fixture with
      a three-debt-asset position fundable only on its second option
- [ ] Eligibility is **two-sided**: a fixture with fully flashloanable debt but
      collateral that has no exit route at acceptable slippage is marked
      `Unfundable`
- [ ] Eligibility re-evaluates on route-depth changes, not only flash-liquidity
      changes
- [ ] Ineligible positions are marked, not deleted; a fixture proves eligibility
      flips back when liquidity returns
- [ ] Source selection ranks by effective cost; a test proves Uniswap V4 is
      chosen over Aave at equal depth, and Aave over V4 when V4 is shallow
- [ ] Gas overhead per provider measured from real fork transactions, recorded
      in config, and asserted in CI
- [ ] Safety haircut configurable; `InsufficientLiquidity` sim failures tracked
      as the tuning signal
- [ ] Multi-source cascade (≤3 sequential sibling groups, same debt asset) works
      on a fork for a size exceeding every single source — **no nested callbacks**
- [ ] Fallback chain re-encodes against the next source on liquidity failure

## Failure modes

| Symptom | Cause |
|---|---|
| Frequent `InsufficientLiquidity` reverts | Haircut too generous, or index polled rather than log-driven |
| Losing bids by small margins | Always using Aave's 5 bps when a 0-fee source had depth |
| Liquidatable positions never targeted | Eligibility checks only the largest debt asset |
| Universe shrinks and never recovers | Ineligible positions deleted instead of marked |
| Large liquidations always fail | Multi-source cascade not implemented; no partial fallback |
| Gas model wrong per provider | `gas_overhead` estimated instead of measured |

## Handoff

GUIDE 08 gates the band manager on eligibility. GUIDE 10 implements the callback
shapes on-chain. GUIDE 12 consumes `FlashRoute` in the profit model — and note
the fee is now an unavoidable line item in every single liquidation, which moves
**both** edges of the viability band (GUIDE 12 §4b) — it is a linear cost, so it
tilts the profit line rather than creating an edge of its own, but flash *depth*
is one of the two things that sets the upper edge.
