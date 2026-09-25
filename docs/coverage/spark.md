<!-- coverage-audit protocol=spark repo=sparkdotfi/sparklend-v1-core commit=900c189feef2fafb97dca24c7b6e502dd035925a min=v3.0 date=2026-09-20 -->
# SparkLend event coverage (Aave V3 fork, pre-3.5)

Source: `sparkdotfi/sparklend-v1-core` @ `900c189feef2fafb97dca24c7b6e502dd035925a` (`dev` tip, 2026-09-02). Live Pool proxy `0xc13e21b648a5ee794902342038ff3adab66be987`; EIP-1967 implementation `0x5ae329203e00f76891094dcfedd5aca082a50e1b` at block `26018679`. AddressesProvider `0x02c3ea4e34c0cbd694d2adfa2c690eecbc1793ee` (registry; **not** `0x02C7eE240950a01E3D026058814C7Db7B7Bf0a30`). Configurator `0x542dba469bde58faee189ffb60c6b49ce60e0738`. Oracle `0x8105f69d9c41644c6a0803fda7d03aa70996cfd9`. Sentinel = `address(0)` on L1. LiquidationLogic still has stable-debt burn + `DEFAULT_LIQUIDATION_CLOSE_FACTOR = 0.5e4` / `CLOSE_FACTOR_HF_THRESHOLD = 0.95e18` and **no** `MIN_BASE_MAX_CLOSE_FACTOR_THRESHOLD` (set `min_base_max_close = 0` in TOML). Shared Pool topic0s match Aave V3 origin for the overlapping ABI.

DirtySet lives in `liq-protocol` (D46). This file names the variant; it does not define the type. `halt` is GUIDE-03/14 HaltSink, not a DirtySet variant.

topic0 = keccak256(canonical ABI signature). `DataTypes.InterestRateMode` -> `uint8`.

Spark is **not** TokenMath 3.5. Deployed aToken impl `0x6175ddec3b9b38c88157c10a01ed4a3fa8639cc6` (Sourcify exact match) `balanceOf` is `scaled.rayMul(index)` with `rayMul = (a * b + HALF_RAY) / RAY`, for supply and for variable debt. Mint and burn scale with the same half-up `rayDiv`. Pool impl `0x5ae329203e00f76891094dcfedd5aca082a50e1b` `LiquidationLogic._calculateDebt` sets the close factor on **this reserve's** stable + variable debt (`percentMul`), not on the position's base-currency total. `spark.toml` selects `balance_model = wad-ray-half-up` and `close_factor_scope = reserve-debt`. Aave V3 keeps TokenMath floor/ceil and the position-base cap. On 2026-09-25 every Spark reserve's stable-debt `totalSupply` was 0, so stable balances are not a second slot; the token is still in the ABI. Isolation / siloed / debt-ceiling configurator events **exist**. No deficit, no V3.2+ position manager, no liquidation grace period.

| DirtySet | when |
|---|---|
| Positions | user supply/debt/collateral/eMode/aToken Transfer |
| MarketAccrual | reserve indexes/rates/treasury mint / isolation total debt |
| MarketReprice | LT/bonus/caps/flags/oracle source/IR strategy/eMode/debt ceiling/siloed |
| ProtocolWide | one log dirties every market on the instance |
| None | emitted; no HF fold |
| halt | proxy/impl/authority change; HaltSink, not DirtySet |

## Coverage

