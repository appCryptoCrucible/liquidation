<!-- mechanism-review protocol=silo-v2 repo=silo-finance/silo-contracts-v2 commit=570a668a98a88a6a2b92697e7b9a3b1c6299dce7 date=2026-09-20 verdict=admit -->
# Silo V2 — admitted 3

Source: GitHub `silo-finance/silo-contracts-v2` @ `570a668a98a88a6a2b92697e7b9a3b1c6299dce7` (API redirects to `silo-finance/silo-contracts-v3`; same SHA, `package.json` name still `silo-contracts-v2`, tag 4.26.0, 2026-07-27). Liquidation: `silo-core/contracts/hooks/liquidation/PartialLiquidation.sol` + `IPartialLiquidation.sol`. Factory events: `ISiloFactory.sol`.

Registry @ **26014442**: 252 silos, 2 factories, **3 admitted**.

## Mechanism — hard (direct seize)

`IPartialLiquidation.liquidationCall(_collateralAsset, _debtAsset, _borrower, _maxDebtToCover, _receiveSToken)` on the **hook receiver**, not the Silo ERC-4626. Liquidator `transferFrom`s debt token, `silo.repay`s, seizes collateral+protected shares (`forwardTransferFromNoChecks`), optionally `redeem`s to underlying. Bonus = `collateralConfig.liquidationFee`. Dust rule: `FullLiquidationRequired` if computed repay > max cover. Solvent user → `UserIsSolvent`. Isolated pair: one collateral token backs one debt token per `SiloConfig`.

Not soft, not auction. `maxLiquidation(borrower)` view for close size.

## Debt assets

Each silo: `asset()` = token supplied/borrowed in that silo. Against a collateral silo, the **other** silo in `getSilos()` is the only borrowable debt asset (isolated pair).

Admitted 3 (all USDC debt silos; pair collateral not admitted — borrowed_usd 0):

| silo (debt) | silo_config | debt | borrowed_usd | pair coll silo | pair asset |
|---|---|---|---|---|---|
| `0xce6ab1c7…018a` | `0x10930071…9985` | USDC | 375_815.26 | `0x9ab28417…e766` | RLP `0x4956b52a…f96` |
| `0x1de3ba67…9bc2` | `0xb0b37f55…f61` | USDC | 2_238_887_423.44 | `0x2e3a8f2d…0417` | xUSD `0xe2fc85bf…f94` |
| `0xf82c626e…02db` | `0x74b21d45…ee3` | USDC | 103_565.48 | `0xd796071d…34eb` | savUSD `0xb8d89678…a4` |

USD from `getCollateralAndDebtTotalsStorage()` × Chainlink Feed Registry at 26014442. The 2.24e9 USDC row is an **outlier vs the other two**; do not round it. Adapter must re-read totals on-chain; if the slot is junk, `minProfit` / D27 bar still gates execution.

All other 249 silos: borrowed_usd < 50_000 or 0 (38 of them carry any debt; 41 of all 252 including the 3 admitted).

## Flash depth (D09 + 07A)

Admitted debt = **USDC only**.

| source | USDC at 26M (07A) | note |
|---|---|---|
| Aave V3 `0x87870Bca…` | aUSDC `181646545.035048` | fee_bps=5 |
| UniV3 | 867 pools in registry | 5-bp USDC/WETH `74293828.839265` |
| UniV4 PM `0x000000000004444c5dc75cB358380D2e3dE08A90` | `66230362.793739` | fee 0 |
| Morpho `0xBBBB…ffCb` | `108682339.582079` | fee 0 |
| Sky DSS | no | DAI-only |

Pair collaterals (RLP, xUSD, savUSD): exit via UniV3 if a pool exists; not required to flash (seize then swap, GUIDE 12). `registry.flash_sources = {}`.

## Enumeration (REGISTRY §3 Family B)

Roots (`discover.py`; confirm live): Factory V2 `0x22a3cF6149bFa611bAFc89Fd721918EC3Cf7b581`, Factory V3 `0x1DAb4A310447185144467076b116DAC7aec3b48F`.

1. `getNextSiloId()` then `idToSiloConfig(i)` for `i = 1 .. n-1` (discover; ids start at 1).
2. Logs: `NewSilo(implementation, token0, token1, silo0, silo1, siloConfig)` — token0/token1 indexed.
3. Per config: `(silo0, silo1) = getSilos()`; `IERC4626(silo).asset()`; debt via `getCollateralAndDebtTotalsStorage()`.
4. Positions: `Deposit`/`Borrow`/`Repay`/`Liquidate` + share `Transfer`. Filter §3b.

## Liquidation ABI → 10R-n (D48)

**New target + return values.** Args rhyme with Aave `liquidationCall` but: (1) call the hook, not the pool; (2) returns `(withdrawCollateral, repayDebtAssets)`; (3) `_receiveSToken` ≠ Aave `receiveAToken`; (4) `FullLiquidationRequired`. Do not reuse A_AAVE_V3.

```
liquidationCall(address _collateralAsset, address _debtAsset, address _user, uint256 _maxDebtToCover, bool _receiveSToken) returns (uint256 withdrawCollateral, uint256 repayDebtAssets)
maxLiquidation(address _borrower) view returns (uint256 collateralToLiquidate, uint256 debtToRepay, bool sTokenRequired)
```

Opens `10R-n`. `_isLiquidatable`: `maxLiquidation` debtToRepay>0 / solvency view on config — not Aave HF.
