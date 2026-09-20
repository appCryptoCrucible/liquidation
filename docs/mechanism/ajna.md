<!-- mechanism-review protocol=ajna repo=ajna-finance/ajna-core commit=0f59e78031af76d62ad575c18405eb325b28849f date=2026-09-20 verdict=admit-zero -->
# Ajna — admitted 0

Source: `ajna-finance/ajna-core` @ `0f59e78031af76d62ad575c18405eb325b28849f` (`master`, 2024-02-17; default branch is `master`, not `main`). Kick: `src/libraries/external/KickerActions.sol`. ABIs: `IPoolKickerActions.sol`, `IPoolTakerActions.sol`, `IPoolFactory.sol`.

Registry @ **26014442**: 235 pools (224 ERC20 + 11 ERC721), **0 admitted**.

## Mechanism — Dutch auction (not hard seize, not LLAMMA)

Not `liquidationCall`. Two-step:

1. `kick(address borrower, uint256 npLimitIndex)` — permissionless. Reverts `BorrowerOk` if still collateralized vs proposed LUP; `AuctionActive` if already kicked. Posts quote-token bond. Emits `Kick`. Moves loan off the heap into `auctions.liquidations[borrower]`.
2. `take(address borrower, uint256 maxAmount, address callee, bytes data) returns (uint256 collateralTaken)` — buy collateral at the **time-decaying auction price** (or `bucketTake` at bucket price). Optional `IERC*Taker.atomicSwapCallback`.

This is `HealthState::AuctionOpen` (GUIDE-01), **D14 defers auction machinery**. Not `SoftLiquidating` (no continuous band rebalance). Bond game + decaying price ≠ Aave bonus.

Pool also implements `IERC3156FlashLender` — **not** a D09 source; do not add it.

## Why admitted = 0

Interim bar: `borrowed_usd ≥ 50_000` (`registry.meta.json` `threshold_usd_borrowed`, D27 unset).

| | n | max borrowed_usd |
|---|---|---|
| all pools | 235 | 467.25 |
| borrowed_usd > 0 | 16 | 467.25 (WETH/ELON `0x706931fb…`) |
| borrowed_usd is None (unpriced quote) | 8 | — |

Max priced debt **$467.25**, two orders below the bar. REGISTRY.md §3 (Ajna recipe): “long-tail debt often fails GUIDE 07 flashloanability — expect a high decline rate, which is correct.” Here the bar fails first: even flashable quotes (USDC/WETH/DAI) have dust borrows. 8 unpriced quotes cannot admit (no USD notional).

Quote-token mix (registry): USDC 51+4 NFT, WETH 27+5 NFT, DAI 18, LVWETH* 40+, USDT 12, AJNA 10, … — permissionless long tail.

## Debt assets

Per pool: **one quote token** (`quoteTokenAddress()`) borrowable against **one collateral** (`collateralAddress()`). No cross-collateral. ERC721 pools: NFT collateral / ERC20 quote.

## Flash depth (D09 + 07A)

For the 16 in-debt pools, quotes seen: WETH, DAI, USDC, wstETH, sDAI. Depth if we ever cleared D14+D27:

| quote | Aave 26M | UniV3 n | UniV4/Morpho 26M | Sky |
|---|---|---|---|---|
| USDC | aUSDC `181646545.035048` | 867 | PM `66.2M` / Morpho `108.7M` | no |
| WETH | aWETH `288592.198…` | 28034 | PM `2131` / Morpho `15880` | no |
| DAI | untracked in 07A Aave fixture | 92 | runtime | **yes** `max=5e8` wad |
| wstETH | runtime | 29 | runtime | no |
| sDAI | not in 07A fixtures | registry.pools only if present | runtime | no |

`registry.flash_sources = {}`. Ajna ERC-3156 pool flash is out of D09.

## Enumeration (REGISTRY §3 Family B)

Roots (`discover.py`; confirm live): ERC20Factory `0x6146DD43C5622bB6D12A5240ab9CF4de14eDC625`, ERC721Factory `0x27461199d3b7381De66a85D685828E967E35AF4c`.

1. `getDeployedPoolsList()` on each factory (what discover used) **or** `PoolCreated(address pool_, bytes32 subsetHash_)` logs.
2. Per pool: `quoteTokenAddress()`, `collateralAddress()`, `debtInfo()` (WAD quote debt). Scale WAD → token decimals before USD.
3. Positions: `DrawDebt` / `Kick` / `Take`. No oracle; liquidatability is LUP vs threshold price on-chain.

## Liquidation ABI → 10R-n (D48)

**New family + auction.** Even after D27, D14 blocks the adapter. When both lift:

```
kick(address borrower_, uint256 npLimitIndex_)
lenderKick(uint256 index_, uint256 npLimitIndex_)
take(address borrowerAddress_, uint256 maxAmount_, address callee_, bytes data_) returns (uint256 collateralTaken_)
bucketTake(address borrowerAddress_, bool depositTake_, uint256 index_)
```

Opens `10R-n`. Not Aave/Morpho. `_isLiquidatable` must read auction/LUP state, not HF.
