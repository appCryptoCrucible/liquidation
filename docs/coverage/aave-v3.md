<!-- coverage-audit protocol=aave-v3 repo=aave-dao/aave-v3-origin commit=8305565ae342f1773c42cd2e4593f175fe5968a0 min=v3.5 date=2026-09-09 -->
# Aave V3 event coverage (>= 3.5)

Source: `aave-dao/aave-v3-origin` @ `8305565ae342f1773c42cd2e4593f175fe5968a0` (`main`, 2026-09-09; parent is tag `v3.7.0` = `cff15de`, 2026-08-05). WP named `aave/aave-v3-origin`; that clone HEAD is a stale v3.1 fork. This file is the live >=3.5 repo. Governance execution-chain: `aave-dao/aave-governance-v3` @ `497226efad1ca3723074034c8341fb90931fc2b2` (`PayloadExecuted` only).

DirtySet lives in `liq-protocol` (D46). This file names the variant; it does not define the type. `halt` is GUIDE-03/14 HaltSink, not a DirtySet variant.

topic0 = keccak256(canonical ABI signature). `DataTypes.InterestRateMode` -> `uint8`.

3.5 TokenMath (`src/contracts/protocol/libraries/helpers/TokenMath.sol`): aToken mint floor, aToken burn/transfer ceil, vToken mint ceil / burn floor (ERC-4626-style, protocol-favoring). `AToken._transfer` / `finalizeTransfer` take `scaledAmount`. `ReserveData.__deprecatedIsolationModeTotalDebt`. No `StableDebtToken`. No `swapBorrowRateMode`. Isolation / siloed / debt-ceiling configurator events gone.

| DirtySet | when |
|---|---|
| Positions | user supply/debt/collateral/eMode/aToken Transfer |
| MarketAccrual | reserve indexes/rates/deficit/treasury mint |
| MarketReprice | LT/bonus/caps/flags/oracle source/IR data/eMode |
| ProtocolWide | one log dirties every market on the instance |
| None | emitted; no HF fold |
| halt | proxy/impl/authority change; HaltSink, not DirtySet |

## Coverage

