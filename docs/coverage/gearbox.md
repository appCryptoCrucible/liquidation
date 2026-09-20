<!-- coverage-audit protocol=gearbox repo=Gearbox-protocol/core-v3 commit=510fc6541c3767ce825929b4c311826fe81d6fa5 date=2026-09-21 -->
# Gearbox V3 event coverage

Source: `Gearbox-protocol/core-v3` @ `510fc6541c3767ce825929b4c311826fe81d6fa5`. Liquidation: `contracts/credit/CreditFacadeV3.sol` + `ICreditFacadeV3.sol`. Manager: `ICreditManagerV3.sol` (`liquidateCreditAccount` is `creditFacadeOnly` — do not call it from encode). Register: `IContractsRegister.sol` `0xA50d4E7D8946a7c90652339CDBd262c375d54D99`.

DirtySet lives in `liq-protocol` (D46). `halt` is GUIDE-03 HaltSink, not a DirtySet variant.

topic0 = keccak256(canonical ABI signature).

**Facade vs manager.** Permissionless liquidator entry is `CreditFacadeV3`. Manager liquidation is `creditFacadeOnly`. 10R must call the facade. **Do not change W** (no Gearbox decoder there).

`encode` validates then `ProtocolError::ExecutorUnwired` until 10R wires the facade ABI. No `ExecutorAdapter` discriminant. Happy-path Unwired is 10R. Check 9 cannot Ok — inapplicable, not starved with healthy-only fixtures.

## Health state

Pin: `_revertIfNotLiquidatable` — `isUnhealthy = twvUSD < totalDebtUSD`; expired accounts are liquidatable even if healthy. Fees/discount from per-manager `fees()`, not a protocol constant.

| condition | HealthState | quote |
|---|---|---|
| no debt | Healthy | None |
| paused facade | Blocked | None |
| `twvUSD < totalDebtUSD` (unhealthy) | Liquidatable | partial repay-and-seize if a non-underlying token is held; else None (full MultiCall unpriced) |
| expired even if `twvUSD >= totalDebtUSD` | Liquidatable | same |
| healthy and not expired | Healthy | None |

## encode validate order

Then `Err(ExecutorUnwired)`:

1. ProtocolMismatch
2. LegOutOfRange
3. CallbackProviderMismatch
4. ZeroRecipient
5. FundingAssetMismatch
6. FundingShort
7. OracleSourceMismatch (repay/seize not interned)
8. AmountTooLarge

## Carry-forwards (do not invent)

- `FeedId(0)` is the first interned Aave oracle, not unset. Gearbox oracles are absent from `registry.oracles`. Do not invent `FeedId::NONE`. `row.price_feed` must not be joined to ticks; health prices via AssetId / PriceVector. `feed=0` is a documented collision.
- No invented IRM: pool `SetInterestRateModel` has no rate in the event. Accrual uses stored `cumulativeIndexLastUpdate` as `indexNow` (zero growth) unless an index is stored. Wei gap vs `calcDebtAndCollateral` is a probe/drift gap.
- Pool `Repay(creditManager, borrowedAmount, profit, loss)` has **no credit account**. `decreaseDebt` without `PartiallyLiquidateCreditAccount` is not journaled. Debt can be overstated. Fail closed (may skip a now-healthy account or mis-size a quote) — do not guess a split across accounts.
- Facade `Execute` has no amounts; on-account adapter swaps are untracked. Full close `MultiCall` fills are **not** invented.
- `ILossPolicy.isLiquidatableWithLoss` is not evaluated off-chain. Full path is unpriced regardless.
- Phantom tokens: `WithdrawPhantomToken` subtracts when the token is listed; unlisted phantom is intern-only.
- Forbidden tokens still count toward value on-chain; adapter does not model `forbiddenTokenMask`.
- W has no Gearbox decoder. Leave W alone.
- MarketId **4200** is the ContractsRegister catalog (reserved, not a credit manager). Managers are **4201..=4299** as discovered. Never intern 0..=3480, 3481..=4199, or 4300+.
- D15 manager count 34 is a cardinality check on `getCreditManagers()`, not a hand-list universe.

| DirtySet | when |
|---|---|
| Positions | open/close/liquidate/partial/add/withdraw collateral, pool Borrow, UpdateQuota, factory take/deploy |
| MarketReprice | factory AddCreditManager (listing), UpdateFees, LT / ramp / expiration, facade pause/unpause |
| None | Execute / MultiCall bookends, pool Repay (no account), matching SetCreditFacade, IRM, limits, adapters, forbid/allow token |
| halt | proxy upgrade / admin / init after pin; SetPriceOracle; CreditConfiguratorUpgraded; SetCreditFacade mismatch; factory Rescue; unknown manager after pin |

## Coverage

