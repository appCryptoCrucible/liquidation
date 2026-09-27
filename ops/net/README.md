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

## What is not claimed

- No colocation, no 0.4 ms RTT, no invented p99.
- Kernel bypass (DPDK / AF_XDP) is not applied here (GUIDE 16 §5: measure first).
- Public mempool is not a path.
- 13A's `reqwest::Client` is one object cloned into the JoinSet (default pool).
  Aggressive keepalive + pre-warm are **ABSENT** on that client; 16D's monitor
  client is built with pool + keepalive and tested for TCP reuse after warmup.
