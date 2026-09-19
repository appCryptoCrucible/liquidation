# Research notes — flash availability & enumeration (2026-09-18)

Sources consulted while writing GUIDE 07 Step 3a and `REGISTRY.md` §3 recipes.
Facts only; not strategy advice.

## Flash availability

| Source | Formula used in docs | Primary references |
|---|---|---|
| Aave V3/V4 | `IERC20(asset).balanceOf(aToken)` iff flash enabled / active / unpaused; fee = `FLASHLOAN_PREMIUM_TOTAL` | [Aave Flash Loans](https://aave.com/docs/aave-v3/guides/flash-loans), `FlashLoanLogic.sol`, `Pool.getReserveData` |
| Uniswap V3 | `IERC20(asset).balanceOf(pool)`; fee = `pool.fee()/100` bps | Uni V3 `IUniswapV3Pool.flash` |
| Uniswap V4 | `IERC20(asset).balanceOf(PoolManager)`; fee = 0 on pure take/settle | [Flash accounting](https://developers.uniswap.org/docs/protocols/v4/guides/flash-accounting), `IPoolManager.take` |
| Morpho Blue | `IERC20(token).balanceOf(Morpho)`; fee = 0; docs equate `maxFlashLoan` to that balance | [Morpho flashLoan](https://docs.morpho.org/developers/contracts/blue) |
| Sky DSS Flash | `maxFlashLoan(DAI) == max` [wad] if token is DAI and unlocked; else 0. Fee = `flashFee` (0 on current deployment). ERC-3156 `onFlashLoan` + approve repay. `vatDaiFlashLoan` out of scope. Mainnet `0x60744434d6339a6B27d73d9Eda62b6F66a0a04FA` | [Sky Dai Flash Mint](https://developers.skyeco.com/protocol/liquidity/dai-flash-mint/), [dss-flash](https://github.com/sky-ecosystem/dss-flash) `src/flash.sol` |
| Balancer | **Excluded** (D08/D09) | User decision — wound down |

Multi-source planable amount: `floor(available · 99/100)`, ≤3 sources, sequential sibling groups (Executor already supports; no nesting).

Provider id `4` = Sky DSS Flash (reclaimed from Balancer reservation at docs-only stage).

## Enumeration recipes

| Protocol | Method summarised in REGISTRY | References |
|---|---|---|
| Sky/Maker | `IlkRegistry.count/list/info` at `0x5a464C28…0F87` | sky-ecosystem/ilk-registry |
| Euler V2 | `ProxyCreated` / `getProxyListSlice` on GenericFactory | euler-vault-kit `GenericFactory.sol` |
| Silo V2 | Factory creation logs → `SiloConfig.getSilos()` | silo-contracts-v2; DefiLlama factory refs |
| Fluid | `VaultDeployed` / resolver `getAllVaultsAddresses`; positions = NFTs | Fluid VaultFactory + VaultResolver docs |
| Liquity V2 | Per-branch `SortedTroves.getFirst/getNext` | liquity/bold SortedTroves + TroveManager |
| Gearbox V3 | CreditManager `creditAccounts` pagination + factory deploy/take events | gearbox core-v3 |
| Ajna | ERC20/ERC721 `PoolCreated` factory logs | ajna-core / subgraph |

Confirm all deployment addresses on-chain at discovery time — do not treat this file as an address book.
