# GUIDE 14 — Risk, Treasury & Kill Switches

| | |
|---|---|
| **Crate** | `liq-risk` |
| **Prerequisites** | GUIDE 02 (drift detector), 07, 09 (build); 13 consumes `RiskGate.allow()`, it is not a prerequisite |
| **Work packages** | **14A** risk gate + matrix + proxy watcher + caps + treasury + ledger (built **before** 13A, which depends on it); **14B** accounting books scanner (after H3 gives the Executor address) |
| **Est. effort** | 1 week |
| **Blocks** | 15, 17 |

## Objective

Bound the blast radius of being right about the state and wrong about the world,
and stop automatically when the system's own correctness signals degrade.

**Flashloan-only operation removes an entire risk category.** There is no
inventory to size, no capital allocator, no cross-asset rebalancing, and no
warehoused collateral to mark to market. What you hold is gas and profit awaiting
sweep. That is a genuinely smaller attack surface — and it means this guide is
shorter than it would otherwise be, not that it is less important.

What replaces inventory risk is **flash liquidity risk**: your ability to act
depends on third-party pools you do not control.

## Build vs. buy

100% build. All signals come from components you already have.

---

## Step 1 — Halt matrix

Each row needs a metric, a threshold and an automatic action, wired to a
`RiskGate` the submitter consults before every submission.

**A halt is not a safety device here, and treating it as one is how it becomes a
liability.** Funds are protected one level down, on-chain and per transaction:
`minProfit` is checked inside `Executor.execute` against realized amounts, a
bundle that fails it reverts at no cost, and nothing is ever broadcast publicly
(D19, GUIDE 13 §1). A losing trade is not reachable, so **no row below prevents
one.**

What a halt does instead is stop us acting on state we have reason to distrust —
and every halt is code, code has bugs, and a mis-thresholded halt is a silent
machine for losing opportunities we would otherwise have captured. That cost is
real and it is invisible: nothing alarms when you are not trading.

**So a halt has to earn its place against three tests (D13c):**

1. **Is it scoped as narrowly as the fault?** Halt the protocol whose proxy moved,
   not the system. Blast radius is the first question, not the last
2. **Does it clear itself when the condition resolves?** If the world caused it,
   the world can end it. Only a fault that needs *code changed* may require a
   human
3. **Can it fire on a market condition?** If yes, it is a liability. A rolling
   metric crossing a threshold during a volatile hour is exactly when the
   opportunity is largest

Rows that fail test 3 are **alerts, not halts.** They still get logged loudly;
they just never stop a submission.

**D13 (manual-only abandonment) is a separate decision** and does not govern this
matrix. Abandoning the project is manual; these scoped, self-clearing halts stay
automatic.

### The matrix

**Class A — scoped, auto-clearing.** The world caused it; the world ends it.

| Condition | Detection | Action | Clears |
|---|---|---|---|
| Node lag | `head_ts` vs wall clock > 2 blocks | Halt all submissions | **Automatically, when the node catches up** |
| Oracle staleness | `now − last_update > heartbeat × 1.5` | Mark asset untradeable | On next fresh update |
| MEV-Share disconnected | SSE heartbeat missing > 60s | Halt SVR-triggered submissions only; others continue | On reconnect |
| **Flash source liquidity collapse** | `FlashIndex` availability drops > X% for a key asset | Mark asset ineligible; **never halt globally** | When depth returns |
| **Flash callback revert streak** | Counter per provider | Disable that provider, fall back down the chain | On provider recovery |
| Reorg deeper than undo ring | Unwinder cannot find ancestor | Halt, rebuild from snapshot | When the rebuild completes |
| **Gas wallet below floor** | Balance check per operator key | Halt that key only | On top-up |
| Reserve paused/frozen | Flag in market table | Exclude that market; no halt | On unpause |

Node lag is the one global halt, and it is global because stale state is global —
but it is Class A, not Class B: it recovers by itself the moment the node is
current again. It must never need a human.

