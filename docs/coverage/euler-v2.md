<!-- coverage-audit protocol=euler-v2 repo=euler-xyz/euler-vault-kit commit=bfb325a6e6ca09613d940b46f72ccfe017353933 date=2026-09-20 -->
# Euler V2 (EVK) event coverage

Source: `euler-xyz/euler-vault-kit` @ `bfb325a6e6ca09613d940b46f72ccfe017353933` (`master`, 2026-09-01). Liquidation: `src/EVault/modules/Liquidation.sol`. Factory: `src/GenericFactory/GenericFactory.sol`. EVC collateral/controller enablement is not vault storage.

DirtySet lives in `liq-protocol` (D46). `halt` is GUIDE-03 HaltSink, not a DirtySet variant.

topic0 = keccak256(canonical ABI signature). Enumeration is Family B: `ProxyCreated` / `getProxyListSlice` on GenericFactory `0x29a56a1b8214D9Cf7c5561811750D5cBDb45CC8e`. Config `vaults` are admitted-address **subscriptions** from the registry pin (block **26015175**), never the live universe.

`encode` returns `ProtocolError::ExecutorUnwired` (D48 / 10R). Liquidation ABI is not an `ExecutorAdapter` discriminant:

```
liquidate(address violator, address collateral, uint256 repayAssets, uint256 minYieldBalance)
```

Target = **debt** EVault. `collateral` = collateral vault address (shares), not underlying. Documented for the 10R redeploy WP; this adapter does not invent a wire id.

Conformance check 9 (`encode` accepts every callback shape) is **inapplicable** until 10R: `run()` with a liquidatable fixture executes checks 5 and 8 then fails check 9 with `ExecutorUnwired`. Do not starve those checks with healthy-only fixtures.

`health_probe` is `accountLiquidity(account, true)` on the debt vault (`RiskManager.sol`). `checkLiquidation` returns `(0,0)` when healthy and cannot recover HF.

Last-healthy `liquidation_price` uses the same predicate as `health()`: liquidatable when `collateralAdjustedValue <= liabilityValue` (HF == 1 is liquidatable).

`quote()` emits one `(repay, seize)` pair: `calculateMaxLiquidation` for the preferred collateral (bonus desc, then seizable value desc). It does not attach `max(repay across collaterals)` to every `SeizeOption`.

## MarketId allocator

`StateStore.market_index` is intern-global (`MarketId → row`, no `ProtocolId`). Allocator:

1. Bind each euler-v2 vault `OnChainId::Addr` to `Intern::from_registry` `MarketRec.id` (`registry.protocols` iteration order). Registry has ~884 euler-v2 rows (26 admitted). Bind every interned vault the adapter may intern from logs. W `WatchDecoder` already maps `Liquidate` to those intern ids.
2. Catalog (vault→MarketId index, not a vault) is **not** interned: **3511**.
3. Vaults not in intern (new `ProxyCreated`) take sequential ids from **3512**. Never 3481–3510 (Liquity rework owns 3508–3510).

Fail-closed gaps: `maxLiquidationDiscount` is per-vault (Initialize storage default 0 is recorded as known, not a protocol cap). Unpinned Euler oracles in `registry.oracles` → vault rows `UNPRICED`. Collateral vault ERC-20s are absent from `registry.tokens` → `UNMAPPED` / `OracleSourceMismatch` until interned. New `ProxyCreated` vaults after the pin are interned in-store but are not in the union filter until `config.vaults` refresh. Owed that does not fit `u128` is refused (EVK max is 143-bit).

W decoder: `Liquidate(address indexed liquidator, address indexed violator, address collateral, uint256 repayAssets, uint256 yieldBalance)` topic0 below; matches `crates/liq-watch/src/abi.rs`.

| DirtySet | when |
|---|---|
| Positions | share Transfer/Deposit/Withdraw, Borrow/Repay, EVC enable, Liquidate intern |
| MarketAccrual | VaultStatus (accumulator + rate) |
| MarketReprice | ProxyCreated/EVaultCreated, GovSetLTV/discount/cool-off/hooks/flags/fee |
| None | allowlist, fees convert, IRM address, flash-neutral, EVC owner |
| halt | factory beacon, proxy admin, governor admin |

