# Orchestrator draft findings (pre-shippability IDs)

Confirmed via unit PoC. Executor **not deployed** → `poc_kind: unit` is Confirmed/submittable per ref-fork-poc (not_live).

---

## [DRAFT-01] Compromised OPERATOR drains standing non-WETH via allowlisted router

**Severity draft:** Critical  
**impact_class:** fund_loss / admin_trust (Template B — single-tx drain with operator key)  
**Location:** `Executor._swapLeg` S_ROUTER branch (lines 1109–1120); `execute` profit/repay `_swap`; `L_TAKE_BALANCE` (1083–1085)  
**broken_invariant:** Standing non-WETH balances cannot leave except via immutable PROFIT_SINK sweep  
**root_cause:** Exact approve of full token balance to ROUTER_A/B + arbitrary `target.call(data[20:])` with no recipient/token allowlist; profit guard only measures WETH delta  
**mechanism_test:** Compromised operator encodes router calldata that `transferFrom`s standing ERC-20 to attacker during profit legs  

**PoC:** `contracts/test/audit/OperatorRouterDrain.t.sol`  
- `test_poc_operator_drains_standing_non_weth_via_router` — PASS  
- `test_poc_standing_weth_theft_via_router_reverts` — PASS (WETH standing protected)  

**immunefi_impact (candidate):** Direct theft of any user funds (standing balances are protocol/op treasury on the executor)

---

## [DRAFT-02] Compromised OPERATOR steals via malicious plan `flashSource` (no CREATE2 / provider allowlist)

**Severity draft:** Critical  
**impact_class:** fund_loss / admin_trust  
**Location:** `_arm(fg.flashSource)` (363–365); `uniswapV3FlashCallback` (416–421); same pattern for other providers via `_checkCallback` only  
**broken_invariant:** Flash callback caller must be a real flash provider that lent funds  
**root_cause:** Callback auth binds `msg.sender` to plan-selected `flashSource`, not to a CREATE2-verified pool or immutable provider allowlist. UniV3 swap path uses CREATE2; flash path does not.  
**mechanism_test:** Fake flash contract calls callback without transferring assets; Executor still pays `flashAmount+fee` to Fake  

**PoC:** `contracts/test/audit/FakeFlashSourceDrain.t.sol`  
- `test_poc_fake_univ3_flashSource_steals_repay` — PASS  

---

## [DRAFT-03] NatSpec security model overstates OPERATOR constraints

**Severity draft:** Informational / Low (documentation)  
**Location:** Executor.sol header NatSpec lines 18–27  
Claims OPERATOR “cannot move funds to any address of its choosing” / “cannot make an arbitrary external call” — contradicted by S_ROUTER path (allowlisted target, arbitrary calldata). Standing non-WETH drain is DRAFT-01.

---

## [DRAFT-04] SafeTransfer omits extcodesize / code existence check

**Severity draft:** Medium or Low (residual — off-chain registry claimed)  
**Location:** SafeTransfer.sol lines 22–29, 35–54  
Empty-code address returns success on call; zero-trust: plan/registry can introduce EOA token → silent “success”. Not Accepted Trust Assumption.

---

## [DRAFT-05] Curve `exchange` min_dy = 0

**Severity draft:** Medium (sandwich within tx depends on builder inclusion; grief via bad route)  
**Location:** Executor._swapCurve line 1186  
Relies solely on plan-level `minProfit` after all legs — no per-leg protection.

---

## [DRAFT-06] Aave V4 reserve IDs unbound to collateral/debt addresses on-chain

**Severity draft:** Low/Medium — encoder bug / wrong ids; profit guard / revert bounds theft of flash funds but can mis-seize  
**Location:** `_liquidateAaveV4` + `plan.tailV4`

---

Slither High `arbitrary-send-eth` on WETH.deposit / CEther liquidate — **false positive** (fixed WETH / market from plan).  
Slither `reentrancy-balance` on execute — mitigated by T_ENTERED; verify in manual-review.
