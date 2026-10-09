# Compound V2 family: which forks to bind

Plan 1B, Compound V2 family, step 5. Read on 2026-10-06 at block ~26,129,300 by
`compound_survey.py`, `compound_usd.py` and `compound_rank.py` (one-off survey
scripts, not committed). Liquidation counts come from the history scan's cache
(`data/history-cache/scan.jsonl`), blocks 25,481,527–26,113,835 (about 88 days).

## Classification of the 266 registry Comptrollers

| Class | Comptrollers | What it means |
|---|---:|---|
| V2, live (borrows > 0) | 111 | `getAllMarkets`, `oracle`, `closeFactorMantissa`, `liquidationIncentiveMantissa` answer; every cToken has `accrualBlockNumber`, `borrowRatePerBlock` and names this Comptroller |
| V2, nothing borrowed | 111 | nothing to liquidate |
| cToken views missing or Comptroller mismatch | 28 | not plain V2; each needs its source read before any binding |
| Not a V2 Comptroller (views missing) | 10 | look-alikes |
| Accrues per second (`accrualBlockTimestamp`) | 6 | our accrual is per block; a timestamp variant is a separate adapter change |

Borrowed USD of the 111 live forks, through each fork's own oracle
(`getUnderlyingPrice`, `1e36 / 10^decimals` scale): 16 above $1M (one with a
broken oracle that prices its borrows at ~10^43 USD), 10 at $100k–1M, 7 at
$10k–100k, 75 under $10k.

## Where liquidations actually happen

| Comptroller | Liquidations (88 days) | Borrowed | Note |
|---|---:|---:|---|
| `0x3d9819210a31b4961b30ef54be2aed79b9c9cd3b` Compound (bound) | 577 | $12.0M | |
| `0x0b9af1fd73885ad52680a1aeaa7a3f17ac702afa` Capyfi | 44 | $8.0M | plain V2 (no Fuse fee fields); **the one addition by evidence** |
| `0x814b02c1ebc9164972d888495927fe1697f0fb4c` Rari Fuse pool 6 (Tetranode) | 22 | $44k | Fuse: `fuseFeeMantissa` 10% |
| `0x621579dd26774022f33147d3852ef4e00024b763` Fuse pool 18 (Olympus gOHM) | 2 | $2.9M | Fuse |
| `0x35de88f04ad31a396aedb33f19aebe7787c02560` (fSDT-27) | 1 | $1.8k | Fuse |
| `0x2a6b72539c020f751e45e235606667c53c105aaf` (fUSDC-34) | 1 | $25 | Fuse |

Every other live fork had no liquidation in 88 days. The largest borrow books are
frozen bad debt, not opportunities: Cream `0x3105d328…` ($555M), Iron Bank
`0xab1c342c…` ($161M), Strike `0xe2e17b2c…` ($159M), Ola `0xcc53f8ff…` ($61M).
Their markets are insolvent or their prices stale, and nobody liquidates them.

## What binding each needs

- **Capyfi.**
  - Its debt markets we could flash are ordinary: USDT, USDC, WBTC, and ETH (two markets answer no `underlying()`).
  - Five underlyings are outside the registry: `0x0f6011f7…`, `0x0df3a853…`, `0xed025a9f…`, `0xd76f5faf…`, plus three idle ones. They need interning and exits (1D).
  - Before binding, its cToken source must be read against `a3214f67`, in particular the seize share and `accrueInterest`.
  - Acceptance: one of its 44 liquidations replays with our health equal to `getAccountLiquidity`.
- **Fuse pools.**
  - `accrueInterest` also accrues `totalFuseFees` and `totalAdminFees`, and the exchange rate subtracts both.
  - The adapter needs a Fuse variant of the exchange-rate and reserve math, from the deployed `CToken` (implementation `0x67db14e7…`), before any Fuse pool is bound.
  - The evidence above says the value is small: 26 liquidations in 88 days, on pools totalling under $3M.
- **Timestamp forks (6) and the 38 non-V2.** Not bound. Each is an adapter variant or a different protocol; revisit only if the scan shows liquidations on them.

## Bound (2026-10-06)

`tools/registry/gen_compound_toml.py` binds every plain-V2, Fuse and Moma
Comptroller: 223 forks (91 / 131 / 1), 1,483 cTokens.
- **Variants:**
  - Fuse (`CToken` `0x67db14e7…`) and Moma (`MToken` `0x1d0fcc81…`) carry two fee accumulators that the exchange rate subtracts. They are read every block, and the adapter projects them with each accrual.
  - The Moma pool (`0x40d39f0f…`, Moma Lending Pool) is the fork behind the uncontested 0.0388 ETH event 26106892. Its mTokens name the Comptroller `momaMaster()`, which is why it fell in the "comptroller mismatch" class above.
