# Silo V2 rounding (`silo-finance/silo-contracts-v2` @ `570a668a`)

Source: `Rounding.sol`, `SiloMathLib.sol`, `SiloSolvencyLib.sol`, `PartialLiquidationLib.sol`, `PartialLiquidation.sol` @ `570a668a98a88a6a2b92697e7b9a3b1c6299dce7`.

OpenZeppelin `Math.mulDiv` with `Rounding.Floor` / `Ceil` only. `_PRECISION_DECIMALS = 1e18`. `_DECIMALS_OFFSET_POW = 1000`. `_UNDERESTIMATION = 2`. `_FULL_LIQUIDATION_THRESHOLD = 0.9e18`. `_BAD_DEBT = 1e18` (cover-size gate in `liquidationPreview` only — not a `BadDebt` health cut).

| id | Solidity | rounding | notes |
|---|---|---|---|
| R1 | `Rounding.LTV` | ceil | `SiloSolvencyLib.ltvMath` = `debt * 1e18 / coll` |
| R2 | `Rounding.COLLATERAL_TO_ASSETS` | floor | borrower coll + protected in `isSolvent` / `maxLiquidation` |
| R3 | `Rounding.DEBT_TO_ASSETS` | ceil | borrower debt in `isSolvent` / `maxLiquidation` |
| R4 | `Rounding.LIQUIDATE_TO_SHARES` | floor | hook converts seize assets → shares |
| R5 | `Rounding.DOWN` | floor | `valueToAssetsByRatio`, fee `debt * fee / 1e18` |
| R6 | `Rounding.ACCRUED_INTEREST` | floor | not folded from events (no rate in `AccruedInterest`) |
| C1 | `_commonConvertTo` collateral | `shares+1000`, `assets+1` | empty silo first zeros dust assets |
| C2 | `_commonConvertTo` debt | raw totals | empty debt: shares==assets |
| H1 | `isSolvent` | `ltv <= collateralConfig.lt` | adapter storage totals (`AccrueInterestInMemory.No`); wei gap vs `Views.isSolvent` (accrues) is fail-closed — no invented IRM |
| H2 | `getPositionValues` | oracle `quote` or price 1 | no oracle → amount as value; adapter uses `PriceVector` RAY by AssetId, not `row.price_feed` |
| H3 | `_BAD_DEBT` | n/a | LTV ≥ 1e18 with coll remaining is Liquidatable; `BadDebt` only when coll+protected assets are 0 |
| L1 | `estimateMaxRepayValue` | floor div | `x = (Dv*1e18 - LT*Cv) / (1e18 - (LT + LT*f/1e18))`; dust → full debt |
| L2 | `calculateCollateralToLiquidate` | floor fee then add | cap at total collateral |
| L3 | `maxLiquidation` preview | floor ratio; −2 wei coll | `collateralToLiquidate` underestimated |
| L4 | `FullLiquidationRequired` | `repay > maxCover` | hook `require`; adapter does not shrink below computed repay |
| L5 | bonus | `collateralConfig.liquidationFee` WAD | engine RAY = fee * 1e9 |
| S1 | `UserIsSolvent` | `debtConfig.silo == 0` | no debt / solvent; `quote` is `None` |

Adapter `health()` / `quote()` use H1 + L1–L3 on stored totals. Fail closed if `getConfig` params were never written (`SiloRow::VIEWED`). `time_to_cross` does not invent an IRM rate from `AccruedInterest(hooksBefore)`. `liquidationPreview` any-cover when LTV ≥ `_BAD_DEBT`; adapter quotes `maxLiquidation` amounts.
