# GUIDE 17 — Production Operations

| | |
|---|---|
| **Crate** | `liq-bot` (wiring, supervision), `ops/` |
| **Prerequisites** | GUIDE 13, 14, 15, 16 |
| **Est. effort** | 1–2 weeks, then continuous |
| **Blocks** | nothing — this is the end state |

## Objective

Make the system survivable without a human watching it, and establish the review
cadence that keeps it competitive as the market adapts.

## Build vs. buy

Buy your existing deployment stack. Build the supervision, the lease, and the
runbook.

---

## Step 0 — The supervised live window

Between the shadow gate and unattended running sits one more stage: a period
where the bot submits for real, but only while you are watching. It is a gate
(`ORCHESTRATOR.md` §4), and it exists because the failures it catches are
invisible to everything upstream of it.

Replay proves you would have detected. Shadow proves you would have bid, and
roughly when. Neither can tell you:

- whether your transactions **land**, and whether they revert when they do
- whether the relay accepts what you actually send, not what you think you send
- whether the bid and gas model survive contact with a live auction
- whether nonce handling holds through a burst of three opportunities in two blocks
- whether **realized** net matches **predicted** net — the number that decides
  whether any of the modelling was real

### Window the submission, not the box

The instinct is to run the machine only during the hours you can watch it. That
is the one version of this that does not work, for three reasons that compound:

- **Bare-metal hourly instances release their local disks.** A Reth re-sync is
  days, so "spin it up for the evening" means it is never synced.
- **Even with persistent storage, a box that is off for 20 hours restarts ~6,000
  blocks behind** and spends the opening stretch of every session catching up —
  which is precisely the stretch you were there to watch.
- **A monthly box costs less than the re-syncs**, and once it is up, running the
  detector around the clock is free.

So the box stays up continuously and `submit_enabled` is a hot-reloadable config
flag (GUIDE 17 Step 1b separates config from compiled behaviour for exactly this).
The *flag* follows your schedule. Nothing else does.

### Observation must never be windowed

This is the subtler half. Liquidation cascades cluster with volatility, and
volatility does not respect your evenings. A quiet four-hour window may contain no
liquidations at all on your target set — at which point "we didn't miss anything"
is a statement about the sample, not about the system, and it is the most
comfortable possible way to be wrong.

Run GUIDE 09 Step 4a's watcher across the whole clock from the moment the node is
synced. Then the miss alarm is measured against the **population** of
liquidations rather than the ones that happened while you were at your desk, and
a supervised session inherits weeks of continuous evidence instead of generating
four hours of it.

### Bounds during the window

- **One protocol first.** The one with the most drift-week evidence behind it.
- **A hard notional cap** per liquidation, set low enough that the worst case is
  tuition rather than damage.
- **A per-session loss limit** that halts submission automatically. You are
  watching, but you are watching one screen and the loop is faster than you.
- **Every revert root-caused before the next session.** Not "it reverted, the
  guard worked, fine." A guard firing is data about a model that was wrong.

### Clearing the gate

| Criterion | Bar |
|---|---|
| Supervised sessions | ≥10 across ≥2 weeks, at least 3 during a volatile period |
| Reverts | Zero unexplained. Every one traced to a named guard and a reason. |
| Miss alarm | Zero `NotTracked` / `HealthWrong` over the **continuous** window, not just the supervised hours |
| Realized vs. predicted | Realized net within a stated tolerance of predicted, on every win — with the tolerance written down *before* the window, not fitted after it |
| Relay | No submission rejected for a reason you cannot explain |
| Nonce | No stuck or gapped nonce across the whole period |

The realized-versus-predicted row is the one that matters most and the one most
easily fudged. Pick the tolerance first.

## Step 1 — Topology (single baremetal box, mainnet-only)

Hardware tuning is GUIDE 16. This step is about supervision and failover on top
of it.

**This is the topology (D05): one box, no spare, no ops box.**

```
        ┌────────────────────────────────────────┐
        │  CONTROL PLANE  (config files + you)   │
        │  config · halts · alerts · PnL        │
        └───────────────────┬────────────────────┘
                            │ hot-reload + kill signal
                ┌───────────▼────────────┐
                │  THE BOX               │
                │  reth + liq-bot ExEx   │
                │  submit_enabled: bool  │
                │  watcher · SQLite PnL  │
                └───────────┬────────────┘
                            │
                ┌───────────▼────────────┐
                │  CEX FEEDS             │
                │  one subscriber/venue  │
                └────────────────────────┘
```

**Failure posture — decided, not open.** Flashloan-only funding means an outage
costs missed opportunities, never stranded capital, so single-box risk is a
priced trade rather than a corner cut. There is no failover. A dead box is a dead
box until you restart it, and the cost of that is measured in opportunities you
did not see.

