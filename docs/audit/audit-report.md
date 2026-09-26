# Smart Contract Security Audit Report

**Protocol:** Liquidation Executor (flash-loan liquidation searcher)  
**Repository:** liquidation bot  
**Foundry root:** `contracts/`  
**Auditor:** Tucker Sossaman  
**Date:** 26 September 2026  
**Chain:** Ethereum mainnet (planned deployment; **not deployed** at audit time)  
**Scope version:** `contracts/src/Executor.sol` + `contracts/src/lib/{PlanDecoder,SafeTransfer,Interfaces,MainnetVenues}.sol`

---

## Executive summary

This engagement audited an immutable flash-loan liquidation executor. Funds are in transit during `execute`, with optional standing balances and permissionless `sweep` to an immutable `PROFIT_SINK`.

**Accepted Trust Assumptions:** None — zero-trust default. NatSpec statements that `OPERATOR` is “trusted” or cannot move funds freely are **claims**, not accepted assumptions. Compromised hot-key calibration applies: a single-transaction drain of contract balances with only the operator key is **Critical**.

| Severity | Shippable (main) | IDs |
|----------|------------------|-----|
| Critical | **3** | C-01, C-02, C-03 |
| High | **1** | H-01 |
| Medium (promoted) | **2** | M-01, M-02 |
| Appendix Low/Info | several | A-01… |

**Not a clean bill of health.** Compromised `OPERATOR` can drain standing inventory in one transaction via (1) allowlisted router calldata, (2) malicious plan `flashSource`, and (3) malicious plan `market` / Compound `isCEther` ETH recipient. Standing WETH larger than that tx’s profit still reverts; **standing WETH up to realized profit is stealable (H-01)**.

**Tools:** `forge build` OK (solc 0.8.28, via_ir). Slither OK (33 in-scope Medium+ raw hits; triage below). `forge coverage` **failed** (stack-too-deep / Yul). PDF via pandoc attempted separately.

---

## Scope and context

### In scope

| File | Role |
|------|------|
| `contracts/src/Executor.sol` | Custody + flash + liquidate + swap + bid + sweep |
| `contracts/src/lib/PlanDecoder.sol` | Packed plan bounds walk |
| `contracts/src/lib/SafeTransfer.sol` | ERC-20 transfer/approve |
| `contracts/src/lib/Interfaces.sol` | External ABIs |
| `contracts/src/lib/MainnetVenues.sol` | UniV2/Sushi/Curve anchors |

### Out of scope

`test/`, `script/`, `src/differential/`, guide copy, Rust crates (residual risk except on-chain binding failures).

### Build environment

Client-supplied `foundry.toml`: solc **0.8.28**, optimizer **true**, `optimizer_runs` **200**, `via_ir` **true**, `evm_version` **cancun**. Not reconstructed. `SLOW_BUILD` set.

### Feature flags

`HAS_EXTERNAL_VALUE_DEP`, `HAS_MULTI_CONNECTOR`, `HAS_TRANSIENT`, `HAS_LENDING`, `HAS_ASSEMBLY`, `SLOW_BUILD`, `TIER_2C_ENABLED`, `INCLUDE_QUALITY_SCORECARD`.  
**False:** `HAS_PROXY`, `HAS_SIGNATURES`, `HAS_ORACLE`, `HAS_ZK`, `HAS_LIVE_CONTRACTS`, `HAS_MEV_TREASURY_VAULT` (operator+flash present but no CallPolicy / delayed withdrawal vault pattern).

### Accepted Trust Assumptions

| ID | Assumption |
|----|------------|
| — | **None — zero-trust default** |

### Module coverage matrix

| File | Capital role | Status |
|------|--------------|--------|
| Executor.sol | All value paths | reviewed — C-01, C-02, M-01 |
| PlanDecoder.sol | Plan integrity | reviewed — no shippable C/H |
| SafeTransfer.sol | Token movement | reviewed — A-01 |
| Interfaces.sol | ABI surface | reviewed |
| MainnetVenues.sol | Venue anchors | reviewed |

### Entrypoint ledger

