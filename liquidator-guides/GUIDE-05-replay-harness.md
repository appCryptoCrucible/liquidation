# GUIDE 05 — Replay Harness & Fixtures

| | |
|---|---|
| **Crate** | `liq-replay` |
| **Prerequisites** | GUIDE 02, 03, 04; the synced node (A3) for the archive |
| **Work packages** | **W** the watcher / ground-truth decoder (`liq-watch`, D51 — starts on day 0, not here); **05A** differential fuzz harness; **05B** archive extraction + ground truth; **05C** recall harness + miss classifier + declines + timing; **05D** named fixtures + CI + determinism; **05E** swap-math parity; **05F** lite validation |
| **Est. effort** | 1 week |
| **Blocks** | 09 |

## Objective

Turn "the drift detector was quiet for a week" into a repeatable, CI-runnable
regression test, and establish the **detection recall** metric that gates
everything downstream.

This works only because the engine is a pure fold over an event stream
(GUIDE 02/03). Backtesting is not a separate system — it is the production code
path fed a recorded stream. If you find yourself writing a "backtest mode", the
purity assumption has been violated somewhere; fix that instead.

## Build vs. buy

80% build. Buy Foundry for the differential fuzzing half. The archive and
harness are yours.

---

## Step 0 — Lite validation, before any of this

Everything below needs the log-filtered node (GUIDE 16 Step 0b). That node is
weeks away on the calendar, and the detector is testable long before it exists.
Do this first, on a free public RPC, in an afternoon.

**The setup.** Subscribe to new blocks. Pick a bounded position universe — every
borrower that appears in a `Borrow`/`Supply`/`Repay` event on one enabled market
over the last few thousand blocks — and track only those. Run the real detector
over them, live, forward. Log every position it puts at HF < 1, with the block.
Separately, watch for actual liquidation events on that market. Then match, in
both directions:

| Observation | Meaning |
|---|---|
| We flagged it, it was liquidated | The detector works on this shape |
| It was liquidated, we never flagged it | **A bug.** Root-cause before anything else |
| We flagged it, nobody liquidated it | Either your HF is wrong, or it was genuinely unprofitable for everyone |
| We flagged it and declined it | Record the reason — Step 3's classification applies here too |

The third row is the one people skip, and it is a real signal: a false positive
is the same adapter bug as a false negative, seen from the other side.

**What this establishes and what it does not.** It catches gross errors — a
misdecoded event, an inverted comparison, a wrong scaling factor, a missing event
path on a common shape. Those are the majority of bugs by count and it finds them
for the price of an RPC key. It establishes **nothing about recall**, because the
universe is partial and the window is days. It is a smoke test.

**Do not let it become the gate.** `TESTING.md` §6 — a fast passing thing
substituted for slow evidence is the specific failure this whole document exists
to prevent. The gate is Step 3, and Step 3 is measured over months of forward
running. This step's only job is to make sure that when you get there, you are
not debugging a decoder.

## Step 1 — Record the event archive

Capture, for a chosen historical window (start with 6 months):

```rust
pub enum ArchivedEvent {
    Block { number: BlockNum, hash: B256, timestamp: u64 },
    Log(DecodedLog),
    PriceUpdate { feed: FeedId, price: Price, tx: TxHash, block: BlockNum },
    Reorg { depth: u8, old_tip: B256 },
}
```

Store as partitioned, compressed columnar files keyed by block range — parquet is
fine; the point is that a replay of one month must not require reading six.

**The source is your own node, not an archive.** This surprises people, so it is
worth being precise about why.

What replay consumes is the **event stream** — logs, decoded. It does not consume
historical state, because reconstructing state from events is the thing being
tested. So the only history you need retained is receipts, and only for the
contracts you track. GUIDE 16 Step 0b configures exactly that with
`prune.segments.receipts_log_filter`: history pruned to 10,064 blocks, receipts
kept from deployment for the tracked address set. The extraction is then a local
`eth_getLogs` walk over your own disk, at local-disk speed, with no rate limit
and no bill.

