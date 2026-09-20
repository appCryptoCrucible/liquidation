<!-- coverage-audit protocol=aave-v4 repo=aave/aave-v4 commit=40232a0a91150d8ee5cab42bd3ddd0baf4ffff9f date=2026-09-15 -->
# Aave V4 event coverage

Source: `aave/aave-v4` @ `40232a0a91150d8ee5cab42bd3ddd0baf4ffff9f` (main, 2026-09-15). `aave-dao/aave-v4` 404s; GUIDE-04 names `aave/aave-v4`. Governance execution-chain events from `aave-dao/aave-governance-v3` @ `497226efad1ca3723074034c8341fb90931fc2b2` (IPayloadsControllerCore only).

DirtySet lives in `liq-protocol` (D46). This file names the variant; it does not define the type. `halt` is GUIDE-03/14 HaltSink, not a DirtySet variant.

topic0 = keccak256(canonical ABI signature). Structs expanded from the declaring interface.

| DirtySet | when |
|---|---|
| Positions | user supply/debt/collateral/premium/dynamic-config |
| MarketAccrual | hub index/rate/liquidity; fan-out via `hub_ref` |
| MarketReprice | LT/bonus/caps/flags/oracle source/IR curve |
| None | emitted; no HF fold (paired with a foldable log, or auth-only) |
| halt | proxy/impl/authority change; HaltSink, not DirtySet |

SignatureGateway and NativeTokenGateway declare no events; they call Spoke and the Spoke logs are the fold.

## Coverage