Everything below that mentions a **lease** still applies with one holder: the
lease is what a restarting process must acquire *after* passing the drift check,
and its value is that a box which restarts into wrong state refuses to submit.
That is worth having on one box. It is not a failover mechanism here.

**The two-box variant is phase 2** (GUIDE 16 Step 7) and nothing in this guide
assumes it. If profit later justifies a warm spare, the lease generalizes to two
holders and the deploy story improves; until then, read every "spare" below as
"not yet."

**Colocation.** Proximity to the relay and builders buys decision time before a
fixed block deadline, and is a straight latency race for the non-auctioned
triggers (GUIDE 16 Step 5). Low *and stable* RTT beats occasionally-low RTT —
measure p99, not p50, continuously.

**CEX feeds.** One subscriber per venue, on the box. Opening N subscriptions to
the same venue gets you rate-limited and gives you N slightly different views of
the same book — so keep it to one even when it is tempting to add a second for
redundancy.

**Warm spare — phase 2, not now.** Recorded here so the lease design makes sense:
a spare would run the full pipeline including simulation while holding no submit
lease, and failover would be lease acquisition against already-current state. Use
a short-TTL lease rather than consensus if you ever build it — a few seconds of
dual submission is survivable (both bundles are guarded), a few minutes of cold
start is not. None of this is built in phase 1.

## Step 1b — Deploys are node restarts

The ExEx architecture (GUIDE 16 Step 1) means your bot is compiled into the Reth
binary. Every deploy — a new adapter, a refit `F(β)` table, a one-line fix — replaces
that binary and stops the node. Reth restarts from disk in tens of seconds, but
that is still a window where you see nothing.

Consequences to plan around:

- **Batch deploys.** Weekly or on a fixed cadence, not on every merge. The bid
  model is tuned by config, so separate hot-reloadable config from compiled
  behaviour and push parameter changes without a rebuild.
- **On one box, the gap is the cost of deploying.** Deploy in a quiet window,
  deliberately rather than by surprise, and know the number — the cold-restart
  time from the acceptance criteria below. Batching deploys is what actually
  reduces this, which is why config is hot-reloadable and behaviour is not.
  *(Phase 2: with a warm spare the restart becomes a lease handoff instead —
  GUIDE 16 Step 7.)*
- **Never deploy during high volatility.** The window costs the most exactly when
  opportunities are densest.
- **Post-restart drift check gates the lease** (Step 2). A binary that restarts
  into wrong state must not submit.

## Step 2 — Supervision

- Process supervisor restarts `liq-bot`; the ExEx restarts with Reth
- On restart: mmap snapshot + replay WAL tail, then verify against the drift
  detector **before** acquiring the submit lease
- Never submit from a process that has not passed a post-restart drift check.
  A fast restart into wrong state is worse than a slow one.

## Step 3 — The runbook

Write it before you need it. One page per alert, each answering: what it means,
what to check first, what to do, when to escalate.

Minimum set:

| Alert | First check | Action |
|---|---|---|
| `HealthWrong` nonzero | Which protocol, which positions | Halt protocol; diff against `health_probe`; likely a missed event path or a protocol upgrade |
| Proxy implementation changed | Which contract, what changed | Halt; read new source; re-run replay; re-arm manually only after fixtures pass |
| Drift mismatch rising | Protocol, band distribution | Halt protocol; check for a silent parameter change |
| MEV-Share disconnected | Network, auth key validity | Non-SVR paths continue; reconnect with backoff; escalate at 10 min |
| Node lag | Disk, peers, sync status | Halt all submission — a lagging node produces confident decisions on stale state. Do not re-arm until lag is understood, not merely gone. |
| Reverts on chain | Revert reason | If guard-related, state is stale — check node lag. If encoding-related, halt and fix. |
| Drawdown halt | PnL ledger by protocol and cause | **Do not re-arm without understanding the cause.** This switch exists precisely for the case where you are tempted to. |
| Gas spend anomaly | Revenue vs. spend ratio by trigger | Usually a revert loop or a bid model regression |

## Step 4 — Review cadence

The system does not stay competitive on its own. Competition adapts, protocols
upgrade, and the bid model decays.

**Daily (automated report):** PnL by protocol and trigger cause; outcome taxonomy
counts; `HealthWrong` and `NotTracked` must be zero; drift mismatch rates.

**Weekly (human):** `F(β)` fit review — is expected profit per opportunity improving
or is win rate being chased? `Outbid` gap distribution. Any new `WaitedTooLong`
data for the V4 timing optimizer.

**Monthly:** Is each path still profitable *net of gas and bids*? The SVR auction
path in particular is worth re-examining monthly — as competition converges, the
auction extracts more of the surplus, and a path that was profitable in month one
may not be in month six. Be willing to turn one off.

