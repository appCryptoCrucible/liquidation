# Network queues (GUIDE 16 §5)

Separate **submission** traffic (builder / MEV-Share HTTPS) from **CEX feed**
subscriptions so a market-data burst cannot share a NIC TX queue with a bundle.

This host is Windows: `ethtool` and `/sys/class/net` are not available.
`verify-queues.sh` / `verify-queues.ps1` must print **ABSENT** and exit 2 here — never PASS.

## Linux apply

On the colocated box, after 16B has named the NIC and CPU masks:

```bash
export LIQ_NET_IFACE=           # real netdev from `ip link`, not a guess
export LIQ_NET_SUBMIT_XPS=2000  # logical CPU 13 (liq-exec-submit)
export LIQ_NET_FEED_XPS=4000    # logical CPU 14 (liq-oracle-cex)
# optional ntuple: write ops/net/ntuple.rules then LIQ_NET_APPLY_NTUPLE=1
ops/net/queues.sh
ops/net/verify-queues.sh
```

The box has two 10 Gbps ports. One carries Reth and Lighthouse peering.
The other carries builder and relay HTTPS. Names come from `ip link` on
the box; `ops/net/queues.conf` stays `iface=CHANGE_ME` until then.
IRQs belong on CPUs 14, 15, 30 and 31 (`irqaffinity` in `ops/host/cmdline`).

`queues.sh` writes `state.applied` only after ethtool + XPS succeed. That file
is **not** in git. Without it, verify is FAIL (Linux) or ABSENT (no `/sys`).

## Endpoint latency (`tools/net/endpoint_latency.py`)

Measures the path to every builder in `config/builders.toml` and, for
geography, the public MEV-Boost relays: per sample a fresh TCP connect, the
TLS handshake, one request (`eth_chainId` to a builder, `GET
/eth/v1/builder/status` to a relay), each timed on its own. Standard library
only; run it unchanged on the production box and compare:

```
python tools/net/endpoint_latency.py --samples 20
```

It prints a Markdown table (min / p50 / p95 ms per phase) and writes the raw
samples to `ops/net/latency-<host>.json`.

### 2026-10-09, dev box (consumer line, Frankfurt area by the numbers)

| endpoint | where (by address) | connect p50 | tls p50 | request p50 |
|---|---|---|---|---|
| titan-eu | AWS eu-central-1 | 13.8 | 17.2 | 12.6 |
| titan (relay) | AWS eu-central-1 | 14.5 | 17.2 | 13.0 |
| buildernet-eu | 198.203.202.x | 20.8 | 24.5 | 19.1 |
| ultrasound (relay) | OVH, France | 26.1 | 28.6 | 23.9 |
| aestus (relay) | OVH, France | 31.0 | 33.4 | 29.5 |
| beaverbuild | AWS Global Accelerator (edge near, origin far) | 13.1 | 34.5 | 56.8 |
| agnostic (relay) | AWS Global Accelerator | 13.9 | 32.9 | 34.5 |
| bloxroute-regulated (relay) | CloudFront edge | 14.8 | 17.1 | 22.6 |
| titan-us | AWS us-east-1 | 98.6 | 110.3 | 97.2 |
| buildernet-us | 200.225.47.x | 98.8 | 105.0 | 98.3 |
| **flashbots** (builder and the MEV-Share endpoint) | AWS us-east-2 | 120.8 | 117.0 | 151.9 |
| flashbots-boost (relay) | Cloudflare edge, US origin | 14.4 | 21.0 | 158.2 |
| rsync | **no A record** (Cloudflare and Google resolvers, 2026-10-09) | — | — | — |
| bloxroute-max-profit (relay) | **NXDOMAIN**: the name is gone | — | — | — |

What the dev run already settles:

- **The Flashbots builder, which is also the only MEV-Share endpoint, is
  US-only.** From Europe a request there is ~150 ms p50 and ~220 ms p95
  (one TCP + TLS + request on a cold connection is ~390 ms). Every SVR
  backrun pays that; the European builders are ~13–20 ms. A box in Frankfurt
  moves the EU figures to a few ms and leaves the US ones where they are.
- **Cold connections cost one TLS handshake per bundle per builder**, which
  is about as long again as the connect: ~230 ms to a US host before the
  request is even sent. The submission client keeps those sockets warm
  (below), so a bundle to a warm builder pays only the request leg.
- **`rsync` cannot be reached**: `rsync-builder.xyz` resolves to nothing,
  and relayscan shows no rsync block in the 7 days to 2026-10-10. It was
  removed from `config/builders.toml` on 2026-10-10.
- **Quasar** (`rpc.quasar.win`, 21% of blocks that week) was added the same
  day. From the dev box: connect 14.6 ms and TLS 18.9 ms to a CloudFront
  edge, request 127.7 ms (p50 of 8), so the origin is far and the
  connection is kept warm.
- **The Flashbots row is a relay, not a builder.** Flashbots has run no
  builder of its own since 2024-12-05; its relay forwards into BuilderNet,
  which the `buildernet-*` rows reach directly. It stays the only
  `mev_sendBundle` endpoint.
- Block propagation is not measured here: it needs the node. The ExEx's
  block-arrival time against the slot start is the number to record on the
  production box.

## Submit path latency (2026-10-10)

