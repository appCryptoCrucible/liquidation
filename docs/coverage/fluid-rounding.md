# Fluid rounding (`Instadapp/fluid-contracts-public` @ `9496626f`)

Source: `contracts/protocols/vault/vaultT1/coreModule/main.sol` `liquidate` / `operate`, `contracts/libraries/tickMath.sol`, T2/T3/T4 `coreModule/main.sol` @ `9496626f71a761fc296dc3b2efbfd54c504e18f0`.

All `/` in the pin are Solidity floor (`Rounding.Down`). TickMath uses wrapping `mul`/`shr` then round-up remainder on the positive `getRatioAtTick` path.

| id | Solidity | rounding | notes |
|---|---|---|---|
| R-ORACLE | `getExchangeRateLiquidate` | protocol | FluidOracle **1e27** (not 1e8). Adapter: `p_coll * 10^debt_dec * 1e27 / (p_debt * 10^coll_dec)` floor via two `mulDivDown`. Zero price → `MissingPrice`, never $1 |
| R-RAW | `(oracle * supplyExPrice) / borrowExPrice` | floor | then cap `1e45` |
| R-COLPER | `1e54 / raw` then `* (10000 + penalty) / 10000` | floor | `colPerDebt` 27 decimals; penalty 4-dec (`100` = 1%) |
| R-LIQRATIO | `(raw * 2^96 / 1e27) * threshold / 1000` | floor | threshold 3-dec (`900` = 90%) |
| R-TICK | `TickMath.getTickAtRatio` | floor | negative ratios `not(tick)` (round toward −∞) |
| R-RATIO | `TickMath.getRatioAtTick` | wrap+round up rem | positive ticks: `div(MAX, factor)` + 1 if `mod 2^32`; tick 1 = `79347004758035734099934266261` |
| R-EX | `amt * 1e12 / exPrice` (to raw) | floor | operate supply down; liquidate `raw * ex / 1e12` floor |
| R-DEBTREMAIN | `(debtAmt_ * 1e12) / borrowExPrice` | floor | input token → raw |
| R-X | `((debt - refRatio*debt/ratio) * 1e27) / (1e27 - colPerDebt*refRatio/2^96)` | floor | single-segment `debtLiquidated_`; if `debtLiquidated_ == debt` then `− 1` wei |
| R-COLX | `(debtLiquidated_ * colPerDebt) / 1e27` | floor | |
| R-TOKENOUT | `(totalLiq * exPrice) / 1e12` | floor | then slip check `(actualCol * 1e18) / actualDebt < colPerUnitDebt_` **1e18** (not 1e27) |
| R-T3SHARE | T3/T4 `getExchangeRateLiquidate` | 1e27 share-per-col | T2/T3/T4 fail closed at health/quote/operate/liquidate (`vault_type != T1` or `n_col\|n_debt != 1`). `PriceVector` is token USD; pin FluidOracle is share units. No `borrow1 == 0` T1-clone gate. |
| H1 | `top_tick > liquidation_tick` | n/a | Liquidatable; equality is Healthy |
| H2 | `BadDebt` | n/a | only zero seizable coll (incl. absorbed) with remaining debt |
| H3 | `n_nfts > 1` without top tick | fail closed | do not use aggregate ratio as the tree top |
| L1 | `debtAmt_ < 10000 \|\| > 2^128` | revert | pin `Vault__InvalidLiquidationAmt` |
| L2 | `absorb_ = true` | pin | absorbed inventory first |
| L3 | `to_ == dEaD` | revert result | `FluidLiquidateResult(col, debt)` quote path (10R) |
| S1 | native `msg.value` | revert if mismatch | UNPRICED; not WETH |

Adapter `health()` / `quote()` use H1 + R-ORACLE…R-X on stored vault totals when `TOP_KNOWN` and `tickStatus == 1`. Fail closed if constants were never written (`VaultRow::VIEWED`) or exchange prices missing (`EX_KNOWN`). `time_to_cross` does not invent a rate. Tick-walk / partials / multi-NFT are not implemented — 10R dead-address quote is the live max.
