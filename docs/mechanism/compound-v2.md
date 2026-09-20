<!-- mechanism-review protocol=compound-v2 repo=compound-finance/compound-protocol commit=a3214f67b73310d547e00fc578e8355911c9d376 date=2026-09-20 verdict=admit -->
# Compound V2 forks — admit (266 instances)

Source: `compound-finance/compound-protocol` @ `a3214f67b73310d547e00fc578e8355911c9d376` (`master` tip, 2022-06-07). `CErc20.liquidateBorrow` / `CEther.liquidateBorrow`; gate `Comptroller.liquidateBorrowAllowed` + `liquidateCalculateSeizeTokens`. GUIDE-01 row: `(Comptroller, user)`, shortfall, static bonus, `closeFactorMantissa`. GUIDE-15: one adapter + config covers the fork family.

Registry @ **26014442**: **266** comptrollers (from `MarketListed` logs), **1982** cTokens, instance `admitted: true`, **0 admitted_markets** (Family A; D27 borrowed_usd bar is Family B only — not a mechanism decline).

Official Compound Unitroller in this snapshot: **`0x3d9819210A31b4961b30EF54bE2aeD79B9c9Cd3B`**, 20 cTokens. Forks are the same ABI with a different comptroller root.

## Mechanism — hard (shortfall seize)

Liquidator repays underlying on the **borrowed cToken** and seizes **cToken collateral** (then redeem). Allowed only if `getAccountLiquidityInternal` returns `shortfall > 0` (`INSUFFICIENT_SHORTFALL` otherwise), both markets listed. Close size: `maxClose = closeFactorMantissa × borrowBalanceStored` (`TOO_MUCH_REPAY` if over). Deprecated markets skip the shortfall check but cannot repay more than the borrow.

Seize math: `comptroller.liquidateCalculateSeizeTokens(cTokenBorrowed, cTokenCollateral, actualRepayAmount)` uses **`liquidationIncentiveMantissa`** (admin `file`, not a constant). Do not invent 1.08.

`closeFactorMantissa` is per-comptroller admin storage. Pin bounds only: `closeFactorMinMantissa = 0.05e18`, `closeFactorMaxMantissa = 0.9e18`. Read the live mantissa per fork.

Not auction, not LLAMMA.

## Debt assets

Each listed cToken: one `underlying` (CEther = ETH / `msg.value`, no ERC-20 `underlying`). Any listed market can be the borrowed leg; collateral is any other listed cToken the borrower holds. Cross-asset via Comptroller entered markets.

Discovery stores cToken addresses, **not** underlyings. Resolve `underlying()` per cToken; do not guess symbols. Official 20-market set is in `registry.json` `compound-v2:0x3d981921…`. Forks vary — that is the config surface.

## Flash depth (D09 + 07A)

Flash the **underlying** being repaid, not the cToken. Depth is per-fork token set (runtime). For underlyings already in 07A @ **26_000_000**:

| if underlying is | Aave V3 | UniV3 n | UniV4 PM | Morpho | Sky DSS |
|---|---|---|---|---|---|
| USDC | aUSDC `181646545.035048` | 867 | PM `66.2M` | Morpho `108.7M` | no |
| WETH / ETH | aWETH `288592.198…` | 28034 | PM `2131` | Morpho `15880` | no |
| DAI | untracked → runtime | 92 | runtime | runtime | **yes** `max=5e8` wad |
| USDT / WBTC / others | runtime | registry.pools if present | runtime | runtime | no |

Long-tail fork underlyings with no D09 depth fail GUIDE-15 §5 and stay untargetable. Aave fee_bps=5 @26M.

## Enumeration (REGISTRY §3 Family A)

`Comptroller.getAllMarkets()` → every cToken. Discover found comptrollers by `MarketListed` logs (`discover.py`), then verified `getAllMarkets()`.

1. Per fork: root = Unitroller/Comptroller in that fork's TOML. Same code, different address.
2. Per cToken: `underlying()`, `comptroller()`, `borrowIndex`.
3. Positions: `Borrow` / `RepayBorrow` / `LiquidateBorrow` / `Mint` / `Redeem` on listed cTokens. No global user list.
4. Oracle: that comptroller's `oracle()` — per fork, not a shared feed.

## Liquidation ABI → 10R-n (D48)

**New** vs Aave/Morpho. Two entrypoints (same seize path):

```
CErc20: liquidateBorrow(address borrower, uint repayAmount, address cTokenCollateral) returns (uint)
CEther: liquidateBorrow(address borrower, address cTokenCollateral) payable
```

Opens `10R-n`. `_isLiquidatable` = Comptroller shortfall > 0 (or deprecated-market path), not Aave HF. Fork multiplier = this ABI + a TOML root.
