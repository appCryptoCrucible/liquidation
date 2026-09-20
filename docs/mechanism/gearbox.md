<!-- mechanism-review protocol=gearbox repo=Gearbox-protocol/core-v3 commit=510fc6541c3767ce825929b4c311826fe81d6fa5 date=2026-09-20 verdict=admit -->
# Gearbox V3 — admit (not in registry.json)

Source: `Gearbox-protocol/core-v3` @ `510fc6541c3767ce825929b4c311826fe81d6fa5` (`main`, 2026-06-20). Facade: `contracts/credit/CreditFacadeV3.sol` + `contracts/interfaces/ICreditFacadeV3.sol`. Manager: `contracts/interfaces/ICreditManagerV3.sol`. Register: `contracts/interfaces/base/IContractsRegister.sol`. GUIDE-15: credit-account family; `creditAccounts()` is enumerable — does **not** fail the scan-decline rule.

Not in `registry.json`. D15 enumerated 1 `ContractsRegister` + 34 credit managers (confirm live).

## Mechanism — hard (account close or partial seize)

Two permissionless paths on **CreditFacadeV3** (not the manager — manager `liquidateCreditAccount` is `creditFacadeOnly`):

1. **Full:** `liquidateCreditAccount(creditAccount, to, calls, lossPolicyData)` (3-arg overload passes `""`). Reverts unless `debt != 0` and (`twvUSD < totalDebtUSD` **or** expired). Liquidator runs `MultiCall[]` (facade + **Gearbox adapters** on the account) to convert collateral → underlying. Non-underlying balances must not increase. Manager then repays the pool, transfers leftover underlying to `to` (`remainingFunds`). Bad-debt path additionally requires `ILossPolicy.isLiquidatableWithLoss`.
2. **Partial:** `partiallyLiquidateCreditAccount(creditAccount, token, repaidAmount, minSeizedAmount, to, priceUpdates)`. Caller `transferFrom`s **underlying** (approval to the credit manager), repays, seizes one `token` at the manager's liquidation discount. No adapter calls. `token` must not be underlying.

Unhealthy test (this pin): `isUnhealthy = cdd.twvUSD < cdd.totalDebtUSD`. Expired accounts are liquidatable even if healthy.

Fees/discount are **per-manager** `fees()` → `(feeInterest, feeLiquidation, liquidationDiscount, feeLiquidationExpired, liquidationDiscountExpired)`. There is **no protocol-level fee constant** at this pin — read `fees()` on each manager. `PERCENTAGE_FACTOR` is imported (bps scale used in `_calcPartialLiquidationPayments`); do not invent the numeric fee.

Not Dutch, not LLAMMA. Full close is a different shape than Aave (account + adapter multicall). Partial is closer to repay-and-seize.

## Debt assets

One **underlying** per `CreditManagerV3` (`underlying()`). That is the token borrowed from `pool()`. Collateral = enabled tokens on the credit account (`enabledTokensMask` / `getTokenByMask`). D15 listed 34 managers from register **`0xA50d4E7D8946a7c90652339CDBd262c375d54D99`** — underlyings are on-chain, not in `registry.json`. Do not invent a USDC/WETH list; read each manager.

## Flash depth (D09 + 07A)

Full liquidation often needs **no** external flash (swaps run on the account via Gearbox adapters). Partial **does** need the underlying in hand.

| if underlying is | Aave V3 | UniV3 n | UniV4 PM | Morpho | Sky DSS |
|---|---|---|---|---|---|
| USDC | aUSDC `181646545.035048` | 867 | PM `66.2M` | Morpho `108.7M` | no |
| WETH | aWETH `288592.198…` | 28034 | PM `2131` | Morpho `15880` | no |
| DAI | untracked → runtime | 92 | runtime | runtime | **yes** `max=5e8` wad |
| other | runtime SlotTable | registry.pools if present | runtime | runtime | no |

D08 swap venues stay UniV3/Curve/Kyber — Gearbox adapter routes are **inside** the credit account, not a sixth D08 venue. Aave fee_bps=5 @26M.

## Enumeration (REGISTRY §3 Gearbox V3)

Family A–like roots, Family B–like accounts.

1. Root: `IContractsRegister.getCreditManagers()` on **`0xA50d4E7D8946a7c90652339CDBd262c375d54D99`** (D15; confirm live).
2. Per manager: `underlying()`, `creditFacade()`, `fees()`, `creditAccountsLen()` + `creditAccounts(offset, limit)` (or `creditAccounts()`).
3. Incremental: account-factory take/deploy + facade `OpenCreditAccount` / `CloseCreditAccount` / `LiquidateCreditAccount` / `PartiallyLiquidateCreditAccount`.
4. Admit managers whose underlying is flashloanable (GUIDE-07) if the adapter uses the partial path; full path still needs an exit for leftover underlying.

## Liquidation ABI → 10R-n (D48)

**New.**

```
liquidateCreditAccount(address creditAccount, address to, MultiCall[] calls, bytes lossPolicyData)
liquidateCreditAccount(address creditAccount, address to, MultiCall[] calls)  // empty lossPolicyData
partiallyLiquidateCreditAccount(address creditAccount, address token, uint256 repaidAmount, uint256 minSeizedAmount, address to, PriceUpdate[] priceUpdates) returns (uint256 seizedAmount)
```

`MultiCall { address target; bytes callData; }`. Opens `10R-n`. `_isLiquidatable` = facade/manager `twvUSD < totalDebtUSD` or expired, not Aave HF.
