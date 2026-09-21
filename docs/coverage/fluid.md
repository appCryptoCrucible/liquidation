<!-- coverage-audit protocol=fluid repo=Instadapp/fluid-contracts-public commit=9496626f71a761fc296dc3b2efbfd54c504e18f0 date=2026-09-21 -->
# Fluid vault event coverage

Source: `Instadapp/fluid-contracts-public` @ `9496626f71a761fc296dc3b2efbfd54c504e18f0`. Liquidation: `contracts/protocols/vault/vaultT1|T2|T3|T4/coreModule/main.sol`. Factory: `contracts/protocols/vault/factory/main.sol` + `interfaces/iVaultFactory.sol`.

DirtySet lives in `liq-protocol` (D46). `halt` is GUIDE-03 HaltSink, not a DirtySet variant.

topic0 = keccak256(canonical ABI signature).

**Quote unit.** `liquidate` does not take an NFT id. Adapter treats **(vault, currently liquidatable debt)** as the position (`PositionKey.user` = vault address).

**Four ABIs.** T1 `(debtAmt, colPerUnitDebt, to, absorb)`; T2 adds col-per-share; T3 `(token0Debt, token1Debt, debtSharesMin, colPerUnitDebt, to, absorb)`; T4 is T3+T2. T2/T3/T4 also `liquidatePerfect`. Type-dispatch from vault `TYPE`. Do not call T1 on T3/T4. **T2 and T3 `liquidate` share the same Solidity types** `(uint256×4, address, bool)` so `topic0`/selector collide — the adapter must branch on `TYPE` (20000 vs 30000), not the selector. T1 and T4 selectors differ.

`to_ == 0x…dEaD` reverts `FluidLiquidateResult` (try/catch quote). 10R must use that path or the pin tick walk. This adapter quotes the **single-segment perfect-tick** formula (top tick → liquidation tick) when `n_nfts == 1` and `tickStatus == 1`. Multi-NFT top-tick / partials are fail-closed (`OracleSourceMismatch`). W has no Fluid decoder — leave W alone.

T1 `encode` emits `ExecutorAdapter::Fluid` (id 6, tail 32 = quoted `colPerUnitDebt`; `absorb_ = true` hardcoded). T2/T3/T4 stay `ProtocolError::ExecutorUnwired` — never call the T1 ABI on them. Check 9 is live for T1 after 10E. Do not starve checks 5/9/10 with healthy-only fixtures.

`absorb_ = true` consumes absorbed liquidity first (pin). Native token uses `msg.value` — native vaults are UNPRICED (not WETH).

Oracle: FluidOracle **1e27**. Adapter composes that rate from `PriceVector` RAY (floor). TWAP/DEX share vs the vector is a documented drift gap. Missing price → `MissingPrice` / UNPRICED, never $1.

## Health state

Pin: liquidate iff `top_tick > liquidation_tick` (threshold 3-dec in `vaultVariables2 >> 42`). `hf = ratio(liq_tick) / ratio(top_tick)` RAY. Equality is Healthy.

| condition | HealthState | quote |
|---|---|---|
| no debt (and no absorbed debt) | Healthy | None |
| debt and **zero** seizable coll (incl. absorbed) | BadDebt | None |
| `top_tick > liquidation_tick` (or absorbed debt with coll) | Liquidatable | single-segment + absorbed |
| `n_nfts > 1` without proven top tick | `OracleSourceMismatch` | — |
| paused | Blocked | None |

## encode validate order

Then `Err(ExecutorUnwired)`:

1. ProtocolMismatch
2. LegOutOfRange
3. CallbackProviderMismatch
4. ZeroRecipient
5. FundingAssetMismatch
6. FundingShort
7. OracleSourceMismatch (repay/seize asset not interned)
8. AmountTooLarge

## Carry-forwards (do not invent)

- MarketIds **4000..=4199** only. Catalog 4000. `vaultId` → `4000 + vaultId`. Gearbox owns 4200..=4299.
- D15 `totalVaults() == 182` is a cardinality check, not a roster. `subscriptions()` covers the factory plus pinned vaults; watch must add vaults after `VaultDeployed` (not a liq-watch change in this WP).
- `NewPositionMinted` increments `n_nfts`. Factory `Transfer` does not name the vault — burns do not decrement. Conservative: extra mints lose `TOP_KNOWN`.
- Tick tree / partials / `liquidatePerfect` share splits: fail-closed. 10R + dead-address quote is the live max path.
- `health_probe` is `ProbeUnavailable` (no RAY hf view; `FluidLiquidateResult` is amounts).
- FluidOracle 1e27 vs PriceVector composition: drift detector gap, not a $1 fallback.
- T2/T3/T4 health/quote/operate/liquidate fail closed (`vault_type != T1` or `n_col|n_debt != 1`). Not T1-cloned.
- Native `0xEeee…` is not in `registry.tokens`. Do not alias WETH.

| DirtySet | when |
|---|---|
| Positions | LogOperate / LogLiquidate / LogAbsorb |
| MarketAccrual | LogUpdateExchangePrice |
| MarketReprice | VaultDeployed, NewPositionMinted, liquidation threshold/max/penalty/oracle/core settings |
| None | Transfer, factory auth, rebalance, unused admin rate/fee/rescue, halt-class at or before pin |
| halt | proxy upgrade / admin / init after pin; LogUpdateOracle after pin |

## Coverage

