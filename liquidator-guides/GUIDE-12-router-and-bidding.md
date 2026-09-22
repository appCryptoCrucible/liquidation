# GUIDE 12 — Router, Profit Model & the Bid

| | |
|---|---|
| **Crate** | `liq-router` |
| **Prerequisites** | GUIDE 04, 07, 08, 11 for the build; **shadow data (09B) only for Step 5/6's fits** |
| **Est. effort** | 3–4 weeks, then ongoing |
| **Blocks** | 13; and 09's shadow run cannot start without this crate |
| **Work packages** | **12A-1** routing + band + the real `RouteCache`; **12A-2** profit, selection, assembly, gas oracle, learning-phase bid; **12B** `F(β)`, `p`, timing — data-gated on 09B (D47) |

## Objective

Turn a candidate into a sized, funded, routed, priced plan — and decide what to
bid.

**On mainnet-with-SVR this is a core competency of the system.** The auction is
decided on value rather than arrival order, so what separates operations is
sizing, source selection, routing and bidding correctly under time pressure.
This guide deserves more ongoing attention than any other.

Latency is the complement, not the alternative: it determines **how many
milliseconds you get to make this decision** before the block seals (GUIDE 16
§4b, Path B). Colocation buys you thinking time; this guide is what you do with
it. And for the eight non-auctioned trigger classes, latency is decisive on its
own.

## Build vs. buy

Two libraries behind one `RouteSolver` trait: `amms-rs` for pool discovery and
state sync from the log stream, and **`Dex-Math-Core-rs` for exact quoting** —
V2/V3/V4, Curve StableSwap, Kyber Elastic (Balancer weighted **out of scope** — library may still compile it; do not route through it), deterministic
integer math with fail-closed errors (`DEPENDENCIES.md` §1).

Exact quoting is what makes the search in Steps 2–3 correct. Simulation
(GUIDE 11) gates the plan you chose; it cannot evaluate the space of routes,
splits and sizes you chose *from* — you cannot simulate a search space inside the
latency budget. At near-maximal bids that distinction decides outcomes:
**bid accuracy is swap accuracy.**

Curve coverage matters more than it looks. Seized collateral is
frequently a stablecoin or LST whose best exit is a Curve pool, and you do not
choose the collateral — the position does. A coverage gap is an opportunity you
cannot price.

Build sizing, the profit model, the timing optimizer and the bid model.

---

## Step 1 — The closed loop, and what binds it

Every liquidation is the same four legs, and each imposes a size ceiling:

```
1. flash-borrow DEBT     → ceiling: flash availability (GUIDE 07)
2. repay debt, seize     → ceiling: protocol close factor / target-HF solve
3. swap COLLATERAL→DEBT  → ceiling: route depth at acceptable slippage
4. repay flash + fee     → ceiling: none, but the fee is an unavoidable cost
```

```rust
fn size(q: &Quote, route: &FlashRoute, cons: &Constraints) -> U256 {
    min4(
        q.max_repay,                                            // leg 2
        route.available_after_haircut(),                        // leg 1
        cons.route_depth_at_slippage(q.seize_asset, q.repay_asset, cons.max_slip), // leg 3
        cons.per_liquidation_notional_cap,
    )
}
```

The binding constraint is **usually leg 1 or leg 3, rarely leg 2**. That is the
practical difference flashloan-only funding makes: with inventory you sized to
the protocol cap and worried about slippage; now flash depth is a co-equal
constraint and it moves block to block.

**A partial liquidation that lands beats a full one that reverts.** When leg 1 or
3 binds below `max_repay`, take the smaller size rather than skipping — a 20%
liquidation at full bonus is real money.

## Step 2 — Joint optimization over repay and seize legs

Both `repay_options` and `seize_options` (GUIDE 01) are sets, so this is a small
search, not a lookup:

```rust
fn best_plan(q: &Quote, flash: &FlashIndex, routes: &RouteCache) -> Option<Plan> {
    q.repay_options.iter().flat_map(|&debt|
        q.seize_options.iter().map(move |&coll| (debt, coll))
    )
    .filter_map(|(debt, coll)| {
        let f = flash.best_route(debt, q.max_repay_for(debt))?;   // leg 1
        let r = routes.best(coll, debt, size)?;                   // leg 3
        Some(evaluate(q, debt, coll, f, r))
    })
    .max_by_key(|p| p.net_profit)
}
```

Typically 1–12 combinations, evaluated serially on the hot thread — see Step 3b
for why spawning per combination is slower, not faster, at this count.

Two payoffs specific to this design:

- A position whose largest debt is unfundable may be fully fundable on a smaller
  debt leg. Checking only the biggest debt silently discards opportunities.
- The cheapest flash source depends on which debt asset you choose, so source
  selection and repay-leg selection must be decided **together**, not in sequence.

## Step 3 — Two-tier route solving

An exact solve does not fit the latency budget.

**Warm tier** (background, continuous, no latency budget): best-route cache for
every `(seize_asset, repay_asset)` pair at notional buckets
(`$10k / $100k / $1M / $5M`), refreshed on pool state changes from the ExEx log
stream. Answers ~95% of queries. On baremetal, widen this aggressively — cache
every pair across every tracked protocol's collateral set, and refresh more
often. Memory is not your constraint.

**Exact tier** (hot path, winning candidate only): water-fill allocation across
the candidate pool set (Step 4), bounded-depth over the pre-built DEX graph with
a hard time budget. On timeout, fall back to the warm allocation and let
simulation catch a bad outcome.

Cache **pool sets and their liquidity shape**, not single best routes — the warm
tier's job is to hand the exact tier a short list of pools worth water-filling
over, since the optimal allocation depends on a size you only know at trigger
time.

Aggregator APIs may feed the warm tier. **Never call one on the hot path** — an
HTTP request in the critical section is an automatic loss.

### Do not route swaps through Uniswap V4

V4 swap behaviour is **pool-dependent**. Hooks fire on `beforeSwap`/`afterSwap`
and can alter amounts, impose extra fees or replace the curve entirely
(`beforeSwapReturnDelta`), so there is no generic V4 quote — only a per-hook one.
`Dex-Math-Core-rs` implements the base V4 math, not hook math, and a base-math
quote against a hooked pool is wrong in a direction you cannot predict.

Route the collateral exit through **V3 and Curve** (and Kyber Elastic where the math crate quotes it), where quoting is
deterministic from pool state. V3 liquidity plus gas-aware splitting (Step 4) is
sufficient for realistic exit sizes.

If a specific V4 pool ever becomes worth routing through, treat it as a per-pool
integration: model that hook explicitly, parity-test it against revm
(GUIDE 05 Step 6b), and allowlist that pool alone. Never quote V4 generically.

