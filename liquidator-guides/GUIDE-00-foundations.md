# GUIDE 00 — Workspace & Foundations

| | |
|---|---|
| **Crates** | `liq-types`, `liq-config`, `liq-obs` |
| **Prerequisites** | none (00D needs the verified registry, C2) |
| **Work packages** | **00A** workspace + lints + CI (T3); **00B** fixed-point (T1); **00C** identities + shared types (D46); **00D** `liq-config` + registry boot assertion |
| **Est. effort** | 2–3 days |
| **Blocks** | 01, 02, 03, 06, 07 directly; every guide transitively |

## Objective

Stand up the Cargo workspace, the numeric types that every later layer depends
on, the config registry, and the tracing skeleton. Nothing here is interesting,
and every mistake here is expensive to undo later — particularly rounding
semantics and ID shapes.

## Build vs. buy

Buy `alloy-primitives` for `U256`/`Address`/`B256`, `serde`+`toml`+`figment` for
config, `tracing`+`metrics` for telemetry. Build the fixed-point newtypes, the ID
types, and the config validation. See `DEPENDENCIES.md` §1.

---

## Step 1 — Workspace skeleton

Create the workspace with all crates stubbed, so the dependency lint (Step 5) is
enforceable from commit one.

```
liquidator/
├── Cargo.toml                 # [workspace] members = [...]
├── rust-toolchain.toml        # pin: 1.94.1 or later (alloy 2.4.2 MSRV)
├── deny.toml                  # cargo-deny: licenses, advisories, bans
├── .github/workflows/ci.yml
└── crates/
    ├── liq-types/  liq-config/  liq-obs/
    ├── liq-protocol/  liq-state/  liq-node/
    ├── liq-adapters/  liq-oracle/  liq-engine/
    ├── liq-flash/  liq-router/  liq-sim/  liq-plan/  liq-exec/
    ├── liq-risk/  liq-watch/  liq-replay/  liq-books/  liq-bot/
```

Eighteen crates (D51). `liq-plan` holds the `BatchPlan` encoder so the Executor's
round-trip test exists before `liq-exec` does; `liq-watch` holds the
liquidation-event decoder and watcher so it can share nothing with the engine;
`liq-books` is the accounting scanner D42 requires to be independent of the
searcher. The full crate map with type homes is `WORK-PACKAGES.md` §0.

`liq-types` also hosts the cross-cutting types other guides *describe*
(D46): `PriceVector` (GUIDE 06), `Band` (GUIDE 08), `HaltScope`/`HaltReason`/
`HaltSink` (GUIDE 14), `LogFilter`/`LogSubscriber` (GUIDE 03), `Submitter`/
`IntendedSubmission` (GUIDE 13), `TraceId`. They are defined here in WP 00C as
types with constructors and no logic, so every later crate depends downward.

Workspace `Cargo.toml` uses `[workspace.dependencies]` with **exact** pins (see
`DEPENDENCIES.md` §3). Every member crate refers to `{ workspace = true }`.

## Step 2 — `liq-types`: fixed-point math

This is the highest-consequence code in the guide. Protocol health factors are
decided in the last wei at HF ≈ 1.0000, and a rounding direction that disagrees
with the protocol produces a bot that is confidently wrong exactly when it
matters.

Implement newtypes over `alloy_primitives::U256`:

```rust
/// 1e27 fixed point. Aave's RAY. Arithmetic width: intermediates need U512.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ray(U256);
/// 1e18 fixed point.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Wad(U256);
/// Storage width for values the chain itself holds as uint128 RAY (Aave
/// liquidityIndex, variableBorrowIndex, rates). Used in `MarketRow` (GUIDE 02)
/// so a row fits two cache lines; widened to `Ray` at the arithmetic boundary,
/// never stored wider than the chain stores it.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RayU128(u128);
impl From<RayU128> for Ray { /* lossless */ }
impl TryFrom<Ray> for RayU128 { /* Err on overflow — the chain cannot produce it, so it is a bug */ }
```

Rules the implementation must follow:

1. **Every operation that can lose precision names its rounding direction.**
   There is no `mul` — there is `mul_down` and `mul_up`. Same for `div`. Make the
   unrounded version impossible to call.
2. Provide `ray_mul_down`, `ray_mul_up`, `ray_div_down`, `ray_div_up`,
   `wad_*` equivalents, and `Ray <-> Wad` conversions that also name a direction.
