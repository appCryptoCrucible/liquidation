# Pre-Flight — What To Settle Before GUIDE 00

Eighteen guides describe *how* to build this. None of them ask *whether the
market supports it*, and that gap is the most expensive one in the set, because
it is answerable in two weeks and currently scheduled to be answered in month
six by the PnL.

This document is the work that should happen before the first line of Rust.

---

## 1. The finding that makes this urgent

Chainlink SVR — the mechanism that auctions the right to backrun Aave's oracle
updates — has recaptured **$18.3M cumulatively, $8.3M in Q1 2026 alone**, and
holds **99% of the market for oracle-related MEV capture**. Over $16.7M of that
came from Aave on Ethereum.

Two things follow, and both cut against the plan as written:

**The recapture is accelerating.** $8.3M in one quarter against $18.3M all-time
means roughly half the lifetime total landed in the most recent quarter. That is
a mechanism scaling up, not a pilot.

**It is spreading beyond Aave.** Compound, Vyro and Steakhouse Finance have
adopted SVR, and it has extended to Arbitrum, Base, BNB Chain and HyperEVM. The
plan targets *all* Ethereum lending protocols — and the largest of them are
progressively adopting the thing that takes the oracle-driven share of the
opportunity and gives it back to the protocol.

Separately, MEV markets concentrate hard. Published work on CEX-DEX arbitrage
finds **eleven searchers taking >80% of volume, with the top two at 49.2%**, and
attributes the persistence to advantages in "opportunity access, latency,
information, capital, and execution quality." Liquidations are a different
niche, but the structural argument transfers: a new entrant faces incumbents who
are ahead on all five axes.

None of this makes the project wrong. It makes the *sizing question* urgent, and
it changes which part of the opportunity is worth the most effort — see §3.

---

## 2. The market study — 1–2 weeks, before GUIDE 00

You do not need the replay harness for this. Dune, or a script over an archive
node, answers all of it. Every question below has a number, and the numbers
together decide whether six to eight months is justified.

**Q1 — How big is the pool?**
Total liquidation bonus paid across every mainnet lending protocol, monthly, for
the last 18 months. This is gross searcher revenue before gas, flash fees and
bids. Split by protocol.

**Q2 — How much of it is already auctioned away?**
What fraction of those liquidations were SVR-backrun? Cross-reference SVR
recapture against total bonus paid. This is the share where you are bidding
against the protocol's own recapture mechanism rather than keeping the spread.

**Q3 — How concentrated is the winner set?**
Count distinct liquidator addresses per protocol and compute the share taken by
the top 1, 3, 5, 10. If three addresses take 80%, you are entering a market with
entrenched incumbents; if it is a long tail of fifty, there is room.

**Q4 — What is the trend in searcher-retained value?**
Gross bonus minus inferred winning bid, by month. Rising, flat, or falling? A
falling line with SVR adoption accelerating is the signal to reconsider scope.

**Q5 — What is the size distribution?**
Histogram of individual liquidation sizes. What fraction clear the
viability band (GUIDE 12 §4b) once you price in gas, flash fees and impact? A pool
that is large in aggregate but composed of $3k liquidations is not addressable.

**Q6 — How much of it is *not auctioned*?**
This is the number §3 turns on, and the cut is **auctioned vs. not**, not price
vs. interest. Classify every liquidation in the window by trigger using the §3
table, and sum the bonus in the rows that are not subject to protocol
recapture. That total — not the interest-drift row alone — is the addressable
pool for a new entrant.

---

## 3. The reframe — and the axis that actually matters