**This does not affect V4 as a flashloan source.** Hooks fire on swap and
liquidity operations, not on the PoolManager's `take`/`settle` accounting, so a
borrow-and-return through `unlock` never touches hook logic. V4 stays the
preferred zero-fee flash source (GUIDE 07) while being excluded as a swap venue —
those are separate decisions about the same contract.

## Step 3b — Rust: concurrency & safety

This crate has two halves with opposite concurrency shapes, and conflating them
is the main mistake available here.

**Hot half — runs on the ExEx thread, synchronous.** Sizing, the combination
search, the profit model and the bid computation are called from the engine and
hold `&` to the store, the flash index and the route cache. No locks, no async.

**Warm half — background thread, publishes by `arc-swap`.** The route cache
builder reads pool state, solves, and republishes:

```rust
// Owned in `Shared`, built once at startup, handed out as `&'static`.
// NOT a `static` + `LazyLock`: that buys an acquire load and a branch on every
// access to avoid an ordering problem this process does not have.
// RUST-CONVENTIONS.md §3.3.
// `shared: &'static Shared` holds `routes: ArcSwap<RouteCache>`.

// Background builder thread.
shared.routes.store(Arc::new(rebuilt));

// Hot path: a field access, then one atomic load. No init check, no contention.
let routes = shared.routes.load();
```

An `RwLock<RouteCache>` would make hot-path readers contend on the lock word with
a writer that runs continuously. `arc-swap` readers never block and never
interact with the writer at all — the writer pays by allocating a new cache per
publish, which is free off the hot path.

**The combination search runs serially on the hot thread by default.** An
earlier draft spawned a scoped thread per combination. Do not: `clone(2)` plus
a stack `mmap` is 15–40 µs per thread on Linux, so twelve spawns cost more than
evaluating twelve combinations, which is exact integer math over a cached route
set at a few microseconds each. The parallel version was slower than the serial
one it replaced, and it broke the pinning model — an unpinned child thread
lands wherever the scheduler puts it.

```rust
// Hot thread. Borrows everything; no Arc, no clone, no spawn.
let best = combos.iter()
    .filter_map(|c| evaluate(c, &quote, &flash, &routes).ok())
    .max_by_key(|p| p.net_profit);
```

`evaluate` returns `Result`, and a failed combination is "unavailable", not a
panic — the hot thread never unwinds (RUST-CONVENTIONS §4).

If the 16C benchmark shows the serial search exceeding its slice of the Path B
budget (it should not at ≤ 12 combinations), the fix is the **pre-spawned,
pinned worker pool with `rtrb` rings that GUIDE 11 already runs for simulation
variants** — hand combinations to idle sim workers between simulations. Never
`rayon` (its global pool fights the pinning) and never `std::thread::spawn` on
the hot path. The acceptance line "completes within budget at 12 combinations"
is measured on the serial version first.

**Floats are allowed here, and only here.** `clippy::float_arithmetic` is denied
workspace-wide because money math must be exact. The bid model's `F(β)` estimation,
the competitor-arrival distribution and the EV integration are statistics, not
money — `#[allow(clippy::float_arithmetic)]` on those functions specifically,
with a comment saying why. **The resulting bid is converted back to an integer
`U256` before it touches a transaction**, and that conversion rounds down.

**`SmallVec` for combinations.** `repay_options × seize_options` is bounded small;
allocate it on the stack.

## Step 4 — Profit model

Compute it from the **exact quote**, not from a bonus percentage. The quote
internalizes both swap costs at once and they are separate, material line items:

```
s           = debt repaid (principal flashed, before flash fee)
seized      = collateral received from the liquidation
              // debt-numeraire value before exit costs ≈ s · (1 + bonus_rate)
swap_out    = exact_quote(seized, route)   // pool FEE and price IMPACT on SEIZED, not on s
flash_owed  = s + flash_fee                // flash_fee is 0 on V4 / Morpho / Sky DSS (today)
gas_cost    = simulated gas · base_fee     // (priority fee modelled separately)

// Objective (D36 / bidding): maximise net_bundle_profit across the plan.
net_bundle_profit = Σ legs (swap_out − flash_owed) − gas_cost_total
searcher_net      = net_bundle_profit − bid

// Per-leg shorthand used below (gas attributed at plan level for the band):
gross_leg = swap_out − flash_owed
```

**Do not charge exit fee/impact on `s`.** You swap the seized collateral, whose
debt-numeraire size is `s·(1+bonus_rate)` (approximately — prefer the exact seized
amount from the quote). Charging fee and impact on `s` alone systematically
understates cost and overstates `net_bundle_profit` when maximising that objective.

**Pool swap fees are a first-order cost, not a rounding detail.** A 30 bps tier on
a $100k exit is $300. Against an $8,000 gross bonus that is 3.75% — at a
99%-of-net bid it dwarfs the margin entirely. Fee tier is therefore a *route
selection* input, not something you discover after choosing:

| Venue | Typical fee | Where it wins |
|---|---|---|
| Curve StableSwap | ~1–4 bps | stable→stable, LST→base. Usually the cheapest exit for the collateral you actually seize |
| Uniswap V3, 100 tier | 1 bps | correlated pairs |
| Uniswap V3, 500 tier | 5 bps | majors |
| Uniswap V3, 3000 tier | 30 bps | long tail — often the only venue, and it hurts |

On a USDC-collateral → USDT-debt exit, Curve at ~1 bp against a V3 500 tier is
4 bps — on $100k that is $40, comparable to the entire gas cost. Route choice by
fee tier is worth as much as the gas oracle.

### Price impact is usually the largest cost — split across N pools

At realistic exit sizes, impact dominates everything else combined:

| Cost | Typical magnitude |
|---|---|
| Gas | $2–20 at calm base fees |
| Flash fee | 0 (V4 / Morpho / Sky DSS today) to 5 bps |
| Swap fee | 1–30 bps |
| **Price impact** | **1–5 bps on a small exit into deep liquidity; 50–300 bps on a large exit into mid liquidity** |

Impact is superlinear in size against a single pool, so the fix is to split one
deep excursion into several shallow ones. **Exact math is what makes this
computable** — and it is the one job an approximate model cannot do at all,
because on concentrated liquidity the optimization depends entirely on knowing
*where the marginal rate jumps* at tick boundaries. `Dex-Math-Core-rs` gives you
those exactly; a constant-product approximation of a V3 pool does not.

### The allocation algorithm

Optimal allocation equalizes **marginal cost** across every pool you use — not
proportional splitting, not equal splitting. Water-fill on the marginal rate λ:

```
g(λ) = Σᵢ absorbed_i(λ) − total        // find the root
```

