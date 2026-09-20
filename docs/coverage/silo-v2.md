<!-- coverage-audit protocol=silo-v2 repo=silo-finance/silo-contracts-v2 commit=570a668a98a88a6a2b92697e7b9a3b1c6299dce7 date=2026-09-20 -->
# Silo V2 event coverage

Source: `silo-finance/silo-contracts-v2` @ `570a668a98a88a6a2b92697e7b9a3b1c6299dce7` (API may redirect to `silo-contracts-v3`; same SHA). Liquidation: `silo-core/contracts/hooks/liquidation/PartialLiquidation.sol` + `IPartialLiquidation.sol`. Factory: `ISiloFactory.sol`. Silo events: `ISilo.sol` + IERC-4626 / IERC-20.

DirtySet lives in `liq-protocol` (D46). `halt` is GUIDE-03 HaltSink, not a DirtySet variant.

topic0 = keccak256(canonical ABI signature).

**Hook receiver vs Silo.** `liquidationCall` / `maxLiquidation` / pin `LiquidationCall` are on the hook receiver (`IPartialLiquidation`). The Silo ERC-4626 is the collateral share token (`collateralShareToken == silo`) and exposes `isSolvent` / `repay` / deposit-borrow. Do not call `liquidationCall` on the Silo.

Pin `LiquidationCall(address,address,address,uint256,uint256,bool)` topic0 `0x3a84f644…` ≠ `liq-watch` `silo::LiquidationCall(address,address,uint256,uint256)` topic0 `0xaefcad93…`. Adapter decodes the pin ABI. **Do not change W** (carry-forward). 10R must use the pin ABI on `hook_receiver`.

`encode` validates then `ProtocolError::ExecutorUnwired` until 10R wires the hook ABI. No `ExecutorAdapter` discriminant. Happy-path Unwired is 10R. Check 9 cannot Ok — inapplicable, not starved with healthy-only fixtures.

## Health state

Pin: `SiloSolvencyLib.isSolvent` (`ltv <= lt`) and `PartialLiquidation`. `_BAD_DEBT = 1e18` changes *cover size* in `liquidationPreview` (any cover). `maxLiquidation` still returns amounts when `ltv > lt`. Example: coll 100, debt 150, fee 4% — cover 50 seizes 52.

| condition | HealthState | quote |
|---|---|---|
| no debt, or `ltv <= collateralConfig.lt` | Healthy | None |
| debt and **zero** coll+protected assets (hook `NoCollateralToLiquidate`) | BadDebt | None |
| paused silo | Blocked | None |
| else, including `ltv >= 1e18` with coll remaining | Liquidatable | `maxLiquidation` |

## encode validate order

Then `Err(ExecutorUnwired)`:

1. ProtocolMismatch
2. LegOutOfRange
3. CallbackProviderMismatch
4. ZeroRecipient
5. FundingAssetMismatch
6. FundingShort
7. AmountTooLarge

(`OracleSourceMismatch` if repay/seize asset is not interned, same as Euler, between FundingShort and AmountTooLarge.)

## Carry-forwards (do not invent)

- `FeedId(0)` is the first interned Aave oracle, not unset. Solvency oracles are absent from `registry.oracles`. Do not invent `FeedId::NONE`. `row.price_feed` must not be joined to ticks; health prices via AssetId / PriceVector. `feed=0` is a documented collision.
- In-memory accrue vs storage totals: fail-closed (no invented IRM). Adapter uses storage totals (`AccrueInterestInMemory.No`). Wei gap vs `Views.isSolvent` (which accrues) is left as a probe/drift gap, not patched with a guessed rate.
- W 4-arg topic0 `0xaefcad93…` vs pin `0x3a84f644…` on `hook_receiver`: do not change W. Adapter stays on pin ABI.
- `health_probe` stays `ProbeUnavailable` (`isSolvent` bool / `maxLiquidation` amounts are not RAY hf).

Admitted borrowed_usd at pin **block 26014442** (mechanism table; do not invent or round):

| debt silo | silo_config | borrowed_usd |
|---|---|---|
| `0xce6ab1c71981e79cd30052c521c162674251018a` | `0x10930071079d2cfff317aba9d2dd997309dd9985` | 375_815.26 |
| `0x1de3ba67da79a81bc0c3922689c98550e4bd9bc2` | `0xb0b37f551e18c540cf7748589d24589945fc1f61` | 2_238_887_423.44 |
| `0xf82c626e99c68e7af81f4e6afc8cd25ca13702db` | `0x74b21d458b9d5cf59f4b4e10a2e829c221670ee3` | 103_565.48 |

The 2.24e9 USDC row is an outlier vs the other two. Adapter re-reads storage totals; it does not substitute a rounded figure.

