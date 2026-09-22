# GUIDE 13 — Execution: MEV-Share & Builders

| | |
|---|---|
| **Crate** | `liq-exec` |
| **Prerequisites** | GUIDE 06 (Step 4 client), 10 (deployed — H3), 11, 12A, **14A** (`RiskGate.allow()`), the watcher (W), **and the GUIDE 09 shadow gate** |
| **Work packages** | **13A** execution (eligible only with the Shadow gate `passed`); **13B** staged rollout after H4 (needs Recall `passed` too). `Submitter`/`IntendedSubmission` are `liq-types` traits (D46); the encoder is `liq-plan` (D51) |
| **Est. effort** | 1.5–2 weeks |
| **Blocks** | 14, 15, 17 |

## Objective

Put a signed bundle on the wire, at the right venue, with the right bid, fast
enough — and know exactly what happened to it.

**Do not start this guide until shadow mode (GUIDE 09) shows > 80% of
opportunities with an intended-submission timestamp preceding the winner's
inclusion.** If that gate is not met, the problem is upstream and executing will
only lose money faster.

## Build vs. buy

Buy `alloy` for signing. Build the MEV-Share client (started in GUIDE 06), the
venue fan-out, the nonce pool and the inclusion watcher. No maintained Rust
MEV-Share client is worth a critical-path dependency.

---

## Step 1 — Venue abstraction

```rust
#[async_trait]
pub trait Submitter: Send + Sync {
    fn venue(&self) -> Venue;
    async fn submit(&self, b: &SignedBundle) -> Result<SubmitReceipt>;
}

pub enum Venue {
    /// THE path for SVR-protected oracle backruns (Aave on mainnet).
    /// Bundle references the hinted oracle tx by hash.
    MevShare { relay: Url },
    /// Direct to builders. For non-SVR triggers: interest drift, user
    /// actions, public oracle updates, pool-state changes.
    BuilderBundle { endpoint: Url, builder: BuilderId },
    // There is deliberately no third variant (D19, D54). A public-mempool
    // path is not "unreachable in config" — it does not exist in the type.
}
```

Route by `TriggerCause` (GUIDE 08 Step 5):

| `TriggerCause` | Venue | Notes |
|---|---|---|
| `SvrAuction` | `MevShare` | Reference the hint hash; bid via `refundConfig` |
| `OraclePublic` | `BuilderBundle` fan-out | Bundle behind the decoded transmit |
| `OraclePullHeld` | `BuilderBundle` fan-out | You hold the signed update; bundle `[update, liquidate]` |
| `InterestDrift` | **one-tx bundle**, builder fan-out | **No auction bid** — uncontested; modest priority fee |
| `DerivedRate` | `BuilderBundle` fan-out | Backrun the rate-update tx; rarely contested |
| `UserAction`, `PoolStateChange` | `BuilderBundle` fan-out | Backrun the triggering tx |
| `ParamChange` | `BuilderBundle` fan-out | Timelocked, so schedulable — but many positions fire at once; see Step 4 |
| `Stale` | **one-tx bundle**, builder fan-out | No race; modest priority fee |

Sending an `InterestDrift` candidate with an auction-sized bid is pure waste, and
it happens by default if you do not branch here.

**Bundles are for ordering, not for payment.** This distinction decides the whole
table above. A backrun must land *immediately after* a specific transaction in
the same block; a standalone transaction, however high its priority fee, can land
before it (your on-chain guard reverts), after a competitor took the position
(guard reverts), or in a later block (position gone). A bundle is the only way to
express "this transaction, right after that one, atomically or not at all."

So `InterestDrift` and `Stale` are the triggers with no ordering requirement:
nothing to sit behind, no competitor with information you lack — you computed the
crossing in closed form and simply need inclusion. A modest priority fee is
correct there, and paying auction-sized value would be burning money.

**But send them as one-transaction bundles, not as private transactions.**
Atomicity is a property of bundles, not of transactions: a lone tx has nothing to
be atomic *with*, so there is no revert invariant for a builder to enforce, and
including your reverting transaction still earns them the priority fee. That is
why revert protection is a product someone sells rather than a default you get.