| Entrypoint | Moves value | Outcome |
|------------|-------------|---------|
| `execute` | yes | C-01, C-02, M-01 |
| `executeOperation` | yes | auth OK if flashSource honest; C-02 if malicious source |
| `uniswapV3FlashCallback` | yes | C-02 |
| `unlockCallback` | yes | same flashSource trust model |
| `onMorphoFlashLoan` | yes | same |
| `onFlashLoan` | yes | same (+ initiator check) |
| `uniswapV3SwapCallback` | yes | CREATE2 + T_SWAPPING — OK |
| `sweep` | yes | destination fixed — OK |
| `receive` | ETH in | bid uses explicit accounting — OK |

---

## Methodology

1. Phase 0 custody/authority/attack-surface maps + protocol intake  
2. Step 0.5: forge build, Slither full/high, summarizer; coverage failed  
3. Step 1.5: code quality Pass A + hotspot map (`docs/audit/quality-*.md`)  
4. Tier 2: manual / economic / static / integration / token / connector waves  
5. Tier 2c: anomalous states, interaction matrix, premise scan (PoC-backed)  
6. Step 2.4: `fork-poc-manifest.json` **status: not_live** → unit PoCs submittable  
7. Red-team calibration + shippability (Immunefi VSCS)  
8. Tier 3: INV-XX derived; existing Foundry invariant suite present; coverage gap residual  
9. Quality Pass B deferred metrics where coverage unavailable  

**AI limits:** Specialist subagents were dispatched in parallel; Critical confirmations are orchestrator PoCs with `forge test` PASS. Residual risk lists items needing human follow-up.

**Coverage:** Unavailable — `forge coverage` failed (stack too deep; `--ir-minimum` also failed on `PlanDecode.t.sol`).

---

## Sign-off Checklist (CAL-01–CAL-14)

Per `known-misses-calibration.md` (not ad-hoc labels):

| CAL | Result | Notes |
|-----|--------|-------|
| CAL-01 | **PASS** | Entrypoint ledger complete in this report |
| CAL-02 | **PASS** | ≥Medium findings have broken_invariant / root_cause / mechanism_test |
| CAL-03 | **PASS** | Assembly is tload/tstore only; no assembly returndata loads |
| CAL-04 | **PASS** | LegFailed args from catch paths |
| CAL-05 | **N/A** | No same-T auction/epoch comparators |
| CAL-06 | **N/A** | HAS_SIGNATURES false |
| CAL-07 | **N/A** | No provider-signer; Template B operator drain = C-01/C-02/C-03 |
| CAL-08 | **PASS** | Depth on C-01/C-02; C-03 unit PoC confirmed |
| CAL-09 | **N/A** | No ZK |
| CAL-10 | **PASS** | Appendix F §R tables (Morpho/V2/bid) |
| CAL-11 | **PASS** | Appendix F §L (liquidator lifecycle) |
| CAL-12 | **PASS** | anomalous-states artifacts |
| CAL-13 | **PASS** | premise-scan + confirmed PoCs |
| CAL-14 | **PASS** | storage-access-graph.json EDGE-01…16 |

**CAL FAIL count:** 0

---

## Findings (shippable: yes)

### C-01 — Compromised OPERATOR drains standing non-WETH via allowlisted router

**Severity:** Critical  
**Immunefi impact:** Direct theft of any user funds, whether at-rest or in-motion, other than unclaimed yield  
**Location:** `Executor.sol`, `_swapLeg` S_ROUTER (approx. lines 1109–1120), `_swap` `L_TAKE_BALANCE` (1083–1085), profit/repay `_swap` from `execute`  
**Status:** Open — **Confirmed**  
**Interacts with:** C-02 (same privileged caller)  
**impact_class:** `fund_loss`  
**shippable:** yes  
**poc_kind:** unit · **poc_submittable:** true · **Environment:** uploaded · fork manifest `not_live`

**Broken invariant:** Standing non-WETH balances may leave the Executor only via `sweep` → immutable `PROFIT_SINK`.  
**Root cause:** `L_TAKE_BALANCE` approves the **full** ERC-20 balance to `ROUTER_A`/`ROUTER_B`, then executes **arbitrary** `target.call(data[20:])` with no recipient/path allowlist; the profit guard only constrains **WETH** `balanceOf` delta.  
**Mechanism test:** If router calldata were forced to return output to `address(this)` and approvals were delta-capped to this-tx proceeds only, the standing-balance steal would fail.

