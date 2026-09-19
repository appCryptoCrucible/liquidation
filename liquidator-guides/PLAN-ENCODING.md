# Plan Encoding — the Solidity ⇄ Rust wire format

`Executor.execute(bytes calldata plan)` takes one opaque blob. This document is
the single source of truth for its layout. `Executor._decode` and the Rust
encoder in `liq-exec` must agree byte-for-byte, and a mismatch is a silent
misliquidation, not a compile error — so the round-trip test in §4 is mandatory.

---

## 1. Layout

Four parts: a small shared header, a flash-group blob, and — inside each group —
its liquidation legs and the swap legs that fund its repay. Profit swaps come
last, once, globally.

```
[ header 35 ]
[ groupCount 1 ]
  ├─ group 0: [ head 59 ][ liq legs 77×N ][ repay swap legs ]
  ├─ group 1: [ head 59 ][ liq legs 77×N ][ repay swap legs ]
  └─ …
[ profitSwapCount 1 ][ profit swap legs ]
```

No offset is ever encoded. Swap legs are variable length, so the decoder walks
them — an encoded offset is a second source of truth for a fact already in the
data, and the two drift.

### 1a. Header — what the whole plan shares

| Offset | Size | Field | Type | Notes |
|---:|---:|---|---|---|
| 0 | 1 | `flags` | u8 | bit0 `SWEEP` |
| 1 | 2 | `bidBps` | u16 | fraction of realized net paid to `block.coinbase` |
| 3 | 16 | `gasCostWei` | u128 | predicted total gas cost; base fee is known exactly a block ahead |
| 19 | 16 | `minProfit` | u128 | **in wei**, what we keep *after* the bid |

**Note what is not here: the flashloan.** One oracle update makes positions
liquidatable across many debt assets, so a plan carries several flash groups. What
*is* shared is one profit floor, one bid fraction, one gas estimate.

`minProfit` is in wei because profit converges on ETH from every group — which is
what makes multi-asset batching tractable at all. Were profit per-debt-asset you
would need one guard and one threshold per group.

### 1b. Flash groups

```
1 byte    groupCount
per group, head = 59 bytes:
  1 byte    provider         0 Aave · 1 UniV3 · 2 UniV4 · 3 Morpho · 4 Sky DSS Flash
  20 bytes  flashSource
  20 bytes  debtAsset
  16 bytes  flashAmount      u128 — may exceed the sum of repays, deliberately
  1 byte    liqCount
  1 byte    repaySwapCount
then: liqCount × 77 bytes of liquidation legs
then: repaySwapCount swap legs (all EXACT_OUT into debtAsset)
```

**Sequential, never nested (D32).** Each group borrows, liquidates, swaps to
cover its own repay, and settles before the next starts. Nesting would deepen the
call stack for no benefit and make every callback's authentication harder to reason
about. `Executor.execute` already runs groups in a plain `for` loop: arm →
`_initiate` (returns only after the callback settled) → disarm → next group. No
contract change is required for multi-source funding.

Balances are shared across groups, so a later group's repay swap can spend
collateral an earlier group seized. That falls out of the contract holding
everything; it is not something the encoder has to arrange.

**A single-position plan is one group with `liqCount == 1`.** No special case —
the batched path is the only path, so it is exercised by every test rather than by
a rare one.

**Over-borrow on purpose.** `flashAmount` should exceed the sum of `repayAmount`
when a leg might be taken by someone else first. With a zero-fee source the surplus
is free: it round-trips untouched. Size to the exact sum and a single lost leg
leaves you unable to repay.

### 1b′. Multi-source flash cascade (same debt asset, sibling groups)

Used **only** when no single source has enough *planable* liquidity for the needed
flash amount. Cap: **3 sources**. Never nest callbacks.

**Planable liquidity** for source *i* on asset *A*:

```
planable_i(A) = floor( available_i(A) · 99 / 100 )   // 1% safety buffer
```

`available_i` is GUIDE 07's per-source formula. The 99% buffer is for multi-source
splitting; it is separate from any global haircut used for eligibility.

**Cascade algorithm** (encoder / `liq-flash`, not the contract):