| path | function/event | log topic(s) | DirtySet | notes |
|---|---|---|---|---|
| factory.VaultDeployed | IFluidVaultFactory → VaultDeployed | 0x00fa89a51ae01c150bfde909191818194382d30b43b645428ed6a71f19551073 | MarketReprice | catalog + market `4000+vaultId`; pin fills VIEWED |
| factory.NewPositionMinted | factory → NewPositionMinted | 0xfcc2278353c4cc5d54b742d7eee2d4a7abc22e4dc6213340088293860d502b51 | MarketReprice | `n_nfts++`; TOP_KNOWN only if `n_nfts==1` |
| factory.Transfer | ERC-721 Transfer | 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef | None | no vault id on the event |
| factory.LogSetDeployer | factory auth | 0x48cc5b4660fae22eabe5e803ee595e63572773d114bcd54ecc118c1efa8d75af | None | |
| factory.LogSetGlobalAuth | factory auth | 0x0a1c6cd77aa2e405e482adf6ee6cf190a27682b6dd1234403f7602e5203c83bb | None | |
| factory.LogSetVaultAuth | factory auth | 0x7aee16d2c366535c2577e873699b458af55a0b0bd4c4fab5e930a780f05669d7 | None | |
| factory.LogSetVaultDeploymentLogic | factory auth | 0x6e71f281df08e5962589123c1ca39a8c9df25c6c9cfa7b6d1525effed3dafd21 | None | |
| vault.LogOperate | operate → LogOperate | 0xfef64760e30a41b9d5ba7dd65ff7236a61d89ed8b44c67a29e84db1a67513a1c | Positions | token→raw at then-current ex; persist raw; tick if single NFT |
| vault.LogUpdateExchangePrice | updateExchangePrices | 0xcde545703e0372175cadfff811d67c32910c3dcb33199679b3271c4106afdf9a | MarketAccrual | 1e12 |
| vault.LogLiquidate | liquidate → LogLiquidate | 0x80fd9cc6b1821f4a510e45ffce6852ea3404807b5d3d833ffa85664408afcb66 | Positions | token amounts; tickStatus=2 |
| vault.LogAbsorb | absorb → LogAbsorb | 0x115609402b8e0707cb9654c5da38e5c0790ccad443a92f71160fe645aa342d04 | Positions | extra absorbed raw |
| vault.LogRebalance | rebalance | 0x9a85dfb89c634cdc63db5d8cedaf8f9cfa4926df888bad563d70b7314a33a0ae | None | not folded |
| admin.LogUpdateLiquidationThreshold | admin | 0x44a667dd6218a52f7ef808da1e39e9c8497db215eaff093c0c42ecf9bf2168f3 | MarketReprice | event 1e2 / 10 → packed 3-dec |
| admin.LogUpdateLiquidationMaxLimit | admin | 0x5ac1492eb2009d4983693cc59b4ec4032b506a0f9cbefc19a7545af5945d26af | MarketReprice | |
| admin.LogUpdateLiquidationPenalty | admin | 0xd3d6bb99321a653b7fc969c9f2a1917bd9623cab8ecaa67a2e1bec34c3eb2e1c | MarketReprice | 4-dec as-is |
| admin.LogUpdateOracle | admin | 0x7a46205dbf7cc57a79f474f580e08d7961ad634e14643486b8df2dc30c392b62 | MarketReprice | HaltSignal after pin |
| admin.LogUpdateCoreSettings | admin | 0xf992c18e9b1aec434f456c32a556717f76f0f19cba16d430d4ba1be0813ab3e8 | MarketReprice | |
| admin.LogUpdateSupplyRateMagnifier | admin | 0x06f8b08c94d657867f843433de70bed3628bbdc19b0c89413af75d30420ad3f3 | None | |
| admin.LogUpdateBorrowRateMagnifier | admin | 0x8d6a11b15739c2d7a6d0a69b9d322262db8c1fb2e4b96239e4e4733df8a5164e | None | |
| admin.LogUpdateCollateralFactor | admin | 0x0e8160f7246256e8f7eea7dc5ee9de8c9fa1d6057c30561f548e6a84defeef15 | None | |
| admin.LogUpdateWithdrawGap | admin | 0xba3aefe95d9bb126dd0e7885e76f453e72b0ee6efd457771f26fe0a7ca56cedc | None | |
| admin.LogUpdateBorrowFee | admin | 0x06a28e5e1500bd478bd28b400a0fb46a9cc8748a5dac616b38bc91c29462c17f | None | |
| admin.LogUpdateRebalancer | admin | 0xdb94ee7fd8b5bbf8f6d59e76731ff4b4f5a02ab3af1d3e0c774862cf96ff613b | None | |
| admin.LogRescueFunds | admin | 0xdff2a3947bcf9fc0807b142e7c8497066db9183428b7bdbfb1fcd0f55c27a3df | None | |
| admin.LogAbsorbDustDebt | admin | 0xae8abcd7cc16d6da9fa7098d41cc4cdb3bd5ce892e46f15d904b44c9b156cb5e | None | |
| halt.Upgraded | ERC1967 | 0xbc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b | halt | HaltSignal after pin |
| halt.AdminChanged | ERC1967 | 0x7e644d79422f17c01e4894b5f4f588d331ebfa28653d42ae832dc59e38c9798f | halt | |
| halt.Initialized | ERC1967 | 0xc7f505b2f371ae2175ee4913f4499e1f2633a7b5936321eed1cdaeb6115181d2 | halt | |
