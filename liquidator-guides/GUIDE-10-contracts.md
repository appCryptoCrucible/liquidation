# GUIDE 10 — Executor Contracts

| | |
|---|---|
| **Directory** | `contracts/` (Foundry) |
| **Prerequisites** | GUIDE 04 (V3 + V4 read-back semantics), 07 Step 1 (callback shapes) |
| **Work packages** | **10A** Executor + callbacks + on-chain adapters for Aave V3, Aave V4, **Morpho Blue** (D48); **10B** `liq-plan` encoder + round-trip proptest; **10C** fork matrix, invariants, gas snapshots; **H3** review + deploy. A later protocol with a new liquidation ABI is a **`10R-n` redeploy WP** repeating all three plus H3 |
| **Est. effort** | 2–3 weeks including review |
| **Blocks** | 11, 13 |

## Objective

The on-chain half. A gated entrypoint that borrows via flash loan, repays,
seizes, swaps and settles atomically — with no capital held between transactions.

**Flashloan-only operation changes the security posture for the better.** The
executor never holds inventory, so a compromised operator key cannot drain a
treasury that does not exist. What remains to protect: gas balance, and the
ability to make arbitrary calls. Both are addressed below, and both are still
capable of losing more than your expected profit.

## Build vs. buy

100% build. Foundry is the toolchain, not the code. Do not fork a public
liquidation bot contract — most carry standing approvals and an ungated call
path.

---

## Step 1 — Layout

```
contracts/src/
├── Executor.sol                  # single entrypoint, operator-gated
├── callbacks/                    # one per CallbackShape (GUIDE 07)
│   ├── AaveCallback.sol          # executeOperation
│   ├── UniV3Callback.sol         # uniswapV3FlashCallback
│   ├── UniV4Callback.sol         # unlockCallback + take/settle
│   ├── MorphoCallback.sol        # onMorphoFlashLoan
│   └── SkyDssCallback.sol        # ERC-3156 onFlashLoan
│   └── (BalancerCallback — OUT OF SCOPE / D08)
├── adapters/                     # one per protocol family (GUIDE 15)
│   ├── AaveV4Adapter.sol   AaveV3Adapter.sol
│   ├── CompoundV3Adapter.sol   CompoundV2Adapter.sol
│   ├── MorphoAdapter.sol   EulerV2Adapter.sol
│   └── TroveAdapter.sol
├── swap/
│   ├── SplitSwap.sol             # N legs; EXACT_OUT repay first, rest to WETH
│   ├── UniV3PoolDirect.sol       # transfer-in-callback, no approval
│   └── RouterLeg.sol             # Curve / aggregators, ALLOWLISTED targets only
│   # No UniV4Swap: hooks make V4 quoting pool-specific, so it is excluded
│   # as a routing venue (GUIDE 12 Step 3). V4 stays a flashloan source.
└── lib/{Errors.sol, Calldata.sol, Reentrancy.sol}
```

The `callbacks × adapters` matrix is why both axes must be data-driven: five
callback shapes times a dozen protocol families is sixty combinations, and you
cannot write sixty contracts. **One executor, dispatching on an encoded plan.**

## Step 2 — The unified entrypoint

```solidity
function execute(bytes calldata plan) external onlyOperator {
    // plan encodes: flash provider, asset, amount, protocol adapter id,
    // liquidation params, swap route, minProfit.
    Plan memory p = Calldata.decode(plan);
    uint256 wethBefore = IWETH(WETH).balanceOf(address(this));
    _initiate(p);                       // dispatch to the provider's entry call
    // control returns here only after the callback has fully settled,
    // which means the flash is already repaid and what remains is profit
    uint256 net  = (IWETH(WETH).balanceOf(address(this)) - wethBefore) - p.gasCostWei;
    uint256 bid  = (net * p.bidBps) / 10_000;
    if (net - bid < p.minProfit) revert Unprofitable(net - bid, p.minProfit);
    _payCoinbase(bid);                  // unwrap, call{value:}
}
```

Each callback contract does the same four things in its own ABI shape:

```solidity
function _onFlashFunds(Plan memory p, uint256 borrowed, uint256 fee) internal {
    _guard(p);                          // recheck health on-chain
    _liquidateLegs(p);                  // N legs, try/catch, adapter dispatch
    _swap(p);                           // EXACT_OUT -> debt asset (repay),
                                        // everything else -> WETH (profit)
    _settle(p, borrowed + fee);                         // provider-specific
    // WETH remainder is profit; the bid and sweep happen in execute()
}
```

**Uniswap V4 is the odd one out** and needs care: there is no "borrowed amount"
handed to you. You `take()` inside `unlockCallback`, and you must `settle()` such
that every currency delta nets to zero before `unlock()` returns — otherwise the
PoolManager reverts. Write this callback first; it is the one that will surprise
whoever implements it, and it is also your cheapest source.

## Step 3 — Guard, and revert freely

```solidity
function _guard(Plan memory p) internal view {
    uint256 hf = _health(p);
    if (hf >= 1e18) revert NotLiquidatable(hf);
}
```

On mainnet you submit bundles (MEV-Share and direct to builders), where a
reverting bundle is dropped and costs nothing. `revert` is therefore the correct
policy — freely and early. The "early-return instead of revert" pattern is for
priority-ordered L2 sequencers and does not apply here.

Keep the check on-chain regardless. Your off-chain state can be one block stale,
and this guard is what converts that into a dropped bundle instead of a loss.

## Step 4 — Aave V4 specifics

The V4 adapter must handle the dynamic close factor. `liquidationCall` takes the
repay amount you computed off-chain, but the protocol clamps it to its own
target-HF-derived maximum. Two consequences:

1. **Do not assume you repaid what you asked for.** Read back actual repaid and
   seized amounts from return values or events, and size the swap from those.
   Under flashloan-only funding this is sharper than it was with inventory: if
   you borrowed for the requested amount and the protocol accepted less, you are
   holding surplus debt token you must still repay with fee. Size the flash loan
   from a conservative repay estimate, or handle the surplus explicitly.
2. **The bonus depends on HF at execution**, which can differ from your quote if
   anything landed ahead of you. `minProfit` is the protection — set it from the
   quoted economics with a tolerance band, not the best case.

## Step 4b — Reference implementation

`Executor.sol` in this directory is the starting point: a working single-entry
executor with five flash providers (id 4 = Sky DSS Flash / ERC-3156),
transient-storage callback authentication, packed-calldata decode and the
conditional sweep. Multi-source cascade is sequential sibling groups — no nested
flash callbacks (PLAN-ENCODING §1b′, D32/D44). `PLAN-ENCODING.md` is the wire
format both it and the Rust encoder must agree on.

It is not audited and the adapter dispatch is deliberately stubbed beyond Aave
V3 — wire each protocol's real view and liquidation functions as GUIDE 15 adds
them, against the deployed contracts rather than documentation.

## Step 4c — Profit custody: one asset, and a sweep that is pure arithmetic

**Profit is WETH and only WETH** (GUIDE 12 §4d). That single fact removes most of
what used to make custody a design question.

It began as a trade-off across several assets: profit accumulated as an assortment
of debt tokens, so leaving it in the executor was standing exposure in each, and
the sweep threshold balanced that against ~30k of transfer gas per win.

ETH-only profit (D29) removed the *multi-asset* half of that. **It did not remove
the risk budget** — and D13 put it back at the centre. With abandonment manual and
no automatic capital control, the executor's standing balance *is* the entire
at-risk amount, so `k` sets how much is exposed between sweeps. Treat it as a risk
number that happens to have a gas floor, not a gas number.

The residual is whatever fraction of net you keep — small by construction at a
high `bidBps` — in one asset, in a contract that holds nothing else. So the
question collapses to arithmetic: **is this balance worth ~9k gas of transfer?**

```
sweep when  balance_wei > k × transfer_gas_cost_wei      // k ≈ 20–50
```

The flag still comes from off-chain and still costs zero extra gas when unset —
the Rust side sees every fill and knows the balance without asking the chain, so
the contract does no storage read for the decision. What changed is that the
threshold is now derivable rather than judged, and it moves with gas like
everything else (D12).