**This only works if the node was synced with that config.** A `--full` node
keeps receipts for 10,064 blocks — about 33 hours — and the history is gone
before you ever open this guide. If that happened, you have two options and they
are both worse than getting it right on day zero: re-sync with the correct filter
(days), or rent an archive RPC for a month (QuickNode, Alchemy, Chainstack;
tens to a few hundred euros) and backfill. Dune or Flipside covers the
liquidation ground truth in Step 2 cheaply either way, and is a useful
cross-check on your own extraction regardless of source.

**Verify the extraction against a second source before trusting it.** Pruned-node
`eth_getLogs` over wide ranges has returned silently empty results rather than
erroring. Take one known liquidation from the far end of the window and one from
the middle, and confirm both appear. A replay harness fed a quietly truncated
archive reports excellent recall against the liquidations it can see.

Once the decoded parquet exists it is the artifact — tens of gigabytes, portable,
and independent of how it was produced.

**The archive is a diagnostic, not the gate.** Step 3 explains why the recall
gate is measured forward rather than over this window. The archive still earns
its cost — it is what lets you root-cause a miss over history instead of waiting
for the shape to recur, and it is what trains the bid model in Step 2 — but if
the extraction is slow, partial, or a month behind, that blocks nothing. Build it
because it is nearly free once the node is configured, not because the gate is
waiting on it.

Record decoded events, not raw ones, so replay does not re-run the decoder under
a changed ABI and silently diverge.

## Step 2 — Ground truth: every actual liquidation

Extract every liquidation that actually occurred on every tracked protocol in the
window:

```rust
pub struct ActualLiquidation {
    pub block: BlockNum,
    pub tx_index: u16,
    pub position: PositionKey,
    pub liquidator: Address,
    pub repay_asset: AssetId, pub repay_amount: U256,
    pub seize_asset: AssetId, pub seize_amount: U256,
    /// Derived: coinbase transfer + priority fee, for the auction model.
    pub inferred_bid: Option<U256>,
    /// Was this bundled with an oracle update in the same block?
    pub oracle_backrun: Option<TxHash>,
}
```

`inferred_bid` is what trains the bid model in GUIDE 12. Extract it now while you
are already parsing these blocks — going back for it later means reprocessing
everything.

## Step 3 — Harness 1: detection recall

The gate metric.

```rust
pub struct RecallReport {
    pub total_actual: usize,
    pub detected_before: usize,      // hf < 1 at or before the liquidating block
    pub detected_late: usize,
    /// Every entry must be CLASSIFIED (Step 3a). The in-scope subset
    /// is the gate metric; the rest are explained exclusions.
    pub never_detected: Vec<(ActualLiquidation, MissClass)>,
    /// Detected, and correctly not pursued. Not a miss — see below.
    pub declined: Vec<(ActualLiquidation, DeclineReason)>,
}

pub enum DeclineReason {
    /// GUIDE 07 — no flash source for this debt asset, or none at this size.
    /// Class: SYSTEM, unless the asset is genuinely exotic.
    DebtNotFlashloanable,
    /// GUIDE 12 §4b — below the lower edge: gas exceeds the bonus at that
    /// block's base fee. Class: MARKET.
    BelowBand,
    /// GUIDE 12 §4b — above the upper edge: our routing cannot move the
    /// collateral at that size without eating the bonus. Class: SYSTEM.
    AboveBand,
    /// GUIDE 07 leg 3 — no path out of the collateral.
    /// Class: SYSTEM if the registry is missing the venue, MARKET if the
    /// collateral is genuinely illiquid. The report must say which.
    NoCollateralExit,
    /// Adapter exists, config says off. Class: KNOWN.
    ProtocolNotEnabled,
}
```

`OutsideViabilityBand` is split deliberately. The two edges have opposite
meanings — one is a fact about the position, the other is a fact about you — and
a single variant hides the difference behind a plausible-sounding label.

Replay the archive through the real engine. For every `ActualLiquidation`, assert
your engine had that position at HF < 1 at or before the block it was liquidated
in.

**The gate is the in-scope miss rate — not a blended percentage.** The
distinction matters more than it looks, because a single ratio over all
liquidations conflates two opposite things:

- A position you **never saw** is a correctness bug in the adapter or ingest
  layer. No amount of execution tuning compensates for it.
