# GUIDE 06 — Oracle Layer, SVR & Price Prediction

| | |
|---|---|
| **Crate** | `liq-oracle` |
| **Prerequisites** | GUIDE 00, 03 for Steps 2–3 (feed registry, canonical prices — **04 needs these**); 04 for Steps 4–8 |
| **Work packages** | **06A-1** feed registry + canonical `PriceVector` (before 04A); **06A-2** derived pricing; **06B** MEV-Share client; **06C** CEX + aggregator sim + fusion; **06D** public-mempool transmit decoder. `PriceVector`/`SourceKind` are defined in `liq-types` (D46) |
| **Est. effort** | 2–3 weeks |
| **Blocks** | 08, 09, 13 |

## Objective

Know what the chain believes, what it is about to believe, and — for SVR feeds —
be positioned to bid the instant the private oracle update is announced.

**Read `DEPENDENCIES.md` §0.2 before starting.** Aave on mainnet uses Chainlink
SVR: the oracle update is routed privately through Flashbots MEV-Share and the
right to backrun it is auctioned. The public-mempool `transmit()` decoder does
not win those races. This guide builds both paths and is explicit about which
applies where.

## Build vs. buy

Buy `alloy` for on-chain reads. Build the MEV-Share client (thin: SSE reader +
one JSON-RPC method), the aggregator simulator, the derived-price adapters, and
the feed registry. No maintained Rust MEV-Share client is worth a critical-path
dependency.

---

## Step 1 — Source abstraction

Encode the strategy in the type. `SourceKind` is not metadata — the executor
branches on it.

```rust
pub enum SourceKind {
    /// What the chain believes now. Ground truth, zero lead.
    Canonical,
    /// Announced privately via MEV-Share; you bid for the right to backrun.
    /// THE path for Aave on mainnet.
    SvrAnnounced { hint: MevShareHint, deadline: Instant },
    /// Visible in the public mempool; bundle behind it. Non-SVR feeds.
    PendingPublic { tx: TxHash, confidence: Confidence },
    /// Inferred from the aggregator's own inputs before it publishes.
    /// Pre-warm only — never fire on this alone.
    Predicted { eta: Option<Instant>, confidence: Confidence },
    /// Computed from other prices + on-chain state (LST rates, LP fair value).
    Derived { deps: SmallVec<[AssetId; 4]> },
}
```

## Step 2 — Feed registry, validated at startup

Boring config that decides correctness. Per `(protocol, market, asset)`:

```toml
[[feed]]
protocol   = "aave-v4"
spoke      = "core-main"
asset      = "0x...WETH"
mechanism  = "chainlink-svr"        # or "chainlink-push"
svr_aggregator      = "0x..."       # what the protocol actually reads
standard_aggregator = "0x..."       # fallback path, still worth watching
sources    = ["binance:ETHUSDT", "okx:ETH-USDT", "coinbase:ETH-USD"]
deviation_bps  = 50       # ETH/USD mainnet: 0.5 %
heartbeat_secs = 3600     # ETH/USD mainnet: 1 h. Most USD majors are 3600; 86400 is
                          # the long-tail default. Copy each feed's values from
                          # its Chainlink feed page — never a shared default.
```

Both numbers gate the aggregator simulator (Step 6). A heartbeat 24× too long
suppresses every heartbeat-driven `PendingUpdate` on the highest-volume feed in
the system; the startup validator asserts them against the on-chain
`AccessControlledOffchainAggregator` config where it is exposed, and against the
observed transmit cadence in the replay archive otherwise.

**Startup validation, fail closed:** read the protocol's configured oracle for
every asset and assert it matches the registry. A protocol migrating a feed while
you are not looking silently poisons every health factor that touches it — this
is failure mode #3 in the system map and it is entirely preventable here.

## Step 3 — Canonical price layer

Read current aggregator answers via `alloy`, maintain them in a dense
`PriceVector` indexed by the global `AssetId`. Update from logs in the ExEx path
(GUIDE 03), not by polling.

`PriceVector` must be a flat `Vec<Price>` — the engine indexes it millions of
times per second and a `HashMap` here is a measurable cost.

## Step 4 — The MEV-Share client (the SVR path)

Two halves, both small.

**Half 1 — the event stream.** SSE from `https://mev-share.flashbots.net`
(`:ping` every 15s if idle). Hints carry optional fields — `hash`, `to`,
`functionSelector`, `callData`, `logs`, gas fields. Senders choose what to share,
so **write the matcher to work with partial information**: match on `to` ==
your configured `svr_aggregator` first, and use `callData`/`logs` to extract the
price when present, falling back to your predicted price when it is not.

