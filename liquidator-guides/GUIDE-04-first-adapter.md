# GUIDE 04 — First Adapter: Aave V4 (+ V3) & the Drift Detector

| | |
|---|---|
| **Crate** | `liq-adapters/aave-v4`, `liq-adapters/aave-v3` |
| **Prerequisites** | GUIDE 01, 02, 03, **06 Steps 2–3** (a real `PriceVector` fed from aggregator logs — WP 06A-1), the event-coverage audit (WP 03C) |
| **Work packages** | **04A** Aave V4; **04B** Aave V3; **04C** start the drift observation (the Correctness gate's clock). The 100k differential run is **05A**'s harness; the 30-day `HealthWrong = 0` and the drift week are **04C**'s evidence, not this WP's |
| **Est. effort** | 2–3 weeks |
| **Blocks** | 05, 06, 08, 10, 12 |

## Objective

Reproduce Aave V4's health and liquidation math exactly, prove it with the drift
detector, then do the same for V3. This is the **Stage 0 gate**: nothing
downstream is worth building until local health factors agree with the chain.

## Build vs. buy

100% build. Use `alloy::sol!` against the verified ABIs; everything else is
reimplementation. Read the V4 source directly — documentation lags contracts, and
you need rounding directions that no document specifies.

---

## Step 1 — Read the contracts, not the docs

Clone `aave/aave-v4` and read, in this order:

1. `src/hub/HubStorage.sol` — asset accounting, `assetId`, `drawnShares`,
   `liquidity`, index/rate fields
2. `src/hub/Hub.sol` — `draw()`, `restore()`, `accrue()`, `updateDrawnRate()`
3. `src/spoke/SpokeStorage.sol` — `Reserve` struct, `_userPositions`,
   `_reserveCount`
4. `src/spoke/Spoke.sol` — supply/borrow/withdraw/repay, **`liquidationCall`**,
   and the health computation it calls
5. Whatever library computes health and the bonus curve

Record, for every arithmetic step, the **rounding direction**. `drawnShares` is
rounded up on draw (protocol-favouring) and down on restore. Your reimplementation
must match each one. This is the detail that decides HF at 0.9999.

Do the same for V3 at **3.5+** — that release changed rounding methods and
internal scaled accounting, so older references will disagree with the chain.

## Step 2 — The V4 data model

```
Hub  ──asset[assetId]──►  { drawnIndex, addExRate, liquidity, drawnRate, … }
  ▲
  │ hub_ref
Spoke ──reserve[i]──────►  { underlying, hub, assetId, decimals,
  │                          collateralRisk, flag, dynamicConfigKey }
  │
  └── _userPositions[user][reserve] ──► { suppliedShares, drawnShares, premium }
```

Map onto GUIDE 02's store:

- `PositionKey = (ProtocolId::AaveV4, MarketId(spoke), user)`
- One store slot per `(spoke, reserve)`; `supply[slot][pos] = suppliedShares`,
  `debt[slot][pos] = drawnShares`
- `MarketRow.supply_index = addExRate`, `MarketRow.debt_index = drawnIndex`,
  denormalized from the hub, fanned out via `hub_ref` on hub accrual
- `PositionExtra = AaveV4Extra { risk_premium, premium_accrued, premium_last_update }`

## Step 3 — `health()`

```
collateral_value = Σ over set bits in config:
    suppliedShares[slot] · addExRate[slot] · price[asset] · collateralRisk[slot]
debt_value = Σ:
    drawnShares[slot] · drawnIndex[slot] · price[asset]  + accrued_premium
hf = collateral_value / debt_value          (normalized: 1.0 is the boundary)
```

Requirements:

- Iterate `AssetMask` set bits first; never loop all reserves
- No allocation, no branching on protocol
- Apply the *same* rounding directions the contract does at each step
- Include the risk premium in debt. Omitting it makes every V4 position look
  healthier than it is, and the error grows with time since last touch — so it
  passes a spot check and fails in production

## Step 4 — `quote()` and the V4 bonus curve

This is what makes V4 different, and it is where the money is.

**Dynamic close factor.** V4 caps repayment at the amount that restores the
position to the Spoke's **Target Health Factor**. Solve:

```
C   = Σ weighted collateral value   (each reserve: value · collateralRisk)   — the HF numerator
D   = debt value incl. accrued premium                                        — the HF denominator
b   = bonus(hf₀)      evaluated at the position's CURRENT hf, before the call
LT  = collateralRisk of the reserve being seized (its liquidation-threshold weight)

Repaying R of debt value removes R from D and seizes collateral worth R·(1+b),
which removes R·(1+b)·LT from the *weighted* numerator:

    (C − R·(1+b)·LT) / (D − R) = target_hf
    ⇒  R = (target_hf·D − C) / (target_hf − (1+b)·LT)

R_max = min(R, debt of the repaid reserve, collateral of the seized reserve / (1+b))
```

Both sides are in the oracle's base currency, so there is no `price_ratio` in
this equation — the conversion happened when `C` and `D` were built. What must
be there is `LT`: seized collateral leaves the numerator at its **weighted**
value, not its full value. Dropping `LT` makes the numerator fall too fast, the
solve reaches `target_hf` at a smaller `R`, and every V4 quote under-liquidates.

The bonus is **not** a fixed point in `R`. The contract evaluates the bonus
curve at the health factor the position has when `liquidationCall` executes,
which is `hf₀`, not the post-liquidation health. Confirm this against the
liquidation library in Step 1 — if the deployed code does evaluate the curve on
post-liquidation health, then and only then is this a fixed point, and two
Newton steps from the closed form above converge. Do not build the iteration
speculatively.

The denominator `target_hf − (1+b)·LT` is positive for every sane parameter set
(`target_hf ≥ 1`, `(1+b)·LT < 1`), and `R > 0` exactly when `hf₀ < target_hf`.
Assert both in debug builds; a non-positive denominator means the parameters
were read wrong.

Then apply the **dust rule**: if `D − R` or the remaining collateral would fall
below the spoke's dust floor, raise `R` to clear the position entirely.

**Variable bonus.** Piecewise linear in HF, from the three governance parameters:

```
bonus(hf) = max_bonus                              , hf <= hf_for_max_bonus
          = interpolate(bonus_factor, max_bonus)   , hf_for_max_bonus < hf < 1
          = n/a                                    , hf >= 1
```

Populate `Quote.curve = BonusCurve::HealthLinear { .. }` with the live spoke
parameters — **do not evaluate it to a scalar**. GUIDE 12 integrates this curve
against a competitor-arrival model to decide when to fire. Collapsing it here
throws away the V4 edge.

## Step 5 — `liquidation_price()` and `time_to_cross()`

`liquidation_price(asset)`: solve `hf(px) = 1` for that asset's price holding
others fixed. With `s` the weighted collateral quantity and `d` the debt
quantity of that asset in the position:

```
hf(px) = (C_other + s·LT·px) / (D_other + d·px) = 1
   ⇒   px* = (D_other − C_other) / (s·LT − d)
```

`d = 0` for a pure collateral asset and the expression is linear; when the same
asset is both collateral and debt (common on stables), it is the rational form
above and `s·LT − d` may be ≤ 0 — the position then **never** crosses on that
asset's price alone and must not be registered on it. Verify the round-trip
property from GUIDE 01's conformance suite (set the returned price, recompute,
assert HF within 1 ulp of 1.0).

