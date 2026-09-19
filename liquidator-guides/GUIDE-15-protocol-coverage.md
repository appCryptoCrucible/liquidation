# GUIDE 15 — Protocol Coverage Program

| | |
|---|---|
| **Crate** | `liq-adapters/*` |
| **Prerequisites** | GUIDE 04 pattern for *building* adapters; GUIDE 13, 14 live only for *enabling* them (D49) |
| **Work packages** | **15A-1** Morpho Blue adapter + its drift week (starts as soon as 04A exists, concurrent with Aave's drift); **15A-2** Spark config-only; **15B** mechanism reviews + flashloanability + `DECLINED.md`; **15C-\<proto\>** one per Tier 2/3 adapter, opening a `10R-n` redeploy WP when the liquidation ABI is new (D48); **15D** enable in production — H5, needs 13B live |
| **Est. effort** | ongoing — 2–4 days per adapter once the first three are done |
| **Blocks** | 17 |

## Objective

Cover the Ethereum mainnet lending universe. This is not a single task but a
**program** with a repeatable per-adapter process, a prioritization rule, and an
explicit list of protocols to decline.

The first three adapters are a test of GUIDE 01. Adapters four onward are a test
of your process.

**The rule: if adding a protocol requires changing `liq-engine`, stop and fix the
trait.** The GUIDE 00 lint catches the crude version; the subtle version is an
enum variant added to a shared type "just for this one".

---

## Step 1 — The universe, prioritized

Approximate mainnet TVL as of mid-2026. Prioritize by **liquidatable debt** and
**debt-asset flashloanability** (GUIDE 07), not by headline TVL — a protocol whose
debt is all long-tail assets is worth less to you than its size suggests.

| Tier | Protocol | ~TVL | Family | Notes |
|---|---|---|---|---|
| 1 | Aave V4 | new, growing | hub-spoke | GUIDE 04. Variable bonus, dynamic close factor |
| 1 | Aave V3 | $14.6B | pooled | GUIDE 04 |
| 1 | Morpho Blue | $11.8B | isolated market | Adapter #2. Also a **zero-fee flash source** |
| 1 | Spark | $3.2B | Aave V3 fork | Near-free once V3 exists — mostly config |
| 2 | Sky Lending | $5.6B | CDP / vault | Different family; auction-based |
| — | **Compound V3 (Comet)** | $1.8B | **absorb + buy** | **DEFERRED — see Step 1b** |
| 2 | Fluid | $1B | lending+DEX hybrid | Novel collateral mechanics; read carefully |
| 2 | Euler V2 | $880M | vault-pair | Controller + enabled collateral vaults |
| 3 | Silo | $410M | isolated silo | Isolated-market family, similar to Morpho |
| 3 | Compound V2 forks | varies | shortfall | One adapter covers many forks via config |
| 3 | Liquity V2 / trove forks | varies | trove | Sorted-list / batch liquidation |
| 3 | Gearbox | varies | credit account | Leverage accounts, different shape |
| 3 | Ajna | varies | bucket auction | Permissionless pools; long-tail debt |
| — | **Curve lending (LLAMMA)** | — | **soft liquidation** | **DECLINE — see Step 5** |

**Fork multiplier.** Spark is an Aave V3 fork; many mid-tier protocols are
Compound V2 forks. One well-built adapter plus a TOML file covers each fork.
This is where "everything is data" (GUIDE 00) pays off most — target
*families*, not protocols.

## Step 1b — Compound V3 is deferred, not declined

Comet is excluded from phase 1. It is a good protocol and the exclusion is about
sequencing, not quality — so it sits apart from `DECLINED.md`, which is for
protocols whose mechanism makes them permanently unattractive.

**Why it is expensive here.** Comet's liquidation is not a variant of
`liquidationCall`; it is a different mechanism with a different profit path.
`absorb()` is permissionless, takes the **entire account** with no choice of
collateral or amount, and pays the caller essentially nothing — it moves the debt
and collateral onto the protocol's own balance sheet. The money is in
`buyCollateral()`, purchasing that collateral back at a discount derived from
`StoreFrontPriceFactor × (1e18 − LiquidationFactor)`.

So the flashloan cycle inverts: *borrow base → buy collateral at a discount →
swap back to base → repay flash*. No repay-and-seize step at all. That is a
second opportunity model — `HealthState::AuctionOpen`, two-leg quoting, a race
where one searcher absorbs and a different one profits — and it earns its cost
only once the single-mechanism path is proven.

**What the exclusion costs.** ~$1.8B TVL of coverage, and the abstraction
pressure it would have applied to `TriggerCause` and `Quote`. Adapter #2 (Morpho
Blue) still tests generalization along the position-scope axis, so the trait is
not going untested — but it is being tested along one axis instead of two, and
a mechanism-shaped flaw in GUIDE 01 will surface later and more expensively than
it would have.