Because each pool has a fixed fee tier, equalize marginal `(fee + impact)`, not
impact alone — a 1 bp Curve pool and a 30 bps V3 pool reach the same marginal
cost at very different allocations.

#### The shape of `g`, which decides the method

| Property | Consequence |
|---|---|
| **Monotone non-increasing in ρ** | The root set is an interval (a plateau, or a jump across zero), not necessarily one λ. What is pinned is the allocation: floor, cap, and the residual to the best-ρ₀ pool. |
| **Continuous but not C¹** | `absorbed_i` is smooth *within* a V3/V4 tick range and kinks at every boundary, because liquidity `L` jumps. `g` inherits kinks at the union of all crossed ticks. |
| **Exact integer arithmetic** | At the finest resolution `g` is a step function, differentiable nowhere in the strict sense. |

Those last two are what rule methods in and out.

#### Method comparison

**Newton–Raphson — no, not as the top-level solve.** It needs `g′(λ)`, which does
not exist at tick boundaries; near a kink it oscillates or overshoots out of the
bracket. Getting the derivative is also awkward: closed-form per AMM means
implicit differentiation of Curve's `D` invariant, and finite differences
reintroduce float error and cost two evaluations per step. Unsafeguarded Newton
on a piecewise function is how you get a router that is fast on 99 routes and
diverges on the hundredth.

**Brent — the right shape, not the implementation.** Derivative-free, maintains
a bracket, superlinear (~1.6) on smooth stretches, and degrades to bisection at
kinks. The code's generic fallback is Illinois regula falsi (order ≈ 1.44,
bisection on stall), not `brentq`. Brent's inverse-quadratic step needs a
three-point rational interpolant that overflows 512-bit arithmetic at Q96 × wei.
Floats are denied. The property this section requires is the bracket, not the
1.6 constant.

One correction worth making explicit: **Brent's root-finder falls back to
bisection, not golden section.** Golden section belongs to Brent's *minimization*
method (golden section + parabolic interpolation). This problem is a root find on
`g(λ) = 0`. An agent told to implement "Brent with golden section fallback" will
produce something that does not converge correctly here. Do not replace the
Illinois fallback with `brentq` either: the inverse-quadratic step overflows
the 512-bit integer path.

**Plain bisection — only as Brent's internal safety net.** On its own it needs
~40+ iterations for useful precision, and each iteration is a quote per pool. At
4 pools and ~1 µs per quote that is ~160 µs against a ~300 µs routing budget.
Brent's ~10–15 iterations puts it near 50 µs. The choice is not academic.

#### Better: exploit the structure instead of solving numerically

The kinks are **at known locations** — tick boundaries — so you do not have to
discover them with a general-purpose root finder:

1. Enumerate candidate tick boundaries across the V3/V4 pools in the plausible λ
   range, and sort them. These partition λ-space into intervals on which `g` is
   genuinely smooth.
2. **Binary search the sorted breakpoint list** to find the interval containing
   the root — `O(log K)` exact evaluations, no tolerance parameter.
3. Inside that interval, solve directly: closed form where available, or two
   Newton steps, which are now safe because there is no kink in the interval.

This terminates *exactly* rather than to a tolerance, and it is the approach good
V3 routers use — walk ticks, do not numerically hunt.

#### So the method is per-AMM, with Brent as the fallback

| Family | Method |
|---|---|
| Uniswap V2 (2-token) | **Closed form** — solve the invariant directly |
| Uniswap V3 / V4 | **Tick-walk**: enumerate breakpoints, binary search, closed form within the interval |
| Curve StableSwap | **Newton on the smooth invariant** — no ticks, and Curve's own `get_dy` already solves `D`/`y` this way |
| Heterogeneous pool sets | Union the V3 breakpoints, treat Curve as smooth contributions, solve within each interval |
| Anything without exploitable structure | **Brent**, bracketed |

#### You do not need λ to wei precision

The precision requirement is on the **allocations summing to the total**, not on
λ itself. So solve λ loosely — 1e-6 relative is ample — compute the allocations,
and assign the rounding residual deterministically to the pool with the best
marginal rate.

That removes the precision-driven iteration count entirely, and it is what
production routers do. Chasing λ to the last wei is solving the wrong problem
expensively.

### Choosing the pool set is discrete, and gas decides it

Allocation across a *given* set is continuous; which pools to include is not.
Each additional pool costs roughly 80–120k gas — a few dollars at calm base fees.

Greedy, and it terminates quickly:

1. Rank candidate pools by marginal rate at zero size (best price first)
2. Add the next pool; re-run the water-fill across the included set
3. **Stop when the next pool's gas exceeds the impact it saves**

Practically that settles at 2–4 pools for most exits, more only on large ones.
The stopping rule is what prevents the classic failure: optimizing impact in
isolation over-splits small liquidations, which are most of them. On a $5k exit,
$3 of extra gas against 60 bps of saved impact is a clear loss; on $100k against
3 bps it is a clear win.

The objective is `swap_fee + price_impact + hop_gas`, minimized jointly.

### Evaluate many, simulate few

Keep the distinction sharp. **Evaluating** a split candidate is exact math in
microseconds — do it for the whole search space of `(paths × allocations)`.
**Simulating** is revm at ~150 µs–1 ms — you cannot do that across the space.

On dedicated cores (GUIDE 11 Step 4b) the practical shape is: water-fill to a
handful of top candidates with exact math, simulate the best 2–3 in parallel,
take the first that passes with its `minProfit` intact. The math does the search;
simulation gates the answer.

### Better routing raises the sizing ceiling

This is the part that is easy to miss. Route depth at acceptable slippage is one
of the four size ceilings in Step 1, and it is frequently the binding one. A split
route absorbs more size at the same slippage than any single pool, so **improving
the router does not only cut cost — it lets you liquidate more of the position.**

On a partial liquidation bound by leg 3, a better split converts directly into a
larger repay, a larger bonus, and a larger bid you can afford to make.

`flash_fee` is no longer optional. At 5 bps on Aave with ~$40 of gas, a $50k
liquidation at a 5% bonus nets ~$2,435 before the bid; the same liquidation at
0 bps from Uniswap V4 nets ~$2,460. Small in isolation, decisive across thousands
of auctions.

## Step 4b — The viability band (there is no minimum viable notional)

An earlier version of this guide told you to compute a `min_viable_notional` per
`(protocol, debt asset)` and cache it. **Delete that idea.** It is wrong in three
ways at once, and the way it is wrong is instructive.

### Why a stored threshold is the wrong object

**It is derived, not input.** You can compute exact net — exact swap math from
`Dex-Math-Core-rs`, exact next base fee from the EIP-1559 formula, exact flash fee
from whichever source has depth. So "is this profitable" is a direct computation
and the threshold is just where that computation crosses zero. Caching it keeps a
second, staler representation of something derivable exactly — the same
two-sources-of-truth mistake as encoding a blob offset you could compute.

