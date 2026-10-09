# Swap venues added with the third swap module (2026-10-08)

SushiSwap V3, PancakeSwap V3, Balancer V2 weighted pools and Fluid DEX T1 as
exits for collateral. The first two are Uniswap V3 forks the Executor's existing
pool-direct leg handles with a factory byte; Balancer and Fluid are venues of a
third module, `DexModule`, because `SwapModule` is at 24,147 B of the 24,576 B
limit.

## Wire

| Leg | Venue | Data |
|---|---|---|
| Sushi V3 / Pancake V3 pool | 0 | pool (20) ‖ factory id (1: Sushi, 2: Pancake); Uniswap's is the bare 20 bytes |
| Balancer V2 weighted | 11 | pool id (32) |
| Fluid DEX T1 | 12 | pool (20) ‖ swap0to1 (1) |

Chain hops (venue 10): kinds 5 (Sushi V3) and 6 (Pancake V3) carry the fee in
the hop word like Uniswap's; kind 7 (Balancer) carries the 32-byte pool id and
kind 8 (Fluid) the 21 bytes above in the extra area.

On chain (`DexModule`, `SwapModule`, `Executor`): a fork pool is accepted only
when its CREATE2 address from the fork's factory/deployer and init hash
(`MainnetVenues`) matches, in the swap callback (`pancakeV3SwapCallback` is
Pancake's name for the same call); a Balancer pool id only when it is a
reviewed 2021 pool or the v4 weighted factory's (`isPoolFromFactory`); a Fluid
pool only when `DexFactory.getDexAddress(pool.DEX_ID()) == pool`. Native ETH in
a Fluid pool is named WETH in the plan: the module unwraps to pay `msg.value`
and wraps what the pool pays out, by the amount the call returned (never
`address(this).balance`). Exact output is refused for a native-ETH input.

## Math, and what proves it

| Venue | Source | Rust | Oracle |
|---|---|---|---|
| Sushi / Pancake V3 | the V3 pool (Pancake: `slot0` has `uint32 feeProtocol`, `lmPool`) | `solver.rs` (V3 state + factory id) | each fork's own `QuoterV2`: 104 quotes (`pool_seed` live test) |
| Balancer weighted | `LogExpMath`, `FixedPoint`, `WeightedMath` of the deployed pools (v4 pool has the exponent 1/2/4 `powUp` shortcut, the 2021 pools do not) | `balancer.rs` | `BalancerQueries.querySwap`: 128 recorded answers (`tests/balancer_vectors.rs`), 40 live quotes, real Vault `Swap` logs fold exactly |
| Fluid DEX T1 | `FluidDexT1` `swapIn`, `CoreHelpers`, and the Liquidity layer's `operate` + `LiquidityCalcs` | `fluid.rs` (port of `tools/registry/fluid_math.py`) | the pool's own `swapIn` estimate: 336 recorded vectors; **real executions** (`eth_call` with the caller funded and the pool approved): 323 equal, 217 refused by both, none the model answers and the chain refuses, none the reverse; three consecutive swaps in one simulated block equal the chain's (79 swaps); 222 live quotes through the production seeder; real Liquidity `LogOperate` events fold exactly |

Fluid details that matter:

- A swap is two `LIQUIDITY.operate` calls by the pool, so the model carries both
  tokens' exchange prices (with accrual), totals, the pool's supply and borrow
  positions and their limits: withdrawal limit with decay, borrow limit and
  maximum utilization, the 1/10,000 and 1/2 ratio caps, the oracle band, the
  50 % imaginary-reserve limit, the minimum liquidity check. A size any of
  them refuses is refused by the model, so a plan built on a quote does not fail
  on a limit.
- 25 of the 50 pools price off an external center price contract
  (`dexVariables2` bits 112–141, the CREATE of the pool's deployer at that
  nonce); its `centerPrice()` is read with the pool every block.
- **Not followed** (the pool is then not live): an active range, threshold or
  center price shift, a pool hook, a paused pool or token, an unreadable hook.
  None of the 50 pools has a hook; one has an active range shift.
- State: re-read every block (reseed thread, like Curve crypto and Balancer);
  between reads a `LogOperate` of another user sets the token's totals and
  exchange prices to the event's words, one of the pool's own marks it stale.
- Exact input only; the leg overshoots like a Curve leg and the surplus debt is
  swept.

## Gas

`config/liq-gas.toml` `[swap]`: `pancake_v3` 408,191 (with a live `lmPool`; a
pool with none costs about what a Uniswap V3 pool does), `balancer` 136,077,
`fluid` 259,756; Sushi is priced as `univ3`. Measured on the fork
(`contracts/test/fork/ForkDexVenues.t.sol`, Foundry 1.8.3 `--isolate`) as the
same real Aave V3 liquidation through the venue against Uniswap V3.

## Tests

- Solidity unit: `ExecutorV3Forks.t.sol` (14), `ExecutorDexVenues.t.sol` (19):
  the module's refusals and the call shapes against doubles of the Vault, the
  factories and a Fluid pool etched at the mainnet anchors.
- Solidity fork: `ForkDexVenues.t.sol`: nine real liquidations whose exit
  is each venue's real pool, native ETH in and out of Fluid included.
- Rust: fixtures from `tools/registry/{balancer_vectors,fluid_vectors,fluid_exec_check,fluid_operate_fixture}.py`;
  live seeding tests in `pool_seed.rs` (`--ignored`, `MAINNET_RPC_URL`).
- The searcher builds plans over these pools in the historical replay:
  `LIQ_REGISTRY_FILE=data/review/2026-10-08-dex-registry.candidate.json
  LIQ_REPLAY_POOLS=graph cargo test -p liq-replay --test historical_liquidations
  all_liquidations_through_our_searcher -- --ignored --nocapture`. On block
  25,791,740 (the $3.2M USDT-collateral Morpho liquidation) the plan it built
  and ran on the pre-state sells USDT through the Fluid USDC/USDT pool
  (`0x6677…9F9B`, chain hop kind 8, then Uniswap V3 to WETH) in three
  two-hop chains and inside a three- and a four-hop chain, and through the
  SushiSwap V3 WETH/USDT pool and a SushiSwap V2 pair: 46.565 ETH captured
  against 46.53 with the committed registry (the winner took 47.70).
  Comparing the two transactions pool by pool then found three limits in
  the router, each fixed and measured on the same block (dev build):

  | change | captured | warm select |
  |---|---|---|
  | new venues only | 46.565 | 12.8 s |
  | direct pools chosen by contribution, not `ρ₀` prefix (`exact::prune`) | 47.060 | 22.1 s |
  | `FLOW_PATHS` 16 → 32 (21 used) | 47.202 | 24.6 s |
  | `SolveBudget::max_pools` 6 → 12 | 47.431 | 30.2 s |

  With the first, Pancake WETH/USDT 0.05 % takes 35,931 USDT (the winner
  sold there) and the three pools that took 7,200 USDT between them leave
  the set. Left: 0.27 ETH. The winner also sold 32k USDT through Curve
  tricrypto (USDT/WBTC/WETH), which goes stale in our flow after one slice
  (only the 2025 Twocrypto pools follow `tweak_price`), and small routes
  through AAVE, UNI and the Balancer wstETH/AAVE pool.
- Sizing a chain's input (`exact::path_in_for`) doubles from one unit: a
  Fluid hop refuses an input under 1e6 of its 12-decimal units and a hop can
  round dust to zero, neither of which is the path's answer. Before this was
  handled the assembler refused every plan with such a chain (`chain input`
  missing) and the replay above captured nothing.

## Admitting pools

Nothing here is in `registry/registry.json`. `tools/registry/discover_dex.py`
writes `data/review/<stem>-registry.candidate.json` and
`<stem>-candidates.json` (every entry `"approve": false`; each label for a V3
fork shows what the pool holds, since the factory's `getPool` lists pools with
only dust). Set `approve` on the pools to take and run `admit_reviewed.py`.
The Balancer BAL/WETH 80/20 pool ($4.8M) is not offered: it is neither the v4
factory's nor one of the reviewed 2021 pools.
