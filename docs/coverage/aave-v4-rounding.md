# Aave V4 — per-step rounding table (WP 04A)

Source pin: `aave/aave-v4` @ `40232a0a91150d8ee5cab42bd3ddd0baf4ffff9f` (main, 2026-09-15).
Code: `crates/liq-adapters/aave-v4/src/math.rs` (one function per row), consumed by
`health.rs`, `solve.rs`, `quote.rs`. Every row names the Solidity line that fixes the
direction; the adapter reproduces it bit-for-bit, in `U256` with `U512` intermediates
(`liq_types::fixed::mul_div`), exactly as the EVM widens to `uint256` and
`Math.mulDiv` uses a 512-bit product.

Directions: **exact** = integer arithmetic with no division (checked; the chain reverts
on overflow, the adapter returns `Fixed(Overflow)`); **down** = floor; **up** = ceil.
Widths are the storage widths the chain casts back into (`toUint120` etc.); the
adapter's `narrow()` refuses a log value that does not fit (`MalformedLog`).

## Index and accrual (`AssetLogic`, `MathUtils`, `WadRayMath`)

| step | Solidity | expression | direction | note |
|---|---|---|---|---|
| I1 | `MathUtils.calculateLinearInterest` | `RAY + rate·Δt / SECONDS_PER_YEAR` | **down** | `Δt = ts − lastUpdateTimestamp`; `ts < lastUpdate` is the chain's revert → `TimestampBeforeUpdate` |
| I2 | `AssetLogic.getDrawnIndex` (no time passed, or `drawnShares == premiumShares == 0`) | stored `drawnIndex` | exact | short-circuit; the stored index is what every reader sees |
| I3 | `AssetLogic.getDrawnIndex` | `rayMulUp(drawnIndex, I1)` | **up** | `WadRayMath.rayMulUp`: `(a·b + RAY − 1) / RAY`; `uint120` |
| I4 | `Premium.calculatePremiumRay` | `premiumShares·idx − premiumOffsetRay` | exact | `int256` then `toUint256()`: negative reverts → `Fixed(Underflow)` |
| I5 | `AssetLogic._calculateAggregatedOwedRay` | `drawnShares·idx + I4 + deficitRay` | exact | RAY-scaled asset units |
| I6 | `WadRayMath.fromRayUp` | `ceil(x / RAY)` | **up** | used by `totalAddedAssets` and `getUnrealizedFees` |
| I7 | `AssetLogic.getUnrealizedFees` | `percentMulDown(fromRayUp(owed(idx)) − fromRayUp(owed(stored)), liquidityFee)` | **down** | zero when `idx == stored` or fee is zero |
| I8 | `AssetLogic.totalAddedAssets` | `liquidity + swept + fromRayUp(I5) − realizedFees − I7` | exact | subtraction below zero is the chain's revert → `Fixed(Underflow)` |

## Health (`Spoke._processUserAccountData`, `SpokeUtils.toValue`)

| step | Solidity | expression | direction | note |
|---|---|---|---|---|
| C1 | `SharesMath.toAssetsDown` via `Hub.previewRemoveByShares` | `suppliedShares·(I8 + 1e6) / (addedShares + 1e6)` | **down** | virtual shares/assets `1e6`; the user's collateral in asset units |
| C2 | `SpokeUtils.toValue` | `C1 · price · 10^(18 − decimals)` | exact | `price` is the oracle's 8-decimal `uint256`; `Value` = `1e26` per USD |
| C3 | accumulate | `totalCollateralValue += C2`; `avgCollateralFactor += collateralFactor · C2` | exact | `avgCollateralFactor` stays **un-normalised** (bps · Value) until H1 (`Spoke.sol:748,770`) |
| D1 | `UserPositionUtils.getDebt` | `drawnShares · idx` | exact | RAY-scaled |
| D2 | `Premium.calculatePremiumRay` (user) | `premiumShares·idx − premiumOffsetRay` | exact | as I4, on the user's pair |
| D3 | `UserPositionUtils.getDebt` | `D1 + D2` | exact | `debtRay` |
| D4 | `SpokeUtils.toValue` | `D3 · price · 10^(18 − decimals)` | exact | `totalDebtValueRay += D4` |
| H1 | `Spoke.sol:773` | `mulDiv(avgCollateralFactor.bpsToWad(), RAY, totalDebtValueRay, Floor)` | **down** | `bpsToWad = ·1e14`; `type(uint256).max` when `totalDebtValueRay == 0` |
| H2 | `Spoke.sol` | `totalDebtValue = totalDebtValueRay.fromRayUp()` | **up** | reporting only; `hf` is never derived from it |
| H3 | adapter | `hf_wad · 1e9` | exact | WAD → RAY normalisation; `uint256.max` ↦ `Health::NO_DEBT_HF` |
| H4 | adapter | `Value / 1e8` → WAD numeraire | **down** | reporting only (`Health::{debt,collateral}_value`); below any feed's resolution |
| P1 | adapter | `Ray price / 1e19` → 8-decimal answer | **down** | `liq-oracle` scales answers by `1e19`, so exact for every real price; zero is the oracle's `InvalidPrice` revert → `MissingPrice` |

