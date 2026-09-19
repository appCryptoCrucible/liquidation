# GUIDE 01 — The Protocol Trait

| | |
|---|---|
| **Crate** | `liq-protocol` |
| **Prerequisites** | GUIDE 00 |
| **Est. effort** | 4–5 days (mostly design, little code) |
| **Blocks** | 02, 04, 07 |
| **Work package** | WP 01 (`WORK-PACKAGES.md`) |

## Objective

Define the single abstraction every lending protocol is expressed through. This
crate contains **no implementations and no dependency on `liq-adapters`**, and
**no dependency on `liq-state`** either: it defines the `StateWriter` trait,
`PositionRef`, `MarketRow` and `PositionExtraRepr` that `liq-state` implements
and lays out (D46; GUIDE 02 Steps 3–4 describe them). It is ~700 lines of types
and ~0 lines of logic, and it determines whether your fifteenth protocol takes
three days or three weeks.

With full-universe coverage as the goal (GUIDE 15), this trait carries more
weight than in a two-protocol system. Every hour spent here is repaid a dozen
times.

## Build vs. buy

100% build. There is no library for this.

---

## Step 1 — Internalize the variance you are abstracting over

The agent must be able to map each of these onto the trait before writing it.
These are real mainnet shapes, not hypotheticals:

| Protocol | Position key | Accrual | Health metric | Bonus | Close factor |
|---|---|---|---|---|---|
| Aave V4 | `(Spoke, user)` | shares + `drawnIndex`/`addExRate`, **plus per-user risk premium** | `eligible collateral / debt` | **variable, scales with HF** | **dynamic: repay to Target HF** |
| Aave V3 (3.5+) | `(Pool, user)` | scaled × `liquidityIndex` | HF ratio | static per reserve | 50% / 100% below 0.95 |
| Morpho Blue | `(marketId, user)` | shares + index | `debt / coll ≤ LLTV` | fixed incentive factor | derived from LIF |
| Compound V3 | `(Comet, user)` | index per base asset | `borrow > Σ coll·price·liqCF` | discount on `buyCollateral` | full absorb, then separate buy |
| Compound V2 forks | `(Comptroller, user)` | `borrowIndex` snapshot | shortfall | static | `closeFactorMantissa` |
| Euler V2 | `(controller, user)` + enabled collateral vaults | ERC-4626 shares | risk-adjusted, per controller | configurable | configurable |
| Liquity V2 / troves | `(trove_id)` | per-trove | ICR vs MCR | fixed | full or batch |
| Curve lending (LLAMMA) | `(controller, user)` | band-based | **soft liquidation — continuous rebalance** | n/a | n/a |

**Two rows break a naive trait.** Aave V4's variable-bonus / dynamic-close-factor
needs a *function*, not a constant. Curve's LLAMMA is not a liquidation
opportunity at all in the normal sense — model it as such explicitly rather than
discovering it after building an adapter (GUIDE 15).

Design for V4 first and most others fall out as degenerate cases.

## Step 2 — The core trait

```rust
pub trait Protocol: Send + Sync + 'static {
    fn id(&self) -> ProtocolId;

    // ---- ingest -------------------------------------------------------
    fn subscriptions(&self) -> Vec<LogFilter>;
    // `StateWriter` is a trait defined in THIS crate (D46); `liq-state`'s
    // `StateStore` implements it. That is what lets liq-protocol not depend
    // on liq-state. It is a `&mut` on one thread — no lock behind it.
    fn apply_log(&self, st: &mut dyn StateWriter, log: &DecodedLog) -> Result<DirtySet>;
    fn backfill(&self, st: &mut dyn StateWriter, src: &dyn Archive, to: BlockNum)
        -> Result<()>;

    // ---- health: pure, hot path, allocation-free ----------------------
    fn health(&self, pos: PositionRef<'_>, px: &PriceVector) -> Health;
    fn liquidation_price(&self, pos: PositionRef<'_>, px: &PriceVector,
                         asset: AssetId) -> Option<Price>;
    fn time_to_cross(&self, pos: PositionRef<'_>, px: &PriceVector)
        -> Option<Timestamp>;

    // ---- action -------------------------------------------------------
    /// Full economics at a given health. Called repeatedly by the timing
    /// optimizer (GUIDE 12), so it must be cheap and pure.
    fn quote(&self, pos: PositionRef<'_>, px: &PriceVector, cons: &Constraints)
        -> Option<Quote>;

    /// Turn a chosen quote + a chosen flash source into concrete calls.
    fn encode(&self, q: &Quote, funding: &FlashRoute, recipient: Address)
        -> Result<LiquidationPlan>;

    // ---- ground truth -------------------------------------------------
    fn health_probe(&self, pos: PositionRef<'_>) -> ProbeCall;
}
```