**It is keyed wrong.** Bonus rate lives on the *collateral* (per-reserve, e-mode
dependent), and exit liquidity is a property of the *(collateral, debt) path*.
Keying on debt asset alone drops both.

**Any threshold in token units is meaningless across assets.** 0.1 USDC and 0.1
WBTC are the same number and nothing like the same decision. That one is obvious
once stated; the first two are not.

### One threshold becomes a band, because the cost terms have different shapes

```
seized_equiv(s) = s · (1 + bonus_rate)     // debt-numeraire size of what you swap
net(s) = s·bonus_rate
       − fee_rate · seized_equiv(s)        // swap fee on seized, NOT on s
       − impact(seized_equiv(s))           // impact on seized, NOT on s
       − s·flash_rate
       − gas
       = s·bonus_rate − s·(1+bonus_rate)·fee_rate − impact(s·(1+bonus_rate))
         − s·flash_rate − gas
```

Equivalently, with exact quotes: `net(s) = exact_quote(seized(s)) − s·(1+flash_rate) − gas`.

- **Gas is fixed**, so it dominates at small `s` → the **lower** edge
- **Impact is convex in seized size**, so it dominates at large `s` → the **upper** edge
- Swap fees (on seized) and flash rate (on `s`) are linear: they tilt the line and
  move both edges, but neither one creates an edge by itself

**Lower edge is per `(ProtocolId, collateral, debt)`** — flash fee + exit costs from
swap math + swap fees, versus gas — not a global scalar (D28 / `BandTable`).
**Upper edge** is where convex impact extinguishes bonus after those linear costs.

Lumping impact in with fees, or charging either on `s` instead of seized, hides the
band shape and biases bids when maximising `net_bundle_profit`. Both edges move every
block.

### Everything reduces to ETH

Profit is realised in the debt token; gas is paid in ETH. ETH is the only
numeraire both sides share, so the comparison is always made after conversion —
which is why no per-token threshold can exist.

**Gas is a cost; the bid is not.** The bid is a distribution of profit paid
*out of* net, not a cost subtracted before it. Keep them separate or you get a
circularity — net depends on the bid, the bid is a fraction of net. Gas cost
for accounting is `(base fee at the block + priority fee) × gas used`. The two
fees stay separate inputs. The builder's `maxFeePerGas` is the base-fee
inclusion ceiling and does not have the priority added into it;
`maxPriorityFeePerGas` carries the priority on its own.

### It is a join, not a new subsystem

The band is four things you already maintain, joined and materialised:

| Input | Where it lives | Keyed on |
|---|---|---|
| flash depth + fee | `FlashIndex` (GUIDE 07) | **debt token** |
| exit liquidity | warm route cache (Step 3) | **(collateral, debt) pair** |
| bonus rate | registry / adapter | (protocol, collateral) |
| base fee and priority fee | gas oracle (Step 6) | per block, stored as two fields. Accounting cost is their sum × gas |

Emit it from the **warm tier**, which already refreshes on pool state changes from
the ExEx stream and already runs off the hot path. Do not build a parallel
service, and do not call it an oracle — that word already means something specific
here (GUIDE 06) and an agent will wire it into the wrong subsystem.

```rust
pub struct ViabilityBand {
    pub min_size: U256,     // below this, gas dominates
    pub max_size: U256,     // above this, impact dominates
    pub base_fee: u64,      // the base fee this band was computed for
    pub block: BlockNum,
}

// Keyed per pair, not per token.
type BandTable = HashMap<(ProtocolId, AssetId /*coll*/, AssetId /*debt*/), ViabilityBand>;
```

### Recompute per block — you do not need gas regimes

Next base fee is a pure function of the parent block, so you know it **exactly,
one block ahead**, and it can move at most ±12.5%. Rather than detecting a regime
change after the fact, recompute during the inter-block window for the base fee
you already know applies next. Twelve seconds and the whole core pool for a few
hundred pairs.

Bucketing into gas regimes is a reasonable fallback if the impact-side walk proves
expensive, but start exact — regimes introduce staleness to solve a problem you do
not have.

### Manipulation resistance, sized to the actual exposure

The band is a **pre-filter, not the execution decision**. A manipulated band leads
to an exact computation, and if that is wrong `minProfit` drops the bundle for
free. So manipulating the band cannot cost you money — only missed opportunities
and wasted solver cycles. **The money is protected on-chain, not here.**

That leaves griefing: suppress apparent liquidity to exclude you from a market.
Real, but it costs the attacker sustained capital and only pays if they take the
liquidation themselves. Worth a cheap defence, not an elaborate one.

**Use `min(spot, twa)` on both liquidity and price.** Conservative in both
directions: inflated spot does not fool you because the TWA is lower, and a
genuine collapse is respected immediately because spot is lower. You give up only
the case where depth truly improved in the last few blocks.

That last point matters more than the manipulation case. **A plain TWA degrades
exactly when you need it most** — average liquidity over N blocks, then a real
collapse during a cascade when everyone pulls at once and opportunities are
densest, and your band claims depth that is not there. You oversize, the guard
catches it, and you lose an opportunity you could have taken at the right size.

A band that includes an unviable size only spends a solver cycle: exact net and
`minProfit` drop it. A band that excludes a viable size loses the trade. The
published edges are therefore kept on the viable side, so the interval sits
**inside** `{net ≥ 0}`. It does not lean generous.

### What replaces the old number operationally

Nothing, at execution time: compute exact net, check the sign.

The band exists for two different decisions that the old number conflated:

- **Tracking** — you cannot run exact net on every position every block, so the
  band is the cheap admission filter. Being wrong here costs a marginal
  opportunity, not money, so crude is fine.
- **Slot allocation** — with twenty nonce slots and three opportunities, take all
  three however small; with thirty opportunities, take the best twenty. The floor
  is **opportunity cost**, which is zero when you have spare capacity, so it
  tracks slot pressure rather than being a constant. See GUIDE 13 Step 4.

## Step 4c — Batching: several positions, several collaterals, one flashloan

Two situations produce the same shape, and the machinery is identical:

- **N positions sharing a debt token.** Alice, Bob and Mary all owe USDC and all
  cross at once. One flashloan covers all three.
- **N collaterals on one position.** Aave accounts are cross-collateralised, so a
  single borrower may post ETH, wBTC and an LST against one USDC debt. Each
  `liquidationCall` seizes one collateral, so taking the position properly means
  several calls.

Either way: one flashloan in the debt asset, several liquidation legs, several
collaterals to convert back. `PLAN-ENCODING.md` §1b/§1c carries it and
`Executor._core` executes it. **A single position is `liqCount == 1`** — there is
no separate path, which is what keeps the batched path tested.

