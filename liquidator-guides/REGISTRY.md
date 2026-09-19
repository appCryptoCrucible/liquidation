# REGISTRY — Static Chain Reference Data

Every address, decimal and token ordering the solver and the contract depend on,
derived from chain and verified against it forever after.

**This is not D15's prune filter.** They overlap and get confused, so:

| | `receipts_log_filter` (D15) | This registry (D23) |
|---|---|---|
| Answers | whose **logs** do I retain | what do I need to **compute correctly** |
| Contains | log emitters only | decimals, token ordering, fee tiers, pool and market addresses |
| Wrong ⇒ | history gone, unrecoverable without re-sync | wrong math, silently |
| Reversible | **no** — pruning is destructive | yes — regenerate and redeploy |

Fill both early. Only one of them is a one-way door.

---

## 1. Why this gets its own document

**A decimals error does not fail safe.** USDC is 6, WBTC is 8, WETH is 18. Get
one wrong and every value in that token is off by a factor of 10^12 — and your
`minProfit` guard is denominated in the same token, computed from the same wrong
scale. The guard passes. Simulation passes. Every safety mechanism in the system
is blind to this specific class of error, because they all inherit it.

The same shape applies to `token0`/`token1`. Uniswap orders pool tokens by
address; `zeroForOne` follows from that ordering. Invert it and the swap either
reverts (survivable) or executes against the wrong side of the curve (not).

These are the errors worth building machinery against, because no downstream
check catches them.

---

## 2. Schema

```jsonc
{
  "chain_id": 1,
  "generated_at_block": 0,          // block the derivation read from
  "tokens": {
    "0x…": {
      "symbol": "USDC",
      "decimals": 6,
      "quirks": ["no_return_data"]  // see §5
    }
  },
  "protocols": {
    "aave-v4": {
      "market": "0x…",              // Pool / Spoke / Comet / Morpho market
      "deployed_block": 0,
      "receipt_tokens": ["0x…"],    // aTokens etc. — position-moving on transfer
      "oracle_adapters": ["0x…"]
    }
  },
  "oracles": {
    "0x…": {                        // aggregator PROXY
      "aggregator": "0x…",          // current underlying — changes on upgrade
      "pair": "ETH/USD",
      "decimals": 8,
      "svr": true
    }
  },
  "pools": {
    "0x…": {
      "venue": "univ3",
      "token0": "0x…",              // NOT alphabetical in your file — on-chain order
      "token1": "0x…",
      "fee": 500,
      "deployed_block": 0
    }
  },
  // flash_sources: Aave pools, UniV3 pools, UniV4 PoolManager, Morpho,
  //   Sky DSS Flash 0x60744434d6339a6B27d73d9Eda62b6F66a0a04FA (DAI only) — NOT Balancer (D09)
  "flash_sources": { "…": {} },
  // routers: Curve (+ allowlisted aggregators). Balancer routers out of scope (D08)
  "routers":       { "…": {} }
}
```

`token0`/`token1` are stored **as the pool reports them**, never as the generator
thinks they should sort. If the two disagree, the pool is right and the
assumption is wrong — that is the bug this field exists to surface.

---

## 3. Discovery — enumerate from chain, never hand-list

**The address list is discovered, not transcribed.** An agent that assembles it
from documentation and blog posts produces a snapshot of what someone wrote down,
which is a different thing from what is deployed.

This is not a theoretical concern. "Aave V3 on Ethereum" is **several markets**,
not one — Core plus separate instances, each with its own Pool address. Aave V4
launched with **three hubs and eleven spokes**. Miss one and it never enters the
prune filter, which is the irreversible mistake; miss a reserve and you have a
collateral you can never exit, which surfaces as a pile of `NoCollateralExit`
declines rather than as an error.

Every address comes from walking a registry contract or an event log — and which
of those it is depends on the protocol family.

### Family A — governed sets, enumerable from a root

Markets are added by governance, so a registry contract knows all of them.
Bounded, slow-changing, every entry already vetted by someone.

