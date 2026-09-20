# Euler V2 rounding (`euler-xyz/euler-vault-kit` @ `bfb325a6e6ca09613d940b46f72ccfe017353933`)

Source: `Liquidation.sol`, `LiquidityUtils.sol`, `Cache.sol`, `Owed.sol`, `RPow.sol`, `LTVConfig.sol`, `Constants.sol`, `BorrowUtils.sol`. Directions copied from those files, not inferred.

`WAD = 1e18`. `RAY = 1e27` (interest accumulator). `CONFIG_SCALE = 1e4`. `INTERNAL_DEBT_PRECISION_SHIFT = 31`.

Liquidation path uses **mid-point** `oracle.getQuote` (`liquidation=true`). Account-status `checkLiquidity` uses bid/ask `getQuotes` and is **not** the liquidation predicate.

| id | Solidity | rounding | notes |
|---|---|---|---|
| R1 | `RPow.rpow` | half-up via `scalar >> 1`; EVM wrapping `mul`; overflow flag | `Cache.initVaultCache`; overflow keeps old accumulator |
| A1 | `newAcc = oldAcc * multiplier / 1e27` | floor; wrapping identity check | skipped on rpow overflow or wrapping mul overflow |
| A2 | `newTotalBorrows = old * newAcc / oldAcc` | floor; wrapping identity | same block as A1 |
| D1 | `OwedLib.toAssetsUpUint` | ceil `(owed + 2^31 - 1) >> 31` | liability assets; `getCurrentOwed(...).toAssetsUp()` |
| D2 | `OwedLib.toAssetsDown` | floor `owed >> 31` | not used on the liquidation path |
| D3 | `OwedLib.mulDiv` | floor `owed * vaultAcc / userAcc` | `getCurrentOwed` |
| D4 | `Assets.toOwed` | `assets << 31` | `increaseBorrow` / remaining after repay |
| Q1 | `oracle.getQuote` | oracle-defined (mid) | liability token = `vault.asset`; collateral token = **collateral vault** |
| Q2 | `getLiabilityValue` unit-of-account short-circuit | identity | if `asset == unitOfAccount`, value = owedAssets (no oracle) |
| L1 | `getCollateralValue` LTV | floor `quote * ltv / CONFIG_SCALE` | `liquidation=true` uses liquidation LTV (ramped) |
| L2 | `LTVConfigLib.getLTV` ramp | floor `target + (initial-target) * timeRemaining / rampDuration` | only while lowering LTV |
| H1 | violation | `collateralAdjustedValue > liabilityValue` is healthy | **equal is liquidatable** (`calculateMaxLiquidation`) |
| DF1 | `discountFactor = collAdj * 1e18 / liability` | floor | |
| DF2 | `minDiscountFactor = 1e18 - 1e18 * maxLiquidationDiscount / CONFIG_SCALE` | floor inner; unchecked sub | `maxLiquidationDiscount` per-vault; `!= CONFIG_SCALE` |
| DF3 | `discountFactor = max(DF1, DF2)` | | |
| M1 | `maxYieldValue = maxRepayValue * 1e18 / discountFactor` | floor | |
| M2 | `maxRepayValue = collateralValue * discountFactor / 1e18` | floor | when collateral is the binding cap |
| M3 | `repay = maxRepayValue * liabilityAssets / liabilityValue` | floor | then `.toAssets()` |
| M4 | `yieldBalance = maxYieldValue * collateralBalance / collateralValue` | floor | |
| M5 | worthless collateral (`collateralValue == 0`) | repay 0, yield = full balance | adapter skips this path (`EmptyQuote`) rather than invent a repay |
| P1 | desired repay `yieldBalance = desired * yield / maxRepay` | floor | `calculateLiquidation` when `repayAssets != type(uint256).max` |
| S1 | `decreaseBorrow` | `toAssetsUp` then subtract assets, remainder `<< 31` | protocol-favouring on repay |
| F1 | fee assets | floor `(newBorrows - old) * fee / (CONFIG_SCALE << 31)` | fee share mint; not an HF input |

Adapter `health()` accrues the vault accumulator with R1/A1 to `pos.timestamp` using the last stored `interestRate`, then D3+D1+Q1+L1+H1. `quote()` uses DF1–M4 (`calculateMaxLiquidation`).
