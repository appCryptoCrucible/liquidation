<!-- mechanism-review protocol=liquity-v2 repo=liquity/bold commit=c8a5a4ee2e9dc024905856b6698a77d849c68c7e date=2026-09-20 verdict=admit -->
# Liquity V2 (BOLD) / trove forks — admit (not in registry.json)

Source: `liquity/bold` @ `c8a5a4ee2e9dc024905856b6698a77d849c68c7e` (`main`, 2026-07-13). `contracts/src/TroveManager.sol`, `contracts/src/Dependencies/Constants.sol`, mainnet `contracts/addresses/1.json`. GUIDE-01: `(trove_id)`, ICR vs MCR, fixed bonus, full/batch. GUIDE-15 §5: `SortedTroves` walk is enumerable — does **not** fail the scan-decline rule.

Not in `registry.json`. BOLD is in `registry.tokens` (18 decimals) and is **unpriced** (`admission:unpriced:… no USD source (BOLD)`).

## Mechanism — hard (batch, Stability Pool offset)

`TroveManager.batchLiquidateTroves(uint256[] _troveArray)` — permissionless. Empty → `EmptyData`. No liquidatable trove in the list → `NothingToLiquidate`. Per id: skip unless `_isActiveOrZombie` (`Status.active` or `Status.zombie` — the enum at this pin has no `unredeemable`); liquidate iff `getCurrentICR(id, price) < MCR`.

**Counterparty is the Stability Pool, not the caller.** `_getOffsetAndRedistributionVals` sends debt/coll to SP (offset) and/or other troves (redistribution). Liquidator profit is **gas compensation only**:

- `ETH_GAS_COMPENSATION = 0.0375 ether` (locked at open; constants + `1.json`)
- coll gas: `min(entireColl / COLL_GAS_COMPENSATION_DIVISOR, COLL_GAS_COMPENSATION_CAP)` with `DIVISOR = 200` (0.5%) and `CAP = 2 ether`

Caller does **not** repay BOLD and does **not** seize the trove's collateral (surplus above the penalty goes to `CollSurplusPool` for the owner). This is still a threshold seize (`ICR < MCR`), not Dutch, not LLAMMA. Flash-repay-seize cycle is **unused** for the liquidation tx.

Branch constants (this pin — not governor storage):

| | WETH branch | SETH branches (wstETH / rETH) |
|---|---|---|
| MCR | `110 * _1pct` | `120 * _1pct` |
| CCR | `150 * _1pct` | `160 * _1pct` |
| LIQUIDATION_PENALTY_SP | `5 * _1pct` | `5 * _1pct` |
| LIQUIDATION_PENALTY_REDISTRIBUTION | `10 * _1pct` | `20 * _1pct` |

`MIN_LIQUIDATION_PENALTY_SP = 5e16`, `MAX_LIQUIDATION_PENALTY_REDISTRIBUTION = 20e16` are the bounds those branch values sit in. On the deployed `TroveManager` these are **`internal immutable`s** set from `_addressesRegistry.MCR()` etc. at construction — the table is the deploy-script value at this pin; the adapter reads the branch's `AddressesRegistry.MCR()` / `CCR()` / penalties at boot and asserts against the config, it does not hard-code them. Redemptions are a different path (`CollateralRegistry`); not this adapter's seize.

## Debt assets

**One debt token: BOLD** `0x6440f144b7e50d6a8439336510312d2f54beb01d` (`1.json` `boldToken`). Collateral is per branch (`1.json` at this pin):

| collSymbol | collToken | TroveManager |
|---|---|---|
| WETH | `0xc02a…6Cc2` | `0x7bcb64b2c9206a5b699ed43363f6f98d4776cf5a` |
| wstETH | `0x7f39…2ca0` | `0xa2895d6a3bf110561dfe4b71ca539d84e1928b22` |
| rETH | `0xae78…6393` | `0xb2b2abeb5c357a234363ff5d180912d319e3e19e` |

`CollateralRegistry` `0xf949982b91c8c61e952b3ba942cbbfaef5386684`. Forks: same recipe, that fork's branch addresses in config. Confirm `1.json` vs live deployments before baking.

## Flash depth (D09 + 07A)

Liquidation itself does not flash BOLD. Survey anyway (GUIDE-15 flashloanability):

| debt | Aave V3 | UniV3 n | UniV4 PM | Morpho | Sky DSS |
|---|---|---|---|---|---|
| BOLD | 07A untracked → runtime | **1** pool (`registry.pools` contains BOLD) | runtime | 9 Morpho markets have `loan_token = BOLD` (singleton flash if inventory exists) | no (DAI only) |

BOLD has **no** Chainlink Feed Registry USD price in this registry (`admission:unpriced`). Do not invent a USD notional.

## Enumeration (REGISTRY §3 Liquity V2)

1. Per collateral branch: `1.json` / fork registry → `TroveManager` + `SortedTroves` + `BorrowerOperations` (+ `CollateralRegistry`).
2. Open troves: `SortedTroves.getFirst()` then `getNext(id)` until `0`. Ids are `uint256` NFTs, not EOAs.
3. State: `TroveManager.getLatestTroveData(troveId)`.
4. Incremental: that branch's TroveManager operation events. Full walk is discovery/reconciliation, not every block.

## Liquidation ABI → 10R-n (D48)

**New.** Not repay-and-seize.

```
batchLiquidateTroves(uint256[] _troveArray)
```

Opens `10R-n`. `_isLiquidatable` = `ICR < MCR` on that branch (MCR read from the branch `AddressesRegistry`, table above is the pin value), not Aave HF. Quote bonus = gas-comp formulas in `Constants.sol`, not an incentive mantissa.
