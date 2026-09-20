<!-- mechanism-review protocol=compound-v3 repo=compound-finance/comet commit=f766f51583c23acc33b2a7824654ef2029a96804 date=2026-09-20 verdict=deferred -->
# Compound V3 (Comet) — DEFERRED (D25)

Status: **deferred, not declined.** GUIDE-15 §1b / STATE D25. No adapter this phase. Abstraction-coverage risk accepted: `HealthState::AuctionOpen` + two-leg quote untested until later.

Source: `compound-finance/comet` @ `f766f51583c23acc33b2a7824654ef2029a96804` (`main`, 2026-06-23). Impl `contracts/CometWithExtendedAssetList.sol` (no `Comet.sol` at this pin).

## Mechanism — hard, two-leg (not `liquidationCall`)

Not a repay-and-seize. Two permissionless calls:

1. `absorb(address absorber, address[] accounts)` — takes the **entire** account. No collateral/amount choice. Seizes all `userCollateral` onto protocol, zeros `assetsIn`, moves debt onto reserves. Caller gets `LiquidatorPoints` (gas accounting), not the discount. Reverts `NotLiquidatable` unless `isLiquidatable`: `principal < 0` and `Σ coll·price·liquidateCollateralFactor < |baseValue|`.
2. `buyCollateral(address asset, uint minAmount, uint baseAmount, address recipient)` — purchase seized collateral from protocol reserves. `quoteCollateral`: `discount = storeFrontPriceFactor × (1e18 − liquidationFactor)`; pay `baseToken`. Blocked if `getReserves() >= targetReserves` (`NotForSale`).

Flash cycle: *flash base → `buyCollateral` at discount → swap coll→base → repay flash*. Money is in the buy, not the absorb. Race: one searcher absorbs, another buys. Model: `HealthState::AuctionOpen` (GUIDE-15 §1b). That is why it is sequenced after the single-mechanism path is proven.

## Debt assets

One **base token** per Comet (`baseToken` immutable). Collaterals = `getAssetInfo(i)` for `i ∈ [0, numAssets)`. Live set is on-chain (migrations add assets); repo `deployments/mainnet/*/configuration.json` is the **initial** listing only — do not treat it as current.

Mainnet Comets from `deployments/mainnet/*/roots.json` + `configuration.json` `baseTokenAddress` at this pin (Family A governed set):

| market dir | comet | baseTokenAddress |
|---|---|---|
| usdc | `0xc3d688B66703497DAA19211EEdff47f25384cdc3` | USDC `0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48` |
| weth | `0xA17581A9E3356d9A858b789D68B4d866e593aE94` | WETH `0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2` |
| usdt | `0x3Afdc9BCA9213A35503b077a6072F3D0d5AB0840` | USDT `0xdAC17F958D2ee523a2206206994597C13D831ec7` |
| wbtc | `0xe85Dc543813B8c2CFEaAc371517b925a166a9293` | WBTC `0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599` |
| wsteth | `0x3D0bb1ccaB520A66e607822fC55BC921738fAFE3` | wstETH `0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0` |
| usds | `0x5D409e56D886231aDAf00c8775665AD0f9897b56` | USDS `0xdC035D45d973E3EC169d2276DDab16f1e407384F` |

Shared Configurator `0x316f9708bB98af7dA9c68C1C3b5e79039cD336E3`. Not in `registry.json` (discovery skipped; D25).

## Flash depth (D09 five sources, 07A)

`registry.flash_sources = {}`. Depths below are 07A fixtures at **block 26_000_000** (registry head 26_014_442). Runtime: log-driven `FlashSource::available`.

| base | Aave V3 Pool `0x87870Bca…` | UniV3 (registry pool count) | UniV4 PoolManager | Morpho `0xBBBB…` | Sky DSS DAI-only |
|---|---|---|---|---|---|
| USDC | yes; aUSDC bal `181646545.035048` | 867 pools | PM USDC `66230362.793739` | Morpho USDC `108682339.582079` | no |
| WETH | yes; aWETH `288592.198…` ETH | 28034 | PM WETH `2131.15…` | Morpho WETH `15880.32…` | no |
| USDT | listed on V3 Pool (07A tracks if seeded) | 877 | runtime SlotTable | runtime | no |
| WBTC | same | 129 | runtime | runtime | no |
| wstETH | same | 29 | runtime | runtime | no |
| USDS | same | 45 | runtime | runtime | no (DSS is DAI `0x6B17…`, not USDS) |
| DAI (not a Comet base) | 07A untracked → runtime | 92 | runtime | runtime | `max=5e8` DAI wad; fee 0 |

Aave fee_bps = 5 at 26M (`FLASHLOAN_PREMIUM_TOTAL`). UniV3 fee = pool tier. UniV4/Morpho/Sky fee = 0.

## Enumeration (REGISTRY §3 Family A)

REGISTRY.md has no Comet row. Recipe from this pin:

1. Governed set = `deployments/mainnet/*/roots.json` `comet` addresses (table above). Not permissionless.
2. Per comet: `baseToken()`, `numAssets()`, `getAssetInfo(i)`, `baseTokenPriceFeed()`.
3. Positions: `AbsorbDebt` / `AbsorbCollateral` / `Supply` / `Withdraw` / `Transfer` on that comet. No global user list.
4. Confirm configurator via `Configurator.getConfiguration(comet)` — do not hard-code asset lists from JSON after migrations.

## Liquidation ABI → 10R-n (D48)

**New family.** Executor first deploy is Aave V3/V4 + Morpho Blue only.

```
absorb(address absorber, address[] accounts)
buyCollateral(address asset, uint256 minAmount, uint256 baseAmount, address recipient)
quoteCollateral(address asset, uint256 baseAmount) view returns (uint256)
isLiquidatable(address account) view returns (bool)
```

Not `liquidationCall`. Opens `10R-n` when the deferral lifts. `_isLiquidatable` must call `isLiquidatable`, not Aave HF.