**Description:**  
NatSpec claims the operator cannot move funds to an address of its choosing and that standing balances are protected because WETH gross underflows if they fall. That protection applies to **WETH only**. A compromised operator key can encode a profit (or repay) router leg that pulls any standing ERC-20 (USDC, etc.) to an attacker-controlled recipient via SwapRouter02-style calldata. The transaction can still satisfy `minProfit` on WETH earned from the liquidation.

**Attack path:**
1. Executor holds standing non-WETH (donation, dust, partial router consume).  
2. Attacker uses compromised `OPERATOR` to call `execute` with a valid liquidation plan.  
3. Profit (or additional) swap leg: venue=`S_ROUTER`, `L_TAKE_BALANCE`, router=`ROUTER_A`, calldata = pull token to attacker.  
4. Liquidation WETH profit satisfies `minProfit`; standing non-WETH is gone.

**Impact:** Full drain of all non-WETH balances on the Executor in one transaction. WETH standing remains protected (see PoC).

**Proof of concept:**  
`contracts/test/audit/OperatorRouterDrain.t.sol`  
Command: `forge test --match-contract OperatorRouterDrainPoC -vvv`  
Results: `test_poc_operator_drains_standing_non_weth_via_router` **PASS**; `test_poc_standing_weth_theft_via_router_reverts` **PASS**.

**Recommendation:**  
- Do not use `L_TAKE_BALANCE` for router venue; or approve only `min(amount, thisTxDelta)`.  
- Restrict router calldata (Permit2/CallPolicy-style): force `recipient == address(this)` / `PROFIT_SINK` only.  
- Optionally track per-token accounting and forbid spending pre-execute balances except via `sweep`.

**References:** protocol-mev-treasury-vault INV-MEV-01; Immunefi SC-C2.

---

### C-02 — Compromised OPERATOR steals via malicious plan `flashSource` (flash path lacks CREATE2 / provider allowlist)

**Severity:** Critical  
**Immunefi impact:** Direct theft of any user funds, whether at-rest or in-motion, other than unclaimed yield  
**Location:** `Executor.sol`, `_arm` (363–365), `uniswapV3FlashCallback` (416–421); same `_checkCallback` model for Morpho/Aave/Sky/V4 unlock  
**Status:** Open — **Confirmed**  
**Interacts with:** C-01  
**impact_class:** `fund_loss`  
**shippable:** yes  
**poc_kind:** unit · **poc_submittable:** true · **Environment:** uploaded

**Broken invariant:** Flash repay is paid only to a provider that actually lent assets.  
**Root cause:** Callback authentication binds `msg.sender` to **plan-selected** `flashSource`, not to a CREATE2-verified UniV3 pool or an immutable provider allowlist. (UniV3 **swap** callback correctly uses CREATE2; **flash** does not.)  
**Mechanism test:** If UniV3 flash required CREATE2(factory, token0, token1, fee)==msg.sender (and other providers were immutable allowlisted), FakeFlash could not pass `_checkCallback`.

**Description:**  
During `execute`, the contract arms `T_EXPECTED_CALLER = fg.flashSource` from the plan. A compromised operator sets `flashSource` to a contract that invokes `uniswapV3FlashCallback` without transferring flash assets. `_core` can still liquidate using **standing** debt tokens; the Executor then transfers `flashAmount + fees` to the fake source.

**Attack path:**
1. Fund Executor with standing debt asset ≥ flash amount (or obtain via other means).  
2. Operator encodes `provider=P_UNIV3`, `flashSource=FakeFlash`.  
3. FakeFlash calls `uniswapV3FlashCallback` without lending.  
4. Liquidation + repay swaps run; Executor pays FakeFlash the “repay”.

**Impact:** Theft of standing debt and/or value converted from seized collateral intended for flash repay — one transaction with the operator key.

**Proof of concept:**  
`contracts/test/audit/FakeFlashSourceDrain.t.sol`  
Command: `forge test --match-contract FakeFlashSourceDrainPoC -vvv` — **PASS**.

