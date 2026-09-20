<!-- mechanism-review protocol=fluid repo=Instadapp/fluid-contracts-public commit=9496626f71a761fc296dc3b2efbfd54c504e18f0 date=2026-09-20 verdict=admit -->
# Fluid — admit (not in registry.json)

Source: `Instadapp/fluid-contracts-public` @ `9496626f71a761fc296dc3b2efbfd54c504e18f0` (`main`, 2026-09-16). T1: `contracts/protocols/vault/vaultT1/coreModule/main.sol`. T2–T4: `vaultT2|T3|T4/coreModule/main.sol`. Factory: `contracts/protocols/vault/factory/main.sol` + `interfaces/iVaultFactory.sol`.

GUIDE-15 §1: "Novel collateral mechanics; read carefully." Not `SoftLiquidating`. Not in `registry.json` (D15 enumerated vaults; 15C still needs discovery wired).

## Mechanism — hard (vault-level seize, not per-NFT)

`liquidate` **does not take a user/NFT id**. Comment at this pin: "allows to liquidate all bad debt of all users at once. Liquidator can also liquidate partially any amount they want." Tick-threshold liquidation of underwater debt in one vault. `to_ == 0x…dEaD` reverts `FluidLiquidateResult` with actual amounts (try/catch quote). `absorb_ = true` consumes absorbed liquidity first.

Four vault types — **four `liquidate` ABIs**. Do not assume T1 on T3/T4.

| type | collateral | debt | `liquidate` args (this pin) |
|---|---|---|---|
| T1 | one token | one token | `(debtAmt, colPerUnitDebt, to, absorb)` |
| T2 | smart (token0+token1 shares) | one token | `(debtAmt, colPerUnitDebt, token0ColAmtPerUnitShares, token1ColAmtPerUnitShares, to, absorb)` |
| T3 | one token | smart (token0+token1) | `(token0DebtAmt, token1DebtAmt, debtSharesMin, colPerUnitDebt, to, absorb)` |
| T4 | smart | smart | T3 debt args + T2 col-per-share args |

T2/T3/T4 also expose `liquidatePerfect` (share-exact payback/withdraw). Oracle: `FluidOracle`, price **1e27**. Positions are factory ERC-721 (`fVLT`); liquidation is still vault-aggregate.

Not Dutch, not LLAMMA. Compatible with `HealthState::Liquidatable` if the adapter treats **(vault, currently liquidatable debt)** as the quote unit — not a per-NFT Aave account.

## Debt assets

Per vault, from vault constants / resolver views — **not** in `registry.json`. T1/T2: one borrow token. T3/T4: two borrow tokens (DEX-pair smart debt). Native token uses `msg.value`. Do not invent a token list; read each vault after factory enumeration.

## Flash depth (D09 + 07A)

Debt token(s) are vault-specific. Depth is log-driven `FlashSource::available` at runtime. For tokens already in 07A fixtures @ **26_000_000**:

| if borrow token is | Aave V3 | UniV3 n | UniV4 PM | Morpho | Sky DSS |
|---|---|---|---|---|---|
| USDC | aUSDC `181646545.035048` | 867 | PM `66230362.79…` | Morpho `108682339.58…` | no |
| WETH | aWETH `288592.198…` | 28034 | PM `2131.15…` | Morpho `15880.32…` | no |
| DAI | untracked → runtime | 92 | runtime | runtime | **yes** `max=5e8` wad |
| USDT / others | runtime SlotTable | registry.pools if present | runtime | runtime | no |

T3/T4 may require flashing **two** tokens (or one-sided `paybackPerfectInOneToken`). Aave fee_bps=5 @26M.

## Enumeration (REGISTRY §3 Fluid)

Root (D15, confirm live): VaultFactory **`0x324c5Dc1fC42c7a4D43d92df1eBA58A54d13Bf2d`**.

1. `VaultDeployed(address indexed vault, uint256 indexed vaultId)` logs, **or** `totalVaults()` + `getVaultAddress(id)` for `id = 1 .. totalVaults` (`IFluidVaultFactory`). Resolver views are convenience; factory logs are the prune-filter source of truth.
2. Per vault: type + supply/borrow tokens + oracle + liquidation params from vault/resolver. Do not hard-code a vault list.
3. Positions: factory `Transfer` / `NewPositionMinted` + vault operate events. Liquidatable set from resolver liquidation helpers or on-chain tick/health views. **No HTTP NFT API** on the hot path.

## Liquidation ABI → 10R-n (D48)

**New.** Four signatures (table above) + `liquidatePerfect` on T2–T4. Not Aave `liquidationCall`. Opens `10R-n`. `_isLiquidatable` must read vault tick/threshold (or dead-address quote), not Aave HF.