- A position you saw and declined is a *decision*, and decisions are judged by
  their reasons, not their count.

So a raw "we matched 90% of actual liquidations" is not a pass or a fail — it is
an unread result. Split it and both halves become actionable.

### `declined` is not a comfortable number

Every entry in `declined` is a liquidation **that actually happened**. Somebody
did it, on-chain, and was willing to pay gas for it. That makes `declined` a
biased sample in exactly the unhelpful direction: it contains only positions
already demonstrated to be worth executing by at least one party.

So the default reading of a decline is suspicion, not reassurance. There are
legitimate declines — the winner held inventory and needed no flashloan, the
winner was the protocol's own keeper, the position was dust that someone swept
for reputational or strategic reasons — but "we correctly skipped it" has to be
*argued*, per reason, against a counterparty who did not skip it.

**Classify every reason, and expect the classes to behave differently:**

| Class | Meaning | What to do |
|---|---|---|
| **MARKET** | A fact about the position. Nothing to build. | Record the share; sanity-check it against the market study |
| **SYSTEM** | A deficiency of ours wearing a decline's clothing | **Work queue.** Each one is addressable opportunity |
| **KNOWN** | Our own config, deliberately | Expected; confirm it matches the enabled set |

`AboveBand` is the sharpest example. "The position was too large for my routing"
is not the market declining us — it is our router being weak, and a better route
converts it into a win. Same for `NoCollateralExit` when the cause is a registry
gap rather than real illiquidity, and same for `DebtNotFlashloanable` when the
cause is an incomplete `FlashIndex` rather than an exotic asset.

