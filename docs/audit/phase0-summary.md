# Phase 0 — Scope and context intake

**Date:** 26 September 2026  
**Auditor:** Tucker Sossaman (AuditAid pipeline)  
**Repository:** `C:\Users\Davina\Desktop\liquidation bot`  
**Foundry root:** `C:\Users\Davina\Desktop\liquidation bot\contracts`  
**Build:** client-supplied `foundry.toml` — solc 0.8.28, optimizer true, optimizer_runs 200, via_ir true, evm_version cancun → **`SLOW_BUILD`**  
**Chain:** Ethereum mainnet only  
**Deployment:** Executor NOT deployed; NOT a proxy → no `HAS_LIVE_CONTRACTS`  
**INCLUDE_QUALITY_SCORECARD:** yes  
**TIER_2C_ENABLED:** true  

---

## Protocol type

Immutable **flash-loan liquidation executor** (MEV searcher bot). One transaction: borrow → liquidate (multi-adapter) → swap → repay flash → optional `block.coinbase` bid → optional WETH sweep to `PROFIT_SINK`.

## Assets

- Transient ERC-20 debt/collateral (plan-selected)
- Canonical WETH (`0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2`) as profit denomination
- Native ETH via `receive()` / WETH wrap-unwrap / coinbase bid

## Trust model (on-chain enforced)

| Role | Powers | Enforced |
|------|--------|----------|
| `OPERATOR` (immutable) | Sole caller of `execute(bytes)` | `msg.sender == OPERATOR` |
| Anyone | `sweep(address[])` → only to immutable `PROFIT_SINK` | destination fixed |
| Flash providers | Callbacks during armed window | `msg.sender == T_EXPECTED_CALLER` ∧ `T_ENTERED` |
| Plan calldata | Markets, amounts, router calldata, flashSource | Operator-encoded; no on-chain market allowlist |

NatSpec claims OPERATOR cannot move funds freely — **claim, not accepted assumption**. Compromised-key calibration applies.

### Accepted Trust Assumptions (client-documented only)

| ID | Assumption | Enforced on-chain? | If false → finding class |
|----|------------|--------------------|-------------------------|
| — | **None — zero-trust default applies** | — | — |

Constructor addresses (planned, not deployed): OPERATOR `0x3247b0709A3f4457b3FeBB5fB1493dc2d780192F`, PROFIT_SINK `0x11fa49084B4D63b156a4C8238291A562019bA49d`, ROUTER_A=ROUTER_B SwapRouter02 `0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45`, UniV3 factory/init hash as given, WETH, MainnetVenues V2/Sushi/Curve.

---

## Feature flags (confirmed from code)

| Flag | Value | Evidence |
|------|-------|----------|
| `HAS_EXTERNAL_VALUE_DEP` | **true** | Liquidations + flash + swaps move assets via Aave/Morpho/Uni/Curve/Sky/Euler/Silo/Liquity/Fluid/Gearbox/Compound |
| `HAS_MULTI_CONNECTOR` | **true** | ≥2 protocol slugs; adapter functions in Executor.sol |
| `HAS_TRANSIENT` | **true** | `tstore`/`tload` for callback auth / group / swap flag |
| `HAS_LENDING` | **true** | Liquidate/seize/redeem across adapters |
| `HAS_ASSEMBLY` | **true** | Transient + CREATE2 assembly |
| `HAS_PROXY` | **false** | Immutable; no proxy |
| `HAS_SIGNATURES` | **false** | No ECDSA/permit/EIP-712 |
| `HAS_ORACLE` / `HAS_VRF` | **false** | No Chainlink/VRF in scope |
| `HAS_MEV_TREASURY_VAULT` | **false** | Operator + flash present, but no CallPolicy, no delayed admin withdrawal, no deposit/reserve accounting — searcher executor, not vault archetype. **Still apply INV-MEV-01/02-style checks** (router + `L_TAKE_BALANCE`) in economic/manual review |
| `HAS_ZK` / `HAS_ROLLUP_BCR` | **false** | — |
| `HAS_ORDER_STRUCT` | **false** | Ephemeral plan calldata only |
| `HAS_GAMEFI` / `HAS_STATE_FLAGS` | **false** | — |
| `HAS_GAUGE_VOTING` / `HAS_BRIDGE_ROUTER` | **false** | — |
| `SLOW_BUILD` | **true** | via_ir |
| `NO_BUILD_CONFIG` | **false** | Real foundry.toml |
| `HAS_LIVE_CONTRACTS` | **false** | Not deployed |
| `REQUIRES_EXTERNAL_DOC_INTAKE` | **true** | Morpho, UniV2/V4, Curve, Sky, Euler, Silo, Liquity V2, Fluid, Gearbox, Compound, Aave V4 — no dedicated protocol-*.md except Aave V3 + Uni V3 |
| `TIER_2C_ENABLED` | **true** | Defaults |

