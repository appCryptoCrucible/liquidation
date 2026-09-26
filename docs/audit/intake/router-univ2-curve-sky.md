# Intake — UniV2 / Curve / Sky DSS / UniV4 flash (combined)

## Uniswap V2 / Sushi
- Pair CREATE2 from factory + init hash; 0.30% fee (`997/1000`).
- Exact-out: amountIn = (rIn * amountOut * 1000) / ((rOut - amountOut) * 997) + 1.
- No flash callback on empty data swap path used by Executor.

## Curve StableSwap plain
- MetaRegistry `is_registered(pool)`; `coins(i)/coins(j)` must match tokenIn/Out.
- `exchange(i,j,dx,min_dy)` — Executor uses min_dy=0; relies on plan minProfit.
- No native exact-out.

## Sky DSS Flash (ERC-3156)
- Mainnet Flash `0x60744434d6339a6B27d73d9Eda62b6F66a0a04FA`; DAI only.
- Callback `onFlashLoan(initiator, token, amount, fee, data)` must return ERC-3156 magic; approve amount+fee.
- Executor checks initiator == address(this).

## Uniswap V4 PoolManager unlock
- `unlock` → `unlockCallback`; `take` borrow; `sync`+transfer+`settle` repay; zero fee; deltas must net zero.
- flashSource must be PoolManager; Executor arms that address.
