# Handoff: finish Phases 2–8 exactly as written

Audience: the next implementer (Grok 4.7). Follow this top to bottom. Do not redesign anything. Every step says where to look, what to write, and how to prove it works. When a step says "verify on chain", do it; never guess an address, a unit or a decimals value.

Companion: `docs/plans/open-items.md` (the why). This file is the how.

---

## 0. Rules (read before touching code)

1. **Never invent data.** No guessed addresses, decimals, prices or fees. If a chain read fails, leave the item out and log it (fail closed). The codebase enforces this everywhere; match it.
2. **Every change gets a test.** Unit tests for logic; `#[ignore = "needs MAINNET_RPC_URL"]` live tests for anything that reads chain. Live tests compare our value to **the protocol's own getter at the same block**, to the wei where the math allows.
3. **Build/test commands** (Windows, Git Bash, repo root `C:/Users/Davina/Desktop/liquidation bot`):
   - `cargo fmt --all`
   - `cargo clippy --workspace --all-targets -- -D warnings` (the workspace denies `arithmetic_side_effects`, `indexing_slicing`, `unwrap_used`, `panic` in non-test code: use `checked_*`/`saturating_*`, `.get()`, `?`)
   - `cargo test --workspace --no-fail-fast` (plain `cargo test` stops at the first failing binary and hides the rest)
   - live: `cargo test -p <crate> --lib <name> -- --include-ignored --nocapture` (env `MAINNET_RPC_URL` must be set; it is an Alchemy free-tier key: 10-block `getLogs`, rate limits, no traces)
   - contracts: `cd contracts && forge test --no-match-path "test/fork/*"` (unit) and `forge test --match-path "test/fork/*"` (fork)
4. **Never run two cargo builds at once.** A concurrent build broke the `aws-lc-sys` build script once this week. Run long builds in the background and wait for them.
5. **Disk:** `target/` was 130 GB once and filled the disk. The dev profile now uses `debug = "line-tables-only"` (deps `debug = false`) and `.cargo/config.toml` links with `rust-lld`. If a build fails with "No space left", run `cargo clean` and rebuild. Don't revert those settings.
6. **Inline Python heredocs** in Git Bash break on some quoting. Write edit scripts to the scratchpad and run them with `python <file>`, or use the Edit tool.
7. **Commit style:** one sentence, present tense, what it does; short body; trailer `Co-Authored-By: <you>`. Stage files explicitly (`git add <paths>`), never `git add -A`: there are untracked scratch files from 20–22 Sep (`registry/rederive-ckpt/`, `tools/registry/_pre_triage_*.json`, `tools/registry/_triage_*.json`, `tools/sleuth/out/`) that must not be committed.
8. **`via_ir` time loops in Solidity fork tests:** never `vm.warp(block.timestamp + dt)` inside a loop. Keep a local `uint256 t` and `t += dt; vm.warp(t);`.

---

## 1. State at handoff (2026-09-25)

`origin/main` = `32113ef` (Phase 0 + Phase 1 + review fixes). Everything below in **§1.2** is **uncommitted in the working tree**, builds cleanly, and is tested except where noted.

### 1.1 Decisions already made by the user (do not revisit)
- Asset ids come from the append-only ledger `registry/asset-ids.json` (done).
- **Health prices: "getters now, reproduce later."** A background thread reads every protocol's own price getter each block, off the hot path, and lays those prices over the canonical vector per `(protocol, market)`. Chainlink/derived feeds stay the canonical vector (early warning, gas and exit valuation). Later, the hottest assets get in-process reproduction.
- Pendle PTs are never held: sell on Pendle's market before maturity, redeem after, always inside the flash-loan transaction.

### 1.2 Uncommitted work (done in this session; commit it first — §2)

| File | What it is | Status |
|---|---|---|
| `crates/liq-engine/src/engine.rs` | `ProtocolPrices` trait, `ProtocolPriceMove`, `World::overlay`, in-place overlay around every fold (`overlay_on`/`overlay_off`), second threshold index `overlay_index`, `Engine::on_protocol_prices`, `Engine::overlay_index()` | done, tested |
| `crates/liq-engine/src/lib.rs` | exports `ProtocolPriceMove, ProtocolPrices` | done |
| `crates/liq-engine/tests/overlay.rs` | 3 tests: protocol price decides health and restores canonical; Cold position caught by protocol-price move; other-market overlay ignored | passing |
| `crates/liq-engine/tests/common/mod.rs`, `crates/liq-replay/src/bench/pipeline.rs` | `overlay: None` in `World` literals | done |
| `crates/liq-protocol/src/price_read.rs` (new), `lib.rs` | `PriceRead { market, target, calldata, tag, assets }`, trait `MarketRows` | done |
| `crates/liq-protocol/src/protocol.rs` | trait methods `price_reads(&self, rows: &dyn MarketRows) -> Vec<PriceRead>` and `decode_prices(&self, read, ret, out) -> Result<()>`, both default to "none" | done |
| `crates/liq-adapters/aave-v3/src/lib.rs` | `getAssetsPrices` read per pool (Aave V3 Core/Prime/EtherFi **and Spark**, same adapter), decode ×`oracle_scale()` (=10^19) | done, **live-tested: 100 prices, WETH exact to the wei** |
| `crates/liq-adapters/aave-v4/src/lib.rs` | `getReservesPrices(reserveIds)` per spoke; reserve→asset from state rows (`slot = reserve_id + 1`), decode ×10^19 | builds; **needs a test (§3.2)** |
| `crates/liq-bot/src/protocol_prices.rs` (new) | `ReaderShared`, `collect_reads`, `read_block`, `spawn_reader` (thread `liq-bot-prices`), `ProtocolPriceBook` (implements `ProtocolPrices`), unit test + live Aave test | done |
| `crates/liq-bot/src/pool_seed.rs` | `aggregate` and `call` made `pub(crate)` (reused by the reader) | done |
| `crates/liq-bot/src/drain.rs` | fields `protocol_prices`, `protocol_moves`, `price_reader`, `reads_at`; `with_price_reader`, `protocol_price_book`, `take_protocol_prices`; `feed_engine` applies newest batch → builds `World { overlay: Some(&self.protocol_prices) }` → `sync_prices` → first batch: `engine.resync`, later: `engine.on_protocol_prices`; `zero_prices(n)` when no canonical vector loaded | done |
| `crates/liq-bot/src/startup.rs` | spawns the reader, `.with_price_reader(price_reader)` | done |
| `crates/liq-bot/src/lib.rs` | `pub mod protocol_prices;` | done |