| DirtySet | when |
|---|---|
| Positions | deposit / withdraw / borrow / repay / share Transfer / hook LiquidationCall (intern only; wei on Repay+Transfer) |
| MarketAccrual | AccruedInterest (timestamp only — no IRM in the event) |
| MarketReprice | NewSilo (writes `getConfig` lt / fee / hook / VIEWED) |
| None | flash, hook start, fee withdraw, share-token listing, matching NewSiloHook |
| halt | proxy upgrade / admin / init after pin; NewSiloHook hook mismatch after pin |

## Coverage

| path | function/event | log topic(s) | DirtySet | notes |
|---|---|---|---|---|
| factory.createSilo | ISiloFactory.createSilo → NewSilo | 0x3d6b896c73b628ec6ba0bdfe3cdee1356ea2af31af2a97bbd6b532ca6fa00acb | MarketReprice | discovers interned pair; slot0=silo0 slot1=silo1 |
| factory.shareTokens | ISiloFactory → NewSiloShareTokens | 0x227b12975e5b52a120983b73ce5edacb34c8b24a9d2046001b3aececfe430b2a | None | collateral share == silo |
| factory.newHook | ISiloFactory → NewSiloHook | 0x762198150052a4717aaed334287e0cf90d2e8a6b1d8a14b48a0ba7059736b8d6 | None | HaltSignal if hook ≠ pin after `pinned_through` |
| hook.liquidationCall | PartialLiquidation.liquidationCall → LiquidationCall | 0x3a84f64446e8eada995aa9da2ddbfcd9b5d5d650503b19f024096d04c05ef2a9 | Positions | 3 indexed; emit includes debt `silo`; wei on Repay+Transfer |
| hook.liquidationStart | PartialLiquidation → LiquidationStart | 0xb59c4a578f1cf03f75770e14b8cbc94d58157d6f0be1fd91afc8416836b1ca10 | None | |
| silo.deposit | IERC4626.deposit → Deposit | 0xdcbc1c05240f31ff3ad067ef1ee35ce4997762752e3a095284754544f4c709d7 | Positions | collateral shares; mint Transfer skipped (from 0) |
| silo.depositProtected | ISilo.deposit → DepositProtected | 0x1ad348f5de3e19e23d88e34248ceb6ab09804a35a3abc0e7758045b7b9036acd | Positions | extra.protected_* |
| silo.withdraw | IERC4626.withdraw → Withdraw | 0xfbde797d201c681b91056529119e0b02407c7bb96a4a2c75c01fc9667232c8db | Positions | |
| silo.withdrawProtected | ISilo.withdraw → WithdrawProtected | 0x2dac01ea07cfa812db5881eaad96ba201c018c042b8f9ea7278cdf6f29e65f24 | Positions | |
| silo.borrow | ISilo.borrow → Borrow | 0x96558a334f4759f0e7c423d68c84721860bd8fbf94ddc4e55158ecb125ad04b5 | Positions | sets collateral_slot = other silo |
| silo.repay | ISilo.repay → Repay | 0xe4a1ae657f49cb1fb1c7d3a94ae6093565c4c8c0e03de488f79c377c3c3a24e0 | Positions | |
| silo.collateralTypeChanged | ISilo → CollateralTypeChanged | 0x4aa7ec2e9d912271d4af0f59b2735f825ef583f4d6ee04f0242be6f58febc0fc | Positions | borrowerCollateralSilo |
| silo.accruedInterest | ISilo → AccruedInterest | 0xd1fe0093c54116fe147366ec7de3cfa98e1e02e91d7ccb124241f6beae255b4c | MarketAccrual | last_update only; AccrueInterestInMemory.No |
| silo.flashLoan | ISilo → FlashLoan | 0x97bf554031869ec4edcf72b0dcdc2234dd406afe091f3631be088f348e179574 | None | |
| silo.hooksUpdated | ISilo → HooksUpdated | 0x2b7ec2cd1f9292c7a9ae800c2479f6be4217beacb0c01ef2d7e4e84fced3d75b | None | |
| silo.withdrawnFees | ISilo → WithdrawnFees | 0x58ce9a502017c1e4e0084fcfd7067e2642dfe9e11757d8017b9cf16d729967c7 | None | |
| silo.deployerFeesRedirected | ISilo → DeployerFeesRedirected | 0x9945fc9ff52146e6ed6c15193727e187fe57c705fb26ca772afc483191a3042d | None | |
| share.transfer | IERC20.transfer → Transfer | 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef | Positions | mint/burn skipped; debt transfer copies collateral_slot |
| proxy.upgraded | ERC1967 → Upgraded | 0xbc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b | halt | after pin |
| proxy.adminChanged | ERC1967 → AdminChanged | 0x7e644d79422f17c01e4894b5f4f588d331ebfa28653d42ae832dc59e38c9798f | halt | after pin |
| proxy.initialized | Initializable → Initialized | 0xc7f505b2f371ae2175ee4913f4499e1f2633a7b5936321eed1cdaeb6115181d2 | halt | after pin |
