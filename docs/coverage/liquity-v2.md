<!-- coverage-audit protocol=liquity-v2 repo=liquity/bold commit=c8a5a4ee2e9dc024905856b6698a77d849c68c7e date=2026-09-20 -->
# Liquity V2 (BOLD) event coverage

Source: `liquity/bold` @ `c8a5a4ee2e9dc024905856b6698a77d849c68c7e` (`main`, 2026-07-13). `TroveManager.sol`, `ITroveEvents.sol`, `IStabilityPoolEvents.sol`, `BorrowerOperations.ShutDown`, `MainnetPriceFeedBase.ShutDownFromOracleFailure`, mainnet `contracts/addresses/1.json`.

DirtySet lives in `liq-protocol` (D46). `halt` is GUIDE-03 HaltSink, not a DirtySet variant.

topic0 = keccak256(canonical ABI signature). `ITroveManager.Status` / `ITroveEvents.Operation` / `BatchOperation` → `uint8`. Trove ids are `uint256` NFTs; the intern key packs the low 160 bits into `PositionKey.user` and stores the full id in `TroveExtra.trove_id`.

`batchLiquidateTroves(uint256[] _troveArray)` is the liquidation ABI (not an event). Selector `0xef49a6b4`. Empty → `EmptyData`. None liquidatable → `NothingToLiquidate`. Per id: skip unless `Status.active` or `Status.zombie` (this pin has no `unredeemable`); liquidate iff `getCurrentICR(id, price) < MCR`. Counterparty is the Stability Pool. Liquidator profit is gas compensation only (`ETH_GAS_COMPENSATION` + coll gas from `collSPPortion`). `encode` emits `ExecutorAdapter::LiquityV2` (id 5, tail 32 = full trove id). Check 8 still rejects `max_repay == 0`; check 9 is not reached in `run()`. Dedicated encode test covers the adapter id.

Boot is fail-closed: `Config::from_toml` then `Config::assert_live_registry(provider, block)` eth_calls each branch `AddressesRegistry` MCR/CCR/penalties (immutables) and refuses a toml disagree, then `LiquityV2::new`. `new` returns `ConfigError::LiveRegistryUnasserted` unless that assert succeeded on the same config. Markets 3508 WETH / 3509 wstETH / 3510 rETH. Euler intern vaults 561..=1444, catalog 3511, first_market 3512.

| DirtySet | when |
|---|---|
| Positions | TroveUpdated / TroveOperation / BatchedTroveUpdated / BatchUpdated (batch members) |
| MarketAccrual | Liquidation (`L_coll` / `L_boldDebt`), SP BOLD deposits, branch shutdown |
| None | redemption summary, SP depositor snapshots, unused SP index events |
| halt | ERC-1967 proxy, `Initialized`, `*AddressChanged` after `pinned_through` |

## Coverage