**OS patching:** scheduled and deliberate, never automatic. Automatic upgrades
are disabled on this box (GUIDE 16 Step 0); that means security patching is now
*your* calendar item, not the distro's. Put it on the monthly review, apply it in
the same window as a batched deploy, and re-run the benchmark harness afterwards
— a kernel update can move p99.9. Track the distro's security advisories for the
kernel, glibc and OpenSSL specifically.

**On every protocol upgrade:** re-read source, re-pin fixtures, re-run the full
replay, and treat the adapter as unverified until the drift detector is quiet
again for a week.

**Before every deploy:** run the GUIDE 16 benchmark harness and compare p99.9
per stage against the recorded baseline. A latency regression is as much a
production incident as a test failure.

**On every dependency bump** (especially Reth): full replay before deploy. An
ExEx API change is the most likely source of a silent ingest regression, and it
will not announce itself.

## Step 5 — Key management

- Operator keys are hot, one per nonce slot, holding only gas
- Vault owned by a cold multisig with per-tx and rolling-daily pull caps
      (GUIDE 10 Step 4)
- Searcher key for Flashbots auth is separate from operator keys
- Rotation procedure documented and rehearsed, not improvised during an incident
- Assume the hot machine is compromised and verify the blast radius is the daily
  cap. If you cannot state that number from memory, the separation is not real.

## Step 6 — What "done" looks like

The system is in steady state when:

- Correctness alerts (`HealthWrong`, `NotTracked`, drift) are reliably zero
- Failover has been exercised in a drill, not just in theory
- Adding a protocol is 2–4 days and touches no engine code
- The weekly review changes a parameter, not the architecture
- You can leave it running for a week without watching it

That last one is the real test, and it is the only justification for the
correctness work in guides 02–05 that felt slow at the time.

---

## Acceptance criteria

- [ ] Post-restart drift check gates lease acquisition (test: corrupt the
      snapshot, confirm the process refuses the lease)
- [ ] Cold-restart time measured and written down — snapshot + WAL replay to
      first submittable opportunity. This is your actual outage cost per restart.
- [ ] The kilobytes are in version control **from day one**: `reth.toml` and its
      address list, the TuneD profile, all bot config, the deployed contract
      addresses, `STATE.md` and its decision log. Minutes of work, and the only
      part of this system that is genuinely unrecoverable.
- [ ] Operator and profit-sink keys backed up off the box, encrypted. Losing the
      operator key does not lose funds — `PROFIT_SINK` is immutable and already
      swept profit is untouched — but it does brick a deployed `Executor` and
      cost you a redeploy.
- [ ] *(Deferrable — do it when you stop watching daily.)* State-store snapshots
      and WAL replicated off the box. The **node database needs no backup at all**:
      `reth download` is the backup. GUIDE 16 Step 7, "What actually needs backing
      up."
- [ ] Runbook covers every alert in the GUIDE 14 matrix
- [ ] Every alert fired at least once in a drill, with the runbook followed
- [ ] Daily automated report delivered; weekly review scheduled with an owner
- [ ] Key rotation rehearsed
- [ ] Blast radius of a compromised operator key documented and verified: the
      operator key can only call `execute()` on an immutable Executor whose
      profit goes to the immutable `PROFIT_SINK`; the at-risk amount is the
      Executor's **standing balance between sweeps** (D12 `k` and max age) plus
      the key's own gas float (GUIDE 14 Step 5 floor). There is no vault and no
      vault cap in this design — GUIDE 10 Step 9 lists what the key can reach
- [ ] Full replay runs before every deploy, gated in CI
- [ ] `submit_enabled` is hot-reloadable config, not a compiled constant — toggled
      without a node restart in a drill
- [ ] Supervised gate cleared: all six rows of Step 0's table, each with its
      number and date range, recorded in `STATE.md`
- [ ] One full week of unattended operation with no manual intervention

## Failure modes

| Symptom | Cause |
|---|---|
| Restart takes far longer than expected | Snapshot stale, WAL tail long, or node re-verifying — measure it once rather than discovering it during volatility |
| Restart into wrong state | Drift check not gating lease acquisition |
| Silent competitiveness decay | No weekly `F(β)` review; win rate chased instead of expected profit |
| Protocol upgrade breaks production | No proxy watcher (GUIDE 14), or fixtures not re-pinned |
| Dependency bump breaks ingest | Replay not gated in CI before deploy |
| Incident handled by improvisation | No runbook, or runbook never drilled |

---

## End of the build order

Fifteen guides, roughly five to seven months of work for a small team, and the
sequencing is deliberate: correctness first (00–05), detection second (06–08),
action third (09–13), generalization last (14–15). The most common way to fail is
to invert that — to build execution early because it is the exciting part, and
discover eighteen months later that the health factors were subtly wrong the
whole time.