| path | function/event | log topic(s) | DirtySet | notes |
|---|---|---|---|---|
| pool.supply | Pool.supply / supplyWithPermit / deposit → Supply | 0x2b627736bca15cd5381dcf80b0bf11fd197d01a037c52b927a881a10fb73ba61 | Positions | also aToken Mint+Transfer |
| pool.withdraw | Pool.withdraw → Withdraw | 0x3115d1449a7b732c986cba18244e897a450f61e1bb8d589cd2e69e6c8924f9f7 | Positions | also aToken Burn+Transfer |
| pool.borrow | Pool.borrow → Borrow | 0xb3d084820fb1a9decffb176436bd02558d15fac9b0ddfed8c465bc7359d7dce0 | Positions | InterestRateMode enum→uint8; stable still legal |
| pool.repay | Pool.repay / repayWithPermit / repayWithATokens → Repay | 0xa534c8dbe71f871f9f3530e97a74601fea17b426cae02e1c5aee42c96c784051 | Positions | useATokens=true burns aToken |
| pool.swapBorrowRateMode | Pool.swapBorrowRateMode → SwapBorrowRateMode | 0x7962b394d85a534033ba2efcf43cd36de57b7ebeb3de0ca4428965d9b3ddc481 | Positions | Spark-only vs origin 3.5 (stable still present) |
| pool.rebalanceStable | Pool.rebalanceStableBorrowRate → RebalanceStableBorrowRate | 0x9f439ae0c81e41a04d3fdfe07aed54e6a179fb0db15be7702eb66fa8ef6f5300 | Positions |  |
| pool.isolationDebt | IsolationModeLogic → IsolationModeTotalDebtUpdated | 0xaef84d3b40895fd58c561f3998000f0583abb992a52fbdc99ace8e8de4d676a5 | MarketAccrual | 04B does not subscribe this topic |
| pool.setUserEMode | Pool.setUserEMode → UserEModeSet | 0xd728da875fc88944cbf17638bcbe4af0eedaef63becd1d1c57cc097eb4608d84 | Positions | category 0 = disable |
| pool.collateralEnabled | Pool.setUserUseReserveAsCollateral → ReserveUsedAsCollateralEnabled | 0x00058a56ea94653cdf4f152d227ace22d4c00ad99e2a43f58cb7d9e3feb295f2 | Positions |  |
| pool.collateralDisabled | Pool.setUserUseReserveAsCollateral → ReserveUsedAsCollateralDisabled | 0x44c58d81365b66dd4b1a7f36c25aa97b8c71c361ee4937adc1a00000227db5dd | Positions |  |
| pool.flashLoan | Pool.flashLoan / flashLoanSimple → FlashLoan | 0xefefaba5e921573100900a3ad9cf29f222d995fb3b6045797eaea7521bd8d6f0 | None | repaid same tx; mode≠0 also emits Borrow |
| pool.liquidationCall | Pool.liquidationCall → LiquidationCall | 0xe413a321e8681d831f4dbccbca790d2952b56f977908e45be37335533e005286 | Positions | user+liquidator |
| pool.reserveDataUpdated | ReserveLogic.updateState → ReserveDataUpdated | 0x804c9b842b2748a22bb64b345453a3de7ca54a6ca45ce00d415894979e22897a | MarketAccrual | indexes+rates; stableBorrowRate still live |
| pool.mintedToTreasury | Pool.mintToTreasury → MintedToTreasury | 0xbfa21aa5d5f9a1f0120a95e7c0749f389863cbdbfff531aa7339077a5bc919de | MarketAccrual |  |
| pool.mintUnbacked | Pool.mintUnbacked → MintUnbacked | 0xf25af37b3d3ec226063dc9bdc103ece7eb110a50f340fe854bb7bc1b0676d7d0 | Positions | bridge path |
| pool.backUnbacked | Pool.backUnbacked → BackUnbacked | 0x281596e92b2d974beb7d4f124df30a0b39067b096893e95011ce4bdad798b759 | MarketAccrual |  |
| atoken.transfer | AToken._transfer / ScaledBalanceTokenBase mint-burn → Transfer | 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef | Positions | not TokenMath 3.5 |
| atoken.mint | ScaledBalanceTokenBase._mintScaled → Mint | 0x458f5fa412d0f69b08dd84872b0215675cc67bc1d5b6fd93300a1c3878b86196 | Positions |  |
| atoken.burn | ScaledBalanceTokenBase._burnScaled → Burn | 0x4cf25bc1d991c17529c25213d3cc0cda295eeaad5f13f361969b12ea48015f90 | Positions |  |
| atoken.balanceTransfer | AToken._transfer → BalanceTransfer | 0x4beccb90f994c31aced7a23b5611020728a23d8ec5cddd1a3e9d97b96fda8666 | Positions |  |
| vtoken.mintburn.transfer | VariableDebtToken mint/burn → Transfer | 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef | Positions |  |
| stoken.mintburn.transfer | StableDebtToken mint/burn → Transfer | 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef | Positions | Spark still has StableDebtToken |
| cfg.reserveInitialized | ConfiguratorLogic.executeInitReserve → ReserveInitialized | 0x3a0ca721fc364424566385a1aa271ed508cc2c0949c2272575fb3013a163a45f | MarketReprice | stableDebtToken arg used |
| cfg.reserveBorrowing | PoolConfigurator.setReserveBorrowing → ReserveBorrowing | 0x2443ba28e8d1d88d531a3d90b981816a4f3b3c7f1fd4085c6029e81d1b7a570d | MarketReprice |  |
| cfg.reserveStableRateBorrowing | PoolConfigurator.setReserveStableRateBorrowing → ReserveStableRateBorrowing | 0x0b64d0941719acd363f1a6be3d8525d8ec9d71738f7445aabcd88d7939b472e7 | MarketReprice | Spark-only vs origin 3.5 |
| cfg.reserveFlashLoaning | PoolConfigurator.setReserveFlashLoaning → ReserveFlashLoaning | 0xc8ff3cc5b0fddaa3e6ebbbd7438f43393e4ea30e88b80ad016c1bc094655034d | MarketReprice |  |
| cfg.collateralConfigurationChanged | PoolConfigurator.configureReserveAsCollateral → CollateralConfigurationChanged | 0x637febbda9275aea2e85c0ff690444c8d87eb2e8339bbede9715abcc89cb0995 | MarketReprice | ltv / liqThreshold / liqBonus |
| cfg.reserveActive | PoolConfigurator.setReserveActive → ReserveActive | 0xc36c7d11ba01a5869d52aa4a3781939dab851cbc9ee6e7fdcedc7d58898a3f1e | MarketReprice |  |
| cfg.reserveFrozen | PoolConfigurator.setReserveFreeze → ReserveFrozen | 0x0c4443d258a350d27dc50c378b2ebf165e6469725f786d21b30cab16823f5587 | MarketReprice |  |
| cfg.reservePaused | PoolConfigurator.setReservePause / setPoolPause → ReservePaused | 0xe188d542a5f11925d3a3af33703cdd30a43cb3e8066a3cf68b1b57f61a5a94b5 | MarketReprice |  |
| cfg.reserveDropped | PoolConfigurator.dropReserve → ReserveDropped | 0xeeec4c06f7adad215cbdb4d2960896c83c26aedce02dde76d36fa28588d62da4 | MarketReprice |  |
| cfg.reserveFactorChanged | PoolConfigurator.setReserveFactor → ReserveFactorChanged | 0xb46e2b82b0c2cf3d7d9dece53635e165c53e0eaa7a44f904d61a2b7174826aef | MarketReprice |  |
| cfg.borrowCapChanged | PoolConfigurator.setBorrowCap → BorrowCapChanged | 0xc51aca575985d521c5072ad11549bad77013bb786d57f30f94b40ed8f8dc9bc4 | MarketReprice |  |
| cfg.supplyCapChanged | PoolConfigurator.setSupplyCap → SupplyCapChanged | 0x0263602682188540a2d633561c0b4453b7d8566285e99f9f6018b8ef2facef49 | MarketReprice |  |
| cfg.liquidationProtocolFeeChanged | PoolConfigurator.setLiquidationProtocolFee → LiquidationProtocolFeeChanged | 0xb5b0a963825337808b6e3154de8e98027595a5cad4219bb3a9bc55b192f4b391 | MarketReprice |  |
| cfg.unbackedMintCapChanged | PoolConfigurator.setUnbackedMintCap → UnbackedMintCapChanged | 0x09808b1fc5abde94edf02fdde393bea0d2e4795999ba31695472848638b5c29f | MarketReprice |  |
| cfg.eModeAssetCategoryChanged | PoolConfigurator.setAssetEModeCategory → EModeAssetCategoryChanged | 0x5bb69795b6a2ea222d73a5f8939c23471a1f85a99c7ca43c207f1b71f10c6264 | MarketReprice | Spark eMode is one category id per asset |
| cfg.eModeCategoryAdded | PoolConfigurator.setEModeCategory → EModeCategoryAdded | 0x0acf8b4a3cace10779798a89a206a0ae73a71b63acdd3be2801d39c2ef7ab3cb | MarketReprice | oracle arg live |
| cfg.reserveInterestRateStrategyChanged | PoolConfigurator.setReserveInterestRateStrategyAddress → ReserveInterestRateStrategyChanged | 0xdb8dada53709ce4988154324196790c2e4a60c377e1256790946f83b87db3c33 | MarketReprice |  |
| cfg.debtCeilingChanged | PoolConfigurator.setDebtCeiling → DebtCeilingChanged | 0x6824a6c7fbc10d2979b1f1ccf2dd4ed0436541679a661dedb5c10bd4be830682 | MarketReprice |  |
| cfg.siloedBorrowingChanged | PoolConfigurator.setSiloedBorrowing → SiloedBorrowingChanged | 0x842a280b07e8e502a9101f32a3b768ebaba3655556dd674f0831900861fc674b | MarketReprice |  |
| cfg.borrowableInIsolationChanged | PoolConfigurator.setBorrowableInIsolation → BorrowableInIsolationChanged | 0x74adf6aaf58c08bc4f993640385e136522375ea3d1589a10d02adbb906c67d1c | MarketReprice |  |
| cfg.aTokenUpgraded | PoolConfigurator.updateAToken → ATokenUpgraded | 0xa76f65411ec66a7fb6bc467432eb14767900449ae4469fa295e4441fe5e1cb73 | halt |  |
| cfg.stableDebtTokenUpgraded | PoolConfigurator.updateStableDebtToken → StableDebtTokenUpgraded | 0x7a943a5b6c214bf7726c069a878b1e2a8e7371981d516048b84e03743e67bc28 | halt |  |
| cfg.variableDebtTokenUpgraded | PoolConfigurator.updateVariableDebtToken → VariableDebtTokenUpgraded | 0x9439658a562a5c46b1173589df89cf001483d685bad28aedaff4a88656292d81 | halt |  |
| cfg.bridgeProtocolFeeUpdated | PoolConfigurator.updateBridgeProtocolFee → BridgeProtocolFeeUpdated | 0x30b17cb587a89089d003457c432f73e22aeee93de425e92224ba01080260ecd9 | None |  |
| cfg.flashloanPremiumTotalUpdated | PoolConfigurator.updateFlashloanPremiumTotal → FlashloanPremiumTotalUpdated | 0x71aba182c9d0529b516de7a78bed74d49c207ef7e152f52f7ea5d8730138f643 | None |  |
| cfg.flashloanPremiumToProtocolUpdated | PoolConfigurator.updateFlashloanPremiumToProtocol → FlashloanPremiumToProtocolUpdated | 0xe7e0c75e1fc2d0bd83dc85d59f085b3e763107c392fb368e85572b292f1f5576 | None |  |
| oracle.baseCurrencySet | AaveOracle constructor → BaseCurrencySet | 0xe27c4c1372396a3d15a9922f74f9dfc7c72b1ad6d63868470787249c356454c1 | ProtocolWide | constructor only; BASE_CURRENCY_UNIT=1e8 at block 26018679 |
| oracle.assetSourceUpdated | AaveOracle.setAssetSources → AssetSourceUpdated | 0x22c5b7b2d8561d39f7f210b6b326a1aa69f15311163082308ac4877db6339dc1 | MarketReprice |  |
| oracle.fallbackOracleUpdated | AaveOracle.setFallbackOracle → FallbackOracleUpdated | 0xce7a780d33665b1ea097af5f155e3821b809ecbaa839d3b33aa83ba28168cefb | ProtocolWide |  |
| provider.poolUpdated | PoolAddressesProvider.setPoolImpl → PoolUpdated | 0x90affc163f1a2dfedcd36aa02ed992eeeba8100a4014f0b4cdc20ea265a66627 | halt |  |
| provider.poolConfiguratorUpdated | PoolAddressesProvider.setPoolConfiguratorImpl → PoolConfiguratorUpdated | 0x8932892569eba59c8382a089d9b732d1f49272878775235761a2a6b0309cd465 | halt |  |
| provider.priceOracleUpdated | PoolAddressesProvider.setPriceOracle → PriceOracleUpdated | 0x56b5f80d8cac1479698aa7d01605fd6111e90b15fc4d2b377417f46034876cbd | halt |  |
| provider.aclManagerUpdated | PoolAddressesProvider.setACLManager → ACLManagerUpdated | 0xb30efa04327bb8a537d61cc1e5c48095345ad18ef7cc04e6bacf7dfb6caaf507 | halt |  |
| provider.aclAdminUpdated | PoolAddressesProvider.setACLAdmin → ACLAdminUpdated | 0xe9cf53972264dc95304fd424458745019ddfca0e37ae8f703d74772c41ad115b | halt |  |
| provider.priceOracleSentinelUpdated | PoolAddressesProvider.setPriceOracleSentinel → PriceOracleSentinelUpdated | 0x5326514eeca90494a14bedabcff812a0e683029ee85d1e23824d44fd14cd6ae7 | halt | unused on this L1 instance (sentinel=0) |
| provider.proxyCreated | PoolAddressesProvider._updateImpl → ProxyCreated | 0x4a465a9bd819d9662563c1e11ae958f8109e437e7f4bf1c6ef0b9a7b3f35d478 | halt |  |
| provider.addressSet | PoolAddressesProvider.setAddress → AddressSet | 0x9ef0e8c8e52743bb38b83b17d9429141d494b8041ca6d616a6c77cebae9cd8b7 | halt |  |
| provider.addressSetAsProxy | PoolAddressesProvider.setAddressAsProxy → AddressSetAsProxy | 0x3bbd45b5429b385e3fb37ad5cd1cd1435a3c8ec32196c7937597365a3fd3e99c | halt |  |
| proxy.upgraded | BaseUpgradeabilityProxy._upgradeTo → Upgraded | 0xbc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b | halt |  |
| proxy.adminChanged | BaseAdminUpgradeabilityProxy._setAdmin → AdminChanged | 0x7e644d79422f17c01e4894b5f4f588d331ebfa28653d42ae832dc59e38c9798f | halt |  |
| atoken.initialized | AToken.initialize → Initialized | 0xb19e051f8af41150ccccb3fc2c2d8d15f4a4cf434f32a559ba75fe73d6eea20b | halt |  |
| vtoken.initialized | VariableDebtToken.initialize → Initialized | 0x40251fbfb6656cfa65a00d7879029fec1fad21d28fdcff2f4f68f52795b74f2c | halt |  |

## Absent from this source (origin 3.5+ / declared unused)

| named path | why |
|---|---|
| pool.deficitCovered / deficitCreated | no `eliminateReserveDeficit` / leftover-deficit in Spark LiquidationLogic |
| pool.positionManagerApproved / Revoked | no V3 PM |
| cfg.pendingLtvChanged | no `setReserveLtvzero` |
| cfg.liquidationGracePeriodChanged / Disabled | no grace-period pause |
| cfg.assetCollateralInEModeChanged / assetBorrowableInEModeChanged / assetLtvzeroInEModeChanged | Spark eMode is `EModeAssetCategoryChanged` + `EModeCategoryAdded` |
| cfg.eModeCategoryIsolationChanged | no isolated eMode category |
| cfg.reserveInterestRateDataChanged | Spark updates whole strategy via `ReserveInterestRateStrategyChanged` |
| irs.rateDataUpdate | DefaultReserveInterestRateStrategyV2 not this pin |
| TokenMath 3.5 rounding | Spark aToken/vToken still WadRayMul |
| MIN_BASE_MAX_CLOSE_FACTOR_THRESHOLD | not in Spark `LiquidationLogic`; TOML `min_base_max_close = 0` |
