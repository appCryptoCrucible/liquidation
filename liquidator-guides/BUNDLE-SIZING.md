# Bundle sizing

What to change so a transaction is capped by gas, by eight accounts of one collateral/debt pair, and by the marginal profit of the combined swap. The solo viability band stays a pre-filter for the first account of a pair. It is not the rule for the second account, and it is not a cap on the sum.

## What is already true

`Executor._core` liquidates every leg in the flash group, then runs that group's repay swaps (`contracts/src/Executor.sol`). A failed leg is caught. A failed swap reverts the group. Profit conversion to WETH is already one `TAKE_BALANCE` per collateral, after every group (`assemble.rs` `swaps_for_leg`).

`liq-sim` `verify` applies the signed calls on revm in order. Each swap in the transaction executes against the reserves the previous swap left. That is the on-chain order. It does not, by itself, merge five swaps into one.

The off-chain quote does not do that merge. `best_plan` sizes each account against the full book and clips it to the solo band (`profit.rs`). `solve_batch` (`exact.rs`) re-quotes a debt group of two or more legs on a scratch book, one seized amount at a time. `assemble.rs` then emits one exact-out repay swap list per account (`swaps_for_leg`). Five accounts that seize 4.87 ETH are five routes, not one route of 4.87.

`select.rs` `pack` starts a new plan when the plan holds `legs_per_plan` accounts. Ordinary drain sets that to `EXACT_K` (8). SVR sets it to `u8::MAX`, sets `nonce_slots` to 1, and caps gas at `min(header, 12_000_000)` (`drain.rs` `svr_select_cfg`).

## Target

One flash group, one debt asset.

- Liquidation calls stay per account.
- Collateral of the same token is sold once, in one water-fill, for the sum seized. The water-fill may still use several pools. It does not repeat per account.
- Different collaterals in the same debt group are separate routes, quoted in order on the book the previous route left.
- At most eight accounts of one collateral/debt pair in one transaction. The pair is the two tokens, so two protocols seizing WETH against USDC share the count and the route.
- A ninth account of that pair opens the next ordinary nonce. On an SVR hint `nonce_slots` stays 1, so the ninth is not a second bundle on that hint. It waits for the next block.
- Stop adding an account when its marginal debt out, on the book after the pair's current sum, does not cover its repay, its flash fee, and the gas of one more `liquidationCall`. Hop gas is charged only for the first account of that pair. `s_max` is the solo zero-profit size. It is not where the sum stops.
- SVR gas ceiling stays 12,000,000. The sum it checks is `tx.base` once, wrap once per debt group, hop gas once per pair route, and liquidation gas per account.

## Steps

### 1. Aggregate a pair before quoting it

`crates/liq-router/src/select.rs` `pack` / `seal_cascades`, and `crates/liq-router/src/exact.rs` `solve_batch`.

Group the legs of one debt by collateral. Sum `seized`. Call `solve_pair` once on that sum. Run `solve_batch` over the distinct collaterals, each with its summed amount, so a shared hop sees the previous collateral's trade. Delete the per-leg `solve_batch` of five partial amounts of the same token.

The first account of a pair is quoted on the book as the transaction will see it (step 6 for a later nonce). Each added account is the difference `Q(sum + seized) − Q(sum)` from that same route, not a fresh quote of `seized` on the untouched book.

### 2. Admit on marginal gas, not on the solo floor

`crates/liq-router/src/profit.rs` `evaluate` and `delta_net`. `crates/liq-router/src/select.rs` `success_gas`.

`evaluate` drops any size below `band.min_size` and caps every size at `band.max_size`. That is the solo transaction. Keep it only for an account that would be the first of its pair. An added account is in when the marginal quote from step 1 covers `s · (1 + flash fee)` plus the gas price times one liquidation. Do not charge wrap, `tx.base`, or a second hop on it. Do not clip it to `band.max_size`.

`success_gas` today is `wrap + liquidation + hops` on every leg. Split that. The pair's first leg carries wrap (once per debt group), hop gas (once per route), and its liquidation. Every later leg of that pair carries liquidation only. `tx.base` stays once per transaction, in the same sum the 12M cap uses.

