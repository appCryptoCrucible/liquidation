# Morpho Blue rounding (`morpho-org/morpho-blue` @ `8e26ca6a`)

Morpho uses **floor and ceil only** (`MathLib.mulDivDown` / `mulDivUp`). There is no half-up step.

`WAD = 1e18`. `ORACLE_PRICE_SCALE = 1e36`. Virtual shares `1e6`, virtual assets `1`.

| id | Solidity | rounding | notes |
|---|---|---|---|
| S1 | `toSharesDown` | floor `(a*(S+1e6))/(T+1)` | supply, repay |
| S2 | `toSharesUp` | ceil | withdraw, borrow, liquidate repaidShares from seized |
| A1 | `toAssetsDown` | floor | withdraw assets, seized from repaidShares |
| A2 | `toAssetsUp` | ceil | supply assets from shares; **borrowed** in `_isHealthy`; repaidAssets |
| M1 | `wMulDown` | floor `a*b/WAD` | interest, fee, LIF inner, maxBorrow, seize * LIF |
| M2 | `wDivDown` | floor `a*WAD/b` | LIF = WAD / (WAD - cursor*(WAD-lltv)) |
| M3 | `wDivUp` | ceil | seized → repaidShares |
| M4 | `mulDivDown` | floor | oracle quote, seize * scale / price |
| M5 | `mulDivUp` | ceil | seizedAssetsQuoted |
| I1 | `wTaylorCompounded` | floor each of 3 Taylor terms | IRM rate × elapsed |
| H1 | `_isHealthy` | `toAssetsUp` debt; `mulDivDown` then `wMulDown` collateral | protocol-favouring |
| L1 | LIF | `min(1.15e18, wDivDown(WAD, WAD - wMulDown(0.3e18, WAD-lltv)))` | static per LLTV |
| R1 | `zeroFloorSub` | saturating sub | repay/liquidate totalBorrowAssets |

Adapter `health()` accrues to `pos.timestamp` with the last `AccrueInterest.prevBorrowRate` (the IRM output Morpho stored for that interval), then applies H1.