| Protocol | Root call | Yields |
|---|---|---|
| **Aave V3 / Spark** | `PoolAddressesProviderRegistry.getAddressesProvidersList()` → per provider, `getPool()` | every Pool instance, without naming any |
| **Aave V3 reserves** | `Pool.getReservesList()` → per asset, `getReserveData()` | reserves **plus the aToken and variable debt token** — receipt tokens fall out of discovery rather than being remembered separately |
| **Aave V3 oracles** | `PoolAddressesProvider.getPriceOracle()` → `AaveOracle.getSourceOfAsset(asset)` → that proxy's `.aggregator()` | the feed and its current underlying |
| **Aave V4 hubs / spokes** | Enumerate spokes from the deployed hub. **Confirm the signature against the contract, not the documentation** — GUIDE 15's rule applies hardest to the newest protocol | hubs and spokes |
| **Compound V2 forks** | `Comptroller.getAllMarkets()` | every cToken |
| **Sky / Maker** | `IlkRegistry` (`0x5a464C28D19848f44199D003BeF5ecc87d090F87`): `count()` → `list()` / `list(start,end)` → per ilk `info(ilk)` (gem, join, pip, …) | every collateral type (ilk) |

**A fork is the same enumeration code with a different root address.** That is
where GUIDE 15's fork multiplier actually comes from — not from the adapter alone.

### Family B — permissionless sets, enumerable only from events

Morpho Blue, Euler V2, Silo, Ajna, and several GUIDE 15 protocols. Anyone can
create a market; there is no governance list to call and the set grows
continuously. Recipes below are concrete enough to implement discovery; confirm
factory addresses from the protocol's published deployments at run time (do not
hard-code solely from this table).

| Protocol | How | Architecture |
|---|---|---|
| **Morpho Blue** | `CreateMarket` events on the Morpho singleton; `idToMarketParams(id)` | **singleton** — one contract emits everything |
| **Euler V2** | EVault `GenericFactory`: `ProxyCreated(proxy, upgradeable, implementation, trailingData)` **or** `getProxyListLength()` + `getProxyListSlice(start, end)`; per vault `asset()` / oracle views | **per-vault** |
| **Silo V2** | Silo factory / deployer `NewSilo` (or equivalent creation) logs → `SiloConfig.getSilos()` → two ERC-4626 silos; read `asset()` per silo | **per-market (pair of silos)** |
| **Ajna** | `ERC20PoolFactory` / `ERC721PoolFactory` `PoolCreated` logs; optional official subgraph for backfill | **per-pool** |
| **Uniswap V3 pools** | `factory.getPool(t0, t1, fee)`, or `PoolCreated` for a sweep | per-pool contracts |

#### Enumeration recipes (GUIDE 15 protocols that were stubs)

**Euler V2**

1. Root: official EVault factory (Ethereum: confirm current `GenericFactory`).
2. Prefer view iteration: `n = getProxyListLength(); proxies = getProxyListSlice(0, n)`.
3. Mirror with logs: `topic0 = keccak256("ProxyCreated(address,bool,address,bytes)")`.
4. Per proxy: read underlying asset, unit of account, governor; admit on borrowed volume (§3b).
5. Position discovery: `Borrow` / `Withdraw` / `Liquidate` logs on admitted vaults (no global user list).

**Silo V2**

1. Root: Silo factory used by DefiLlama/TVL indexers (Ethereum Main-V2 factory —
   confirm address from Silo deployments docs before baking into the registry).
2. `eth_getLogs` for market-creation events from that factory since deploy.
3. For each `SiloConfig`: `(silo0, silo1) = getSilos()`; `IERC4626(silo).asset()`.
4. Position discovery: share-token `Transfer` + silo `Deposit`/`Borrow`/`Repay`/
   `Liquidate` logs; filter by §3b borrowed.

**Fluid**

1. Vault set: `VaultFactory` `VaultDeployed(vault, vaultId)` logs, **or** periphery
   `FluidVaultResolver.getAllVaultsAddresses()` / `getVaultAddress(id)` for
   `id = 1 .. totalVaults` (resolver is convenience; factory logs are the
   prune-filter source of truth).
2. Per vault: supply/borrow tokens, oracle, liquidation params from vault or
   resolver views.
3. Positions are **NFTs**: track `Transfer` on the position NFT / vault operate
   events; liquidatable set via resolver liquidation helpers or on-chain health
   views. Do not rely on the HTTP NFT API for the prune filter or hot path.