**Do not assume declines will be numerous.** On Aave V3 Ethereum, three assets —
WETH, USDT, USDC — are about 96% of borrowed value, and all three are deeply
flashloanable at zero fee (`REGISTRY.md`, and the market study's Q5/Q6). On that
distribution, `DebtNotFlashloanable` should be rare. If it is not, the first
hypothesis is a gap in your flash-source index, not a fact about the market. A
large `declined` bucket is a finding to investigate, not a sign of a
well-calibrated filter.

The gate therefore requires the **breakdown, classified**, not a target size. Any
single reason above ~20% of declines needs a written explanation in `STATE.md`
alongside the number, and any SYSTEM reason above that threshold is a defect to
route back into GUIDE 07 or 12 before this gate clears.

**Why zero is a cheaper target than it sounds.** Detection misses are not
randomly distributed across the tail — they cluster. A decoder that mishandles
one collateral type misses *every* position holding it; an event path you forgot
misses every position that moved through it. Ten percent missing is usually one
or two bugs, not five hundred edge cases, and the miss list tells you which. That
is also why stopping at 90% is the wrong instinct: the residue is not noise, it
is a systematic blind spot over some asset or protocol — and by the argument in
`PRE-FLIGHT.md` §3, the uncontested opportunity lives disproportionately in the
places other people's bots are also not looking.

Every entry in `never_detected` gets root-caused and turned into a fixture
(Step 5).

### Step 3a — Every miss gets decoded, and the denominator is in-scope

Not every liquidation on a tracked protocol is one we wanted. A $4 seizure of a
long-tail collateral, a borrower self-liquidating to avoid something worse, a
protocol keeper doing its job, a bot running at a loss — none of those are
evidence that our detector is broken, and counting them against it produces a
metric that can never be driven to zero and therefore never read.

So `never_detected` is not a number. It is a list, and **every entry is decoded
individually** into one of:

```rust
pub enum MissClass {
    /// Target debt asset, tracked protocol, flashloanable at that size,
    /// exit path existed, inside the band at that block's base fee.
    /// THE GATE METRIC. Root-cause every one.
    InScope,
    /// Protocol or instance we do not track.
    OutOfScopeProtocol,
    /// Debt or collateral outside the configured target set.
    OutOfScopeAsset,
    /// Below the lower edge — gas exceeded the bonus at that block. MARKET.
    BelowBand,
    /// Above the upper edge — too large for our routing at that block. SYSTEM;
    /// see Step 3's classification. Excluded from the denominator, but tracked
    /// separately, because this is our limitation and not theirs.
    AboveBand,
    /// No flash source for this debt asset at that size.
    NotFlashloanable,
    /// The executor's own realized economics were <= 0. Someone else's bad
    /// trade is not our miss.
    ExecutorUnprofitable,
    /// liquidator == borrower, or a known protocol keeper address.
    SelfOrKeeper,
}
```

**The threshold (D40).** If the **in-scope** miss rate exceeds **30%**, there is
a systemic blind spot and it must be found before proceeding. At or below 30%,
proceed to the next stage and keep tuning as observations accumulate. Stated as
the integer test the code runs: **pass iff `misses · 10 ≤ 3 · in_scope`** — at
`n = 50` that is `misses ≤ 15`. The table below is computed for that comparator
(`≤ 15`); a strict `< 30%` (`≤ 14`) would read 45 / 19 / 5 / 1 instead.

**The floor is 50 in-scope observations** — not 50 liquidations. Most observed
liquidations will classify out of scope, so a run of 50 total might carry only a
dozen in the denominator; the count that matters is the one the rate is computed
over.

What 50 buys, stated plainly so nobody over-reads the number:

| If the true in-scope miss rate is | it passes a 50-observation check |
|---|---|
| 30% | 57% of the time |
| 35% | 28% |
| 40% | 10% |
| 45% | 2% |

So 50 reliably catches a system that is broken (45% and up passes twice in a
hundred) and discriminates poorly right at the line (a true 35% slips through
about a quarter of the time). That is the correct trade here: the miss rate is an
**opportunity** metric, not a safety one — missing a liquidation costs the
liquidation and nothing else — so the cost of a borderline pass is some foregone
profit and more data arriving anyway, not a loss.

#### Per-slice, not just aggregate — this is where 50 is genuinely thin

The whole justification for 30% is that **misses cluster by cause.** That
argument cuts both ways: if misses cluster, then an aggregate over 50 can hide a
complete failure in a thin slice.

Concretely: one collateral type missed 3 times out of 3 observations, inside a
run of 50 where everything else was clean, is a **6% aggregate miss rate and a
100% failure on that asset.** It passes comfortably and it is exactly the
signature the 30% threshold was written to catch.

So report the rate **per coverage row** as well as in aggregate, and:

> **Any slice where every in-scope observation was missed is root-caused before
> proceeding, regardless of its n — including n = 2.**

This is not a statistical claim and does not need to be one. Low n means you
cannot conclude the slice is broken; it does not mean you should not *look*.
Looking costs a handful of positions and a fork call each. The confidence
threshold governs whether you proceed, not whether you investigate.

Slices with zero in-scope observations are `uncovered` rows, not passes — the
window section below.

Thirty percent is defensible for a specific reason, not as a round number:
**detection misses cluster by cause.** A decoder that mishandles one collateral
type misses every position holding it; a forgotten event path misses every
position that moved through it. So a third of in-scope liquidations missing is
one or two bugs with names, not three hundred edge cases — which is exactly the
situation where "find it before continuing" is the right instruction. Below that,
the remaining causes are individually small and are better found by accumulating
more observations than by staring at the ones you have.

**The hard requirement is not the miss rate — it is that no miss is unclassified.**
A miss sitting in `InScope` is a known bug with a work item. A miss sitting in
`OutOfScopeAsset` is a deliberate exclusion. A miss sitting in *neither*, because
nobody decoded it, could be either, and a pile of those is how a systemic blind
spot hides inside an acceptable-looking ratio. 30% in-scope is a *proceed*
threshold; **100% classified is a floor and does not move.**

#### The classifier cannot use our position state

This is the part that voids the whole metric if it is got wrong, and it is easy
to get wrong.

If we never detected a position, we do not have its state. The obvious way to
decide "was it in scope" is to reconstruct the position and ask the adapter — and
that is a tautology of the exact kind `TESTING.md` §2 forbids. **A decoder bug
that caused the miss will also cause the position to be mis-classified as
out-of-scope.** Both errors share a root. A buggy system asked to grade its own
misses will report that they did not matter.

The escape is that **every input the classification needs is derivable from the
liquidation event and a fork call, with no reference to our state:**

| Input | Independent source |
|---|---|
| Debt asset, collateral asset | The event's own fields |
| Size | `debtToCover`, `liquidatedCollateralAmount` from the event |
| Protocol instance | The emitting contract address |
| In our target set? | Our **config** — a declared input, not a computed one |
| Flashloanable at size? | `eth_call` the flash source at block *N−1* — not our `FlashIndex` cache |
| Exit path exists? | Fork-quote collateral → debt at block *N−1* — not our router's cached pool state |
| Bonus, and the band edges | Oracle prices by fork call at that block, plus the block's own base fee |
| Executor's realized economics | Their receipt: gas × effective price, coinbase transfer, their token deltas in that tx |

Every row is the chain. None is us. **Build the classifier that way and the metric
is sound; build it against our own cached state and it certifies our own bugs.**

Expect this to be slow — it is a fork call per miss — and that is fine. Misses are
rare by construction and each one is worth a few seconds of RPC.

### The window: coverage, not calendar

The obvious way to state this gate is "no misses over six months." That is the
wrong shape, for two reasons beyond the denominator problem Step 3a just fixed.

It is **unaffordable as stated**. Replaying six months requires the full event
stream — every borrow, repay, supply, withdraw, transfer and oracle update across
every tracked contract, millions of logs — which is the expensive half of Step 1.
The *ground truth* half is cheap (a handful of event signatures, a few thousand
events, fetchable from a free RPC with patience), but the replay input is not.

It is also **the wrong measurement**. Six months was never the requirement; it
was a proxy for "enough variety that an empty miss list means something." A
six-month window containing one market regime and four collateral types proves
less than three weeks spanning a cascade. The proxy can be replaced by the thing
it was standing in for.

**So the gate is measured forward, and cleared by coverage.**

The detector runs continuously from the moment it works — it has to anyway, for
the miss alarm (GUIDE 09 Step 4a), and the miss alarm *is* this measurement. Each
observed liquidation lands in one of the three buckets. The gate clears when the
accumulated observation covers:

- [ ] **≥ 1 observed liquidation on every enabled protocol instance** — each Aave
      market, each V4 spoke, each enabled Morpho market family. Not "Aave": the
      instance, because a spoke with its own config is its own adapter path
- [ ] **≥ 1 per collateral family** present in the tracked universe — volatile,
      stablecoin, LST/LRT, and any long-tail asset above a configured share of
      tracked debt
- [ ] **≥ 1 per trigger class observed** — oracle update, interest accrual, a
      user action that worsened HF
- [ ] **≥ 1 block in the top decile of realized volatility** over the observation
      period. Quiet-market evidence is a biased sample of the easy case
- [ ] **≥ 50 in-scope observations** (D40) — enough that the rate is not the
      small-numbers artifact it would be at n = 4. In-scope, not total
- [ ] **The in-scope miss rate (Step 3a) computed across all of it**

This is strictly stronger than a calendar window — it measures the diversity the
calendar was approximating — and it costs nothing but time. It also cannot be
cleared by picking a favourable window, because there is no window to pick: you
observe what happens.

**The matrix qualifies the miss rate; it is not a second gate.** An in-scope miss
rate of 8% measured over a period containing no volatile block and no LST
collateral is an 8% figure about the easy half of the market. An uncovered row
does not fail the gate — it marks the slice where the number is unmeasured, which
is the honest thing to carry forward into the next stage rather than a reason to
stop.

**The cost is that rare shapes arrive at their own rate.** If a governance
parameter change or a long-tail collateral liquidation happens quarterly, forward
observation waits a quarter. Two mitigations, in order of preference: run the
backward replay over whatever history the archive covers and count those
observations toward coverage (they are the same ground truth, arriving faster);
and where a row simply will not fill, record it as an **uncovered row** in
`STATE.md` with what is missing, rather than quietly dropping it. An uncovered row
is a known blind spot, which is a different thing from a cleared gate.

A row filled only by archive replay, never observed live, is marked as such. It
counts, but it is weaker evidence: replay proves the fold handles the shape, live
observation proves the ingest path delivers it in time.

## Step 4 — Harness 2: timing

For each detection, record the block and intra-block position at which you would
have fired, versus the winner's actual position.

```
detection_block_delta   : how many blocks early were you?
intra_block_delta       : within the trigger block, before or after the winner?
```

This is the diagnostic that separates "I need faster ingest" from "I need a
better bid". Before execution exists (GUIDE 13), it is your only honest estimate
of competitiveness — and it is available months before you risk capital.

## Step 5 — Harness 3: named fixtures

A fixture is a specific historical block range plus an assertion. One per guard
condition, named after the event so a failure tells you what you broke.

Minimum set, from the GUIDE 03 coverage audit:

```
fixtures/
├── v4_risk_premium_accrual/        # premium drifts HF over a quiet period
├── v4_target_hf_close_factor/      # dynamic repay amount matches chain
├── v4_bonus_curve_ramp/            # bonus at 3 HF levels matches chain
├── v4_dust_floor_full_clear/       # remainder below floor → full clear
├── v4_spoke_config_change/         # MarketReprice fans out correctly
├── v4_position_manager_action/     # position moves without a direct user tx
├── v3_emode_recategorization/      # thresholds change, balances don't
├── v3_isolation_ceiling/           # isolated collateral accounting
├── v3_grace_period/                # unhealthy but not liquidatable
├── v3_deficit_accounting/          # 3.3+ bad debt
├── collateral_token_transfer/      # position moves with no protocol event
├── flashloan_mediated_change/      # intra-tx state churn
├── governance_param_change/        # market-wide reprice
└── reorg_depth_8_and_64/           # unwind correctness under real reorgs
```

Each fixture: a pinned block range, the expected engine behavior, and a one-line
comment saying what real-world thing it encodes. These run in CI.

## Step 6 — Harness 4: differential math fuzz

Foundry fuzz tests comparing your Rust `health()` against the protocol's own
on-chain view function over randomized state. Run per protocol, and for V4 per
spoke configuration; for V3 per e-mode category and isolation setting.

This is how rounding-direction bugs surface. They are invisible at HF = 1.3 and
decisive at HF = 0.99995, so random sampling across the full range will miss
them — **bias the fuzzer's state generation toward HF ∈ [0.995, 1.005]**.

## Step 6a — Market sizing, if PRE-FLIGHT was skipped

`PRE-FLIGHT.md` §2 asks six market questions that should have been answered
before GUIDE 00. If they were not, answer them here — the archive and ground
truth from Steps 1–2 make it nearly free, and this is the last checkpoint before
the expensive half of the project.

From the `ActualLiquidation` set you already extracted:

- Total bonus paid per protocol per month, 18 months
- Fraction SVR-backrun vs. not
- Winner concentration: share taken by the top 1/3/5/10 addresses
- Searcher-retained value (gross bonus − inferred winning bid), trend by month
- Size distribution vs. the viability band (GUIDE 12 §4b) at historical gas —
  what share of real liquidations fell inside your band, and how much of the
  miss was the lower edge versus the upper one
- **Share of liquidations that were NOT auctioned** — classify by the nine
  trigger classes in `PRE-FLIGHT.md` §3 and sum the non-auctioned rows. That is
  the addressable pool, and it is far larger than the interest-drift row alone

That last number decides how to weight effort between GUIDE 12's bid model and
GUIDE 08's breadth. It does not decide which triggers to support — the answer
there is always all of them.

## Step 6b — Harness 5: swap-math parity

The same differential pattern, second target. Fuzz `Dex-Math-Core-rs` quotes
against revm executing the real pool bytecode, over randomized pool states and
input sizes, **biased to the notionals you actually trade** rather than uniformly.

Cover every AMM family you actually route through — V2, V3, Curve StableSwap,
Kyber Elastic — and treat any wei-level divergence as a bug in
one of the two until proven otherwise.

**Uniswap V4 is excluded from routing** (GUIDE 12 Step 3) because hooks make
quoting pool-specific, so there is nothing generic to parity-test. If you later
allowlist an individual V4 pool, its hook gets its own fixture here before it
prices anything.

Two reasons this is not optional. The library's own README flags the Uniswap V4
implementation as new and recommends independent parity validation before live
capital. And at near-maximal bids, quote error is bid error: a 0.1% divergence on
an $8,000 gross is $8, a sixth of a $50 floor. A systematically optimistic quote
produces bids that cannot be honoured and bundles that revert.

You already have the pool-state history from Step 1, so this costs a harness and
not much else.

## Step 7 — Wire into CI

- Fixtures + differential fuzz: every commit
- Full recall replay over whatever the archive covers: nightly, and **mandatory
  before any dependency bump**, especially Reth (the ExEx API is the most likely
  silent regression)
- Publish `RecallReport` as a build artifact so regressions are visible as a
  number, not a test failure

---

## Acceptance criteria — the Stage 1 gate

- [ ] **Step 0 lite validation run and clean** before the node exists — no
      unexplained miss on the bounded universe
- [ ] **Classifier built from chain-independent inputs only** (Step 3a) — no
      reference to our position state, `FlashIndex` cache or router cache
- [ ] **100% of `never_detected` classified.** No unclassified entry, ever —
      this floor does not move
- [ ] **≥ 50 in-scope observations** accumulated (D40) — in-scope, not total
- [ ] **In-scope miss rate < 30%** (D40). Above it, the systemic cause is found
      and fixed before proceeding — not noted and passed
- [ ] **Miss rate reported per coverage row**, not only in aggregate
- [ ] **No slice with 100% in-scope misses**, at any n, left un-root-caused
- [ ] Every `InScope` miss has a root cause and a fixture
- [ ] **Every coverage row in Step 3 filled**, or recorded as uncovered — the
      matrix states where the miss rate is measured, and where it is not
- [ ] **`declined` breakdown classified** MARKET / SYSTEM / KNOWN, with any reason
      above 20% explained, and no SYSTEM reason above 20% left unaddressed
- [ ] All named fixtures from Step 5 exist and pass
- [ ] Differential fuzz: 100k cases biased to HF ∈ [0.995, 1.005], zero mismatches
- [ ] Replay of one month completes in < 10 minutes (if not, the archive
      partitioning or the fold is doing I/O it shouldn't)
- [ ] Replay is deterministic: same archive, same output, bit-identical, twice
- [ ] Timing report produced, with the block-delta distribution
- [ ] CI runs fixtures on every commit and full replay nightly

## Failure modes

| Symptom | Cause |
|---|---|
| In-scope miss rate above 30% | A clustered cause — one collateral type, one event path, one instance. Group the misses by asset and by protocol before reading them individually; the cluster is usually obvious. |
| Aggregate rate passes but one slice is all misses | The failure the aggregate was never going to catch. Root-cause it regardless of n; see Step 3a. |
| 50 in-scope observations are slow to arrive | Expected in quiet markets. Convert the market study's arrival rate (Q5/Q6) into a calendar estimate early, so the gate's duration is planned rather than discovered. |
| Misses mostly classify as out-of-scope | **Check the classifier's inputs first.** If it touches our state, this is exactly the result a buggy detector produces about itself (Step 3a). |
| `declined` is most of the sample | Almost certainly a SYSTEM cause — an incomplete flash-source index or a weak router — not a market fact. Classify before concluding. |
| A coverage row never fills | Expected for rare shapes. Record it uncovered; do not redefine the row to fit what you observed. |
| Replay non-deterministic | Hidden I/O, a `HashMap` iteration order dependency, or wall-clock time in the fold |
| Replay slower than real time | Archive not partitioned, or WAL/snapshot on the replay path |
| Fixtures pass, production drifts | Fixture window doesn't cover the live protocol version — re-pin after every protocol upgrade |
| Differential fuzz always passes | State generator isn't biased to the boundary; it's testing HF = 1.4 |

## Handoff

With the in-scope miss rate under 30%, every miss classified, and the coverage
matrix stating where that number does and does not apply, you have earned the
right to build the parts that decide.
GUIDE 06 (oracle) and GUIDE 08 (engine) can proceed in parallel from here —
they touch different crates and the replay harness will catch integration
mistakes in either.
