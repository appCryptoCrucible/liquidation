<!-- mechanism-review protocol=curve-lending repo=curvefi/curve-stablecoin commit=cf1d05fb6bf7c608973cc41786b2e1fd81dc3a6a date=2026-09-20 verdict=declined -->
# Curve lending (LlamaLend) — DECLINED

Source: `curvefi/curve-stablecoin` @ `cf1d05fb6bf7c608973cc41786b2e1fd81dc3a6a` (`master`, 2026-08-20). Product: `curve_stablecoin/lending/LendController.vy` (exports `core.liquidate`). AMM: `curve_stablecoin/AMM.vy`. Core: `curve_stablecoin/controller.vy`.

GUIDE-15 §5 / GUIDE-01 `HealthState::SoftLiquidating`. Not in `registry.json` (never discovered).

## Mechanism — soft primary; hard backstop is not our opportunity

LLAMMA AMM “spreads collateral across price bands and continuously converts it between the borrowed and collateral coins as the oracle price moves, **soft-liquidating loans instead of closing them at a single price**” (`AMM.vy` notice, this pin). Conversion is AMM `exchange` by arbitrageurs, not a searcher `liquidationCall`.

Controller still has a **bad-liquidation** entry when health is already negative:

```
liquidate(_user, _min_x, _frac=1e18, _callbacker=0, _calldata="")
```

`controller.vy`: `assert health_before < 0, "Not enough rekt"` (unless self-approved). Then `AMM.withdraw` of remaining bands; repay leftover debt. `_health`: liquidation starts when `< 0`; comment: “devaluation of collateral doesn't cause liquidation” — bands convert first.

That hard path fires **after** the AMM has already done the work. There is no GUIDE-12 viability-band event of the form HF<1 → repay debt → seize coll + bonus. Modelling this as Aave-class is the failure mode GUIDE-15 names. Decline; do not open 15C/10R-n.

See `docs/mechanism/llamma.md` for the AMM itself (same repo pin).

## Debt assets

Per market: one `borrowed_token` vs one `collateral_token` (`LendController.__init__`). crvUSD mint markets borrow crvUSD; LlamaLend markets borrow the vault’s asset. Not enumerated in this registry. Irrelevant: declined.

## Flash depth

Not surveyed for execution. D09 sources could fund a `liquidate` repay of a flashable borrowed token; that does not create a hard-liq opportunity stream. Skip.

## Enumeration

Not implemented. Would be Controller/Vault factories on Curve deployments. Do not build.

## Liquidation ABI

```
liquidate(address _user, uint256 _min_x, uint256 _frac, address _callbacker, bytes _calldata)
health(address _user, bool _full=False) view returns (int256)
```

New vs Executor Aave/Morpho. **No 10R-n** — declined, no adapter.