**Liquity V2 (BOLD) / trove forks**

1. Per collateral branch: deployment registry → `TroveManager` + `SortedTroves` +
   `BorrowerOperations` (+ `CollateralRegistry` where present).
2. Enumerate open troves: `SortedTroves.getFirst()` then `getNext(id)` until `0`
   (interest-rate sorted list). Troves are NFT ids (`uint256`), not EOAs.
3. State: `TroveManager.getLatestTroveData(troveId)` (or fork-equivalent).
4. Incremental: subscribe to that branch's TroveManager operation events; full
   walk is for discovery/reconciliation, not every block.
5. Forks: same recipe with each fork's branch addresses in config.

**Gearbox V3**

1. Root: `ContractsRegister` / documented CreditManager list from Gearbox
   deployments (governed set of managers — Family A–like roots, Family B–like
   accounts).
2. Per `CreditManagerV3`: `creditAccountsLen()` + paginated `creditAccounts(offset, limit)`
   for currently open accounts.
3. Incremental: `DeployCreditAccount` / `TakeCreditAccount` (factory) and close/
   liquidate events on the manager.
4. Admit managers whose underlying debt asset is flashloanable (§3b + GUIDE 07).

**Ajna**

1. Factories: `ERC20PoolFactory` and `ERC721PoolFactory` (confirm mainnet
   addresses from Ajna deployments).
2. `PoolCreated` logs → pool address + collateral/quote tokens.
3. Optional: Ajna subgraph for historical backfill; boot assertion still re-reads
   pool tokens on-chain (§4c).
4. Buckets/loans: pool borrow/liquidate events; long-tail debt often fails GUIDE 07
   flashloanability — expect a high decline rate, which is correct.

That architecture split has a consequence for the prune filter that is easy to
miss, and it runs the opposite way to intuition:

- **Singleton** (Morpho) — *one* entry in `receipts_log_filter` covers thousands
  of markets. Easier than Aave, not harder.
- **Per-vault** (Euler, Silo) — addresses are created permissionlessly over time,
  so **you cannot pre-list addresses that do not exist yet**. A vault created in
  month three has its receipts pruned as they arrive until the filter is
  regenerated.

Live operation is unaffected — the ExEx notification stream does not care what is
stored — so the loss is *replay*, which means a late-discovered vault cannot clear
the recall measurement on state it no longer matches. The fix is mostly cadence: re-run
discovery often enough that "discovered late" does not happen, and accept that a
genuinely new market's replay history starts near its creation, which is fine
because there is nothing earlier to replay. If you want a grace window, set a
rolling full-receipt retention distance alongside the filtered-from-deployment
set, and **verify on your own node that the two compose as expected** rather than
assuming it.

**Record the counts as evidence**: instances, reserves per instance, aggregators,
receipt tokens. A later discovery run that finds *fewer* than the recorded count
is visibly wrong rather than quietly incomplete.

### Discovery is a recurring job, not a one-off

New reserves get listed. New spokes get deployed. A protocol adds a market.

So discovery re-runs on a schedule, and **the diff against the committed registry
is the alert**. That closes the loop with GUIDE 09: a governance listing you
didn't know about shows up first as `NoCollateralExit` declines accumulating on
one asset, and second as a discovery diff naming it. Two independent signals for
the same event, which is what you want for something that arrives on someone
else's schedule.

This is a good standing job for the operating agent (`AGENT-OPS.md`): run
discovery, diff, propose the registry addition with the evidence attached. It is
gathering, which is the half an agent should own.

## 3b. Admission — what gets into the registry at all

Family B forces a question Family A does not: with thousands of permissionless
markets, which ones do you track?

**Phase 1 answer: the ones with real borrowed volume.** But the *reason* matters
more than the answer, because it decides where the bar sits and whether it ever
moves.

### The oracle does not need to be trusted

Worth settling first, because it is the intuitive objection and it is wrong.

A Morpho market's five parameters split usefully. **LLTV** is
governance-whitelisted (nine approved values) and **IRM** is governance-whitelisted
(only AdaptiveCurveIRM). **Loan token** must be flashloanable and **collateral
token** must have an exit — both already filtered by GUIDE 07. That leaves the
**oracle**, which is any contract address, whitelisted by nobody.