`delta_net` has to run on the marginal contribution after step 1, not on the solo contribution. A positive solo `delta_net` is not a reason to keep a leg whose add reduces the bundle.

### 3. Encode one repay route per pair

`crates/liq-router/src/assemble.rs` `swaps_for_leg`, and the group builder that appends those lists.

`_core` already performs every liquidation, then one repay-swap list. Emit that list from the aggregate quote: one water-fill, exact-out shares of the group's pull for the debt that was actually repaid, one sequence of pools. Do not append a second copy of the same pool path for the next account.

`TAKE_BALANCE` profit swaps stay one per collateral. They already are.

A failed liquidation still reverts the group if the remaining collateral cannot fund the exact-out. That is the same revert the current per-account exact-outs produce: `_swap` does not catch. Merging the route does not add that hole. Do not change `Executor.sol` for this step.

Curve stays exact-in. One exact-in of the combined collateral that buys the group's Curve share, not one exact-in per account.

### 4. Count eight per pair, not eight per transaction

`crates/liq-router/src/select.rs` `pack`, `plan_leg_count`, `SelectCfg.legs_per_plan`. `crates/liq-bot/src/drain.rs` `form_select` and `svr_select_cfg`.

Replace the plan-wide `plan_leg_count >= legs_per_plan` seal. In the plan being built, count accounts with the same collateral and the same debt. The ninth of that pair seals the plan and starts the next nonce, then is the first account of that pair there. A different pair keeps filling the current plan.

SVR: leave `nonce_slots = 1` and `header_gas_limit = min(header, 12_000_000)`. Apply the same per-pair count of 8. A pair that is already at 8, or a leg that fails step 2, or a leg that does not fit in the remaining gas, is not a second SVR bundle. The ordinary drain after the auction block sends it.

The 12M comparison uses the gas sum in the target section. `pack` today adds hop gas on every leg and never adds `tx.base`. Both of those counts change in step 2, and the SVR cap has to use the new sum.

`EXACT_K` stops meaning "legs in one bundle". The drain can keep exact-solving `8 * NONCE_SLOTS` candidates only if that bound is raised to the number of pairs a block can actually hold. Otherwise a ninth pair never gets solved. Size `exact_k` to `NONCE_SLOTS` times the most pairs one transaction will carry, and let gas, the per-pair eight, and step 2 be what stop the plan.

### 5. Rebuild the pair at the hinted price

`crates/liq-bot/src/drain.rs` `poll_svr`, `publish_pair_terms`, `ensure_bands`. `crates/liq-bot/src/bands.rs` `compute_one`.

`poll_svr` applies the hint to the engine shadow and emits candidates. `publish_pair_terms` then reads `self.prices`, which was not updated, and `ensure_bands` skips any pair that already has a band. The swap size and `s_min` / `s_max` stay on the pre-hint ratio.

Before `enqueue_svr` selects, write the hinted price into the ratio `coll_per_debt` for every pair that uses that asset, and recompute that pair's band with `compute_one` on the current book. Step 1 quotes the summed collateral at that ratio. The pre-hint band must not cap the backrun.

The transmit does not move pool reserves. The book in that recompute is the current book. Only the oracle ratio changes, which changes how many tokens a given repay seizes.

### 6. Displace the book across nonces that land together

`crates/liq-router/src/select.rs` `pack`, after a plan is sealed.

Ordinary drain sends up to `NONCE_SLOTS` plans into the same block. Each plan currently calls `solve_batch` on the original book. After sealing a plan, apply its aggregate routes to a scratch book and quote the next plan on that book. Step 2 then sees the residual depth.

SVR is one plan. This step does not run on the hint. It runs on the bundles from step 4 that share a block.

### 7. Simulation checks the merged route

`crates/liq-sim/src/verify.rs` already runs the transaction. No second price walker.

The check to add is that the plan submitted to `verify` contains one repay route per collateral in the group, and that the simulated debt out of that route matches the aggregate `solve_pair` within the existing min-out tolerance. A fixture with five accounts of one pair, seized amounts summing to a known size, asserts one swap sequence and the single-swap output, not five times the one-account output.