**What makes this safe is not the threshold — it is that the operator cannot
redirect funds.** `PROFIT_SINK` is immutable. `OPERATOR` can only call
`execute()`. There is no rescue function, no arbitrary call, no setter. A
compromised hot key can waste gas and nothing else. `sweep()` is deliberately
permissionless for the same reason: the destination is fixed, so letting anyone
push funds out is a backstop against an operator that stops setting the flag.

If you want a changeable sink, put it behind a cold multisig **and a timelock**.
An owner-settable sink with no delay recreates the exact risk the immutable
design removes.

**The bid is not custody.** It leaves in the same transaction that earns it, via
`block.coinbase`, and never rests in the contract — which is what lets the
executor stay a proxy rather than becoming the ETH float a priority-fee design
would have required.

## Step 4d — Wallet-funded bidding: built, unused, and that is deliberate

`execute` is `payable`. When `msg.value` is zero — the default, and what every
plan should do — the bid is unwrapped from the WETH the liquidation earned. When
`msg.value` is non-zero it becomes a **ceiling**: the bid is still computed from
realized net, `min(bid, msg.value)` is paid, and the remainder is refunded to the
operator.

**Why build something you do not intend to use.** The executor is immutable by
design — no setters, no rescue, no upgrade path — and that is what makes a
compromised operator key harmless. It also makes changing your mind expensive: a
branch now, a redeploy and migration in month six. Cheap optionality against an
irreversible decision.

**The case it exists for**, if the data ever justifies it: stablecoin collateral
against stablecoin debt, where converting profit to ETH costs real basis points
and the natural exit is a few. Funding the bid from a wallet balance would let
profit stay in the debt asset for those plans. That path costs working capital, an
operational dependency (run dry and you cannot bid, so you cannot win), and it
gives up realized-net bidding — so measure the stable-against-stable share from
the replay archive before enabling anything.

**The trap the code avoids:** never read `address(this).balance` for the bid.
`receive()` is open — it has to be, for `WETH.withdraw` — so a donation would be
indistinguishable from the bid budget and would be bid away. `msg.value`
explicitly, always.

## Step 5 — Approvals: exact, scoped, and mostly unnecessary

Most of this contract needs no approvals at all, which is both cheaper and
safer than approving carefully:

| Path | Settlement | Approval |
|---|---|---|
| Uniswap V3 flash | `transfer` to the pool in the callback | **none** |
| Uniswap V4 flash | `sync` → `transfer` → `settle` | **none** |
| Sky DSS Flash | `approve` Flash for amount+fee (pull) | ERC-3156 success magic |
| ~~Balancer flash~~ | — | **out of scope (D08/D09)** |
| Aave flash | pool pulls | exact amount + premium |
| Morpho flash | Morpho pulls | exact amount |
| Aave `liquidationCall` | pool pulls | exact repay amount |
| Swap via router | router pulls | exact amount in |

**Exact approvals self-zero.** `approve(spender, exact)` followed by the
spender's `transferFrom(exact)` leaves the allowance at zero with no second
`SSTORE`. Approving a round number "to be safe" is what creates a standing
allowance; approving the exact amount does not.

Prefer pool-direct swaps with a swap callback over a router where the route
allows it — same transfer-in-callback pattern, no approval, less gas.

**Never a standing infinite approval.** With accumulation (Step 4c) the contract
holds up to your sweep threshold between liquidations, and a standing approval
hands that balance to any exploit in the approved spender.

## Step 5b — Split swap execution

The router hands down N legs with allocations (GUIDE 12). The contract executes
them and nothing more — it does not re-optimize, and it does not second-guess the
split.

Two details that are easy to get wrong:

**The last leg takes the remainder.** Ignore the final leg's encoded amount and
pass whatever is left. Allocations then sum exactly to the seized amount however
the off-chain solver rounded. Trusting an encoded sum strands dust in the
executor or reverts a leg on balance — and since seized amounts are themselves
protocol-clamped (Step 4), the input is not known precisely until execution.