A bundle containing one transaction gives you default atomic semantics — "any
revert invalidates the entire bundle" — with no ordering constraint on anything
else. Nothing is given up, and one thing is gained: a bundle targets a specific
block, whereas a private transaction stays valid for many blocks and can land at
a price you computed several oracle updates ago. Per-block resubmission against
fresh state is the behaviour you want anyway; the bundle makes it structural
rather than something you have to remember.

Never list the liquidation leg in `revertingTxHashes` — that opts it out of
exactly the protection you came for.

**Losing a race is therefore free.** The builder simulates, sees the position was
already cleared earlier in the block, and drops the bundle. Nothing executes, no
gas is paid, and there is no on-chain trace of the attempt.

One consequence worth carrying into GUIDE 12: **the band's lower edge is not a
gas-waste threshold.** It answers "if I win, is this profitable after gas," not
"what does losing cost me" — because losing costs nothing. The naive version of
that calculation passes on opportunities that are free to attempt, which matters
most at the small end of the market.

What actually bounds how wide you go is **nonce slots and solver throughput**
(Step 4), not gas burn. That is also where the one genuinely absolute floor
lives, and it is an *opportunity-cost* floor rather than a profitability one:
with twenty slots and three opportunities, take all three however small; with
thirty opportunities, take the best twenty. It is zero when you have spare
capacity, so it tracks slot pressure rather than being a constant.

Treat "reverts are free" as a strong default rather than a guarantee — there is
at least one reported case of `eth_sendBundle` including a reverting transaction
despite atomic semantics. The on-chain `minProfit` guard stays regardless, and it
is doing different work: bundle atomicity protects you from losing the race, the
guard protects you from your own net calculation being wrong.

For **SVR-triggered Aave liquidations the auction is not optional** — it is the
access mechanism. The SVR price update is routed privately and the right to
backrun it is auctioned; the standard feed lags by the configured delay, so a
searcher who does not bid is not liquidating those positions at all, at any
priority fee.

**Priority fee vs. coinbase payment.** A builder orders by total value delivered
and is largely indifferent to the form. The difference that matters: a priority
fee is paid on all gas used even when the transaction reverts, while a
`block.coinbase` payment inside the executor can be made conditional on success.
Inside a bundle this is moot — reverting bundles are dropped — so use a priority
fee there for simplicity, and prefer conditional payment for standalone private
transactions that carry revert risk.

**Never the public mempool, for any trigger.** A liquidation visible in the
public pool is frontrun before it is mined. There is no `Venue` variant for it
(D54) and no code path that could reach it (D19) — an agent that adds one, even
behind a flag, has violated a settled decision.

### Your own node is not the submission path

Worth stating plainly, because colocating the node with the searcher invites the
opposite assumption. Every venue in that enum is an
**HTTPS POST to someone else's endpoint** — a relay or a builder. The signing
happens in your process; the bundle then leaves the box over the network. Your
node is not on that wire.

In fact `eth_sendRawTransaction` against your own node is *precisely* the thing
you never want: it gossips the transaction to your peers, which is the public
mempool by another name. The one route your node offers for submission is the one
route the table above forbids.

What the node actually buys — all of it real, and all of it upstream of
submission rather than part of it:

- **No RPC between the bot and the node.** Under the ExEx architecture (GUIDE 16
  Step 1) the bot is compiled into the node binary and reads state from the same
  process — no `eth_call` to localhost, no round trip, no serialization. This is
  better than "our own private RPC," not a version of it. Submission itself *is*
  JSON-RPC (`eth_sendBundle`, `mev_sendBundle`), but outbound to a third party,
  not to your node.
- **Simulation against exactly the state you are about to act on** (GUIDE 11),
  with no fork-point ambiguity and no provider latency between decision and check.
- **No rate limits and no queue** behind other customers.
- **No information leakage.** A third-party RPC provider sees every position you
  query. That query stream *is* your target list, your trigger model and your
  timing, handed to a company with no obligation to you. This is the argument for
  self-hosting that survives even if latency were free.

