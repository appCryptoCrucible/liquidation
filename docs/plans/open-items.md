# Open items: step-by-step plan

Status as of 2026-09-25. Phase 0 is on origin (`977242f`). Phase 1's ledger is on origin (`32113ef`). The getter overlay is on origin (`ae84bb5`): each bound adapter except Silo publishes its own price getter, off the hot path, and the engine lays that over the canonical vector per market. Gearbox G1 is in the tree: a Redstone, Pyth, or `updatable()` leaf marks the token, and a debt account with that token enabled is not a candidate (one coverage warning per account per hour). Chainlink reproduction, Curve exits, unwraps, the docs audit, G2–G6, and the other carry-forwards are not done. Tokens that failed the chain read are listed under Phase 1 and were not added.

Each phase lists why it matters, what is true today, the steps in order, how we know it's done, and any decisions for you. Sizes are rough: **S** is under a day, **M** is a few days, **L** is a week or more.

## Recommended order

| # | Phase | Size | Depends on |
|---|---|---|---|
| 0 | Commit the current work | S | — |
| 1 | Stable asset ids, then add missing tokens to the registry | M | 0 |
| 2 | Price-feed coverage for every tracked asset | L | 1 |
| 3 | Protocol-native oracles: Morpho, Liquity V2, Fluid | L | 2 (shares the feed graph) |
| 4 | Curve StableSwap-NG exits | M | 1 |
| 5 | Curve crypto-pool exits (twocrypto / tricrypto) | L | 4 |
| 6 | Unwrap exits (LP / vault / PT collateral) | L | 4, 5 help; can start independently |
| 7 | Adapter-vs-protocol-docs audit (remaining protocols) | M | — (run alongside) |
| 8 | Smaller carry-forwards | S each | varies |

Phases 1 → 2 → 3 are the pricing spine: every later phase prices through it. Phases 4–6 widen exits. Phase 7 is the standing rule that every adapter matches the protocol's real, deployed liquidation process.

---

## Phase 0: Commit the current work (S)

**Why.** About two sessions of changes sit uncommitted: V2/Curve exits, Gearbox v3.1, Silo, gas measurements, the three generated protocol configs, and the build speedups. One bad checkout loses all of it.

**Steps**
1. Review `git status`; confirm no generated junk is staged (fork traces in `tools/gas-measure/` are gitignored).
2. Commit in logical pieces so each can be reverted alone:
   1. Build settings (`Cargo.toml` dev profile, `.cargo/config.toml`).
   2. Executor + contract tests: V2/Curve venues, Gearbox partial/full, Silo/Gearbox fork tests.
   3. Engine: `RepayOption.min_repay` / `pair_seize`, profit/select, conformance check 8.
   4. Gearbox adapter: v3.1 discovery, min-debt tracking, partial window, full leg.
   5. Registry: exit pools, null-symbol fix, schema.
   6. Pool book + Curve/V2 seeding.
   7. Generated protocol configs + generators + id-drift tests.
   8. Gas config.
   9. Docs.
