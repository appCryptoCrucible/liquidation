# Open items: step-by-step plan

Status as of 2026-09-25. Phase 0 is on origin (`977242f`). Phase 1's ledger is on origin (`32113ef`). The getter overlay is on origin (`ae84bb5`): each bound adapter except Silo publishes its own price getter, off the hot path, and the engine lays that over the canonical vector per market. Gearbox G1 is on origin (`0e09099`). Signed Redstone/Pyth payloads are not a target. Spark stays on the Aave V3 adapter with its own rounding: half-up `rayMul` and a close factor on that reserve's debt, taken from the deployed pool `0x5ae329…`. Chainlink reproduction, Curve exits, unwraps, the docs audit, G2–G6, and the other carry-forwards are not done. Tokens that failed the chain read are listed under Phase 1 and were not added.

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

**Progress.** The getter path is on origin (`ae84bb5`), ahead of a full reproduction inventory. On 2026-09-25 every Aave V3 `price_sources` row was classified at head: 16 are plain Chainlink (`aggregator()` answers, `decimals()` = 8) and already have a `config/feeds/aave-v3.toml` row; 61 do not answer `aggregator()` (adapter sources, not copied); three underlyings share the already-listed tBTC proxy `0xb41e773f…` whose directory row is BTC/USD. Every resolved `aave-v3` registry oracle already has a feed row, so nothing was added. Heartbeat and deviation stay the directory values. The getter reader stays the authority. `liq-bot-prices` multicalls each adapter's `price_reads` at each new head and `ProtocolPriceBook` overlays the answers per `(protocol, market)`. Aave V3/Spark (`getAssetsPrices`), Aave V4 (`getReservesPrices`), Compound (`getUnderlyingPrice`), Euler (`getQuote` for the USD unit `0x…0348` only), Gearbox (`getPrice`, 8-decimal USD) and Liquity (`fetchPrice`, and BOLD at 1e27) publish RAY USD per whole token. Morpho and Fluid publish a ratio that `oracle_price` / `oracle_debt_per_col_1e27` turns back into the protocol's own integer; a Fluid rate that does not round-trip is omitted. Silo is not overlaid: `debt_value` sizes the quote and the solvency oracle is in the pair's quote token, which the rows do not prove is USD. When a canonical slot has `ts == 0`, sizing uses the first USD getter (Aave, Spark, Compound, Euler, Gearbox) and never a ratio price.

