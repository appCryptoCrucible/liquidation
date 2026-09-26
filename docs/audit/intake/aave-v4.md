# Intake — Aave V4 Spoke

Pin: aave/aave-v4 @ 40232a0a (Interfaces.sol).

- Reserves by `reserveId` not underlying; plan tail carries collId|debtId.
- `getUserAccountData.healthFactor`; liquidate with `receiveShares=false`.
- Protocol clamps debtToCover to target-HF max; integrator must zero approval after.
- No on-chain address→id view in Executor — encoder binds ids.