**Recommendation:**  
- UniV3 flash: verify pool via CREATE2 like `uniswapV3SwapCallback` (token0/token1/fee from pool).  
- Aave/Morpho/Sky/V4: immutable allowlist of provider addresses (or EIP-712-bound provider set at construction).  
- Reject flashSource not on allowlist before `_arm`.

**References:** Uniswap V3 flash callback auth class; Immunefi SC-C2. Same class applies to Aave/Morpho/Sky DSS plan `flashSource` ([Connector Review](fdd69120-e61c-4d14-9bb4-14ad58555666)).

---

### C-03 — Compromised OPERATOR drains via plan-chosen liquidation `market` (unchecked approve / ETH send)

**Severity:** Critical  
**Immunefi impact:** Direct theft of any user funds, whether at-rest or in-motion, other than unclaimed yield  
**Location:** `Executor.sol`, `_liquidateAaveV3` approve+call (~550–559); all approve-based `_liquidate*`; Compound `isCEther` `{value:}` to `l.market` (~999–1003)  
**Status:** Open — **Confirmed**  
**Interacts with:** C-01, C-02  
**impact_class:** `fund_loss`  
**shippable:** yes  
**poc_kind:** unit · **poc_submittable:** true · **Environment:** uploaded

**Broken invariant:** Debt approvals / ETH liquidation payments only go to real protocol markets.  
**Root cause:** `l.market` (and Gearbox `creditManager` from plan facade) is plan-controlled with exact `safeApprove` / `{value:}` and no on-chain market allowlist (unlike ROUTER_A/B and V2/Curve CREATE2).  
**Mechanism test:** Binding `l.market` to an immutable registry / cETH pin stops FakeMarket and FakeCEther drains.

**Description:**  
Under zero-trust, a compromised OPERATOR encodes a fake Aave-shaped market that returns a low HF and `transferFrom`s the approved debt during `liquidationCall`. The Compound `isCEther` path similarly unwraps WETH and sends ETH to whatever `market` the plan names. NatSpec “curated markets” is not an Accepted Trust Assumption.

**Attack path (ERC-20 approve):**
1. Standing debt ≥ 2× flash amount (or equivalent).  
2. Plan: fake UniV3 flashSource + Aave V3 leg with `market = FakeAaveMarket`.  
3. Fake pulls `repayAmount`; fake flash receives repay; `minProfit = 0`.  

**Attack path (Compound ETH):**
1. Profitable Aave group produces coll→WETH headroom.  
2. Second group: WETH fake flash + Compound `isCEther` leg with `market = FakeCEther`.  
3. Executor unwraps WETH and sends ETH to FakeCEther; profit still clears gross.

**Impact:** One-tx drain of approved debt and/or ETH equal to `repayAmount` under OPERATOR compromise.

**Proof of concept:**  
- `contracts/test/audit/FakeMarketApproveDrain.t.sol` — **PASS**  
- `contracts/test/audit/CompoundCEtherEthDrain.t.sol` — **PASS**  
Command: `forge test --match-contract "FakeMarketApproveDrainPoC|CompoundCEtherEthDrainPoC"`

**Recommendation:** Immutable per-adapter market allowlist (pin official Pool / cETH / facades); never approve or `{value:}` to plan-supplied addresses without binding.

**References:** [Static Analysis](989aab98-fb52-4bdb-84a7-80ffe67f6cff) DRAFT-SA-01; [Manual Review](cf4c280d-2d80-401e-b4d2-35b6bcb94710) DRAFT-07; Immunefi SC-C2.

---

### H-01 — Standing WETH stealable when amount ≤ realized WETH profit

**Severity:** High  
**Immunefi impact:** Direct theft of any user funds, whether at-rest or in-motion, other than unclaimed yield  
**Location:** `Executor.sol`, `execute` gross math `wethAfter - wethBefore` (~261); `_swapLeg` S_ROUTER + `L_TAKE_BALANCE` on WETH  
**Status:** Open — **Confirmed**  
**Interacts with:** C-01  
**impact_class:** `fund_loss`  
**shippable:** yes  
**poc_kind:** unit · **poc_submittable:** true · **Environment:** uploaded

