<!-- mechanism-review protocol=llamma repo=curvefi/curve-stablecoin commit=cf1d05fb6bf7c608973cc41786b2e1fd81dc3a6a date=2026-09-20 verdict=declined -->
# LLAMMA — DECLINED (`SoftLiquidating`)

Source: `curvefi/curve-stablecoin` @ `cf1d05fb6bf7c608973cc41786b2e1fd81dc3a6a` (`master`, 2026-08-20). `curve_stablecoin/AMM.vy` title: “LlamaLend V2 AMM”. Shared by crvUSD mint and LlamaLend v1/v2 lend markets (this pin’s AMM notice).

GUIDE-01: `HealthState::SoftLiquidating` — “the position rebalances continuously and is never liquidatable.” GUIDE-15 §5: decline on purpose.

## Mechanism — soft (continuous band conversion)

Collateral sits in discrete price bands (`bands_x` / `bands_y`). As `p_oracle` crosses a user’s range, the AMM curve converts y (coll) ↔ x (debt) via ordinary swaps. Arbitrageurs, not a liquidator role, do that conversion. Fees/slippage on conversion do not unwind if price recovers.

Controller `liquidate` is a **hard backstop** after health `< 0` (see `docs/mechanism/curve.md`). It is not the GUIDE-01 liquidatable state. Quote must never emit `Liquidatable` for this family (conformance check 10).

## Debt assets

Per AMM: `borrowed_token` (x) vs `collateral_token` (y). One pair per market. Not in registry.

## Flash depth

N/A for this bot. Soft conversion is DEX flow (D08 allows Curve as a **swap** venue — that is StableSwap/NG routing, not LLAMMA lending). Do not confuse D08 Curve-swap-in with Curve-lend-in.

## Enumeration

Not required. crvUSD ControllerFactory / LlamaLend vault factories exist on Curve docs; unused.

## Liquidation ABI

No adapter. AMM has `exchange` / `withdraw` (controller-only). Controller `liquidate` unused here. **No 10R-n.**
