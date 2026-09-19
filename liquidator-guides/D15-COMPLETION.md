# D15 completion pass — what the Essential draft was missing

Generated 2026-09-19T13:29:51+0200 at head 26011256 via `https://ethereum.publicnode.com` (calls) and `https://gateway.tenderly.co/public/mainnet` (logs).

Input: **7825** addresses (`d15_addresses.complete.run1.json`). Added this run: **609**. Total: **8434**.

Definition of complete: `D15-ADDRESSES.md` → Completeness (15 classes). Classes 1–15 map onto the kinds below; a class with zero rows is a bug, not a result.

## All addresses, by class (merged)

| protocol | kind | n |
|---|---|---:|
| aave-v3 | aToken | 80 |
| aave-v3 | addresses_provider | 3 |
| aave-v3 | oracle_aggregator | 31 |
| aave-v3 | oracle_aggregator_phase | 44 |
| aave-v3 | oracle_proxy | 65 |
| aave-v3 | oracle_source | 24 |
| aave-v3 | pool | 3 |
| aave-v3 | price_oracle | 3 |
| aave-v3 | provider_registry | 1 |
| aave-v3 | variableDebtToken | 80 |
| aave-v4 | giver_position_manager | 1 |
| aave-v4 | hub | 4 |
| aave-v4 | oracle_aggregator | 1 |
| aave-v4 | oracle_source | 5 |
| aave-v4 | spoke | 14 |
| aave-v4 | spoke_oracle | 13 |
| aave-v4 | taker_position_manager | 1 |
| ajna | erc20_pool | 224 |
| ajna | erc20_pool_factory | 1 |
| ajna | erc721_pool | 11 |
| ajna | erc721_pool_factory | 1 |
| asset | erc20 | 837 |
| chainlink | oracle_aggregator | 24 |
| chainlink | oracle_aggregator_phase | 39 |
| compound-v3 | comet | 6 |
| compound-v3 | configurator | 1 |
| compound-v3 | oracle_aggregator | 12 |
| compound-v3 | oracle_aggregator_phase | 9 |
| compound-v3 | oracle_source | 53 |
| curve | meta_registry | 1 |
| curve | pool | 526 |
| euler-v2 | generic_factory | 1 |
| euler-v2 | oracle_adapter | 357 |
| euler-v2 | oracle_aggregator | 16 |
| euler-v2 | oracle_aggregator_phase | 15 |
| euler-v2 | oracle_router | 183 |
| euler-v2 | oracle_source | 166 |
| euler-v2 | vault | 884 |
| fluid | liquidity_layer | 1 |
| fluid | vault | 182 |
| fluid | vault_factory | 1 |
| gearbox-v3 | contracts_register | 1 |
| gearbox-v3 | credit_manager | 34 |
| gearbox-v3 | oracle_aggregator | 2 |
| gearbox-v3 | oracle_aggregator_phase | 3 |
| gearbox-v3 | oracle_source | 80 |
| gearbox-v3 | pool | 17 |
| gearbox-v3 | price_oracle | 2 |
| kyber-elastic | factory | 1 |
| kyber-elastic | pool | 20 |
| liquity-v2 | activePool | 3 |
| liquity-v2 | borrowerOperations | 3 |
| liquity-v2 | collateral_registry | 1 |
| liquity-v2 | priceFeed | 3 |
| liquity-v2 | sortedTroves | 3 |
| liquity-v2 | stabilityPool | 3 |
| liquity-v2 | troveManager | 3 |
| liquity-v2 | troveNFT | 3 |
| morpho-blue | adaptive_curve_irm | 1 |
| morpho-blue | market_oracle | 1350 |
| morpho-blue | oracle_aggregator | 55 |
| morpho-blue | oracle_aggregator_phase | 44 |
| morpho-blue | oracle_source | 517 |
| morpho-blue | rate_provider | 126 |
| morpho-blue | singleton | 1 |
| oracle-network | chainlink_feed_registry | 1 |
| oracle-network | pyth | 1 |
| rate-provider | cbeth | 1 |
| rate-provider | etherfi_liquidity_pool | 1 |
| rate-provider | lido_steth | 1 |
| rate-provider | renzo_restake_manager | 1 |
| rate-provider | reth | 1 |
| rate-provider | rocket_network_balances | 1 |
| rate-provider | rseth_lrt_oracle | 1 |
| rate-provider | sdai | 1 |
| rate-provider | sfrxeth | 1 |
| rate-provider | susde | 1 |
| rate-provider | weeth | 1 |
| rate-provider | wsteth | 1 |
| silo-v2 | factory | 1 |
| silo-v2 | factory_v2 | 1 |
| silo-v2 | factory_v3 | 1 |
| silo-v2 | llama_factory | 1 |
| silo-v2 | oracle_source | 216 |
| silo-v2 | repository | 1 |
| silo-v2 | share_collateral | 252 |
| silo-v2 | share_debt | 252 |
| silo-v2 | silo | 252 |
| silo-v2 | silo_config | 126 |
| sky-maker | clipper | 16 |
| sky-maker | dog | 1 |
| sky-maker | dss_flash | 1 |
| sky-maker | ilk_registry | 1 |
| sky-maker | join | 21 |
| sky-maker | jug | 1 |
| sky-maker | oracle_source | 5 |
| sky-maker | pip | 21 |
| sky-maker | pot | 1 |
| sky-maker | spotter | 1 |
| sky-maker | susds | 1 |
| sky-maker | vat | 1 |
| spark | aToken | 20 |
| spark | addresses_provider | 1 |
| spark | oracle_aggregator | 1 |
| spark | oracle_aggregator_phase | 5 |
| spark | oracle_proxy | 12 |
| spark | oracle_source | 1 |
| spark | pool | 1 |
| spark | price_oracle | 1 |
| spark | provider_registry | 1 |
| spark | variableDebtToken | 20 |
| uniswap-v3 | factory | 1 |
| uniswap-v3 | pool | 981 |
| uniswap-v4 | pool_manager | 1 |
| **total** | | **8434** |

## Failures / needs a human at C3

- (none)

## Notes

- morpho-blue: 1782 markets from CreateMarket; 1728 distinct-or-not oracles resolved
- fluid: vault oracles are read through VaultResolver.getVaultEntireData().configs.oracle — resolve at C3 with the resolver ABI; Fluid oracles are composite (Chainlink/Redstone/UniV3 TWAP) and their sources are covered by the FeedRegistry pass for the same assets
- uniswap-v3: 981 pools for 7892 asset×hub pairs × 4 fee tiers (flash sources + exit venues)
- uniswap-v3: 877 pools for 6676 asset×hub pairs × 4 fee tiers (flash sources + exit venues)
- curve: 830 pool hits from MetaRegistry for tracked pairs (deduped on address)
- kyber-elastic: 20 pools

Wall time 180s.
