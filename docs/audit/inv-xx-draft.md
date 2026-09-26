# INV-XX draft (manual-review to validate/extend)

| ID | Invariant | Enforcement |
|----|-----------|-------------|
| INV-01 | Callbacks only when `T_ENTERED!=0` and `msg.sender==T_EXPECTED_CALLER` | `_checkCallback` |
| INV-02 | Aave/Sky flash initiator == address(this) | executeOperation / onFlashLoan |
| INV-03 | UniV3 swap callback caller == CREATE2(factory, tokens, fee) and T_SWAPPING | uniswapV3SwapCallback |
| INV-04 | UniV3/V2 pair addresses match CREATE2 | _swapV2 / swap callback |
| INV-05 | Curve pool registered and coins(i/j) match | _swapCurve |
| INV-06 | Router target ∈ {ROUTER_A, ROUTER_B} | _swapLeg |
| INV-07 | Approvals zeroed after liquidate/swap (success or fail) | adapters / _swapLeg |
| INV-08 | WETH standing cannot decrease across execute (gross underflow) | execute gross math |
| INV-09 | Non-WETH standing cannot leave except sweep→PROFIT_SINK | **BROKEN** — router L_TAKE_BALANCE (DRAFT-01) |
| INV-10 | Flash repay paid only to real lender | **BROKEN** for plan flashSource (DRAFT-02) |
| INV-11 | Sweep destination == PROFIT_SINK | _sweep |
| INV-12 | AllLegsFailed if filled==0 | _core |
| INV-13 | Plan fully bounds-checked before external calls | PlanDecoder.header |
| INV-14 | minProfit checked on keep after bid | execute |
| INV-15 | No persistent storage / no proxy upgrade | architecture |