### What it buys

One flash fee instead of N. Gas amortised across one callback and one repay.
One bundle's worth of information exposure. And the one that is easy to
undervalue: **one nonce slot instead of N**. Slot pressure binds during cascades,
which is exactly when positions cross together — so the saving is largest
precisely when the resource is scarcest.

### Sizing: over-borrow deliberately

`flashAmount` should exceed the sum of the repays whenever a leg might be taken
first. With a zero-fee source the surplus costs nothing — it round-trips
untouched and is repaid with everything else. Size the flash to the exact sum and
a single lost leg leaves you unable to repay.

The cost only appears if `FlashIndex` (GUIDE 07) routes you to a fee-charging
source, because then you pay on the unused portion too. That is a real input to
source selection at batch size, not an afterthought.

### Failure tolerance is the whole design

Batching turns N independent opportunities into one correlated one, and the
correlation is worst during a cascade, when individual races are most likely to
be lost. Without per-leg tolerance, one competitor beating you on Alice costs you
Bob and Mary as well.

So each leg is attempted inside `try/catch`, a failed leg is skipped, and
`minProfit` judges the batch as a whole. Two consequences for the solver:

- **Set `minProfit` as a floor, not an expectation.** If it encodes the
  all-legs-succeed outcome, any partial fill reverts the batch — which is free,
  but throws away a profitable partial. Size it to the worst partial you would
  still accept.
- **Zero successful legs reverts.** `AllLegsFailed`. The bundle is dropped and
  costs nothing.

### The routing problem is the hard part

Step 4's water-fill solves one collateral into one debt asset. A batch has K
collaterals into one debt asset, and their paths **share output pools** — an
LST→USDC route likely passes through ETH/USDC, which is also where ETH→USDC
goes. Solve them independently and each solver assumes it has the whole pool,
so every quote understates impact, and it understates in the direction that
inflates net and makes you overbid.

The resolution is simpler than a joint convex program, because of how the legs
actually run: **swap legs execute sequentially on-chain, each seeing the state
the previous one left.** So a sequential simulation is not an approximation — it
is exact for a given ordering.

```
for each candidate ordering of the K collaterals:
    state = current pool state
    cost  = 0
    for collateral c in ordering:
        alloc  = water_fill(c, state)        // Step 4, unchanged
        cost  += cost_of(alloc, state)
        state  = apply(alloc, state)         // subsequent legs see the impact
    keep the cheapest ordering
```

The inner call is the Step 4 water-fill you already have. What changes is that
it runs against a *progressively displaced* state rather than the block's state,
and that ordering now matters.

With K ≤ 4 the permutations are cheap enough to evaluate exhaustively; beyond
that, largest-notional-first is a good heuristic — it places the leg with the
most impact where it faces the deepest book. This is the same
"evaluate many, simulate few" discipline as the single-collateral case, one level
up.

**The encoder closes each collateral exactly once.** Every collateral gets
precisely one swap leg flagged `TAKE_BALANCE`; `PLAN-ENCODING.md` §2 asserts it,
because the contract cannot.

### Which collateral to seize is a maximisation, not a minimisation

Worth stating separately, because it applies whether or not you batch. Aave sets
`liquidationBonus` **per reserve**, and e-mode changes it again for correlated
pairs. So on a multi-collateral position, seizing wBTC at one bonus versus ETH at
another yields different revenue before exit cost enters the picture at all.

GUIDE 07's eligibility filter ends `.max_by_key(|r| r.net_of_bonus())` — bonus
value minus exit cost (D26; corrected in CHANGES Round 21b). The trap it avoids:
`min_by_key(effective_cost)` picks the cheapest exit rather than the best trade,
and on a position holding a thin-but-high-bonus collateral beside a
deep-but-low-bonus one, those give opposite answers. If you find a `min_by_key`
over an exit cost anywhere in `liq-flash` or `liq-router`, it is a regression.

Batching generalises this from "pick the best collateral" to "pick the best
*subset*" — seizing a second collateral is worth it only while its marginal
bonus exceeds its marginal exit cost against a book the first leg already moved.
That falls out of the sequential simulation above: extend the ordering by one
collateral, and keep it only if total net improved.

### Sizing the benefit

Batching is specified, but how much it is worth is an empirical question the
replay archive answers directly:

```sql
-- How often do positions sharing a debt token cross in the same block?
SELECT debt_token,
       COUNT(*)   AS blocks_with_batch,
       AVG(n)     AS mean_positions_per_batch,
       SUM(n)     AS total_positions_coverable
FROM (
  SELECT block, debt_token, COUNT(*) AS n
  FROM actual_liquidations
  GROUP BY block, debt_token
  HAVING COUNT(*) >= 2
)
GROUP BY debt_token
ORDER BY total_positions_coverable DESC;
```

Run it split by volatility regime — the cascade answer is the one that matters
and it will differ sharply from the all-time average. Pair it with the
distribution of collaterals per liquidated position, which sizes the second shape
independently of the first.

## Step 4d — Two outputs: repay the flash, take profit in ETH

The swap pass produces **exactly two** outputs, and which is which is decided by
the leg, not by the plan.

```
seized collateral ──┬─► debt asset, EXACT OUTPUT = flash_owed   (repay)
                    └─► WETH, take-balance                      (profit)
```

**Exact output on the repay leg.** Size it to the amount owed and let the swap
consume whatever collateral that requires. Exact-input would force you to
over-provision and strand debt-token dust, or under-provision and fail the repay.
V3 encodes the mode in the sign of `amountSpecified`, so it is the same call.

**Everything else to WETH.** Profit is denominated in ETH — always, whatever was
borrowed or seized.

### Why ETH and not the debt asset

The argument that settles it: **the bid must be paid in ETH regardless.** Builders
value a bundle by `coinbaseDiff`, the coinbase's *native balance* delta — an
ERC-20 sent to that address moves it by zero. At a high `bidBps` the bid is nearly
all of net, so that conversion is happening either way. Routing the remaining few
percent along with it is close to free and buys three things:

- **One numeraire.** `minProfit` is in wei, gas is in wei, the viability band
  (§4b) is already in wei. The guard and the cost it guards against are finally
  the same unit, with no conversion between them.
- **The executor stops being a vault.** It holds exactly one asset between
  transactions instead of an accumulating pile of debt tokens, which is the
  posture the contract was designed for — a proxy, not a treasury.
- **The bid funds itself.** No ETH float, no top-up loop, no working capital.
  Every liquidation pays its own bid out of its own proceeds, which keeps the
  flashloan-only invariant (D02) intact end to end.