**Not applied:** the Compound V2 reads. The exact code is in §3.3 — apply it verbatim.

### 1.3 How the overlay works (so you don't break it)
- The engine owns one canonical `PriceVector` (slot `i` = `AssetId(i)`, unit **RAY USD per whole token**). Adapters read `px.0.get(asset)` — they were **not** changed.
- Before each fold, `overlay_on` writes the position's `(protocol, market)` prices from `World::overlay` into the vector (saving the old values), the adapter evaluates, `overlay_off` restores. So `engine.prices()` is always canonical.
- A threshold on an overlaid asset registers in `overlay_index` (protocol-price units), never in the canonical `index`. `on_protocol_prices(moves)` sweeps `overlay_index.crossed(asset, old, new)` + Hot/Warm/Cool members of moved markets.
- A `(protocol, market, asset)` seen for the first time → the drain calls `engine.resync` (O(N), once at startup and when new markets appear).
- **Unit contract for every `decode_prices`:** output RAY (1e27) USD per **whole** token, exactly the unit the adapter's `health` reads from the vector. Check each adapter's price accessor (`grep -n "px.0.get" crates/liq-adapters/<name>/src/*.rs`) and the conversion it applies, and make decode the exact inverse. If the protocol quotes in something other than USD (a ratio, a unit of account, the debt token), see the per-adapter notes in §3.

---

## 2. First: commit the uncommitted work

1. `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` → must be clean.
2. `cargo test --workspace --no-fail-fast` → must be 0 failed (856+ passing).
3. `cargo test -p liq-bot --lib protocol_prices -- --include-ignored --nocapture` → prints `100 prices (80 aave-v3, 20 spark)`.
4. Stage exactly the files in §1.2 table, `docs/plans/grok-handoff.md` and `tools/gearbox/pull_feeds.py`. Commit: "Price health from each protocol's own getters, read off the hot path and laid over the canonical vector per market." Push.

---

## 3. Phase 2/3 (merged): a price reader for every adapter

