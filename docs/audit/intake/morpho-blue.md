# Intake — Morpho Blue (slug: morpho-blue)

**Sources:** https://docs.morpho.org/developers/contracts/blue/ ; morpho-org/morpho-blue @ pin referenced in Interfaces.sol

## Flash loan
- `flashLoan(token, assets, data)` transfers assets to caller, then `onMorphoFlashLoan(assets, data)`, then `safeTransferFrom` pull of `assets` (zero fee).
- Integrator must approve Morpho for exact `assets` before return.
- Auth: Morpho only calls caller; Executor must require `msg.sender == Morpho` (via T_EXPECTED_CALLER).

## Liquidate
- `liquidate(marketParams, borrower, seizedAssets, repaidShares, data)` — exactly one of seized/repaidShares zero.
- Internal `_isHealthy` guard; healthy → revert HEALTHY_POSITION.
- Shares math: virtual shares/assets (1e6 / 1); Executor converts assets→shares with toSharesDown and caps at borrower shares.
- `idToMarketParams` must match loan/collateral tokens (Executor LegMismatch).

## Integrator responsibilities
1. Callback auth + arm expected Morpho address
2. Approve pull after callback
3. Market Id ↔ tokens consistency
4. Share conversion vs approval (toAssetsUp ≤ approved)