Network distance is the one cost search cannot buy back: every millisecond
the bundle spends in transit is a millisecond less to search before the
builders' cutoff. The submit path is built to spend only the one-way wire
time.

**Fan-out: three builders, three endpoints, all warm.** `config/builders.toml`
lists Titan EU, BuilderNet EU and Quasar (90% of blocks, relayscan 7 d to
2026-10-10). The box is in the EU, so the Titan and BuilderNet US rows were
dropped the same day; BuilderNet propagates a submit to its other regions.
Quasar has one region: "We currently have a single endpoint in us-east-1.
Multi-region endpoints will be released shortly." (docs.quasar.win); the
~123 ms below is Frankfurt to us-east-1, and no EU host name resolves yet. beaverbuild and the Flashbots relay are not in the
`eth_sendBundle` fan-out: both feed BuilderNet, which the direct rows reach in
~20 ms from Frankfurt instead of ~150 ms. The Flashbots relay remains the
MEV-Share endpoint, and it is warmed too.

**HTTP/2 on every connection.** All six endpoints negotiate h2 over TLS 1.3
(ALPN probe and the live test below). reqwest is built with `http2` and
`native-tls-alpn`; without the latter the native TLS backend offers no ALPN
and every connection is HTTP/1.1. On h2 a warm POST and a bundle multiplex on
one connection, so a bundle sent while a warm is in flight no longer opens a
cold socket. The client also PINGs every connection every 5 s (3 s timeout),
so a dead connection is dropped from the pool before a bundle finds it.

**Warm sockets.** The pool keeps connections 90 s idle with TCP keepalive
every 10 s. The exec worker pre-warms every builder and the relay at start,
then re-warms the warm set every 20 s with one `eth_chainId` POST per target
(builders with `warm = true`, plus the MEV-Share relay always). Servers close
idle connections on their own clock (AWS ALB defaults to 60 s); 20 s stays
under every one with room for a failed round.

**The sockets live on the submit core.** The exec worker thread is
`liq-exec-submit`, pinned to its `cores.toml` core (13: isolated,
`nohz_full`, the core the NIC's submit TX queue is steered to), and its tokio
runtime's single I/O worker is pinned to the same core. The prewarm and
re-warm loop run on that runtime, so every pooled connection, and the task
that writes its bytes, belongs to the submit core rather than the main
runtime on the housekeeping cores with the IRQs and Lighthouse. Before
2026-10-10 the worker was unpinned and the warm sockets were the main
runtime's.

**No job waits on another's acks.** The worker signs and gates each job in
arrival order on its own thread (nonces stay in order), then spawns the POST.
A builder fan-out returns at the first accept and leaves the other POSTs to
finish and log on their own. Before, each job waited for its slowest ack
(~100 ms from a US builder, ~150 ms from the relay) before the next job was
even signed.

Measured through the real submit client from the dev box
(`live_warm_targets_speak_h2_and_warm_beats_cold`, three runs; cold includes
DNS and the Windows TLS handshake):

| target | protocol | cold | warm p50 |
|---|---|---|---|
| titan-eu | HTTP/2 | 75–159 ms | 14–17 ms |
| buildernet-eu | HTTP/2 | 127 ms | 24 ms |
| titan-us | HTTP/2 | 471–740 ms | 99–102 ms |
| buildernet-us | HTTP/2 | 450 ms | 101 ms |
| quasar | HTTP/2 | 403–653 ms | 123–614 ms per run; 30 keep-alive samples: min 114, p50 171, p90 399 (CloudFront POP Munich, origin us-east-1) |
| relay.flashbots.net (MEV-Share) | HTTP/2 | 483–880 ms | 113–145 ms |

Tests, all on the real `ExecPath`:

- `crates/liq-exec/tests/warm_connections.rs`: a keep-alive mock counts
  accepted sockets. Re-warms and a signed MEV-Share submit ride one socket,
  and the re-warm loop keeps a socket open past a server's 300 ms idle close.
  The ignored live test checks h2 and warm against cold on every shipped
  target.
- `crates/liq-exec/tests/submit_path.rs`
  `fanout_returns_at_first_accept_and_far_builder_still_gets_it`: with a
  builder that answers at 800 ms, the submit returns under 400 ms and the far
  builder still receives the same signed bytes.
- `crates/liq-bot/src/exec_bind.rs`
  `exec_worker_does_not_wait_on_acks_between_jobs`: two queued jobs reach a
  relay that answers 600 ms after reading well inside 300 ms of each other.
  With the POST awaited in line, as before, the gap is 622 ms and the test
  fails.

`liq_obs::thirteen_a_http_pool_seam().handshake_free_critical` reports
`PROVEN_SUBMIT_CLIENT` while the warm-connection test and the `rewarm` /
`spawn_rewarm` functions exist.

## What is not claimed

- No colocation, no 0.4 ms RTT, no invented p99.
- Kernel bypass (DPDK / AF_XDP) is not applied here (GUIDE 16 §5: measure first).
- Public mempool is not a path.
- Quasar's origin and the Flashbots relay are ~110–125 ms away from
  Frankfurt even when warm. A box location cannot fix both that and the EU
  figures; that trade is the production box's to measure.
- The warm set is a dev-box measurement; rerun `endpoint_latency.py` on the
  production box before changing it.
