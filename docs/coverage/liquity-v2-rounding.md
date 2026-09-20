# Liquity V2 rounding (`liquity/bold` @ `c8a5a4ee`)

Source: `LiquityMath.sol`, `LiquityBase._calcInterest`, `TroveManager._getOffsetAndRedistributionVals` / `_getCollGasCompensation` / `getCurrentICR`. Solidity `/` is floor for unsigned integers.

`DECIMAL_PRECISION = 1e18` (`WAD`). `ONE_YEAR = 365 days = 31536000`. `RAY = 1e27` is the adapter's health scale, not a Liquity type.

| id | Solidity | rounding | notes |
|---|---|---|---|
| C1 | `_computeCR` | floor `coll * price / debt` | `type(uint256).max` at zero debt |
| I1 | `_calcInterest` | floor twice: `weighted * period / ONE_YEAR / DECIMAL_PRECISION` | two successive `/`, not a fused mulDiv |
| I2 | `_getInterestPeriod` | none | `now - last` if `shutdownTime == 0`; else `shutdownTime - last` if `last < shutdownTime`, else 0 |
| R1 | redist gain | floor `stake * (L - snapshot) / DECIMAL_PRECISION` | `L_coll` / `L_boldDebt` |
| B1 | batch recorded debt | floor `batch.debt * shares / totalShares` | `_getLatestTroveDataFromBatch` L996 |
| B2 | batch interest / fee | `_calcInterest` on **batch.debt × rate** (TroveManager L1042 / L1046) then floor `× shares / totalShares` (L998–1000) | not `_calcInterest(recorded × rate)`; association differs by ≥1 wei on GUIDE-01 fixture |
| O1 | `debtToOffset` | `min(entireDebt, boldInSPForOffsets)` | `boldInSPForOffsets = total - min(MIN_BOLD_IN_SP, total)` |
| O2 | `collSPPortion` | floor `entireColl * debtToOffset / entireDebt` | **not** `entireColl` unless the SP covers the whole debt |
| G1 | `_getCollGasCompensation` | floor `_coll / 200`, then `min(·, 2 ether)` | `_coll` is `collSPPortion` |
| G2 | `ETH_GAS_COMPENSATION` | constant `0.0375 ether` | WETH `transferFrom` the gas pool, not native ETH |
| H1 | `_isLiquidatable` | `ICR < MCR` strict | `Status.active` or `Status.zombie` only; no `unredeemable` at this pin |
| H2 | adapter `hf` | floor `ICR * RAY / MCR` | `hf = 1.0` ⇔ `ICR >= MCR` under this floor |
| H3 | unbatched `entireDebt` | no management fee | `_getLatestTroveData` L969–976 when `interestBatchManager == 0` (after `removeFromBatch`) |
| P1 | last healthy coll price | ceil `MCR * entireDebt / entireColl` (WAD), then `* 1e9` RAY | one RAY down floors to `p_wad - 1` |

Adapter `health()` applies pending redistribution and I1 / B2 to `pos.timestamp`, then C1 + H1. Batched vs unbatched is `interestBatchManager != 0` (not `totalShares`). ICR does not load WETH; `quote()` does (gas-pool seize). `quote()` uses G1+G2 only: `BonusCurve::Static { bonus: 0 }`, `max_repay = 0`. Never a flash-repay-seize incentive mantissa. Empty SP still pays `ETH_GAS_COMPENSATION` (0.0375 WETH).
