# D15 — receipts_log_filter address discovery (draft)

**Status:** Essential draft generated (2,653); **completion pass run 2026-09-19 →
8,434 addresses, zero unresolved failures** (see "Completeness" below and
`D15-COMPLETION.md`); human review (H1) before sync. The file that goes into
`reth.toml` is `d15_receipts_log_filter.complete.toml`.

## How generated

Script: `tools/discover_d15_addresses.py`. Walks on-chain roots per `REGISTRY.md` §3 (Family A registries + Family B factory/event enumeration). GUIDE-16 Essential tier only (markets, spokes, receipt tokens, oracle proxies + aggregators). Excludes routers, plain ERC-20 underlyings, flash-only helpers, and Balancer.

- RPC used: `https://ethereum-rpc.publicnode.com`
- Generated: 2026-09-19T10:15:28+0200
- Total Essential addresses: **2653**

## Counts per protocol

| Protocol | Addresses |
|---|---:|
| aave-v3 | 250 |
| aave-v4 | 33 |
| ajna | 237 |
| compound-v3 | 4 |
| euler-v2 | 885 |
| fluid | 183 |
| gearbox-v3 | 52 |
| liquity-v2 | 22 |
| morpho-blue | 1 |
| silo-v2 | 887 |
| sky-maker | 43 |
| spark | 56 |
| **total** | **2653** |

## Failures / incomplete

- (none recorded for Silo/Ajna after audit fix)

## Notes / caveats

- Roots are cached in script constants with comment "confirm against official deployments".
- `before` blocks: known deploy heights filled where available; others `before = 0`. **`before = 0` is the safe direction, not a placeholder** (D52): it tells Reth to keep every receipt containing this address's logs from genesis, and no receipt before deployment can match, so the cost is zero receipts and zero risk. Filling in deploy heights is an optional disk optimisation that may happen *after* the sync; it never delays it. Audit: ~99% `before=0` — fine.
- Permissionless sets (Euler, Silo, Ajna, Fluid vaults) grow over time; re-run discovery on a schedule (REGISTRY.md §3).
- Oracle aggregators: proxy + current `aggregator()` when callable; **Capo/custom feeds** (all Spark sources sampled; many Aave V3) leave holes if only the proxy is listed.
- Morpho Blue: singleton only (one filter entry covers all markets); per-market oracles not expanded in Essential draft.
- Sky/Maker: join + pip from IlkRegistry; gems (underlyings) omitted.
- Useful tier (UniV3 pools): skipped — optional second pass if cheap.
- Silo: corrected to DefiLlama V2 factory + silo-contracts-v3 factory; enum via `getNextSiloId` / `idToSiloConfig` / `getSilos` (+ share tokens).
- Ajna: pools from `getDeployedPoolsList()` (no eth_getLogs required).
- Compound V3: USDC + WETH + USDT comets; other configurator comets may still be missing.
- Compound V2 forks (GUIDE-15 Tier 3): **not present** in this Essential list — gap vs roster.

### Discovery notes

