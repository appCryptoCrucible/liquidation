# TESTING — What Must Be Asserted, and Against What

Every guide has acceptance criteria. This document is the layer underneath them:
**what each test must actually prove, where the expected value is allowed to come
from, and what the plausible-looking fake version of that test is.**

It exists because of one failure mode.

---

## 1. The failure mode

An agent that writes both the code and its test derives the expected value *from
the code*. The test then asserts what the code does rather than what it should do,
and a bug is locked in with a green checkmark on top of it.

This is not laziness and it does not look wrong. It looks like this:

```rust
// Tautology. Proves the function is deterministic and nothing else.
let health = adapter.health(&position);
assert_eq!(health, compute_expected_health(&position));  // ← same formula
```

Both sides trace to the same understanding. If that understanding is wrong —
which is precisely what the test was for — both are wrong together.

**Coverage does not detect this.** A tautological test executes every line.

---

## 2. The oracle rule

> **Every assertion names where its expected value came from. If the answer is
> "the code under test," the test is void.**

This is checkable by review, which is what makes it useful. An agent can be asked
to annotate each assertion with its oracle, and an assertion that cannot name one
is deleted rather than argued about.

### The four legitimate oracles

| Oracle | What it is | Where it applies |
|---|---|---|
| **The chain** | A deployed contract's view function, an actual receipt, real token behaviour on a fork | Health math, protocol responses, token quirks, gas |
| **An independent implementation** | Rust encoder vs Solidity decoder; your swap math vs revm; your state fold vs `eth_call` | Encoding, quoting, state reconstruction |
| **A mathematical invariant** | `undo(apply(x)) == x`; V4 deltas net to zero; split allocations sum to the total; monotonicity | Store, swaps, solver |
| **A recorded real-world outcome** | A historical liquidation and what actually happened in it | Recall, profit model, timing |

Anything else — a constant you typed because the code returned it, a helper that
reimplements the logic, a mock that behaves the way you assumed — is the code
under test wearing a different hat.

---

## 3. The assertion map

Per area: what must be true, the oracle that establishes it, and **the fake** —
the test that passes, looks reasonable, and proves nothing. Naming the fake is
what stops it being written.

### Fixed-point math — GUIDE 00

| | |
|---|---|
| **Assert** | Every rounding direction matches the protocol's, at the boundary |
| **Oracle** | The protocol's own on-chain view function, differentially fuzzed |
| **Fake** | Asserting `mul_ray(a,b) == <constant the code produced>`; or fuzzing uniformly over the input space |

**Bias the fuzz to the boundary.** Uniform random inputs spend their time at
HF ≈ 1.4, where nothing is at stake. The assertion is worthless unless the
generator concentrates on HF ∈ [0.995, 1.005], where a 1-wei rounding difference
decides whether a position is liquidatable at all.

A fuzz test that always passes is usually testing the wrong region, not proving
correctness. Treat "100k cases, zero failures, first run" as a reason to inspect
the generator.

### State store and event fold — GUIDE 02, 03

| | |
|---|---|
| **Assert** | Replaying events reproduces chain state exactly; `undo(apply(x)) == x` for every event path |
| **Oracle** | `eth_call` at that block (chain), plus the round-trip invariant |
| **Fake** | Asserting the store returns what you inserted. That tests a hashmap. |

The reorg path needs its own negative assertion: apply a chain of events, unwind
to depth 8 and 64, and assert byte-equality with a snapshot taken before. An undo
record that silently drops a field passes every forward test.

### Adapter health — GUIDE 04

| | |
|---|---|
| **Assert** | `adapter.health()` equals the protocol's view function, to the wei, across e-modes, isolation settings and spoke configs |
| **Oracle** | **The deployed contract.** Fork, call, compare. |
| **Fake** | Computing the expected health in the test using the same formula the adapter uses |

**This is the single most likely tautology in the system**, because writing the
expected value out longhand feels like independent verification and is not. The
expected value must come from a different implementation — and the chain is
sitting right there.

### Detection recall — GUIDE 05

| | |
|---|---|
| **Assert** | In-scope miss rate below threshold across a filled coverage matrix of real liquidations, every miss classified |
| **Oracle** | Actual on-chain liquidations, and **fork calls** for the scope classification — independent by construction |
| **Fake** | Replaying synthetic events you generated; a fixture set containing only cases you already thought of; a calendar window standing in for coverage; **classifying misses with the engine that missed them** |

Structurally the soundest test in the system, because the ground truth was
produced by other people. Protect that: the observations must come from chain,
never from a generator.

The subtle fake here is the window. "Six months elapsed" is not the assertion —
"every enabled protocol instance, collateral family and trigger class was
observed, plus a volatile block" is, and a long quiet window can satisfy the
first while failing the second. GUIDE 05 Step 3.