Note `encode` takes the flash route. With flashloan-only funding (GUIDE 07) the
calls are wrapped in a provider's callback, and the wrapping differs per
provider — so the adapter must be told which one before it can encode.

## Step 3 — `Health`, normalized

```rust
pub struct Health {
    /// Normalized so 1.0 is the boundary for EVERY protocol.
    pub hf: Ray,
    pub debt_value: Wad,
    pub collateral_value: Wad,
    pub price_sensitivity: AssetMask,
    pub state: HealthState,
}

pub enum HealthState {
    Healthy,
    Blocked { reason: BlockReason },       // paused, frozen, grace, dust
    Liquidatable,
    AuctionOpen { started: Timestamp },
    BadDebt { deficit: Wad },
    /// Curve LLAMMA and similar: the position rebalances continuously and
    /// is never "liquidatable". Modelled explicitly so GUIDE 15 can decline
    /// the protocol on purpose rather than by accident.
    SoftLiquidating,
}
```

Normalization is the highest-leverage decision in this crate. It is what lets the
band manager, threshold index, priority ordering and risk limits be written once
for fifteen protocols.

## Step 4 — `Quote`: economics as a function, not a number

```rust
pub struct Quote {
    pub position: PositionId,
    pub repay_asset: AssetId,
    pub seize_asset: AssetId,
    /// Maximum repayable RIGHT NOW. V3: close_factor × debt. V4: enough to
    /// restore Target HF, adjusted up if the remainder falls below dust.
    pub max_repay: U256,
    pub bonus: Ray,
    /// How bonus and max_repay evolve as health deteriorates.
    pub curve: BonusCurve,
    /// Which assets the protocol will accept as repayment. Usually one, but
    /// multi-debt positions offer a choice — and that choice interacts with
    /// flashloan availability (GUIDE 07).
    pub repay_options: SmallVec<[AssetId; 4]>,
    pub seize_options: SmallVec<[AssetId; 8]>,
}

pub enum BonusCurve {
    Static { bonus: Ray },
    /// Aave V4: piecewise-linear in HF, saturating at max_bonus.
    HealthLinear { bonus_at_threshold: Ray, hf_for_max: Ray, max_bonus: Ray },
    TimeDescending { start: Timestamp, curve: AuctionCurve },
}
```

**Do not collapse `BonusCurve` to a scalar.** A `Static` curve means fire
immediately; a `HealthLinear` curve means the optimal firing point is strictly
later than HF = 1.0, and finding it is worth real money on V4.

**`repay_options` is new and matters under flashloan-only funding.** A position
with debt in three assets gives you three possible repay legs, and only some of
them will be flashloanable at depth. The eligibility filter (GUIDE 07) needs the
full option set to pick a viable one — a `Quote` that names a single repay asset
throws away liquidatable positions.

## Step 5 — Funding is external to the protocol

Under flashloan-only operation, funding is **not** a protocol concern. The trait
exposes what must be repaid and what can be seized; GUIDE 07 decides where the
capital comes from. Keep them separate:

```rust
/// Produced by liq-flash (GUIDE 07), consumed by Protocol::encode.
pub struct FlashRoute {
    pub provider: FlashProvider,
    pub asset: AssetId,
    pub amount: U256,
    pub fee_bps: u16,          // 0 for Uniswap V4 / Morpho / Sky DSS (today)
    pub callback: CallbackShape,
}
```

The alternative — letting each adapter own its funding — means reimplementing
provider selection fifteen times and getting it inconsistently wrong. Resist it.