**Class B — scoped, manual, because clearing it requires a code change.** These
are the only rows that wait for a person, and they wait because a person has to
*change something*, not because a person has to approve something.

| Condition | Detection | Action | Why manual |
|---|---|---|---|
| **Proxy implementation changed** | EIP-1967 slot watcher on every tracked proxy | **Halt that protocol alone**, loud | The new implementation has to be read before the adapter can be trusted |
| Drift mismatch > threshold | GUIDE 02 detector | Halt that protocol alone | Our health math disagrees with the chain — that is a bug with an address |
| Consecutive sim-pass / chain-revert | Counter per protocol | Halt that protocol alone, dump state | Our simulation and the chain disagree |

Every Class B row halts **one protocol**. None of them is global. A proxy upgrade
on one Aave V4 spoke does not stop Morpho liquidations, and a drift bug in one
adapter does not stop the other eleven.

**Class C — alert only, never a halt.** These measure real things worth knowing
and fail test 3: each is a rolling metric that a volatile hour can move.

| Condition | Detection | Action |
|---|---|---|
| Gas spend rate anomaly | Rolling gas spend vs. revenue | **Alert. No halt.** |
| PnL drawdown | Rolling realized PnL vs. limit | **Alert. No halt.** |

Neither can lose funds — with bundle-only submission, gas is spent on landed
bundles that cleared `minProfit`, and on nothing else. They are diagnostics about
whether the *business* is working, which is a question for a person over a week,
not for a gate over a block.

The proxy-upgrade trigger remains the most important row in the matrix. A proxy
implementation change can silently alter storage layout, health math or event
signatures — the failure that turns a profitable bot into an expensive one
overnight with nothing in the logs. Aave V4 Spokes are upgradeable, so this is
not theoretical, and with a dozen protocols tracked (GUIDE 15) the surface is
large.

### Step 1b — The action-required log

**Class B halts go to their own log, not to the general alert stream.**

The general stream carries everything: an alert is something that happened. The
action-required log carries only what is **waiting on a change from you** — a
searcher-side fix, an adapter correction, an executor upgrade. Nothing auto-clears
out of it; an entry leaves when the code ships.

The reason for the separation is triage under load. During a volatile hour the
alert stream is busy and mostly transient — staleness, lag, provider failover, all
resolving by themselves. A proxy upgrade landing in the middle of that is the one
thing that will still be broken tomorrow, and it is the one most likely to scroll
past. Two streams, and the short one is the work queue:

| Log | Contents | Clears |
|---|---|---|
| **Alerts** | Class A halts, Class C, miss alarm, ops events | By itself |
| **Action required** | Every Class B halt, with the protocol, the trigger value and the halted scope | **Only when you ship a change** |

An empty action-required log means nothing is waiting on code. That is the one
thing worth being able to check in ten seconds from a phone.

## Step 2 — Halt semantics

Halts must be **granular and composable**: per protocol, per asset, per flash
provider, per trigger cause, and global.

```rust
pub enum HaltScope {
    Global, Protocol(ProtocolId), Market(MarketId),
    Asset(AssetId), FlashProvider(FlashProvider), Trigger(TriggerKind),
}
```

Every halt logs its trigger metric value; every `allow()` carries the `TraceId`
so a missed opportunity traces back to the halt that caused it. You will need
this — some halts will prove too aggressive, and you cannot tune what you cannot
attribute.

**Auto-clearing is the default, and the bar for manual is specific.** Every
Class A halt clears itself when its condition resolves — node lag when the node
catches up, staleness on the next fresh update, a provider when it recovers. Only
Class B waits for a person, and the test is not "is this serious" but **"does
clearing it require code to change?"** Proxy upgrade, drift mismatch and
sim/chain divergence pass that test; nothing else in the matrix does.

Getting this backwards is the expensive direction. A halt that needed a human and
did not get one costs every opportunity until someone looks — which, on a system
designed to run with intermittent monitoring, can be overnight.