**It is often cheaper, not dearer.** Most collateral routes to a stablecoin
*through* ETH, so stopping at ETH saves a hop rather than adding one. The case
that genuinely costs is stablecoin collateral against stablecoin debt, where
collateral→debt is a few bps and collateral→ETH is not. Price that case honestly
in the band rather than assuming the conversion is free everywhere.

**One consequence to accept deliberately:** profit in ETH means you are long ETH
between liquidation and disposal. For a bot whose costs are denominated in ETH
that is arguably a hedge — margin in ETH terms is stable regardless of price. If
you think in EUR it is exposure, and the answer is disposal cadence (GUIDE 14
Step 5b), not changing the denomination.

## Step 4e — The bid has three channels, not one

`bid = f(net)` is one number; *expressing* it is venue-specific.

| Channel | Where | Mechanics |
|---|---|---|
| **`block.coinbase` transfer** | builder bundles | Exact wei, computed on-chain from **realized** net. The default. |
| **Priority fee** | builder bundles | A *rate* — `priority_fee_per_gas × gas_used`. Keep it modest and nonzero. |
| **`refundConfig`** | MEV-Share / SVR | A refund percentage on the hinted transaction, not a payment you make. |

**Coinbase is the default because the bid tracks realized net.** Priority fee is
committed before the swap runs, so a quote that came in optimistic overpays out of
a margin you already spent down to 1%. A coinbase payment computed from the
measured WETH balance shrinks with the shortfall and the fraction you keep
survives. That asymmetry is the whole reason to do the work.

Keep the priority fee **modest but nonzero** — some builders treat a zero-priority
bundle as noise — and treat it as a floor rather than the bid.

Implementation notes: use `call{value: x}("")`, never `.transfer()`, because a fee
recipient that is a contract fails on the 2300-gas stipend. EIP-3651 pre-warms
`COINBASE`, so the payment costs ~9k gas rather than paying cold access on top —
that EIP exists precisely because this is the intended searcher pattern.

**SVR is not a payment at all**, which is easy to get wrong when generalising. You
do not transfer value; you specify what share of the backrun you return. The bid
model must produce a venue-appropriate expression, not a wei amount it then tries
to force into every channel.

## Step 4f — Candidate selection: the gate, then the ranking

This is the step most likely to be built wrong, and the wrong version looks
reasonable. Read the first rule twice.

### Eligibility is a gate. Ranking never expands it.

> A position enters the candidate set **only** if simulation against the updated
> state puts it at HF < 1. Ranking then orders that set and the gas budget
> truncates it. A position with excellent net-per-gas that is **not liquidatable**
> is not a candidate at any rank.

An agent told to "maximise net-per-gas across candidates" will otherwise assemble
a candidate set of *nearly*-liquidatable positions, because that makes the
optimisation richer. It is the wrong set.

**And it is self-punishing, which is the part worth internalising.** A failed leg
still burns gas — `try/catch` pays for the call whether or not it succeeds — and
that gas comes out of realized net, which is what the on-chain bid is computed
from. Padding a bundle with speculative positions does not merely waste space, it
**shrinks the bid you can afford** and makes you lose the auction you were trying
to win. Ignoring the rule produces a worse outcome, not a subtly wrong one.

### Within the gate: expected contribution-per-gas

Builders rank by value delivered, but they are filling a constrained block, so
what actually matters is value **per gas**. The plan objective remains
**maximise `net_bundle_profit`** (then bid a fraction of realized net).

`net` in Step 4 already subtracts `gas_cost`. The ratio is only a sort key.
Truncation is the separate Δnet test, which subtracts gas once. For a common
`p` and a common wei-per-gas, `p · (contribution − gas) / expected_gas` and
`p · contribution / expected_gas` differ by a constant and rank the same — the
first form does not, by itself, drop a leg that Δnet would keep. They stop
being order-identical once `gas_failed > 0` makes `expected_gas` not
proportional to `p`. The numerator the code uses is the pre-gas contribution:

```
contribution           = swap_out − flash_owed          // = gross_leg; NO gas subtraction
expected_gas           = p · gas_success + (1 − p) · gas_failed
expected_contrib_per_gas = p · contribution / expected_gas
```

Add / keep a marginal candidate when it raises expected `net_bundle_profit`:

```
Δ net_bundle_profit ≈ p · contribution − (marginal expected gas · (base_fee + priority_fee))  >  0
```

Do **not** truncate on the ratio. Truncate on Δnet.

`p` is the probability the position is still liquidatable when the bundle
executes. Start at `p = 1` — you cannot estimate it before you have data — then
fit it per bracket and contest class from the GUIDE 09 miss taxonomy, which
already records exactly the outcomes you would fit on.

**Falling `p` shrinks a batch only when `gas_failed > 0`.** At `gas_failed = 0`,
`expected_gas = p · gas_success`, so Δnet = `p · (contribution − gas_cost)` and
the sign does not change as `p` falls. With `gas_failed > 0`, expected gas
stops scaling with `p` and marginal candidates cross Δnet ≤ 0 on their own.
`gas_failed = 0` at learning `p = 1` is allowed; `p < 1` with `gas_failed = 0`
skips that candidate. A rule like "batch when more than N opportunities in a
block" is still the wrong shape — a single position is a batch of one — but
`p` alone is not a batch-size controller.

### Grouping and truncation

1. **Group** candidates by debt asset — each flash group funds one source; the
   multi-source cascade may emit ≤3 sibling groups for the same debt
   (PLAN-ENCODING §1b′, GUIDE 07 Step 7).
2. **Rank** within the plan by expected contribution-per-gas (pre-gas numerator).
3. **Truncate** when marginal Δ`net_bundle_profit` would be ≤ 0, or when the gas
   budget or available nonce slots run out — not when a double-gas net-per-gas
   metric goes soft.

**Read the gas limit from the block header.** It has gone 30M → 36M → 45M → 60M in
about eighteen months, and Glamsterdam targets 200M. Anything hard-coded is wrong
within a year — see `PRE-FLIGHT.md` §7.

### SVR forces batching; cascades only reward it

Worth separating, because only one is a choice.

In an SVR auction you are bidding for the right to backrun **one transaction**.
Everything you want behind it has to be in that one bundle — you cannot split it
into N bundles each backrunning the same hint, because only one wins the slot. So
aggregate net is what you bid with, and a competitor backrunning a single position
is not marginally worse off, they are playing a different game. **Batching there is
the competitive mechanism, not an optimisation.**

A cascade is many independent opportunities arriving together. Batching helps for
the ordinary reasons — one flash fee, amortised gas, one slot instead of N — but
nothing forces it, and the grouping above handles it with no special case.

### Degrading when candidates exceed solver capacity

The real cascade problem is not when to batch, it is what happens when fifty
candidates arrive and you can solve twelve exactly inside the latency budget.

