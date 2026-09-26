# Intake — lending liquidation adapters (Aave V4, Euler, Silo, Liquity V2, Fluid T1, Gearbox V3, Compound V2)

## Aave V4 Spoke
- `liquidationCall(collateralReserveId, debtReserveId, user, debtToCover, receiveShares)`.
- Guard: `getUserAccountData.healthFactor < 1e18`.
- Reserve ids from plan tail — no on-chain address→id check in Executor; mismatch bounded by profit guard / revert.
- Clamps debtToCover; approve must be zeroed after.

## Euler V2 (EVK + EVC)
- Liquidate on **debt vault**; collateral is vault shares; redeem underlying after.
- `checkLiquidation` returns (0,0) when not liquidatable; HF==1 liquidatable.
- Batch: enableController → enableCollateral → liquidate → repay(max) → disableController → redeem.
- Residual debt must not remain (disableController reverts on outstanding debt).

## Silo V2
- Call **hook receiver** `IPartialLiquidation`, not Silo ERC-4626.
- `maxLiquidation` → debtToRepay==0 skip; `sTokenRequired` → Executor skips (no sToken redeem path).
- `liquidationCall(..., receiveSToken=false)`.

## Liquity V2
- `batchLiquidateTroves(uint256[])`; Stability Pool is counterparty — no token repay from liquidator.
- Status active=1 / zombie=4. Gas compensation may be WETH + collateral; wrap any native ETH delta.

## Fluid T1
- `liquidate(debtAmt, colPerUnitDebt 1e18, to, absorb=true)`.
- T2/T3/T4 different ABI — encoder must not route non-T1. No HF view.

## Gearbox V3
- Facade entry; approvals to **credit manager** not facade.
- Mode 0: `partiallyLiquidateCreditAccount` with minSeized.
- Mode 1: full `liquidateCreditAccount` + multicall addCollateral/withdrawCollateral; Executor enforces minSeized after.

## Compound V2
- Official Unitroller; market = debt cToken; seize lands as cTokens → redeem.
- CEther path: unwrap WETH, payable liquidateBorrow, wrap leftover ETH.
- Error codes: liquidateBorrow may return non-zero without revert.
