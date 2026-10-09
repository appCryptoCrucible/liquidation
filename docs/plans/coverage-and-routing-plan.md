# Coverage and routing: step-by-step plan

Status as of 2026-10-05. Written after the historical replay of the 29 liquidations in blocks 26,098,020–26,118,019 (`crates/liq-replay/tests/historical_liquidations.rs`). Nothing here is built yet except where a step says "done".

The goal: **every position on a protocol we have an adapter for is one we can detect, price, fund and exit**, and the exit is the best one the chain offers, through any number of pools. Phases 1 to 3 close coverage. Phase 4 builds the router that makes coverage general and is the base for arbitrage later.

Each phase lists why it matters, what is true today, the steps in order, how we know it is done, and any decisions for you. Sizes are rough: **S** is under a day, **M** is a few days, **L** is a week or more.

## Recommended order

| # | Phase | Size | Depends on |
|---|---|---|---|
| 0 | Land the replay work and make replays fast | S | — |
| 1 | Close coverage gaps on the protocols we have adapters for | L | 0 |
| 2 | Aave V2 adapter, tokens and pools | M | 0 |
| 3 | Aave V4 check (build only what the check finds missing) | S, or M if gaps | 0 |
| 4 | The token graph and multi-hop router | L | 1 for its inputs; can start in parallel |

Phases 1, 2 and 3 are independent of each other. Phase 4's search code does not depend on them, but its value does: a graph only routes over edges that exist.

## What the replay found

29 events. How each ended:

| Outcome | Events | Which |
|---|---|---|
| Compared with the real liquidator | 4 | Aave V3 Core: 26098187, 26103141, 26106270, 26106490 |
| Market not tracked | 16 | 8 Euler, 6 Compound-family, 2 Aave-family |
| Detected, correctly skipped as dust | 5 | 1 Aave, 3 Compound, 1 Morpho |
| Missed: crossed by interest, judged a block late | 2 | Compound cUSDC 26113835, Morpho 26104570 |
| Missed: collateral has no exit | 2 | Aave PT-USDe 26112137, Morpho `CTOKEN1` 26117092 |

The largest single result in the window is a miss: Morpho 26117092 paid the real liquidator **0.318 ETH with nothing paid to the builder**. We detected it (health 0.805 against the singleton's 0.809 before interest), 28 flash sources held the debt, and the collateral `0x387d46494f6dc5c6b26abc20ae40623ac041dd92` had no pool and no unwrap.

Where coverage stands in the committed config, against the registry (`registry/registry.json` at block 26,015,175):

| Protocol | In the registry | Bound by the bot | Gap |
|---|---|---|---|
| Aave V3 | 3 pools | 3 pools, 67 assets | forks not bound |
| Spark | 1 pool | 1 pool, 20 assets | none known |
| Aave V4 | 53 spokes, 4 hubs | 13 lending spokes | see Phase 3 |
| Morpho Blue | 1,782 markets | 1,620 priced | 162 unpriced |
| Euler V2 | 884 vaults | 26 admitted, 0 oracles pinned | nothing can be sized |
| Compound V2 family | 266 Comptrollers | 1 Comptroller, 18 cTokens | 265 forks |
| Silo V2 | 252 silos | 126 pairs, 73 assets | pairs whose oracle reverts (16 sides) |
| Fluid | read from the factory at bind | all vaults | exits per token |
| Gearbox V3 | read from the address provider at bind | all v3.1 managers | exits per token |
| Liquity V2 | 3 branches | 3 branches | none known |
| Ajna | 235 pools | no adapter | out of this plan |
| Aave V2 | 1 pool (config-defined) | 1 pool, 36 reserves | AMPL (rebasing) |

Exits, over the 1,084 registry tokens:

| | Tokens |
|---|---|
| Have at least one pool | 331 |
| No pool, but an unwrap (78 Pendle PT, 59 ERC-4626, 12 Pendle market, 5 Curve LP) | 152 |
| No exit at all | 601 |
| Have a direct WETH pool | 182 |

The router today exits direct or through WETH, never more than two hops, and WETH is the only hub.

---

## Phase 0: Land the replay work and make replays fast (S)

**Why.** Every later step is accepted by replaying an event and seeing "no job" turn into a job. Today an uncached block takes 30 to 40 minutes on the free RPC, which makes that loop unusable.

**True today (local, not committed).**
- `all_liquidations_through_our_searcher` replays every captured event; Morpho and Compound seed through the bot's own ingest (`tests/historical/seed.rs`: `morpho`, `compound`), with Aave Core seeded beside them as the USD getter.
- Two bot fixes in `crates/liq-bot/src/drain.rs`: ratio prices use the batch's own USD prices on the first batch; a collateral only its own market prices is sized at that market's oracle price.
- The harness filter keeps a token's unwrap targets and its LP pool (`tests/historical/bot.rs`).
- Not yet run on these changes: the workspace test suite and clippy.

**Steps.**
1. Run `cargo test --workspace --exclude liq-replay`, `cargo clippy --workspace --all-targets -- -D warnings` (and with `--features liq-bot/alloc-assert`), `cargo fmt --check`. Fix what they find.
2. Add a regression test for each drain fix in `drain.rs`'s own test module (a first batch holding a Morpho read and its numeraire's USD read; a quote whose collateral has only an overlay price).
3. Make replays fast. The slow part is the bot's own startup seeding (2,509 flash reads, tick ranges for every kept pool) fetched one request at a time from a rate-limited endpoint. In order of preference:
   - Batch the harness's upstream reads the way production batches them (multicall), so one block is tens of requests, not thousands.
   - Seed only what the event needs: flash sources holding the event's debt token, pools on the event's tokens and their unwrap targets.
   - Point `MAINNET_RPC_URL` at an archive node without rate limits when one is available.
4. Extend the fixture: scan a longer window (three months) for the same four event topics plus Aave V2's and the Compound-fork indexed variant, and record for each event which layer blocked us (untracked, unpriced, no exit, dust, late). This is the ranking that orders the work inside Phase 1. Store it beside `liquidations.json` with the same provenance note.
5. Commit, per the house rules (explicit paths, fetch, push `main`).

**Done when.** The suite is green on the local changes; a previously uncached event replays in under two minutes; the three-month fixture exists with a blocked-by column.

---

## Phase 1: Close coverage gaps on the protocols we have adapters for (L)

Four layers, in the order a position passes through them. A position is covered only when all four hold.

### 1A. Detection: judge health at the block we would land in (S–M)

**Why.** Two events were healthy at the previous block and liquidatable in their own block through interest alone. The winner was first in the block; we would have seen it one block later. This applies to every protocol with accruing debt.