- silo: enumerated V2 factory 0x22a3cF6149bFa611bAFc89Fd721918EC3Cf7b581 + V3 factory 0x1DAb4A310447185144467076b116DAC7aec3b48F via idToSiloConfig/getSilos/getShareTokens
- ajna: enumerated getDeployedPoolsList on ERC20 (0x6146DD43C5622bB6D12A5240ab9CF4de14eDC625) and ERC721 (0x27461199d3b7381De66a85D685828E967E35AF4c) factories
- silo: corrected to DefiLlama V2 factory 0x22a3…b581 + silo-contracts-v3 factory 0x1dab…b48f; enum via getNextSiloId/idToSiloConfig/getSilos (+ getShareTokens).
- ajna: pools from getDeployedPoolsList() — 224 ERC20 + 11 ERC721 (no eth_getLogs).
- compound-v3: USDT comet added in audit pass; survey other configurator-deployed comets before sync.
- oracle Capo/custom: Spark 0 aggregators; many Aave V3 sources lack aggregator() — Capo hole.
- before_block=0 for vast majority — **not a blocker** (D52; see above). The real blockers before H1 are *missing addresses*: Capo/custom aggregators behind Spark and Aave V3 proxies, the five flash sources (Aave pools' aTokens, UniV3 top pools, UniV4 `PoolManager`, Morpho singleton, Sky DSS Flash) that WP 07A's availability check and WP 05E's parity need, and the top exit pools per collateral family. `WORK-PACKAGES.md` §1 row C3 lists the tiers.

## Completeness — what "complete" means for this filter, and the second pass that gets there

The Essential draft above is **not the filter**. It enumerates markets and receipt
tokens and stops at the first oracle proxy. Everything the replay, the watcher,
the flash index and derived pricing read from logs must be in the filter, or the
archive is silently missing it and no later WP can recover it. The checklist
below is the definition of complete; `tools/d15_complete.py` executes it from
chain on top of `d15_addresses.json` and writes `*.complete.*` files.

| # | Class | Why it must be in the filter | How it is enumerated (no hand-typed addresses) |
|---|---|---|---|
| 1 | **Every OCR aggregator that emits `AnswerUpdated`, resolved recursively** | Aave V3 mainnet has 65 oracle sources and only 15 are Chainlink proxies; the rest are Capo / synchronicity / SVR wrappers with no events. The event lives on the aggregator *behind* them. Replay of any price-triggered liquidation needs it | Walk every source through the known getters (`aggregator`, `ASSET_TO_USD_AGGREGATOR`, `BASE_TO_USD_AGGREGATOR`, `RATIO_PROVIDER`, `ASSET_TO_PEG`, `PEG_TO_BASE`, Morpho `BASE_FEED_1/2`, `QUOTE_FEED_1/2`, Euler `feed`, `oracleBaseCross`, `oracleCrossQuote`, Gearbox `priceFeed*`, Liquity `*Oracle()` structs, Sky OSM `src`) to depth 5 |
| 2 | **Every historical phase aggregator** | A replay over the archive needs the aggregator that was live *then*. Proxies re-point on every feed upgrade | `AggregatorProxy.phaseId()` → `phaseAggregators(1..n)`; Chainlink `FeedRegistry.getPhaseFeed(base, quote, 1..n)` for every tracked asset × {USD, ETH, BTC} |
| 3 | Aave V4 spoke-oracle sources | Zero V4 aggregators in the draft | `SpokeOracle.getReserveSource(reserveId)` (selector `e4337e38`, verified on-chain), enumerate until zero; `AssetSourceUpdated` logs where emitted |
| 4 | Compound V3: all comets + per-asset feeds | Draft had 3 comets and no feeds | `Configurator.CometDeployed` logs; `numAssets`/`getAssetInfo(i).priceFeed`, `baseTokenPriceFeed` |
| 5 | Morpho Blue: market oracles + IRMs | Singleton covers positions; prices do not flow through it | `CreateMarket` logs → `oracle`, `irm`; oracle resolved per #1 |
| 6 | Euler V2: router + adapter per (asset, unitOfAccount) incl. collaterals | 884 vaults, no oracle in the draft | `vault.oracle()`, `unitOfAccount()`, `LTVList()` → `router.getConfiguredOracle(asset, uoa)` |
| 7 | Gearbox V3 price oracle + per-token feeds | none in draft | `creditManager.priceOracle()`, `getTokenByMask`, `priceFeeds(token)` |
| 8 | Silo V2 solvency / maxLtv oracles | none in draft | `SiloConfig.getConfig(silo)` fields 7, 8 |
| 9 | Liquity V2 underlying aggregators | only the PriceFeed wrappers were listed | `ethUsdOracle()` etc. (first word of the struct) |
| 10 | Sky: Vat, Jug, Pot, Spotter, **Dog + every Clipper**, OSM **medianizers**, DSS Flash, gems | Liquidation ground truth (`Bark`, `Kick`, `Take`) and the price path (`LogMedianPrice` → `LogValue` → `Poke`) were entirely absent | `Dog.ilks(ilk).clip`, `Spotter.ilks(ilk).pip` → `src()`, `IlkRegistry.gem`; core singletons as flagged constants |
| 11 | **Flash sources**: UniV4 `PoolManager`, Sky DSS Flash, UniV3 pools (Aave pools + aTokens and the Morpho singleton were already present) | WP 07A availability replay; WP 05E parity | constants (flagged `confirm`) + UniV3 `factory.getPool` for every tracked asset × hub asset × {100, 500, 3000, 10000} |
| 12 | **Exit venues**: UniV3, Curve (`MetaRegistry.find_pools_for_coins`), Kyber Elastic pools for tracked asset × hub | 05E swap-math parity and 12B route backtests are impossible without pool events | as above; hubs = WETH, USDC, USDT, DAI, WBTC, wstETH, USDe, cbBTC; Curve additionally against native ETH |
| 13 | **Rate providers** for derived pricing (stETH, wstETH, RocketNetworkBalances, rETH, cbETH, sDAI/Pot, sUSDe, sUSDS, weETH/LiquidityPool, sfrxETH, rsETH LRTOracle, Renzo) | GUIDE 06 Step 7 triggers on their events | constants (flagged `confirm`) |
| 14 | Oracle networks that push on-chain (Pyth) | pull-based feeds behind Euler/Fluid adapters | constant (flagged `confirm`) |
| 15 | Every tracked ERC-20 underlying | `Transfer` events are how flash-source and pool balances are reconstructed without `eth_call` at every block | collected as a side effect of #1–#11 (`asset/erc20` rows) |

Everything marked `confirm` is a protocol singleton read from the protocol's
published deployments; H1 verifies each has code and is the canonical instance.
Nothing else in the second pass is typed by hand.

**Not resolvable from chain, left for C3:** Fluid vault oracles
(`VaultResolver.getVaultEntireData().configs.oracle`, needs the resolver ABI —
their underlying feeds are covered by #2 for the same assets); Redstone classic
adapters behind some Euler/Silo/Fluid oracles (each is its own contract with
`ValueUpdate`; the recursion lists the adapter, C3 confirms it emits); Compound
V2 forks (out of the Essential roster).

Sizing note: the second pass adds several thousand addresses (pools dominate).
Reth's filter is a map keyed by address; the receipts kept are those whose logs
match, so the disk cost is proportional to *matching* receipts, not to filter
size. Every class above is one the bot reads; none is speculative.

## Artifacts

- `d15_addresses.json` — Essential draft (structured list)
- `d15_receipts_log_filter.toml` — Essential draft filter entries
- `d15_addresses.complete.json`, `d15_receipts_log_filter.complete.toml`,
  `D15-COMPLETION.md` — the second pass; **this is what H1 signs off and what
  goes into `reth.toml`**
- `tools/discover_d15_addresses.py` — Essential regenerator
- `tools/d15_complete.py` — completion pass (needs an `eth_getLogs`-capable RPC;
  publicnode refuses `eth_getLogs`, Tenderly's public gateway serves multi-million
  block windows, mevblocker serves 10k windows)