**Keep the RPC server on, and keep it bound to localhost.** It earns its place
for the GUIDE 05 extraction and for ad-hoc ops queries — neither of which is
latency-critical. It should not be reachable from the internet. "Private RPC"
here means *not exposed*, not "my own endpoint I can hit from anywhere"; an
exposed execution-client RPC on a box holding an operator key is a much larger
problem than any opportunity it would help you catch.

### Builders, and the two things called "relay"

You submit to **builders**, directly, at their own public endpoints —
beaverbuild, rsync-builder, Titan, payload, builder0x69, lokibuilder, nfactorial,
Eden, bloXroute, and others. They all publish them. There is no gatekeeper.

Two sources of confusion worth clearing once:

- **`relay.flashbots.net` is Flashbots' builder**, not a relay. The name predates
  the builder/relay split and never changed. Posting a bundle there is talking to
  one builder among many.
- **mev-boost relays** (`boost-relay.flashbots.net`, ultrasound, agnostic, …) sit
  between **builder and validator**, not between you and anyone. They exist as
  trust escrow: the relay holds the full block, hands the proposer only the
  header, and reveals the payload only after the proposer signs blind — so the
  proposer cannot unbundle and steal the builder's MEV. Searchers are not in that
  picture. "Submitting via relays" is not a thing you do.

So the fan-out is to builder endpoints. Prioritise by market share — a handful
win most blocks — then let measured inclusion-per-builder tell you who is worth
keeping a warm connection to. Curate rather than spray: every builder you send to
sees the bundle whether or not it lands, and builders have leaked in the past.

Note also that **you can be the best bid and still not land**, because the
builder that included you lost the slot. That is noise, not a bid problem, and
it will be a meaningful share of your non-landing submissions.

### The identity key — a third key

Relay requests carry `X-Flashbots-Signature: <address>:<signature>`, an EIP-191
hash of the JSON body signed with an ECDSA key. **That key is separate from every
transaction-signing key.** It never holds funds and never signs a transaction.
Its job is identity: the address accumulates your reputation with the relay over
time.

Two consequences. It belongs in key management (GUIDE 17 Step 5) alongside the
operator and profit-sink keys. And it is the one whose *continuity* matters most
— rotating it resets the reputation you have been building, so treat rotation as
a deliberate decision rather than hygiene.

**There is no fallback, and that is deliberate.** If every builder endpoint is
unreachable, the correct behaviour is to miss the opportunity. The public mempool
is technically reachable through your own node — **do not wire it, under any
condition, for any trigger.** A broadcast liquidation is usually taken by a
generalized backrunner before inclusion, so the choice is not "submit publicly or
miss it" but "miss it, or pay gas to hand it to someone else." It also forfeits
the free-revert property that the entire safety model rests on: a bundle that
loses its race costs nothing, a broadcast transaction that loses its race costs
gas.

An alert on total builder unreachability is the whole response. **Do not build
the fallback path**, including as a flag that defaults off — a disabled path is
one config mistake away from an enabled one.

## Step 2 — MEV-Share bundle construction

```rust
// mev_sendBundle -> https://relay.flashbots.net
{
  "inclusion": { "block": "0x...", "maxBlock": "0x..." },   // span 2-3 blocks
  "body": [
    { "hash": "0x<hinted oracle tx>" },                     // by hash
    { "tx": "0x<your signed liquidation>", "canRevert": false }
  ],
  "validity": { "refundConfig": [ { "address": "0x...", "percent": N } ] },
  "privacy": { "builders": [ ... ] }
}
```

Sign the request body with your searcher key and send the signature in the
Flashbots auth header. Set `maxBlock` to span two or three blocks — inclusion
odds improve and there is no downside for a guarded bundle.

Use `mev_simBundle` for matched bundles as a belt-and-braces check against your
own revm result during rollout. Drop it from the hot path once the two agree
consistently; it is a round-trip you cannot afford long term.