## Step 6 — `DirtySet`: precision that buys two orders of magnitude

```rust
pub enum DirtySet {
    None,
    Positions(SmallVec<[PositionId; 4]>),
    /// Index/rate moved. Re-PROJECT, do not re-fold.
    MarketAccrual(MarketId),
    /// Risk parameters changed. Full recompute + threshold re-derivation.
    MarketReprice(MarketId),
    ProtocolWide,
}
```

With fifteen protocols tracked, an adapter that returns `MarketReprice` for a
routine rate update does not just slow itself down — it starves every other
protocol's recompute budget. Enforce minimality in the conformance suite.

## Step 7 — `PositionRef` and the storage contract

```rust
pub struct PositionRef<'a> {
    pub id: PositionId,
    pub config: AssetMask,
    pub supply: &'a [u128],
    pub debt: &'a [u128],
    pub extra: &'a PositionExtra,     // fixed-size union, never boxed
    pub markets: &'a [MarketRow],
}
```

Size `PositionExtra` from the worst protocol. With full-universe coverage that
worst case is larger than a two-protocol system — audit it once all families are
scoped (GUIDE 15) rather than growing it incrementally.

## Step 8 — Conformance suite, written before any adapter

Put it in `liq-protocol/tests/conformance.rs` as a generic harness that every
adapter must pass:

```rust
pub fn conformance<P: Protocol>(p: &P, fixtures: &[PositionFixture]) {
    // 1.  health() is pure and allocation-free
    // 2.  health() is monotone in every collateral price
    // 3.  liquidation_price() round-trips to hf == 1.0 within 1 ulp
    // 4.  quote().max_repay applied yields the protocol's post-state
    // 5.  BonusCurve evaluated at current hf equals quote().bonus
    // 6.  apply_log() then undo restores byte-identical state
    // 7.  DirtySet is minimal — no ProtocolWide for routine accrual
    // 8.  repay_options and seize_options are complete and ordered by
    //     protocol preference, not by storage order
    // 9.  encode() produces calls valid under EVERY CallbackShape
    // 10. SoftLiquidating protocols never emit a Liquidatable quote
}
```

Criteria 3, 9 and 10 catch most adapter bugs and are free once written.

---

## Acceptance criteria

- [ ] `liq-protocol` compiles with zero dependencies on any adapter crate
- [ ] `health()` allocation-free (test allocator panics on allocation)
- [ ] `Quote` expresses all eight protocol shapes in Step 1 without an
      adapter-specific escape hatch
- [ ] `BonusCurve::HealthLinear` at `hf_for_max` equals `max_bonus`; at 1.0
      equals `bonus_at_threshold`
- [ ] `repay_options` populated for multi-debt positions; a test proves a
      three-debt-asset position yields three options
- [ ] Funding appears nowhere in the trait except as an `encode()` parameter
- [ ] `encode()` compiles against every `CallbackShape` variant (GUIDE 07 — five today;
      the count is not a constant, so assert exhaustively with a `match`, not a number)
- [ ] Conformance harness exists and passes against a mock adapter
- [ ] `HealthState::SoftLiquidating` exists and is documented as "decline, don't
      adapt"
- [ ] GUIDE 00 dependency lint still passes

## Failure modes

| Symptom | Cause |
|---|---|
| Adding protocol #N requires changing `liq-engine` | Trait leaked a protocol assumption. Stop and fix the trait — this is the signal the design exists to give you. |
| Liquidatable positions skipped | `Quote` names one repay asset; the flashloanable alternative was never surfaced |
| V4 liquidations under-earn | `BonusCurve` collapsed to a scalar |
| Provider selection logic duplicated per adapter | Funding pulled into the trait instead of staying in GUIDE 07 |
| Latency degrades as protocols are added | Adapters over-reporting `DirtySet` |
| Months wasted on a Curve adapter | `SoftLiquidating` not modelled; mechanism not read before coding |

## Handoff

GUIDE 02 builds the store `PositionRef` views into. GUIDE 07 builds the funding
layer `encode()` consumes. Settle `PositionExtra`'s size and the
`CallbackShape` enum before either starts.