**Broken invariant:** Standing WETH cannot be moved to an arbitrary recipient (NatSpec / INV-08 intent).  
**Root cause:** Profit guard only enforces **net** WETH non-decrease across the tx, not conservation of the pre-tx standing balance identity; router can siphon standing WETH up to newly realized profit.  
**Mechanism test:** Snapshotting `wethStandingBefore` and requiring `wethAfter >= wethStandingBefore` **after excluding** this-tx profit routing to sink-only would block the steal (or ban WETH router `L_TAKE_BALANCE`).

**Description:**  
NatSpec implies standing WETH is protected because gross underflows if the balance falls. That is true only when stolen standing **exceeds** realized profit in the same transaction. If standing `S ≤ P` (profit), the operator can router-steal `S` to an attacker while still ending with `wethAfter ≥ wethBefore`.

**Attack path:**
1. Executor holds standing WETH `S` (e.g. 0.5 ETH) without sweep.  
2. Compromised OPERATOR runs a profitable liquidation realizing `P ≥ S` WETH.  
3. Profit legs: router steals `S` WETH to attacker; remaining coll→WETH supplies `P`.  
4. `gross = wethAfter - wethBefore ≥ 0`; `minProfit` may still pass.

**Impact:** Partial standing-WETH drain up to per-tx realized profit under operator key compromise. Larger standing still protected by underflow (see OperatorRouterDrain WETH revert test).

**Proof of concept:**  
`contracts/test/audit/ExecutorKnownFailProperties.t.sol` — `test_knownfail_INV08_small_standing_weth_stealable` **PASS** (bug confirmed).  
Fuzz: `testFuzz_knownfail_INV08_standing_below_profit_stealable` (256/256).

**Recommendation:** Snapshot standing WETH at start; forbid net transfer of pre-tx WETH to non-`PROFIT_SINK` addresses; or disallow WETH as `tokenIn` on `S_ROUTER` legs.

**References:** INV-08 gap (Tier 3 fuzz); Immunefi SC-C2 (calibrated High: conditional on `S ≤ P`).

---

### M-01 — Curve StableSwap legs use `min_dy = 0`

**Severity:** Medium  
**Immunefi impact:** Griefing (e.g. no profit from trade) — calibrated Medium  
**Location:** `Executor.sol`, `_swapCurve`, `ICurvePool.exchange(..., 0)` (~line 1186)  
**Status:** Open  
**impact_class:** `grief` / `pricing_allocation`  
**shippable:** yes  

**Broken invariant:** Each swap leg should enforce a minimum output consistent with solver quotes.  
**Root cause:** Hardcoded `min_dy = 0`; only plan-level `minProfit` on final WETH protects value.  
**Mechanism test:** Encoding a non-zero min_dy from the plan would block sandboxed under-delivery on that leg.

**Description:**  
Curve exact-input legs accept any output. Adverse execution (builder/searcher conflict, thin pool) can destroy value on that leg; recovery depends entirely on the global WETH `minProfit` check after all groups. Failed plans revert (DoS of the opportunity), but value can also be lost to MEV if `minProfit` is set loosely.

**Attack path:**
1. Plan includes Curve leg with `min_dy=0`.  
2. Sandwich or poor liquidity reduces `dy`.  
3. If remaining WETH still clears `minProfit`, loss is realized; else tx reverts.

**Impact:** Conditional value loss / opportunity DoS; not a permissionless drain of standing balances.

**Recommendation:** Encode per-leg `min_dy` in swap data and pass to `exchange`.

---

### M-02 — Gearbox partial liquidate passes uninitialized `PriceUpdate[]` memory pointer

**Severity:** Medium  
**Immunefi impact:** Griefing (e.g. no profit from trade) — unreliable liquidations / possible garbage calldata  
**Location:** `Executor.sol`, `_liquidateGearbox`, lines 899–906  
**Status:** Open  
**impact_class:** `grief` / `core_flow`  
**shippable:** yes  

**Broken invariant:** Mode-0 Gearbox calls pass an empty `PriceUpdate[]`.  
**Root cause:** `PriceUpdate[] memory none;` without `new PriceUpdate[](0)` leaves a zero pointer; ABI encoding may read scratch `mload(0)`.  
**Mechanism test:** `PriceUpdate[] memory none = new PriceUpdate[](0);` forces empty array.