### Tier 2c config

| Knob | Value |
|------|-------|
| `TIER_2C_MAX_PARALLEL` | 8 |
| `PREMISE_ENTRYPOINT_SHARD_SIZE` | 12 |
| `PREMISE_SHARD_MODE` | auto |
| `INTERACTION_LLM_BATCH_SIZE` | 30 |
| `STATE_PARTITION_MODE` | single (single contract, no independent module seeds) |
| `STATE_GLOBAL_MAX_STATES` | 256 |

---

## Protocol MDs to load

- `protocol-aave-v3.md` (Aave V3 pool flash + liquidation)
- `protocol-uniswap-v3.md` (V3 flash + swap callback CREATE2)
- MEV vault checks 1–4 from `protocol-mev-treasury-vault.md` as guidance (flag false)

## Connector assignment table

| slug | connector file(s) | protocol ref | intake status |
|------|-------------------|--------------|---------------|
| aave-v3 | Executor.sol `_liquidateAaveV3`, `executeOperation` | protocol-aave-v3.md | loaded-md |
| uniswap-v3 | Executor.sol flash/swap callbacks, `_swapLeg` | protocol-uniswap-v3.md | loaded-md |
| aave-v4 | `_liquidateAaveV4` | intake/aave-v4.md | pending |
| morpho-blue | `_liquidateMorpho`, `onMorphoFlashLoan` | intake/morpho-blue.md | pending |
| uniswap-v4 | `unlockCallback`, `_initiate` P_UNIV4 | intake/uniswap-v4.md | pending |
| uniswap-v2 | `_swapV2`, MainnetVenues | intake/uniswap-v2.md | pending |
| curve | `_swapCurve` | intake/curve.md | pending |
| sky-dss | `onFlashLoan`, IDssFlash | intake/sky-dss.md | pending |
| euler-v2 | `_liquidateEulerV2` | intake/euler-v2.md | pending |
| silo-v2 | `_liquidateSiloV2` | intake/silo-v2.md | pending |
| liquity-v2 | `_liquidateLiquityV2` | intake/liquity-v2.md | pending |
| fluid-t1 | `_liquidateFluid` | intake/fluid-t1.md | pending |
| gearbox-v3 | `_liquidateGearbox*` | intake/gearbox-v3.md | pending |
| compound-v2 | `_liquidateCompoundV2` | intake/compound-v2.md | pending |
| erc20-safe | SafeTransfer.sol | intake/erc20.md (USDT/empty code) | pending |

---

## Custody map

| Stage | Where funds sit |
|-------|-----------------|
| At rest (optional) | Any ERC-20 / WETH left on Executor; `sweep` → PROFIT_SINK |
| Flash borrow | Provider → Executor |
| Liquidation repay | Executor → lending market (approval) |
| Seized collateral | Executor (underlying or cToken then redeem) |
| Repay / profit swaps | Executor ↔ pools/routers |
| Flash repay | Executor → provider |
| Bid | WETH unwrap or msg.value → `block.coinbase` |
| Profit | WETH on Executor → PROFIT_SINK if F_SWEEP |