**The sharpest tautology in this document lives here.** The gate's denominator is
*in-scope* liquidations, so something has to decide scope for a position we never
detected — and the obvious implementation reconstructs it with our own adapter.
That is the code under test grading its own failures: the decoder bug that caused
the miss also reports the miss as out-of-scope. Every input must come from the
liquidation event and a fork call (GUIDE 05 Step 3a), never from our state,
`FlashIndex` or router cache. This one passes review easily because the wrong
version looks more thorough than the right one.

A second-order form: an unclassified miss. It is not asserted either way, so it
silently leaves the denominator. "100% classified" is what closes that.

`declined` is the other gameable direction, and not in the way it first looks.
Every entry is a liquidation somebody executed, so declines are evidence to
explain, not a filter working. Classify each reason MARKET / SYSTEM / KNOWN; a
SYSTEM reason is a defect wearing a decline's label.

### Plan encoding — `PLAN-ENCODING.md` §4

| | |
|---|---|
| **Assert** | Rust encoder and **Solidity** decoder agree field-for-field, over randomised plans |
| **Oracle** | The other implementation, reached by fork call |
| **Fake** | Round-tripping Rust→Rust through a decoder the same agent wrote. Or a single fixture. Or varying one count while holding the others at 1. |

The decoder under test is the one in `Executor.sol`, called on a fork. A Rust
reimplementation shares the author's misreading of the layout and will agree with
the encoder about it.

**Vary every count independently** — groups, legs per group, repay swaps, profit
swaps, `data` length — including the minimum of each. The decoder derives offsets
by *walking* variable-length legs, so:

- a stride bug is correct for leg 0 and wrong for every leg after it
- a walk bug only appears when a variable-length leg precedes what you are reading

Single-group, single-leg fixtures find neither. This is the test most likely to be
written in a shape that cannot fail.

### Executor contract — GUIDE 10

| Assert | Oracle | The fake |
|---|---|---|
| Every callback reverts when `msg.sender` is not the provider we called | Negative test | Only testing the authenticated path |
| The CREATE2 pool check actually runs | Real pool on a fork | A mock pool — the derivation never executes |
| No arbitrary external call is reachable | Exhaustive path review + a test that a non-allowlisted router reverts | "We reviewed it" |
| **Profit is WETH and only WETH** after `execute` | Balance assertions on every asset touched | Asserting only the WETH balance rose |
| Bid computed from **realized** net | A swap that under-delivers; assert the bid shrank | A swap that delivers exactly the quote |
| Per-leg failure tolerance | Kill one leg's position mid-test; assert the others landed | All-legs-succeed only |
| Router allowance is zero after each leg | Direct allowance read | Trusting the comment |
| **USDT works** end to end | **Fork, real USDT** | A mock ERC-20 |

**The USDT row is the lesson for the whole table.** A standards-compliant mock
token returns a bool, so a suite built on mocks passes while the contract cannot
trade one of the largest debt assets on Aave. **Mocks that are better-behaved than
reality are how bugs survive testing.** For token behaviour, pool math and
protocol responses: fork tests against real contracts, no exceptions.

### Simulation — GUIDE 11

| | |
|---|---|
| **Assert** | Simulated outcome matches actual execution, on historical liquidations |
| **Oracle** | The real receipt |
| **Fake** | Asserting the simulation does not revert |

### Profit model and router — GUIDE 12

| | |
|---|---|
| **Assert** | Computed net reproduces realized net within a stated tolerance across ≥100 historical liquidations, flash fee and impact included |
| **Oracle** | Receipts |
| **Fake** | Asserting the router returns *a* route, or that net is positive |

Set the tolerance **before** the run. A tolerance chosen after seeing the spread
is a description of your error, not a bound on it.

### Candidate selection, band, bid model — GUIDE 12 §4b, §4f, §6

| Assert | The fake |
|---|---|
| A non-liquidatable position with excellent net-per-gas **never enters the candidate set** | Only feeding liquidatable positions |
| Batch size falls when `p` falls | Asserting batching happens at all |
| A simulated liquidity collapse shrinks `max_size` on the **next block** | Testing the band with static pool state |
| A self-win does **not** move the `F` estimator | Never feeding the estimator your own address |
| Bid is `argmax`, not the cap | Asserting the bid is below net |

Each left column is a **negative** or **adversarial** assertion. An agent writes
positive ones by default; these are the ones that have to be asked for by name.

---

## 4. The mutation list — the meta-test

The only mechanical defence against pass-shaped tests: **break the code
deliberately and confirm the suite goes red.** A suite that stays green against
these is decorative regardless of its coverage number.