`time_to_cross()`: **must integrate base rate and risk premium.** V3's version is
the pure index ratio; V4's carries the per-position premium term. Getting this
wrong means the time-cross heap (GUIDE 08) fires late on exactly the
uncontested opportunities it exists to catch.

## Step 6 — `apply_log()` with undo records

Cover every event from GUIDE 03 Step 3's audit. For V4 specifically, do not miss:

- Hub-level accrual → `DirtySet::MarketAccrual` fanned out via `hub_ref`
- Spoke config / liquidation-config changes → `DirtySet::MarketReprice`
- Risk premium updates → `DirtySet::Positions` (per-position, not market-wide)
- Position-manager-mediated actions (Giver/Taker/SignatureGateway) — these move
  positions without a direct user transaction
- Collateral token transfers

Every mutation pushes an `UndoOp`. The GUIDE 02 property test covers this;
re-run it with real V4 event sequences from a fixture block range.

## Step 7 — Wire up the drift detector and run it

This is the gate. Configure the detector (GUIDE 02, Step 8) to sample stratified
across bands and compare `health()` against `health_probe()`.

Run continuously for **one week minimum** against live mainnet, read-only.

## Step 8 — Then do V3

V3 is strictly easier: static bonus, fixed close factor, scaled balances,
`liquidityIndex`. It is a good confirmation that the trait generalizes, and V3
still holds meaningful TVL. Reuse the fork-test harness wholesale.

Handle explicitly: e-mode categories, isolation mode + debt ceilings, siloed
borrowing, grace periods, and 3.3+ deficit accounting.

---

## Acceptance criteria — the Stage 0 gate

- [ ] **Drift detector shows < 1 bp mismatch across ≥ 10,000 sampled positions
      over 7 consecutive days**, for V4 and V3 separately
- [ ] Zero `HealthWrong` classifications against every actual liquidation in a
      30-day historical window
- [ ] `liquidation_price()` round-trip within 1 ulp for 10k fuzzed positions
- [ ] Differential fuzz: `health()` vs. the protocol's on-chain view function
      over randomized state, 100k cases, zero mismatches (Foundry + a Rust FFI
      harness, or replay via revm)
- [ ] `quote().max_repay` applied on a fork restores HF to within 1 bp of the
      spoke's target
- [ ] `BonusCurve` evaluated at 5 sampled HFs matches the contract's bonus within
      1 bp, verified on a fork
- [ ] Conformance suite from GUIDE 01 passes for both adapters
- [ ] `health()` allocation-free; p99 < 2 µs per position
- [ ] Event coverage document complete and checked in

## Failure modes

| Symptom | Cause |
|---|---|
| Mismatch grows with time since position touched | Risk premium not accrued (V4) |
| Mismatch only at HF near 1 | Wrong rounding direction somewhere in the chain |
| Mismatch only for some positions | Missed event path — cross-reference the coverage audit |
| Mismatch appears suddenly, protocol-wide | Proxy upgraded; halt and re-read source (GUIDE 14) |
| Correct HF but wrong repay amount | Target-HF solve ignores the bonus feedback term, or dust rule missing |
| V4 liquidations under-earn vs. competitors | Bonus curve collapsed to a scalar; see GUIDE 12 |

## Handoff

GUIDE 05 builds the replay harness that turns the one-week drift run into a
repeatable regression test. **Do not skip ahead to the engine.** An engine built
on an adapter that has not passed the drift gate produces confident, wrong,
expensive decisions.
