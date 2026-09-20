# Aave V3 — per-step rounding table (WP 04B)

Source pin: `aave-dao/aave-v3-origin` @ `8305565ae342f1773c42cd2e4593f175fe5968a0`
(`main`, 2026-09-09; parent tag `v3.7.0`). Code: `crates/liq-adapters/aave-v3/src/math.rs`.

Directions: **half-up** = `(a·b + HALF) / ONE` (`WadRayMath.rayMul` / `wadDiv` /
`PercentageMath.percentMul`); **down** = floor; **up** = ceil. 3.5 `TokenMath` is
ERC-4626 protocol-favouring: aToken mint / balance floor, aToken burn/transfer ceil,
vToken mint / balance ceil, vToken burn floor.

## Index and accrual (`MathUtils`, `ReserveLogic`, `WadRayMath`)

| step | Solidity | expression | direction |
|---|---|---|---|
| I1 | `MathUtils.calculateLinearInterest` | `RAY + rate·Δt / YEAR` | **down** |
| I2 | `getNormalizedIncome` (same second) | stored `liquidityIndex` | exact |
| I3 | `getNormalizedIncome` | `rayMul(I1, liquidityIndex)` | **half-up** |
| D1 | `MathUtils.calculateCompoundedInterest` | `x = rate·Δt / YEAR`; `RAY + x + rayMul(x, x/2 + rayMul(x, x/6))` | **down** on `x`; **half-up** on `rayMul` |
| D2 | `getNormalizedDebt` (same second) | stored `variableBorrowIndex` | exact |
| D3 | `getNormalizedDebt` | `rayMul(D1, variableBorrowIndex)` | **half-up** |

## Token balances (`TokenMath` 3.5+)

| step | Solidity | expression | direction |
|---|---|---|---|
| C1 | `getATokenBalance` | `rayMulFloor(scaled, liquidityIndex)` | **down** |
| C2 | `getATokenMintScaledAmount` | `rayDivFloor(amount, index)` | **down** |
| C3 | `getATokenBurnScaledAmount` / transfer | `rayDivCeil(amount, index)` | **up** |
| V1 | `getVTokenBalance` | `rayMulCeil(scaled, variableBorrowIndex)` | **up** |
| V2 | `getVTokenMintScaledAmount` | `rayDivCeil(amount, index)` | **up** |
| V3 | `getVTokenBurnScaledAmount` | `rayDivFloor(amount, index)` | **down** |

## Health (`GenericLogic.calculateUserAccountData`)

| step | Solidity | expression | direction |
|---|---|---|---|
| C4 | collateral base | `(C1 · price) / 10^decimals` | **down** |
| C5 | `avgLiquidationThreshold` accumulator | `Σ C4 · LT` (e-mode LT if bitmap bit set) | exact |
| D4 | debt base | `mulDivCeil(V1, price, 10^decimals)` | **up** |
| H1 | health factor | `wadDiv(C5, D4) / 10_000` | **half-up** then **down** |
| H2 | adapter | `hf_wad · 1e9` | exact (WAD→RAY) |
| H3 | adapter | base · `10^(18 − oracle_decimals)` | exact (Health WAD numeraire) |

E-mode: collateral LT/bonus from the category when the reserve is on
`collateralBitmap`; LTV 0 when on `ltvzeroBitmap`. Isolated e-mode zeroes LTV of
non-bitmap collateral (`ValidationLogic.getUserReserveLtv`). Isolation-mode debt
ceilings and siloed flags are **config/registry**, not compiled constants (Spark 15A-2).

## Liquidation (`LiquidationLogic`)

| step | Solidity | expression | direction |
|---|---|---|---|
| B1 | bonus | reserve `liquidationBonus`, or e-mode bonus if collateral is on the e-mode bitmap | exact (static) |
| Q1 | close factor | 100% unless both legs ≥ `min_base_max_close` **and** `hf > close_factor_hf`; then `percentMul(totalDebt, close_factor_bps)` | **half-up**; all three params from `Config` |
| Q2 | `baseCollateral` | `(debtPrice · cover · collUnit) / (collPrice · debtUnit)` | **down** |
| Q3 | `maxCollateral` | `percentMulFloor(Q2, bonus)` | **down** |
| Q4 | seize-all re-solve | `percentDivCeil(...)` | **up** |
| Q5 | protocol fee | `percentDivFloor` then `percentMulCeil` of bonus slice | **down** / **up** |
| Q6 | leftover dust | `mulDivCeil` remaining debt; floor remaining collateral; both ≥ `min_base/2` or revert `MustNotLeaveDust` | **up** / **down** |

3.3+ deficit: `DeficitCreated` adds leftover debt after collateral is exhausted;
`DeficitCovered` writes it down. User scaled debt is already burned on the
liquidation/burn path.

Grace: `liquidationGracePeriodUntil > now` → `HealthState::Blocked { GracePeriod }`.
L2 `PriceOracleSentinel.isLiquidationAllowed`: sequencer answer 0 and
`now ≥ updatedAt + grace`; missing sequencer update with a configured sentinel is
fail-closed (`Blocked`). Sentinel address is read from `PoolAddressesProvider`
(config).