| path | function/event | log topic(s) | DirtySet | notes |
|---|---|---|---|---|
| tm.troveUpdated | TroveManager → TroveUpdated | 0x0fba2673863b12c7b8463f3fa2f9b0cb1d534c573cdec5b5d895ee00d6ce6f5e | Positions | recorded debt/coll/stake/rate/snapshots; 0 < debt < MIN_DEBT → zombie; leave-batch (TroveUpdated before BatchUpdated) clears denorm |
| tm.troveOperation | TroveManager → TroveOperation | 0x962110f281c1213763cd97a546b337b3cbfd25a31ea9723e9d8b7376ba45da1a | Positions | Operation uint8; liquidate → closedByLiquidation; removeFromBatch/close/liquidate clear batch denorm |
| tm.batchedTroveUpdated | TroveManager → BatchedTroveUpdated | 0x6464838e073667756f10746b26734b60870fdcad31d7861c6e5603430bccac61 | Positions | shares + batch manager; debt from BatchUpdated denorm |
| tm.batchUpdated | TroveManager → BatchUpdated | 0xecf6daab6f1facdfdd8dfe32b525744d8a7a940824dd52e2b53c24028ee5faa0 | Positions | scan market positions with batch_manager == event manager (leavers already 0) |
| tm.liquidation | TroveManager.batchLiquidateTroves → Liquidation | 0x7243af9a1cff94d3429b2ee00b78c1c10589259f20dc167cb67704f38f9e824e | MarketAccrual | L_coll / L_boldDebt; field 8 is `_L_ETH` in ABI |
| tm.redemption | TroveManager.redeemCollateral → Redemption | 0x84ec8e1674d62e3a8ff294b1a7f53527d2d10291765fadf94e0ce431b2334334 | None | trove deltas arrive as TroveUpdated |
| tm.redemptionFee | TroveManager → RedemptionFeePaidToTrove | 0xc7e8309b9b14e7a8561ed352b9fd8733de32417fb7b6a69f5671f79e7bb29ddd | None | |
| tm.nftChanged | TroveManager → TroveNFTAddressChanged | 0x39b3d3f08f5292d52497444fc183b3915a339c0b41fb021bf52ae59505e455b2 | halt | after pin |
| tm.boChanged | TroveManager → BorrowerOperationsAddressChanged | 0x3ca631ffcd2a9b5d9ae18543fc82f58eb4ca33af9e6ab01b7a8e95331e6ed985 | halt | after pin |
| tm.boldChanged | TroveManager → BoldTokenAddressChanged | 0x28fe9b1bb8b27b863bb5635cb5bbd4e1beb7af490191ba03efe587680895b4fd | halt | after pin |
| tm.spChanged | TroveManager → StabilityPoolAddressChanged | 0x82966d27eea39b038ee0fa30cd16532bb24f6e65d31cb58fb227aa5766cdcc7f | halt | after pin |
| tm.gasPoolChanged | TroveManager → GasPoolAddressChanged | 0xcfb07d791fcafc032b35837b50eb84b74df518cf4cc287e8084f47630fa70fa0 | halt | after pin |
| tm.surplusChanged | TroveManager → CollSurplusPoolAddressChanged | 0xe67f36a6e961157d6eff83b91f3af5a62131ceb6f04954ef74f51c1c05e7f88d | halt | after pin |
| tm.sortedChanged | TroveManager → SortedTrovesAddressChanged | 0x65f4cf077bc01e4742eb5ad98326f6e95b63548ea24b17f8d5e823111fe78800 | halt | after pin |
| tm.registryChanged | TroveManager → CollateralRegistryAddressChanged | 0x4f8a3037ce0d3c62ab7c79fec792f6db7216b27b94e09faf499753381c33f847 | halt | after pin |
| tm.activePoolChanged | TroveManager → ActivePoolAddressChanged | 0x78f058b189175430c48dc02699e3a0031ea4ff781536dc2fab847de4babdd882 | halt | after pin |
| tm.defaultPoolChanged | TroveManager → DefaultPoolAddressChanged | 0x5ee0cae2f063ed938bb55046f6a932fb6ae792bf43624806bb90abe68a50be9b | halt | after pin |
| tm.priceFeedChanged | TroveManager → PriceFeedAddressChanged | 0x8c537274438aa850a330284665d81a85dd38267d09e4050d416bfc94142db264 | halt | after pin |
| bo.tmChanged | BorrowerOperations → TroveManagerAddressChanged | 0x143219c9e69b09e07e095fcc889b43d8f46ca892bba65f08dc3a0050869a5678 | halt | after pin |
| sp.boldBalance | StabilityPool → StabilityPoolBoldBalanceUpdated | 0xd86fb5f91c764c66ffa0ee206b53b8bb35a30494d6ded98f9b78cd12d4fe499e | MarketAccrual | `boldInSPForOffsets` for coll gas |
| sp.collBalance | StabilityPool → StabilityPoolCollBalanceUpdated | 0x2a0dc684edec911db4f58fdef07c51b499ddca1a9b13b118109befea136b06b4 | None | |
| sp.depositUpdated | StabilityPool → DepositUpdated | 0xbccccd6e317144f41782f2dfb27241c3f8cd514a8959f7fbf1f61e81b131c747 | None | depositor, not trove |
| sp.depositOperation | StabilityPool → DepositOperation | 0xdf459587a9bfd896271616423088d4842cfad6948a5a975c7d82b52d951805e4 | None | |
| sp.pUpdated | StabilityPool → P_Updated | 0xc1a9618cb59ebca77cbdbc2949f126823c407ff13edb285fd0262519a9c18e8c | None | |
| sp.sUpdated | StabilityPool → S_Updated | 0x79499aa0fdd7db8a04361384056d61d959950b3f6486282199c2f00d61e1b5f8 | None | |
| sp.bUpdated | StabilityPool → B_Updated | 0xe367a96648d02811ced605ca0b93efb0b7fe59bda272f8179c454ea64133e756 | None | |
| sp.scaleUpdated | StabilityPool → ScaleUpdated | 0x3bed654efb708b58f2d77966f880bd1798be286fdd36983d20cbf0897e186c6e | None | |
| bo.shutDown | BorrowerOperations.shutdown → ShutDown | 0x3ea78f7c2d896613dfa93eea56016064d98758df2a799e6eb38ce050c9f9c10e | MarketAccrual | `shutdownTime`; interest period caps |
| pf.oracleFail | PriceFeed → ShutDownFromOracleFailure | 0xbc6a72aabe3f2b93b4e83572c36881d8379588e52b1b3c66610a8595ce2c734d | MarketAccrual | same shutdownTime write |
| proxy.upgraded | ERC1967 → Upgraded | 0xbc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b | halt | |
| proxy.adminChanged | ERC1967 → AdminChanged | 0x7e644d79422f17c01e4894b5f4f588d331ebfa28653d42ae832dc59e38c9798f | halt | |
| proxy.initialized | Initializable → Initialized | 0xc7f505b2f371ae2175ee4913f4499e1f2633a7b5936321eed1cdaeb6115181d2 | halt | |
