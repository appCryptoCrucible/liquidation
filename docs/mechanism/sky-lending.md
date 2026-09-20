<!-- mechanism-review protocol=sky-lending repo=sky-ecosystem/dss commit=fa4f6630afb0624d04a003e920b0d71a00331d98 date=2026-09-20 verdict=deferred -->
# Sky Lending (Maker CDP) — DEFERRED (D14)

Status: **deferred, not declined.** GUIDE-15 §1: "Different family; auction-based." STATE **D14** defers auction machinery (`HealthState::AuctionOpen` / `TimeDescending`). Largest Tier 2 (~$5.6B TVL in the guide). No adapter this phase.

Source: `sky-ecosystem/dss` @ `fa4f6630afb0624d04a003e920b0d71a00331d98` (`master` tip, 2022-05-18 — core is frozen). Liquidation: `src/dog.sol`, `src/clip.sol`. Registry family `sky-maker` @ **26014442**: 1 instance, **35 ilks**, 20 unique gems, instance `admitted: true`, **0 admitted_markets** (Family A; D27 bar is Family B only).

## Mechanism — Dutch auction (not hard seize, not LLAMMA)

Not `liquidationCall`. Two permissionless legs:

1. `Dog.bark(bytes32 ilk, address urn, address kpr) returns (uint256 id)` — starts the auction. Reverts `Dog/not-unsafe` unless `ink * spot < art * rate` (`Vat.urns` / `Vat.ilks`). Partial if `Hole`/`ilk.hole` room is tight; leftover dusty vault is fully taken. `vat.grab` moves `ink`/`art` onto the ilk's Clipper. `tab = dart * rate * chop / WAD`. Then `Clipper.kick`, which pays `kpr` a DAI suck `coin = tip + wmul(tab, chip)` (`vat.suck(vow, kpr, coin)`) — both are **per-clipper `file` params**, not constants; `redo` pays the same incentive.
2. `Clipper.take(uint256 id, uint256 amt, uint256 max, address who, bytes data)` — buy collateral at the **time-decaying** price `calc.price(top, now − tic)` (`AbacusLike`; `calc` is per-clipper, not a protocol constant). Pays **Vat DAI** (`vat.move` to `vow`). Optional `ClipperCallee.clipperCall`. `redo` resets if `elapsed > tail` or `price/top < cusp`.

`chop`, `hole`, `buf`, `tail`, `cusp`, `chip`, `tip` are **ilk/clipper governor `file` values**. Do not invent a 13% chop or a fixed curve.

This is `HealthState::AuctionOpen` (GUIDE-01). **D14 blocks the adapter** until auction quoting exists. Not `SoftLiquidating` (no continuous band rebalance). Same family as Ajna `kick`/`take`, not Compound V3's reserve-buy.

## Debt assets

**One debt:** Vat internal DAI. `Clipper.take` collects DAI (`rad`). Sky DSS Flash is DAI-only (D09 / 07A). USDS (`0xdc035d45…384F`) is a different ERC-20 — DSS `available(USDS) = 0`. Do not treat USDS as the take currency from this pin.

Collateral = per-ilk `gem` from IlkRegistry. Registry unique gems (20 addresses, 18 symbols from `registry.tokens` — `spDAI` and `G-UNI` each have two gem addresses): WETH, WBTC, wstETH, USDC, USDP, GUSD, SKY, aDAI, cDAI, spDAI, aEthLidoUSDS, UNI-V2, G-UNI, RWA001/002/004/005/009. Ilk set is live on-chain (`count`/`list`); this snapshot is 35 ilks / 20 gems.

## Flash depth (D09 + 07A)

Take currency = DAI `0x6B17…1d0F`. `registry.flash_sources = {}`. 07A @ **26_000_000**:

| debt | Aave V3 `0x87870Bca…` | UniV3 n | UniV4 PM | Morpho | Sky DSS `0x60744434…` |
|---|---|---|---|---|---|
| DAI | 07A untracked → runtime | 92 | runtime | runtime | **yes** `max=5e8` wad; fee 0 |

Aave fee_bps=5 @26M. UniV4/Morpho/Sky fee=0.

## Enumeration (REGISTRY §3 Family A)

Root: IlkRegistry **`0x5a464C28D19848f44199D003BeF5ecc87d090F87`** (`discover.py`).

1. `n = count(); list()` / `list(start,end)` → `info(ilk)` → `(name, symbol, class, dec, gem, pip, join, xlip)`.
2. Clipper per ilk: `Dog.ilks(ilk).clip` (D15: Dog **`0x135954d155898d42c90d2a57824c690e0c7bef1b`**, confirm live). Vat **`0x35D1b3F3D7966A1DFe207aa4514C12a259A0492B`** (07A).
3. Positions: no global urn list. Subscribe `Bark` / `Kick` / `Take` / `Vat` frob. Unsafe test is on-chain `ink/art` vs `spot/rate`.

## Liquidation ABI → 10R-n (D48)

**New family + auction.** Executor first deploy is Aave V3/V4 + Morpho only. When D14 lifts:

```
bark(bytes32 ilk, address urn, address kpr) returns (uint256 id)
take(uint256 id, uint256 amt, uint256 max, address who, bytes data)
redo(uint256 id, address kpr)
getStatus(uint256 id) view returns (bool needsRedo, uint256 price, uint256 lot, uint256 tab)
```

Opens `10R-n`. `_isLiquidatable` = unsafe urn **or** live sale; not Aave HF.