- **Capyfi:** LAC's only exit is the hookless LAC/USDC 1 % Uniswap V4 pool, now routed through swap venue 9.
- **Binding is per fork and per cToken:** a fork failing its live check, such as a zero incentive or a Comptroller not yet deployed at a replay's block, is left out on its own. A close factor of 1e18 is accepted (Fuse).
- **Halts are per fork:** a proxy event on one fork's Comptroller or cToken blocks that fork's positions only.
- **Replays:** both Capyfi and all four Fuse events show our health agreeing with `getAccountLiquidity`.

## The 28 left out for cToken views (2026-10-06, block ~26,133,400)

`gen_compound_toml.py` now classifies each listed cToken on its own instead of refusing the fork over one. **Result: 207 forks and 1,481 cTokens bound, all of them binding live** (`live_compound_binds_frozen_and_copycat_forks`).

- **Frozen markets.** Five cTokens in otherwise ordinary forks answer no `borrowRatePerBlock`. For three of them an `eth_call` of `accrueInterest()` reverts too: their rate model reverts, so their stored totals never move and any liquidation that touches them reverts (Cream v1's crCREAM, zenUSDT, pepeDAI).
  - They are pinned `frozen = true`. The account's liquidity counts them at their stored values, as the Comptroller does, and no leg repays or seizes them. The bind checks that `accrueInterest()` still reverts, and drops the pin otherwise.
  - Inverse's two xINV markets are not Compound code (its `accrueInterest` succeeds and `borrowRatePerBlock` does not exist), so they are left out. That binds **Cream v1 (92 cTokens), zen (64), Inverse (22) and pepe (8)**.
- **Copycats.** Six Comptrollers list cTokens that name another Comptroller, which is already bound. A copycat cannot seize those: the cToken's own Comptroller refuses in `seizeAllowed`. Their own markets do not help either, since all four copycats with own markets have a close factor or incentive of zero. The generator now leaves out any fork with a zero close factor or incentive (25 in all); the live check already dropped them at bind.
- **Renamed getters.**
  - **DeFiPie** (`0x36de5bbc…`, about $1.26M borrowed by its own oracle): its pTokens name the Controller `controller()`, sit behind `ProxyWithRegistry` (implementation at `registry().pTokenImplementation()`), and emit the five-field `AccrueInterest(…, totalReserves)`. Sources: Sourcify exact match for the Controller `0x1152d128…` and the pTokens `0xb4ef9b69…`/`0x489dc359…`. The liquidity and seize math are early Compound V2's (all of the seize goes to the liquidator). Bound with all 30 pTokens.
  - **`0x39313c37…`** names its Controller `controller()` and runs the original 2019 cToken; bound.
  - The rest, under `riskManager()` or no getter at all, are dead (no oracle, or a zero close factor or incentive) or hold under $40k.
- **Every pinned cToken's events are now proven from its code.** Its implementation (behind a delegator, an EIP-1967 or EIP-1167 proxy, or DeFiPie's registry proxy) must contain the topics of `Mint`, `Redeem`, `Borrow`, `RepayBorrow`, `Transfer` and one of three `AccrueInterest` forms. This dropped 59 cTokens that were bound before and whose events the adapter never saw:
  - NFT-collateral `CErc721…` markets, which have no borrow path in their code;
  - `PERC20` and `SeBep20` markets, whose `Mint`/`Redeem`/`Borrow` carry extra fields;
  - one market with no accrual event the adapter knows.
  
  An account that entered a dropped market fails closed: health now refuses any entered market outside the config, whatever its balance, because that market's events are not followed.
- **Found on the way: the official Compound's 2019 cTokens.** cETH, cUSDC, cBAT, cZRX, cREP, the first cWBTC and the first cDAI emit the original `AccrueInterest(interestAccumulated, borrowIndex, totalBorrows)`, with no `cashPrior` (`CEther.sol` of `0x4ddc2d19…`, Sourcify). The adapter followed only the four-field form, so these markets' accruals were never folded, and between events their borrow index stood still apart from the rate projection. The adapter now folds all three forms. The original form leaves cash to the deltas, and the five-field form takes the reserves as stated.