**Searcher access is permissionless.** Protocol/SVR integration approval is not a
gate on listening or bidding. Match hints whose `functionSelector` is `6fadcf72`
(when present), decode, backrun, and bid via `block.coinbase` / `refundConfig` as
GUIDE 12 §4e specifies. You do not need a special allowlist to be a searcher on
the MEV-Share stream.

```rust
pub struct MevShareHint {
    pub hash: B256,                     // reference this in your bundle
    pub to: Option<Address>,
    pub function_selector: Option<[u8; 4]>,
    pub call_data: Option<Bytes>,
    pub logs: Option<Vec<Log>>,
}
```

**Half 2 — bundle submission.** `mev_sendBundle` to `https://relay.flashbots.net`,
with `inclusion` (`block`, `maxBlock`), `body` (the hinted tx **by hash**, then
your signed liquidation), and `validity` (`refundConfig`). Sign the request body
with your searcher key and send it in the Flashbots auth header. GUIDE 13 owns
the submission path; build the client here and hand it over.

Also implement a `/api/v1/history` reader — historical hints are how you calibrate
the bid model offline before risking anything.

**Chainlink's own guidance is to integrate with all supported orderflow auction
providers.** On mainnet that is MEV-Share today; keep the venue behind the
`Submitter` trait (GUIDE 13) so adding another is a file, not a refactor.

## Step 5 — Public mempool decoder (non-SVR feeds)

Still worth building. Decode OCR `transmit()` calldata from the pending stream,
extract the median observation, emit `SourceKind::PendingPublic`. This covers
non-SVR feeds, other protocols, and the SVR **standard** aggregator, which still
publishes publicly as the fallback path.

Budget effort proportionally: on a mainnet-Aave-focused bot this is a secondary
path, not the edge.

## Step 6 — The aggregator simulator

This is what makes you fast enough to bid well, and under SVR it matters *more*,
not less: you cannot win by seeing the hint sooner than anyone else — everyone
gets the same SSE — so you win by having already computed the candidate set,
the quote, the route and the bid before the hint arrives.

```rust
impl AggregatorSim {
    fn on_tick(&mut self) -> Option<PendingUpdate> {
        let px = (self.aggregation)(&self.sources);   // median across CEX venues
        let dev = deviation_bps(px, self.last_onchain.0);
        let hb_due = self.last_onchain.1.elapsed().as_secs() >= self.heartbeat_secs;
        (dev >= self.deviation_threshold_bps || hb_due).then(|| PendingUpdate {
            feed: self.feed, predicted: px,
            eta: self.estimate_eta(dev), confidence: self.confidence(dev),
        })
    }
}
```

Build CEX book-top subscribers by hand (raw websockets, one per venue). Exchange
SDKs add latency and dependencies for a subscription that is a few hundred lines.

**Calibrate `estimate_eta` empirically.** Log every historical `transmit` against
your simulated deviation curve and fit the observed delay distribution. It is not
a constant — it varies by feed and by gas conditions. The replay archive from
GUIDE 05 already contains what you need.

Emitting `PendingUpdate` triggers pre-warming in GUIDE 08: recompute the warm
band under the predicted price, pre-solve routes, pre-sign templates. **Never
fire on a prediction alone** — `Predicted` confidence is not `SvrAnnounced`
certainty.

## Step 7 — Derived pricing

A quiet-bug factory. Any collateral not priced by a direct feed needs an adapter
that reimplements the protocol's exact formula:

- **LST/LRT** — `wstETH = stETH_price × stEthPerToken()`. The rate is an on-chain
  read that moves on rebase. Subscribe to it as a price source.
- **Yield-bearing stables** — `sDAI = DAI × chi`, same shape
- **LP tokens** — fair-value pricing, not spot reserves. Must match the protocol's
  oracle contract exactly, including rounding.
- **Capped oracles** — some protocols cap an LST exchange rate's growth. Replicate
  the cap or compute a health factor the protocol disagrees with.
- **Cross-rate composition** — `X/USD = X/ETH × ETH/USD`; rounding order matters.

Rule: if the protocol prices it with a contract, your adapter reimplements that
contract, and the drift detector covers it.

**These are trigger sources, not just correctness obligations.** An LST exchange
rate update moves every position collateralized by that asset — with no Chainlink
transmit, no MEV-Share hint, and no auction. A searcher subscribed only to
aggregator feeds does not see it coming. Emit a `PriceTick` with
`SourceKind::Derived` on every rate change and let the engine raise
`TriggerCause::DerivedRate` (GUIDE 08).

