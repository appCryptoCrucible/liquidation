# Gearbox V3 rounding (`Gearbox-protocol/core-v3` @ `510fc654`)

Source: `CreditFacadeV3.sol`, `CreditLogic.sol`, `CollateralLogic.sol`,
`PriceOracleV3.convert`, `Constants.sol` @ `510fc6541c3767ce825929b4c311826fe81d6fa5`.

`PERCENTAGE_FACTOR = 1e4`. Fees/discount are **per-manager** `fees()` — there is
no protocol-level liquidation premium. `liquidationDiscount = PERCENTAGE_FACTOR - liquidationPremium` (`UpdateFees` emits premiums). Do not invent 1.05 / 1.08.

| id | Solidity | rounding | notes |
|---|---|---|---|
| C1 | `PriceOracleV3.convert` | one floor | `amount * priceFrom * scaleTo / (priceTo * scaleFrom)`; 8-dec oracle scale cancels when prices are stored as RAY |
| C2 | two-floor convert | n/a | `amount*priceFrom/priceTo` then `*scaleTo/scaleFrom` is a different number (fixture 23 vs 22) — adapter uses C1 |
| P1 | `_calcPartialLiquidationPayments` seized | two sequential floors | `convert(amount, underlying, token) * PERCENTAGE_FACTOR / liquidationDiscount` |
| P2 | `_calcPartialLiquidationPayments` fee | floor | `amount * feeLiquidation / PERCENTAGE_FACTOR` (`feeLiquidationExpired` when `!isUnhealthy`) |
| P3 | `_calcPartialLiquidationPayments` repaid | unchecked sub | `amount - feeAmount` (configurator keeps fee < 100%) |
| H1 | `isUnhealthy` | strict `<` | `cdd.twvUSD < cdd.totalDebtUSD`; equal is healthy |
| H2 | `_isExpired` | `>=` | `expirable && expirationDate != 0 && now >= expirationDate`; expired is liquidatable even if healthy |
| H3 | `_hasBadDebt` | product compare | `totalValue * liquidationDiscount < (debt + accruedInterest) * PERCENTAGE_FACTOR`; uses **non-expired** discount even on expired accounts; `ILossPolicy` is an extra full-close gate, not evaluated off-chain |
| H4 | `CollateralLogic.calcOneTokenCollateral` | floor then min | `min(valueUSD * LT / PF, quotaUSD)`; quoted tokens only + underlying (`quotaUSD = max` for underlying) |
| H5 | `CreditLogic.getLiquidationThreshold` | floor mix | linear ramp `ltInitial → ltFinal`; static = `ltInitial` with ramp start in the future |
| H6 | `CreditLogic.calcAccruedInterest` | floor | `(amount * indexNow) / indexLast - amount`; adapter does **not** invent a pool IRM — `indexNow = indexLast` unless stored |
| H7 | `CreditLogic.calcTotalDebt` | add | `debt + accruedInterest + accruedFees` |
| L1 | `CreditLogic.calcLiquidationPayments` | floor | `totalFunds = totalValue * discount / PF`; `amountToPool += totalValue * feeLiq / PF`; identity `amountWithFee` at this pin |
| L2 | bonus (engine RAY) | floor | `(PERCENTAGE_FACTOR - discount) * RAY / discount` from **this manager's** discount |
| S1 | partial `token != underlying` | n/a | facade reverts if seized token is underlying |
| S2 | full `MultiCall` | unpriced | leftover underlying to `to`; adapter does not invent swap fills |

Adapter `health()` uses H1+H2+H4+H5+H6 on stored balances. `quote()` uses P1–P3 on the preferred non-underlying token. Fail closed if `fees()` was never written (`ManagerRow::FEES`). Missing `PriceVector` entry → `MissingPrice`, never $1.
