<!-- coverage-audit protocol=morpho-blue repo=morpho-org/morpho-blue commit=8e26ca6a8dbc5089edcd67fb576248810fd2870a date=2026-09-09 -->
# Morpho Blue event coverage

Source: `morpho-org/morpho-blue` @ `8e26ca6a8dbc5089edcd67fb576248810fd2870a` (`main`, 2026-09-09). Singleton `Morpho.sol`; markets discovered from `CreateMarket`, never a hand list.

DirtySet lives in `liq-protocol` (D46). `halt` is GUIDE-03 HaltSink, not a DirtySet variant.

topic0 = keccak256(canonical ABI signature). `Id` → `bytes32`.

| DirtySet | when |
|---|---|
| Positions | user supply/borrow/collateral/liquidate shares |
| MarketAccrual | AccrueInterest (rate + totals) |
| MarketReprice | CreateMarket, SetFee |
| None | owner/IRM/LLTV allowlist, flash, auth |
| halt | proxy upgrade |

## Coverage

| path | function/event | log topic(s) | DirtySet | notes |
|---|---|---|---|---|
| morpho.createMarket | Morpho.createMarket → CreateMarket | 0xac4b2400f169220b0c0afdde7a0b32e775ba727ea1cb30b35f935cdaab8683ac | MarketReprice | discovers interned MarketId |
| morpho.supply | Morpho.supply → Supply | 0xedf8870433c83823eb071d3df1caa8d008f12f6440918c20d75a3602cda30fe0 | Positions | loan-token shares |
| morpho.withdraw | Morpho.withdraw → Withdraw | 0xa56fc0ad5702ec05ce63666221f796fb62437c32db1aa1aa075fc6484cf58fbf | Positions | |
| morpho.borrow | Morpho.borrow → Borrow | 0x570954540bed6b1304a87dfe815a5eda4a648f7097a16240dcd85c9b5fd42a43 | Positions | |
| morpho.repay | Morpho.repay → Repay | 0x52acb05cebbd3cd39715469f22afbf5a17496295ef3bc9bb5944056c63ccaa09 | Positions | totals saturating sub |
| morpho.supplyCollateral | Morpho.supplyCollateral → SupplyCollateral | 0xa3b9472a1399e17e123f3c2e6586c23e504184d504de59cdaa2b375e880c6184 | Positions | raw collateral |
| morpho.withdrawCollateral | Morpho.withdrawCollateral → WithdrawCollateral | 0xe80ebd7cc9223d7382aab2e0d1d6155c65651f83d53c8b9b06901d167e321142 | Positions | |
| morpho.liquidate | Morpho.liquidate → Liquidate | 0xa4946ede45d0c6f06a0f5ce92c9ad3b4751452d2fe0e25010783bcab57a67e41 | Positions | may realize bad debt |
| morpho.accrueInterest | Morpho._accrueInterest → AccrueInterest | 0x9d9bd501d0657d7dfe415f779a620a62b78bc508ddc0891fbbd8b7ac0f8fce87 | MarketAccrual | IRM rate snapshot |
| morpho.setFee | Morpho.setFee → SetFee | 0x139d6f58e9a127229667c8e3b36e88890a66cfc8ab1024ddc513e189e125b75b | MarketReprice | after accrue in same tx |
| morpho.setOwner | Morpho.setOwner → SetOwner | 0x167d3e9c1016ab80e58802ca9da10ce5c6a0f4debc46a2e7a2cd9e56899a4fb5 | None | |
| morpho.setFeeRecipient | Morpho.setFeeRecipient → SetFeeRecipient | 0x2e979f80fe4d43055c584cf4a8467c55875ea36728fc37176c05acd784eb7a73 | None | |
| morpho.enableIrm | Morpho.enableIrm → EnableIrm | 0x590e04cdebeccba40f566186b9746ad295a4cd358ea4fefaaea6ce79630d96c0 | None | |
| morpho.enableLltv | Morpho.enableLltv → EnableLltv | 0x297b80e7a896fad470c630f6575072d609bde997260ff3db851939405ec29139 | None | |
| morpho.flashLoan | Morpho.flashLoan → FlashLoan | 0xc76f1b4fe4396ac07a9fa55a415d4ca430e72651d37d3401f3bed7cb13fc4f12 | None | |
| morpho.setAuthorization | Morpho.setAuthorization → SetAuthorization | 0xd5e969f01efe921d3f766bdebad25f0a05e3f237311f56482bf132d0326309c0 | None | |
| morpho.incrementNonce | Morpho.setAuthorizationWithSig → IncrementNonce | 0xa58af1a0c70dba0c7aa60d1a1a147ebd61000d1690a968828ac718bca927f2c7 | None | |
| proxy.upgraded | ERC1967 → Upgraded | 0xbc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b | halt | |
| proxy.adminChanged | ERC1967 → AdminChanged | 0x7e644d79422f17c01e4894b5f4f588d331ebfa28653d42ae832dc59e38c9798f | halt | |
| proxy.initialized | Initializable → Initialized | 0xc7f505b2f371ae2175ee4913f4499e1f2633a7b5936321eed1cdaeb6115181d2 | halt | |

## Liquidation rules audit (2026-09-29)

Checked against the docs page (docs.morpho.org, "Liquidation") and the **deployed** singleton `0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb` (Sourcify-verified `src/Morpho.sol`, solc 0.8.19; immutable, no proxy).

| rule | deployed | adapter |
|---|---|---|
| liquidatable | `!_isHealthy`: `toAssetsUp(borrowShares) > mulDivDown(collateral, price, 1e36).wMulDown(lltv)` | `health.rs` H1, same directions |
| incentive | `min(1.15e18, WAD.wDivDown(WAD − 0.3e18.wMulDown(WAD − lltv)))`; docs: `min(M, 1/(β·LLTV + (1−β)))`, β 0.3, M 1.15 | `math::liquidation_incentive_factor`, pinned by `lif_pin` |
| close factor | none; exactly one of `seizedAssets` / `repaidShares` | full debt, or all collateral when the debt's seize exceeds it |
| seize from shares | `toAssetsDown · LIF (wMulDown) · 1e36 / price (mulDivDown)` | `quote.rs`, same order and directions |
| shares from seize | `mulDivUp(seized, price, 1e36).wDivUp(LIF).toSharesUp` | same |
| repaid assets | `repaidShares.toAssetsUp` | same |
| bad debt | when collateral reaches 0: remaining shares removed, `badDebtAssets = min(totalBorrowAssets, toAssetsUp(shares))` from borrow and supply totals | **fixed**: the fold removed the shares only when `badDebtAssets > 0`; now always (`bad_debt_shares_leave_the_totals_even_with_zero_bad_debt_assets`) |
| who | anyone; callback optional | Executor via callback |
| fee | none to the protocol | — |

**Interest between events.** Health accrues with the last `AccrueInterest.prevBorrowRate`. The AdaptiveCurveIRM moves its rate at target by at most ~50×/year, so over the minutes-to-hours between a market's events the rate difference changes accrued interest by a negligible fraction; the Executor's simulation is the final check.

**Not covered: pre-liquidations.** Borrowers can opt in to a `PreLiquidation` contract (factory `0x6FF33615e792E35ed1026ea7cACCf42D9BF83476`, `morpho-org/pre-liquidation`) that allows partial liquidation between `preLltv` and `lltv` with a linear close factor (`preLCF1..2`) and incentive (`preLIF1..2`). The adapter does not track these contracts or authorizations; opted-in borrowers are only seen once they cross the market's LLTV.