Live, same block, to the wei: Aave V3/Spark `getAssetPrice` (100 prices, 80 + 20, at block 26054734), Aave V4 spoke `0xe190…` `getReservePrice`, Compound cUSDC `getUnderlyingPrice` (`mantissa / 1000`), Morpho `price()` on 20 pins. Added 2026-09-29, each passing at head: Liquity, every branch's `fetchPrice` × 1e9 and BOLD at 1 RAY; Euler, 8 admitted USD-unit vaults, `getQuote(10^dec, asset, USD)` × 1e9; Gearbox, all 70 managers bound live, 285 token prices equal to `convertToUSD(10^dec, token)` × 1e19, while 21 reverting feeds and 79 zero-price feeds are not published. Fluid publishes no price: its health is each vault's own liquidation, read every block (Phase 3c), and that read is live-tested against Fluid's resolvers. The Aave V3 plain-Chainlink inventory is done and added nothing. Derived specs, `AnswerUpdated` wiring, and the same inventory for the other protocols are not done. Gearbox G1 (pull-feed alarm) is on origin (`0e09099`); since 2026-09-29 it also checks the node at the walk's depth limit (5), and a node there that still has children is counted as pull (fails closed). G2–G6 stay unbuilt until that alarm fires.

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
**Done 2026-09-29** (see the Fluid note under Phase 3's progress). The steps below were the oracle-reproduction plan; the adapter instead reads each vault's own liquidation every block, which needs no oracle.

**Steps**
1. Read the Fluid oracle for each vault: `getExchangeRateLiquidate()` / `getExchangeRateOperate()`, at 1e27 scale.
2. Classify implementations as for Morpho: decompose into feeds where possible, else read per block.
3. Use the *liquidate* rate for health and seizure sizing.
4. Live test per vault at a pinned block.
5. **Add the missing Fluid fork test.** `test_fork_fluid_t1_cannot_open_without_invented_state` is still skipped: open a real T1 position, make it liquidatable, liquidate through the Executor, and measure gas (replacing the mainnet-frame estimate in `liq-gas.toml`).

**Progress.** Morpho's overlay reconstructs `IOracle.price()` to the wei (loan price is `1e36`, not `1e27`, because `1e27` cannot represent a `price()` that is not a multiple of `1e9`). Liquity publishes `fetchPrice` even when `newOracleFailureDetected` is set, because a shut-down branch still liquidates at `lastGoodPrice`; a revert is the only skip. Since 2026-09-29 the hot thread restates Morpho ratios in USD before they reach the book (`protocol_prices::to_usd`): each read is multiplied by the numeraire's USD price (Morpho's loan token; canonical, else a USD getter) over the numeraire's published value, so the pair's ratio is the protocol's own and the values are real USD. A read whose numeraire has no USD price is left out.

**Fluid (rebuilt 2026-09-29).** Production had no Fluid vault data (no vault read at bind, no vault logs subscribed, four tokens configured, only single-position vaults quotable), and the price read called the vault instead of its oracle, so Fluid never quoted. It is now read, quoted and executed for all four vault types:
- Bind (`Config::bind_live`): every vault from the factory (182 at head), type, tokens, decimals and DEX sides through Multicall3; tokens through the registry intern, native ETH as WETH.
- Every block (`liq-bot-state` thread → `AfterBlock::amend` on the ingest thread, inside the tip block's undo record): each vault's own dead-address `liquidate` / `simulateLiquidate`, with and without absorb, then the DEX's one-token estimates on a smart side. No tick math, no oracle reproduction.
- Quote: one option per token on each side; the tail carries the vault type, one-token choices, absorb, native flags and three floors from the quoted amounts (`PLAN-ENCODING.md` §1b, 98 bytes).
- Executor: T1/T2 `liquidate`, T3/T4 `liquidate` with the exact token repay; WETH unwrapped for native debt, native collateral wrapped.
- Proof: live test equal to `VaultResolver.getVaultLiquidation` and `DexResolver` estimates at the same block; fork tests on real vaults for T1 (native collateral), T2 (smart collateral taken in ETH) and T3 (smart debt repaid in USDC); unit tests for all four ABIs.
- Limits: a vault deployed after bind is read from the next restart; a one-token estimate the DEX refuses (dust, or a whole-vault size beyond one token's reserve) drops that token's option, and a smaller slice is not tried; T1 can leave one wei of debt token (its rounding); T4 has no fork test of its own (its two paths are T2's and T3's).

Morpho's liquidation incentive is the documented floor `wDivDown(WAD, WAD − wMulDown(0.3e18, WAD − lltv))` capped at `1.15e18`; the unit test pins both the 0.86e18 case and the cap.

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

**Done 2026-09-29.** Registry venue `curve_ng` with per-coin `asset_types` (schema updated); `discover_exits.py` admits NG pools whose `stored_rates()` / `offpeg_fee_multiplier()` answer, whose NG factory reports asset types without rebasing coins, and whose own `get_dy` the NG port reproduces exactly at two sizes for every ordered pair — **184 NG pools** at block 26_084_027 (97 standard, 87 with oracle / ERC-4626 rates), plus 17 plain. The router's `CurveState` gains `ng` (`get_D` divides by `N^N` once), the dynamic fee (`ng_dynamic_fee`, also in `curve_rho`), and `stored_rates()` as its rates; the reseed reads `stored_rates()` and `offpeg_fee_multiplier()` with the balances, and pools with oracle / ERC-4626 coins are re-read every block (`dynamic_rates`). NG pool logs (`AddLiquidity`/`RemoveLiquidity*` with dynamic arrays, `ApplyNewFee`) mark the pool stale. Live: all 201 Curve pools quote within 1 wei of `get_dy` (`committed_exit_pools_quote_exactly_on_chain`, 424 quotes). Fork: the Executor repays through an NG pool with standard coins (`0x4f49…3c85`) and one with rate-oracle coins (`0x1804…cc23`) — no contract change. Gas: `[swap].curve_ng = 117_442` (rate-oracle case; standard 108_535).

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

**Done 2026-09-29.** Exact integer ports (`tools/registry/crypto_math.py`, `crates/liq-router/src/crypto.rs`) of the deployed math: twocrypto-ng on `CurveTwocryptoMathOptimized` v2.0.0 / v2.1.0, tricrypto-ng on `CurveTricryptoMathOptimized` v2.0.0 (analytic cubic `get_y` with its Newton fallback, `_fee` / `reduction_coefficient`), and the original `CurveCryptoSwap2` (`newton_y` in the pool). Registry venue `curve_crypto` with `crypto_kind`; discovery admits a pool only when the port reproduces its own `get_dy` for every ordered pair at two sizes (a size the pool refuses must be refused too) — **57 live pools** at block 26_085_009 (25 original CurveCryptoSwap2, 17 twocrypto v2.1.0, 2 twocrypto v2.0.0, 13 tricrypto-ng). Not ported: twocrypto `v2.1.0d` and `v3.0.0`, the old tricrypto; dead and dust pools fail the gate. Router `PoolState::Crypto`: exact `dy`, `ρ` by forward difference, re-read every block (`D` / `price_scale` move with each trade and `tweak_price`), stale on the pool's events; a simulated swap marks the pool used for the rest of that plan. **Executor change:** venue `4` (`S_CURVE_CRYPTO_POOL`, `_swapCurveCrypto`), MetaRegistry + coin-index checks like venue 3; unit tests; fork liquidations through tricrypto-ng (`0xf5f5…e2b4`) and an original pool (`0x5fae…404a`); every admitted pool's code carries the `exchange(uint256,uint256,uint256,uint256)` selector. Proof: `crypto_vectors.rs` replays 498 recorded live quotes to the wei; the live test quotes all 57 against chain. Gas `[swap].curve_crypto = 170_472`. **Needs the Executor redeploy (not yet deployed, so it lands in the first deploy).**

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
**Step 6a done 2026-09-29: ERC-4626 vaults.** Executor venue `5` (`S_UNWRAP_4626`, `_unwrap4626`): `redeem(balance, this, this)` with the vault equal to `tokenIn` and its `asset()` equal to `tokenOut`; unit tests. Plan: unwrap legs first in a repay blob, exact input, output closed (debt, WETH, or a take-balance leg to WETH) — asserted by `liq-plan`. Router: the book holds an unwrap table (wrapper → asset, `previewRedeem` rate); a collateral with no pool of its own exits through its unwrap, the solve sells the converted amount (linear rate less 1 ppm and 1 wei) on the asset's pools — or nothing when the asset is the debt — with the rate scaling `ρ` and the unwrap gas on the route; the assembler prepends the unwrap leg. Bot: registry token field `unwrap` (asserted at boot: `asset()` unchanged), rates re-read every block by the reseed thread, gas `[swap].unwrap_4626`. Discovery `tools/registry/discover_unwraps.py` admits a vault only when a real holder's simulated `redeem` pays exactly `previewRedeem` for one share and for their whole balance, `previewRedeem` is linear in size (1k and 100k units = k × one unit, to rounding — autoETH, spPYUSD and spETH slip with size and are refused), and the asset has a pool. A vault whose liquidity falls short at the seized size fails the pre-send simulation. **Needs the Executor redeploy (lands in the first deploy).**

**Step 6b done 2026-09-30: expired Pendle PTs.** At 2026-09-29, 166 of the 181 registry PTs are past maturity, where a PT redeems 1:1 for its asset with no market: Executor venue `6` (`S_PENDLE_PT_REDEEM`, `_redeemPendlePt`): the PT and YT must name each other and the YT be expired; the PT goes to the YT, `redeemPY` pays SY, `SY.redeem(this, sy, tokenOut, 0, false)` pays the routed token (the SY refuses one it cannot pay); unit tests (`MockPendlePT/YT/SY`). Quote, from `PendleYieldToken._redeemPY` and `SYUtils.assetToSy` at the pinned source: `SY = pt · 1e18 / max(SY.exchangeRate(), YT.pyIndexStored())`, then `SY.previewRedeem(tokenOut, SY)` — linear, read every block in two rounds by the reseed thread. Registry `unwrap.kind = "pendle_pt"` with `yt` / `sy` (boot asserts `PT.YT()`, `YT.PT()`, `YT.SY()`). Discovery runs the Executor's own path on a real holder's PT (`eth_call` with the holder's code overridden by `PendleRedeemProbe`) and admits only when it pays the quote exactly for one PT and the whole balance — which refuses SYs whose redemption is permissioned (tETH's reverts `Unauthorized`). Fork liquidation through PT-sUSDE-25SEP2025. Gas `[swap].pendle_pt = 141_280`. **Needs the Executor redeploy (first deploy).** Pre-maturity PTs (15 at this date) still need the market sale below.

**Step 6c done 2026-09-30: Curve LP.** Of the 21 inventory "curve_lp" tokens only 6 are Curve LPs (the rest are Idle tranches, KNC and the like), all StableSwap-NG 2-coin pools already in the registry as swap pools. Executor venue `7` (`S_CURVE_LP_ONE_COIN`, `_withdrawCurveLp`): the pool must be the LP being spent, be in the MetaRegistry and hold `tokenOut` at `i`; `remove_liquidity_one_coin(amount, i, 0)`; unit tests (`MockCurveNgLp`). Router: `Unwrap` now carries a rate model (`UnwrapRate`): linear for vaults and expired PTs, and for an LP the exact port of NG `_calc_withdraw_one_coin` (v7.0.0, `ng_withdraw_one_coin`) on the book's own state of the pool plus the LP `totalSupply` read each block — the quote is nonlinear and exact, so the withdrawal's fee and imbalance are priced at size. Registry `unwrap.kind = "curve_lp"` (`into` = the coin with the deepest normalized balance). Discovery admits an LP only when the Python port equals the pool's `calc_withdraw_one_coin` for every coin at 1 LP and a tenth of the supply and a real holder's withdrawal pays it: **5 admitted** (USD0/bUSD0, sUSDe/sUSDS, USR/USDC, USDC/sUSDat, cUSDO/USDC); USDU/USDC (≈10 LP) has no holder that proves it. The Rust port equals the chain to the wei on all 5 at both sizes (live test). Fork liquidation through USR/USDC. Gas `[swap].curve_lp = 190_494` (the MetaRegistry check is 122_873 of it). **Needs the Executor redeploy (first deploy).**

**Step 6d done 2026-09-30: live Pendle PTs (market sale).** All 15 live PTs trade on `PendleMarketV6` markets from one factory (`0x6d24…9a9f`). `crates/liq-router/src/pendle.rs` (and `tools/registry/pendle_math.py`) port `MarketMathCore.swapExactPtForSy` with `LogExpMath` and `PMath` at pendle-core-v2-public `7c15b66e` exactly — int256 truncation, every revert the market makes (expired, proportion above 96 %, rate below one, zero LP fee). Proof: 25 live sales across 14 markets, each run from a real holder through the Executor's own path (`PendleSellProbe`, `eth_call` code override), are reproduced to the wei by both ports (`tests/pendle_vectors.rs`). Executor venue `8` (`S_PENDLE_MARKET_SELL`, `_sellPendlePt`): the market must be `isValidMarket` on the V6 factory and trade `tokenIn`; PT to the market, `swapExactPtForSy`, then the market's SY `redeem`; unit tests. Unwrap rate `Pendle(MarketSnapshot)`: `readState(0)` (no router fee override applies), the index `pyIndexCurrent` returns, and the SY's linear `previewRedeem`, read every block and quoted for the next block's time. Registry `unwrap.kind = "pendle_market"` with `market` / `yt` / `sy` (boot asserts the links and `isValidMarket`). Discovery (market from Pendle's API, then verified on chain) admits **12**: PT-reUSD, PT-sUSDS, PT-USD3, PT-sUSDE, PT-apxUSD, PT-USDat, PT-sUSDat, PT-sUSDD, PT-srUSDe, PT-trUSD, PT-strUSD, PT-fxSAVE; refused PT-srUSDat (its SY's redeem reverts), PT-apyUSD (its redeem into apyUSD fails the gate) and PT-ROY-ST-apyUSD (no routed output). Fork liquidation through PT-sUSDS-26NOV2026's market. Gas `[swap].pendle_market = 445_782` (the SY redeem into DAI is 275_577 of it). A market from a newer factory needs its math verified and an Executor update. **Expiry is automatic (2026-09-30):** when a market snapshot shows the next block at or past expiry, the reseed thread switches that PT to the post-expiry redeem (venue 6, same YT, SY and output token, `[swap].pendle_pt` gas), unrouted until the redeem rate is read — no discovery run or restart (`PoolBook::expire_pendle_market`; live-tested on PT-sUSDE-26DEC2024). A later discovery run records it as `pendle_pt`. **Needs the Executor redeploy (first deploy).**

5. **Pendle PT** *(decided 2026-09-25)*: never held. Every liquidation is flash-funded, so seized PT must become the debt asset inside the same transaction:
   - **Before maturity:** sell on the PT's Pendle market (PT → SY → underlying), then route as usual. Quote it exactly off the hot path with Pendle's on-chain quote views (RouterStatic); verify the market address against Pendle's market factory, as we verify V2/Curve pools.
   - **After maturity:** redeem 1:1 to the underlying through Pendle's redeem path (no market). This is an unwrap step.
   - Health pricing of PTs is separate: it's the protocol's own PT oracle, handled in Phase 2/3.

**Done when** the Gearbox live accounts' collateral has a quoted exit, proven by a fork liquidation of a real Gearbox account (Beefy/Curve LP) that ends in WETH profit.

---

## Phase 7: Adapter-vs-protocol-docs audit (M, run alongside)

**Why.** Your standing rule is that every adapter must match the protocol's real, deployed liquidation process, checked against official docs *and* the deployed source (Sourcify, `version()`, live probes), not just a pinned repo branch. This session's Gearbox and Aave checks both found real mismatches.

**Done 2026-09-29 — every bound protocol.** Aave V3 (docs page, `LiquidationLogic` at the pin, live pools at revision 11, all matching). Gearbox v3.1 (fixed: discovery, partial rules, full path). Each other protocol was checked rule by rule against its docs and the **deployed** source (Sourcify), recorded under `## Liquidation rules audit` in `docs/coverage/<protocol>.md`:
- **Morpho Blue:** matches; **fixed** the bad-debt fold (shares were left in the totals when `badDebtAssets` was 0). Pre-liquidation contracts (opt-in, factory `0x6FF3…3476`) are not tracked.
- **Euler V2:** matches (discount, LTV ramp, cool-off, max repay/yield, socialization).
- **Compound V2:** **fixed** the seize: `seizeInternal` keeps `protocolSeizeShareMantissa` (2.8% on 11 of 18 cTokens) — the bonus and max seize now pay what reaches the liquidator.
- **Silo V2:** **fixed** the payout: with `receiveSToken = false` the hook redeems seized shares, which the silo pays only up to its liquidity; the quote now caps there (scaled repay) or refuses when the hook forces the whole debt.
- **Liquity V2, Spark, Fluid:** match.
- **Aave V4:** all 58 deployed source files on all 13 spokes are byte-identical to the `40232a0a` pin; docs agree (target HF, Dutch-auction bonus, $1,000 dust rule).

**For each remaining protocol** (Aave V4, Spark, Morpho, Euler V2, Compound V2, Silo V2, Liquity V2, Fluid):
1. Read the official liquidation docs page, and note close factor, bonus, fees, dust and minimum rules, bad-debt handling, and who may liquidate.
2. Confirm the **deployed** version and source (Sourcify or verified source; `version()`/`REVISION()`), and diff against our pinned commit.
3. Check the adapter's quote rules against both, rule by rule. Write the findings into `docs/coverage/<protocol>.md`.
4. Fix mismatches, each with a test. Where possible, add a fork test on real contracts.

**Known items already on the list**
- **Spark** runs pre-3.5 Aave code. Its rounding (`WadRayHalfUp`) and per-reserve close factor (`ReserveDebt`) are bound, and `bind.rs::spark_and_aave_v3_are_bound_with_their_own_models_and_tokens` checks each pool gets its own. Stable debt (2026-09-29): stable borrowing is disabled and supply is 0 on all 20 Spark reserves, so it is not modelled. The adapter subscribes each `StableDebtToken`'s `Mint`/`Burn` and the configurator's `ReserveStableRateBorrowing`; an account holding stable debt fails with `UntrackedDebt` (never quoted) until a burn clears it, and each of these warns under `coverage`. `spark_conformance.rs::accrued_index_quote_is_spark_half_up_not_token_math` pins the half-up quote at an accrued index where 3.5 token math differs by 1 wei. The 20 stable tokens are not in the reth prune filter; live ExEx notifications still carry them.
- **aToken transfers (Aave V3 and Spark, fixed 2026-09-29).** `BalanceTransfer` was never subscribed, so a collateral transfer and the liquidation protocol fee (moved to the treasury by `BalanceTransfer`, not in `liquidatedCollateralAmount`) were missed, leaving phantom collateral on the borrower. Each pool's aTokens are now listed under `tokens` and subscribed. `LiquidationCall` debits collateral only when `receiveAToken` is false (burned collateral); with it, the transfer event already moved it. Tests in `aave-v3/tests/tokens.rs`.
- **Aave V4:** confirm the target-health-factor and dynamic-bonus rules against Aave's V4 docs once published, and that live spokes run the pinned `40232a0a` code.

---

## Governance liquidations (GUIDE 08 §5 `ParamChange`)

**Done 2026-09-28 (not committed).** Review of the overlay work found that a `MarketReprice` refolded Cold positions only from the canonical threshold index, so an LT/LTV cut on an overlay-priced market (Aave V3/V4, Spark, Euler) could leave a Cold position unrefolded until its old accrual crossing. `gather_rows` now walks `overlay_index` too; regression test `overlay.rs::reprice_refolds_cold_positions_priced_by_the_overlay` fails without it.

The old governance path could not fire: every real Aave payload is `execute()` delegatecalled on a payload contract with empty calldata, so the calldata classifier refused all of them, and the poller's cursor skipped any payload not yet due at first sight. `crossing.rs` and the classifier are removed. What replaces them:

- **Timing** from the deployed `PayloadsControllerCore`: `executePayload` has no access check and runs when `queuedAt + delay < block.timestamp < queuedAt + delay + gracePeriod` (strict at both ends; fork-tested on payload 469). The poller re-reads the last 64 ids every head and lists every Queued one.
- **Worker `liq-bot-gov`**: at each head where the next slot can execute a payload, `eth_simulateV1` runs `executePayload` on the head state as the next block. Replayed at block 26019517, payload 469's 12 protocol logs match the real execution in 26019518 byte for byte. It repeats every block while the payload stays Queued.
- **Hot thread** (`governance::newly_liquidatable`): the simulated logs go through each adapter's own `apply_log` into a `liq_state::Overlay` (nothing committed). The touched positions are evaluated with `Engine::probe` (health, quote, eligibility; mutates no band, index, heap or queue) with and without the change; only the newly liquidatable are planned, one account per transaction. End-to-end tests run this through the real Aave V3 adapter: an LT cut from 82.5 % to 70 % yields exactly the account it pushes below HF 1, and nothing for a cut that leaves it healthy, an account already liquidatable, or an unheld reserve.
- **Sky spells (Spark)**: Spark's `SubProxy` is warded to Sky's PauseProxy, so its parameter changes are Sky executive spells, and `DssExec.cast()` has no access check (deployed source). The worker tracks the Chief's hat each head and simulates `cast()` as the next block once `nextCastTime()` allows. Fork tests on spell `0xF01b…BaDC` at block 26076916: a contract casts it; it is castable from `eta` inclusive (1164 s before anyone did); weekends are outside office hours.
- **Aave V4**: its AccessManager's zero-delay admin is Aave's governance executor `0x5300…`, so governance changes to V4 arrive as PayloadsController payloads and are covered. Other role holders (risk stewards) act without a timelock; the reactive `MarketReprice` path covers them.
- **Executor**: header flag bit1 `GOV_EXEC` + 5-byte payload id, or bit2 `GOV_SPELL` + 20-byte spell, after the profit swaps (never both). `execute` try/catches `executePayload(id)` on the fixed PayloadsController, or `cast()` on a spell only when Sky's `DSPause` holds the plan the spell's own fields hash to. The transaction that applies the change pays its gas out of net (gas × `tx.gasprice`); one that finds it applied pays nothing, so plans no longer carry a flat surcharge. Unit and decode-fixture tests. Fork tests run the Executor itself on mainnet state (`ForkGovLiquidation.t.sol`): it applies payload 469 and casts spell 0xF01b…BaDC, each followed by a Morpho liquidation in the same `execute`; skips each when it was already applied, and the legs still run; and charges the payload's ~2.4M gas only to the transaction that ran it. Breaking the governance call or the charge turns them red. Not deployed; the redeploy is still pending.
- **Submission**: one `eth_sendBundle` for the target block, every transaction in `revertingTxHashes`, only transactions that succeed in a bundle simulation at the real gas price (compiled Executor placed at the signing address while undeployed), total gas limits capped at 15M. Same risk gate, recorder and live-send conjunction as `submit_path`.
- **Nonces**: the allocator resyncs to the operator's chain nonce once per target block; ordinary jobs and governance bundles both allocate from it, so within a block nothing reuses a nonce, and a bundle that did not land leaves no gap for the next.
- **Ordinary path without a simulator**: production binds no in-process `DrainSim`. A job whose trigger is already in committed state now goes to the exec worker marked `rpc_verify`; the worker simulates it with `eth_simulateV1` on the block before its target (Executor code placed while undeployed), sizes its gas from that, and drops it if it reverts. Triggers that need a parent transaction replayed still skip without a simulator. `ExecPath` sends to `venues.executor` once it is set, else the placeholder.
- **Second operator key (2026-10-03)**: the Executor takes `BACKRUN_OPERATOR` beside `OPERATOR`, with the same single right (`execute`). MEV-Share backruns cannot be replaced or cancelled, and with one key every bundle for a block shared one nonce sequence, so a backrun and that block's builder bundle could only both be valid if one landed first. Now the bot signs SVR backruns with the second key on nonce slot 1 (`LIQ_BACKRUN_OPERATOR_SECRET`, `operator.env`), resyncs both slots from chain each target block, and simulates each job as the key that signs it. Address `0x40807B6299C89e12a1393D8387b26DcdAA51C72a` (in `DeployExecutor.s.sol`); the key needs gas before its first send. **Needs the Executor redeploy (first deploy).**
- **Executor split (2026-10-03)**: the Executor could not deploy. Its runtime was 28,554 bytes (28,692 with the second key), over EIP-170's 24,576, and the simulator had lifted the limit. It is now a core (12,558 B) that delegatecalls `LiquidationModule` (21,767 B) and `SwapModule` (11,152 B) inside `execute`, on the core's address and balances. Sizes are at 1,000,000 optimizer runs, raised from 200 because the split left the room. The module addresses are core immutables, checked at deploy. A module refuses any call from outside `execute`, and nothing gained storage or a setter (D55-A, STATE D66). `DeployExecutor.s.sol` deploys the modules, then the core. While the Executor is undeployed, both simulators place all three contracts: in process at `0xe0…`/`0xe1…`/`0xe2…`, and on the node by state override. The local build now refuses code over the size limits. Gas, measured on the mainnet fork suite against the single contract with the same plans: +5,927 (median) per one-group liquidation with repay and profit swaps (+12,280 at first; 1,000,000 optimizer runs took off about 2,250, flash callbacks that read their group from transient storage about 1,850, and running each group from the header's decode about 2,200); a second group costs about 3,000 less than on the single contract. `config/liq-gas.toml` charges +6,000 per transaction. `.gas-snapshot` regenerated with Foundry 1.8.3. **Needs the Executor redeploy (first deploy).**
- **New swap venues, third module (2026-10-08)**: SushiSwap V3, PancakeSwap V3, Balancer V2 weighted pools and Fluid DEX T1 as exits; `DexModule` (6,017 B) holds Balancer and Fluid because `SwapModule` is at 24,147 B of 24,576. Details, oracles and gas in `docs/mechanism/dex-venues.md`. 82 pools are proposed in `data/review/2026-10-08-dex-candidates.json` (40 V3 fork, 4 Balancer, 38 Fluid), all `"approve": false`: admission is a manual step. `config/liq-gas.toml` gained `[swap] pancake_v3`, `balancer`, `fluid` (measured on the fork); `.gas-snapshot` regenerated with Foundry 1.8.3. **Needs the Executor redeploy (first deploy).** Open: Balancer V3 boosted pools (they need an ERC-4626 wrap hop); the Balancer BAL/WETH pool (not a reviewed pool); other Curve crypto kinds' `tweak_price`; Fluid pools with an active shift or a hook are not followed.
- **In-process simulator wired (2026-10-02)**: inside Reth the drain attaches `LiveSim` (`liq_sim::NodeSim`). Once the store reaches the node's head (the lease is granted), each job runs in revm on the node's own state at the store's tip, read through Reth's provider by block hash, in the next block under mainnet's fork rules at its timestamp (Osaka now; the simulator had pinned Cancun). The calls go to the address jobs are sent to (`venues.executor`, else the compiled Executor at the placeholder), profit is the WETH the Executor and its `PROFIT_SINK` gain, and the gas limit is the gas spent before refunds plus a fifth, within the 2^24 transaction cap. An SVR hint that names its sender is now replayed and sent. The RPC path above remains for the standalone binary and for the catch-up after a restart.

**H4 closed 2026-09-29.** `ExecPath::sync_nonce` stores `SubmitLease::nonce_resync` true when the chain read for the target block succeeds and false when it fails, so the third bit of the live-send conjunction follows the chain (`submit_gov.rs::nonce_resync_bit_follows_the_chain_read`). With `submit_enabled` and the lease held, POSTs start.

**Go-live (D64, 2026-09-29).** No staged rollout: `submit_enabled = true` is committed and the bot runs live under manual monitoring; the shadow and supervised gates are withdrawn (`liquidator-guides/STATE.md` D64). The tests that required `submit_enabled` to stay false were removed; `submit_enabled` stays hot-reloadable. `lockfile_pin` now pins `alloy-sol-types` 1.6.1, the version the manifest has required since the Reth integration (`50dd39b`).

**Deferred:** a governance path for the other protocols (Compound, Euler, Morpho, Liquity, Fluid, Gearbox, Silo). Their parameter changes are covered only reactively, by `MarketReprice` once the change lands.

**Audit tests.** The failing standing-balance cases were deleted on 2026-09-29 (INV-08 standing-WETH theft in `ExecutorEdgeFuzz`, `ExecutorFocusInvariants`, `OperatorRouterDrain`; the INV-08 gap and the broken INV-09 invariant in `ExecutorKnownFailProperties`): the Executor holds no standing balance in operation.

**Limits.** A spell that was the hat, was scheduled, and was then replaced as hat before this process started is not tracked until it is the hat again. Parent-transaction triggers (public transmit, pull payload) are still not sent: the simulator replays a parent only as its known sender, and a public transmit is known by hash only, a pull payload without its sender.

## Historical replay through the searcher (2026-10-04)

`crates/liq-replay/tests/historical_liquidations.rs` replays the real liquidations found in blocks 26,098,020–26,118,019 through the bot's own searcher and Executor, without a node or a running bot. For each event the pre-state is the chain's (block N−1, plus the block's earlier oracle updates when the liquidation was a backrun), read from the free endpoint and cached in `data/history-cache/`. A local JSON-RPC server serves that state to the bot's own readers. The bot is built as `startup::run` builds it: the protocol adapter, the index seeded by `startup::seed_index`, the drain, its price and state readers, and the compiled Executor simulated on the pre-state. The pool's configuration goes in as events through the bot's ingest, and the borrower comes from the adapter's resync reads, which stand in for the backfill. The job the bot builds runs on the pre-state and is valued against the real transaction (value captured before the builder payment, after the base fee).

Oracles, so the bot is never checked against itself: the pool's own `getUserAccountData` for the borrower's health (the store must agree exactly), the real transaction's event and receipt, and the chain's own views for every number a test asserts.

**Code bugs it found, fixed:**

1. **Aave V3 e-mode table full at 28.** Core had 48 categories at block 26,112,136, and the 29th `EModeCategoryAdded` failed the fold, so the bot could not follow Core at all. Categories are now kept by id in nine rows after the meta row (every `uint8` id has a place), reserves start at slot 10, and each reserve holds its e-mode sets by category id. `STATE_EPOCH` 3 rebuilds old snapshots.
2. **E-mode changes repriced no account.** A category change marked the pool's meta row, which no account holds. It now reprices the reserves that are collateral in the category.
3. **Flash sources were never loaded.** Every source started at zero and only followed Transfer deltas; Aave pools were built with no reserves, premium 0, and (Aave V3 family) no configurator; Morpho, the V4 PoolManager and Sky were left out because the registry's `flash_sources` was empty. Now each source reads its state from chain at one pinned block at startup (`FlashSource::seed_reads` / `apply_seed`, `liq_bot::flash_seed`), before routing: Aave pools in three rounds (reserve list and premium, each reserve's aToken and flags, each aToken balance). The registry has the four singleton rows (Morpho, PoolManager, `MCD_FLASH`, `MCD_END` from the Chainlog), `discover.py` writes them, and `rederive.py` uses the same kinds. Aave configurators come from `aave-v3.toml` as well as `spark.toml`. Live test: each seeded source equals direct `eth_call` reads at the pinned block.
4. **No position could become eligible.** A band came only from a candidate's pair terms, a candidate needed an exit, and an exit came only from a band. Now the engine hands out the quote of every position it holds `Unfundable`, the drain publishes their terms each block and stamps the band block last, Unfundable positions are re-folded when flash depth or the warm table moves (`on_flash_change` had no caller), and a collateral no band has evaluated is `unbanded`: the drain sizes it the same block (D67). No notional ladders.
5. **Aave dust rule dropped legs.** A leg whose close-factor cap would leave less than `MIN_LEFTOVER_BASE` was dropped. It now repays down to the floor; block 26,098,187's WBTC leg comes out at 2,465,870 sat, the real liquidator's amount, and the seize at the event's 808,710,281,724,846,850 wei.
6. **Whole-debt legs left interest behind.** The quote valued debt at the parent block's timestamp, so repaying "all" left the interest accrued until the call ran and Aave reverted `MustNotLeaveDust`. A leg that repays the whole debt (or takes the whole collateral) now asks for an hour's interest more, which Aave clamps.
7. **One position, several legs.** A position folded more than once between drains queued a candidate each time, and each became a leg in one plan: four legs, a four-fold flash, and a swap selling collateral never seized. The queue's drain now yields one candidate per job, the newest; a backrun of a different transaction stays its own job.
8. **Leg choice in the wrong units.** `best_plan` compared legs of different debt assets in their raw units, so a 6-decimal $1 token's contribution beat an 8-decimal $84k one's. It now compares in WETH wei.
9. **The profit closer took the first pool it found.** The TAKE_BALANCE closer sells the leftover collateral at no minimum, and its pool was the first live one in book order. At block 26,103,141 that was the 1 % LINK/WETH pool: 8.11 LINK fetched 0.0243 WETH, about 43 % under the oracle price, and the plan failed its own profit check. The closer now takes the pool that quotes the most for the leftover the plan expects (the reward, or the flash surplus, for the other two closers).
10. **The profit floor was the exact quote.** `minProfit` encoded the keep of the quoted outcome, so any shortfall at all reverted the plan: block 26,098,187's WBTC leg kept 0.000185 ETH against a 0.000186 floor. The floor now leaves the slack the swaps' guards already accept (`min_out_tolerance_bps` of what the exit sells), as GUIDE 12 asks ("a floor, not an expectation").
11. **The bid paid out of our own gas.** The bid is a fraction of `gross − gasCostWei`, and `gasCostWei` was the planner's estimate: block 26,098,187 budgeted 872k gas, ran 1.10M, bid 99.5 % and lost 0.00022 ETH. The drain now charges the gas the simulation measured and simulates again; a plan that is not profitable at its real gas is not sent.
12. **No exit through WETH.** The router sold collateral only through pools holding both the collateral and the debt (or an unwrap), and a collateral with no such pool had no exit at all. Every real LINK liquidation in the window went LINK → WETH → stablecoin; ours took the LINK/USDT pool at block 26,106,490 and captured 0.0424 ETH against their 0.0590 (3.3–3.5 % less on the two LINK/USDC events). The book now has a hub, WETH (D68): `solve_pair` also solves collateral → WETH → debt and takes the exit that leaves more after gas, charging the direct exit the closer hop it runs. A hub exit's repay blob sells all the collateral into WETH and buys the pull with WETH; it never shares a flash group. `liq-plan` checks leg order per token and accepts repay legs into WETH.
13. **The registry had none of the deepest V3 pools.** `discover.py` paired tracked tokens with the five hubs but skipped hub-hub pairs, and the `PoolCreated` sweep reaches back only 14 months: all 38 V3 pools among WETH, USDC, USDT, DAI and WBTC (USDC/WETH 0.05 %, WETH/USDT 0.05 %, WBTC/WETH 0.3 %, …) were missing. Block 26,098,187's real liquidator flash-swapped the WBTC/WETH 0.3 % pool; we had to go through Curve tricrypto (exact-in plus a closer, 533k more gas), and the LINK events' real liquidators used USDC/WETH 0.01 %. The scan now pairs hubs too, and every market token with every flash-loanable one (1,105 × 756 tokens at every fee tier, 2.16M `getPool` calls in 28 minutes on the free endpoint): 296 more pools, 1,344 V3 pools in all.
14. **V3 seeding dropped every pool in a refused batch.** `pool_seed` reads each pool's ticks around the price through TickLens, 150 words a call. One deep pool's window costs millions of gas (USDC/WETH 0.05 %: 1,114 populated ticks, 12.7M), so a batch over a few of them passed the node's 50M `eth_call` cap, the call failed, and every pool in it went unseeded, shallow ones with it: with the hub pairs added, 26 of 78 pools at block 26,106,490, LINK/WETH 0.3 % among them, and the plan routed around it. A refused batch is now split in halves until each part runs.
15. **The exit's pools were chosen by price alone.** The water-fill's pool set grows from the best marginal price, so a pool whose extra gas outweighs its price edge was still the first pick, and a Curve repay leg (exact input only) was not charged the closer that sells what it overbuys. At block 26,098,187, 0.8 WETH → WBTC went through Curve tricrypto plus a closer: 0.770105 WETH net against V3 0.3 % exact-out's 0.770126 (QuoterV2 on the same state), about 150k gas more. V3 0.3 % alone was also the best single pool, though the lower-fee V3 pools come first on price. The solver now charges a Curve leg in an exit into a non-WETH debt its closer hop, and weighs each pool alone and a set grown within each gas class (V2; V2 and V3; …) against the set grown on price, keeping the exit that leaves the most after gas.
16. **No flash swaps.** Every real liquidation of the window that could flash-swap did: at block 26,098,187 the WBTC/WETH 0.3 % pool paid the WBTC first and was paid the seized WETH inside its swap callback, one pool call for the flash loan and the repay swap, at the swap fee the exit pays anyway. We borrowed from the V4 PoolManager and swapped on the same pool afterwards. Now (D69): Executor provider `P_UNIV3_SWAP = 6` swaps exact-out and runs the group in `uniswapV3SwapCallback`; `liq-plan` holds such a group to one leg with no repay leg into the debt and none on the lender; the router re-solves the exit onto one V3 pool and takes the flash swap when output less what is owed less hop and wrap gas beats the cheapest loan's. Measured on the mainnet fork like the other wraps: `[wrap].univ3_swap` 75,100 against V4's 105,901, so about 31k gas and the loan's fee per plan.
17. **Per-leg tolerance did not hold for assembled plans.** `SwapModule.runSwaps` skipped only a TAKE_BALANCE leg on an empty balance, so in a flash group of several legs a beaten liquidation's own exact-output repay still ran, its pool could not be paid for collateral that never arrived, and `TransferFailed` reverted the plan. The contract tests that claimed tolerance hand-built one exact output for the live leg; the assembler emitted one per leg, the premium folded into the first. Now (D70): swap-leg flags bits 2–7 tie each repay swap to its leg and the swap module skips a tied swap whose leg did not fill; the provider's callback hands its fee to the core, and the group's first pool exact output to run buys it, so exact outputs buy each leg's pull alone; `liq-plan` asserts the ties, sizes each leg to its pull and refuses a leg that could fill alone with nothing to buy the premium; the floor is the worst landing (each flash group's weakest leg alone, its premium, the plan's gas once). `ExecutorBeatenLeg.t.sol` runs both collateral shapes with either leg beaten, both live, untied swaps still reverting, and a premium that moved after simulation.

**Open:**

- **Flash swaps, the rest.** Only Uniswap V3 pools lend this way; a V2 pair's `uniswapV2Call` is the same shape and the V2 hub pairs are deep. A debt no source in the index lends but a V3 pool sells is still unfundable: eligibility asks the index, and the flash swap is found only once a leg is being sized. A flash-swap group holds one leg, so a batch of several positions on one debt keeps the flash loan; and it has no fallback source (a re-encode onto a loan would need the repay leg the group lacks).
- **The rest of the gap to the real liquidator at block 26,098,187: 663,924 gas against 569,432, same pool, same flash swap.** Frame by frame (`LIQ_HISTORY_TRACE=1` prints both transactions): their transaction pays 226,800 for an access list of 28 addresses and 84 keys and runs its contract in 321,012; ours runs 700,012 before refunds. What we do that they do not: (1) **`getUserAccountData` before the leg, 230,198 gas** — the guard of GUIDE 10 Step 3, which Aave's `liquidationCall` repeats itself and reverts on (`HEALTH_FACTOR_NOT_BELOW_THRESHOLD`) inside the leg's `try/catch`, so for Aave V3 and V4 the read protects nothing the protocol does not; dropping it moves its oracle reads into `liquidationCall` (ours ran warm at 269,190 after the guard; theirs 235,890 after the list) and should net about 120–150k, enough to land under their 569k. Per adapter: Morpho and Euler revert on a healthy account too; Compound V2 returns an error code instead and needs the read or a return-code check. (2) **A fresh Executor's balance slots**: WBTC in costs us 32,803 against their 9,703, the WETH credit 27,962 against a warm slot — about 45k for zero→nonzero stores a contract holding one wei of each token avoids (D29 says the Executor holds WETH only between transactions; one wei of dust is the question). (3) **The sweep to the sink every transaction, 25,162** — GUIDE 12 §4c accumulates up to a threshold instead; the drain's flag is the risk budget's call. (4) The module delegatecalls and the plan decode, ~15k. Decisions, not bugs: each is a documented design choice with a measured price.
- **Hub-only pairs are not in the warm table.** `PoolBook::pairs` lists pairs with a pool of their own (and unwraps), so a collateral whose only exit into a debt is through WETH has no warm entry for that pair: it is sized on the hot path (its exit cap comes from its WETH pair), but the crude rank and the TWA haircut have nothing for it.
- **Only WETH is a hub, and an exit is direct or through it, never split between the two.**
- **Curve tricrypto2 (USDT/WBTC/WETH, `0xd51a…`) is not in the registry**; the Curve discovery does not list it.
- **Matured Pendle PTs without an unwrap.** 104 of the registry's 194 PT tokens have none, PT-USDe-27NOV2025 (block 26,112,137's collateral) among them, so their positions have no exit. `discover_unwraps.py` admits an expired PT only when a real holder's redemption matches the quote; find out why these were skipped (Phase 6).
- **Aave flash depth is `balanceOf(aToken)`.** From 3.1 a flash loan is limited by the reserve's virtual balance, which excludes donations. The simulation catches the difference; reading `getVirtualUnderlyingBalance` would remove it.
- **Seed block vs. first ingested block.** Flash and pool seeds read at the head when the process starts; logs between that block and the ExEx's first notification are missed or counted twice until the next log moves the balance. Same limit as the pool seed.
- **First sighting of a pair that is not viable** logs "select emitted no plans" once at error level.
- **Replay limits.** The harness keeps only pools whose tokens are the liquidation's or the majors (the bot routes no better than in production, possibly worse). Seeding is Aave V3 only so far; Euler, Morpho and Compound need their own. A backrun event is replayed on the post-update state through the ordinary path, so its timing is not the real one.

## Phase 8: Smaller carry-forwards (S each)

1. **Gearbox MarketId stability — done 2026-09-29.** Managers still get MarketIds in discovery order, but the state snapshot now records a fingerprint of every bound `(protocol, address, topic0)` (`state_build::bindings_fingerprint`, plus `STATE_EPOCH`). A start whose adapters bind a different set (a new or removed Gearbox manager, a new adapter, a changed event list) refuses the snapshot and rebuilds it (`startup::run_on_built_state`), so an id never moves under stored state. Bands moved: Gearbox 71000..=71999 (999 managers), Fluid 4000..=4999 (vault ids up to 999), Morpho unchanged at 5000..=70536.
2. **Aave V4 spoke cap.** The adapter tracks at most 32 spokes (13 lending spokes today). The generator ranks by debt and lists the rest; raise `SpokeFlags::MAX_SPOKES` if it ever binds.
3. **Fork-test time loops.** Under via-IR, `vm.warp(block.timestamp + dt)` in a loop can reuse a cached timestamp and never advance (this stalled the Silo test). The Aave V4 open loop, the Morpho interest loop, and the Liquity ICR loop now use a local clock, same as Silo. Single warps outside a loop were left as they are. The fork tests were not re-run.
4. **Morpho runtime markets.** The adapter assigns MarketIds from `CreateMarket` logs (catalog 5000, markets from 5001). Production never calls `MorphoBlue::backfill` (or any adapter's `Protocol::backfill`): the cold start below replays every adapter's logs through the live fold, `CreateMarket` included, from `backfill_from`. The trait method is used only by the replay harness.
9. **Reward-only liquidations — done 2026-09-30.** Liquity V2 liquidations could never fire: the adapter's quote is reward-only (`max_repay = 0`; the Stability Pool is the counterparty and the liquidator is paid gas compensation), but eligibility required a flash route for the debt (none exists for BOLD) and sizing capped every leg at `max_repay`. Now a reward-only quote is eligible on a flash-less route (`FlashProvider::None`, `CallbackShape::Direct`), sized by `profit::reward_plan` (every paid asset valued by its exit to WETH; `contribution` in WETH wei), grouped apart from funded legs, and assembled as one `P_NONE` group with no flash, no repay swaps, and a take-balance closer to WETH per paid asset; `liq-plan` refuses such a group with any flash amount, source, fee, repay swap or pull. Executor `P_NONE` (provider 5) runs the group's legs inline. Sky keeper incentives (`bark` / `redo`) use the same path. **Needs the Executor redeploy (first deploy).**
8. **Cold start — done (D65).** Production used to start with an empty store and only saw accounts that emitted an event after it started (and halted on `UnknownMarket` for markets created before). Now the first start replays every adapter's logs from `backfill_from` to the node's finalized block, writes `data/snapshot.bin` + `snapshot.head` (`liq-bot/src/state_build.rs`), and every start hands that block to Reth as the ExEx head (`catch_up_notifications_with_head`), so Reth re-executes the blocks since. Snapshots every `snapshot_every_blocks`; the lease is granted only once the store reaches the node head. The prune filter is generated from the bound adapters (GUIDE 16 §0b). **Limits:** the snapshot's head block reorged out across a restart halts at start (delete `snapshot.head` to rebuild); a stop longer than the node's 10,064-block state history needs a rebuild; an adapter the bind omits (after three rebinds of the live-bound four) refuses both the build and every start, since running without it would move the snapshot past events it never folded; the replay's `eth_getLogs` page halves on a too-large response and does not grow back.
10. **Superseded 2026-10-07: registry changes are reviewed and taken at a restart.** No pool or unwrap joins the running bot unreviewed: the registry watch is not started, Uniswap V3 `PoolCreated` discovery is off, `daily_refresh.py` writes `data/review/<date>-candidates.json` instead of the live file, and `admit_reviewed.py` admits the approved entries (GUIDE-17, "Registry changes"). What follows is the earlier design.
    **Registry refresh without restarts — done 2026-09-30.** The ExEx now follows registry changes to exits live: `registry_watch` polls `registry/registry.json`, diffs it against the running registry, asserts the additions on chain (the boot assertion on just them), adds V2 pairs (seed-guarded: a `getReserves` read older than a folded `Sync` is refused), Curve / crypto pools (forced stale at the head once routed) and unwraps (and drops removed ones), and asks the hot thread to rebuild its log router (`liq_node::Resubscribe`: the hot thread rebuilds from its handlers, publishes the address set the ExEx forwards, and acknowledges; the same path now routes Uniswap V3 pools discovered from `PoolCreated`, which before had no logs). New tokens, protocol markets, V3 registry pools and removals go to `data/review/`. `tools/registry/daily_refresh.py` + `ops/systemd/liq-discovery.timer` run the exit discovery daily with a drop safety valve (more than 10 % and at least 3 of a kind), plus a `discover.py` market scan for the review report. Live-tested: a V2 pair, a Curve NG pool and an unwrap removed from the running registry were re-added from the file without a restart.
5. **PnL ledger deferred fields.** Outcome rows are net-only today; the other fields wait on `liq-books`. Wire them when `liq-books` produces them.
6. **Gearbox margins.** The full-liquidation over-add (0.5% of account value) and partial lower-bound headroom (1%) are first guesses. Tune them from fork runs and live outcomes; the full-path surplus comes back anyway, and partial headroom trades revert risk against size.
7. **Registry families with no adapter:** `ajna` (235 markets) and `sky-maker`. Decide whether to build adapters (breadth) or drop them from the registry.