Two-stage, reusing the structure Step 3 already has for routing: **crude expected
contribution-per-gas from the warm tier across all candidates, exact solve only for the top
K that fit the budget.** The warm tier already answers ~95% of route queries
without a latency budget; this is the same split applied one level up, to which
candidates deserve an exact solve at all.

## Step 5 — The V4 timing optimizer

Aave V4's bonus rises as HF falls. Firing at HF = 0.9999 earns
`bonus_at_threshold`; waiting earns more, up to `max_bonus`. Waiting also risks
losing the position.

```
EV(h) = P(still available at h) · [ repay(h) · bonus(h) − costs(h) ]
```

- `bonus(h)`, `repay(h)` — exact, from `Quote.curve`
- `costs(h)` — gas + slippage + **flash fee at that size**, which moves with `h`
  since `repay(h)` grows as health falls
- `P(still available at h)` — **must be measured**, from the `WaitedTooLong`
  observations in GUIDE 09. Fit per protocol, notional bucket and volatility
  regime.

Evaluate at 5–10 candidate firing healths, take the argmax, re-evaluate each
tick. Cheap, and not in the sub-millisecond path — you are deciding about
*future* ticks.

**Start conservative: fire immediately at HF < 1.** Enable waiting only after
≥ 200 `WaitedTooLong` observations. An unmeasured competitor model loses
positions invisibly.

## Step 6 — The bid model

Under SVR you are in a sealed-bid auction against searchers seeing the same hint.

### The gas oracle — a first-class component

Everything below depends on knowing what the transaction will cost *in the block
you are bidding into*. Build this as a real component, not a constant.

**Next block's base fee is computable exactly, not estimated.** EIP-1559 makes
`base_fee(n+1)` a pure function of block `n`'s gas used against target, bounded
to ±12.5%. You have block `n` the instant it commits (GUIDE 03), so you can
compute the exact base fee for the block your bundle targets:

```rust
fn next_base_fee(parent_base: u64, parent_gas_used: u64, target: u64) -> u64 {
    // EIP-1559: deterministic, ±12.5% per block. No estimation involved.
}
```

Anyone treating gas as an estimate is carrying an error term you do not have to.

**Priority fee is 1 gwei on every path.** It is not a rolling percentile and
not a second estimate beside the base fee. Contested and uncontested sends
use the same tip. The coinbase bid is still a fraction of net, not this tip.

**Track the regime, not just the number.** Base fee moves an order of magnitude
between calm markets and volatility, and volatility is when liquidations cluster.
Feed the live figure into the cost model on every evaluation; never cache it
across blocks, and never configure it.

Gas per liquidation comes from **simulation** (GUIDE 11), not a table. Cost is
then `gas_used × (base_fee + priority_fee)`, both known for the target block.

### The bid is an optimisation, and you have the data to solve it

Two failure modes bracket this, and the answer is between them:

- **Flat 99%** hands your margin to builders on every uncontested opportunity.
- **Fitting tightly to observed bids** hands auctions to competitors.

The balance point is not a matter of taste. In a first-price auction against an
unknown opponent, expected profit per opportunity is:

```
E[profit | β]  =  net · F(β) · (1 − β)
```

where `β` is your bid as a fraction of net and `F(β)` is the probability your bid
wins. **`F(β)` is your win rate** — so "more wins" is not a separate consideration
the optimisation ignores, it is the term being multiplied. And it is multiplied by
`(1 − β)`, which goes to zero as β approaches 1.

That is the argument against a flat high bid, and it is arithmetic rather than
opinion: at β = 0.99 each win earns 1% of net however many you take. For 0.99 to
be optimal, `F` has to be a near-step function exactly there — you would need to
win almost nothing at 0.98 and almost everything at 0.99.

Illustrative only, with a plausibly-clustered opponent distribution:

| β | F(β) | F(β)(1−β) |
|---|---|---|
| 0.90 | 0.10 | 0.0100 |
| 0.95 | 0.25 | **0.0125** |
| 0.97 | 0.40 | 0.0120 |
| 0.99 | 0.90 | 0.0090 |

Going 0.97 → 0.99 more than doubles the win rate and still loses money, because
the margin fell threefold. **Near the top, win-rate gains have to be enormous to
pay for the margin they cost.**

Those numbers are invented. Yours are not — see the estimator below.

### Estimating F from chain