Run this as a checklist, not a tool. The specific mutations matter more than the
count.

| # | Mutation | Must be caught by |
|---|---|---|
| 1 | Flip one rounding direction in the fixed-point library | Differential fuzz (GUIDE 00/04) |
| 2 | Off-by-one in the liquidation-leg stride | Encoding proptest |
| 3 | Swap `token0`/`token1` in the pool registry | Registry boot assertion |
| 4 | Change one token's `decimals` by 1 | Registry boot assertion |
| 5 | Remove the `minProfit` check | Executor fork tests |
| 6 | Remove one callback's `msg.sender` check | Callback auth tests |
| 7 | Invert the HF comparison in `_isLiquidatable` | Guard tests + recall |
| 8 | Drop one field from an undo record | Reorg round-trip |
| 9 | Make `safeApprove` skip the zeroing write | USDT fork test |
| 10 | Let a non-liquidatable candidate through the gate | Selection negative test |
| 11 | Use `address(this).balance` for the bid | Donation test |
| 12 | Return the cap instead of the `argmax` from the bid model | Bid model test |
| 13 | Point the miss classifier at our own position state instead of a fork call | Review — **no test catches this**; it is the one item here that is a design check, and it is on the list because it invalidates the recall gate silently |
| 14 | Drop the `LT` weight from the V4 target-HF solve (`R = (t·D − C)/(t − (1+b))`) | Differential fuzz against `liquidationCall` on a fork: repay amount must match the contract's to the wei (GUIDE 04 Step 4) |
| 15 | Swap the round-trip property to `div_up(mul_down(x,y),y) >= x` | The proptest itself must **fail** on `x = 1, y = 1`; a CI job runs the mutated property and asserts red (GUIDE 00 Step 2) |
| 16 | Widen one `MarketRow` field to `U256` | `const` size assert (GUIDE 02 Step 3) |
| 17 | Use a flat `× 1.125` headroom for a 3-block inclusion span | Fee-field test: bundle for `block + 3` must remain valid at `base_fee · 1.125³` (GUIDE 13 Step 4b) |
| 18 | Change the D40 comparator from `≤` to `<` | Gate test with `n = 50, misses = 15` must pass (GUIDE 05 Step 3a) |

**Every mutation on this list corresponds to a real decision recorded in
`STATE.md`.** If the suite does not catch one, the decision it encodes is not
actually enforced — it is a comment.

Do this once at the end of each guide, not continuously. It is a design review
with a compiler.

---

## 5. When each tier runs

| Tier | Trigger | Contents |
|---|---|---|
| **Unit + invariant** | every commit | Fixed-point proptests, store round-trips, encoding proptest |
| **Differential** | every commit, small N | Health vs chain, quotes vs revm |
| **Fork matrix** | every PR | Adapters × callbacks × providers, real tokens including USDT |
| **Full replay** | **before every deploy**, gated in CI | Recall over the available archive, named fixtures |
| **Coverage matrix** | continuously, forward | The recall gate itself — GUIDE 05 Step 3. Not a test |
| **Mutation checklist** | end of each guide | §4 |
| **Drift detector** | continuously, in production | Not a test — the same comparison running live |

The drift detector deserves the distinction. It is the same assertion as the
differential test, running forever against real state, and it is the only one that
catches a protocol changing underneath you.

---

## 6. What cannot be tested

Three things in this system are **evidence, not tests**, and an agent under
pressure will be tempted to substitute a synthetic version that passes in
seconds.

- **The 7-day drift week** cannot be simulated. A synthetic drift test is a
  differential test, which you already have; it is not the drift gate.
- **Recall coverage** cannot be run against generated events, and cannot be
  accelerated. The value is entirely in the liquidations being real and in the
  matrix filling at the rate the market fills it. GUIDE 05 Step 0's lite
  validation is a smoke test on a partial universe — it finds decoder bugs, and
  it clears nothing.
- **Shadow mode** cannot be shortened. Two weeks of it is two weeks.

A passing test is never evidence for a gate. `ORCHESTRATOR.md` §4 holds.

---

## 7. Review checklist

For any test an agent produces:

- [ ] Every assertion names its oracle, and the oracle is one of the four
- [ ] No expected value was produced by the code under test
- [ ] No mock stands in for token behaviour, pool math or a protocol response
- [ ] At least one assertion is negative — proving a guard fires, not that the
      happy path works
- [ ] Generators vary every dimension independently, including minimums
- [ ] Fuzz generators concentrate on the boundary, not the average case
- [ ] Tolerances were set before the run, not after
- [ ] The relevant mutation from §4 has been applied and the test went red

The last one is the only one that cannot be satisfied by writing convincing prose.
When in doubt about a test, break the code and see.