## Authority map (single-tx max outflow)

| Actor | Max single-tx outflow |
|-------|----------------------|
| Compromised OPERATOR | Plan-controlled: flash + liquidate + **router arbitrary calldata with exact approve of plan amounts / `L_TAKE_BALANCE` full balances** + fake `flashSource` callback path. Candidate **one-call drain** of standing non-profit-guarded tokens → Critical calibration |
| Permissionless sweeper | All balances of named assets → PROFIT_SINK only |
| External (unarmed) | Callbacks revert `BadCallback` |

## Attack surface map (capital priority)

1. `execute` + flash callbacks (`executeOperation`, `uniswapV3FlashCallback`, `unlockCallback`, `onMorphoFlashLoan`, `onFlashLoan`) — auth + repay
2. `_swap` / `_swapLeg` router path — arbitrary call + approve
3. `L_TAKE_BALANCE` + profit guard only on WETH gross — non-WETH standing balance
4. UniV3 flash: plan-chosen `flashSource` without CREATE2 (unlike swap callback)
5. Liquidation adapters (wrong market / share math / residual debt)
6. `uniswapV3SwapCallback` CREATE2 + T_SWAPPING
7. `sweep` / `receive` / coinbase bid accounting
8. `SafeTransfer` (no extcodesize; USDT approve dance)

## Stated invariants (from NatSpec — verify in Tier 2)

- Callback requires expected provider AND inside execute
- Profit in WETH; gross = after − before; underflow if standing WETH falls
- Sweep destination immutable
- No arbitrary external call except allowlisted routers with plan calldata
- Per-leg liquidation failure tolerated; all-fail reverts
- Approvals zeroed after liquidate/swap

**Gap:** No formal INV-XX doc in repo for on-chain executor — derive in manual-review.

## Module coverage matrix

| File | Capital role | Assigned reviewer | Status |
|------|--------------|-------------------|--------|
| contracts/src/Executor.sol | Custody + all value paths | manual, economic, integration, static, connectors | pending |
| contracts/src/lib/PlanDecoder.sol | Plan bounds / offsets | manual, integration | pending |
| contracts/src/lib/SafeTransfer.sol | Token movement | token-review, manual | pending |
| contracts/src/lib/Interfaces.sol | External ABIs | integration, connectors | pending |
| contracts/src/lib/MainnetVenues.sol | V2/Curve anchors | integration | pending |

## Entrypoint ledger skeleton

| Entrypoint | Moves value | Storage written | External calls | Reviewer | Outcome |
|------------|-------------|-----------------|----------------|----------|---------|
| `execute(bytes)` | yes | transient only | flash, liq, swap, coinbase, WETH | manual, economic | pending |
| `executeOperation` | yes | transient | `_core`, approve | manual, aave-v3 | pending |
| `uniswapV3FlashCallback` | yes | transient | `_core`, transfer | manual, univ3 | pending |
| `unlockCallback` | yes | transient | take/sync/settle, `_core` | manual, univ4 | pending |
| `onMorphoFlashLoan` | yes | transient | `_core`, approve | manual, morpho | pending |
| `onFlashLoan` | yes | transient | `_core`, approve | manual, sky | pending |
| `uniswapV3SwapCallback` | yes | none persistent | transfer | manual, univ3 | pending |
| `sweep(address[])` | yes | none | transfer → PROFIT_SINK | manual, economic | pending |
| `receive()` | yes (ETH in) | none | — | manual | pending |

## Doc questions (per connector — intake must answer)

1. Who may call the liquidation / flash callback and what initiator checks exist?
2. What is repaid (assets vs shares) and who pulls?
3. What collateral form is received (underlying, shares, cTokens) and is redeem required?
4. Can a malicious/wrong market address steal approvals or leave debt?
5. Fee / close-factor clamping relative to exact approvals?

---

## Out of scope

`contracts/test/**`, `contracts/script/**`, `contracts/src/differential/**`, `liquidator-guides/Executor.sol`, Rust crates (residual risk only except binding failures).