Pattern for every adapter below (copy the Aave V3/V4 implementations in `crates/liq-adapters/aave-v3/src/lib.rs` and `aave-v4/src/lib.rs`, search for `fn price_reads`):
1. Add the getter to a `sol!` block in the adapter's `lib.rs`.
2. Implement `price_reads(&self, rows: &dyn liq_protocol::MarketRows) -> Vec<liq_protocol::PriceRead>` inside `impl Protocol for <Adapter>` (after `health_probe`). Fill `market` = **the `MarketId` positions of that protocol are keyed by** (`pos.key.market`; find it in the adapter's `apply.rs` where positions are created, or `config.rs` `interned`/`market` fields). Fill `assets` with the `AssetId` each answer slot prices, in order. Use `tag` for any extra per-read constant (decimals, a unit, an index).
3. Implement `decode_prices`: decode with `<Call>::abi_decode_returns(ret)`, return `ProtocolError::ProbeDecode` on any mismatch, skip zero answers (never publish 0), convert to RAY USD per whole token with `checked_*`.
4. Add an ignored live test in `crates/liq-bot/src/protocol_prices.rs` `mod tests` (copy `live_aave_reads_price_every_reserve`): bind configs with `crate::bind::load_protocols(&root.join("config"), &intern, live)` — adapters that need a live RPC at bind (Liquity, Fluid, Gearbox, Compound) need `Some((&LiveRpc, block))`; copy how `crate::bind::tests::gearbox_v31_discovery_loads_live_managers` builds `LiveRpc`. Then `read_block` at head, and compare at least one price against the protocol's own getter called directly at the same block.
5. `cargo test -p <adapter crate>` must stay green (conformance runs every adapter; the default trait methods keep other adapters unaffected).

### 3.1 How to find the market key and asset per slot, per adapter
- `grep -n "MarketId\|market:" crates/liq-adapters/<name>/src/config.rs` → config fields holding market ids.
- `grep -n "push_market\|MarketRow::blank" crates/liq-adapters/<name>/src/apply.rs` → which market id and slot each row is written to; `row.asset` is the asset of that slot.
- For adapters whose markets only exist in state (Morpho), use `rows.rows(market_id)` in `price_reads` (that's why `MarketRows` exists).

### 3.2 Aave V4 — add the missing test (code is written)
- In `protocol_prices.rs` tests, add a live test: for spoke `0xe1900480ac69f0b296841cd01cc37546d92f35cd` (it's in `config/protocols/aave-v4.toml`), call `getReserveCount()` and `getReserve(i)` (struct `(address underlying, address hub, uint16 assetId, uint8 decimals, uint24 collateralRisk, uint8 flags, uint32 dynamicConfigKey)`) on chain, build a `MarketRows` stub whose `rows(spoke_market)` returns `MarketRow::blank(intern.asset(underlying), decimals)` at index `i + 1` (index 0 any blank row), call `price_reads`, `read_block`, and assert each decoded price equals `AaveOracle(spoke.ORACLE()).getReservePrice(i)` (or `getReservesPrices([i])[0]`) × 10^19.

### 3.3 Compound V2 — apply this code verbatim
File `crates/liq-adapters/compound-v2/src/lib.rs`:
- change `use alloy_sol_types::SolEvent;` → `use alloy_sol_types::{SolCall, SolEvent};`
- replace the end of `impl Protocol for CompoundV2` (the `fn health_probe ... Err(ProtocolError::ProbeUnavailable) }` and the closing `}`) with:

```rust
    fn health_probe(&self, _pos: PositionRef<'_>) -> Result<ProbeCall> {
        Err(ProtocolError::ProbeUnavailable)
    }

    /// `PriceOracle.getUnderlyingPrice(cToken)` per cToken of each interned
    /// comptroller (no batch getter). Tag = the underlying's decimals.
    fn price_reads(&self, _rows: &dyn liq_protocol::MarketRows) -> Vec<liq_protocol::PriceRead> {
        let mut out = Vec::new();
        for fork in &self.cfg.forks {
            if fork.oracle.is_zero() {
                continue;
            }
            let Some(market) = self
                .cfg
                .interned
                .iter()
                .find(|(a, _)| *a == fork.comptroller)
                .map(|(_, m)| *m)
            else {
                continue;
            };
            for c in &fork.ctokens {
                let asset = if c.underlying.is_zero() {
                    self.cfg.native_asset(fork)
                } else {
                    self.cfg.asset_by_underlying(c.underlying)
                };
                let Some(asset) = asset else { continue };
                out.push(liq_protocol::PriceRead {
                    market,
                    target: fork.oracle,
                    calldata: alloy_primitives::Bytes::from(
                        getUnderlyingPriceCall { cToken: c.ctoken }.abi_encode(),
                    ),
                    tag: u32::from(asset.decimals),
                    assets: vec![asset.asset],
                });
            }
        }
        out
    }

    /// Compound's mantissa is `USD · 10^(36 − decimals)` per whole token;
    /// RAY is `USD · 10^27`, so `ray = m · 10^(decimals − 9)`. The adapter
    /// re-derives the mantissa keeping 18 USD decimals (`math::compound_price_mantissa`)
    /// — a sub-1e-18 relative difference on tokens under 18 decimals.
    fn decode_prices(
        &self,
        read: &liq_protocol::PriceRead,
        ret: &[u8],
        out: &mut Vec<(AssetId, liq_types::Ray)>,
    ) -> Result<()> {
        let m = getUnderlyingPriceCall::abi_decode_returns(ret)
            .map_err(|_| ProtocolError::ProbeDecode)?;
        let [asset] = read.assets.as_slice() else {
            return Err(ProtocolError::ProbeDecode);
        };
        if m.is_zero() {
            return Ok(());
        }
        let dec = read.tag;
        let ray = if dec >= 9 {
            m.checked_mul(U256::from(10u64).pow(U256::from(dec - 9)))
        } else {
            m.checked_div(U256::from(10u64).pow(U256::from(9 - dec)))
        }
        .ok_or(ProtocolError::ProbeDecode)?;
        out.push((*asset, liq_types::Ray::from_raw(ray)));
        Ok(())
    }
}

alloy_sol_types::sol! {
    /// Compound V2 `PriceOracle.getUnderlyingPrice`.
    function getUnderlyingPrice(address cToken) external view returns (uint256);
}
```
- clippy will flag `dec - 9` / `9 - dec` (`arithmetic_side_effects`): use `dec.saturating_sub(9)` / `9u32.saturating_sub(dec)` inside the branches.
- Live test: bind needs live RPC (Compound asserts its registry live). Compare cUSDC `0x39AA39c021dfbaE8faC545936693aC917d5E7563` against `getUnderlyingPrice` on the comptroller's `oracle()` at the same block: `ray == m * 10^(6-9)` → `m / 1000`.

### 3.4 Euler V2
- Where to look: `crates/liq-adapters/euler-v2/src/health.rs` (price accessor ~line 52), `config.rs` (vaults, `interned`, `unit_of_account`?), `docs/coverage/euler-v2.md`.
- Euler vaults price collateral and debt through the vault's **oracle router** in the vault's **unit of account** (often USD `0x…0348`, sometimes WETH): `IPriceOracle(vault.oracle()).getQuote(amount, base, unitOfAccount)`. **Liquidation health uses the mid-point `getQuote` with liquidation LTVs** (EVK whitepaper: "when liquidating an existing loan, the getQuote mid-point price is used"; bid/ask `getQuotes` is only for new borrows). So one price per asset is exact for liquidation: publish `getQuote`. Confirm `crates/liq-adapters/euler-v2/src/health.rs` uses liquidation LTV (`LTVLiquidation`), not borrow LTV.
- Read per `(vault = market, asset)`: `getQuote(10^decimals, asset, unitOfAccount)` → answer = unitOfAccount units per whole token. If `unitOfAccount == 0x0000000000000000000000000000000000000348` (USD, 18 decimals) → `ray = answer * 10^9`. If the unit of account is a token (e.g. WETH): price in USD requires multiplying by that token's USD price → take the unit's price from the canonical vector is NOT allowed in decode (no state). Instead: publish the **ratio** by setting the unit of account's own price to 1 (see Morpho ratio trick, §3.8) only if the adapter's health divides out the unit. If unsure, skip non-USD units and list them in a log line; don't guess.
- Market key: Euler positions are keyed by the **controller vault** market (`grep -n "key.market" crates/liq-adapters/euler-v2/src/*.rs`). Collateral vaults price through the **controller's** oracle router — use the controller vault's oracle for every collateral in that market.

### 3.5 Silo V2
- Where: `crates/liq-adapters/silo-v2/src/health.rs` (~line 51), `config/protocols/silo-v2.toml` (each pair has `silo0/silo1` with `solvency_oracle`), `docs/mechanism/silo-v2.md`.
- Health uses `solvencyOracle.quote(amount, token)` → answer in the pair's **quote token** (the fork test used oracle `0x05b843…` returning WETH per wstETH, `quoteToken()` = WETH). A zero-address oracle means the token is priced 1:1 in the quote token.
- Read per silo: `quote(10^decimals, token)` on `solvency_oracle`. Market key = the debt silo's interned market (config `market = 3427` etc.). Publish the collateral price as `quote × price(quoteToken)`: since decode has no state, emit **both** assets for the market in quote-token units with the quote token priced at `10^27` (1.0) — health only uses the ratio collateral/debt when the debt token **is** the quote token. Verify that by reading `silo-v2/src/health.rs`; if the adapter needs USD for sizing (debt_value), confirm with `grep -n "value_wad\|debt_value" crates/liq-adapters/silo-v2/src/*.rs`. If USD is needed, skip the overlay for Silo and note it (canonical prices stay).

### 3.6 Gearbox V3.1
- Where: `crates/liq-adapters/gearbox/src/health.rs` (`price_ray`, line ~59), `config.rs` (`ManagerConfig` — add a `price_oracle: Address` field read from `ICreditManagerV3.priceOracle()` in `load_manager`, same style as `pool`/`quota_keeper`).
- Gearbox health (`calcDebtAndCollateral`) prices every token through the manager's `PriceOracleV3.convertToUSD(amount, token)` (8 decimals USD). Read per manager: for each token `getPrice(token)` → 8-decimal USD per whole token → `ray = p * 10^19`. **Caveat:** v3.1 oracles can use on-demand (Redstone pull) feeds that revert without price updates; a reverting read is just "no price" (the batch counts it as failed) — correct.
- Market key = manager `market` (`ManagerConfig.market`).

### 3.7 Liquity V2 and Fluid
- **Liquity V2:** three branches (WETH/wstETH/rETH). Where: `crates/liq-adapters/liquity-v2/src/config.rs` (`price_feed` per branch, line ~64), `health.rs` (~line 42). Liquidations call `PriceFeed.fetchPrice()` (state-changing but callable via `eth_call`; returns `(uint256 price, bool newOracleFailureDetected)`, price = USD × 1e18 per whole collateral). Read `fetchPrice()` per branch → `ray = price * 10^9`. BOLD (the debt) = 1 USD in Liquity's own math: publish BOLD at `10^27` for every branch market. If `newOracleFailureDetected` is true, skip that branch (branch shutdown rules differ; see open-items Phase 3b step 3).
- **Fluid (T1 vaults):** where: `crates/liq-adapters/fluid/src/health.rs` (~line 47), `config.rs`. Vault oracle `getExchangeRateLiquidate()` returns **collateral per debt? or debt per collateral** at 1e27 — read the Fluid oracle interface in `docs/mechanism/fluid.md` and the adapter's `health.rs` to see which direction it expects, then publish as a ratio (debt asset priced 10^27, collateral = rate or 1/rate in RAY). Test against the vault's `operateRate`/`liquidateRate` getter at the same block.

### 3.8 Morpho Blue (ratio trick)
- Where: `crates/liq-adapters/morpho-blue/src/health.rs` (`price_ray` ~line 40 and `oracle_price(p_coll, p_loan, …)` right below it — it **reconstructs** `IOracle.price()` from two per-asset prices). Config pins: `config/protocols/morpho-blue.toml` `[[price_sources]]` `(oracle, collateral, loan)`.
- Morpho `MarketId`s are assigned at runtime from `CreateMarket` (catalog 5000, markets 5001+). In `price_reads(rows)`: walk markets starting at `first_market` (5001) while `rows.rows(MarketId(n))` is `Some`; for each market read its oracle and loan/collateral assets from the row bodies (`grep -n "struct LoanRow\|struct CollRow\|morpho_id\|oracle" crates/liq-adapters/morpho-blue/src/layout.rs`). Emit one read per market: target = market oracle, calldata = `price()` (returns uint256, collateral in loan units scaled 1e36 × 10^(loanDec − collDec)), `assets = [loan, collateral]`, `tag` = packed decimals `(loanDec << 8) | collDec`.
- Decode: invert `oracle_price` exactly. Price the loan at `P_loan = 10^27` (1.0) and solve for `P_coll` so that `oracle_price(P_coll, P_loan, …) == price()` — read the exact formula in `health.rs` and implement its inverse with the same rounding (write a unit test: for random `price()` values, `oracle_price(inverse(price), 10^27) == price()`). Because Morpho health only uses the ratio, pricing the loan at 1.0 is exact for liquidation; USD sizing downstream uses canonical prices (see §3.9).
- Live test: 20 markets from `morpho-blue.toml`, compare reconstructed `oracle_price` to `IOracle(oracle).price()` at the same block, to the wei.

### 3.9 Make sizing see protocol prices where canonical has none
`crates/liq-bot/src/drain.rs`: `publish_per_eth` and `publish_pair_terms` read `feed.merged` (canonical). Most Aave reserves have no canonical feed, so no band/pair terms → never sized. Fix:
1. Add to `ProtocolPriceBook` a method `fn usd(&self, asset: AssetId) -> Option<Ray>` returning the first USD-denominated price for the asset across Aave V3/V4/Spark/Compound/Gearbox markets (skip ratio-priced protocols: Morpho, Silo, Fluid, Liquity — mark them with a `usd: bool` flag per `(protocol, market)` set by the reader; simplest: `PriceBatch` entries carry the protocol; keep a `HashSet<ProtocolId>` of ratio protocols in the book built from ids via `bind::resolve_family`).
2. In `publish_per_eth` / `publish_pair_terms`, when `feed.merged` has `ts == 0` for an asset, fall back to `self.protocol_prices.usd(asset)`.
3. Test: a drain unit test (see existing `per_eth_scales...` tests in `drain.rs`) where canonical lacks asset X and the book has it → pair terms published.

### 3.10 Phase 2 "reproduce later" (do after 3.1–3.9)
Per open-items Phase 2 steps 1–4: inventory which Aave sources are plain Chainlink proxies (`config/protocols/aave-v3.toml` `[[price_sources]]`; a source is plain Chainlink if `aggregator()` answers and `decimals()` = 8). Add those to `config/feeds/aave-v3.toml` (copy the six entries added 2026-09-25 at the bottom of that file: heartbeat/deviation from `registry/feeds-mainnet.json` matched by `contractAddress`) so the canonical vector reacts to `AnswerUpdated` logs in-process. Keep the getter reader as the authority.

---

## 4. Phase 4 — Curve StableSwap-NG exits

Files: `crates/liq-router/src/solver.rs` (`CurveState`, `curve_get_d`, `curve_get_y`, `curve_dy` ~lines 575–800), `crates/liq-bot/src/index.rs::load_book`, `crates/liq-bot/src/pool_seed.rs` (`read_curve`, `refresh_curve`), `tools/registry/discover_exits.py` (`discover_curve`, `curve_get_dy`).

1. Source: `https://raw.githubusercontent.com/curvefi/stableswap-ng/main/contracts/main/CurveStableSwapNG.vy` (pin the commit you read in a comment). Copy exactly: `get_D` (NG: `D_P = D; for x in xp: D_P = D_P * D / x; D_P /= N**N`), `get_y`, `_dynamic_fee(xpi, xpj, _fee)` = `_fee` if `offpeg_fee_multiplier <= FEE_DENOMINATOR` else `offpeg * _fee / ((offpeg - FEE_DENOMINATOR) * 4 * xpi * xpj / (xpi + xpj)**2 + FEE_DENOMINATOR)`, and `exchange`: `dy = xp[j] - y - 1; dy_fee = dy * _dynamic_fee((xp[i]+x)/2, (xp[j]+y)/2, fee) / FEE_DENOMINATOR; dy = (dy - dy_fee) * PRECISION / rates[j]`.
2. `CurveState`: add `offpeg_fee_multiplier: U256` and `ng: bool` (default false; update every literal: `grep -rn "CurveState {" crates`). Branch in `curve_get_d`/`curve_dy` on `ng`. Unit tests with recorded on-chain `get_dy` values for 2 NG pools.
3. Rates: `stored_rates()` (returns `uint256[]`) at every reseed; for asset types 1 (oracle) and 3 (ERC-4626) rates move without logs → in `pool_seed::curve_targets` treat NG pools with non-static rates as always stale (reseed every poll).
4. Discovery: in `discover_curve`, stop rejecting `stored_rates()`/`offpeg_fee_multiplier()` pools; add an NG branch to the Python math and the exact-match check (`get_dy` equality; the Rust side models `exchange`, which may be 1 wei under `get_dy` — see `committed_exit_pools_quote_exactly_on_chain` in `pool_seed.rs`). Registry entries get `"ng": true` → add `#[serde(default)] pub ng: bool` to `PoolEntry` (`crates/liq-config/src/registry.rs`) and `"ng": {"type":"boolean"}` to `registry/schema.json` `$defs.pool.properties`.
5. Executor: NG pools use the same `exchange(int128,int128,uint256,uint256)` and are in the MetaRegistry → no contract change. Prove with a fork test in `contracts/test/fork/ForkLiveLiquidations.t.sol` copying `test_fork_v3_usdc_coll_dai_repay_via_curve_3pool` with an NG pool (find one with USDC/USDT or USDe/USDC from the discovery output).
6. Gas: `forge test --match-test <new test> --isolate -vvvv > tools/gas-measure/fork-trace-ng.txt`, `python tools/gas-measure/fork_decompose.py tools/gas-measure/fork-trace-ng.txt`, add `curve_ng = <swap frame>` under `[swap]` in `config/liq-gas.toml`, extend `HopGas` in `crates/liq-bot/src/gas_model.rs`.

## 5. Phase 5 — Curve crypto pools
1. Sources: `curvefi/twocrypto-ng` and `curvefi/tricrypto-ng` (`get_y` Newton on the gamma curve, `_fee(xp)` from `mid_fee/out_fee/fee_gamma`, `price_scale`). Port to `PoolState::Crypto` in `solver.rs` with exact integer math; test against recorded `get_dy`.
2. Contract: add venue `S_CURVE_CRYPTO = 4` in `contracts/src/Executor.sol` next to `S_CURVE_POOL` (copy `_swapCurve`, but call `exchange(uint256,uint256,uint256,uint256)` and verify `coins(uint256)` the same way); `VENUE_CURVE_CRYPTO = 4` in `crates/liq-wire/src/wire.rs`, re-export in `crates/liq-plan/src/types.rs`/`lib.rs`, validate in `crates/liq-plan/src/validate.rs::check_swap`, encode in `crates/liq-router/src/assemble.rs::venue_bytes`. Unit tests in `contracts/test/unit/ExecutorVenues.t.sol` (copy the Curve tests), fork test, gas. **New Executor deploy required** — batch with Phase 6.

## 6. Phase 6 — unwrap exits (Beefy/Curve LP/Balancer/Pendle PT)
Pattern already in the Executor: `_redeemEulerShares` / Compound redeem (search `Executor.sol` for `redeem`).
1. Inventory: `python tools/registry/...`: for each live Gearbox account (script `gearbox_accounts.py` logic: list accounts, `calcDebtAndCollateral(ca, 3)`, balances of enabled tokens), and each Aave/Morpho collateral without a direct pool, classify: Beefy `wmoo*` (ERC-4626-like wrapper → `redeem`/`withdraw` to the Beefy vault share → `withdrawAll` to the LP), Curve LP (`remove_liquidity_one_coin(amount, i, min)`), Balancer BPT (`Vault.exitPool` single-token), Pendle PT (before maturity: Pendle Router `swapExactPtForToken`; after: `redeemPyToToken`).
2. Plan format: add a leg type `UNWRAP` (kind u8, target, token_in, token_out, min_out) to `crates/liq-wire/src/wire.rs`, `contracts/src/lib/PlanDecoder.sol`, executed after the liquidation leg and before swaps. Each kind verifies its target on chain (Curve LP: MetaRegistry `get_pool_from_lp_token`; Beefy: `want()` equals the expected token; Pendle: market/SY/PT from Pendle's factory `isValidMarket`).
3. Router: the exit for a wrapped token = exact unwrap preview (`previewRedeem`, `calc_withdraw_one_coin`, Balancer `queryExit`, Pendle RouterStatic) chained into `solve_pair`.
4. Done when: a fork test liquidates a real Gearbox v3.1 account holding `wmooCurveETH+-WETH` (manager `0x79c6C1ce5B12abCC3E407ce8C160eE1160250921`, use `ForkSiloGearbox.t.sol`'s LT-lowering fixture) and ends in WETH profit.

## 7. Phase 7 — adapter vs protocol docs audit
For each of Aave V4, Spark, Morpho, Euler, Compound, Silo, Liquity, Fluid: (1) read the official liquidation docs page; (2) confirm deployed version (Sourcify `https://sourcify.dev/server/v2/contract/1/<addr>?fields=sources`, `version()`/`REVISION()`/`POOL_REVISION()`); (3) diff rules against the adapter's `quote.rs` (close factor, bonus, fees, dust/min rules, bad debt, who may liquidate); (4) write findings into `docs/coverage/<protocol>.md`; (5) fix with a test.
Known: **Spark runs pre-3.5 Aave** (`config/protocols/spark.toml` header): our V3 adapter ignores Spark's pre-3.5 events and uses 3.5 rounding and total-debt close-factor caps — needs a Spark event profile.

## 8. Phase 8 — carry-forwards
1. Gearbox MarketId stability: persist manager→MarketId in a committed JSON like `asset-ids.json`; widen the 4201–4299 band (70 used).
2. Morpho cold start: verify startup backfill replays `CreateMarket` from Morpho's deployment block (18883124) so markets 5001+ exist before §3.8 reads.
3. PnL ledger fields waiting on `liq-books`.
4. Tune Gearbox margins (`FULL_MARGIN_BPS = 50`, `PARTIAL_MIN_MARGIN_BPS = 100` in `crates/liq-adapters/gearbox/src/quote.rs`) from fork runs.
5. Decide `ajna` (235 markets) / `sky-maker`: build adapters or drop from registry (ask the user).
6. Gearbox pull price feeds: G1 alarm now; G2–G6 when triggered (§8a).

## 8a. Gearbox pull price feeds (Redstone, Pyth): decision and implementation

### What they are
Some Gearbox tokens are priced by **pull feeds** (`PRICE_FEED::REDSTONE`, `PRICE_FEED::PYTH`, in `Gearbox-protocol/oracles-v3` `contracts/oracles/updatable/`). The feed only stores the last price someone pushed. A signed price is pushed with the permissionless `updatePrice(bytes data)`: Redstone data = `abi.encode(uint256 expectedPayloadTimestamp, bytes payload)`, Pyth data = `abi.encode(uint256 expectedPublishTimestamp, bytes[] updateData)`. Accepted window: 10 minutes behind to 1 minute ahead of `block.timestamp`. `latestRoundData()` on a Redstone feed does not check staleness; `PriceOracleV3` checks it against the token's `stalenessPeriod` and reverts when stale. A stale pull-fed token therefore makes `getPrice` revert, and an account holding it can't be priced or liquidated unless the liquidation carries a fresh signed price.

In the liquidation call: `partiallyLiquidateCreditAccount(..., PriceUpdate[] priceUpdates)` takes the updates directly (our Executor passes `none` today, `contracts/src/Executor.sol:899`). For the full `liquidateCreditAccount`, `onDemandPriceUpdates(PriceUpdate[] updates)` must be the **first** call in the multicall ("Reverts if placed not at the first position"). `struct PriceUpdate { address priceFeed; bytes data; }` (already in `contracts/src/lib/Interfaces.sol:218`). Gearbox dev-docs: "If the token with an on-demand feed is disabled by the end of the multicall, then the price update does not need to be included."

### Measurement (2026-09-25, mainnet, `tools/gearbox/pull_feeds.py`)
- 70 v3.1 credit managers across 12 price oracles; 8 have debt, about $12.7M in total: wstETH ≈ 2,986, WETH ≈ 911, USDC ≈ 92,236, WBTC ≈ 2.47.
- The oracles behind the wstETH, WETH and WBTC managers (`0x6C10935f…`, `0x3B337BA1…`, `0x2fB170D7…`) have **no pull-fed tokens**.
- The USDC manager's oracle `0x991A5028…` has 6 pull-fed tokens: CRV, pmcrvUSD, stkcvxpmcrvUSD, stkcvxllamathena (Redstone), and pmfrxUSD, stkcvxpmfrxUSD (Pyth). CRV is not allowed collateral in that manager (`getTokenMaskOrRevert` reverts). All 33 accounts there have enabled mask 1 (USDC) or 9 (USDC + RLP, not pull-fed).
- Every pull feed found is abandoned: Redstone last updates are 55 to 290 days old, and the Pyth feeds revert on read.
- Gearbox's own liquidator removed Redstone support on 2026-09-23 (`liquidator-v2` v10.0.0: "remove redstone support", "updatable price feeds are deprecated").
- **Result: today no Gearbox debt needs a signed price to be liquidated.**

### Decision
Coverage is the goal, but this path currently unlocks $0, and Gearbox is retiring it. So:
1. **Now (required): build the alarm (G1 below).** The bot must know when an account with debt holds a pull-fed token, and must say so loudly rather than silently skip it. Cheap, and it turns the rest into a data-driven trigger.
2. **Build the payload path (G2–G6) only when the alarm fires,** or when the user explicitly says to build it anyway. It is fully specified below, so it can start the same day.

If the user asks for it regardless, do G1 through G6 in order. There is no live account to test against, so the fork test uses a position you open yourself (G5).

### G1 — Pull-feed inventory and alarm (do now)
1. `crates/liq-adapters/gearbox/src/events.rs`, in the views `sol!`: add `function priceOracle() external view returns (address);` (credit manager), `function priceFeeds(address token) external view returns (address);` (PriceOracleV3), `function contractType() external view returns (bytes32);`, `function priceFeed0() external view returns (address);`, `function priceFeed1() external view returns (address);`, `function priceFeed() external view returns (address);`, `function underlyingPriceFeed() external view returns (address);`, `function updatable() external view returns (bool);`.
2. `config.rs`: `ManagerConfig` gains `price_oracle: Address` (also needed by §3.6). Each token entry gains `pull: bool`. In `load_manager`, per token: `priceFeeds(token)`, then walk children with the same recursion as `tools/gearbox/pull_feeds.py::pull_leaves` (depth ≤ 5; a call that reverts means no child). Set `pull = true` if any leaf's `contractType` contains `REDSTONE` or `PYTH` or it answers `updatable() == true`. Keep the leaf addresses (`pull_feeds: Vec<Address>`); G3 needs them. Update the fixture manager in `crates/liq-adapters/gearbox/tests/common/mod.rs` and `mock_registry` answers (`priceOracle`, `priceFeeds`, `contractType` → a plain Chainlink type, so `pull = false`), plus one test with a Redstone leaf → `pull = true`.
3. Health: the adapter already fails closed on an unpriced token. Add one thing. When an account with debt has a `pull` token in its enabled mask, emit `tracing::warn!(target: "coverage", manager, account, token, "gearbox account needs a pull-feed price payload — not liquidatable by this bot")` at most once per account per hour. Also count it in the metrics the bot already exports (`grep -rn "metrics::" crates/liq-bot/src | head`, and follow the existing counter style).
4. Test: an adapter unit test where the enabled mask includes a pull token → the position reports unpriced (no candidate) and the warning path runs.
5. Re-run `python tools/gearbox/pull_feeds.py` monthly, and whenever the alarm fires.

### G2 — Signed-price source (only when triggered)
New module `crates/liq-bot/src/pull_prices.rs`, a background thread `liq-bot-pull` (same pattern as `protocol_prices.rs::spawn_reader`).
- **Redstone:** `GET https://oracle-gateway-1.a.redstone.finance/data-packages/latest/redstone-primary-prod`, falling back to `oracle-gateway-2.a.redstone.finance`. Verified 2026-09-25: it returns a JSON object keyed by data-feed id (983 keys, ~1.9 MB). Each value is an array of packages `{timestampMilliseconds, signature (base64, 65 bytes r‖s‖v), dataPoints: [{dataFeedId, value}], dataServiceId, dataPackageId, signerAddress}`; ETH had 5 packages. Poll every 10 s (the payload is valid for 10 min). Keep only the feed ids our pull feeds use.
- Per Redstone feed, read at bind: `dataFeedId()` (bytes32), `dataServiceId()` (string), `getUniqueSignersThreshold()` (uint8), and the authorised signers. The signers are immutables; VERIFY the getter names in `RedstonePriceFeed.sol` (likely `signerAddress0()`…`signerAddress9()`). Keep only packages whose `signerAddress` is authorised. All packages sent together must have **the same timestamp** (`TimestampsMustBeEqual`), so pick the newest timestamp that has ≥ threshold authorised signers.
- **Pyth:** `GET https://hermes.pyth.network/v2/updates/price/latest?ids[]=<priceFeedId>&encoding=hex` gives `binary.data[]` as `updateData`. The feed pays `pyth.getUpdateFee(updateData)` from **its own balance** (`IPyth(pyth).updatePriceFeeds{value: fee}`). Check the feed's ETH balance at bind; if it can't pay, mark the token unliquidatable (and alarm). Per feed read `priceFeedId()`, `pyth()`, and `maxConfToPriceRatio`; `latestRoundData` reverts when the confidence is too wide.

### G3 — Payload encoding (Rust, exact bytes)
RedStone payload layout. Reference: `redstone-finance/redstone-oracles-monorepo` `packages/evm-connector/contracts/core/RedstoneConstants.sol` (`SIG_BS = 65`, `TIMESTAMP_BS = 6`, `REDSTONE_MARKER_BS = 9`, `DATA_POINT_SYMBOL_BS = 32`, marker `0x000002ed57011e0000`) and `CalldataExtractor.sol`. VERIFY the order against those files before coding:
- Each data package: `[dataFeedId bytes32 ‖ value uint256]` per data point, then `timestamp` (6 bytes, **milliseconds**), then `valueByteSize` (4 bytes = 32), then `dataPointsCount` (3 bytes), then `signature` (65 bytes).
- Payload: `packages… ‖ packagesCount (2 bytes) ‖ unsignedMetadata ‖ unsignedMetadataByteSize (3 bytes) ‖ marker (9 bytes)`.
- `value` on chain is an integer with 8 decimals: the gateway gives a decimal number, so convert with exact decimal arithmetic (parse the JSON number as a string, never through f64). Some feeds give a base64 byte string instead; handle both.
- **Offline self-check (mandatory unit test):** `ecrecover(keccak256(package bytes without the signature), signature) == signerAddress` for every package in a saved gateway fixture (`crates/liq-bot/tests/fixtures/redstone_eth.json`, saved from the live gateway). If this fails, the value or byte order is wrong. Don't proceed until it passes.
- The feed reads the payload **from the end of calldata**, but `abi.encode(uint256, bytes)` pads `bytes` to 32 bytes, and trailing zero padding would hide the marker. Make the payload length a multiple of 32 by padding **inside unsigned metadata** (and set its byte size accordingly). VERIFY this against the pre-v10 `Gearbox-protocol/liquidator-v2` tag `v9.6.1`, or the `Gearbox-protocol/sdk` source before its updatable-feed removal: search for `redstone`, `unsignedMetadata`, `PriceUpdate`.
- `data = abi.encode(uint256(timestampMs / 1000), payload)`; `PriceUpdate.priceFeed` = the **leaf** pull feed. VERIFY this for composite feeds in v3.1: read how the v3.1 credit facade forwards `onDemandPriceUpdates` (to the market's `PriceFeedStore.updatePrices`, which may require the feed to be registered). `IPriceFeedStore` has `updatePrices(PriceUpdate[])` and `getStalenessPeriod(address)`.

### G4 — Exact pricing and health (reproduce nothing)
Don't reimplement median or composite math. Add `contracts/src/lens/PullPriceLens.sol` with `function priceAfter(PriceUpdate[] calldata u, address oracle, address[] calldata tokens) external returns (uint256[] memory)`: call `IUpdatablePriceFeed(u[i].priceFeed).updatePrice(u[i].data)` for each update, then `IPriceOracleV3(oracle).getPrice(token)` for each token. Never deploy it: run it with `eth_call` plus a **state override** that puts its runtime bytecode at a fixed address (reth and Alchemy both support `stateOverride`; embed the bytecode with `include_bytes!` from the forge build output). The `liq-bot-pull` thread does this each block for pull-fed tokens of managers with debt, and publishes the answers as Gearbox overlay prices (§1.3, §3.6 units: 8-decimal USD × 10^19 → RAY). The health that results is exactly what the protocol computes after our update.

### G5 — Executor, plan wire, assembly, tests
1. Plan tail (Gearbox leg), backward compatible: `minSeized (32) ‖ mode (1) ‖ nUpdates (1) ‖ { feed (20) ‖ len (2) ‖ data (len) }*`. A 33-byte tail means no updates. Update `contracts/src/lib/PlanDecoder.sol::tailGearbox`, `crates/liq-wire` (tail encoder and its length checks), `crates/liq-plan/src/validate.rs` (bounds: `nUpdates ≤ 8`, `len ≤ 4096`) and `crates/liq-router/src/assemble.rs` (fill from `pull_prices` at plan-build time with the **freshest** payload; drop the plan if the newest payload is older than 8 minutes).
2. `Executor.sol`: mode 0 passes the decoded `PriceUpdate[]` as `priceUpdates`. Mode 1 grows `calls` by one and sets `calls[0] = MultiCall({target: l.market, callData: abi.encodeCall(ICreditFacadeV3Multicall.onDemandPriceUpdates, (updates))})` (add that function to `Interfaces.sol`). The updates must be the first call.
3. Tests:
   - Unit (`contracts/test/unit/`): decoding the tail with 0, 1 and 2 updates; calls[0] is the update in mode 1.
   - Fork (`contracts/test/fork/ForkSiloGearbox.t.sol` pattern): pin a block, save a gateway payload captured within 60 s of that block's timestamp as a fixture (`contracts/test/fixtures/redstone_<block>.json`), `vm.warp(payloadTs + 5)`, open an account in a manager that allows a Redstone-fed token, lower that token's LT (the existing LT-lowering fixture), then liquidate with and without the update: without it must revert (stale price), with it must succeed.
   - Rust live: G4's lens on a real pull feed at head returns the gateway median ± 0.
4. Gas: `forge test --match-test <fork test> --isolate -vvvv`, then `tools/gas-measure/fork_decompose.py`. Add `[gearbox] pull_update_per_feed = <measured>` to `config/liq-gas.toml`, and add it per update in the cost model (`crates/liq-bot/src/gas_model.rs`). Calldata for 5 signed packages is ~0.9 KB per feed; count its calldata gas.
5. Redeploy the Executor, batched with Phase 5/6 contract changes.

### G6 — Done when
The alarm is live (G1). If triggered: the offline signature self-check passes, the lens price equals the post-update `getPrice`, the fork liquidation succeeds only with the update, the gas is measured, and `tools/gearbox/pull_feeds.py` shows the exposed accounts now reach the candidate list.

## 9a. Official liquidator docs per protocol (read before coding that adapter)

Found 2026-09-25. Order of authority: **deployed source** (Sourcify / the version tag actually deployed) > protocol repo interface natspec > official docs page > official bot repo. Official bots are references for *what to check*, not for our math.

### Morpho Blue
- Docs: https://docs.morpho.org/learn/concepts/liquidation/ and https://docs.morpho.org/developers/ecosystem/liquidation-bots/
- Official bot: https://github.com/morpho-org/morpho-blue-liquidation-bot (RPC-only; read its data provider for how it lists markets/positions).
- Rules to match (quoted from the docs): `COLLATERAL_VALUE_IN_LOAN_TOKEN = COLLATERAL_AMOUNT × ORACLE_PRICE / ORACLE_PRICE_SCALE` with `ORACLE_PRICE_SCALE = 10^36`; liquidatable when `LTV > LLTV`; `LIF = min(M, 1/(β×LLTV + (1−β)))`, `β = 0.3`, `M = 1.15`; seized = repaid × LIF at the oracle price; up to 100% of debt per call.
- What it means for §3.8: health depends only on `price()`, so pricing the loan at 1.0 and the collateral at the inverse is exact. Also check `crates/liq-adapters/morpho-blue/src/quote.rs` LIF against the formula above (integer version is `wDivDown(WAD, WAD − wMulDown(0.3e18, WAD − lltv))` capped at 1.15e18 in `Morpho.sol`).

### Euler V2 (EVK)
- Whitepaper (the spec): https://github.com/euler-xyz/euler-vault-kit/blob/master/docs/whitepaper.md, "Liquidation" and "Price oracles" sections.
- Docs: https://docs.euler.finance/concepts/risk/liquidations/ and https://docs.euler.finance/developers/evk/
- Official bot: https://github.com/euler-xyz/liquidation-bot-v2 (finds accounts from EVC `AccountStatusCheck` events; simulates one liquidation per collateral).
- Rules: liquidation uses **mid-point `getQuote`** and **liquidation LTV** in the vault's unit of account; discount grows with how deep the violation is, capped by `maxLiquidationDiscount`; a **cool-off period** means an account that passed a status check this block cannot be liquidated in the same block; with debt socialization on, leftover debt after all collateral is seized is cancelled; the liquidator **receives collateral vault shares and takes on the debt** (it must then repay, e.g. inside an EVC batch).
- Use in tests: `checkLiquidation(liquidator, violator, collateral)` returns `(maxRepay, maxYield)`; our quote must equal it at the same block.

### Fluid
- Liquidation swaps guide: https://docs.fluid.instadapp.io/integrate/liquidation-swaps.html
- Resolver (ground truth for tests): https://docs.fluid.instadapp.io/autogenerated-docs/periphery/resolvers/vaultLiquidation/main.sol/contract.FluidVaultLiquidationResolver.html. `getVaultSwapData(vault)` returns the exact available liquidation with and without absorb; `exactInput(tokenIn, tokenOut, inAmt)` sizes it. Note: "withAbsorb = true" consumes the liquidity the withoutAbsorb swap would use.
- Vault interface: https://docs.fluid.instadapp.io/autogenerated-docs/protocols/vault/interfaces/iVaultT1.sol/interface.IFluidVaultT1.html
- Call: `liquidate(uint256 debtAmt_, uint256 colPerUnitDebt_, address to_, bool absorb_) returns (uint actualDebtAmt_, uint actualColAmt_)`; `colPerUnitDebt_` = min collateral per unit of debt in 1e18 (slippage).
- Still unverified: the direction of `getExchangeRateLiquidate()` (§3.7). Read the deployed oracle source on Sourcify, then assert in the live test that our Fluid quote equals `getVaultSwapData(vault).withoutAbsorb` at the same block.

### Liquity V2
- The spec is the repo README: https://github.com/liquity/bold/blob/main/README.md (sections on liquidations, gas compensation, PriceFeed, shutdown, zombie troves). Docs: https://docs.liquity.org/
- Rules: `batchLiquidateTroves(uint256[] _troveArray)` skips troves with ICR ≥ MCR. **The liquidator repays nothing**: the Stability Pool offsets the debt (redistribution when the pool is empty) and the liquidator receives only `ETH_GAS_COMPENSATION` (0.0375 WETH per trove) plus min(0.5% of collateral, 2 units), and that collateral part is paid **only for the SP-offset share**. The pool always keeps ≥ 1e18 BOLD. Zombie troves (debt < MIN_DEBT after redemption) stay liquidatable.
- Prices: `fetchPrice()` per branch: WETH = ETH-USD; wstETH = stETH-USD × exchange rate; rETH = min(market, exchange rate). On oracle failure the branch shuts down and uses `lastGoodPrice`.
- For §3.7: after a shutdown, `fetchPrice` keeps returning `lastGoodPrice`, and liquidations still use it. So publish it, and only skip a branch when the read reverts. Also confirm our Liquity profit model counts gas compensation only (no bonus spread) with no flash loan needed. VERIFY in `crates/liq-adapters/liquity-v2/src/quote.rs`.

### Silo V2
- `docs.silo.finance` now shows **Silo V3**. Use the V2 repo at the deployed tag: https://github.com/silo-finance/silo-contracts-v2 (`silo-core/README.md`, `silo-core/contracts/interfaces/IPartialLiquidation.sol`, `PartialLiquidation.sol`). Helper: https://github.com/silo-finance/liquidation
- Calls (from the interface natspec):
  - `liquidationCall(address _collateralAsset, address _debtAsset, address _user, uint256 _maxDebtToCover, bool _receiveSToken) returns (uint256 withdrawCollateral, uint256 repayDebtAssets)`, on the hook contract.
  - `maxLiquidation(address _borrower) view returns (uint256 collateralToLiquidate, uint256 debtToRepay, bool sTokenRequired)`. `collateralToLiquidate` is an underestimate; `sTokenRequired = true` means the silo lacks the liquidity to pay underlying, so you must take sTokens.
- Rules: insolvency uses the **solvency oracle** (the maxLtv oracle is for borrowing and falls back to the solvency oracle), so §3.5 is right. The fee is fixed per market in `SiloConfig`. Liquidation is partial, but becomes full when it would leave dust.
- Tests: our quote must equal `maxLiquidation` at the same block. When `sTokenRequired`, our plan must use the sToken path or skip.

### Gearbox V3.1
- Docs: https://dev.gearbox.finance/credit/liquidation and https://github.com/Gearbox-protocol/dev-docs/blob/main/pages/core/liquidation.md
- Official liquidator: https://github.com/Gearbox-protocol/liquidator-v2 (full, partial and deleverage modes; partial-liquidator contracts; pathfinder). It is the only real reference for **on-demand (Redstone) price updates**; read its source for how it builds them.
- Rules: `liquidateCreditAccount(address creditAccount, address to, MultiCall[] calls)` handles both low health and expiry. Check which case applies first. "On demand price updates are applied, if needed." Health and payouts use oracle prices, so slippage comes out of the premium. Losses stop borrowing in that manager until governance re-enables it.
- Pull (Redstone/Pyth) price feeds: measured, decided and specified in **§8a**.

## 9. When you finish each phase
Update `docs/plans/open-items.md` (progress note under the phase), run the full check in §0.3, commit, push. Report to the user: what's done, what was skipped and why, test counts.