**Description:**  
Slither `uninitialized-local` confirmed by [Static Analysis](989aab98-fb52-4bdb-84a7-80ffe67f6cff). Dirty scratch space can encode a non-empty or garbage `priceUpdates` array into `partiallyLiquidateCreditAccount`, causing unexpected oracle updates or leg failures.

**Recommendation:**
```solidity
PriceUpdate[] memory none = new PriceUpdate[](0);
```

**References:** Slither uninitialized-local; Solidity memory array allocation.

---

## Phase 8 — Interactions

| Composite | Parents | Note |
|-----------|---------|------|
| COMP-01 | C-01 + C-02 + C-03 + H-01 | Same caller (compromised OPERATOR). Combined blast = router + fake flash + fake market/ETH + partial WETH. Keep separate for independent fixes. |

---

## Residual risk

1. **`forge coverage` unavailable** — cannot quantify line coverage; residual unknown gaps in adapter tails.  
2. **Off-chain searcher / key management** — hot `OPERATOR` is a single point of failure (C-01/C-02). Recommend hardware isolation, spend limits, or session keys with calldata policies.  
3. **SafeTransfer no extcodesize** — empty-code “token” passes; relies on off-chain registry (not an accepted assumption) → Appendix A-01.  
4. **Aave V4 reserve IDs** — not bound on-chain to token addresses; encoder bugs → failed/misdirected liquidations bounded by profit/revert.  
5. **Fee-on-transfer / rebasing** debt or collateral — not specially handled; may break exact-out repay or accounting.  
6. **Silo sTokenRequired** — legs skipped; no redeem path (fail-closed, opportunity loss).  
7. **Specialist connector waves** for Fluid/Gearbox/Liquity/Compound — deep ABI pin risk remains; fork-test every adapter × provider before mainnet (as NatSpec states).  
8. **Slither High `arbitrary-send-eth`** on WETH.deposit / CEther — **false positive** (fixed WETH / plan market).  

---

## Recommendations (priority)

1. **P0:** Fix C-01 (router policy), C-02 (flash provider binding), and C-03 (market allowlist / cETH pin) before any mainnet deploy with standing balances.  
2. **P0:** Treat `OPERATOR` as a high-value hot wallet; minimize standing inventory on the Executor.  
3. **P1:** H-01 standing WETH accounting; M-01 Curve min_dy; M-02 Gearbox `new PriceUpdate[](0)`.  
4. **P1:** Add extcodesize/code check in SafeTransfer or on-chain token allowlist.  
5. **P2:** Restore coverage (reduce stack pressure in tests or dedicated coverage profile).  

---

## Shippability Summary

| Draft | impact_class | immunefi_impact | immunefi_severity | shippable | severity | notes |
|-------|--------------|-----------------|-------------------|-----------|----------|-------|
| DRAFT-01 | fund_loss | SC-C2 Direct theft… | Critical | yes | Critical | C-01 PoC PASS |
| DRAFT-02 | fund_loss | SC-C2 Direct theft… | Critical | yes | Critical | C-02 PoC PASS |
| DRAFT-07 / SA-01 | fund_loss | SC-C2 Direct theft… | Critical | yes | Critical | C-03 PoC PASS (market + CEther) |
| INV-08-GAP | fund_loss | SC-C2 Direct theft… | Critical | yes | **High** | H-01 — capped High (`S ≤ P`) |
| DRAFT-05 | grief | Griefing | Medium | yes | Medium | M-01 |
| DRAFT-SA-02 | grief | Griefing | Medium | yes | Medium | M-02 Gearbox PriceUpdate[] |
| DRAFT-03 | best_practice | null | — | appendix | Info | NatSpec overclaim |
| DRAFT-04 | best_practice | null | — | appendix | Low | SafeTransfer |
| DRAFT-06 | ops_offchain | null | — | appendix | Low | Aave V4 ids |
| Slither arbitrary-send-eth | — | — | — | drop | — | FP |
| Slither reentrancy-balance execute | — | — | — | drop | — | T_ENTERED |

---

## Signature

**Tucker Sossaman**  
Independent security review (AuditAid pipeline)  
26 September 2026  

---

## Appendix A — Low / Informational

### A-01 — SafeTransfer omits contract-code check