3. All arithmetic is checked. A silent overflow in health math is a wrong answer,
   not a crash. Return `Result` or saturate explicitly — never wrap.
4. Provide `mul_div(a, b, denom, rounding)` as the primitive, implemented with a
   512-bit intermediate. Everything else is a thin wrapper.

### Rounding direction: match, do not be conservative

Every lending protocol rounds in its own favour. That is the right mental model
for *reading* the contracts, but it is not the rule your implementation follows.
Two refinements matter:

**The numeric direction flips by operation.** "Protocol-favouring" is not
"always round up":

| Operation | Protocol-favouring direction |
|---|---|
| Borrow → debt shares minted | **up** (you owe more) |
| Repay → debt shares burned | **down** (less debt cleared per token) |
| Supply → supply shares minted | **down** (you get fewer) |
| Withdraw → supply shares burned | **up** (you burn more) |

So you derive the direction per call site. A blanket rule produces a
reimplementation that is wrong in half the places.

**Your requirement is matching, not conservatism.** This is the part that catches
people. If you round *more* conservatively than the protocol you compute a lower
health factor than reality, fire early, and the on-chain guard reverts — a wasted
submission. If *less*, you compute liquidatable when it is not. Both are wrong.
The goal is bit-identical agreement with the deployed contract, including
anywhere the protocol's own rounding is inconsistent.

And it will be inconsistent somewhere. Aave V3.5 shipped "improved rounding
methods and internal scaled accounting" as a deliberate revision, which means
earlier versions rounded in ways someone later judged wrong. Where a contract used
plain Solidity integer division rather than an explicit `mulDiv` with a rounding
flag, the direction is *toward zero by accident*, not by design, and it may not
favour the protocol at all. Those cases cannot be derived from principle — you
read them out of the source (GUIDE 04 Step 1).

**Reasoning gets you a candidate; the differential fuzz certifies it.** GUIDE 05
Step 6 fuzzes your `health()` against the protocol's own view function biased to
HF ∈ [0.995, 1.005]. That, not the reasoning above, is what proves you matched.
Treat any mismatch as a rounding bug until shown otherwise — it almost always is.

Property tests (proptest) that must pass:

- `mul_down(a,b) <= mul_up(a,b)` and they differ by at most 1 unit
- `ray_div_down(ray_mul_up(x, y), y) >= x` and
  `ray_div_up(ray_mul_down(x, y), y) <= x` for nonzero `y`. The directions are
  not interchangeable: `mul_down` then `div_up` is `ceil(floor(xy/R)·R/y)`, and
  since `floor(xy/R)·R/y ≤ x` with `x` an integer, the ceiling is **≤ x** — the
  first non-trivial input (`x = 1, y = 1`) gives `0`. A property written the
  other way round fails immediately and invites "fixing" the math to pass it.
- `mul_down(x, y) == x·y / R` exactly, checked against `U512` arithmetic on
  random inputs (the fixed-point op must not lose the high word)
- Round-trips through `Wad -> Ray -> Wad` never increase the value with `_down`
- No input in the full `U256` range panics; overflow returns `Err`, not a
  wrapped value

## Step 3 — `liq-types`: identities

```rust
pub struct ChainId(pub u64);                 // 1, always, in this project
pub struct ProtocolId(pub u16);              // aave-v4, aave-v3, morpho-blue, …
pub struct MarketId(pub u32);                // spoke / pool / market, protocol-scoped
pub struct AssetId(pub u16);                 // GLOBAL, not protocol-scoped — see below
pub struct PositionId(pub u32);              // dense interned index

/// The full natural key. Interned to PositionId once, then never used on the
/// hot path.
pub struct PositionKey {
    pub protocol: ProtocolId,
    pub market: MarketId,      // Aave V4 Spoke, Morpho market id, Aave V3 pool
    pub user: Address,
}
```

**`AssetId` is global, not per-protocol.** WETH is one `AssetId` across every
adapter. This is what lets the threshold index (GUIDE 08) keep one sorted array
per asset spanning all protocols, and lets one price tick fan out correctly. Get
this wrong and you will rewrite the engine.

`MarketId` is protocol-scoped and must accommodate Aave V4's `(Hub, Spoke)`
shape, Morpho Blue's market id, and Aave V3's single pool. Model it as a dense
`u32` with a side table mapping to the protocol's own identifier.