**True today.**
- The drain evaluates at the tip's timestamp.
- Morpho's `health` accrues from the market's `last_update` to the position's timestamp. Compound's does not accrue at all, and `compound-v2/src/solve.rs::time_to_cross` returns `None`.
- The engine has an interest-drift trigger and a time-to-cross heap; it has not been checked against these two events.

**Steps.**
1. Reproduce in the harness: replay 26113835 (Compound) and 26104570 (Morpho) with the health evaluated at the tip's timestamp plus one slot. Oracle: the protocol's own view on the state at block N (`getAccountLiquidity`; Morpho's `_isHealthy` inputs after `accrueInterest`). Expect both to flip to liquidatable.
2. Decide where the projection lives. Preferred: the engine folds every Hot position at `tip.timestamp + 12` (the next slot), so a position that crosses in the next block is emitted now. The job's simulation already runs in the next block.
3. Compound: implement accrual in `health` (borrow index and total borrows projected with the cToken's borrow rate per block, as `accrueInterest` does; pin `a3214f67`) and a real `time_to_cross`. Read the rate model's current rate through the state reader; do not reimplement every rate model.
4. Audit the other adapters for the same question: does `health(pos @ t+12)` equal the protocol's answer at block N+1 with no events in between? Aave V3 and V4 (index projection), Euler (accumulator), Silo, Fluid (its own per-block read), Gearbox, Liquity (none accrue the same way; write down why).
5. Guard against false positives: a job built on projected interest must still pass simulation in the next block, which it does by construction.

**Done when.** Both events replay to a job (or to "detected, dust" with the reason printed), and each adapter has a conformance test comparing projected health with the protocol's view one block later on real state.

**Built (2026-10-07).**
- Step 2: the drain judges health, quotes and sizes at `tip + 12` (`drain::eval_ts`).
- Step 3: Compound's `health` projects each market as `accrueInterest` would leave it, and `time_to_cross` bisects on the clock over ten years, as Morpho's does (`tests/time_to_cross.rs`: a rate set to cross at block 1,001 crosses at `T0 + 1001 · 12`, not a second before).
- Step 1, replayed 2026-10-07 (graph pools, graph routing on): **Morpho 26104570 is now a job.** Our health one slot on is 1.000000 while the market's stored totals still say 1.000227; the bundle succeeds and captures 0.02524 ETH against the winner's 0.02518 in all (0.00081 kept, 0.02437 to the builder). **Compound 26113835 is detected** (our health one slot on 1.000000; the Comptroller at N−1 shows no shortfall) and is not profitable at its measured gas: the simulation stops at the Executor's `gross − gas` check (`Panic(0x11)`); the winner kept nothing (0.00271 ETH, all to the builder).
- Step 4, the audit: every adapter but Fluid has a `time_to_cross` from its own accrual (Aave V3/V2/Spark, Aave V4, Compound, Euler, Gearbox, Liquity, Morpho, Silo). Fluid's health is the vault's own per-block read, so it has nothing to project; it returns `None` by design.

### 1B. Markets: track every market of every adapter (L)

**Why.** 16 of 29 events were on markets the bot does not follow. Mostly configuration, not code.

**Decided: no admission floor and no hand list. Every market is tracked, tiered by viability.** (Decision 2026-10-05.)

| Tier | What the bot does | Cost per market |
|---|---|---|
| **Cold** | Folds the market's logs into state (they arrive with the block anyway). Prices it from its own oracle at current, on a slow cadence (every N blocks; N set from measured read cost). Recomputes its viability band on that cadence. No per-block health evaluation. | log fold, state memory, one price read per N blocks |
| **Warm / Hot** | What admitted markets get today: per-block prices, the engine's health bands, candidates, jobs. | as today |

- **Promotion** is early, not at liquidation: a cold market is promoted when its largest position's debt clears the band's lower edge at a low gas price (the cheapest tenth of recent blocks) **and** that position's health is inside the engine's outermost band. Promotion runs the adapter's resync reads for its positions, so it is warm before it can cross.
- **Demotion** when no position clears the band for a sustained period (hours, not blocks), so markets do not flap.
- **The band for a cold market** uses the same calculation as a warm one: the market's own oracle price, the exit routes the router has (the warm route table today, the zero-size graph table after Phase 4), and gas. Where an exit is missing, the band says so, and the market is listed for 1D instead of being dropped.
- **Oracles.** A cold market is priced from the oracle its protocol itself consults, read at current. For permissionless protocols (Morpho, Euler, Silo), the oracle is checked at promotion (that it is the one the market's own params name, and that its answer is in line with other sources for the same token), not at admission. A failed check keeps the market cold and logs why.
- **Measure the cost** as markets move from the hand list to cold tracking: startup reads, state size, log-fold time per block, cold price reads per block. These are the numbers that set N and say whether anything needs to be dropped. Nothing is dropped by a floor in advance.

**Euler V2 (M).** 26 of 884 vaults admitted; no oracle pinned, so even admitted vaults are unpriced.
1. Track every vault, cold by default (above), replacing the hand list of 26. Generate `euler-v2.toml` with a script like `gen_morpho_toml.py`, reading the factory's proxy list and each vault's `asset()`, `oracle()`, `unitOfAccount()`, LTV list and totals at a pin block. Better still, have the adapter discover vaults from the factory at bind, as Fluid and Gearbox discover theirs, so the TOML carries no list at all.
2. Pricing: Euler vaults price through an `EulerRouter` per vault. Pin each admitted vault's router as a price source and read `getQuote` through the protocol price reader, as Morpho reads each market's oracle. Per `verify-against-protocol-docs`: check the adapter's math against the deployed EVK source, not only the pinned repo.
3. Share tokens: collateral vault shares are not registry tokens. Intern them (Phase 0 of `open-items.md` settled append-only ids) with an ERC-4626 unwrap into the vault's asset.
   - **Done 2026-10-06** (`tools/registry/gen_euler_toml.py`). 603 share tokens were interned (ids 1084..=1686). The TOML now carries all 884 registry vaults, 919 assets and 190 oracle pins, and vaults in a token unit of account (83: WETH, USDC, WBTC, …) are priced as ratio reads (`RATIO_READ_TAG`), which the bot restates in USD. Left out and reported by the generator: 5 factory proxies newer than the registry (interning them renumbers the markets after them, so it waits for a registry refresh), and 16 LTV collaterals that are not proxies of this factory.
   - The unwraps themselves come from `discover_unwraps.py --kinds erc4626`, which proves a real holder's redeem before writing one.
   - **Open, your call: halt-class logs now arrive from 884 vaults instead of 26.** `docs/coverage/euler-v2.md` classes `GovSetGovernorAdmin` (an authority change) and the proxy events as `halt`. A halt is an `apply_block` error, so one governor handover on any followed vault stops ingest for the whole bot. A handover changes no vault math: every later parameter change still arrives as its own `GovSet*` event. Options:
     - keep the halt, accepting more stoppages;
     - fold `GovSetGovernorAdmin` as a no-op;
     - scope the halt to that vault's market (`HaltScope::Market`), which needs the halt path to carry a scope.

     Recommendation: scope it to the market. (A 100k-block count of these events across all vaults timed out twice on the shared RPC and is still owed.)
   - **Done: an EVK share's redeem is capped by the vault's cash.** A collateral vault that is also lent out pays only up to its `cash()` (EVK `E_InsufficientCash`).
     - Registry: an unwrap carries `"cash_capped": true`, which `discover_unwraps.py` writes for every EVK proxy. 385 entries.
     - Seeding: `pool_seed` reads `cash()` with the rate each block. A capped vault whose cash fails to read is not routed.
     - Router: `UnwrapRate::Linear::max_into` makes `convert` return `InsufficientLiquidity` above the cash, which the band scan treats as unabsorbable at that size.
     - Test: `a_cash_capped_unwrap_refuses_more_than_the_vault_holds`.
   - **Unwrap gate for EVK shares.** EVK collateral holders are borrowers, and the EVC refuses their redeem, so no holder can prove the unwrap. For a factory proxy, `discover_unwraps.py` gates by source instead: `OP_REDEEM` (`1 << 3`) must not be in `hookConfig()`'s ops. Result: 384 of the 603 new shares have an unwrap; 21 are refused (redeem hooked); the rest unwrap into an asset with no routed pool.
4. Replay seeder for Euler (`seed::euler`): `GovSetLTV`, `VaultStatus`, the EVC collateral and controller statuses, then the adapter's resync reads for the borrower.
5. Acceptance: the 8 Euler events replay to a verdict other than "out of scope". Expect dust on most; the point is that they are judged.
   **Passed 2026-10-06** (`seed::euler`).
   - Health: all 8 are judged, and on every one our health equals the vault's own `accountLiquidity(account, true)` to six decimals (0.926725–0.999977).
   - Verdict: all 8 are NoJob, which is correct on the evidence. The winners captured between −0.00143 and +0.00012 ETH, so two were losses and the rest dust.

**Compound V2 family (M).** 1 of 266 Comptrollers bound.
1. Classify the 266: genuine V2 ABI (unindexed `LiquidateBorrow`, `liquidateBorrow(borrower, repay, cTokenCollateral)`), versus look-alikes with indexed events or different seize rules. The six out-of-scope events include both kinds: `0x2c38134a…` (V2-style, unbound) and `0x13eb80fd…`, `0xc82780db…` (indexed variant).
2. Bind every genuine fork, cold by default. The adapter already supports several forks (`[[forks]]`); the per-fork facts (close factor, incentive, oracle, each cToken's underlying, protocol seize share) are read live at bind (`assert_live_registry`). Generate the TOML from the registry.
3. The indexed-event family: identify the protocol from the deployed source. If it is a liquidation ABI we already have a leg for, bind it; if not, it is a new adapter and goes on the list at the end of this phase, not into this one.
4. The Executor's Compound leg validates `(cDebt, cCollateral, isCEther)` pins; extend `compound_validate_pins` to every bound fork.
5. Acceptance: 26106892 replays to a verdict; the classification table is committed under `docs/coverage/`.

**Done 2026-10-06** (`tools/registry/gen_compound_toml.py`, `docs/coverage/compound-forks.md`).
- **Bound: 207 forks** (78 plain V2, 128 Rari Fuse, 1 Moma Lending Pool), 1,481 cTokens, all binding live (re-run 2026-10-06 with per-cToken classification; see `docs/coverage/compound-forks.md`).
- **Variants:** `ForkVariant::{Compound, Fuse, Moma}`.
  - Fuse (`CToken` `0x67db14e7…`, Sourcify) and Moma (`MToken` `0x1d0fcc81…`) keep two fee accumulators beside the reserves, accrued from each `accrueInterest` and subtracted in the exchange rate.
  - Both are read each block (`totalFuseFees`/`totalAdminFees` and their rates; `totalFees`/`totalMomaFees` and theirs), because a Fuse fee withdrawal emits no event.
  - A fork's collateral fails closed until its fees are read.
- **Per-fork binding:**
  - A fork whose live check fails is logged and left out, and a cToken whose check fails drops only itself; the others bind.
  - The close-factor check is "usable" (`0 < cf ≤ 1e18`): Fuse pools run at 1e18, and Compound's 0.05–0.9 were the official Comptroller's setter limits.
  - Oracles may be shared (Rari's master oracle).
- **Replays (`LIQ_HISTORY_EVENTS`, events drawn from the scan cache):**
  - Fuse pools 6 and 18: our health agrees with the Comptroller's `getAccountLiquidity` on all four events (three short; one where we project interest past the stored index and the winner liquidated next block).
  - Capyfi: our health agrees on both.
  - All six ended with no job: captures of 0.0002–0.0025 ETH, and collateral without an exit or a price.
- **The 27 left out for cToken views: done 2026-10-06.**
  - Cream v1, zen, Inverse and pepe are bound, with three frozen markets (`accrueInterest()` reverts) counted in health but given no leg. DeFiPie (30 pTokens, `controller()`, five-field accrual) and `0x39313c37…` are bound too.
  - The copycats and the forks with a zero close factor or incentive have nothing to liquidate.
  - Every pinned cToken's events are proven from its code. That dropped 59 cTokens bound before whose events the adapter never saw: NFT-collateral markets, and forks whose `Mint`/`Borrow` carry extra fields.
  - An account that entered a market outside the config now fails closed.
  - **Found on the way:** the official Compound's 2019 cTokens (cETH, cUSDC and five more) emit a three-field `AccrueInterest` the adapter did not follow. It folds all three forms now.
- **Still left out:** 9 non-V2, 5 per-second. Compound halts are per fork.

**Silo V2: done 2026-10-06.**
- **Bound: all 126 pairs (252 silos, 73 tokens),** from `tools/registry/gen_silo_toml.py` (`getSilos`/`getConfig` per `SiloConfig`).
  - Each pair's market id is its slot-1 silo's registry id, so the three pairs bound before keep theirs; the generator asserts it.
  - Every pair comes from the two configured factories.
- **Prices (`Protocol::price_reads`):** one read per pair, an `aggregate3` of both sides' solvency-oracle `quote` of a million whole tokens, so the two answers come from one block. A side without an oracle is its raw amount, as `getPositionValues` values it.
  - The quote unit is often a "Silo Virtual Asset" (8 decimals, no price of its own). It is a per-pair abstract unit, not USD: under one of them WBTC and USDC both quote about 1.
  - So the read is a ratio, restated in USD through whichever side has a USD price. `to_usd` now anchors on any priced asset of the read when the numeraire has none, which also covers Morpho markets whose loan token has no feed.
  - Every live oracle quotes linearly to 1e-6 over that range (survey at 26132977). 16 sides revert (`InvalidPrice` and similar), and those pairs fail closed.
- **Accrual (`Protocol::state_reads`):** per borrowed silo, each block reads `getDebtAssets()`, `getCollateralAssets()` (both with interest), the storage totals and `utilizationData()`.
  - The totals are set to the chain's at each head.
  - Health grows the debt at a measured rate, and the collateral by that interest net of `daoFee + deployerFee`, as `getCollateralAmountsWithInterest` does.
  - The rate is the debt's growth from a base point, taken once it has grown 100 units (so it resolves to a percent). A busy silo is measured block to block; a quiet one over as many blocks as it needs.
  - A long average since the last accrual is not the rate now: on two stale silos it was half and eighteen times the real rate, because Silo's rate model moves while utilization sits off target.
  - `AccruedInterest` no longer adds its amount once a rate is known: the projection already holds it.
  - **Live oracle** (`live_silo_totals_project_to_what_the_silo_reports_later`): ten consecutive blocks of reads, projected 50 blocks on, against each silo's own `getDebtAssets()`/`getCollateralAssets()` there. 17 silos were measured, with the worst error 0.54% of the span's interest. 25 had grown under 100 units over the reads and stay at the chain's exact totals each block.
- **Halts are per pair:** a halt-class log from a pair's silo, hook, share token or config (or a hook swap through `NewSiloHook`) marks both rows paused. Only a factory's still stops the protocol (decision 8).
- **Replay case: done 2026-10-07** (`seed::silo`). The pair is listed by its `NewSilo`, the borrower's protected, collateral and debt shares arrive as `DepositProtected` / `Deposit` / `Borrow` with a `CollateralTypeChanged` on its collateral silo, and each silo's totals are written from `getTotalAssetsStorage` (checked against `getCollateralAndDebtTotalsStorage`) and the share tokens' `totalSupply`. On 26107908 our health is 0.999970 and the silo's own `isSolvent` is false; no job, correctly: the seized collateral (asset 717) has no route at all, and the winner lost 0.0001 ETH.

**Morpho Blue: the 7 tokens, checked 2026-10-07; none added.** Three have no code on mainnet (Base-chain addresses: `0x833589…2913` is Base USDC, `0x1f32b1…` Base wstETH); their markets hold no debt. Four share one 4,810-byte contract that reverts `Paused()` on every call, `symbol()` included, so a liquidation's collateral transfer would revert; their three markets with debt owe about $1,331 of USDC in all. Nothing to bind until those tokens unpause.

**Morpho Blue (S).** Recounted 2026-10-06 (`gen_morpho_toml.py --dry-run`): the "162 unpriced" was a miscount. 1782 registry markets map to 1620 pins because pins are deduplicated by `(oracle, collateral, loan)`: 101 markets share a triple with another market and differ only in LLTV or rate model, and they are priced. Unpinned: 54 idle (no oracle or no collateral; nothing to liquidate) and 7 with a token outside the registry. Add those 7 tokens. The live gap is markets created after the registry's block, whose new oracles have no pin. They belong to the cold tier: the oracle is checked at promotion, against the market's own `idToMarketParams`.

**Aave V3 forks (S).** `0xd55b0b38…` emitted an Aave-V3-shaped `LiquidationCall`. Identify it (deployed source), and if it is an unmodified V3 pool, add it to `aave-v3.toml` with its own configurator and oracle. One adapter instance per pool already works (Core, Prime, EtherFi).
- **Identified 2026-10-07: Phiat ("Phiat genesis market"), an Aave V2 `LendingPool` recompiled at solc 0.7.6, not unmodified.** Against Aave V2's deployed source (Sourcify, token-level diff): a blacklist (`repayAllAndBlacklist`, `isBlacklisted`, `Blacklist` event), per-reserve deposit limits (`ReserveLimits`), an aToken `handleRepayment` hook, its own incentives controller, and the older rate-strategy signature. It needs its own adapter; with one liquidation of $154 in the 20k-block window it is listed for later, not bound.

**Fluid, Gearbox, Liquity (S).** These discover their markets from chain at bind. Verify the count bound at startup against the factory's count, and log the difference.
- **Done 2026-10-07.** Fluid logs `totalVaults()` against vaults bound and quotable; Gearbox refuses a v3.0 register whose manager count differs from `expected_managers` and logs the v3.1 managers it skips; Liquity now reads `CollateralRegistry.totalCollaterals()` (through a branch's `AddressesRegistry.collateralRegistry()`) and logs it against the branches configured (3 = 3 at head).

**A standing check.** Add a daily job (`tools/registry/daily_refresh.py` already exists) that reports, per protocol: markets on chain, markets tracked, cold and warm counts, markets priced, and any market on chain the bot does not track (target: none). Coverage that is not measured decays.
- **Built 2026-10-07 (not yet run end to end).** The daily report has a coverage table per family: markets discovery finds on chain, markets in the registry, those not tracked and those gone. Which markets bind and which are priced is the bot's own startup log (below, 1C).

### 1C. Pricing: every tracked market priced from its own oracle (M)

**Why.** A tracked market with no price is invisible. Euler is entirely in this state.

**Steps.**
1. Per adapter, every admitted market has a price read (`Protocol::price_reads`) against the oracle the protocol itself consults. Morpho, Aave, Compound, Fluid, Gearbox, Euler and Silo do.
2. Sizing falls back to the market's own oracle price when neither a canonical feed nor a USD getter covers the collateral (done locally, Phase 0). Extend the same fallback to `publish_per_eth` so the band can be computed for such a token.
3. A startup report: for each admitted market, priced or not, and why not. Zero unpriced admitted markets is the target.

**Built (2026-10-07).**
- Step 2: `publish_per_eth` falls back, for a token no canonical feed or USD getter prices, to the median of the markets that price it (`ProtocolPriceBook::usd_or_markets`, at most 15 markets): permissionless markets name any oracle, so no one market sets the gas conversion.
- Step 3: after the first price batch the drain logs, per protocol, the markets with an oracle read, how many are priced, and the rest by reason (read failed, or a ratio read with no USD anchor), with the first ten ids of each (`protocol_prices::coverage`).

### 1D. Exits: every collateral has a way out (L, ongoing)

**Why.** Two events, including the largest. 601 registry tokens have no edge.

**Steps.**
1. **The 0.318 ETH event: traced, and closed as a one-off (2026-10-06).** The winner seized the whole supply of `CTOKEN1` (`totalSupply` = 1e18) and called `convert(uint256,address,uint256)` (`0xc09ee636`) on `0xbb3f7bbf…`. That is a UUPS proxy (implementation `0x77586f65…`) with an owner-set `exchangeRate()` / `updateExchangeRate(uint256)`, owned by the same EOA (`0xB9b2789B…`) as the token. The converter held exactly 3,730,000 sats of cbBTC at N−1, the redemption value of the whole supply. The winner then took cbBTC→USDC on V3 `0x45482…`, repaid 2,275.9 USDC, and swapped the remaining 858.5 USDC to 0.318 WETH on the 0.05% pool. Afterwards the converter holds the supply and no cbBTC, so the market cannot pay out again. No adapter is worth building for it. What generalizes: an owner-funded converter has finite, readable liquidity. If such converters recur in the scan, they are an unwrap kind capped at the converter's balance.
2. **PT-USDe on Aave (26112137).** 104 of 194 registry PTs have no unwrap (`open-items.md`). Matured PTs redeem through their YT; live ones sell on their market. Run `discover_unwraps.py` over all of them and record why each remaining one fails.
   **Run 2026-10-07** (`discover_unwraps.py --kinds pendle_pt,pendle_market`): 9 new PT unwraps, each proved by a real holder's redemption through the Executor's path paying exactly the bot's quote (PT unwraps 78 → 87), applied with `--tokens` so no existing entry was rebuilt. The rest: 40 expired PTs redeem into an SY output no route reaches ("no routed SY output"), 2 live ones sell and redeem into such an output, and 10 are not expired and have no market sale that proves. Their exit is a route for the SY's output, not another unwrap kind.
3. **The triage tool** (2026-10-07): `tools/registry/triage_exits.py` writes `docs/coverage/exit-triage.md`, every token without an exit by kind (Euler share, Pendle PT / LP, Curve LP not NG, gauge, other) and whether it is collateral anywhere, ranked by the scan's liquidations seized without an exit. Run it after the three-month scan; it orders what follows.
3. **Triage the 601.** For each: is it collateral in an admitted market (after 1B)? If not, drop it from the count. If so, find its exit:
   - a pool on a venue we route (Uniswap V2/V3, Curve stable/NG/crypto): `discover.py` and `discover_exits.py`;
   - an unwrap we support (ERC-4626, Pendle PT, Pendle market, Curve NG LP);
   - a wrapper we do not support yet. From the sample: staked and vault wrappers (`st-yETH`, `stakedao-…`, `sd-…-vault`), leverage vault shares (`LVWETH…`), Pendle LP wrappers, Curve gauge tokens. Each new kind is an Executor swap venue plus a router unwrap kind; batch them by frequency from the Phase 0 scan.
4. **Curve LP unwraps for non-NG pools.** The loader accepts only NG pools (`index.rs::build_unwrap`). Older stable and crypto pools need their own `remove_liquidity_one_coin` signatures (int128 against uint256 indices).
5. **Venues.** Balancer V2/V3 and Fluid DEX are absent. Add a venue only when the scan shows collateral whose only liquid exit is there.
   **Uniswap V4: done 2026-10-06 (decision 7).**
   - **Contract:** swap venue 9 (`SwapModule._swapV4`; `Executor.unlockCallback` dispatch; `T_V4_UNLOCKED`) takes a pool by its key and handles exact input or output and native ETH. Inside a V4 flash it swaps in that unlock; otherwise it unlocks itself.
   - **Hook rule (`MainnetVenues.v4HookAllowed`):** none, or a hook with no swap permission bit, or a reviewed listed hook (none yet).
   - **Fork tests (`ForkUniV4Swap`, a real Capyfi LAC liquidation):** both unlock paths, the seize equal to the Comptroller's own, a hooked pool refused, and the bit rule.
   - **Router:** a V4 pool is `V3State` plus a `V4Key`. It is folded from the PoolManager's `Swap`/`ModifyLiquidity`, seeded through `StateView`, and checked at boot (key hashes to id, pool initialized).
   - **Live oracle:** 11 router quotes on two V4 pools equal Uniswap's `V4Quoter` to the wei.
   - **Registry:** `tools/registry/discover_v4.py` added 2,709 pools (both tokens in the registry, liquidity in range), which gives 440 tokens their first exit.
   - **Refused:** 36,011 pools for their hooks. Among registry-token pools, 3,410 sit behind 1,856 swap-active hooks; the top few gate 100–330 pools each and are the review list for the allowlist.
   - **Found on the way:** the V3 swap math carried the last liquidity past the ticks the seed read. A seeded pool now stops at its window's edge (`V3State::window`).
6. **Two-hop exits through a non-WETH token** are Phase 4's job, not a registry fix. Mark tokens that have an exit only through such a path so Phase 4 can be tested on them.

**Done when (Phase 1 overall).** On the three-month fixture: no event on a protocol we have an adapter for ends "out of scope" or "no exit" except by a written decision; every remaining "no job" is dust or lost on merit.

**Decisions for you.**
- Whether look-alike forks that need their own adapter are in scope now or listed for later.

---

## Phase 2: Aave V2 adapter, tokens and pools (M)

**Why.** One event in the window (`0x7d2768de…`, the V2 LendingPool). V2 is frozen for new borrows but still holds debt, and its liquidators face little competition.

**True today.** No adapter, no config, not in the registry. The event scanner caught it only because V2 and V3 share the `LiquidationCall` topic.

**What carries over from V3.**
- `liquidationCall(collateral, debt, user, debtToCover, receiveAToken)` has the same selector and argument order. **The Executor's existing Aave V3 leg should execute a V2 liquidation unchanged** now that the pre-call health guard is gone. This must be proven on a fork before anything else is built; if it holds, Phase 2 needs no contract change.
- Reserve data, indexes in ray, aToken and debt-token balances, `LiquidationCall`, `ReserveDataUpdated`.

**What differs (verify each against the deployed V2 source, not from memory).**
- Close factor is a fixed 50%; no e-mode, no isolation mode, no dust rule.
- Stable-rate debt exists beside variable debt: two debt tokens per reserve, and stable debt accrues per user at the user's own rate.
- Configuration events come from `LendingPoolConfigurator` with V2's own names and shapes.
- The oracle quotes in ETH (wei per token), not USD with 8 decimals.
- Liquidation bonus is per collateral reserve; no liquidation protocol fee on mainnet V2.
- Interest uses V2's compounding formula.
- `flashLoan` charges 9 bps and has a different signature from V3's `flashLoanSimple`.

**Steps.**
1. Fork test first: one real underwater V2 position (or one made by supply, borrow, warp at a pinned block), liquidated through the current Executor with adapter id `A_AAVE_V3` and `market` = the V2 pool. Oracle: the pool's own `LiquidationCall` event and balances.
   **Passed 2026-10-06** (`contracts/test/fork/ForkAaveV2.t.sol`).
   - Setup: the real V2 liquidation at 26,104,558. The fork stands at N−1, moved to block N's number and time, with only the ENJ price update (tx index 197) replayed.
   - Our Executor flash-swapped 118.7 ENJ from the Uniswap V3 ENJ/WETH pool and liquidated through `legV3` on the V2 pool. It seized exactly the winner's 1,792,886,701,300,749 wei WETH and left no balance or allowance.
   - Result: V2 needs no contract change; step 5 keeps `AaveV3` on the wire.
2. **Done 2026-10-06, as a version of the Aave V3 adapter, not a crate of its own** (`AaveVersion::V2` in `liq-adapters/aave-v3`). The two share the reserve and user model; V2 differs where its deployed source does, each checked against the pool `0x02d84abd…` and the collateral manager `0xcc963272…`:
   - **Balances:** read from the aToken and debt-token events, each scaled by its own index with half-up `rayDiv`. V2's `BalanceTransfer` carries the unscaled amount.
   - **Interest:** V2's compounded-interest approximation.
   - **Health:** `GenericLogic`'s average liquidation threshold (floored), then `percentMul` and a half-up `wadDiv`. Debt is floored, and a collateral with LT 0 does not count.
   - **Liquidation:** the whole of the reserve's debt may be repaid. The deployed collateral manager has no 50% close factor, unlike what this section first assumed. The seize applies `percentMul` before the division, with a half-up `percentDiv`.
   - **Grace:** each reserve's window comes from the sentinel's `GracePeriodSet`, and liquidation is refused while `until >= block.timestamp`. V3.1's check is the same, and V3's `in_grace` now matches.
   - **Not in V2:** no e-mode, no isolation, no dust rule.
   - **Halts:** per pool (`PoolMeta.halted`); a paused pool blocks.
3. **Done.** `tools/registry/gen_aave_v2_toml.py` writes `aave-v2.toml` from `getReservesList` and each reserve's `getReserveData`. It leaves out AMPL, which rebases and has its own aToken implementation, so a position holding it fails closed.
4. **Done.**
   - Bound as protocol 12, market 65000, config-defined like Liquity, Fluid and Gearbox, so no interned id moves.
   - Price reads: the V2 oracle in ETH with WETH first, restated to USD through WETH's price (`ETH_QUOTED_READ`).
   - `liq-gas.toml`: `aave-v2 = 596_794`, measured on the fork.
5. **Done.** `ExecutorAdapter` stays `AaveV3` on the wire.
6. **Done (health).** The replay seeds a V2 pool (`seed::aave_v2`) and compares the health factor with the pool's own `getUserAccountData`. V2 values are in ETH, so only the HF is compared.
   - On 26104558 and on twelve recent V2 liquidations (26051395–26131956), our HF agrees with the pool's on every event.
   - Eleven ended with no job: the winners captured 0.00001–0.0044 ETH, under gas.
   - **The twelfth (26087344, a WETH/WETH position tipped over by interest; the winner captured 0.195 ETH) found a router bug.** A V4 pool is V3 state, so `lender_of` offered the V4 ETH/WETH pool as a flash-swap lender. The Executor called `token0()` on its book address, which is the pool id's low 20 bytes, not a contract, and reverted.
   - The fix: `Pool::is_v3_contract` keeps V4 pools out of the three places that need a V3 pool contract. Pools holding one token twice (V4 ETH/WETH) are refused at load and in `discover_v4.py`, and the one in the registry was removed.
   - **Behind it, a wider gap:** with that pool gone, a position seized in its own debt asset had no exit at all. Every WETH/WETH or stable/stable loop got no band and no job. `solve_pair` now returns the identity exit for `coll == debt` (no swap, no hop gas), and plan validation accepts a seize in the debt asset as the repay.
   - **Replayed:** 26087344 now ends in a job whose bundle succeeds on the real Executor at that block. We capture 0.19479 ETH to the winner's 0.19475, at 437,467 gas to their 408,296.
7. Optional, separate: Aave V2 as a flash source. Low value (9 bps against free sources); skip unless the scan shows debt only V2 holds.

**Done when.** The V2 event replays to a verdict; conformance holds on a sample of real V2 borrowers; the fork test liquidates through the Executor.

**Decision for you.** If step 1 fails and V2 needs its own Executor leg: build it, or leave V2 out.

---

## Phase 3: Aave V4 check (S; M only if the check finds gaps)

**Why.** V4 is where Aave's new debt goes. We believe it is covered; this phase proves it or lists what is missing.

**True today (from the repo; to be confirmed by the check).**
- Adapter `crates/liq-adapters/aave-v4`, pinned to `aave/aave-v4@40232a0a`.
- `config/protocols/aave-v4.toml`, generated by `gen_aave_v4_toml.py` at block 26,052,705: 13 of 13 lending spokes, 24 assets, 75 price sources. The registry lists 53 spokes; the generator's header says the rest are hub-registered addresses that are not lending spokes.
- Executor leg `A_AAVE_V4` (`_liquidateAaveV4`), fork-tested on a mainnet spoke (`test_fork_v4_weth_aave_flash`).
- Gas: `[wrap].aave_v4` and `[liquidation].aave-v4` in `config/liq-gas.toml`.
- No V4 liquidation occurred in the replay window, and the replay has no V4 seeder.

**The check. V4 is present, and Phase 3 is skipped, only if all of these pass.**
1. The bot binds V4 at startup from the committed config with no omission logged.
2. The spoke set is current: rerun the generator at head; the diff against the committed TOML is empty or explained.
3. Conformance: for a sample of real V4 borrowers at a pinned block, the adapter's health equals the spoke's `getUserAccountData` (the adapter's own tests used synthetic positions when V4 was not yet live).
4. A real V4 position can be replayed end to end: add `seed::aave_v4` to the harness, find V4 liquidations in the three-month scan (or the closest-to-liquidation real borrower if there are none), and get a job that simulates.
5. The Executor leg's guard: `_liquidateAaveV4` still calls `getUserAccountData` before `liquidationCall`. Measure its net cost the way V3's was measured, and decide whether it goes the way V3's did.
6. Flash: the startup log says the V4 flash index group is omitted ("no V3-style flash pool address"). Confirm V4 hubs are not a flash source we should have, or add them.

**If any item fails.** Build exactly that item. Each is S on its own.

**Done when.** A one-page result under `docs/coverage/aave-v4.md` with the six items and their evidence.

---

## Phase 4: The token graph and multi-hop router (L)

**Why.** Coverage by hand-added exits does not scale. A router that can leave through any path makes every pool and unwrap we add useful to every token at once, finds better exits than the hub when they exist, and is the base the arbitrage search will run on.

**Rule that makes it safe to ship in stages.** The current exit (direct, or through WETH) stays the baseline. A graph route replaces it only when its exact quote nets more after gas. The router can never do worse than it does today.

### 4A. The graph (M)

- **Nodes**: tokens, by the dense `AssetId` we already intern.
- **Edges**: one per direction through one venue. A two-token pool gives two; an n-coin Curve pool gives n·(n−1); an unwrap or redeem gives one, one way.
- **Edge payload**: pool id, the two coin indices, fee, measured hop gas, a liveness flag. No pool state: reserves and ticks stay in `PoolBook`, the single copy.
- **Storage**: compressed adjacency (edges sorted by source token, one offset per token). Built once at startup from the committed registry's pools.
- **No refresh, no runtime demotion (decision 2026-10-07).** The graph is not rebuilt or re-curated while the bot runs, and no edge is demoted or quarantined automatically: which pools the graph uses changes only through a reviewed registry commit and a restart.
- **Edge filter, from day one**: an edge enters only if its pool is live, seeded, and holds at least a minimum depth in USD. An edge whose quoted rate the exact solver later contradicts is demoted and logged.
- Scale today: 483 connected tokens, about 3,600 directed edges.

### 4B. Zero-size table (M)

- For every target token, a backward layered pass: the best rate from every token to the target in at most 1, 2, … K hops, at zero size, stored as logs with the next hop. K = 10 as the hard cap.
- All pairs. About 17 million relaxations per rebuild, tens of milliseconds by operation count (to be measured), about 28 MB. Rebuilt every block on the warm thread; published like the route table is today (`ArcSwap`).
- It is the only table that is provably optimistic, so it is the one the search prunes with.
- It replaces nothing yet: the warm route table and the bands keep working beside it.

### 4C. Depth curves on every edge (S–M)

**Dropped (decision 2026-10-07).** The search quotes each hop exactly on `PoolBook` instead of interpolating curves.

- Per edge, the output at a ladder of input sizes (eight points, $100 to $10M in the input token), computed from `PoolBook` each block: about 29,000 quotes per block.
- Read by interpolation during search. Derived data, thrown away each block.

### 4D. Bands with a depth dimension (M)

**Met through `solve_pair` (2026-10-07).** The band prices every size with `solve_pair`, which weighs the direct, WETH, graph-hub, chain and split exits with each one's hop gas, so a size pays for k hops exactly when the best route at that size does. The three rules hold by construction: affordability is the band's own test (bonus against fixed gas plus the route's hop gas); fees and slippage are in the exact quotes; and the graph search's bound is net of each hop's gas, so it stops where one more hop cannot pay. No separate per-k table is kept.

- Per (collateral, debt) pair and per hop count k: the minimum position size that pays for k hops.
- Three rules bound the search for a candidate:
  1. **Affordability.** `gas(k hops) + fees and slippage over k hops < bonus value`. The budget is the bonus, not the seized amount.
  2. **Fees bind for large positions.** Every hop charges its fee on the full notional; carry the cheapest cumulative fee per depth.
  3. **Marginal gain.** With the best route of up to k hops known, its loss against the oracle mid (fees plus slippage, in ETH) is the most any longer route can recover. If that loss is below the gas of one more hop, stop.
- The existing viability band (`bands::compute_one`) becomes the k = hub case of this.

### 4E. Search per candidate (M)

**Built (2026-10-07): a direct exit split with a chain.** When the best exit sells through direct pools and a chain of two to four hops leads from the same token (the collateral, or what it unwraps into), the sale is divided where the two marginal outputs meet (ternary search; a step of a thousandth toward the chain at either end that gains nothing skips it) and kept when it nets more after both routes' gas. The two cannot share a pool (direct pools hold the start token and the debt; a chain's hops do not), so each is quoted on the book as it is. The repay gives each side the pull in proportion to its output: the pools their exact-output shares, the chain one exact-output leg. A flash swap never funds such an exit. Not built: splitting across several chains, and logging routes that ranked well but failed the exact quote.

1. Start from the protocol's maximum seize.
2. Forward search from the seized token, carrying the real amount hop by hop through the depth curves. The zero-size table prunes any branch whose best possible finish cannot beat the current best; the three band rules cap the depth.
3. **Two targets.** The repay slice must become exactly the pull, in the debt token; the remainder goes to WETH. Routes may share a prefix (collateral to X once, then only the repay slice continues), for any X, not only WETH.
4. Keep the top n routes (n about 4).
5. Exact quote on those with the existing solver, splitting across them by water-fill on a working copy of pool state, so routes that share a pool see each other's impact.
6. If the marginal unit sold loses money, shrink the size and re-quote the same n routes.
7. Compare with the baseline exit net of gas; take the better.
- A flash swap fits unchanged: the last hop of the repay route is the pool that lends the debt.
- Record every case where a route ranked well and failed the exact quote. That rate says when the curves need more points and flags junk edges.

### 4F. Plan format and Executor: exact multi-hop chains (M; contract work)

**Decided: chains are exact to the wei, computed by the pools at execution, not quoted off-chain with a margin.** (Decision 2026-10-05.)

- **Today** a repay blob can express unwraps first, then set-amount legs with one `TAKE_BALANCE` per token, and the hub shape (sell all into WETH, buy the debt exact-out). It cannot express a chain whose intermediate amounts are only known at execution.
- **The exact way** is how Uniswap's own router does multi-hop exact output: the Executor swaps the **last** hop first for the exact pull in the debt token; the pool, before it is paid, calls `uniswapV3SwapCallback` for what it is owed; inside that callback the Executor swaps the **previous** hop for exactly that amount, and so on backward until the first hop is paid in the seized token. Every amount is the pool's own answer at execution: no margin, no leftover intermediate tokens, no sweep swaps, and nothing to revert from a quote that moved.
- **Built (2026-10-07): venue 10, both directions.** Exact output buys the pull backward through nested callbacks, as above. Exact input sells forward, each hop paid from the Executor's balance; it closes a leftover that has no pool into WETH (a `TAKE_BALANCE` profit leg). The router also starts a chain from what the collateral unwraps into, and accepts a collateral that reaches WETH only through a closing chain, whose hops the exit is charged. Hops: Uniswap V3, Uniswap V2 and SushiSwap; fork tests are exact against QuoterV2.
- **Full curated graph (2026-10-07).** Chains now run through every pool the graph curates: Uniswap V3 and V2 / Sushi hops as before, plus Uniswap V4 hops (the pool key, the same hook allowlist and settlement as a V4 leg, inside the unlock the Executor holds or one it takes) and Curve stable and crypto hops (the same MetaRegistry handler and coin checks as a Curve leg). Their keys and pool data ride in an extras section that each hop names by offset. Neither can be nested inside the exact-output recursion, and Curve has no exact output at all, so a chain with a V4 or Curve hop sells an exact input: what buys its part of the pull on the book, plus the tolerance, with the surplus debt swept to WETH, as a Curve leg's is. Fork tests: Curve 3pool then V3, and V3 then the hookless V4 ETH/USDC pool, each equal to the venues' own quoters.
- **Split across several chains (2026-10-07).** A sale is divided across the direct pools and up to three chains that share no pool, a slice at a time to whichever route pays most for the next one (`exact::split_routes`).
- **Encoding: a new venue, not a flag.** The flags byte is full (bit 0 take-balance, bit 1 exact-out, bits 2–7 the leg tie). A chain is one swap leg with a new venue id (the next free one, 9) whose data is the path: token, pool, token, pool, …, token. The leg keeps its tie, so per-leg tolerance (D70) carries over unchanged, and the flash fee rule applies to it as to any pool exact-out leg (the Executor adds the fee to its output).
- **Which venues can be in a chain.**
  - Uniswap V3: yes, by nested callback.
  - Uniswap V2 / Sushi: yes. The required input is computed on chain from the pair's live reserves (`getAmountIn`, exact at execution); pay the pair, then `swap`. No callback needed.
  - Curve: no exact-output swap exists. A Curve hop can only be the first hops of a route, sold exact-in or take-balance, ahead of the chain.
  - Unwraps (ERC-4626, Pendle, Curve LP one-coin): exact-in, so likewise only at the start.
- **Callback security.** The swap callback is the Executor's most security-critical code, and it will now start swaps. Each nested callback must authenticate its caller as the CREATE2-derived pool of the hop it is paying, exactly as today, and the chain's position must be held in transient storage so a callback cannot be replayed out of order or from another pool. This gets its own fork and unit tests, including a hostile pool in the middle of a path.
- **Validator.** `liq-plan` checks: the path's first token is the leg's token-in and its last is the debt token; consecutive hops share a token; every pool is a V3 or V2 venue; the chain's exact output equals the tied leg's pull; at most the hop cap.
- **Gas.** Measured per chain length on the fork; `liq-gas.toml` gets a per-hop figure for chained hops (cheaper than separate legs: no intermediate balance reads or approvals).
  - **Measured 2026-10-07** (`ForkChain` `test_gas_chain*`, cold, WBTC → WETH → USDC → USDT): one V3 hop 113,309, two 180,672, three 258,184. A chained hop costs 67k–78k, under `[swap].univ3` (82,100), which the router keeps per hop; the leg's own 31,209 is charged once per chain (`graph::CHAIN_LEG_GAS`), so every measured length is charged a little over its cost.
- **Profit leg.** The remainder of the seized collateral still leaves through an ordinary take-balance leg to WETH, which may itself be routed through the graph as exact-in hops. Only the repay slice needs the exact chain.

### 4G. Acceptance (S)

**Dropped (decision 2026-10-07).**

- Unit: on synthetic books, the search finds the known-best path and split, and never returns a route worse than the baseline.
- Replay: every event the hub already wins is unchanged or better; tokens marked in 1D as "exit only through a non-WETH path" now produce jobs.
- The three-month fixture: capture against the real liquidator, per event, before and after.
- Timing: detection to job, in a release build, per event, before and after. This number has never been measured end to end; Phase 4 must not ship without it.

### What Phase 4 leaves for later (not in this plan)

- **Cycle search.** With rates as logs, a profitable cycle is a negative cycle on the same table. DEX-to-DEX arbitrage is the same solver with no liquidation attached.
- **Backruns** from the mempool and MEV-Share: apply the pending swap to a copy of state, recompute the touched pools' curves, search cycles through them. New work: hint decoding per venue, sizing blind when a hint omits amounts, and latency.
- **Convex flow** across the whole graph, if path-level splitting visibly leaves money behind.

---

## Decisions for you, collected

| # | Decision | Needed by |
|---|---|---|
| 1 | ~~Admission floor~~: decided, every market tracked and tiered cold/warm by viability | 1B |
| 2 | ~~Look-alike forks~~: decided 2026-10-06, widest coverage. Bind Capyfi, build the Fuse variant, and work through the remaining forks by liquidation evidence | 1B |
| 3 | ~~Aave V2 own Executor leg~~: not needed; the V3 leg carries over (fork test) | 2, step 1 |
| 4 | ~~Aave V4 pre-call health guard~~: already removed | 3, item 5 |
| 5 | ~~Swap-leg flag~~: decided, exact chains by nested callback, encoded as a new venue | 4F |
| 6 | New swap venues (Balancer, Fluid DEX) when the scan shows collateral that exits only there | 1D |
| 7 | ~~Uniswap V4 swaps~~: decided 2026-10-06, allowed on hookless pools (`hooks == 0`) and on pools whose hook is on a reviewed allowlist; still refused on any other hook. Reverses the earlier hook-based exclusion. First case: Capyfi's LAC exits only through the hookless LAC/USDC 1% V4 pool | 1D |
| 8 | ~~Halt scope~~: decided 2026-10-06. A halt-class log from one vault or fork halts that market only, never the bot (Euler per vault, Compound per fork, Silo per pair and Gearbox per credit manager are done; a Gearbox pool, quota keeper or account factory halts the managers sharing it, and only the contracts register still stops the protocol) | 1B |

## Risks

- **Replay speed** gates everything; Phase 0 step 3 comes first for that reason.
- **Admitting hundreds of markets** raises startup reads, state size and per-block price reads. Measure each before and after 1B.
- **Thin pools and dead tokens** will look attractive in a zero-size table. The edge filter and the exact-quote check are what keep them out of plans.
- **Contract changes** (4F, possibly 2) mean new fork tests, a new gas snapshot with Foundry 1.8.3, and the Executor is still undeployed, so they cost no redeploy now and would later.