3. Run the full check before the last commit: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`, `cargo test --workspace --no-fail-fast`, `forge test` (unit and fork).

**Done when** the tree is clean and every commit builds.

---

## Phase 1: Stable asset ids, then registry token coverage (M)

**Why.** An `AssetId` is today the token's **position in the registry's sorted token list** (`Intern::from_registry`). Adding one token renumbers every token after it, which silently invalidates every committed protocol config (`spark.toml`, `euler-v2.toml`, `compound-v2.toml`, and the generated `aave-v3/v4`, `morpho-blue` files) and any persisted state keyed by `AssetId`. We can't add the missing tokens safely until ids are stable.

**Today**
- 1,073 tokens interned.
- Missing from the registry, so unpriced and unliquidatable:
  - **Aave V3 Core:** 9 reserves: LUSD, SNX, ENS, 1INCH, STG, KNC, FXS, PT-USDe-7MAY2026, PT-srUSDe-22OCT2026.
  - **Aave V4:** 3 reserves (listed in the `aave-v4.toml` header).
  - **Morpho:** 7 markets use a token outside the registry (8 distinct tokens).
  - **Curve:** 1,355 registered pools include at least one untracked coin.
- The id-drift tests (`protocols::tests::*_toml_ids_match_registry_intern`) now **fail** if ids move, so a renumbering can't slip through silently.

**Steps**
1. **Id scheme: append-only id ledger** *(decided 2026-09-25)*. Commit `registry/asset-ids.json` (address → id). Existing tokens keep today's ids; new tokens get the next free id; removed tokens keep a tombstone. `Intern::from_registry` reads the ledger instead of sorting.
2. Implement the ledger in `liq-config`:
   - Load, validate (no duplicate ids, every registry token has an id), and keep `u16` bounds.
   - Unit tests: adding a token keeps all prior ids; a token missing from the ledger fails the load.
3. Write the ledger once from today's order, so no id changes (verify: all existing tests still pass unchanged).
4. Point `intern_ids()` in `tools/registry/gen_aave_v3_toml.py` (also used by the V4 and Morpho generators) at the ledger.
5. **Token discovery pass:** collect every token that any tracked protocol can lend or take as collateral:
   - Aave V3/V4 reserve lists.
   - Morpho market params.
   - Spark reserves (GNO is already a known gap).
   - Compound/Euler/Silo/Fluid/Liquity/Gearbox collateral lists.
   - Curve coins, only where a pool is otherwise usable (Phase 4).

   Record symbol, decimals and quirks exactly as `discover.py` does, and run the boot assertion (`committed_registry_matches_chain`).
6. Add the tokens to the registry and ledger; regenerate all protocol configs; re-run the id-drift tests, the bind test and the live boot assertion.
7. Spark: drop the "asset 1073 = first unused slot" hack for GNO once GNO has a real id.

**Progress.** Steps 1–4 are done (`registry/asset-ids.json`, `Intern::from_registry` reads it, the generators read it). Step 5 added the reserves whose metadata read: Aave V3's 9, Aave V4's 2 (one shared with V3), and Spark GNO at id 1073. `aave-v3.toml` and `aave-v4.toml` were regenerated at block 26052705 and no longer list a reserve as left out. Morpho was re-read at the same block: still 7 markets unmapped, 0 registry/chain disagreements. Existing asset ids did not move. Source addresses on reserves that were already listed did not change; the informational `feed` labels above the interned-feed range did, because those labels are first-seen order.

Not added, because `decimals()` failed on mainnet (fail closed, no guessed metadata):

- Morpho, no contract code: `0x1f32b1c2345538c0c6f582fcb022739c4a194ebb`, `0x833589fcd6edb6e08f4c7c32d4f71b54bda02913`, `0x84b78bc998e4b1a63f2cf9ebfe76c55fc96a5a9b`.
- Morpho, contract reverts `Paused()` (`0x9e87fac8`) on `decimals()`/`symbol()`: `0x091356e6793a0d960174eaab4d470e39a99dd673` (`asset()` returns USDC), `0x2a5c94fe8fa6c0c8d2a87e5c71ad628caa092ce4`, `0x94f6cb4fae0eb3fa74e9847dff2ff52fd5ec7e6e`, `0x9fb57943926749b49a644f237a28b491c9b465e0`.
- Euler `asset` values that revert `decimals()` with no data: `0x10674c8c1ae2072d4a75fe83f1e159425fd84e1d`, `0x33243bfb60c05d9f20a0064365dc10f5ee599956`, `0x656d2c7a970d13073a6a58fae18f39abf31fae41`, `0x83e0698654df4bc9f888c635ebe1382f0e4f7a61`, `0x912ce59144191c1204e64559fe8253a0e49e6548`, `0xecac9c5f704e954931349da37f60e39f515c11c1`.

Gearbox, Fluid, and Liquity are not families in `registry.protocols`, so this pass did not walk their collateral lists. Curve coins of pools discovery rejects are Phase 4.

**Review (2026-09-25).** Verified: every original id 0–1072 is unchanged, the 11 appended tokens are 1073–1083, and the live full-registry boot assertion passes with them. The excluded tokens really are unreadable on mainnet: other-chain addresses with no code, or `Paused()`. Fixed:
- **Price book no longer loaded.** The new tokens made six registry oracles resolvable (SNX, ENS, 1INCH, STG, KNC, FXS). `FeedSet::bind` requires a `config/feeds` entry for every resolvable oracle, so the canonical price book failed to load (`MissingToml`). Added the six feeds to `config/feeds/aave-v3.toml` (heartbeat and deviation from Chainlink's directory; source and aggregator checked on chain).
- **Id space vs live count.** Tables indexed by `AssetId` were sized with `intern.assets().len()`, and the canonical price vector was filled in list order. Both break at the first retired id: later assets would sit at the wrong index and read as unpriced. Added `Intern::asset_id_capacity()` (the ledger's `next`) and used it at every such site (canonical book, drain decimals, flash index, shared state, engine capacity, replay).
- **Phase 0 gap.** `config/bid.toml` (99.5 % Aave, 75 % other) was left out of the Phase 0 commits, so `origin/main` fails its own bid test. It's in the tree now; commit it with Phase 1.
- Tests pinned to the old counts updated: feeds resolve 15/15, with a negative case kept by dropping the six tokens from a copy; Spark GNO now interns at 1073.

**Done when** every reserve/market token of every bound adapter has an intern id, the protocol configs list nothing as "left out: token not in registry", and adding a token never changes an existing id. The Aave configs meet that. Morpho's 7 markets do not, until those tokens can be read.

**Risk.** Persisted state (snapshots) written under the old scheme: the ledger keeps today's ids, so this is a no-op if step 3 is exact. Verify with a snapshot round-trip test.

---

## Phase 2: Price-feed coverage for every tracked asset (L)

**Why.** Our breadth edge only counts if we can price what we can liquidate. Two prices matter, and they're different:
- **Health price:** what the *protocol* uses to decide liquidatable and how much collateral a repay seizes. It must match the protocol's own oracle, or we mis-size or miss positions.
- **Exit price:** what the collateral actually sells for. It comes from the pool book (exact swap quotes), not from oracles.

**Today**
- `config/feeds/` has **one file** (`aave-v3.toml`) with 16 Chainlink feeds covering **15 distinct assets** out of 1,084 (every registry oracle; 9 before Phase 1).
- `DerivedBook` (wstETH/rETH rates, sDAI chi, Aave CAPO, fair LP, cross rates) exists in `liq-oracle` but is **omitted at startup**: "no committed derived-spec config".
- The engine's `coll_per_debt` comes from these Chainlink prices, which only approximates Morpho/Liquity/Fluid (Phase 3).
- The protocol configs already record every Aave V3/V4 and Spark **price source** per reserve, and every Morpho oracle per pair. That's the inventory of what each protocol actually reads.

**Steps**
1. **Inventory:** for every (protocol, asset) with an intern id, record the protocol's source address. Aave V3/V4/Spark sources are already in the configs. For Compound, Euler, Silo, Liquity, Fluid and Gearbox, read their oracle getters. Output: `registry/price-sources.json`.
2. **Classify each source** by what it computes, reading the source contract. Classes:
   - Chainlink-direct USD.
   - Chainlink ETH-quoted × ETH/USD (`Formula::Cross`).
   - Exchange-rate LST (`LstWad`).
   - Aave CAPO-capped (`CappedLst`).
   - ERC-4626 `convertToAssets` × underlying.
   - Pendle PT oracle.
   - LP fair price (`LpFair`).
   - Pegged/fixed.
   - Other.
3. **Write the feed and derived-spec configs** from the classification with a generator (same pattern as the protocol configs: chain-read, id-drift test):
   - `config/feeds/<protocol>.toml` for push feeds (heartbeat, deviation from Chainlink's reference directory, as today).
   - `config/derived/<protocol>.toml` for `DerivedSpec`s (the startup loader in `index.rs::load_derived` is the stub to replace).
4. **Fill `DerivedBook` gaps:** add `Formula` variants only for classes the inventory actually needs (likely ERC-4626 and Pendle PT). Each variant gets a unit test against a recorded on-chain value.
5. **Per-protocol verification (the key acceptance test).** For every (protocol, asset) pair, a live ignored test compares our computed price with the protocol's own getter at one block:
   - Aave: `getAssetPrice`.
   - Morpho: `price()` (see Phase 3).
   - Compound: `getUnderlyingPrice`.
   - Euler: `getQuote`.

   Tolerance zero where the math is ours to reproduce; document any rounding-only difference.
6. **Update triggers:** subscribe to each feed's `AnswerUpdated` (push) and the rate contracts' events. Where a rate has no event (some ERC-4626 vaults), refresh it off the hot path each block, like the Curve reseed thread.
7. **Staleness:** mirror each protocol's own staleness rule where it has one; otherwise heartbeat × 1.5 → unpriced (fail closed).
8. Remove the "Chainlink approximation" note from the engine pair terms once Phase 3 lands.

**Done when** every asset of every bound adapter has a health price whose value equals the protocol's getter at a pinned block, and startup omits nothing for "derived".

**Decided 2026-09-25.** Where a protocol reads a price we can't reproduce exactly (proprietary adapter, off-chain signed price), we read the protocol's own getter each block, **off the hot path** (a background thread writes the value; the hot path only reads it), for that residue only. This costs one RPC call per such asset per block, which is fine on the production reth node.

**Progress.** The getter path is in, ahead of the reproduction inventory. `liq-bot-prices` multicalls each adapter's `price_reads` at each new head and `ProtocolPriceBook` overlays the answers per `(protocol, market)`. Aave V3/Spark (`getAssetsPrices`), Aave V4 (`getReservesPrices`), Compound (`getUnderlyingPrice`), Euler (`getQuote` for the USD unit `0x…0348` only), Gearbox (`getPrice`, 8-decimal USD) and Liquity (`fetchPrice`, and BOLD at 1e27) publish RAY USD per whole token. Morpho and Fluid publish a ratio that `oracle_price` / `oracle_debt_per_col_1e27` turns back into the protocol's own integer; a Fluid rate that does not round-trip is omitted. Silo is not overlaid: `debt_value` sizes the quote and the solvency oracle is in the pair's quote token, which the rows do not prove is USD. When a canonical slot has `ts == 0`, sizing uses the first USD getter (Aave, Spark, Compound, Euler, Gearbox) and never a ratio price.

Live, same block, to the wei: Aave V3/Spark `getAssetPrice` (100 prices, 80 + 20, at block 26054734), Aave V4 spoke `0xe190…` `getReservePrice`, Compound cUSDC `getUnderlyingPrice` (`mantissa / 1000`), Morpho `price()` on 20 pins. Euler, Fluid, Liquity and Gearbox have unit tests of the conversion and still need that same live equality test. Steps 1–4 and 6–7 (Chainlink inventory, derived specs, `AnswerUpdated`) are not done.

---

## Phase 3: Protocol-native oracles for Morpho, Liquity V2, Fluid (L)

**Why.** These three price health through their own oracle contracts, not a shared feed. Today the engine approximates them with Chainlink ratios. The approximation is wrong whenever the oracle has a vault-conversion leg, a hard peg or its own scale, so we'd mis-size or miss positions.

### 3a. Morpho Blue
**Today.** 1,620 `(oracle, collateral, loan)` pins in `morpho-blue.toml`. Health is `collateral · price() / 1e36 · lltv ≥ debt`, with `price()` from each market's oracle.

**Steps**
1. Group the 1,620 oracles by implementation (bytecode hash). Expect `MorphoChainlinkOracleV2` to dominate.
2. For `MorphoChainlinkOracleV2`: decompose each into its `baseFeed1/2`, `quoteFeed1/2`, `baseVault`/`quoteVault`, conversion samples and `SCALE_FACTOR`, read once at startup. `price()` is then computed from the Phase 2 feed graph. Its update triggers are those feeds' events plus vault rate changes.
3. For other implementations: read `price()` directly per block for markets that hold positions near liquidation (off the hot path; own node).
4. Feed the per-market price into the adapter's health and the engine's pair terms (`coll_per_debt`), replacing the Chainlink approximation for Morpho.
5. Live test: for a sample of 50 markets across implementations, our price equals `oracle.price()` at a pinned block.

### 3b. Liquity V2
**Today.** Three branches (WETH, wstETH, rETH); the adapter already knows each branch's `PriceFeed`.

**Steps**
1. Read each branch `PriceFeed` at the pinned commit: the Chainlink primary and canonical-rate composition, `lastGoodPrice`, and the shutdown/fallback behaviour when the oracle fails.
2. Reproduce `fetchPrice()`'s *liquidation* path exactly. Note that liquidations use `fetchPrice()` (it can update `lastGoodPrice`), not the view.
3. Handle branch shutdown: a shut-down branch has different liquidation rules. Treat it as its own state; don't quote liquidations there.
4. Live test: our price equals the branch `PriceFeed` result at a pinned block, for all three branches.

### 3c. Fluid
**Today.** The adapter handles T1 vaults only; the Executor calls `liquidate(debtAmt, colPerUnitDebt, to, absorb)`.

**Steps**
1. Read the Fluid oracle for each vault: `getExchangeRateLiquidate()` / `getExchangeRateOperate()`, at 1e27 scale.
2. Classify implementations as for Morpho: decompose into feeds where possible, else read per block.
3. Use the *liquidate* rate for health and seizure sizing.
4. Live test per vault at a pinned block.
5. **Add the missing Fluid fork test.** `test_fork_fluid_t1_cannot_open_without_invented_state` is still skipped: open a real T1 position, make it liquidatable, liquidate through the Executor, and measure gas (replacing the mainnet-frame estimate in `liq-gas.toml`).

**Progress.** Morpho's overlay reconstructs `IOracle.price()` to the wei (loan price is `1e36`, not `1e27`, because `1e27` cannot represent a `price()` that is not a multiple of `1e9`). Liquity publishes `fetchPrice` even when `newOracleFailureDetected` is set, because a shut-down branch still liquidates at `lastGoodPrice`; a revert is the only skip. Fluid publishes a T1 pair only when `getExchangeRateLiquidate` round-trips through `oracle_debt_per_col_1e27`. The Fluid fork test is still skipped. The LIF check against the documented `0.3` / `1.15` formula is not done.

**Done when** health for all three uses the protocol's own price with a per-protocol live equality test, and the Fluid fork test passes.

---

## Phase 4: Curve StableSwap-NG exits (M)

**Why.** Of the Curve pools whose coins we track, **499 are StableSwap-NG**. Only 30 are the older plain design (16 in use today). NG is where most current stable liquidity is.

**Today.** `CurveState` models plain pools (static rates, fixed fee). Discovery (`tools/registry/discover_exits.py`) rejects any pool with `stored_rates()` or `offpeg_fee_multiplier()`. The Executor's Curve leg calls `exchange(int128,int128,uint256,uint256)`, which NG also has.

**Steps**
1. Read the NG pool source at a pinned commit (`curvefi/stableswap-ng`): `get_D` (note: `D_P` divides by `N^N` once, not per coin), `get_y`, `_dynamic_fee(xpi, xpj, fee, offpeg_fee_multiplier)`, and the fee applied *before* scaling to raw.
2. Extend the solver: `CurveState` gains `offpeg_fee_multiplier` and a `ng` variant with the NG math, including exact `exchange` rounding.
3. Rates: `stored_rates()` depends on asset type:
   - 0 standard: static.
   - 1 oracle: rate from an external call.
   - 2 rebasing.
   - 3 ERC-4626: `convertToAssets`.

   Types 1 and 3 change without pool logs, so the Curve reseed thread refreshes them every block.
4. Discovery: accept NG pools, and extend the exact-math check to NG (our quote must equal `get_dy` within the known 1-wei exchange-vs-get_dy rounding).
5. Executor: confirm on a fork that NG pools pass `MetaRegistry.is_registered` and `coins(uint256)`, and that `exchange` works with our approve/exchange/approve-0 flow. No contract change expected.
6. Fork test: repay swap through a real NG pool (e.g. a USDe/USDC NG pool).
7. Live Rust cross-check (extend `committed_exit_pools_quote_exactly_on_chain` to NG pools).
8. Measure NG swap gas (dynamic fee and rate calls cost more than plain) and add `[swap].curve_ng` to `liq-gas.toml`.

**Done when** NG pools are in the registry, quote exactly on chain, and a fork liquidation exits through one.

---

## Phase 5: Curve crypto-pool exits (L)

**Why.** 180 crypto pools (twocrypto-ng, tricrypto-ng) hold volatile-pair liquidity (e.g. WETH/WBTC/USDT, CRV/ETH).

**Steps**
1. Read twocrypto-ng and tricrypto-ng source at pinned commits: the Newton solver on the gamma curve, `price_scale`, dynamic fee from `xp` imbalance (`mid_fee`/`out_fee`/`fee_gamma`), and the `A_gamma` ramp.
2. Solver: new `PoolState::Crypto` with exact integer math and `get_dy` parity tests against recorded on-chain values.
3. State: `price_scale` and D move with every trade, so treat these pools like Curve plain (stale on log, re-read each block).
4. **Executor change:** crypto pools use `exchange(uint256,uint256,uint256,uint256)`. Add a venue id (e.g. `VENUE_CURVE_CRYPTO = 4`), verified via MetaRegistry, with unit tests, a fork test and a gas measurement. This means a new Executor deploy.
5. Discovery, registry and exact-math check as for NG.

**Done when** crypto pools route with exact quotes and a fork liquidation exits through one.

---

## Phase 6: Unwrap exits for LP / vault / PT collateral (L)

**Why.** Most live **Gearbox** collateral is wrapped:
- Beefy `wmoo…` over Curve/Balancer LP (e.g. `wmooCurveETH+-WETH`, `wmooBalancerEthereumosETH-WETH`).
- Pendle PT (`PT-DETH-23APR2026`).
- savETH.

Aave and Morpho also list PTs and LSTs. We can liquidate these accounts, but the router can't sell what we seize, so they quote with no exit route. The Executor already unwraps two kinds, Euler vault shares and Compound cTokens, and that's the pattern to extend.

**Steps**
1. Inventory, from Phase 1's token list: every collateral token that has no direct pool route but converts to something that does. Group by wrapper kind:
   - ERC-4626 vault (`redeem`).
   - Beefy vault (`withdrawAll` / `withdraw(shares)`).
   - Curve LP (`remove_liquidity_one_coin`).
   - Balancer BPT (`exitPool` single-token).
   - Pendle PT (router swap before maturity, `redeem` after).
   - Others (savETH etc.: read each contract).
2. Pick the first wrapper kinds by collateral value at stake, e.g. Beefy → Curve LP → one coin, which covers most of the Gearbox WETH/wstETH debt.
3. Executor: add an **unwrap step** to the plan format (a pre-swap leg type, like the Euler/Compound redeem): `(kind, target, amount = balance, min_out)`, with targets verified on chain per kind (e.g. Curve LP via MetaRegistry, Beefy vault → `want()` equals the expected LP). Include unit tests, fork tests per kind and gas measurements. This is a new Executor deploy, so batch it with Phase 5 if timing allows.
4. Router/engine: the exit for a wrapped collateral is `unwrap → pool route`. The quote chains the unwrap's exact output (`previewRedeem`, `calc_withdraw_one_coin`, Balancer query) with the existing solver.
5. **Pendle PT** *(decided 2026-09-25)*: never held. Every liquidation is flash-funded, so seized PT must become the debt asset inside the same transaction:
   - **Before maturity:** sell on the PT's Pendle market (PT → SY → underlying), then route as usual. Quote it exactly off the hot path with Pendle's on-chain quote views (RouterStatic); verify the market address against Pendle's market factory, as we verify V2/Curve pools.
   - **After maturity:** redeem 1:1 to the underlying through Pendle's redeem path (no market). This is an unwrap step.
   - Health pricing of PTs is separate: it's the protocol's own PT oracle, handled in Phase 2/3.

**Done when** the Gearbox live accounts' collateral has a quoted exit, proven by a fork liquidation of a real Gearbox account (Beefy/Curve LP) that ends in WETH profit.

---

## Phase 7: Adapter-vs-protocol-docs audit (M, run alongside)

**Why.** Your standing rule is that every adapter must match the protocol's real, deployed liquidation process, checked against official docs *and* the deployed source (Sourcify, `version()`, live probes), not just a pinned repo branch. This session's Gearbox and Aave checks both found real mismatches.

**Done so far.** Aave V3 (docs page, `LiquidationLogic` at the pin, live pools at revision 11, all matching). Gearbox v3.1 (fixed: discovery, partial rules, full path).

**For each remaining protocol** (Aave V4, Spark, Morpho, Euler V2, Compound V2, Silo V2, Liquity V2, Fluid):
1. Read the official liquidation docs page, and note close factor, bonus, fees, dust and minimum rules, bad-debt handling, and who may liquidate.
2. Confirm the **deployed** version and source (Sourcify or verified source; `version()`/`REVISION()`), and diff against our pinned commit.
3. Check the adapter's quote rules against both, rule by rule. Write the findings into `docs/coverage/<protocol>.md`.
4. Fix mismatches, each with a test. Where possible, add a fork test on real contracts.

**Known items already on the list**
- **Spark** runs pre-3.5 Aave code (`STATE.md` carry-forward). Our V3 adapter ignores Spark's pre-3.5 events (isolation debt, siloed borrowing, e-mode changes) and uses 3.5+ rounding and the total-debt close-factor cap, where Spark is per-reserve. It needs a Spark event profile or its own adapter variant.
- **Aave V4:** confirm the target-health-factor and dynamic-bonus rules against Aave's V4 docs once published, and that live spokes run the pinned `40232a0a` code.

---

## Phase 8: Smaller carry-forwards (S each)

1. **Gearbox MarketId stability.** Managers get MarketIds in discovery order. A new manager added to an early configurator shifts later ones across restarts. Persist a manager → MarketId map (same idea as Phase 1's ledger). The 4201–4299 band has 99 slots and 70 are used today, so widen it before it fills.
2. **Aave V4 spoke cap.** The adapter tracks at most 32 spokes (13 lending spokes today). The generator ranks by debt and lists the rest; raise `SpokeFlags::MAX_SPOKES` if it ever binds.
3. **Fork-test time loops.** Under via-IR, `vm.warp(block.timestamp + dt)` in a loop can reuse a cached timestamp and never advance (this stalled the Silo test). The Aave V4 open loop, the Morpho interest loop, and the Liquity ICR loop now use a local clock, same as Silo. Single warps outside a loop were left as they are. The fork tests were not re-run.
4. **Morpho runtime markets.** The adapter assigns MarketIds from `CreateMarket` logs (catalog 5000, markets from 5001). `MorphoBlue::backfill` asks the archive for logs from block 0, not from the singleton's deployment block. A node that truncates that range fails the backfill. A cold start on the production node has not been checked.
5. **PnL ledger deferred fields.** Outcome rows are net-only today; the other fields wait on `liq-books`. Wire them when `liq-books` produces them.
6. **Gearbox margins.** The full-liquidation over-add (0.5% of account value) and partial lower-bound headroom (1%) are first guesses. Tune them from fork runs and live outcomes; the full-path surplus comes back anyway, and partial headroom trades revert risk against size.
7. **Registry families with no adapter:** `ajna` (235 markets) and `sky-maker`. Decide whether to build adapters (breadth) or drop them from the registry.