Subscribe to the rate contracts themselves — `stEthPerToken()`, sDAI's `chi`
accrual, LRT rate providers — through the same ExEx log stream, so a rate update
is an ordinary event rather than something you poll for.

## Step 7b — Rust: concurrency & safety

This crate straddles the async/sync boundary, so it is where
`RUST-CONVENTIONS.md` §5 gets applied.

**Async on the edges, sync in the middle.** Websocket subscriptions, the
MEV-Share SSE stream and RPC reads are `async` on a Tokio runtime pinned to
non-isolated cores. The `PriceVector` the engine reads is touched synchronously.
Nothing in this crate calls `block_on` from the hot thread.

**Many sources → one fuser: bounded MPSC.**

```rust
let (tx, rx) = crossbeam_channel::bounded::<PriceTick>(8192);
// One sender clone per venue/feed task; one fusion thread receives.
```

Multiple producers makes SPSC wrong here. Bounded gives backpressure; on full,
drop and count — a stale tick from one venue is not worth stalling the others.

**One fuser → hot path: triple buffer, not a channel.**

```rust
let (mut input, mut output) = triple_buffer::TripleBuffer::new(&PriceVector::zeroed()).split();

// Fusion thread: publish a complete vector.
input.write(vector);

// Hot path: wait-free, always the most recent COMPLETE vector.
let px = output.read();
```

This is the important choice in the crate. A channel would build a backlog of
superseded prices the engine must drain to find the current one; a `Mutex` would
put the hot path behind the fusion thread's writes. The engine wants *the latest
complete state*, which is precisely what a triple buffer provides — wait-free,
allocation-free, no torn reads.

`PriceVector` must therefore be `Clone` and cheap: a flat `Vec<Price>` indexed by
the global `AssetId`, sized once at startup.

**SSE reconnect without losing your place.** The stream task owns its own
backoff and never panics on a dropped connection — a disconnect is an expected
state that feeds the GUIDE 14 halt matrix, not an error to unwrap. No
`.unwrap()` on any network result in this crate; deny-level lints enforce it.

**Aggregator simulator state is thread-local.** Each feed's `AggregatorSim` lives
on the fusion thread and is mutated through `&mut`. It is never shared, so it
needs no synchronization.

## Step 8 — Fusion

One `PriceOracle` that serves, per asset: the canonical price, the best available
forward-looking price with its `SourceKind`, and a staleness flag. The engine asks
for a `PriceVector` at a given `SourceKind` confidence level and never learns
where the numbers came from.

---

## Acceptance criteria

- [ ] Startup validation rejects a config whose `svr_aggregator` does not match
      what the protocol actually reads (test by mutating the config)
- [ ] MEV-Share SSE client stays connected ≥ 24h, handles `:ping`, and reconnects
      with backoff on drop without losing its place
- [ ] Hint matcher correctly identifies SVR oracle hints for every configured
      feed, **including hints with `callData` absent**
- [ ] `mev_sendBundle` request signs and serializes correctly, verified against
      the Flashbots spec on Sepolia before mainnet
- [ ] Historical hint replay from `/api/v1/history` reproduces ≥ 30 days of SVR
      oracle announcements, joined to the GUIDE 05 liquidation ground truth
- [ ] Aggregator sim predicts ≥ 90% of actual `transmit` events with ETA error
      within one block, on 30 days of replay
- [ ] Derived price adapters match the protocol's oracle contract within 1 bp on
      a fork, for every LST/LP asset in the registry
- [ ] `PriceVector` lookup is a flat index; no `HashMap` on the hot path
- [ ] Staleness detection trips at `heartbeat × 1.5` and marks the asset
      untradeable (feeds GUIDE 14)

## Failure modes

| Symptom | Cause |
|---|---|
| Never win an Aave liquidation despite fast detection | Watching the public mempool for an SVR feed — wrong path entirely |
| Hints arrive but never match | Matcher requires `callData`; senders share optionally |
| Bid is correct but always late | Nothing pre-warmed; candidate computed after the hint instead of before |
| HF wrong only for wstETH/sDAI/LP collateral | Derived price formula or rounding mismatch |
| HF wrong protocol-wide overnight | Feed migrated; startup validation was a warning not a failure |
| Aggregator sim fires constantly | Deviation threshold or aggregation function doesn't match the feed's config |

## Handoff

GUIDE 08 consumes `PriceTick` and `PendingUpdate`. The pre-warm contract matters:
the engine must act on `Predicted` by *preparing* and on `SvrAnnounced` /
`PendingPublic` by *firing*. Make that distinction explicit at the interface.