## Step 3 — Presigned templates

```rust
pub struct Template {
    unsigned: TxEnvelope,
    patch_points: SmallVec<[PatchPoint; 4]>,   // byte offsets for amounts
    signer: Arc<PrecomputedSigner>,            // secp256k1 precomputed tables
}
```

Signing with a precomputed context is ~30–60 µs; building calldata, ABI-encoding
and estimating gas from scratch is 10× that. For Hot-band positions, templates are
built and held **before** the trigger arrives — which under SVR is the entire
reason prediction (GUIDE 06) pays.

## Step 4 — Nonce pool

```rust
pub struct NonceAllocator { keys: Vec<KeyState> }
pub struct KeyState {
    next: u64,
    in_flight: BTreeMap<u64, InFlight>,
    gap_filler: GapFiller,      // self-transfer to fill a dropped-tx gap
}
```

**One key per concurrent opportunity slot.** Single-key operation serializes your
entire throughput behind one nonce sequence — and a cascade, when twenty positions
go underwater at once, is exactly when you make money and exactly when
serialization costs the most. Size the pool to your observed peak concurrency
from shadow data, then double it.

A `ParamChange` trigger can produce dozens of simultaneous candidates. Test this
case explicitly.

## Step 4b — Rust: concurrency & safety

This is the **async** crate, and the one place in the system where a lock is the
right primitive.

**Tokio runtime on non-isolated cores** (GUIDE 16). Submission is I/O-bound
network work; it must never be scheduled onto a pinned hot-path core.

**Fee fields come from the gas oracle, per submission** (GUIDE 12). `maxFeePerGas`
is set from the *exactly computed* base fee for the target block plus headroom
for the base-fee moves the inclusion window can see: `base_fee(n+1)` is exact,
and each further block compounds, so spanning `maxBlock = block + k` needs
`base_fee(n+1) · 1.125^k` (k = 1 → +12.5 %, k = 2 → +26.6 %, k = 3 → +42.4 %),
rounded up — not a flat 12.5 %. Under-headroom makes the bundle invalid in
exactly the later blocks you paid the span for. `maxPriorityFeePerGas` is
1 gwei on standalone sends and inside a bundle. Never reuse a base fee
computed for a previous block, and do not add the 1 gwei into `maxFeePerGas`.

**Hot path → executor: bounded MPSC, never a blocking call.** The engine hands
off a verified bundle and returns immediately. The hot thread never awaits, never
calls `block_on`, and never touches the Tokio runtime.

```rust
// Hot thread (sync side).
if tx.try_send(bundle).is_err() {
    metrics::counter!("submit_queue_full").increment(1);   // count, don't block
}
```

**The nonce allocator is a `parking_lot::Mutex`, and that is correct.**
Contention is one acquisition per submission, the critical section is two integer
operations and a map insert, and this is the async path, not the sub-millisecond
one. `RUST-CONVENTIONS.md` §4 is the reasoning; this is its canonical example.

```rust
let nonce = {
    let mut g = key.state.lock();
    let n = g.next;
    g.next += 1;
    g.in_flight.insert(n, InFlight::new());
    n
};                                   // guard dropped HERE, before any await
submit(bundle, nonce).await?;
```

The scope block is deliberate. **Never hold the guard across `.await`** —
`clippy::await_holding_lock` is denied workspace-wide and will catch it, and
`parking_lot`'s guards are `!Send`, so the compiler refuses independently. Two
mechanisms for the same rule, because this one causes real outages.

Do **not** reach for `tokio::sync::Mutex` here. It exists for holding a lock
across an await, which is precisely what you must not do.

**One `KeyState` per key, each independently locked.** Keys do not share a lock,
so concurrent submissions on different keys never contend. This is what makes the
cascade throughput in Step 4 achievable.

**Venue fan-out: `JoinSet`, not sequential awaits.**

```rust
let mut set = tokio::task::JoinSet::new();
for v in &self.venues { set.spawn(v.submit(bundle.clone())); }
while let Some(res) = set.join_next().await { record(res); }
```