**Target every trigger.** Price moves cause most liquidations, and the detection
engine is trigger-agnostic by design (GUIDE 08's `TriggerCause`), so tracking all
of them costs almost nothing marginal — only the execution branch differs.

The useful distinction is not price vs. interest. It is **auctioned vs. not**.

| Trigger | Price-driven | Auctioned | How you win |
|---|---|---|---|
| **SVR oracle update** — Aave, spreading | yes | **yes** | Outbid. Margin compressed by design. |
| Non-SVR oracle update — other protocols | yes | no | Backrun bundle; latency race vs. searchers |
| Pull oracle (Pyth / Redstone) | yes | no | **You** submit the update; race to land |
| **Derived-rate move** — wstETH/stETH, sDAI `chi`, LRT rates | yes | no | Model the rate contract. Most bots watch Chainlink only. |
| DEX TWAP / LP-priced collateral | yes | no | Simulate pending swaps |
| User action — borrow or withdraw into insolvency | no | no | Backrun their transaction |
| **Governance parameter change** — LTV/threshold cut | no | no | **Scheduled in advance by timelock.** Fully deterministic. |
| Interest accrual — incl. Aave V4 risk premium | no | no | Closed-form crossing time |
| Stale liquidatable — nobody took it | either | no | Just be watching |

**How much is actually auctioned.** Aave's SVR rollout went in phases: ~3% of
TVL, then ~27%, and phase 3 covers **~75% of Aave's Ethereum TVS — about 95% of
its OEV-relevant value**. So for Aave oracle-triggered liquidations,
near-everything is auctioned. That is one row of nine.

**Where a new entrant's edge plausibly lives** — the rows that are price-driven
*and* uncontested, which I under-weighted before:

- **Derived-rate triggers.** A wstETH position goes underwater when
  `stEthPerToken()` updates or when stETH's own feed moves. A bot subscribed only
  to Chainlink aggregators sees the second and misses the first. GUIDE 06 §7
  already requires you to reimplement these formulas for *correctness*; doing so
  also makes them a trigger source competitors do not watch.
- **Governance parameter changes.** Aave votes are public and execute behind a
  timelock. You know weeks ahead that asset X's liquidation threshold drops on
  block N, and you can precompute the exact set of positions that become
  liquidatable at that block. Scheduled, deterministic, and nobody is bidding
  against you for information everyone could have had.
- **Stale liquidatable positions.** Illiquid collateral, sizes below other
  searchers' `min_viable_notional`, long-tail protocols nobody watches, or a
  competitor's bot being down. No race at all — it just requires full-universe
  coverage, which is what GUIDE 15 buys you.
- **Interest accrual**, including Aave V4's per-user risk premium. Positions
  accrue at different rates and crossing time depends on state most competitors
  do not model.

The common thread is not "slow." It is that **all four are won by breadth and
correctness rather than by latency or capital** — precisely the axes where the
guides 02–05 correctness spine and guide 15's coverage program pay off, and
precisely where an incumbent's latency advantage is worth nothing.

So: build for every trigger. Let Q6 tell you how to *weight* the effort between
GUIDE 12's bid model and GUIDE 08's breadth, rather than which triggers to
support.

---

## 4. The abandonment decision is manual (D13)

**There is no mechanical kill rule, and that is a decision, not an omission.**

The usual argument for writing a threshold in advance is that you will not write
one once you are invested. That argument assumes the decision is time-critical —
that by the time you notice, something irreversible has happened. Here it is not.
Capital is protected **per transaction, on-chain**: `minProfit` is checked inside
`Executor.execute` against realized amounts, and a bundle that fails it reverts
for free (GUIDE 10, GUIDE 13). A losing trade is not a thing this system can do.
So the only thing at stake in abandoning late rather than early is **time**, and
time spent is already spent when you notice.

What a lost opportunity costs is the opportunity. That is diagnosable over weeks
and does not need a tripwire.

**What replaces the threshold: required inputs.** The shadow gate is still the
checkpoint — it arrives after guides 00–09, before any capital is committed. It
must produce these, and a human reads them together:

- [ ] Expected profit per opportunity, by trigger class, with the spread — not a
      mean alone
- [ ] Modelled win rate against the observed `F(β)` (GUIDE 12 §6), and what it
      implies monthly at current volumes
- [ ] Liquidator concentration: top-3 address share, and our simulated timing
      versus theirs, per protocol
- [ ] In-scope miss rate and the coverage matrix (GUIDE 05 Step 3a) — the profit
      figure means little without knowing which slice it was measured on
- [ ] Infrastructure cost per month against all of the above
- [ ] The `declined` SYSTEM breakdown (GUIDE 05 Step 3) — the addressable
      opportunity not yet captured, which is a *reason to continue* and belongs
      in the same view

**The one discipline that survives.** Do not renegotiate a number after seeing
it. Manual does not mean retrospective — if you look at this table and decide to
continue, write down *why*, so the next time you look you are comparing against
your own reasoning rather than re-arguing from scratch.

Everything up to the shadow gate is recoverable regardless: the correctness work
transfers to any Ethereum data product you build later.

---

## 5. Smaller gaps in the guide set

Real, but none of them change the plan's shape.

**Infrastructure budget.** Never costed. Baremetal with multi-TB NVMe, a second
box if you chose the warm-spare posture (GUIDE 16 §7), CEX data subscriptions,
and the archive storage for the replay archive. Put a monthly number against it
and compare to Q1's answer.

**Reth breaking changes.** The ExEx API is not covered by a stability guarantee.
A major refactor upstream could cost weeks. Pin to a tag (`DEPENDENCIES.md` §3),
budget for one disruptive bump a year, and treat the replay harness as the thing
that catches it.

**MEV-Share access.** Confirm you can actually register as a searcher and submit
bundles before building the client (GUIDE 06). Verify on Sepolia early; do not
discover a reputation or access requirement in month five.

**Liquidating into a crash.** `minProfit` protects against a bad quote, not
against a correct quote on a wrong price. If an oracle prints badly and twenty
positions go liquidatable at once, you can execute all of them correctly and
still be holding the wrong side. GUIDE 14's caps bound this — set them
deliberately rather than at defaults.

**Dress rehearsal.** There is no step between fork tests and mainnet. Add one:
run the full stack against a shadow fork of mainnet with real state and real
timing, submitting to a test relay, for a week before the first live bundle.

**Key management specifics.** "Hot operator keys, cold sink" is stated but not
specified. Decide: where the hot keys live, how they are generated, how rotation
works mechanically, and who has the cold multisig. Rehearse a rotation before
you need one.

**Bus factor.** The guides say "a small team." If it is one person, an
always-running system with granular halts that require manual re-arm (GUIDE 14)
means being reachable. Decide what happens when you are asleep or away, and
whether an un-re-armed halt for twelve hours is acceptable. It probably is —
which is worth knowing in advance rather than at 3am.

**Which legal entity owns this.** You already have a separate MEV searcher
venture with a collaborator and have worked through the GbR vs. GmbH/UG
question. Settle explicitly whether this liquidation system sits inside that
vehicle or is separate — and if you are building it with the collaborator
without papering it, the GbR-by-operation-of-law risk you analysed for AuditAid
applies here too. Worth one conversation with your advisor now rather than after
the first profitable month.

---

## 6. Scoping when code is cheap

The guides were written to be handed to agents, and with capable models the
build itself is days to a couple of weeks, not months. So **scope by what
actually constrains you**, which is no longer developer-hours.

Three different constraints, and only one of them compresses:

| Constraint | Examples | Compresses with agents? |
|---|---|---|
| **Code** | adapters, flash sources, engine, contracts, router, MEV-Share client | **Yes, enormously** |
| **Calendar** | Reth sync, archive extraction, the drift week, shadow mode | **No.** A week of live observation takes a week. |
| **Data** | bid-model `α` fit, V4 timing optimizer, haircut tuning | **No.** Needs production history that does not exist yet. |

### What this changes about the plan

**Build wide, not narrow.** The earlier advice to cut to three protocols was
reasoning from effort, and effort is the wrong axis now. Breadth *is* the edge —
small wins across the long tail that larger operations skip. If an adapter is a
day of agent time, build twelve. Same for flash sources: all five in-scope arenas, not two.
Same for triggers: all nine.

**Defer only what is data-gated.** The bid model, the V4 timing optimizer and
haircut auto-tuning stay in phase 2, but not because they are expensive to write
— because they need observations you can only collect by running. Writing them
before you have the data produces a model fit to nothing.

**The critical path starts before the code.** Given the above, the long pole is
the calendar column, and two items on it can start today, before a line is
written:

1. **Write the prune profile, then bring the node up from a snapshot.** Nothing
   downstream works without a node, and `reth download --with-receipts-since`
   makes it hours rather than the week a genesis sync costs. The profile is
   decided first because it determines the download flags, and because Reth
   prunes as it runs — a node brought up on `--full` keeps ~33 hours of receipts
   and quietly destroys the replay archive you will want for root-causing misses.
   The recall gate itself is measured forward and needs no history (GUIDE 05
   Step 3) — but a miss you cannot reproduce over past blocks is a miss you debug
   by waiting for it to happen again.
   GUIDE 16 Step 0b.
2. **Run the GUIDE 05 extraction** against that node once it is synced. Local
   disk, no rate limits, no rental — provided step 1 was done right.

Everything else can proceed in parallel while those run.

### The realistic timeline

| | |
|---|---|
| Prune profile decided | ~1 hour, and it gates the sync |
| Node up (`reth download`) + local extraction | hours, in parallel with everything |
| Build guides 00–17 | ~1–2 weeks with agents |
| **Drift gate** (GUIDE 04) | **7 days of live observation, minimum** |
| **Recall gate** (GUIDE 05) | hours once the archive exists |
| **Shadow gate** (GUIDE 09) | **≥ 2 weeks and ≥ 200 contested opportunities**, so volume-dependent |
| First live submission | **~4–6 weeks out**, into the supervised window |
| **Supervised gate** (GUIDE 17 §0) | **2+ weeks** of watched sessions |
| Unattended running | **~7–9 weeks out** |

Most of that is waiting, not working. That is a very different statement from the
six-to-eight months this document previously implied, and the correction is
yours — but it is also different from "a few days," and the difference is
entirely the gates.

### Why the gates survive cheap code

They are the thing that makes it professional-grade rather than a toy, and they
get *more* important when code is generated, not less.

An agent will produce an Aave V4 adapter that compiles, reads correctly, and has
one rounding direction backwards. It will be invisible at HF 1.3 and decisive at
HF 0.9999. Nothing about the code looks wrong; the only thing that catches it is
the drift detector disagreeing with the chain, and the differential fuzzer biased
to the boundary.

So the shape of the risk moves. It is no longer "will this get built" — it is
"is the thing that got built subtly wrong in a way that silently loses money."
The acceptance criteria in each guide exist for exactly that, and they are the
part worth being strict about precisely because everything upstream of them got
fast.

One asset worth using: you run a security research community. The executor
contract (GUIDE 10) is the one component where a bug costs more than an
opportunity, and self-reviewing your own liquidation contract has an obvious
blind spot. Getting other eyes on it is cheap for you and it is the highest-value
review in the project.

---

## 6b. What this does not change

The engineering is sound and the ordering is right. Guides 00–05 — the
correctness spine — are worth building regardless of what the market study says,
because an accurate real-time model of every lending position on Ethereum has
value beyond liquidations: risk monitoring, protocol analytics, a data product,
or the foundation for a different strategy entirely.

That is the actual hedge. The three gates exist so that you find out early and
cheaply, and the study in §2 moves the first real decision point from month six
to week two.

---

## 7. Protocol changes on the horizon

The chain is not a fixed platform, and two of this design's assumptions have
expiry dates. Neither is a reason to redesign now — both are reasons to
**parameterise rather than hard-code**, and to keep a watch item.

### Gas limit — parameterise it

30M → 36M → 45M → **60M** (November 2025), and Glamsterdam targets **200M** with
devnets already running it. Read `gasLimit` from the block header and derive the
batch budget from that (GUIDE 12 §4f). Anything hard-coded is wrong within a year.

### Glamsterdam — watch, do not pre-build

| Change | Touches |
|---|---|
| **EIP-7732 (ePBS)** — enshrined proposer-builder separation, described as replacing reliance on external mev-boost middleware | The whole submission path: relays, builder endpoints, `coinbaseDiff` valuation (GUIDE 13 §1). **The most exposed part of this design.** |
| Block gas limit → 200M target | Batch sizing (GUIDE 12 §4f) — the rule holds, the budget grows |
| **EIP-2780** baseline cost restructuring; state creation repriced | The gas oracle and every per-liquidation gas estimate |
| **EIP-7928** block-level access lists | What is inferable from a block; parallel execution |
| Data propagation window 2s → 9s | The latency budget (GUIDE 16 §4b) |

The searcher-facing implications of ePBS are not settled, and guessing them now
would bake in assumptions worse than the ones we have. **Track it; do not
pre-build for it.** Revisit when the specification stabilises or when a testnet
exposes the searcher submission path concretely.

What this does mean today: no constant in this system should encode a protocol
parameter that validators or a hard fork can move. Gas limit from the header,
per-liquidation gas from measurement, base fee from the EIP-1559 formula rather
than an observed average.