Record that as a known, accepted risk rather than an oversight.

**What it does not change.** Aave accounts are cross-collateralised too, so
multi-collateral positions remain in scope, and the joint routing they require
(GUIDE 12 Step 4c) is built regardless.

## Step 2 — Sequencing, and why

1. **Morpho Blue** — easiest possible second adapter, chosen deliberately. Tests
   generalization along the axis that matters (position scope: many markets × few
   users each, the inverse of Aave's shape) while holding everything else simple.
   Bonus: it is also a zero-fee flash source, so the GUIDE 07 work overlaps.
2. **Spark** — the fork test. Should be a config file and a day. If it isn't,
   the V3 adapter hard-coded something.
3. **Everything else**, by the Step 1 tiering.

The original ordering put Compound V3 third precisely because it was the cheapest
moment to discover an abstraction flaw. Deferring it (Step 1b) gives that up
knowingly: the trait now gets one generalization test instead of two, so treat
any later mechanism-shaped change to `liq-engine` as the delayed bill for this
decision rather than as a surprise.

## Step 3 — The per-adapter checklist

Every adapter, no exceptions. The temptation to skip the drift week on adapter #7
because #1–6 were fine is how a silently-wrong protocol enters production.

- [ ] **Read the contracts, not the docs.** Record every rounding direction.
- [ ] **Mechanism review before coding** — confirm it is a hard liquidation, not
      soft/continuous (Step 5)
- [ ] **Flashloanability survey** — what assets is debt denominated in, and are
      they flashloanable at depth? A protocol whose debt is exotic is low
      priority regardless of TVL
- [ ] **Enumeration recipe** implemented from `REGISTRY.md` §3 (Family A/B + per-protocol recipes for Fluid, Euler V2, Silo, Liquity V2, Gearbox, Ajna). Discovery is from chain/events, never a hand list
- [ ] Event coverage audit document (GUIDE 03)
- [ ] `apply_log` with undo records for every path
- [ ] Conformance suite passes (GUIDE 01)
- [ ] Named fixtures per guard condition (GUIDE 05)
- [ ] **Drift detector < 1 bp for 7 days** (GUIDE 04)
- [ ] Recall coverage row filled for this protocol instance, and no unclassified
      miss on it (GUIDE 05 Step 3/3a)
- [ ] Contract adapter + combinatorial fork tests across all callback shapes
      (GUIDE 10)
- [ ] Shadow gate cleared for this adapter — ≥ 2 weeks and ≥ 200 contested
      opportunities (GUIDE 09)
- [ ] Only then: enable execution

## Step 4 — What to expect per adapter

| Adapter | Hand-written | With agents |
|---|---|---|
| #1 Aave V4 | 2–3 weeks | 2–3 days |
| #2 Morpho Blue | ~1 week | ~1 day |
| #3 Spark (fork) | ~1 day | hours |
| #4–#12 | 2–4 days each | hours each |
| *(deferred)* Compound V3 | ~1.5 weeks (new mechanism) | ~2 days |

**When code is cheap, build wide early.** Breadth is the edge — the small wins
across the long tail that larger operations skip — and the marginal cost of
adapter #8 is now hours. What does *not* compress is the per-adapter checklist in
Step 3: the drift week and the recall replay are calendar, not effort. Run them
concurrently across adapters rather than serially, and let the drift detector
gate which ones go live.

If adapter #5 still takes a week, the abstraction is not paying. Stop adding and
do a deliberate refactor pass — the cost of that decision compounds across every
remaining protocol.

Track actual effort per adapter. It is the single best health metric for the
architecture.

## Step 5 — Protocols to decline, on purpose

**Curve lending / LLAMMA.** Positions are *continuously rebalanced* across price
bands rather than liquidated at a threshold. There is no "HF < 1, seize
collateral, take a bonus" event to win. Model it as
`HealthState::SoftLiquidating` and decline it explicitly.

Mis-modelling this is the single most expensive mistake available in this
program — it is months of adapter work producing an opportunity stream that does
not exist. **Read the liquidation mechanism before writing any code, for every
protocol.** Add the finding to a checked-in `DECLINED.md` with the reason, so
nobody revisits it in six months.

Other decline criteria:

- Debt denominated in assets with no flashloan source at depth (GUIDE 07) — you
  cannot fund the repay, so the position is untargetable by construction
- Liquidation bonus too thin for the viability band to open at all (GUIDE 12 §4b) at
  realistic sizes
- Protocols whose position discovery requires full state scans with no event
  trail **and** no on-chain enumerable structure — technically possible, rarely
  worth it. (Liquity V2's `SortedTroves` walk and Gearbox's `creditAccounts()` are
  enumerable — see `REGISTRY.md` §3 — so they do not fail this criterion.)

## Step 6 — Scale effects

Twelve protocols is not two protocols twelve times. Watch for:

- **Position count** growing 10–50×. The GUIDE 02 store was designed columnar for
  this; verify the assumption holds with real cardinality.
- **Threshold index memory** under many-markets shapes (Morpho, Silo, Ajna).
  Measure against the Aave-only baseline.
- **Recompute budget contention.** One adapter over-reporting `DirtySet` now
  starves eleven others. Enforce minimality in the conformance suite.
- **Log volume** at ingest. The union filter grows; dispatch must stay a hash
  lookup plus jump table.
- **Config surface.** Dozens of markets × assets × feeds. Startup validation
  (GUIDE 00) is what keeps this from silently rotting.

Baremetal (GUIDE 16) gives you headroom for all of this. Use it deliberately —
headroom is not a substitute for the columnar layout, it is what lets you run
the full universe on top of a layout that was already efficient.

---

## Acceptance criteria

- [ ] **`liq-engine` unchanged** across every new adapter (git diff proves it)
- [ ] No protocol-specific variant added to a shared type that is not genuinely a
      new *category* of behaviour
- [ ] Tier 1 fully covered: Aave V4, Aave V3, Morpho Blue, Spark
- [ ] Compound V3 deferral recorded in `STATE.md` with its reason, and the
      abstraction-coverage risk it creates acknowledged (Step 1b)
- [ ] Multi-collateral positions handled: a fixture liquidates two collaterals
      from one borrower in a single plan and the joint route solve is used
- [ ] Spark adapter is predominantly configuration
- [ ] Every enabled adapter passed the full Step 3 checklist
- [ ] `DECLINED.md` exists **in the repo** (it is a build artifact this guide
      creates, not a spec file), with Curve/LLAMMA and a reason
- [ ] Flashloanability survey completed per protocol and used in prioritization
- [ ] Adapter effort tracked; #5 onward measurably below #1
- [ ] Threshold index memory under many-markets shapes measured against baseline
- [ ] Full-universe replay adds no `InScope` miss and meets latency budgets

## Failure modes

| Symptom | Cause |
|---|---|
| Engine needs a change | Trait leaked — fix the trait, never special-case |
| Months lost on an adapter with no opportunities | Mechanism not read before coding (LLAMMA class) |
| Adapter #6 still takes a week | Abstraction not paying; refactor before continuing |
| New adapter passes tests, drifts live | Event coverage audit skipped |
| Latency regresses as coverage grows | One adapter over-reporting `DirtySet` |
| High-TVL protocol yields nothing | Debt assets not flashloanable; survey skipped |
| Memory blows up | Store slot allocation assumes Aave's dense-multi-asset shape |

## Handoff

GUIDE 17 is operations. Coverage is never "done" — new protocols launch, and the
per-adapter process is what makes adding them routine rather than a project.