1. Sort remaining sources by effective total cost ascending (GUIDE 07 Step 6).
2. Let `need` = required flash principal for this debt asset.
3. For up to 3 sources, while `need > 0`:
   - `take = min(planable_i(A), need)` — always pull as much as planable from the
     cheapest remaining source before touching the next.
   - Emit one `FlashGroup` with that `provider` / `flashSource` / `debtAsset` /
     `flashAmount = take` (plus over-borrow headroom within that source if desired).
   - Assign liquidation legs (or split a single position's `repayAmount`) whose
     repay sums to what this group funds. Same borrower + collateral across sibling
     groups is allowed — each group is a partial repay of the same position.
   - `need -= take`.
4. If `need > 0` after 3 sources (or after exhausting sources): the size is
   unfundable at this notional — shrink to what is fundable or decline.

**Wire shape:** sequential sibling groups with the **same `debtAsset`**, different
`provider`/`flashSource`. Repay swaps stay scoped per group (invariant 3), so
repeated debt assets are unambiguous. Profit swaps still run once globally after
every group has settled.

**Executor capability:** current `Executor.sol` already supports this. `T_GROUP` /
`T_EXPECTED_CALLER` are per-group and reset between iterations; `_initiate` is
synchronous through the provider callback. Nested multi-pool (GUIDE 07's old Step 7
diagram) is **rejected** — do not implement it.

**Immutability note:** no Executor change is required for multi-source cascade.
Provider id `4` is **Sky DSS Flash** (reclaimed from the Balancer reservation —
docs-only stage; nothing on-chain depended on revert-on-4). Balancer remains out
of scope and has no provider id.

#### Liquidation legs (77 bytes each)

```
  1 byte    adapter          0 AaveV3 · 1 AaveV4 · … (GUIDE 15 assigns)
  20 bytes  market           Aave Pool · V4 Spoke · Morpho market
  20 bytes  borrower
  20 bytes  collateralAsset
  16 bytes  repayAmount      u128 — what we ask the protocol to take
```

`adapter` is per-leg, so one group may span protocols — Alice on Aave V3 and Bob
on Aave V4, both owing USDC, is one group.

### 1c. Swap legs

**Not** identical shape in both positions. The **profit** blob carries a leading
`legCount` byte; the **repay** blob does not — its count lives in the group head
(`repaySwapCount`). Getting this wrong is silent: the decoder reads the first
leg's `venue` byte as a count and performs zero swaps. Verified by EVM execution;
see the round-trip suite. Otherwise identical shape in both positions — inside a group (repay) and at the end
(profit). The off-chain water-fill (GUIDE 12) decides allocations **jointly across
collaterals**, since paths out of different collaterals share output pools.

```
1 byte    legCount
per leg, head = 60 bytes:
  1 byte    venue      0 = UniV3 pool-direct · 1 = allowlisted router
  20 bytes  tokenIn    which collateral this leg spends
  20 bytes  tokenOut   where it goes
  1 byte    legFlags   bit0 TAKE_BALANCE · bit1 EXACT_OUT
  16 bytes  amount     u128 — exact output when EXACT_OUT, else input;
                       ignored when TAKE_BALANCE is set
  2 bytes   dataLen    u16
  N bytes   data       venue-specific
```

**`tokenOut` is encoded, not derived.** An earlier version inferred it from the
flag — debt asset for `EXACT_OUT`, WETH otherwise — which worked while a plan had
exactly one debt asset. It no longer does. 20 bytes a leg is a few hundred gas; a
leg that guesses its own output is a silent misliquidation.

**Repay legs are `EXACT_OUT` into the group's debt asset.** Sized to what is owed,
consuming whatever collateral that takes. Exact-input would force you to
over-provision and strand debt-token dust, or under-provision and fail the repay.
V3 encodes the mode in the sign of `amountSpecified`, so it is one call site.

**Profit legs converge on WETH.** The bid must be paid in ETH regardless — builders
value a bundle by `coinbaseDiff`, the coinbase's *native* balance delta, so an
ERC-20 sent there is worth zero to them. At a high `bidBps` the bid is nearly all
of net, so the conversion happens either way; routing the remainder with it buys
one numeraire, a guard in the same unit as gas, and an executor that holds exactly
one asset between transactions.

It is often *cheaper*, not dearer: most collateral routes to a stablecoin through
ETH, so stopping at ETH saves a hop. The case that genuinely costs is stablecoin
collateral against stablecoin debt — see `PROFIT_IN_DEBT` in §3.

**`TAKE_BALANCE` replaces positional remainder logic.** Set it on the last leg per
collateral and that leg spends the whole balance of that token. Strictly better
than "the last leg takes what's left": it cannot disagree with what is actually
held, so it absorbs solver rounding *and* an under-delivering leg *and* a
liquidation leg that was skipped entirely.

**Order is load-bearing within a blob.** `EXACT_OUT` legs consume an unknown amount
of collateral, so any `TAKE_BALANCE` leg must run after them. The encoder emits
them in that order and asserts it (§2).

A leg whose `tokenIn` balance is zero is skipped silently — the normal consequence
of a liquidation leg being beaten, not an error.

Venue data:

| Venue | `data` |
|---|---|
| `0` UniV3 pool-direct | 20 bytes: pool address. Settled in `uniswapV3SwapCallback` — no approval on this path. |
| `1` Allowlisted router | 20 bytes target (must equal `ROUTER_A` or `ROUTER_B`) + the router's own calldata. Exact approval, zeroed after. |

**Uniswap V4 is not a swap venue.** Hooks make swap behaviour pool-specific, so
there is no generic quote (GUIDE 12 Step 3). V4 remains the preferred *flashloan
source* — a different field and a separate decision.

**Why u128 for amounts.** 2^128 ≈ 3.4e38, or 3.4e20 tokens at 18 decimals. No
realistic liquidation approaches it, and it saves 16 bytes per field against u256.
Big-endian, matching EVM word order.

**Why packed, not ABI-encoded.** ABI encoding pads every field to 32 bytes and the
padding scales with leg count — a three-group batch pays it three times over.

**How much this actually matters.** Less than it used to. EIP-7623 raised calldata
costs, but its floor binds only on transactions that are calldata-heavy *relative
to their computation*. A liquidation burns hundreds of thousands of gas of EVM
work, so it stays on the standard 4/16 schedule. Packing is worth doing;
hand-writing fragile assembly beyond the straightforward decoder is not.

## 2. Rust encoder

```rust
// crates/liq-plan/src/lib.rs  (own crate, D51 — built in WP 10B, before liq-exec exists;
//                              liq-exec and liq-router depend on it, it depends on liq-types only)
pub const HEADER_LEN:        usize = 35;
pub const GROUP_HEAD_LEN:    usize = 59;
pub const LIQ_LEG_LEN:       usize = 77;
pub const SWAP_LEG_HEAD_LEN: usize = 60;

pub struct BatchPlan {
    pub flags: PlanFlags,
    pub bid_bps: u16,
    pub gas_cost_wei: U256,
    pub min_profit_wei: U256,
    pub groups: Vec<FlashGroup>,
    pub profit_swaps: Vec<SwapLeg>,   // everything left -> WETH
}

pub struct FlashGroup {
    pub provider: ProviderId,
    pub flash_source: Address,
    pub debt_asset: Address,
    pub flash_amount: U256,           // >= sum of repays, deliberately
    pub liqs: Vec<LiqLeg>,
    pub repay_swaps: Vec<SwapLeg>,    // all EXACT_OUT into debt_asset
}

pub struct SwapLeg {
    pub venue: SwapVenue,
    pub token_in: Address,
    pub token_out: Address,           // encoded, never inferred
    pub leg_flags: LegFlags,          // TAKE_BALANCE | EXACT_OUT
    pub amount: U256,
    pub data: Vec<u8>,
}

impl EncodedPlan {
    pub fn encode(p: &BatchPlan) -> Result<Self, EncodeError> {
        if p.groups.is_empty() { return Err(EncodeError::NoGroups); }
        p.validate()?;                                    // §2a

        let mut b = Vec::with_capacity(p.size_hint());
        b.push(p.flags.bits());
        b.extend_from_slice(&p.bid_bps.to_be_bytes());
        b.extend_from_slice(&u128_be(p.gas_cost_wei)?);
        b.extend_from_slice(&u128_be(p.min_profit_wei)?);
        debug_assert_eq!(b.len(), HEADER_LEN);

        b.push(u8::try_from(p.groups.len()).map_err(|_| EncodeError::TooManyGroups)?);
        for g in &p.groups {
            b.push(g.provider as u8);
            b.extend_from_slice(g.flash_source.as_slice());
            b.extend_from_slice(g.debt_asset.as_slice());
            b.extend_from_slice(&u128_be(g.flash_amount)?);
            b.push(u8::try_from(g.liqs.len()).map_err(|_| EncodeError::TooManyLegs)?);
            b.push(u8::try_from(g.repay_swaps.len()).map_err(|_| EncodeError::TooManyLegs)?);
            for l in &g.liqs { encode_liq_leg(&mut b, l)?; }
            encode_swap_legs_body(&mut b, &g.repay_swaps)?;   // no count prefix
        }

        encode_swap_legs(&mut b, &p.profit_swaps)?;           // count prefix
        Ok(Self(b))
    }
}

/// U256 -> big-endian u128. Returns an error rather than truncating: a silent
/// truncation would produce a valid-looking plan that liquidates the wrong
/// amount, and that failure is invisible until you read the receipt.
fn u128_be(v: U256) -> Result<[u8; 16], EncodeError> {
    let n: u128 = v.try_into().map_err(|_| EncodeError::AmountTooLarge(v))?;
    Ok(n.to_be_bytes())
}
```

`RUST-CONVENTIONS.md` §2 forbids `unwrap` here for exactly that reason.

The outer transaction calldata is the standard ABI encoding of `execute(bytes)`
wrapping this blob — `alloy::sol!` for that part; only the inner blob is packed.

### 2a. Invariants the encoder must assert

The contract cannot see these, so the encoder is the only place they can be
caught. Each has a failure mode that is silent rather than loud.

```rust
impl BatchPlan {
    fn validate(&self) -> Result<(), EncodeError> {
        // 1. Every collateral is closed exactly once by a TAKE_BALANCE leg.
        //    Zero strands it until the next sweep; two makes the second a
        //    silent no-op against an empty balance.
        for g in &self.groups {
            for l in &g.liqs {
                let closers = self.all_swaps()
                    .filter(|s| s.token_in == l.collateral_asset
                             && s.leg_flags.contains(LegFlags::TAKE_BALANCE))
                    .count();
                if closers != 1 {
                    return Err(EncodeError::BadCollateralClosure {
                        collateral: l.collateral_asset, closers,
                    });
                }
            }
        }

        // 2. Within each blob, EXACT_OUT legs precede TAKE_BALANCE legs. An
        //    EXACT_OUT leg consumes an unknown amount of collateral, so a
        //    balance sweep before it would take collateral the repay needs.
        for blob in self.all_blobs() { blob.assert_exact_out_first()?; }

        // 3. Every repay swap targets its own group's debt asset. A leg
        //    pointed at the wrong group's asset leaves one group short and
        //    another with a surplus it will sweep as profit.
        for g in &self.groups {
            if g.repay_swaps.iter().any(|s| s.token_out != g.debt_asset) {
                return Err(EncodeError::RepayTargetMismatch { group: g.debt_asset });
            }
        }

        // 4. Every profit swap targets WETH.
        if self.profit_swaps.iter().any(|s| s.token_out != WETH) {
            return Err(EncodeError::ProfitTargetNotWeth);
        }

        // 5. Same debt asset in multiple groups is allowed ONLY for the
        //    multi-source cascade (§1b′): ≤ 3 groups sharing a debtAsset,
        //    each a different provider/flashSource, repay swaps still
        //    targeting that group's own debtAsset (invariant 3).
        //    Two groups with the same debtAsset AND the same provider is
        //    always a solver bug — merge them.
        assert_multi_source_cascade_ok(&self.groups)?;
        Ok(())
    }
}
```


```rust
fn assert_multi_source_cascade_ok(groups: &[FlashGroup]) -> Result<(), EncodeError> {
    use std::collections::HashMap;
    let mut by_debt: HashMap<Address, Vec<&FlashGroup>> = HashMap::new();
    for g in groups {
        by_debt.entry(g.debt_asset).or_default().push(g);
    }
    for (debt, gs) in by_debt {
        if gs.len() == 1 { continue; }
        if gs.len() > 3 {
            return Err(EncodeError::TooManySourcesForDebt { debt, n: gs.len() });
        }
        let mut seen = std::collections::HashSet::new();
        for g in &gs {
            let key = (g.provider, g.flash_source);
            if !seen.insert(key) {
                return Err(EncodeError::DuplicateSourceForDebt { debt, provider: g.provider });
            }
        }
    }
    Ok(())
}
```

## 3. Flags

```rust
bitflags::bitflags! {
    pub struct PlanFlags: u8 {
        const SWEEP = 0b0000_0001;
    }
}

bitflags::bitflags! {
    pub struct LegFlags: u8 {
        const TAKE_BALANCE = 0b0000_0001;
        const EXACT_OUT    = 0b0000_0010;
    }
}
```

`SKIP_SWAP` and `RECEIVE_ATOKEN` are gone. Skipping is expressed by emitting zero
legs, and aTokens are never taken — profit has to converge on WETH, and a receipt
token cannot.

**`SWEEP` is decided off-chain, per plan.** The Rust side tracks the executor's
WETH balance (it sees every fill) and sets the flag when it crosses the threshold,
so the contract does no storage read for the decision. With ETH-only profit that
threshold is arithmetic rather than a risk budget — GUIDE 14 §5.

## 4. The round-trip test — mandatory

A layout mismatch is silent. Both sides must be tested against each other, not
just against themselves.

```rust
proptest! {
    #[test]
    fn plan_encoding_matches_solidity(plan in arb_batch_plan()) {
        let encoded = EncodedPlan::encode(&plan).expect("generator emits valid plans");

        let hdr: SolPlan = executor.debugDecodeHeader(encoded.as_bytes()).call()?;
        prop_assert_eq!(hdr.minProfit, plan.min_profit_wei);
        prop_assert_eq!(hdr.bidBps,    plan.bid_bps);

        // The decoder derives every offset by walking variable-length legs, so
        // the offsets ARE the thing most likely to be wrong. Assert them.
        for (gi, g) in plan.groups.iter().enumerate() {
            let sol: SolGroup = executor.debugDecodeGroup(encoded.as_bytes(), gi).call()?;
            prop_assert_eq!(sol.debtAsset,   g.debt_asset);
            prop_assert_eq!(sol.flashAmount, g.flash_amount);
            prop_assert_eq!(sol.liqCount,    g.liqs.len() as u8);

            for (li, l) in g.liqs.iter().enumerate() {
                let leg: SolLiqLeg = executor
                    .debugDecodeLiqLeg(encoded.as_bytes(), gi, li).call()?;
                prop_assert_eq!(leg.borrower, l.borrower);
                // … every field, no exceptions
            }
        }
    }
}
```

**The generator must vary group count, legs per group, repay-swap count, profit-
swap count and `data` length independently**, including the minimum of each. A
stride bug is correct for leg zero and wrong for every leg after it, and a walk
bug only appears when a variable-length leg precedes the thing you are reading.
Single-group, single-leg fixtures find neither.

Put `debugDecode*` behind a test-only build so they never reach mainnet, or expose
them from an `ExecutorHarness` that inherits.

## 5. Transaction alignment

How the plan reaches the chain, by trigger (GUIDE 13 routes this):

| Trigger | Venue | Shape |
|---|---|---|
| `SvrAuction` | MEV-Share bundle | `[{hash: oracleTxHash}, {tx: signed execute(plan)}]` — the oracle update referenced by hash, our call after it |
| `OraclePublic` | builder bundle | `[oracleTx, execute(plan)]` |
| `InterestDrift` | one-tx bundle, builder fan-out | no ordering requirement, but bundle semantics make a lost race free — GUIDE 13 §1 |
| `UserAction` / `PoolStateChange` | builder bundle | `[triggerTx, execute(plan)]` |

**The bundle exists for ordering, not for payment.** A backrun needs to land
immediately after a specific transaction in the same block; a standalone
transaction — however high its priority fee — can land before it (guard reverts),
after a competitor (guard reverts), or in a later block (position gone). Only
`InterestDrift` has no transaction to sit behind. It still ships as a
**one-transaction bundle** with builder fan-out — never a private transaction, and
never the public mempool (D19, GUIDE 13 §1). Having nothing to backrun changes the
bundle's contents, not its channel.

Never the public mempool, for any trigger. A liquidation visible in the public
pool is frontrun before it is mined.

---

## 6. Versioning

Byte 0 of a future revision may become a version tag if the layout changes. For
now the layout is fixed at v1 and both sides assert `HEADER_LEN == 35` at
compile time. If you change the layout:

1. Change both sides in the same commit
2. Re-run the §4 round-trip proptest
3. Redeploy the Executor — the old one keeps the old layout, and pointing new
   Rust at an old contract is exactly the silent failure this document exists to
   prevent
4. Update `config/venues.toml` with the new address before the binary ships
