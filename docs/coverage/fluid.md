<!-- coverage-audit protocol=fluid repo=Instadapp/fluid-contracts-public commit=9496626f71a761fc296dc3b2efbfd54c504e18f0 date=2026-09-29 -->
# Fluid vault coverage

Source: `Instadapp/fluid-contracts-public` @ `9496626f71a761fc296dc3b2efbfd54c504e18f0`. Vaults: `contracts/protocols/vault/vaultT1|T2|T3|T4/coreModule/main.sol`, `vaultTypesCommon/coreModule/main.sol`. DEX: `contracts/protocols/dex/poolT1`. Factory: `contracts/protocols/vault/factory/main.sol`. Resolvers (test oracles only): `periphery/resolvers/vault`, `periphery/resolvers/dex`.

## Model

Fluid liquidates a vault's whole underwater tick range at once; `liquidate` takes no position id. The adapter keeps **one position per vault** and asks the vault, every block, what a full liquidation is. Nothing of Fluid's tick tree, branches or oracle is reproduced.

**Bind** (`Config::bind_live`, Multicall3 at the boot block): factory `totalVaults` / `getVaultAddress`; per vault `TYPE()` (T1 has none) and `constantsView()` in that type's shape; ERC-20 `decimals()` for smart-side tokens. Tokens map through the registry intern; native ETH (`0xEeee…`) maps to WETH, since the Executor pays and receives it as WETH. A vault with no interned token on one side is not read. A vault id past the 4001..=4999 MarketId band (vault 999) is left out and logged.

**Every block** (`liq-bot-state` thread, pinned to the head; folded on the ingest thread into that block's undo record, only while it is still the tip):

| stage | call | answer |
|---|---|---|
| 1 | T1 `liquidate(X128, 0, 0x…dEaD, absorb)`; T2–T4 `simulateLiquidate(0, absorb)`; each with and without `absorb` | `FluidLiquidateResult(col, debt)` in the vault's units: tokens, or DEX shares on a smart side. Any other revert (`Vault__InvalidLiquidation`) = nothing to liquidate |
| 2 | smart debt: borrow DEX `paybackPerfectInOneToken(shares, max|0, 0|max, estimate)`; smart collateral: supply DEX `withdrawPerfectInOneToken(shares, 1|0, 0|1, 0x…dEaD)`; each token | `FluidDexSingleTokenOutput` / `FluidDexLiquidityOutput`: the shares in that one token |

Absorb or not: Fluid's liquidation resolver's rule — absorb when the plain liquidation is empty, or when absorb adds size at no worse collateral per debt.

State: debt cell per debt-token slot = what the full liquidation costs **in that one token**; supply cell per collateral-token slot = what it pays **in that one token** (a smart side's two tokens are alternatives, not a sum). Position extra (`VaultExtra`): the liquidation in vault units (shares kept for the tail), the absorb choice, and the block time read.

**Health**: `Liquidatable` iff the latest read (current for its block and the next, `FRESH_SECS = 12`) has debt and collateral; debt without collateral is `BadDebt`; anything else `Healthy` with `NO_DEBT_HF`. `hf` has no price dependence (liquidatable = `RAY − 1`): the engine refolds a vault when the per-block read changes it, not on price thresholds.

**Quote**: one repay option per mapped debt token, one seize option per mapped collateral token, any pair; `bonus` = what the seize is worth over the preferred repay at the vector's prices (0 if not more). `SlotRef::Slot` names the token.

**Tail** (`liq_router::FluidPins` → `fluid_tail_from_quote`): vault type, one-token choice per side, absorb, native flags; floors `colPerUnitDebt` (vault units), T3/T4 `debtSharesMinPerToken`, T2/T4 `colPerShareMin`, each lowered by `min_out_tolerance_bps`. The Executor calls T1/T2 `liquidate` and T3/T4 `liquidate` with the exact token repay (`PLAN-ENCODING.md` §1b).

## Verified

- Live (`liq-bot` `state_reads::live_fluid_vault_answers_match_the_resolvers`): 182 vaults bound; every liquidation folded at the head equals `VaultResolver.getVaultLiquidation` in the chosen variant, and every one-token amount equals the `DexResolver` estimate, to the wei.
- Fork (`contracts/test/fork/ForkFluid.t.sol`, real Executor, real vaults): T1 native-ETH collateral; T2 smart collateral taken in ETH alone; T3 smart debt repaid in USDC alone.
- Unit: `fluid/tests/conformance.rs` (reads, follow-ups, folding, absorb rule, staleness, ten checks, bind), `contracts/test/unit/ExecutorFluid.t.sol` (all four ABIs, native both ways, share floor).

## Limits

- A vault deployed after bind is logged and read from the next restart.
- A one-token estimate the DEX refuses (dust below its `_verifyRedeem` bound, or a whole-vault size larger than one token's reserve) leaves that token without an option; the other token, if it answers, is still quoted. Sizing a smaller slice in that case is not done.
- T1 `liquidate` can repay one wei under `debtAmt_` (raw rounding); that wei of debt token stays in the Executor until `sweep()`.
- T4 is covered by unit mocks and the pinned ABI; its payback and withdraw paths are the T3 and T2 ones proved on the fork.

## Coverage

| path | function/event | log topic(s) | DirtySet | notes |
|---|---|---|---|---|
| factory.VaultDeployed | IFluidVaultFactory → VaultDeployed | 0x00fa89a51ae01c150bfde909191818194382d30b43b645428ed6a71f19551073 | None | vault rows for a bound vault; a vault deployed after bind is logged, not read |

Every other vault or factory event is intentionally unsubscribed: the per-block read is the source of truth.