**Scope defaults to the narrowest that contains the fault.** `Global` is reserved
for node lag, where the state feeding every decision is stale. Everything else is
`Protocol`, `Market`, `Asset`, `FlashProvider` or `Trigger`. A halt written at
`Global` scope when `Protocol` would do is a bug, and it is the specific bug that
turns a safety mechanism into a source of losses.

## Step 3 — Flash liquidity risk

The risk that replaces inventory risk, and it has a different shape: you are
exposed to pools you neither own nor control.

- **Concentration.** Track what fraction of your liquidations depend on a single
  provider. If 80% route through Uniswap V4, a V4 incident stops 80% of your
  business. Ensure every major debt asset has at least two viable sources
  configured, and alert when one drops to a single source.
- **Depth monitoring.** Alert when availability for a top-volume debt asset falls
  below the level that funds your median liquidation. This is an early warning
  that your eligible universe is shrinking.
- **Haircut tuning.** The GUIDE 07 safety haircut is a risk parameter, not a
  constant. Drive it from the observed `InsufficientLiquidity` rate: rising
  failures mean other searchers are competing for the same source in the same
  block, and the haircut should widen.
- **Provider risk.** A flash provider can be paused, upgraded, or exploited.
  Treat each as a tracked proxy under the Step 1 watcher, and have the fallback
  chain (GUIDE 07) actually exercised in drills rather than assumed.

## Step 4 — Position limits

- Per-liquidation notional cap
- Per-protocol exposure cap
- Global concurrent-opportunity cap
- **Per-provider concurrent cap** — two simultaneous liquidations sourcing the
  same pool in the same block will contend, and the second reverts

The tail risk under flashloan-only is not "I am left holding a bad asset" —
atomic settlement removes that. It is "the oracle printed a wrong price, twenty
positions went liquidatable at once, I took all of them, and every one of them
paid gas and a flash fee to seize collateral that was mispriced." Caps make that
survivable.

The concurrent cap interacts with the nonce pool (GUIDE 13) — size them together
and make the cap binding so you never exhaust keys.

## Step 5 — Treasury

Radically simpler than an inventory model, and simpler still since profit became
ETH-denominated (GUIDE 12 §4d). You hold:

1. **Gas balance** per operator key — enough for the observed peak burst plus
   margin, topped up on a schedule, alerted on a floor.
2. **Unswept WETH** in the executor. One asset, never a pile.
3. **Nothing else.** No per-asset targets, no rebalancing, no bridge routes, and
   **no ETH float for bids** — every liquidation funds its own bid out of its own
   proceeds, which is what keeps D02 true end to end.

```rust
pub struct Treasury {
    gas_floor: HashMap<KeyId, U256>,
    sweep_threshold_wei: U256,    // a risk budget again since D13 — see below
    sweep_destination: Address,   // cold
}
```

### The sweep threshold is a risk budget again (D13)

It used to be one: profit accumulated as an assortment of debt tokens, so an
unswept balance was standing exposure to an executor bug across several assets,
and the threshold traded that against transfer gas.

