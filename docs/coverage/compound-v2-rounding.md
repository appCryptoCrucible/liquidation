# Compound V2 rounding (`compound-finance/compound-protocol` @ `a3214f67`)

Source: `ExponentialNoError.sol`, `Comptroller.sol` (`getHypotheticalAccountLiquidityInternal`, `liquidateCalculateSeizeTokens`, `liquidateBorrowAllowed`), `CToken.sol` (`borrowBalanceStoredInternal`, `exchangeRateStoredInternal`) @ `a3214f67b73310d547e00fc578e8355911c9d376`.

`Exp.mantissa` is 1e18. Every `mul_` / `div_` / `truncate` is **floor** (unsigned `/`). `halfExpScale` exists on `Double` only and is not used on the liquidation path.

| id | Solidity | rounding | notes |
|---|---|---|---|
| E1 | `mul_(Exp, Exp)` | floor | `a * b / 1e18` |
| E2 | `div_(Exp, Exp)` | floor | `a * 1e18 / b` |
| E3 | `mul_ScalarTruncate` | floor | `mantissa * scalar / 1e18` |
| E4 | `mul_ScalarTruncateAddUInt` | floor then add | liquidity sums |
| H1 | `tokensToDenom` | E1 twice | `CF * exchangeRate * oraclePrice` |
| H2 | `sumCollateral` | E4 | `tokensToDenom * cTokenBalance` |
| H3 | `sumBorrow` | E4 | `oraclePrice * borrowBalance` |
| H4 | shortfall | subtract | `sumBorrow > sumColl` → liquidatable; equality is **not** |
| B1 | `borrowBalanceStored` | floor | `principal * marketIndex / accountIndex` |
| X1 | `exchangeRateStored` | floor | `(cash + borrows - reserves) * 1e18 / totalSupply` |
| L1 | `maxClose` | E3 | `closeFactorMantissa * borrowBalance` — admin file, pin bounds 0.05e18–0.9e18 |
| L2 | `seizeTokens` | E1, E2, E3 | `incentive * priceBorrowed / (priceCollateral * exchangeRate)` then `* repay` |
| L3 | deprecated | n/a | skip shortfall; repay ≤ borrow; CF=0 ∧ borrow paused ∧ RF=1e18 |
| P1 | PriceVector | floor | RAY-per-1.0-token → Compound mantissa `price_wad * 10^(18-decimals)` |
| T1 | `time_to_cross` | n/a | IRM rate is not in `AccrueInterest` — `None`, not invented |

`liquidationIncentiveMantissa` is admin `_setLiquidationIncentive`, never a `1.08` constant. Adapter bonus RAY = `(incentive − 1e18) * 1e9`.