| path | function/event | log topic(s) | DirtySet | notes |
|---|---|---|---|---|
| hub.add | Hub.add → Add | 0xb233dd05ed21346e144167b35a6213bcf04768dbdffdc8339e8b027b94b9f305 | MarketAccrual | accrue+updateDrawnRate same tx (UpdateAsset) |
| hub.remove | Hub.remove → Remove | 0x535be2ff85ab4c5d0991e10dc057a4951ea2bac426ffb036eded23036a3942b2 | MarketAccrual | |
| hub.draw | Hub.draw → Draw | 0xe2497bc41b1fa7c4ba996f24dc2affdffb2a5571584db6db0eed8fbbf1dc8517 | MarketAccrual | |
| hub.restore | Hub.restore → Restore | 0x119e7f996dc987b3ae79eb3735f1620c4292f6a7761a1e0f834c445f7798b912 | MarketAccrual | PremiumDelta=(int256,int256,uint256) |
| hub.refreshPremium | Hub.refreshPremium → RefreshPremium | 0x3fa96ecf17429fddfbb919a64196f4e43f71b57f0c5c38c49a21c8e1e763d18c | MarketAccrual | hub `premiumShares`/`premiumOffsetRay` move → `totalAddedAssets` of every supplier of the asset (04A re-audit: was None); Spoke.RefreshPremiumDebt is the user fold |
| hub.reportDeficit | Hub.reportDeficit → ReportDeficit | 0x4845ee5c72bde2b62defc8a1ca2f0fc3313b2d9e799997ce4f6776da9773bcbf | MarketAccrual | hub-side; user fold is spoke.reportDeficit |
| hub.transferShares | Hub.transferShares / payFeeShares → TransferShares | 0x0d93b0e8579bc9db73c85a1fb79d785ffc47f8e20d346253f809cc98c48292a0 | MarketAccrual | liquidation fee shares |
| hub.sweep | Hub.sweep → Sweep | 0x69bb3893073d7a893f3933f3871309fc25acfc72e365b71f554d439a85b20e8b | MarketAccrual | liquidity out to reinvestment controller |
| hub.reclaim | Hub.reclaim → Reclaim | 0x566111831db1f090374baff3c3f9fc512084f5a9b8f5b199fb475d9c43a8013f | MarketAccrual | |
| hub.eliminateDeficit | Hub.eliminateDeficit → EliminateDeficit | 0xe97b8576ac531cdc817b933309d0518ca3d26c6b46d490f3ae9fa39426a141ee | MarketAccrual | |
| hub.mintFeeShares | Hub.mintFeeShares → MintFeeShares | 0xafd21228e21de4a3f779e1cc3617e12672c3da091dcf3812a931036aa0bf633c | MarketAccrual | |
| hub.addAsset | Hub.addAsset → AddAsset | 0x92fb402b777f3710166f15b30098f41042b439850df67d0195196d125458e7b3 | MarketReprice | new hub asset |
| hub.updateAsset | AssetLogic.updateDrawnRate → UpdateAsset | 0xa1facf110ded5028ee267fa3d5986f2aa4dc14230b79ffd27e95760f14883350 | MarketAccrual | **canonical** index+rate; every mutating hub op emits this |
| hub.updateAssetConfig | Hub.updateAssetConfig → UpdateAssetConfig | 0xea358cc423f2a5739a0914913452665f0a41d404780bfe9038844d2980e5b974 | MarketReprice | feeReceiver/liquidityFee/irStrategy/reinvestmentController |
| hub.addSpoke | Hub.addSpoke → AddSpoke | 0x47acdb603dbca71028fbd9b37192e17a62e64fa160e2e607eef3853b792ea5ab | MarketReprice | |
| hub.updateSpokeConfig | Hub.updateSpokeConfig → UpdateSpokeConfig | 0x90984699e37aaae5f79c2f33e480f273509662005a8ff82a17b325eb7072454e | MarketReprice | caps/halted/active/riskPremiumThreshold |
| hub.setInterestRateData | AssetInterestRateStrategy.setInterestRateData → UpdateInterestRateData | 0xfdf3625f63f29674d0df2cf3b106add2a8cb5be2a5d36820760e39fdd210425d | MarketReprice | IR curve; next UpdateAsset has new drawnRate |
| spoke.supply | Spoke.supply → Supply | 0xd986db228cb1fe8392c5f45ff5f2c639b7db6cbd9ca7d1fe70b2de90c2c8c961 | Positions | PM/gateway/sig paths emit this too |
| spoke.withdraw | Spoke.withdraw → Withdraw | 0xfe7813e2866053d5c3938554e517b554fce6666a6561bed9eaa7419b29fa9b68 | Positions | |
| spoke.borrow | Spoke.borrow → Borrow | 0xef18174796a5d2f91d51dc5e907a4d7867bbd6e800f6225168e0453d581d0dcd | Positions | |
| spoke.repay | Spoke.repay → Repay | 0xd765a0263e8a360da8dd4fdb8c0dc5553adec12a96f29a462cdb45e5bea407dd | Positions | includes PremiumDelta |
| spoke.liquidationCall | LiquidationLogic.liquidationCall → LiquidationCall | 0x2a1f12d996f530f89d8038aa293f9fde81cac44b6dfd6225e3358d09b78a4a37 | Positions | user+liquidator |
| spoke.reportDeficit | LiquidationLogic → ReportDeficit | 0x59932f333b3a5e3fec86e662babe8dd767529ed207420e7468bd220cdfb3f076 | Positions | bad debt burned from user |
| spoke.setUsingAsCollateral | Spoke.setUsingAsCollateral → SetUsingAsCollateral | 0x4763df430bc5274807f8ab4ce0734e7898513638418d6eec0c5285ef85f7f51f | Positions | |
| spoke.updateUserRiskPremium | Spoke.updateUserRiskPremium → UpdateUserRiskPremium | 0x9a9082fd74a00ac52b567642a2d8fd3383cb2bd8690f6b2a3b7b37aaf489dac1 | Positions | per-position premium (GUIDE-04); also emitted with riskPremium=0 on deficit |
| spoke.refreshPremiumDebt | Spoke._notifyRiskPremiumUpdate → RefreshPremiumDebt | 0x4fd0c5440d5b8c1dd712c65f039f54384c59e81a139427b0a9155260d974a9a7 | Positions | per borrowed reserve |
| spoke.refreshAllUserDynamicConfig | Spoke.updateUserDynamicConfig → RefreshAllUserDynamicConfig | 0x837314749a8459031ad895d39a13552d1627fddc93d64b404bab0ae5f0798da7 | Positions | |
| spoke.refreshSingleUserDynamicConfig | Spoke._refreshDynamicConfig → RefreshSingleUserDynamicConfig | 0x5790b5f096c9cfee6b98a4e2d4f54ff3fc4ca306df5bc2093d93a36496d917b8 | Positions | |
| spoke.setUserPositionManager | Spoke.setUserPositionManager / renounce → SetUserPositionManager | 0x413bea992b9956f4f10f6c819bf7a6c8ed5baa119a2901fe221ae03171d52277 | None | auth only; later Spoke Supply/Borrow/… fold |
| spoke.setSpokeImmutables | SpokeInstance.initialize → SetSpokeImmutables | 0x6d87c7e547bc13244d61719fa011b6947b26036a16d69a607c1cf72a77d052bc | halt | reinitializer |
| spoke.updateLiquidationConfig | Spoke.updateLiquidationConfig → UpdateLiquidationConfig | 0x9062eec1933c38394d82dc926d7ddcd777a5cd08e1ae6baa94e90047338d3459 | MarketReprice | target HF / max-bonus HF / bonus factor |
| spoke.addReserve | Spoke.addReserve → AddReserve | 0xb2d3221c3db1eb0d586556ae23399acdfe3e52ff0fcd184c19069c730f9ca2e9 | MarketReprice | |
| spoke.updateReserveConfig | Spoke.updateReserveConfig → UpdateReserveConfig | 0xe9495512a0eb05fe0cbdd52286bdeb54cb8e5a8d50e7e17d75f75903a98e2af8 | MarketReprice | collateralRisk/paused/frozen/borrowable/receiveSharesEnabled |
| spoke.updateReservePriceSource | Spoke.updateReservePriceSource → UpdateReservePriceSource | 0x18a45d070f507b6387b78837652d7468e733927acc7f9a13d9cc308675735c08 | MarketReprice | |
| spoke.addDynamicReserveConfig | Spoke.addDynamicReserveConfig → AddDynamicReserveConfig | 0xfcede5501ba87e3766118ae6ed360a87ee9b6570156ae9cac52d35ff0de0403b | MarketReprice | CF / maxBonus / liqFee |
| spoke.updateDynamicReserveConfig | Spoke.updateDynamicReserveConfig → UpdateDynamicReserveConfig | 0x2d4f2760aaff0dfa53526a8fdd306864689a7d5e43f44ddfeece0f38315c298d | MarketReprice | |
| spoke.updatePositionManager | Spoke.updatePositionManager → UpdatePositionManager | 0x8e04e916c2b397f8ab1cf9a55e94728a44837b3751f72369339ad991d371edc4 | None | instance-level PM active flag |
| oracle.updateReserveSource | AaveOracle.setReserveSource → UpdateReserveSource | 0xb828dda2b9aa56f34e592f8a1c065bf11753e12bed944560d220d26367bb8140 | MarketReprice | |
| oracle.setSpoke | AaveOracle.setSpoke → SetSpoke | 0xb2c96f10011316e9ecc602e317590ebd1bc932917a80ce5dca277398005d8240 | halt | oracle↔spoke binding |
| tok.setImmutables | TokenizationSpokeInstance.initialize → SetTokenizationSpokeImmutables | 0x808040ca2aecea41673ac4029f18fd4d2279c5c841920cf62b5df586ffd1c122 | halt | |
| tok.deposit | TokenizationSpoke._deposit → Deposit (ERC-4626) | 0xdcbc1c05240f31ff3ad067ef1ee35ce4997762752e3a095284754544f4c709d7 | MarketAccrual | hub add; not a Spoke borrower position |
| tok.withdraw | TokenizationSpoke._withdraw → Withdraw (ERC-4626) | 0xfbde797d201c681b91056529119e0b02407c7bb96a4a2c75c01fc9667232c8db | MarketAccrual | distinct topic from Spoke.Withdraw |
| tok.transfer | IERC20.Transfer on TokenizationSpoke | 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef | None | receipt-token Transfer; does not touch Spoke._userPositions. Vanilla Spoke has no aToken |
| pm.registerSpoke | PositionManagerBase.registerSpoke → RegisterSpoke | 0x9b8897e66a7714715dca5bcb7859e2c0bd6ec9348070d948c5b12222e71ad9ec | None | |
| pm.supplyOnBehalfOf | GiverPositionManager.supplyOnBehalfOf → SupplyOnBehalfOf | 0xbfc02064a33362b7e05e87e92e2b495d1c3305eff06b77eb6b7bbdbe7c871456 | Positions | also Spoke.Supply |
| pm.repayOnBehalfOf | GiverPositionManager.repayOnBehalfOf → RepayOnBehalfOf | 0x5583a9c669606ecca39b834015a4588f03b174f3e970cf7903f0d2c78f5e4e35 | Positions | also Spoke.Repay |
| pm.withdrawOnBehalfOf | TakerPositionManager.withdrawOnBehalfOf → WithdrawOnBehalfOf | 0x4e79a982b60bc32361a1f1fd9462e76c51c121ef5b4458191e32eafa33cb481e | Positions | also Spoke.Withdraw |
| pm.borrowOnBehalfOf | TakerPositionManager.borrowOnBehalfOf → BorrowOnBehalfOf | 0xe848b9c976bbfe8a81cae95295517e945534729e2b9aa7b5ca988273b5d91910 | Positions | also Spoke.Borrow |
| pm.withdrawApproval | TakerPositionManager.approveWithdraw → WithdrawApproval | 0xb148fa3af25b3fb3ed9a3b0bb70eaad923b79e14f18a51566c537129ffbd78c5 | None | |
| pm.borrowApproval | TakerPositionManager.approveBorrow → BorrowApproval | 0x9a1746ce1b6fc00233e5121b2cc44dc3af79c12aa2304ff3e84d6be81b9fc2f7 | None | |
| pm.updateConfigPermissions | ConfigPositionManager → UpdateConfigPermissions | 0xd4fbec2a3b93ec0e6690b7c5381ef39de0f0c5351036191eb0627c2de95c39f1 | None | ConfigPermissions is uint8 |
| pm.setUsingAsCollateralOnBehalfOf | ConfigPositionManager → SetUsingAsCollateralOnBehalfOf | 0xf7803d8ee19f0fb4335c683f8fdcada17f94fbf222e1338b56225ef387d852bc | Positions | also Spoke.SetUsingAsCollateral |
| pm.updateUserRiskPremiumOnBehalfOf | ConfigPositionManager → UpdateUserRiskPremiumOnBehalfOf | 0x69952cea6e303cc0fd27632ddcf6123201fa7545c7262d43b82b322b17b182b2 | Positions | also Spoke.UpdateUserRiskPremium |
| pm.updateUserDynamicConfigOnBehalfOf | ConfigPositionManager → UpdateUserDynamicConfigOnBehalfOf | 0x449e95e7d4ed764b362ae5edce7d98301efe68a97a6d74e135982a72cce4d982 | Positions | also Spoke.RefreshAllUserDynamicConfig |
| proxy.upgraded | IERC1967.Upgraded (TransparentUpgradeableProxy) | 0xbc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b | halt | Hub+Spoke+TokenizationSpoke are ERC-1967 transparent proxies (deploy procedures) |
| proxy.adminChanged | IERC1967.AdminChanged | 0x7e644d79422f17c01e4894b5f4f588d331ebfa28653d42ae832dc59e38c9798f | halt | |
| proxy.initialized | Initializable.Initialized | 0xc7f505b2f371ae2175ee4913f4499e1f2633a7b5936321eed1cdaeb6115181d2 | halt | reinitializer on impl bump |
| access.authorityUpdated | IAccessManaged.AuthorityUpdated | 0x2f658b440c35314f52658ea8a740e05b284cdc84dc9ae01e891f21b8933e7cad | halt | who may configure/upgrade |
| gov.payloadExecuted | IPayloadsControllerCore.PayloadExecuted | 0xda6084bb0aa902a7f6da10ba185d4aa129414651c90772417eff02a52112af2a | None | execution-chain; subsequent protocol logs carry DirtySet/halt. Not in aave-v4 repo |

## Absent from this source (WP-named)

| named path | why |
|---|---|
| swapBorrowRateMode | no match in `src/` |
| setUserEMode | no match in `src/` |
| aToken / variableDebtToken / stableDebtToken Transfer | vanilla Spoke stores `_userPositions`; no debt/aToken contracts. Receipt-token Transfer is TokenizationSpoke IERC20.Transfer only |
| InitializableAdminUpgradeabilityProxy | V4 deploy uses OZ `TransparentUpgradeableProxy` (IERC1967.Upgraded) |

HubConfigurator / SpokeConfigurator emit nothing; they call Hub/Spoke which emit the rows above.
