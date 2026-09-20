<!-- mechanism-review protocol=euler-v2 repo=euler-xyz/euler-vault-kit commit=bfb325a6e6ca09613d940b46f72ccfe017353933 date=2026-09-20 verdict=admit -->
# Euler V2 (EVK) — admitted 26

Source: `euler-xyz/euler-vault-kit` @ `bfb325a6e6ca09613d940b46f72ccfe017353933` (`master`, 2026-09-01). Liquidation: `src/EVault/modules/Liquidation.sol`. Factory: `src/GenericFactory/GenericFactory.sol`.

Registry @ block **26014442**: 884 vaults, **26 admitted** (interim `$50_000` borrowed, D27 unset / `registry.meta.json`).

## Mechanism — hard (direct seize)

`liquidate(violator, collateral, repayAssets, minYieldBalance)` on the **debt vault** (controller). Liquidator takes on violator debt (`transferBorrow`) and seizes collateral vault shares (`enforceCollateralTransfer`). Partial OK; `repayAssets = type(uint256).max` = max. Discount: `discountFactor = collAdjValue / liabilityValue`, floored at `minDiscountFactor = 1e18 − 1e18 × maxLiquidationDiscount / CONFIG_SCALE` (`CONFIG_SCALE = 1e4`). `maxLiquidationDiscount` is a **per-vault governor setting** (`Governance.setMaxLiquidationDiscount(uint16)`, only constraint `!= CONFIG_SCALE`); there is **no protocol-level cap constant** at this pin — read `maxLiquidationDiscount()` on each vault, never assume a fixed ceiling. Remaining debt socialized if no collateral left and `CFG_DONT_SOCIALIZE_DEBT` unset (`DebtSocialized`). Not Dutch, not LLAMMA. Cool-off: `liquidationCoolOffTime` after last status check. Self-liq, unrecognized collateral, deferred EVC check → revert.

`checkLiquidation` returns `(0,0)` if healthy (does not revert).

## Debt assets

Each EVault: one `asset()` = the token borrowed **in that vault**. Collateral = other EVaults with `LTVLiquidation(collateral) > 0` (`LTVList()`). Registry stores vault→`asset` only; LTV graph is on-chain, not in `registry.json`.

Admitted 26 debt tokens (vault.asset, borrowed_usd at 26014442):

| asset | n vaults | largest vault borrowed_usd |
|---|---|---|
| USDC `0xa0b8…eB48` | 16 | 14_387_124.49 (`0x01864ae3…`) |
| WETH `0xc02a…6Cc2` | 2 | 4_270_112.44 |
| WBTC `0x2260…c599` | 2 | 5_756_279.57 |
| RLUSD `0x8292…17ed` | 2 | 432_652.39 |
| USD0 `0x73a1…0acf5` | 1 | 6_669_767.39 |
| wstETH `0x7f39…2ca0` | 1 | 650_568.47 |
| USDT `0xdac1…1ec7` | 1 | 286_258.63 |
| PYUSD `0x6c3e…a0e8` | 1 | 136_883.25 |

Borrow vs a given collateral C: any admitted vault V where `V.LTVBorrow(C) > 0` and the user enabled C on V's controller. Do not guess LTV; read `LTVFull`.

## Flash depth (D09 + 07A + registry.pools)

`registry.flash_sources = {}`. 07A balances at block **26_000_000**. UniV3 counts = `registry.pools` venue=univ3 containing the token.

| debt | Aave V3 `0x87870Bca…` | UniV3 n | UniV4 PM | Morpho | Sky DSS |
|---|---|---|---|---|---|
| USDC | aUSDC `181646545.035048` | 867 | PM USDC `66230362.793739` | Morpho USDC `108682339.582079` | no |
| WETH | aWETH `288592.198…` | 28034 | PM WETH `2131.15…` | Morpho WETH `15880.32…` | no |
| WBTC | 07A untracked → runtime | 129 | runtime | runtime | no |
| USDT | runtime | 877 | runtime | runtime | no |
| wstETH | runtime | 29 | runtime | runtime | no |
| PYUSD | runtime | 14 | runtime | runtime | no |
| RLUSD | runtime | 11 | runtime | runtime | no |
| USD0 | runtime | 4 | runtime | runtime | no |
| DAI (not an admitted debt) | 07A untracked → runtime | 92 | runtime | runtime | `max=5e8` wad |

Aave fee_bps=5 @26M. UniV4/Morpho/Sky fee=0. Depth for non-USDC/WETH/DAI is log-driven 07A `SlotTable`, not a registry balance.

## Enumeration (REGISTRY §3 Family B)

Root: GenericFactory **`0x29a56a1b8214D9Cf7c5561811750D5cBDb45CC8e`** (`tools/registry/discover.py`; confirm live).

1. `n = getProxyListLength(); getProxyListSlice(0, n)` (discover uses 100-wide slices).
2. Mirror logs: `ProxyCreated(address indexed proxy, bool upgradeable, address implementation, bytes trailingData)`.
3. Per proxy: `asset()`, `totalBorrows()`, `LTVList()`. Admit if `_borrowed_usd(totalBorrows, asset) ≥ 50_000` (interim).
4. Positions: `Borrow` / `Withdraw` / `Liquidate` on admitted vaults. No global user list. Per-vault addresses grow; prune filter must regenerate (REGISTRY §3).

## Liquidation ABI → 10R-n (D48)

**New.** Not Aave `liquidationCall`, not Morpho `liquidate(id, …)`.

```
liquidate(address violator, address collateral, uint256 repayAssets, uint256 minYieldBalance)
checkLiquidation(address liquidator, address violator, address collateral) view returns (uint256 maxRepay, uint256 maxYield)
```

Target = debt EVault. Collateral arg = collateral vault address (shares), not underlying. Opens `10R-n`. `_isLiquidatable`: `checkLiquidation` maxRepay>0 (or RiskManager liquidity); do not reuse Aave HF.