## Coverage

| path | function/event | log topic(s) | DirtySet | notes |
|---|---|---|---|---|
| factory.genesis | GenericFactory constructor → Genesis | 0x6bf6eaff5e9af8fbccb949f4c38cc016936f8775363ccf4224db160365785d52 | None | |
| factory.proxyCreated | GenericFactory.createProxy → ProxyCreated | 0x04e664079117e113faa9684bc14aecb41651cbf098b14eda271248c6d0cda57c | MarketReprice | trailingData = asset\|\|oracle\|\|unit (60 bytes); discovers MarketId |
| factory.setImplementation | GenericFactory.setImplementation → SetImplementation | 0xddebe6de740fe0dd01cc33ffa314d11c6ac6acbbe50b80513c4c360ae7aa4f04 | halt | beacon upgrade, all upgradeable proxies |
| factory.setUpgradeAdmin | GenericFactory.setUpgradeAdmin → SetUpgradeAdmin | 0x7b1ebd0f3ec81bf1cd5f478166ec87beaea1eee7f3bc2612295ae161048a239f | halt | |
| vault.created | Initialize.initialize → EVaultCreated | 0x0cd345140b9008a43f99a999a328ece572a0193e8c8bf5f5755585e6f293b85e | MarketReprice | emitted before ProxyCreated in the factory tx |
| vault.status | RiskManager.checkVaultStatus → VaultStatus | 0x80b61abbfc5f73cfe5cf93cec97a69ed20643dc6c6f1833b05a1560aa164e24c | MarketAccrual | interestAccumulator + interestRate |
| vault.transfer | Token.transfer → Transfer | 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef | Positions | peer only; mint/burn from Deposit/Withdraw |
| vault.approval | Token.approve → Approval | 0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925 | None | |
| vault.deposit | Vault.deposit/mint → Deposit | 0xdcbc1c05240f31ff3ad067ef1ee35ce4997762752e3a095284754544f4c709d7 | Positions | collateral vault shares |
| vault.withdraw | Vault.withdraw/redeem → Withdraw | 0xfbde797d201c681b91056529119e0b02407c7bb96a4a2c75c01fc9667232c8db | Positions | |
| vault.borrow | Borrowing.borrow → Borrow | 0xcbc04eca7e9da35cb1393a6135a199ca52e450d5e9251cbd99f7847d33a36750 | Positions | assets; owed = assets << 31 |
| vault.repay | Borrowing.repay → Repay | 0x5c16de4f8b59bd9caf0f49a545f25819a895ed223294290b408242e72a594231 | Positions | decreaseBorrow: toAssetsUp then << 31 |
| vault.interestAccrued | BorrowUtils.logBorrow/logRepay → InterestAccrued | 0x5e804d42ae3b860f881d11cb44a4bb1f2f0d5b3d081f5539a32d6f97b629d978 | None | folded via Borrow/Repay + accumulator |
| vault.liquidate | Liquidation.liquidate → Liquidate | 0x8246cc71ab01533b5bebc672a636df812f10637ad720797319d5741d5ebb3962 | Positions | intern only; balances from Repay/Borrow/Transfer |
| vault.pullDebt | Borrowing.pullDebt → PullDebt | 0xe6d0bfd9025bf59969101a13cf02e3ba2811b533816c47d7155546c7c8a1048f | Positions | also emits Repay+Borrow |
| vault.debtSocialized | Liquidation.executeLiquidation → DebtSocialized | 0xe786d0bc2e83bf230ed9895a9c4d7756ab0c6e22eb8a4ff69c161ece76bd36df | Positions | leftover after worthless collateral |
| vault.convertFees | Governance.convertFees → ConvertFees | 0x4e16b07cac5fe5604af487e07b1b62efc8bd47477b18839f4688d2cae957f965 | None | |
| vault.balanceForwarder | BalanceForwarder → BalanceForwarderStatus | 0xc3e011ddce6181dafb5798a536341c7c601913626c31d31744f91b77b7e2412d | None | |
| gov.governorAdmin | Governance.setGovernorAdmin → GovSetGovernorAdmin | 0x1c145a4cd16d4148579b0f2296884ac4aa47536e4ef10a32e1cdc0dc3dd20ea4 | halt | authority |
| gov.feeReceiver | Governance.setFeeReceiver → GovSetFeeReceiver | 0x836a1afef2bc89de2cb4713cc8d312fccf2ff835230721c5f41f13374707413a | None | |
| gov.ltv | Governance.setLTV → GovSetLTV | 0xc69392046c26324e9eee913208811542aabcbde6a41ce9ee3b45473b18eb3c76 | MarketReprice | recognized iff targetTimestamp != 0 |
| gov.irm | Governance.setInterestRateModel → GovSetInterestRateModel | 0xe5f2a795fc5f8baf1b05659293834c88859298226d87422c88624b4c9f4d3a43 | None | rate via VaultStatus |
| gov.maxLiquidationDiscount | Governance.setMaxLiquidationDiscount → GovSetMaxLiquidationDiscount | 0x558a63d245d08220a137de3573129d3921e70e806adccf3a068c4723b9b3322d | MarketReprice | per-vault; != CONFIG_SCALE |
| gov.coolOff | Governance.setLiquidationCoolOffTime → GovSetLiquidationCoolOffTime | 0xdf4edc1d288e7b3306b287d03fd77b2070b8b308c702bf7297f72d928175dfa5 | MarketReprice | |
| gov.hookConfig | Governance.setHookConfig → GovSetHookConfig | 0xabadffb695acdb6863cd1324a91e5c359712b9110a55f9103774e2fb67dedb6a | MarketReprice | OP_LIQUIDATE hooked ⇒ paused |
| gov.configFlags | Governance.setConfigFlags → GovSetConfigFlags | 0xe7f84c52c0ef295afe77de8cb30516d6f28d50306f979b45776dd1b619ae5ffc | MarketReprice | CFG_DONT_SOCIALIZE_DEBT |
| gov.caps | Governance.setCaps → GovSetCaps | 0xadbdcd178dfddc478805a3703b6cf3b72ca5e78ecebacffe1aad03188cc1cbf4 | None | not an HF input |
| gov.interestFee | Governance.setInterestFee → GovSetInterestFee | 0x634a58674e370383703eff32d9d4e4b3d1add94d50e8bcb631b04995d8e47341 | MarketReprice | |
| evc.collateral | EVC → CollateralStatus | 0xf022705c827017c972043d1984cfddc7958c9f4685b4d9ce8bd68696f4381cd2 | Positions | enable mask |
| evc.controller | EVC → ControllerStatus | 0x9919d437ee612d4ec7bba88a7d9bc4fc36a0a23608ad6259252711a46b708af9 | Positions | intern on enable |
| evc.accountStatusCheck | EVC → AccountStatusCheck | 0x889a4d4628b31342e420737e2aeb45387087570710d26239aa8a5f13d3e829d4 | Positions | cool-off clock |
| evc.ownerRegistered | EVC → OwnerRegistered | 0x67cb2734834e775d6db886bf16ac03d7273b290223ee5363354b385ec5b643b0 | None | |
| evc.lockdown | EVC → LockdownModeStatus | 0xaf5120bc58372f0063d8362c9bba9070c462c07ae24c24082d080a426432798b | None | |
| evc.vaultStatusCheck | EVC → VaultStatusCheck | 0xaea973cfb51ea8ca328767d72f105b5b9d2360c65f5ac4110a2c4470434471c9 | None | |
| proxy.upgraded | ERC1967 → Upgraded | 0xbc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b | halt | |
| proxy.adminChanged | ERC1967 → AdminChanged | 0x7e644d79422f17c01e4894b5f4f588d331ebfa28653d42ae832dc59e38c9798f | halt | |
| proxy.initialized | Initializable → Initialized | 0xc7f505b2f371ae2175ee4913f4499e1f2633a7b5936321eed1cdaeb6115181d2 | halt | |