And it does not need to be. The market's oracle determines **liquidatability**,
which is a fact about protocol state whether the oracle is honest or not. Your own
routing math determines **profitability**, and `minProfit` enforces that on-chain
in the debt token.

Walk the attack: someone creates a market with a lying oracle and seeds a position
that looks liquidatable. You repay real debt token and seize their collateral,
then try to swap it. No real liquidity means nothing comes back, `minProfit`
fails, the bundle is dropped, you pay nothing. For it to work the collateral would
have to appear swappable at simulation and not at execution — and inside a private
bundle they cannot see your transaction to arrange that, while simulation runs
against the same block state as execution.

> **A dishonest oracle creates opportunities at that market's lenders' expense,
> not yours.** It cannot make an unprofitable liquidation look profitable, because
> profit is measured in the swap.

### So the costs are focus and evidence, not safety

**Focus** — thousands of dust markets cost state-store memory and health
computation on the hot path, and solver time on candidates that never clear.

**Evidence** — the same argument as building on Aave V3 rather than V4. Enable
3,000 markets where 2,950 produce no liquidations in six months and your recall
gate is measured on the fifty that mattered anyway. The rest add noise, not
confidence.

That second one is the real reason to start narrow. Not danger — the gates need
populated markets to mean anything.

### The bar is mechanical, and it is meant to move

Admission on **minimum borrowed, sized to the viability band's lower edge**
(GUIDE 12 §4b) at a representative gas level. Note the band is computed per block
and per `(protocol, collateral, debt)` — for admission you want a coarse, stable
read of it, not the live value, because you are deciding what to *track* rather
than what to execute.

- **Borrowed, not TVL.** A market with $50M supplied and $200k borrowed has $200k
  of liquidatable surface. Supply-only deposits are never liquidated.
- **A number, not a curated allowlist.** Pick B for safety reasons and you will
  set a conservative bar and never move it. Pick it for focus and evidence and the
  bar is something you lower as the system proves out — and lowering it *is* the
  long-tail edge.
- Widening is already a governed action: enabling markets in production config is
  its own session with its own evidence (`ORCHESTRATOR.md` §7).

### One thing stays curated regardless

**The registry is the safety boundary.** Market addresses reach `Executor` from
the plan, and the batching `try/catch` does not bound gas — a market that burns
the call's gas takes the whole batch down with it. That is acceptable only because
the address set is curated.

Generous bar, but a bar. Never arbitrary input.

The same reasoning transfers to DEX pools: do not vet them, filter on depth plus
the oracle cross-price (GUIDE 12), and let `minProfit` catch the rest.

## 3c. Derivation

Once the admitted addresses are known, the per-address fields are all on-chain and
readable from a free public RPC. No archive, no paid plan, no rate-limit problem
at this volume.

| Field | Source |
|---|---|
| `decimals`, `symbol` | `decimals()`, `symbol()` on the token |
| `token0`, `token1`, `fee` | `token0()`, `token1()`, `fee()` on the pool |
| pool address | factory `getPool(t0, t1, fee)` — derive, do not trust a list |
| `deployed_block` | first log from the address, or the creation tx |
| oracle `aggregator` | `aggregator()` on the proxy |
| oracle `decimals` | `decimals()` on the feed |
| receipt tokens | falls out of §3 discovery — do not source separately |
| market `borrowed` | protocol view, for the §3b admission bar |

**Derive the pool address from the factory rather than copying it.** A factory
call proves the (token0, token1, fee) triple maps to that pool. A copied address
proves nothing, and it is the same derivation `Executor`'s
`uniswapV3SwapCallback` does on-chain to authenticate the caller — so if they
disagree, swaps revert in a callback rather than failing loudly at fill time.

---

## 4. Verification — the machine checks the chain, not another agent

Two agents can make correlated errors, and the second one anchors on the first's
answer. Agreement is not correctness. The data is deterministic, so verification
should be deterministic too.

**Three layers, and they catch different things:**

**4a. Independent re-derivation.** A script re-reads every field from chain
without seeing the committed file, then diffs. Not a review of the registry — a
second generation of it. Any difference fails CI.