Competitors' bids are visible: `inferred_bid = coinbase_transfer +
(effectiveGasPrice − baseFee) × gasUsed`, which GUIDE 05 Step 2 already extracts.
A bundle's whole trace is visible too, so you can reconstruct the winner's *net* —
their swap output, flash fee, gas — and therefore bid-as-a-fraction, not just
bid-in-wei.

**The limitation that shapes everything: you only ever see auctions someone
entered.** For an auction that cleared, the winning bid **is** the max rival
bid — it is the clearing level of that auction, not a sample from above it.
The missing mass is the auctions nobody entered. That atom `(1 − q)` is not on
chain, and leaving it out pushes `F̂` down. Do not treat the observed winner
as biased high relative to a clearing level you did not see.

Four things the estimator must get right:

**Bracket on ETH-normalised size.** A $1k liquidation and a $1M one clear at
completely different fractions. Token quantities are meaningless across assets —
0.1 USDC and 0.1 WBTC are the same number and nothing like the same decision.

**Condition on contest class, not just size.** One of your nine trigger classes is
auctioned; eight are races where the bid does not decide the winner. Applying a
bracket average to an uncontested `InterestDrift` donates the bracket's
competitive premium for an opportunity nobody else wanted — the waste GUIDE 13's
routing table already warns about.

**The better contest signal is what went untaken.** The bids you observe come from
liquidations that happened. The fraction of opportunities in a bracket that went
**unclaimed** says directly that nobody is competing there, and it is cleaner than
an average over the ones that were.

**Window by sample count, not blocks.** Liquidations are lumpy; "last 50 blocks"
is frequently zero observations in a given bracket. "Last N observations in this
bracket" gives you an estimator.

**Exclude your own wins.** Otherwise your bid enters the estimate, which feeds
your next bid, which enters the estimate. That ratchet only goes one way and you
will be bidding against yourself within weeks. Your executor address is known to
you — filter it.

### Randomise the decision, not the measurement

A deterministic, inferable bid rule lets a competitor attribute your wins to your
address, fit the rule, and sit one wei above it. A randomised margin is a mixed
strategy: to beat your maximum reliably they must bid your maximum, which costs
them the spread on every auction.

Randomise the **margin**, not the lookback window. Adding noise to the window
degrades your own estimate without making your output less predictable
conditional on state they can also observe.

```rust
let f      = brackets.fit(size_eth, contest_class);   // excludes own wins
let target = f.argmax_expected_profit();              // argmax F(β)(1−β)
let jitter = rng.gen_range(cfg.jitter_lo..cfg.jitter_hi);
let beta   = (target + jitter).min(cfg.beta_cap);     // cap < 1, always
```

### Bid high during the learning phase, deliberately

Three reasons the steady-state optimisation does not capture:

You cannot estimate `F` from losses you never observe. Winning generates the
realized-versus-predicted data the whole profit model depends on, and GUIDE 17's
supervised gate is explicitly waiting on it. Early wins are worth more than they
earn.

Included bundles build relay reputation on the identity key, which compounds.

And a model fitted on someone else's historical behaviour is worth less than one
fitted on your own outcomes.

So: start from the committed schedule, measure `F` from your own wins plus
the archive, and optimise downward once the curve is visible. That is
sequencing, not a concession.

The live bid is that schedule, not one cap and not `F(β)`. `config/bid.toml`
holds four exact rates. Size is the sized repay in ETH (`per_eth(debt)`).
Under 3 ETH is `below`. 3 ETH and above is `above`.

| | under 3 ETH | 3 ETH and above |
| --- | --- | --- |
| Aave v3 and Aave v4 | 99.5% of net | 99.8% of net |
| every other protocol | 65% of net | 67% of net |

A missing `per_eth` skips the leg. The executor carries one `bidBps` per
plan, so legs that resolve to different rates are packed separately.

### One objective, stated once

If the goal is **maximum wins** rather than maximum profit, that is a coherent
choice — set `beta_cap` high and stop optimising. But write down which one you
picked. A future agent reading "maximise wins" and "`E[profit per opportunity]` is
the headline metric" in the same guide will reconcile them by guessing.

This set optimises **expected profit**, with win rate reported alongside and
labelled as secondary.

## Acceptance criteria

- [ ] Joint repay × seize × source optimization implemented; a fixture proves a
      position is won on its second-choice debt leg when the first is unfundable
- [ ] Warm cache serves ≥ 95% of queries and stores pool *sets*, not single routes
- [ ] Water-fill allocation equalizes marginal `(fee + impact)` across used pools;
      a test proves it beats proportional and equal splitting on a fixed fixture
- [ ] Allocation solve uses tick-walking on V3/V4 and closed form or Newton on
      smooth invariants; **Brent (bisection fallback, not golden section) only
      where structure is unavailable**
- [ ] A fixture places the root exactly at a tick boundary and proves the solver
      converges — this is where an unsafeguarded Newton implementation fails
- [ ] Allocations sum exactly to the input; the rounding residual is assigned
      deterministically to the best-marginal-rate pool, and λ is not solved to
      wei precision
- [ ] Allocation solve completes within the routing budget at 6 pools (benchmark,
      not assumption)
- [ ] Pool-set selection stops when marginal hop gas exceeds impact saved; a
      fixture proves a small exit takes one pool and a large exit takes several
- [ ] Split candidates are **evaluated** with exact math and only the top 2–3 are
      **simulated** — no test path simulates the search space
- [ ] No HTTP call anywhere on the hot path (assert in test)
- [ ] Partial-size fallback: when flash depth or route depth binds below
      `max_repay`, the smaller liquidation is taken, not skipped
- [ ] Profit model reproduces realized profit within 2% on 100 historical
      liquidations replayed through GUIDE 11, **flash fee included**.
      This 2% is a replay tolerance. It is not the `(1 − β)` retained
      fraction (1% of net at β = 0.99). `historical_profit_parity` returns
      `ArchiveUnavailable` until that archive exists; it must not be
      treated as a pass.
- [ ] Viability band computed per `(protocol, collateral, debt)` per block from
      the exactly-known next base fee, emitted by the warm tier — **no cached
      `min_viable_notional`, and no gas regimes unless per-block proved too slow**
- [ ] Band uses `min(spot, twa)` for liquidity and price; a test proves a
      simulated liquidity collapse shrinks `max_size` on the next block rather
      than being averaged away
- [ ] Band gas cost is `(base fee + priority fee) × gas`; the two fees stay
      separate fields, and the bid is modelled as a distribution of net, not a cost
- [ ] **Eligibility gate is separate from ranking in the code, not just in the
      comments** — a test feeds a high-net-per-gas position that is NOT
      liquidatable and asserts it never enters the candidate set
- [ ] Expected contribution-per-gas folds in `p` (numerator is pre-gas).
      With `gas_failed > 0`, batch size falls when `p` falls. At
      `gas_failed = 0` the sign of Δnet does not change with `p`
- [ ] **No batching threshold anywhere.** A single position is one group with
      `liqCount == 1`, taking the same code path as fifty
- [ ] Gas limit read from the block header, never a constant (grep for it in CI)
- [ ] Two-stage selection under load: crude rank across all candidates, exact
      solve only for the top K that fit the budget
- [ ] `F(β)` fitted per ETH-normalised bracket **and contest class**, excluding
      own wins; a test proves a self-win does not move the estimate
- [ ] Bid is `argmax F(β)(1−β)` plus a randomised margin, capped below 1 —
      **not** a flat percentage
- [ ] Bid model back-fits historical auctions: the model's bid would have won the
      auctions classified as winnable
- [ ] `E[profit per opportunity]` is the headline metric; win rate secondary and
      labelled as such
- [ ] Timing optimizer **disabled by default**, gated on ≥ 200 `WaitedTooLong`
      observations, enabled by config flag
- [ ] Combination search completes within budget at 12 combinations, serial on
      the hot thread; no `std::thread::spawn` / `thread::scope` / `rayon` in
      `liq-router` (grep-assert in CI)

## Failure modes

| Symptom | Cause |
|---|---|
| High win rate, low profit | Bidding near the cap: `(1−β)` is near zero, so wins earn nothing |
| Bundle loses to a smaller competitor | Speculative candidates padding the batch — failed legs burn gas, which shrinks the bid |
| Bid drifts upward over weeks | Own wins not excluded from the `F` estimator; it is bidding against itself |
| Consistently outbid by small margins | `F(β)` fit stale or bracketed wrong, gas estimated rather than simulated, or paying 5 bps where 0 bps was available |
| Opportunities skipped entirely | No partial-size fallback when flash depth binds |
| Positions never targeted | Only the largest debt leg checked for fundability |
| Profitable small liquidations lose money | Band lower edge wrong — flash fee or gas not in the cost model |
| Large liquidations revert on slippage | Band upper edge stale, or a TWA averaging away a real liquidity collapse |
| `WaitedTooLong` spikes after enabling the optimizer | Competitor model fit on too few samples or a different volatility regime |

## Handoff

GUIDE 13 puts the plan and bid on the wire. The bid model is never "done" — it is
the component you revisit weekly for the life of the system.