## Liquidation price (`solve.rs`, derived from H1)

`hf ≥ 1e18 ⇔ Σ cf·C2 · 1e23 ≥ Σ D4` (no division: `floor(x) ≥ n ⇔ x ≥ n`). With `X` the
slots of the asset, `coef = 1e23·Σ_X cf·a·s − Σ_X dr·s` (`s = 10^(18−d)`, `a` = C1, `dr` = D3):

| case | boundary | direction | returned RAY price |
|---|---|---|---|
| net collateral (`coef > 0`) | `p* = (D_other − C_other) / coef` | **up** | `p*·1e19` — one RAY unit lower reads as `p* − 1` (unhealthy) |
| net debt (`coef < 0`) | `p* = (C_other − D_other) / |coef|` | **down** | `(p* + 1)·1e19 − 1` — the largest RAY that still reads as `p*` |
| `coef == 0`, asset not held, boundary at or below price `1` | — | — | `None` |

Verified exact by the 10 000-case round trip (`tests/conformance.rs`).

## Liquidation amounts (`LiquidationLogic._calculateLiquidationAmounts`)

| step | Solidity | expression | direction | note |
|---|---|---|---|---|
| B1 | `calculateLiquidationBonus` | `min = percentMulDown(max − 1e4, factor) + 1e4` | **down** | bps |
| B2 | `calculateLiquidationBonus` | `hf ≤ hfForMaxBonus ? max : min + mulDivDown(max − min, 1e18 − hf, 1e18 − hfForMax)` | **down** | evaluated at the **current** `hf` (H1); `BonusCurve::HealthLinear` carries `min`, `max`, `hfForMax`, quantum 1 bp |
| Q1 | `_calculateDebtToTargetHealthFactor` | `penalty = percentMulUp(bpsToWad(bonus), collateralFactor)` | **up** | |
| Q2 | `_calculateDebtToTargetHealthFactor` | `mulDiv(totalDebtValueRay, unit·(target − hf), (target − Q1)·price·1e18, Ceil)` | **up** | RAY-scaled debt units; the WP's `R = (target·D − LT·C)/(target − (1+b)·LT)` |
| Q3 | `_calculateDebtToLiquidate` | `premium = min(roundRayUp(Q2), premiumDebtRay)` | **up** | `roundRayUp = fromRayUp · RAY`; `debtToCover < fromRayUp(premium)` caps it at `debtToCover·RAY` |
| Q4 | `_calculateDebtToLiquidate` | `drawn = min(divUp(Q2 − premium, idx), (debtToCover − fromRayUp(premium))·RAY / idx, drawnShares)` | **up** / **down** | only when the whole premium is taken and `Q2` exceeds it; the cover term is `rayDivDown` |
| Q5 | dust (debt) | `remaining = (drawnShares − Q4)·idx + premiumDebtRay − Q3`; `toValue(remaining) < 1000e26·RAY ⇒ take all` | exact | `DUST_LIQUIDATION_THRESHOLD = 1000e26` |
| Q6 | `_calculateCollateralToLiquidate` | `assets = mulDiv(Q4·idx + Q3, debtPrice·collUnit·bonus, debtUnit·collPrice·1e4·RAY, Floor)` | **down** | then `toSharesDown(assets, I8, addedShares)` **down** |
| Q7 | dust (collateral) | `toAssetsDown(supplied − Q6)`, `toValue(·) < 1000e26 ⇒ seize all` | **down** | |
| Q8 | seize-all re-solve | `debtRay = mulDiv(toAssetsUp(supplied), collPrice·debtUnit·1e4·RAY, debtPrice·collUnit·bonus, Ceil)` | **up** | `toAssetsUp` **up**; then `premium = min(roundRayUp(debtRay), premiumDebtRay)` or `drawn = divUp(debtRay − premium, idx)` **up**, capped at `drawnShares` |
| Q9 | `amountToRestore` | `rayMulUp(Q4, idx) + fromRayUp(Q3)` | **up** | the liquidator's payment = `RepayOption::max_repay` |
| Q10 | `collateralSharesToLiquidator` | `shares − mulDivUp(shares, fee·(bonus − 1e4), bonus·1e4)` | **up** (fee) | protocol fee shares |

## `HalfUp` (carry-forward 1)

`liq_types::fixed::Rounding::HalfUp` exists for Aave **V3**'s `rayMul`/`percentMul`
(`(a·b + HALF) / RAY`), added to the single `mul_div` with the `(1, 1)` tie-break test.
Aave V4 has no half-up step: `WadRayMath` at `40232a0a` only exposes explicit
`*Up`/`*Down` variants, and `PercentageMath` likewise. No row above uses it.