| path | function/event | log topic(s) | DirtySet | notes |
|---|---|---|---|---|
| facade.open | OpenCreditAccount | 0x6e4927aac3383b13ffc5b6f44447693caf351f2f7ca800c9b4463b76997911b0 | Positions | PositionKey.user = credit account |
| facade.close | CloseCreditAccount | 0x460ad03b1cf79b1d64d3aefa28475f110ab66e84649c52bb41ed796b9b391981 | Positions | zeros debt/supply/extra |
| facade.liquidate | LiquidateCreditAccount | 0x7dfecd8419723a9d3954585a30c2a270165d70aafa146c11c1e1b88ae1439064 | Positions | remainingFunds not a per-token trail |
| facade.partial | PartiallyLiquidateCreditAccount | 0x04d7a59a828995563eaa48eb65f11b681f7fec2fb7d6bc1a5426243882f9d249 | Positions | repaidDebt / seizedCollateral wei |
| facade.addCollateral | AddCollateral | 0xa32435755c235de2976ed44a75a2f85cb01faf0c894f639fe0c32bb9455fea8f | Positions | enables token bit |
| facade.withdrawCollateral | WithdrawCollateral | 0xe7655dfddd0226889710c711da4e725dd44525fb5717b2321017a97d32793ab8 | Positions | |
| facade.startMultiCall | StartMultiCall | 0x6637691e02875fb5c598316278034ab86d133a75ab6d76491287290e03979284 | None | |
| facade.withdrawPhantom | WithdrawPhantomToken | 0xfb2a92d9536987a99026b9f077b3f5bc11912c1acc475a93d6dcbed8cf26b260 | Positions | listed token only |
| facade.execute | Execute | 0x1b835de7d84f000a333cdc5822ae62eb63b38d4c622ef96ac50f27db56d7c768 | None | no amounts — internal swaps untracked |
| facade.finishMultiCall | FinishMultiCall | 0x9fe19f2060e67aed557c7d1bc297d4bd2d8a8b952e3545c658ec4bc00be7d6c4 | None | |
| facade.paused | Paused | 0x62e78cea01bee320cd4e420270b5ea74000d11b0c9f74754ebdbfc544b05a258 | MarketReprice | Blocked |
| facade.unpaused | Unpaused | 0x5db9ee0a495bf2e6ff9c91a7834c1ba4fdd244a5e8aa4e537bd38aeae4b073aa | MarketReprice | |
| configurator.addToken | AddCollateralToken | 0x7c3f95f8569977586927f95930461a261e2121e326fcb513242f9e5c8b8ea6dc | None | universe is live `getCreditManagers` + `getTokenByMask` at boot |
| configurator.setLT | SetTokenLiquidationThreshold | 0xda5e841a0cb137f4a60661969e409f01ef7627723a4a929414e4f69b5475ee8c | MarketReprice | static LT (`timestampRampStart = type(uint40).max`) |
| configurator.rampLT | ScheduleTokenLiquidationThresholdRamp | 0xa8193c198aab4146e3640f414ba8473918c6d028f45b27fb08b185a16c15ce23 | MarketReprice | |
| configurator.forbidToken | ForbidToken | 0x9d65afef45c30b784a1e4621dbcbb194ebb6aabe16c9a4abce9ab1775a962b76 | None | forbidden still counts on-chain; unmodeled |
| configurator.allowToken | AllowToken | 0x14009112f2dcb15cad32dab6bf972d6d85286e4ae1178f27323ffe25359459e6 | None | |
| configurator.allowAdapter | AllowAdapter | 0x0bc09e53304ef58ff3ff8295411d9171c75ee4af48277db5fc605ab12e056bee | None | |
| configurator.forbidAdapter | ForbidAdapter | 0x3f688c7b4a117ceec70e927a9ed68836d3da0224eee121f856fc87ad5baa2a80 | None | |
| configurator.updateFees | UpdateFees | 0x2d179a43c34a4a80c102e61bb259930222752df8bdfc749e5f2fd6ef9dab971c | MarketReprice | premiums → discount = PF − premium; feeInterest unchanged |
| configurator.setPriceOracle | SetPriceOracle | 0x88a686e0e341d9099f2f990c3aa759a86822142a67579064b43ded9354a25662 | halt | after pin |
| configurator.setFacade | SetCreditFacade | 0x1cd439329e916b95ce297eb699326f2799c8de28be6bba10f28db1d9067778f1 | None | HaltSignal if address ≠ pin after `pinned_through` |
| configurator.upgraded | CreditConfiguratorUpgraded | 0x5a0b7d0f9c24b39256e112a0584b4c5ce38d8f1dee2e7c56f15b852604cdc886 | halt | after pin |
| configurator.borrowLimits | SetBorrowingLimits | 0xb2cc80ffa4c2f75731dbb99fcd29cccd7829c55d4cd5d6a884506b1435d6d1f3 | None | |
| configurator.maxDebtMult | SetMaxDebtPerBlockMultiplier | 0xaebbd82c9dcdcd553331f5850bbdf5add33bf8fce5c7c76e2c9e7912ad5f1564 | None | |
| configurator.lossPolicy | SetLossPolicy | 0x0dd9695b4eccd5a199100008de0ffc740332660d2783f935058d73a8dab61980 | None | full path unpriced; not evaluated off-chain |
| configurator.expiration | SetExpirationDate | 0xb019cf1dc4b3caa72aa4723abcc271a2bb3138bee0a89cd911fb8980b0c93d56 | MarketReprice | expired-but-healthy liquidatable |
| manager.setConfigurator | SetCreditConfigurator | 0xd87efcee33ed285df83ed2ffd66f67c15e0ecf17eb1f1705adae3ae2f1778da0 | None | HaltSignal if ≠ pin after pin |
| pool.borrow | Borrow | 0x312a5e5e1079f5dda4e95dbbd0b908b291fd5b992ef22073643ab691572c5b52 | Positions | routes via event `creditManager` (shared pools) |
| pool.repay | Repay | 0x2fe77b1c99aca6b022b8efc6e3e8dd1b48b30748709339b65c50ef3263443e09 | None | no creditAccount |
| pool.addManager | AddCreditManager | 0xbca7ba46bb626fab79d5a673d0d8293df21968a25350c4d71433f98600618f5f | None | register is the universe |
| pool.setIRM | SetInterestRateModel | 0x60d671e95013fc5fd0cf35d947791aa49209ad86fccf748e0b126f3f9f0a83ba | None | no rate in event — no invented IRM |
| pool.setQuotaKeeper | SetPoolQuotaKeeper | 0x553438de7e02bc6929ef4f6c3653130beca086dd506f1aa2785b58e6a13c3264 | None | |
| pool.setTotalDebt | SetTotalDebtLimit | 0x9154a5b15c38625466fe66233214f14f17fd994f819818caf08017b94d0787ba | None | |
| pool.setCmDebt | SetCreditManagerDebtLimit | 0xce20e043afe93acdab0352023688eb8da23cdfd33d80471cce1e6c9239662bcd | None | |
| pool.setWithdrawFee | SetWithdrawFee | 0x7be0a744e4d6f887e4fd578978ae62cb2568d860f0f2eb0a54fd0de804b16440 | None | |
| pool.uncoveredLoss | IncurUncoveredLoss | 0x33fc1787be707f18e553b02263e12d2fa6d2d40733535382066fd1d77e32c595 | None | |
| pool.refer | Refer | 0xd01c12ea61a25b0a57aa9b86b06dacf8f140567dd44ec9db66ef7955f6a956d2 | None | |
| factory.deploy | DeployCreditAccount | 0xe1b644b6334c18193b2f9111182fd3cf0af58bf954cd9949a56ef6a486a120e1 | Positions | incremental account |
| factory.take | TakeCreditAccount | 0x98b5155c93d7cea03235a2f37b8da764be2e15649b032ccb5cbbfe61cd716299 | Positions | |
| factory.return | ReturnCreditAccount | 0xe8d6fd8171676387a50d82e1751b2f078b786ff0700ed1877049c66f5b150d83 | None | |
| factory.addManager | AddCreditManager | 0x837f8321879761d39749a08e7aaca7841729ada6ac050818f4c30b6c82560a29 | MarketReprice | lists interned manager slots from live config |
| factory.rescue | Rescue | 0xd6593e4d07214fc404c843fe5d0f4431176033323e8796fb09177798fb0aaa9d | halt | after pin |
| quota.update | UpdateQuota | 0x22cce666192befd41ad1b89f8592d35a7ce7c6960853f89ada56db03bb61b096 | Positions | quoted flag + enable bit; TWV cap |
| quota.rate | UpdateTokenQuotaRate | 0xfb19913ea8fcd2e3d22d200707473d031876b05d1ecb42173e73292ed910ac85 | None | no invented quota IRM |
| quota.setGauge | SetGauge | 0x17228b08e4c958112a0827a6d8dc8475dba58dd068a3d400800a606794db02a6 | None | |
| quota.addManager | AddCreditManager | 0xbca7ba46bb626fab79d5a673d0d8293df21968a25350c4d71433f98600618f5f | None | same topic0 as pool AddCreditManager(address) |
| quota.addToken | AddQuotaToken | 0x7401ff10219be3dd6d26cc491114a8ae5a0e13ac3af651aae1286afad365947d | None | |
| quota.setLimit | SetTokenLimit | 0x86089ad7ab4cb6d03a20ccb3176599b628f4a4b80ceacf88369108bf10ffa1c9 | None | |
| quota.setFee | SetQuotaIncreaseFee | 0x1f985277936e1ecc9dd715575b48f1c6f18902eeb1a1b3a32779122296e64a66 | None | |
| proxy.upgraded | Upgraded | 0xbc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b | halt | after pin |
| proxy.adminChanged | AdminChanged | 0x7e644d79422f17c01e4894b5f4f588d331ebfa28653d42ae832dc59e38c9798f | halt | after pin |
| proxy.initialized | Initialized | 0xc7f505b2f371ae2175ee4913f4499e1f2633a7b5936321eed1cdaeb6115181d2 | halt | after pin |