With ETH-only profit and a bid paid in the same transaction, the residual is
whatever fraction of net you keep — small by construction at a high `bidBps` —
in one asset. So the question collapses to arithmetic: **is the balance worth the
gas of a transfer?** The residual is **WETH, an ERC-20**, so the transfer is
`WETH.transfer(PROFIT_SINK, bal)`: ~25–30k gas inside a plan that already runs
(`F_SWEEP`: two balance SSTOREs, the recipient's cold), ~50k as a standalone
`sweep()` transaction (21k base on top). The ~9k figure is a native-ETH
`CALL{value}` and does not apply here.

```
transfer_gas = 30_000 (in-plan, F_SWEEP) | 50_000 (standalone sweep())
sweep when  balance_wei > k × transfer_gas × base_fee    // k ≈ 20–50
```

That is derivable rather than judged, and it moves with gas like everything else.
Keep a maximum age alongside it so a quiet week does not leave a balance sitting
indefinitely.

**D12 changes meaning accordingly.** It is no longer "how much inventory do I
tolerate" but "what multiple of transfer cost justifies a transfer" — a far
easier number to set, and one an agent can compute rather than guess.

## Step 5b — Disposal: you are long ETH on purpose

Profit denominated in ETH means you carry ETH exposure between liquidation and
disposal. Worth being explicit that this is a choice, not a side effect.

**The case for it.** Gas is priced in ETH, so your cost base and your revenue are
the same asset — margin measured in ETH is stable regardless of what ETH does.
For a bot whose dominant cost is gas, that is closer to a hedge than a risk.

**The case against.** If you account in EUR, ETH-denominated profit is volatile in
the unit that actually matters to you. The answer is disposal cadence, not
changing the denomination.

**Disposal belongs on a CEX, in batches.** Taker fees around 10 bps beat a 30 bps
pool plus impact, and batching amortises the transfer gas. This is the one place
where deferring a conversion is genuinely cheaper rather than deferring cost into
a worse execution context — because unlike the in-liquidation swap, there is no
atomicity, no guard and no bundle to give up. You are moving realised profit, not
sizing a trade.

Set a cadence and a floor rather than doing it ad hoc:

- A batch is worth sending when accumulated ETH exceeds some multiple of the
  withdrawal fee plus transfer gas
- A maximum age caps how long you sit exposed
- Both are parameters, both belong in config, neither is a judgement call once set

The exchange account is a counterparty and a custody risk that nothing else in
this design has. Size the balance you leave there to what you would shrug off.

## Step 6 — PnL ledger

Off the hot path, in **SQLite** (D06; GUIDE 09 Step 4b). Per liquidation: gross bonus, repay,
slippage, **flash fee and provider**, gas, bid, net; plus the GUIDE 09 `Outcome`
classification. Roll up daily by protocol, trigger cause, flash provider and
notional bucket.

Recording the provider per liquidation is what lets you answer "what did source
selection actually save us this month?" — which is the ongoing justification for
GUIDE 07's complexity, and the input to the concentration analysis in Step 3.

This ledger feeds the `F(β)` fit (GUIDE 12 Step 6, WP 12B) and the drawdown alert (Class C). Reconcile
to on-chain balances daily.

---

## Step 7 — The books: a separate, immutable accounting log

**This is not Step 6.** Step 6 is operational analytics — mutable, aggregated,
re-derived whenever a model changes, and correct for that purpose. Step 7 is an
accounting record that must still be readable and unchanged in eight years. They
share no storage, no schema and no code path. Merging them means a schema change
made to tune the bid model rewrites a tax record.

### Built as an independent on-chain scanner

The accounting log is produced by a **separate process that watches the
Executor's own transactions on chain** — not by the searcher emitting rows as it
trades.

Two reasons, and the second is the important one:

1. **Zero hot-path cost.** Nothing is added to the latency budget (GUIDE 16 §5).
   It can run on a cron, batched, hours behind, and nothing suffers
2. **Independence.** The record of what the system earned is not produced by the
   system that earned it. Same principle as the miss classifier (GUIDE 05
   Step 3a): a bug in the searcher's own accounting would otherwise write itself
   into the books. A scanner reading receipts and logs from chain agrees with the
   searcher or it does not, and a disagreement is a finding

Reconcile the two monthly. They will differ occasionally — reverted-then-retried
submissions, a bundle landing in an unexpected block — and the scanner is right
by construction, because it is reading what actually happened.

### Write at finality, never at inclusion

Rows are appended once the block is **finalized** (two epochs, ~13 minutes), not
when the transaction is included. A finalized block cannot be reorged, so the
file needs no reversing entries and stays strictly append-only.

This matters beyond tidiness: GoBD requires **Unveränderbarkeit** of accounting
records. A file that is only ever appended to, in which every line is a settled
fact, satisfies that in the simplest possible way. A file containing entries that
were later negated is defensible in double-entry terms but is a much harder thing
to explain to an auditor, and a naive sum over it is wrong.

### The files

CSV, UTF-8, `\n` line endings, ISO 8601 UTC timestamps, one file per calendar
month, never edited after the month closes:

```
books/
  bundles-2026-09.csv
  liquidations-2026-09.csv
  bundles-2026-09.csv.sha256
  liquidations-2026-09.csv.sha256
```

Two files because CSV is flat and a bundle can contain ten liquidations. They
join on `tx_hash`, and the child rows sum to the parent.

**`bundles-YYYY-MM.csv`** — one row per landed, finalized bundle:

| Column | Notes |
|---|---|
| `finalized_at_utc` | From the block timestamp, ISO 8601 |
| `block_number`, `block_hash`, `tx_hash` | The audit trail back to chain |
| `liquidation_count` | Child rows in the other file |
| `gross_bonus_wei` | Realized, from the receipt — not the quote |
| `flash_fee_wei`, `swap_cost_wei` | |
| `gas_used`, `gas_price_wei`, `gas_cost_wei` | **An operating expense.** Capture it |
| `coinbase_bid_wei` | Paid to the builder. Also a cost of doing business |
| `net_retained_wei` | What the Executor kept. Components above sum to it |
| `eth_usd_price`, `eth_usd_round_id`, `eth_usd_updated_at` | |
| `eur_usd_price`, `eur_usd_round_id`, `eur_usd_updated_at` | |
| `eth_eur_rate` | Derived — see below |
| `net_retained_eur` | The number the books actually want |
| `ecb_rate_date`, `ecb_eur_usd` | Second source, see below |
| `schema_version` | So a later column addition is legible |
| `prev_row_hash`, `row_hash` | Hash chain, see below |

**`liquidations-YYYY-MM.csv`** — one row per liquidation inside a bundle:
`tx_hash`, `log_index`, `protocol`, `market_instance`, `borrower`, `debt_asset`,
`debt_repaid_raw`, `debt_decimals`, `collateral_asset`, `collateral_seized_raw`,
`collateral_decimals`, `gross_bonus_wei`.

Raw amounts **and** decimals, both. A raw integer without its decimals is
unreadable in five years when a token has been migrated or delisted.

### The EUR rate: there is no Chainlink ETH/EUR feed on Ethereum mainnet

Only ETH/USD and EUR/USD exist there, so the rate is derived:

```
eth_eur_rate = eth_usd_price / eur_usd_price
```

- ETH/USD — `0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419`
- EUR/USD — `0xb49f677943BC038e9857d61E7d053CaA2C1734C1`

Both read with `latestRoundData()` at the **same block** as the liquidation, and
**both feeds' `answer`, `roundId` and `updatedAt` are recorded, not just the
derived rate.** Two reasons. The derivation is then reproducible by anyone from
public data, which is the whole point of an audit trail. And `updatedAt` makes
staleness visible: Chainlink posts on a heartbeat and a deviation threshold, so
"the price at block N" is the last posted price and can be hours old. That is
fine — it is deterministic and verifiable — but it should be *visible* rather
than silently folded into one number.

**Also record the ECB daily reference rate** for the same date (`ecb_rate_date`,
`ecb_eur_usd`). It is free, it is the conventional source for EUR conversion in
German books, and capturing both costs one HTTP call a day. Which one the books
use is a question for a Steuerberater — but only one of these choices can be made
retroactively, and it is the one where you kept both.

### Immutability: hash-chain the rows

Each row carries `row_hash = sha256(prev_row_hash || <the row's fields>)`, and
each closed month gets a `.sha256` sidecar over the whole file.

This is cheap and it is the standard technical answer to **Unveränderbarkeit**: a
row altered after the fact breaks every hash after it, so the file demonstrates
its own integrity rather than asserting it. Anchor the month-end file hash
somewhere dated if you want more — a timestamped note is enough; it does not need
to go on chain.

### Retention, and what to back up

Since **BEG IV** (in force 1 Jan 2025), **Buchungsbelege** carry an **8-year**
retention period rather than ten — but **Verfahrensdokumentation under GoBD
remains 10 years**. So the CSVs are the 8-year artifact, and the document
describing how they are produced is the 10-year one. See the note below.

These files join the set in GUIDE 16 §7 that is **genuinely unrecoverable**: the
node can be re-synced, the state store rebuilt, the archive re-extracted. A month
of books cannot be reconstructed if the chain data that produced it is gone and
nobody kept the file. They are a few hundred kilobytes a year. Back them up
off-box, versioned, from day one.

### The Verfahrensdokumentation you now partly have

GoBD requires a **Verfahrensdokumentation** — a written description of how
records come into existence, are processed and are stored — and for an automated
system it is not optional. The March 2025 BMF Schreiben on crypto is pointed
about this: software-generated tax reports depend on the quality of the
underlying data, and are no longer accepted uncritically.

This section, plus `REGISTRY.md` and the GUIDE 13 submission path, is the
technical core of that document. Keep it current as the system changes, and keep
the version that was in force for each year, because that is the version the year
is audited against.

**None of the above is tax advice.** The engineering decisions — independent
scanner, finality, append-only, hash chain, both rate sources, capture components
not just net — are all in the direction of *keeping more, verifiably*, which is
the direction that leaves the tax questions open for someone qualified to answer
them. A net-only log closes them by default and badly.

---

## Acceptance criteria

- [ ] Every row in the Step 1 matrix has a metric, threshold and tested action,
      **and a class** — A (scoped, auto-clearing), B (scoped, manual, needs code)
      or C (alert only)
- [ ] **No Class B halt is global.** Each names the single protocol it stops
- [ ] **No halt can fire on a market condition** — anything measuring a rolling
      metric against a threshold is Class C
- [ ] Action-required log exists, separate from alerts, carrying Class B only
- [ ] The manual stop works
- [ ] **Accounting log (Step 7) is a separate process** reading chain, sharing no
      storage or schema with the Step 6 ledger
- [ ] Rows written only at **finality**; the file is append-only and has never
      been edited in place
- [ ] Both Chainlink feeds recorded with `roundId` and `updatedAt`, plus the ECB
      daily rate — not only the derived EUR figure
- [ ] Hash chain verifies end to end; each closed month has its `.sha256`
- [ ] Monthly reconciliation of the scanner's totals against the Step 6 ledger
      and against the Executor's on-chain balance history
- [ ] Books are backed up off-box (GUIDE 16 §7), versioned, from the first
      landed bundle
- [ ] Proxy watcher covers every tracked protocol **and every flash provider**,
      verified by a test enumerating config addresses
- [ ] Halts are granular; a flash-provider halt does not stop liquidations that
      can route elsewhere
- [ ] Fallback chain exercised in a drill: disable the primary provider, confirm
      liquidations continue via the secondary
- [ ] Every major debt asset has ≥ 2 configured sources; alert fires when one
      drops to a single source
- [ ] Haircut auto-tunes from the observed `InsufficientLiquidity` rate
- [ ] Per-provider concurrent cap prevents same-block same-pool contention
      (proven with a synthetic cascade)
- [ ] Gas floor alert fires before any key is exhausted
- [ ] Sweep runs on threshold and on max-age; unswept balance stays bounded
- [ ] PnL ledger records flash provider and fee per liquidation; reconciles to
      on-chain within 1% daily
- [ ] Drawdown switch halts all submission and requires explicit re-arm

## Failure modes

| Symptom | Cause |
|---|---|
| Profitable bot quietly stops winning | Proxy upgraded; no watcher |
| All liquidations fail at once | Single-provider concentration; no second source for that asset |
| Rising revert rate during volatility | Haircut static while competition for the same pools increased |
| Two of your own bundles contend | No per-provider concurrent cap |
| Missed opportunities, cause unknown | Halts not attributable via `TraceId` |
| Large loss from an executor bug | Profit swept infrequently; standing balance too large |
| Cannot justify source-selection work | Flash provider not recorded per liquidation in the ledger |

## Handoff

GUIDE 15 expands protocol coverage. With risk controls in place you can afford to
be wrong about a new adapter — which is precisely why coverage comes after this
and not before.