| path | function/event | log topic(s) | DirtySet | notes |
|---|---|---|---|---|
| pool.supply | Pool.supply / supplyWithPermit / deposit → Supply | 0x2b627736bca15cd5381dcf80b0bf11fd197d01a037c52b927a881a10fb73ba61 | Positions | also aToken Mint+Transfer; PM/gateway same log |
| pool.withdraw | Pool.withdraw → Withdraw | 0x3115d1449a7b732c986cba18244e897a450f61e1bb8d589cd2e69e6c8924f9f7 | Positions | also aToken Burn+Transfer |
| pool.borrow | Pool.borrow → Borrow | 0xb3d084820fb1a9decffb176436bd02558d15fac9b0ddfed8c465bc7359d7dce0 | Positions | InterestRateMode enum→uint8; STABLE deprecated 3.2; also vToken Mint+Transfer |
| pool.repay | Pool.repay / repayWithPermit / repayWithATokens → Repay | 0xa534c8dbe71f871f9f3530e97a74601fea17b426cae02e1c5aee42c96c784051 | Positions | useATokens=true burns aToken |
| pool.setUserEMode | Pool.setUserEMode / setUserEModeOnBehalfOf → UserEModeSet | 0xd728da875fc88944cbf17638bcbe4af0eedaef63becd1d1c57cc097eb4608d84 | Positions | PM onBehalfOf emits this; category 0 = disable |
| pool.collateralEnabled | Pool.setUserUseReserveAsCollateral / OnBehalfOf → ReserveUsedAsCollateralEnabled | 0x00058a56ea94653cdf4f152d227ace22d4c00ad99e2a43f58cb7d9e3feb295f2 | Positions | UserConfiguration.setUsingAsCollateral |
| pool.collateralDisabled | Pool.setUserUseReserveAsCollateral / OnBehalfOf → ReserveUsedAsCollateralDisabled | 0x44c58d81365b66dd4b1a7f36c25aa97b8c71c361ee4937adc1a00000227db5dd | Positions |  |
| pool.flashLoan | Pool.flashLoan / flashLoanSimple → FlashLoan | 0xefefaba5e921573100900a3ad9cf29f222d995fb3b6045797eaea7521bd8d6f0 | None | repaid same tx; mode=VARIABLE also emits Borrow |
| pool.liquidationCall | Pool.liquidationCall → LiquidationCall | 0xe413a321e8681d831f4dbccbca790d2952b56f977908e45be37335533e005286 | Positions | user+liquidator; leftover also DeficitCreated |
| pool.reserveDataUpdated | ReserveLogic.updateState → ReserveDataUpdated | 0x804c9b842b2748a22bb64b345453a3de7ca54a6ca45ce00d415894979e22897a | MarketAccrual | canonical indexes+rates; stableBorrowRate field deprecated 3.2 |
| pool.deficitCovered | Pool.eliminateReserveDeficit → DeficitCovered | 0x84b203e49f1a4b553088061534231969a68ad1c81be192205e96d23a206cb26a | MarketAccrual |  |
| pool.mintedToTreasury | Pool.mintToTreasury → MintedToTreasury | 0xbfa21aa5d5f9a1f0120a95e7c0749f389863cbdbfff531aa7339077a5bc919de | MarketAccrual |  |
| pool.deficitCreated | LiquidationLogic → DeficitCreated | 0x2bccfb3fad376d59d7accf970515eb77b2f27b082c90ed0fb15583dd5a942699 | MarketAccrual | leftover bad debt after liquidation |
| pool.positionManagerApproved | Pool.approvePositionManager → PositionManagerApproved | 0x540e692f36c2fa13e7583c4deeffd91ce6bc04f91e7d84f295d9d858372875fc | None | V3 PM = onBehalfOf only (no Giver/Taker events) |
| pool.positionManagerRevoked | Pool.renouncePositionManagerRole / approvePositionManager(false) → PositionManagerRevoked | 0x08c92c3870d10c79e9673fecea8f4ff261f8e6b661067d9ca63fd777882bff15 | None |  |
| atoken.transfer | AToken._transfer / ScaledBalanceTokenBase mint-burn → Transfer | 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef | Positions | silent HF; 3.5 ceil scaledAmount; mint/burn also emit from/to 0 |
| atoken.mint | ScaledBalanceTokenBase._mintScaled → Mint | 0x458f5fa412d0f69b08dd84872b0215675cc67bc1d5b6fd93300a1c3878b86196 | Positions | aToken 3.5 floor; vToken mint uses same event with 3.5 ceil |
| atoken.burn | ScaledBalanceTokenBase._burnScaled → Burn | 0x4cf25bc1d991c17529c25213d3cc0cda295eeaad5f13f361969b12ea48015f90 | Positions | aToken 3.5 ceil; vToken burn 3.5 floor |
| atoken.balanceTransfer | AToken._transfer → BalanceTransfer | 0x4beccb90f994c31aced7a23b5611020728a23d8ec5cddd1a3e9d97b96fda8666 | Positions | always with Transfer; scaledAmount param |
| vtoken.mintburn.transfer | VariableDebtToken mint/burn → Transfer | 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef | Positions | user transfer()/transferFrom() revert OperationNotSupported |
| cfg.reserveInitialized | ConfiguratorLogic.executeInitReserve → ReserveInitialized | 0x3a0ca721fc364424566385a1aa271ed508cc2c0949c2272575fb3013a163a45f | MarketReprice | stableDebtToken arg still in ABI, unused 3.2+ |
| cfg.reserveBorrowing | PoolConfigurator.setReserveBorrowing → ReserveBorrowing | 0x2443ba28e8d1d88d531a3d90b981816a4f3b3c7f1fd4085c6029e81d1b7a570d | MarketReprice |  |
| cfg.reserveFlashLoaning | PoolConfigurator.setReserveFlashLoaning → ReserveFlashLoaning | 0xc8ff3cc5b0fddaa3e6ebbbd7438f43393e4ea30e88b80ad016c1bc094655034d | MarketReprice |  |
| cfg.pendingLtvChanged | PoolConfigurator.configureReserveAsCollateral / setReserveLtvzero / setReserveFreeze → PendingLtvChanged | 0x6a3fa1f355f7c7ab43e41cb277d1f8471f2693c63dca91049d5ec127bb588e10 | MarketReprice |  |
| cfg.collateralConfigurationChanged | PoolConfigurator.configureReserveAsCollateral / _setReserveLtvzero → CollateralConfigurationChanged | 0x637febbda9275aea2e85c0ff690444c8d87eb2e8339bbede9715abcc89cb0995 | MarketReprice | ltv / liqThreshold / liqBonus |
| cfg.reserveActive | PoolConfigurator.setReserveActive → ReserveActive | 0xc36c7d11ba01a5869d52aa4a3781939dab851cbc9ee6e7fdcedc7d58898a3f1e | MarketReprice |  |
| cfg.reserveFrozen | PoolConfigurator.setReserveFreeze → ReserveFrozen | 0x0c4443d258a350d27dc50c378b2ebf165e6469725f786d21b30cab16823f5587 | MarketReprice |  |
| cfg.reservePaused | PoolConfigurator.setReservePause / setPoolPause → ReservePaused | 0xe188d542a5f11925d3a3af33703cdd30a43cb3e8066a3cf68b1b57f61a5a94b5 | MarketReprice |  |
| cfg.reserveFactorChanged | PoolConfigurator.setReserveFactor → ReserveFactorChanged | 0xb46e2b82b0c2cf3d7d9dece53635e165c53e0eaa7a44f904d61a2b7174826aef | MarketReprice |  |
| cfg.borrowCapChanged | PoolConfigurator.setBorrowCap → BorrowCapChanged | 0xc51aca575985d521c5072ad11549bad77013bb786d57f30f94b40ed8f8dc9bc4 | MarketReprice |  |
| cfg.supplyCapChanged | PoolConfigurator.setSupplyCap → SupplyCapChanged | 0x0263602682188540a2d633561c0b4453b7d8566285e99f9f6018b8ef2facef49 | MarketReprice |  |
| cfg.liquidationProtocolFeeChanged | PoolConfigurator.setLiquidationProtocolFee → LiquidationProtocolFeeChanged | 0xb5b0a963825337808b6e3154de8e98027595a5cad4219bb3a9bc55b192f4b391 | MarketReprice |  |
| cfg.liquidationGracePeriodChanged | PoolConfigurator.setReservePause → LiquidationGracePeriodChanged | 0xdf4f96448786bcd6fecc9f1fa25f1fbbbee6a5c9e76d635a615ac57bb5983d10 | MarketReprice | blocks liquidations until timestamp |
| cfg.liquidationGracePeriodDisabled | PoolConfigurator.disableLiquidationGracePeriod → LiquidationGracePeriodDisabled | 0x1df36dc1651d06d990805068d22811a3a9ca4396190787ef59f9102e61868fff | MarketReprice |  |
| cfg.assetCollateralInEModeChanged | PoolConfigurator.setAssetCollateralInEMode → AssetCollateralInEModeChanged | 0x79409190108b26fcb0e4570f8e240f627bf18fd01a55f751010224d5bd486098 | MarketReprice |  |
| cfg.assetBorrowableInEModeChanged | PoolConfigurator.setAssetBorrowableInEMode → AssetBorrowableInEModeChanged | 0x60087ca045be9d8d1301445e67d6248eddba97629c80284fa4910c0e52f103ab | MarketReprice |  |
| cfg.assetLtvzeroInEModeChanged | PoolConfigurator.setAssetLtvzeroInEMode / _setEmodeLtvZero → AssetLtvzeroInEModeChanged | 0xc0a450fbad635333e47f3322ec028a8e0b3909f79c721312a356a0fab5fa8b94 | MarketReprice |  |
| cfg.eModeCategoryAdded | PoolConfigurator.setEModeCategory → EModeCategoryAdded | 0x0acf8b4a3cace10779798a89a206a0ae73a71b63acdd3be2801d39c2ef7ab3cb | MarketReprice | oracle arg deprecated 3.2; label is string |
| cfg.eModeCategoryIsolationChanged | PoolConfigurator.setEModeCategoryIsolated / setEModeCategory → EModeCategoryIsolationChanged | 0xea07f8a4f488dcc1dc8c27b9c526ac8ba2d04b8024e206616f306c51d9b16826 | MarketReprice |  |
| cfg.reserveInterestRateDataChanged | PoolConfigurator.setReserveInterestRateData / initReserves → ReserveInterestRateDataChanged | 0x1e608c2c753fede2f1f22fca4170277b53ebe5015e488a53414a8921446b7c40 | MarketReprice |  |
| cfg.aTokenUpgraded | PoolConfigurator.updateAToken → ATokenUpgraded | 0xa76f65411ec66a7fb6bc467432eb14767900449ae4469fa295e4441fe5e1cb73 | halt | aToken impl bump |
| cfg.variableDebtTokenUpgraded | PoolConfigurator.updateVariableDebtToken → VariableDebtTokenUpgraded | 0x9439658a562a5c46b1173589df89cf001483d685bad28aedaff4a88656292d81 | halt | vToken impl bump |
| cfg.flashloanPremiumTotalUpdated | PoolConfigurator.updateFlashloanPremium → FlashloanPremiumTotalUpdated | 0x71aba182c9d0529b516de7a78bed74d49c207ef7e152f52f7ea5d8730138f643 | None | not an HF param |
| oracle.baseCurrencySet | AaveOracle constructor → BaseCurrencySet | 0xe27c4c1372396a3d15a9922f74f9dfc7c72b1ad6d63868470787249c356454c1 | ProtocolWide | constructor only |
| oracle.assetSourceUpdated | AaveOracle.setAssetSources → AssetSourceUpdated | 0x22c5b7b2d8561d39f7f210b6b326a1aa69f15311163082308ac4877db6339dc1 | MarketReprice |  |
| oracle.fallbackOracleUpdated | AaveOracle.setFallbackOracle → FallbackOracleUpdated | 0xce7a780d33665b1ea097af5f155e3821b809ecbaa839d3b33aa83ba28168cefb | ProtocolWide |  |
| irs.rateDataUpdate | DefaultReserveInterestRateStrategyV2.setInterestRateParams → RateDataUpdate | 0x5d123bea2036a4052274206f59d99350b9741e17da56ffae335d809b25ee0942 | MarketReprice |  |
| provider.poolUpdated | PoolAddressesProvider.setPoolImpl → PoolUpdated | 0x90affc163f1a2dfedcd36aa02ed992eeeba8100a4014f0b4cdc20ea265a66627 | halt |  |
| provider.poolConfiguratorUpdated | PoolAddressesProvider.setPoolConfiguratorImpl → PoolConfiguratorUpdated | 0x8932892569eba59c8382a089d9b732d1f49272878775235761a2a6b0309cd465 | halt |  |
| provider.priceOracleUpdated | PoolAddressesProvider.setPriceOracle → PriceOracleUpdated | 0x56b5f80d8cac1479698aa7d01605fd6111e90b15fc4d2b377417f46034876cbd | halt | oracle contract swap; every price binding must re-validate against the registry (GUIDE 06 §2) before any HF is trusted — recompute alone cannot fix it. Same class as V4 oracle.setSpoke |
| provider.aclManagerUpdated | PoolAddressesProvider.setACLManager → ACLManagerUpdated | 0xb30efa04327bb8a537d61cc1e5c48095345ad18ef7cc04e6bacf7dfb6caaf507 | halt |  |
| provider.aclAdminUpdated | PoolAddressesProvider.setACLAdmin → ACLAdminUpdated | 0xe9cf53972264dc95304fd424458745019ddfca0e37ae8f703d74772c41ad115b | halt |  |
| provider.priceOracleSentinelUpdated | PoolAddressesProvider.setPriceOracleSentinel → PriceOracleSentinelUpdated | 0x5326514eeca90494a14bedabcff812a0e683029ee85d1e23824d44fd14cd6ae7 | halt | sentinel gates liquidationCall/borrow (`isLiquidationAllowed`); the new sentinel must be read before liquidations are trusted — recompute does not model it |
| provider.proxyCreated | PoolAddressesProvider._updateImpl → ProxyCreated | 0x4a465a9bd819d9662563c1e11ae958f8109e437e7f4bf1c6ef0b9a7b3f35d478 | halt |  |
| provider.addressSet | PoolAddressesProvider.setAddress → AddressSet | 0x9ef0e8c8e52743bb38b83b17d9429141d494b8041ca6d616a6c77cebae9cd8b7 | halt |  |
| provider.addressSetAsProxy | PoolAddressesProvider.setAddressAsProxy → AddressSetAsProxy | 0x3bbd45b5429b385e3fb37ad5cd1cd1435a3c8ec32196c7937597365a3fd3e99c | halt |  |
| proxy.upgraded | BaseUpgradeabilityProxy._upgradeTo → Upgraded | 0xbc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b | halt | InitializableImmutableAdminUpgradeabilityProxy (Pool/aToken/vToken/configurator); InitializableAdminUpgradeabilityProxy in deps shares this topic |
| proxy.adminChanged | BaseAdminUpgradeabilityProxy._setAdmin → AdminChanged | 0x7e644d79422f17c01e4894b5f4f588d331ebfa28653d42ae832dc59e38c9798f | halt | mutable-admin proxy in deps; production immutable-admin proxy does not change admin |
| atoken.initialized | AToken.initialize → Initialized | 0xb19e051f8af41150ccccb3fc2c2d8d15f4a4cf434f32a559ba75fe73d6eea20b | halt | IInitializableAToken |
| vtoken.initialized | VariableDebtToken.initialize → Initialized | 0x40251fbfb6656cfa65a00d7879029fec1fad21d28fdcff2f4f68f52795b74f2c | halt | IInitializableDebtToken; different topic from aToken |
| gov.payloadExecuted | IPayloadsControllerCore.executePayload → PayloadExecuted | 0xda6084bb0aa902a7f6da10ba185d4aa129414651c90772417eff02a52112af2a | None | not in aave-v3-origin; aave-dao/aave-governance-v3@497226ef; subsequent protocol logs carry DirtySet/halt |

## Absent from this source (WP-named / declared)

| named path | why |
|---|---|
| swapBorrowRateMode | no match in `src/contracts` (stable rate removed 3.2) |
| StableDebtToken Transfer | no `StableDebtToken` contract |
| isolation / siloed / debt-ceiling events | no emit; `getSiloedBorrowing`/`getDebtCeiling` return constants; `__deprecatedIsolationModeTotalDebt` |
| Giver/Taker PM events | V3 PM is `approvePositionManager` + `setUserUseReserveAsCollateralOnBehalfOf` / `setUserEModeOnBehalfOf`; those emit the Pool rows above |
| ReserveInterestRateStrategyChanged | declared on `IPoolConfigurator`; **no emit** at this commit (rate updates emit `ReserveInterestRateDataChanged` only) |
| FlashloanPremiumToProtocolUpdated | declared; **no emit** (v3.4+ comment: premium-to-protocol is always 100%) |
