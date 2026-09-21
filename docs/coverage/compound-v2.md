<!-- coverage-audit protocol=compound-v2 repo=compound-finance/compound-protocol commit=a3214f67b73310d547e00fc578e8355911c9d376 date=2026-09-21 -->
# Compound V2 forks event coverage

Source: `compound-finance/compound-protocol` @ `a3214f67b73310d547e00fc578e8355911c9d376` (`master` tip, 2022-06-07). `CErc20.liquidateBorrow` / `CEther.liquidateBorrow`; gate `Comptroller.liquidateBorrowAllowed` + `liquidateCalculateSeizeTokens`.

DirtySet lives in `liq-protocol` (D46). `halt` is GUIDE-03 HaltSink, not a DirtySet variant.

topic0 = keccak256(canonical ABI signature). Intern key is the **comptroller**. Official Unitroller `0x3d9819210A31b4961b30EF54bE2aeD79B9c9Cd3B` is **MarketId 355**. Family intern ids **295..=560** (266). cTokens are not intern markets. ProtocolId 3.

`encode` emits `ExecutorAdapter::CompoundV2` (id 8, tail 21 = `cTokenCollateral ‖ isCEther`). Liquidation ABI:

```
CErc20: liquidateBorrow(address borrower, uint repayAmount, address cTokenCollateral) returns (uint)
CEther: liquidateBorrow(address borrower, address cTokenCollateral) payable
```

CEther is discriminated by the **config pin** (`underlying == 0`), not a symbol and not an on-chain `underlying()` guess. Conformance check 9 is live after 10E. Do not starve checks 5/8/9/10 with healthy-only fixtures.

`health()` is Comptroller shortfall (`sumBorrow > sumColl`), not Aave HF. Equality is healthy. Deprecated-market path (`isDeprecated`) is also `Liquidatable`. Bonus = that comptroller's `liquidationIncentiveMantissa` (admin file). closeFactor is per-comptroller admin storage; pin bounds only `0.05e18 < x ≤ 0.9e18`. Live values from `assert_live_registry` eth_call + `NewCloseFactor` / `NewLiquidationIncentive`. Do not invent `1.08`.

`from_toml` leaves `interned` empty; `CompoundV2::new` refuses `EmptyInterned` and `LiveRegistryUnasserted`. Unknown comptroller → Unbound / UnexpectedLog. Do not invent MarketIds 3512+ / 4000+.

W decoder: `LiquidateBorrow(address liquidator, address borrower, uint256 repayAmount, address cTokenCollateral, uint256 seizeTokens)` — same topic0 as `liq-watch` `compound_v2`.

| DirtySet | when |
|---|---|
| Positions | Mint / Redeem / Borrow / RepayBorrow / LiquidateBorrow / Transfer (peer) / MarketEntered / MarketExited |
| MarketAccrual | AccrueInterest (borrowIndex + cash/reserves) |
| MarketReprice | MarketListed, NewCollateralFactor, NewReserveFactor, per-market ActionPaused(Borrow) |
| ProtocolWide | NewCloseFactor, NewLiquidationIncentive, NewPriceOracle, global ActionPaused(Seize) |
| None | Transfer mint/burn (zero address), unused pause strings |
| halt | ERC-1967 proxy / Initialized after `pinned_through` |

## Coverage

| path | function/event | log topic(s) | DirtySet | notes |
|---|---|---|---|---|
| cmp.listed | Comptroller._supportMarket → MarketListed | 0xcf583bb0c569eb967f806b11601c4cb93c10310485c67add5f8362c2f212321f | MarketReprice | lists cToken; intern market is the comptroller |
| cmp.entered | Comptroller.enterMarkets → MarketEntered | 0x3ab23ab0d51cccc0c3085aec51f99228625aa1a922b3a8ca89a26b0f2027a1a5 | Positions | entered mask bit |
| cmp.exited | Comptroller.exitMarket → MarketExited | 0xe699a64c18b07ac5b7301aa273f36a2287239eb9501d81950672794afba29a0d | Positions | |
| cmp.closeFactor | Comptroller._setCloseFactor → NewCloseFactor | 0x3b9670cf975d26958e754b57098eaa2ac914d8d2a31b83257997b9f346110fd9 | ProtocolWide | admin file; out-of-pin-bounds → halt |
| cmp.collateralFactor | Comptroller._setCollateralFactor → NewCollateralFactor | 0x70483e6592cd5182d45ac970e05bc62cdcc90e9d8ef2c2dbe686cf383bcd7fc5 | MarketReprice | |
| cmp.incentive | Comptroller._setLiquidationIncentive → NewLiquidationIncentive | 0xaeba5a6c40a8ac138134bff1aaa65debf25971188a58804bad717f82f0ec1316 | ProtocolWide | admin file, never 1.08 |
| cmp.oracle | Comptroller._setPriceOracle → NewPriceOracle | 0xd52b2b9b7e9ee655fcb95d2e5b9e0c9f69e7ef2b8e9d2d0ea78402d576d22e22 | ProtocolWide | per-fork oracle |
| cmp.pauseGlobal | Comptroller._setSeizePaused → ActionPaused(string,bool) | 0xef159d9a32b2472e32b098f954f3ce62d232939f1c207070b584df1814de2de0 | ProtocolWide | Seize |
| cmp.pauseMarket | Comptroller._setBorrowPaused → ActionPaused(address,string,bool) | 0x71aec636243f9709bb0007ae15e9afb8150ab01716d75fd7573be5cc096e03b0 | MarketReprice | Borrow |
| ctk.accrue | CToken.accrueInterest → AccrueInterest | 0x4dec04e750ca11537cabcd8a9eab06494de08da3735bc8871cd41250e190bc04 | MarketAccrual | cashPrior, borrowIndex, totalBorrows |
| ctk.mint | CToken.mintInternal → Mint | 0x4c209b5fc8ad50758f13e2e1088ba56a560dff690a1c6fef26394f4c03821c4f | Positions | |
| ctk.redeem | CToken.redeemInternal → Redeem | 0xe5b754fb1abb7f01b499791d0b820ae3b6af3424ac1c59768edb53f4ec31a929 | Positions | |
| ctk.borrow | CToken.borrowInternal → Borrow | 0x13ed6866d4e1ee6da46f845c46d7e54120883d75c5ea9a2dacc1c4ca8984ab80 | Positions | accountBorrows + index snap |
| ctk.repay | CToken.repayBorrowFresh → RepayBorrow | 0x1a2a22cb034d26d1854bdc6666a5b91fe25efbbb5dcad3b0355478d6f5c362a1 | Positions | |
| ctk.liquidate | CToken.liquidateBorrowFresh → LiquidateBorrow | 0x298637f684da70674f26509b10f07ec2fbc77a335ab1e7d6215a4b2484d8bb52 | Positions | watch ABI; same topic0 |
| ctk.rf | CToken._setReserveFactor → NewReserveFactor | 0xaaa68312e2ea9d50e16af5068410ab56e1a1fd06037b1a35664812c30f821460 | MarketReprice | |
| ctk.transfer | CToken.transfer → Transfer | 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef | Positions | skip mint/burn (zero address) |
| proxy.upgraded | ERC1967 → Upgraded | 0xbc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b | halt | |
| proxy.adminChanged | ERC1967 → AdminChanged | 0x7e644d79422f17c01e4894b5f4f588d331ebfa28653d42ae832dc59e38c9798f | halt | |
| proxy.initialized | Initializable → Initialized | 0xc7f505b2f371ae2175ee4913f4499e1f2633a7b5936321eed1cdaeb6115181d2 | halt | |