Sequential awaits would serialize your submission across builders and lose the
block.

**No `unwrap` on any network result.** A relay timeout, a malformed response, a
dropped connection — all are expected states feeding the GUIDE 14 halts,
never panics. This crate runs inside the Reth process; a panic here is a node
outage.

**Inclusion watcher state** is owned by a single task and mutated through `&mut`.
Other components learn outcomes through a channel, not by sharing the map.

## Step 5 — Inclusion watcher

Every submission tracked to a terminal state:

```rust
pub enum Terminal {
    Included { profit: I256, gas: u64 },
    Dropped,                              // bundle not taken
    Reverted { reason: Bytes },
    LostToCompetitor { tx: TxHash, their_bid: Option<U256> },
}
```

`LostToCompetitor` is the important one. Resolve it by watching each protocol's
own liquidation events and matching on target position. You need the winner's
transaction to extract their bid and inclusion timestamp — that is the training
signal for GUIDE 12's bid model and the `Outbid` half of the GUIDE 09 taxonomy.

Without this, you cannot tell "I bid too low" from "I was too slow", and those
have opposite fixes.

## Step 6 — Rollout

Do not turn everything on at once.

1. **Interest-drift only, small size.** Uncontested; validates the entire
   execution path — contracts, signing, nonces, inclusion — without competing.
   First real PnL comes from here.
2. **Public-oracle triggers on one protocol.** Now you are racing, but on the
   simpler path.
3. **SVR auctions at the learning-phase bid** (GUIDE 12 Step 6: near the cap
   until `F(β)` has ≥ 2 weeks of data — WP 12B). Enable last. Watch the
   `Outbid` gap distribution and let the `F(β)` fit converge before raising size.
4. **Raise size gradually**, gated on realized-vs-simulated PnL agreement.

Each stage runs for at least a week with its own review.

---

## Acceptance criteria

- [ ] Shadow gate met before any live submission (GUIDE 09, > 80%)
- [ ] MEV-Share bundle validated on Sepolia end-to-end before mainnet
- [ ] `TriggerCause` correctly routes to venue; a test asserts `InterestDrift`
      never carries an auction-sized bid
- [ ] Sign + submit p99 < 500 µs (excluding network)
- [ ] Nonce pool sustains 20 concurrent submissions in a synthetic cascade with
      no serialization stall and no nonce gaps
- [ ] Gap filler recovers from a deliberately dropped transaction
- [ ] Every submission reaches a terminal state; zero unresolved after 5 blocks
- [ ] `clippy::await_holding_lock` is deny-level and the crate passes
- [ ] Nonce guard is dropped before any `.await` (enforced by `parking_lot`'s
      `!Send` guard; verify the crate does not use `tokio::sync::Mutex` here)
- [ ] Venue fan-out is concurrent (`JoinSet`), not sequential awaits
- [ ] No `.unwrap()` on any network result in this crate
- [ ] `LostToCompetitor` resolves the winner's tx and extracts their bid for
      ≥ 90% of losses
- [ ] First successful mainnet liquidation with positive realized PnL, from the
      interest-drift path
- [ ] Realized PnL within 5% of simulated PnL across the first 50 wins

## Failure modes

| Symptom | Cause |
|---|---|
| Bundles never included | `maxBlock` too tight, bid too low, or bundle malformed — check `mev_simBundle` first |
| Throughput collapses during cascades | Single key, or pool undersized |
| Cannot improve the bid model | `LostToCompetitor` not resolving winner bids |
| Paying auction bids on uncontested liquidations | No `TriggerCause` branch in venue routing |
| Realized PnL below simulated | Gas estimate instead of sim output; or the oracle update's own price impact not modelled in slippage |
| Reverts on chain | Guard logic (GUIDE 10) or stale state; on bundles this should cost nothing — if it costs, you sent it publicly |

## Handoff

GUIDE 14 wraps this in risk controls. Run at minimum size until those controls
exist — the failure modes that lose real money are not in this guide, they are in
the next one and in GUIDE 10.