**4b. Identity check against a canonical list.** This is the layer people skip
and it catches the error the other two cannot. A boot check confirms an address
*behaves* like a token; it does not confirm it is the *right* token. USDC and
USDC.e both answer `decimals() == 6` quite happily. So cross-check every token
address against a canonical token list once, at fill time, and every market
address against the protocol's own published deployment addresses.

> The dangerous error is not a malformed address. It is a valid address for the
> wrong thing.

**4c. Boot-time assertion — the one that never expires.** On every start the bot
re-reads `decimals()`, `symbol()`, `token0()`, `token1()`, `fee()` for every
entry and asserts against the committed file. **Any mismatch, refuse to start.**

That last one is what makes the registry safe to depend on. It stops being a
document someone maintains and becomes a cache of on-chain truth that cannot
silently drift. A wrong entry becomes a startup failure instead of a wrong trade,
and a proxy upgraded underneath you becomes an alert instead of a mystery.

```rust
/// Runs before the ExEx registers. There is no degraded mode: a registry that
/// disagrees with chain means every downstream number is suspect.
pub fn assert_registry(reg: &Registry, rpc: &Provider) -> Result<(), RegistryError> {
    for (addr, t) in &reg.tokens {
        let on_chain = erc20_decimals(rpc, *addr)?;
        if on_chain != t.decimals {
            return Err(RegistryError::DecimalsMismatch {
                token: *addr, expected: t.decimals, found: on_chain,
            });
        }
    }
    for (addr, p) in &reg.pools {
        let (t0, t1) = pool_tokens(rpc, *addr)?;
        if (t0, t1) != (p.token0, p.token1) {
            return Err(RegistryError::TokenOrderMismatch { pool: *addr, found: (t0, t1) });
        }
    }
    Ok(())
}
```

Note what is *not* boot-checkable cheaply: `deployed_block`, and identity. Those
rely on 4a and 4b, done once, carefully.

---

## 5. Token quirks that must be recorded, not assumed

The `quirks` array exists because these change what correct code looks like:

| Quirk | Example | Consequence |
|---|---|---|
| `no_return_data` | USDT, BNB, OMG | bool-returning interface reverts on decode — `SafeTransfer` is mandatory, see `Executor.sol` |
| `approve_nonzero_reverts` | USDT | must zero the allowance before setting a new one |
| `fee_on_transfer` | some | received ≠ sent; size the swap from the **measured** balance delta |
| `rebasing` | stETH | balance moves with no Transfer event; never cache it |
| `low_decimals` | USDC, USDT (6), WBTC (8) | rounding headroom is much smaller than 18-decimal intuition |
| `nonstandard_metadata` | MKR | `symbol()` returns bytes32, not string — decoders must handle it |

Record the quirk when you find it. A quirk discovered at 3am during a cascade,
in a token you have already been trading, is the expensive version.

---

## 6. Ownership and sequencing

Fill this **early** — before it is encoded in the solver or the contract, so the
data is already there and verified when it is needed, rather than being produced
under pressure by whoever is mid-build.

1. An agent session runs **discovery** (§3) from a public RPC — enumerating
   instances, reserves, receipt tokens and aggregators rather than listing them
   (Track C, day 0; no dependency on the node being synced)
2. Admission (§3b) decides which of them are tracked; derivation (§3c)
   fills the per-address fields for those
3. The independent re-derivation script (4a) runs and must agree
4. The canonical-list identity check (4b) runs once, and its result is recorded
   along with the discovery counts
5. The registry is committed — it is part of the kilobytes in version control
   that GUIDE 16 §7 says are the only genuinely unrecoverable artifacts
6. **D15's `receipts_log_filter` list is generated from the committed registry**,
   not assembled by hand. That is the whole reason discovery runs before the node
   sync: the prune filter inherits a verified address set instead of whatever was
   to hand, and it is the one decision that cannot be revised.
7. Boot assertion (4c) runs forever after; discovery re-runs on a schedule and
   diffs (§3)

The agent does the gathering. The machine does the verifying. The split matters:
generation is judgement, verification is arithmetic, and the second one should
never be delegated to something that can be confidently wrong.
