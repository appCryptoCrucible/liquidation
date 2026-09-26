# Liquidation Executor — Security Audit (Client Summary)

**Auditor:** Tucker Sossaman  
**Date:** 26 September 2026  
**Scope:** On-chain flash-loan liquidation executor (`Executor.sol` and libraries)  
**Status:** Not yet deployed

## Bottom line

Do **not** deploy with a hot operator key and standing balances until Critical and High issues are fixed. This is not a clean bill of health.

| Severity | Count | IDs |
|----------|-------|-----|
| Critical | 3 | C-01, C-02, C-03 |
| High | 1 | H-01 |
| Medium | 2 | M-01, M-02 |

## What went wrong (plain language)

1. **C-01** — The profit check only watches WETH. A stolen operator key can push any other token sitting on the contract out through the Uniswap router to an attacker.  
2. **C-02** — Flash-loan “providers” come from the plan. A stolen key can name a fake provider that never lends but still collects the repayment.  
3. **C-03** — Liquidation `market` addresses also come from the plan. A stolen key can approve or send ETH to a fake market (including Compound `isCEther`) and drain inventory.  
4. **H-01** — Even WETH sitting on the contract can be stolen up to the size of that transaction’s profit. Only larger standing WETH is blocked.  
5. **M-01** — Curve swaps have no per-trade minimum out; only the final profit floor protects you.  
6. **M-02** — Gearbox partial liquidate builds an uninitialized `PriceUpdate[]` memory array (waste / risk of bad calldata to Gearbox).

## What is solid

- Callbacks require being inside `execute` and talking to the armed provider.  
- Permissionless `sweep` can only send to the fixed profit sink.  
- UniV3 *swap* pools are CREATE2-checked (flash path is the gap).  
- Standing WETH larger than that tx’s profit still cannot be drained via the router.

## Before mainnet

1. Fix C-01 / C-02 / C-03 / H-01 (router policy + flash provider binding + market/cETH pin + standing WETH accounting).  
2. Keep almost no standing inventory on the executor.  
3. Treat the operator key like production treasury custody.  
4. Set Curve `min_dy` and fix Gearbox `PriceUpdate[]` construction.

Full technical detail: `docs/audit/audit-report.md`.  
PDF: not generated (pandoc available; no LaTeX/WeasyPrint engine on this host). HTML: `docs/audit/audit-report.html`.