**The swap callback needs its own authentication.** `uniswapV3SwapCallback` has a
different selector from `uniswapV3FlashCallback` and is a separate attack
surface. Verify both that a swap is in progress (transient flag) *and* that
`msg.sender` is the canonical pool for `(token0, token1, fee)` by CREATE2 against
the factory. Storing "the pool we called" does not generalize to N legs; address
derivation does.

Pool-direct V3 legs settle by transfer inside that callback — **no approval on
that path at all**, which is both cheaper and a smaller surface than routing
through a router contract.

## Step 6 — No arbitrary call

`AggregatorCall.sol` validates its target against an allowlist. An executor with
a reachable `call(target, data)` is a single-key-compromise total loss regardless
of how well-gated it looks. This is the one item on the list that survives the
removal of inventory, and it is the reason this guide still warrants independent
review.

## Step 7 — Gas

Under flashloan-only, gas and the flash fee are the two unavoidable costs on
every liquidation, and both come straight out of your bid (GUIDE 12).

1. **Choose the cheapest viable flash source** — that is GUIDE 07's job, but the
   contract must make each provider's overhead measurable. Emit or return gas
   used per leg during fork tests.
2. Immutables for routers, pools and protocol addresses; no `SLOAD` in the hot
   path.
3. Custom errors, not revert strings.
4. Packed calldata rather than ABI encoding — ~3,200 gas on a 151-byte header
   versus 352 padded bytes. Worth doing, but **do not over-invest here**:
   EIP-7623's calldata floor applies only to transactions that are calldata-heavy
   relative to their computation, and a liquidation's 350–600k gas of EVM work
   keeps it on the standard 4/16 schedule. Packing is ~1% of transaction gas.
   Source selection (GUIDE 07) moves basis points; this moves a fraction of a
   percent.
5. Uniswap V4's `take`/`settle` in the same unlock is cheaper than a V3 flash
   callback. Let the numbers, not intuition, drive selection.

Measure with `forge snapshot`, gate regressions in CI at 2%.

## Step 8 — Test suite

**Fork tests** (`test/fork/`) — the matrix that matters: every protocol adapter ×
every callback shape, pinned to real blocks where liquidations occurred. Generate
these combinatorially; do not hand-write sixty tests.

**Invariant tests** (`test/invariant/`) — the security boundary:

- Executor token balance is zero after every successful call, **for every token
  except accumulated profit awaiting sweep**
- No allowance to any address is nonzero after any call
- `netProfit >= minProfit` or the call reverted
- No path reaches an external `call` with an un-allowlisted target
- **Every flash loan is repaid exactly** — no path exits with an outstanding
  delta (this is what Uniswap V4 enforces natively and every other provider does
  not)

**Differential tests** (`test/differential/`) — each adapter's `_health()` agrees
with the protocol's own view function under fuzzed state. The on-chain half of
the drift detector.

## Step 9 — Review before mainnet

Focused independent review of: who can call what, what approvals can persist,
what external calls are reachable, what a compromised operator key can do, and
whether every flash callback correctly validates its caller. That last one is
specific to this design — **a callback that does not verify `msg.sender` is the
expected provider is a free-money function for anyone who finds it.**

---

## Acceptance criteria

- [ ] All five flash providers implemented and fork-tested; Sky DSS rejects non-DAI
- [ ] **Every callback validates `msg.sender` against the expected provider** and
      validates that the flash was initiated by this contract
- [ ] Uniswap V4 `unlockCallback` settles all deltas; a test proves a deliberate
      under-settle reverts
- [ ] Combinatorial fork test matrix (adapters × callbacks) generated, not
      hand-written, and passing
- [ ] All invariant tests pass at ≥ 10k runs, including zero-allowance and
      exact-flash-repayment
- [ ] **Every token movement goes through `SafeTransfer`.** No raw
      `IERC20(x).transfer(…)` or `.approve(…)` anywhere — grep for it in CI.
      USDT and others return no data, and the bool-returning interface reverts
      in the ABI decoder, which would silently exclude one of the largest debt
      assets on Aave.