**Severity:** Low (appendix)  
**Location:** `SafeTransfer.sol`  
NatSpec documents deliberate omission (~2600 gas). Under zero-trust, an EOA/empty token address can make transfers appear to succeed. Mitigate with on-chain token allowlist or `extcodesize` check.

### A-02 — NatSpec overstates OPERATOR constraints

**Severity:** Informational  
Header claims no arbitrary external call / no free fund movement; `S_ROUTER` allows arbitrary calldata to allowlisted routers (C-01).

### A-03 — Aave V4 reserve IDs unbound on-chain

**Severity:** Low  
Encoder must pin ids; contract cannot verify address↔id.

---

## Appendix B — Slither triage (summary)

Source: `docs/audit/slither-summary.md` / `slither-counts.json` (33 Medium+ in scope).

| Detector | Verdict |
|----------|---------|
| arbitrary-send-eth (WETH.deposit, CEther) | **FP** — fixed WETH / plan market |
| reentrancy-balance (execute, gearbox, redeem) | **FP / mitigated** — T_ENTERED; liquidation try/catch |
| divide-before-multiply (_swapV2) | **Accepted** UniV2 library formula |
| uninitialized-local | **FP** — assigned in try or branches |
| unused-return | **Info** — intentional |

---

## Appendix C — Trust assumptions

None accepted. Operator centralization is **in-scope** as Critical when single-tx drain exists (C-01, C-02, C-03).

---

## Appendix D — Fuzzing / invariants

INV-XX: `docs/audit/inv-xx-draft.md`. Tier 3 added:
- `test/audit/ExecutorFocusInvariants.t.sol` — INV-01/08-enforced/11 **PASS** (10k invariant runs)
- `test/audit/ExecutorKnownFailProperties.t.sol` — INV-09/10 known-fail + **INV-08 gap → H-01**
- `test/audit/ExecutorEdgeFuzz.t.sol` — edge fuzz

Exclude `ExecutorKnownBrokenInvariant` from green CI. Coverage % unavailable.

---

## Appendix E — Code Quality Scorecard

Pass A/B: see `docs/audit/quality-scorecard.md`. Weighted Pass A ≈ **64/100**; Pass B ≈ **68/100** (test pillar after Tier 3 fuzz). Coverage UNVERIFIED. Hotspots: `docs/audit/quality-hotspots.md`.

---

## Appendix F — Playbook §R / §L (CAL-10 / CAL-11)

### R1 — Denominator inventory

| ID | Location | Divisor | Guard |
|----|----------|---------|-------|
| DEN-01 | `execute` bid | `10_000` | constant |
| DEN-02 | Morpho shares | `totalBorrowAssets + 1` | virtual assets |
| DEN-03 | V2 exact-out | `(rOut - amountOut) * 997` | `amountOut >= rOut` reverts |
| DEN-04 | V2 exact-in | `rIn * 1000 + inWithFee` | amountIn>0 ⇒ denom>0 |

### R2 — Reachability
No Executor-persistent share denominator can be zeroed by users. Morpho/V2 guards hold. `bidBps > 10000` → keep underflow (operator self-DoS only).

### L1–L5 — Lending lifecycle (liquidator)
Executor has **no** Loan/CDP storage. External protocol positions are seized into Executor then swapped.  
**L5 repaid-not-closed:** N/A on Executor; Euler batch enforces repay+disableController.  
**INV-LEND-01:** Morpho divisor ≥ 1 via virtual assets.  
**INV-LEND-02:** Group requires `filled ≥ 1` or `AllLegsFailed`.

---

## Appendix G — Specialist merge notes

Tier 2 confirmations integrated from: [Manual Review](cf4c280d-2d80-401e-b4d2-35b6bcb94710), [Economic Analysis](13fd8a51-8cbd-431d-8e8e-0a20cc6d2728), [Static Analysis](989aab98-fb52-4bdb-84a7-80ffe67f6cff), [Integration Review](1a93dfb8-beeb-4f0c-b6c0-9fcab3155d3e), [Token Review](055befa2-f70a-475c-8e2a-00627a5be702), connectors, [Red Team](9c6df1b3-4645-4d40-a5a4-a35a0b2942ed), Tier 3 fuzz. Duplicate drafts (flashSource / router) merged into C-01/C-02; plan-market class → C-03.