## Step 4 — `liq-config`

Config is a correctness surface. Three requirements:

1. **Everything protocol-specific is data.** Addresses, deployment blocks,
   decimals, feed mappings, risk limits. Adding a fork of a protocol you already
   support must be a TOML file, not a crate.
2. **A `validate()` trait every config section implements**, called at startup,
   which checks the config against the chain. GUIDE 04 fills this in; define the
   trait now so adapters have somewhere to put it.
3. **The whole loaded config hashes to a `ConfigVersion`** that is logged with
   every trace and every submission. When you diagnose a bad health factor from
   three weeks ago you need to know exactly what was live.

```rust
pub trait Validate {
    /// Called at startup with a provider. MUST fail closed.
    async fn validate(&self, p: &dyn Provider) -> Result<(), ConfigError>;
}
```

Layout: `config/protocols/*.toml`, `config/feeds/*.toml`, `config/risk.toml`,
`config/venues.toml`. Load with `figment` so env vars can override secrets
without them ever appearing in a file.

## Step 5 — `liq-obs` skeleton and the dependency lint

Minimal now, expanded in GUIDE 09. What must exist today — **in `liq-types`, not
in `liq-obs`** (D46), because every hot-path crate carries these and none of them
may depend on `liq-obs`:

```rust
/// Created at the triggering input, carried through every stage.
#[derive(Copy, Clone)]
pub struct TraceId(u64);

/// Every stage boundary calls this. It emits a `tracing` event; GUIDE 09's
/// subscriber (in liq-obs) turns them into histograms. The hot path never
/// links liq-obs.
pub fn stage(trace: TraceId, stage: Stage);
```

Thread `TraceId` through function signatures from the start. Retrofitting it
after the engine exists means touching every function in the hot path.

**The dependency lint.** Add a CI step that fails the build on any forbidden edge
in the crate graph. Assert the **full** list, not just the famous one — every
line below is a cycle or a hot-path contamination that an agent has a plausible
reason to introduce:

```bash
# forbid.txt: one "from to" pair per line. All are transitive.
cat > forbid.txt <<'EOF'
liq-engine   liq-adapters
liq-engine   liq-oracle
liq-engine   liq-obs
liq-engine   liq-router
liq-engine   liq-exec
liq-protocol liq-state
liq-protocol liq-adapters
liq-types    liq-protocol
liq-obs      liq-engine
liq-obs      liq-state
liq-obs      liq-router
liq-flash    liq-router
liq-state    liq-node
liq-plan     liq-exec
liq-watch    liq-state
liq-watch    liq-engine
liq-books    liq-exec
liq-books    liq-risk
liq-books    liq-state
EOF
while read -r from to; do
  if cargo tree -p "$from" -e normal --prefix none | awk '{print $1}' | grep -qx "$to"; then
    echo "forbidden dependency: $from -> $to"; exit 1
  fi
done < forbid.txt

# hot-path crates carry no async runtime in their normal dependency tree
for c in liq-types liq-protocol liq-state liq-engine liq-flash liq-plan; do
  if cargo tree -p "$c" -e normal --prefix none | awk '{print $1}' | grep -qx tokio; then
    echo "$c must not depend on tokio"; exit 1
  fi
done
```

`liq-types` sits at the bottom, so `liq-types → anything-in-workspace` is
forbidden by construction: the first line of the loop for `liq-types` may list
every other crate. The CI step must also **prove it works**: a test job adds
`liq-adapters` to `liq-engine`'s `Cargo.toml` on a scratch branch and asserts the
step goes red. A lint that has never failed has never been tested.

This lint is the mechanical enforcement of the whole multi-protocol design *and*
of the acyclic crate graph in `WORK-PACKAGES.md` §0. Add it before there is
anything to lint, so it can never be "temporarily" violated.

## Step 5b — Lints, profile and the Rust conventions

`RUST-CONVENTIONS.md` is the canonical reference. This step makes it mechanical
so no crate can silently opt out.

**Workspace lints**, set once and inherited by every member via
`[lints] workspace = true`:

```toml
[workspace.lints.rust]
unsafe_code = "forbid"
unreachable_pub = "warn"

[workspace.lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
indexing_slicing = "deny"
arithmetic_side_effects = "deny"   # D53 — `integer_arithmetic` is the deprecated old name; -D warnings would fail on it
float_arithmetic = "deny"          # no floats in money math, ever
await_holding_lock = "deny"        # the one that prevents a real outage
mutex_atomic = "warn"
rc_buffer = "warn"
```

**Release profile:**

```toml
[profile.release]
overflow-checks = true    # yes, in release — cheap, catches wrong health factors
panic = "unwind"          # REQUIRED: catch_unwind cannot contain an abort
lto = "fat"
codegen-units = 1
debug = 1                 # keep symbols; you will profile this in production
```

`panic = "abort"` is forbidden. It would make the ExEx panic containment in
GUIDE 03 impossible, turning any adapter bug into an Ethereum node outage.

`overflow-checks = true` in release is deliberate. A silent wraparound in health
math produces a confidently wrong answer rather than a crash, which is the single
worst failure mode this system has.

**Error crates:** `thiserror` in every library crate, `eyre` only in `liq-bot` at
the top level. Hot-path error variants carry no `String` — use fieldless variants
or `&'static str` so an error path cannot allocate.

**Dependencies to add now** so later guides can reach for them without debate:
`crossbeam-channel`, `rtrb`, `triple_buffer`, `arc-swap`, `parking_lot`,
`smallvec`, `arrayvec`, `bumpalo`, `bytemuck`, `fixedbitset`, `thiserror`,
`tracing`, `metrics`, and `mimalloc` as the global allocator. Dev-dependencies:
`proptest`, `loom`, `criterion`.

## Step 6 — CI

`ci.yml` must run, on every commit: `cargo fmt --check`, `cargo clippy
-- -D warnings`, `cargo test --workspace`, `cargo deny check`, and the dependency
lint. Later guides add the replay harness and Foundry tests to this pipeline.

**Build on the production OS image.** Pick the distro now (GUIDE 16 Step 0 —
Ubuntu 26.04 LTS is the recommendation) and make CI build in a container of that
exact release. A binary compiled against a different glibc than production runs
is an avoidable class of incident, and it surfaces at the worst possible moment:
when you are deploying a fix under pressure. Pin the Rust toolchain in
`rust-toolchain.toml` so CI and the box agree on that too.

---

## Acceptance criteria

- [ ] `cargo build --workspace` succeeds with all 15 crates stubbed
- [ ] `Ray`/`Wad` have no un-rounded multiply or divide in their public API
- [ ] Proptest suite for fixed-point math passes with 10k cases, no panics —
      **with the generator biased to HF ∈ [0.995, 1.005]**. A fuzz test that
      passes on the first run against uniform inputs is testing the wrong
      region, not proving correctness (`TESTING.md` §3)
- [ ] `AssetId` is global; a test asserts the same `AssetId` resolves from two
      different protocol configs for WETH
- [ ] Config loads, hashes to a stable `ConfigVersion`, and `Validate` is wired
      into startup even though no adapter implements it yet
- [ ] **Registry boot assertion is wired before anything else starts.** Every
      token's `decimals()`/`symbol()` and every pool's `token0()`/`token1()`/
      `fee()` re-read from chain and asserted against the committed registry;
      any mismatch refuses to start, with no degraded mode. See `REGISTRY.md` §4
      — a decimals error is off by orders of magnitude and the `minProfit` guard
      inherits the same wrong scale, so nothing downstream will catch it.
- [ ] A deliberately corrupted registry entry fails startup in a test — flip one
      token's decimals and confirm the process refuses to run
- [ ] Dependency lint is in CI and demonstrably fails when the forbidden edge is
      added (add it, watch CI fail, revert)

## Failure modes

| Symptom | Cause |
|---|---|
| Health factor off by 1–2 wei vs chain | Un-rounded or wrong-direction fixed-point op. Caught in GUIDE 05 differential fuzz, expensive to find later. |
| Threshold index can't fan out one price to two protocols | `AssetId` was made protocol-scoped |
| Cannot reproduce a historical decision | `ConfigVersion` not logged with traces |
| Engine imports an adapter "just for now" | Dependency lint was not added on day one |

## Handoff

GUIDE 01 defines the `Protocol` trait using these types. Do not start it until
the fixed-point property tests pass — the trait's signatures bake in `Ray`/`Wad`
and changing them afterwards touches every adapter.