- [ ] **Fork tests run the full matrix against USDT specifically**, as debt
      asset and as swap input, across every flash provider. A suite that only
      exercises DAI and WETH passes while the contract cannot trade USDT at all.
- [ ] Non-zero → non-zero approve is tested: set an allowance, do not consume it
      fully, approve again, and confirm no revert
- [ ] Router allowance is zero after every swap leg, asserted in the test
- [ ] **Profit is WETH and only WETH** after `execute` — a test asserts no debt
      token remains, including the exact-output repay leg leaving zero dust
- [ ] Coinbase bid paid with `call{value:}`, not `.transfer()`; a test with a
      contract fee recipient that needs >2300 gas still succeeds
- [ ] Bid is computed from **realized** net: a test where the swap under-delivers
      confirms the bid shrinks and `minProfit` still clears
- [ ] `gross < gasCostWei` reverts (underflow is the intended failure)
- [ ] Debt-asset-is-WETH case covered: repay and profit share a balance, and the
      post-`_initiate` measurement still isolates profit correctly
- [ ] **Multi-group plan fork-tested**: two flash groups, different providers,
      different debt assets, one profit guard — and a test where group 1's legs
      all fail while group 2's succeed
- [ ] Group re-walk in the callbacks lands on the right group: a three-group plan
      asserts each callback saw its own `debtAsset`
- [ ] Wallet-funded bid: `msg.value` capped, remainder refunded, and a test
      donating ETH via `receive()` proves the donation is **not** bid away
- [ ] Fee-on-transfer safety: seized and swapped amounts are read from measured
      balance deltas, never from the requested amount — proven with a mock
      fee-on-transfer token
- [ ] Differential `_health()` tests pass, 100k cases biased to HF ≈ 1
- [ ] Gas snapshot per provider recorded; V4 measurably cheaper than V3 flash
- [ ] V4 adapter reads back actual repaid/seized rather than assuming
- [ ] Surplus-borrow case handled: flash more than the protocol accepts, and the
      transaction still settles profitably or reverts cleanly
- [ ] No reachable arbitrary external call, proven by exhaustive path review
- [ ] `PROFIT_SINK` is immutable (or cold-multisig + timelock); no operator-
      reachable setter exists
- [ ] No rescue function, no arbitrary external call, no operator-settable state
- [ ] `sweep()` is permissionless and can only send to `PROFIT_SINK`
- [ ] Sweep threshold documented as a risk budget with an owner, not a constant
- [ ] Plan encoding round-trip proptest (Rust encoder ⇄ Solidity decoder) passes
      over randomized plans, not a single fixture — see `PLAN-ENCODING.md` §4
- [ ] Transient-storage callback guard verified: an external call to every
      callback function from an unarmed context reverts with `BadCallback`
- [ ] **`uniswapV3SwapCallback` verifies the caller by CREATE2 pool derivation**,
      not by a stored address — a test proves a non-canonical caller reverts even
      mid-swap
- [ ] Split swap executes N legs and the last absorbs the remainder; a fixture
      with deliberately under-summing allocations still settles exactly
- [ ] Router legs reject any target outside the immutable allowlist
- [ ] Pool-direct V3 legs leave zero allowance (they set none)
- [ ] Independent review completed
- [ ] Deployed to mainnet; addresses recorded in config

## Failure modes

| Symptom | Cause |
|---|---|
| Anyone can drain the executor | Callback doesn't validate `msg.sender` or initiator. **The characteristic flashloan-bot vulnerability.** |
| Uniswap V4 calls always revert | Deltas not netted to zero before `unlock()` returns |
| Occasional reverts on volatile blocks | Swap sized from requested repay rather than actual (V4 clamping) |
| Profitable in sim, unprofitable on chain | `minProfit` from best-case bonus; or flash fee omitted from the check |
| Losing bids | Gas not golfed, or an expensive flash source chosen by default |
| Combinatorial gaps | Fork tests hand-written, so some adapter × callback pairs never ran |

## Handoff

GUIDE 11 simulates against these deployed contracts. Deploy to mainnet before
starting it so simulation runs against real bytecode at the real address — gas
profiles differ from a test deployment, and gas is now a first-order term.
