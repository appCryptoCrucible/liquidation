# Liquidation bot

Liquidates under-collateralized positions on Ethereum mainnet lending protocols: Aave V3, Spark, Aave V4, Morpho Blue, Euler V2, Silo V2, Liquity V2, Fluid, Gearbox V3 and Compound V2 forks. It runs inside a Reth node as an execution extension (ExEx), mirrors every tracked account from the node's own logs, and liquidates through the flash-loan-funded `Executor` contract, sending bundles to block builders and MEV-Share.

## Build and run

- **Production** is one process, Reth with the bot as its ExEx. `cargo build --release -p liq-reth --features node` builds the `reth` binary (Linux only: Reth's storage provider does not compile on Windows). `ops/systemd/reth-liq.service` runs it from the repo root, beside Lighthouse (`lighthouse.service`), with `LIQ_RPC_URL` set to the node's own HTTP RPC.
- **First start** builds the state by replaying every tracked log from block 7,710,671 to the node's finalized block, so the node must be fully synced. Later starts resume from the last snapshot.
- **Sending** needs `submit_enabled` (`config/node.toml`, re-read on change or `SIGHUP`), the submit lease and a nonce resync. Without all three the same path runs and only records what it would have sent.
- **Standalone**, without hosting the loop in a node: `cargo build --release -p liq-bot`, run by `ops/systemd/liq-bot.service`.
- **Tests:** `cargo test -p <crate>` for the Rust crates, `forge test` in `contracts/` for the Executor.

Design guides are in [`liquidator-guides/`](liquidator-guides/INDEX.md), per-protocol event coverage in [`docs/coverage/`](docs/coverage/), and protocols deliberately not supported in [`DECLINED.md`](DECLINED.md).

## System map

How the system runs, as built. Each diagram answers one question; read them in order, or jump to the one you need.

1. [The whole system](#1-the-whole-system): what runs where, and what it talks to
2. [Inside the process](#2-inside-the-process-threads-and-what-they-share): threads and what they hand each other
3. [One block, start to finish](#3-one-block-start-to-finish)
4. [How an account is found, tracked and corrected](#4-how-an-account-is-found-tracked-and-corrected)
5. [Drift and resync](#5-drift-and-resync)
6. [From candidate to transaction](#6-from-candidate-to-transaction)
7. [On chain: `Executor.execute(plan)`](#7-on-chain-executorexecuteplan)
8. [Offline jobs and companion processes](#8-offline-jobs-and-companion-processes)
9. [Crate layers](#9-crate-layers)

Boxes are processes, threads or steps; cylinders are stored data; diamonds are decisions. Pieces that are built but not part of the production process are listed at the end, in [Built but not wired](#built-but-not-wired), rather than drawn.

---

### 1. The whole system

Production is one process: Reth with the liquidation loop installed as an ExEx (`liq-reth`, started by `ops/systemd/reth-liq.service`). Every chain read the bot makes goes to that co-located node; a hosted RPC is used only by the daily discovery job.

```mermaid
flowchart LR
    subgraph eth["Ethereum"]
        P2P["p2p network"]
        CHAIN["Executor contract →<br/>lending protocols · flash sources · DEX pools"]
    end

    subgraph proc["reth process · liq-reth"]
        RETH["Reth execution client<br/>keeps receipts only for addresses<br/>in ops/reth/reth.toml's log filter"]
        EXEX["liquidator ExEx"]
        BOT["Liquidation bot · liq-bot"]
    end

    subgraph host["Also on the bot host"]
        LH["Lighthouse<br/>consensus client"]
        STATE[("State dir<br/>snapshot · head · WAL · shadow JSONL")]
        CFG[("config/*.toml<br/>registry/registry.json")]
        DISC["liq-discovery<br/>daily 04:30 UTC"]
        COMP["Companion processes<br/>liq-watch · liq-books<br/>see section 8"]
    end

    subgraph flow["Order flow"]
        MEVS["MEV-Share"]
        BUILDERS["Block builders"]
    end

    HOSTED["Hosted RPC<br/>discovery only"]

    P2P -->|"blocks and transactions"| RETH
    LH -->|"Engine API"| RETH
    RETH -->|"committed, reverted, reorged chains"| EXEX
    EXEX -->|"owned blocks and logs"| BOT
    BOT -->|"FinishedHeight, once folded"| EXEX
    BOT -->|"localhost JSON-RPC<br/>views · eth_call · eth_simulateV1"| RETH
    STATE -->|"snapshot at start"| BOT
    BOT -->|"snapshots · shadow records"| STATE
    CFG -->|"at start, and registry exits live"| BOT
    MEVS -->|"Chainlink SVR hints"| BOT
    BOT -->|"backrun bundles"| MEVS
    BOT -->|"eth_sendBundle"| BUILDERS
    MEVS --> BUILDERS
    BUILDERS -->|"blocks carrying our bundles"| P2P
    HOSTED --> DISC
    DISC -->|"exits applied · markets to review"| CFG
    COMP -->|"reads the node"| RETH
```

---

### 2. Inside the process: threads and what they share

One pinned thread, `liq-node-hot`, owns all mutable state: it folds every block, runs the engine and plans liquidations, and never awaits or blocks. Everything slow (chain reads, signing, snapshots, the drift check) runs on its own thread and reaches the hot thread through lock-free rings, published `ArcSwap` values, or the pool book's one lock.

```mermaid
flowchart LR
    EXEX["ExEx future<br/>on Reth's runtime"]

    subgraph hot["liq-node-hot · pinned · the only writer"]
        direction TB
        ING["Ingest<br/>LogRouter sends each log to<br/>10 protocol adapters · 5 flash sources ·<br/>the DEX pool book · oracle feeds"]
        STORE[("Hot-thread state<br/>StateStore + undo ring · FlashIndex ·<br/>PoolBook · Chainlink and derived prices")]
        DRAIN["DrainJoin<br/>drives the Engine · plans liquidations ·<br/>folds read batches · applies drift actions"]
        ING --> STORE --> DRAIN
    end

    subgraph feed["Feed the hot thread"]
        direction TB
        MEVT["liq-oracle-mevshare<br/>SVR hints"]
        GOV["liq-bot-gov<br/>simulated governance payloads"]
        WARM["liq-bot-warm<br/>route table, re-solved from<br/>the pool book every block"]
        BOOKUP["liq-bot-registry · liq-bot-curve<br/>new exits · Curve reseeds"]
    end

    subgraph loop["Read for the hot thread, folded back in"]
        direction TB
        PRICES["liq-bot-prices<br/>each protocol's own prices"]
        STATER["liq-bot-state<br/>Fluid vaults · Gearbox interest<br/>and accounts · drift resyncs"]
        DRIFT["liq-obs-drift<br/>health vs protocol view"]
    end

    subgraph out["Take work from the hot thread"]
        direction TB
        SNAP["liq-bot-snapshot<br/>store to disk"]
        EXW["liq-bot-exec<br/>verify · sign · send"]
        INCL["liq-bot-inclusion<br/>receipts · outcomes · PnL"]
    end

    subgraph gate["Gate sending"]
        direction TB
        RISK["RiskGate<br/>halts and allow"]
        LEASE["Submit lease<br/>snapshot and WAL intact"]
        FLAG["submit_enabled<br/>file watch and SIGHUP"]
        STALL["liq-bot-stall<br/>node heartbeat"]
    end

    EXEX -->|"block ring"| ING
    ING -.->|"FinishedUpTo ring"| EXEX
    MEVT -->|"hint ring"| DRAIN
    GOV -->|"sim ring"| DRAIN
    WARM -->|"ArcSwap route table"| DRAIN
    BOOKUP -->|"pool book writes"| STORE
    DRAIN -->|"read sets"| PRICES
    DRAIN -->|"read sets"| STATER
    PRICES -->|"batch per head"| DRAIN
    STATER -->|"batch per head"| DRAIN
    DRAIN -->|"snapshot, every 100 blocks"| DRIFT
    DRIFT -->|"resync · quarantine · release"| DRAIN
    DRAIN -->|"snapshot, every 100 blocks"| SNAP
    DRAIN -->|"ExecInbox · GovInbox"| EXW
    EXW --> INCL
    STALL --> RISK
    RISK -.-> EXW
    LEASE -.-> EXW
    FLAG -.-> EXW
```

---

### 3. One block, start to finish

A committed block crosses from Reth to the hot thread and is folded before Reth is told it may prune. Planning happens in the same call. Chain reads that logs cannot replace land a moment later, inside the same block's undo record, so a reorg unwinds them with the block.

```mermaid
sequenceDiagram
    autonumber
    participant R as Reth
    participant X as ExEx future
    participant H as Hot thread (ingest)
    participant D as Hot thread (drain)
    participant E as Engine
    participant S as State and price readers
    participant W as Exec worker
    participant N as Node RPC
    participant B as Builders and MEV-Share

    R->>X: ExExNotification (committed chain)
    X->>H: owned blocks over the ring
    loop each block in the chain
        H->>H: route each log, apply_log, journal the inverse
        H->>H: collapse dirty sets by market
    end
    H-->>X: FinishedUpTo (store consistent)
    X-->>R: FinishedHeight (safe to prune)
    H->>D: after_block(store, dirty, touched positions)
    D->>D: every 100 blocks, snapshot to the writer and drift thread
    D->>S: republish read sets if they changed
    D->>E: price sync, protocol prices, on_dirty, on_block
    E-->>D: candidates
    D->>D: eligibility, select, assemble, bid, encode
    D->>W: ExecJob via try_send (never blocks)
    S->>N: multicall reads pinned to the head
    S-->>D: batch for that block
    D->>D: amend, folding the batch into the open block
    D->>E: on_dirty for settled accounts and accrual rows
    W->>N: nonce resync, simulate at the target block
    W->>W: record the intended submission (shadow, always)
    W->>B: signed bundle, only when submit_enabled, lease and nonce all hold
```

---

### 4. How an account is found, tracked and corrected

No list of accounts exists anywhere. Contracts are fixed when the adapters bind; an account appears the first time a log names it, and stays current through the same fold on every block. Where logs cannot carry a change, the account is re-read from chain.

```mermaid
flowchart TD
    BIND["Bind: contract set fixed at startup<br/>config/protocols/*.toml + registry,<br/>enumerated on chain for Fluid · Gearbox · Compound · Liquity"]
    REPLAY["First start: replay every subscribed log<br/>from block 7,710,671 to finalized<br/>out of the node's retained receipts"]
    SEEN["A log names an account<br/>→ intern(protocol, market, user)<br/>= discovered"]
    FOLD["Every block: logs fold into balances<br/>apply_log → StateWriter, undo-journaled"]
    Q{"Can the logs<br/>carry the change?"}
    CUR["State current for this block"]
    STALE["Gearbox multicall or partial liquidation<br/>account marked STALE, health Blocked/Unread"]
    REREAD["Re-read at the tip<br/>creditAccountInfo · balanceOf · getQuota"]
    FLUID["Fluid: the vault re-read every block<br/>simulated liquidate"]
    ENG["Engine recomputes health<br/>dirty positions · accrual · price crossings"]
    DRIFT["Every 100 blocks: drift sample<br/>health vs the protocol's own view"]
    RESYNC["Mismatch: resync the account from chain<br/>resync_reads"]
    NEW["New market or contract"]
    LIVE["Picked up live<br/>Morpho CreateMarket · Aave ReserveInitialized"]
    RESTART["Config change + restart<br/>bindings changed → full replay"]

    BIND --> REPLAY --> SEEN --> FOLD --> Q
    Q -->|"yes: Aave · Spark · Morpho · Euler<br/>Silo · Liquity · Compound"| CUR
    Q -->|"no: Gearbox"| STALE --> REREAD --> CUR
    Q -->|"no: Fluid"| FLUID --> CUR
    CUR --> ENG
    CUR --> DRIFT
    DRIFT -->|"mismatch"| RESYNC --> CUR
    NEW --> LIVE --> FOLD
    NEW --> RESTART --> REPLAY
```

---

### 5. Drift and resync

The drift thread compares sampled positions' health with each protocol's own health view, at the protocol's own prices and the snapshot's block. It never halts: wrong state is fixed from chain, and a position whose math still disagrees is kept out of quoting. Probes exist for Aave V3, Spark, Aave V4, Euler and Gearbox; the other protocols are not drift-checked.

```mermaid
stateDiagram-v2
    [*] --> Sampled: snapshot every 100 blocks
    Sampled --> Agrees: within 100 bps of the probe
    Sampled --> Resyncing: mismatch
    Resyncing --> SecondLook: next snapshot, re-probed
    SecondLook --> StateFixed: agrees now
    SecondLook --> Quarantined: still disagrees
    StateFixed --> ProtocolSweep: at least 3 fixed and a quarter of compared
    StateFixed --> Agrees
    ProtocolSweep --> Agrees: every account re-read, hottest first
    Quarantined --> Quarantined: still disagrees, re-probed each snapshot
    Quarantined --> Agrees: matches again, released
    Agrees --> Sampled: next snapshot

    note right of Resyncing
        stored state replaced from chain views,
        settled inside the block's undo record
    end note
    note right of Quarantined
        math or price bug: kept out of quoting,
        error logged with target drift
    end note
```

---

### 6. From candidate to transaction

Every candidate passes a gate before it is ranked, is sized by the smallest of four ceilings, and, when it acts on committed state, is simulated on the node before it is signed. Sending needs three conditions at once; without them the same path runs and only records what it would have sent. Governance liquidations enter separately, from a simulated payload rather than an engine candidate.

```mermaid
flowchart TD
    CAND["Candidate from the engine<br/>cause: price move · user action · accrual ·<br/>parameter change · pool change · SVR hint"]
    ELIG{"Eligible?<br/>a flash source can fund it<br/>and an exit route exists"}
    PARK(["parked in the Unfundable band"])
    SEL["select<br/>liquidatable only, ranked by net per gas"]
    SIZE["size = smallest of max repay,<br/>flash after haircut, route depth, viability band"]
    ASM["assemble BatchPlan<br/>flash · protocol leg · swaps and unwraps · minProfit"]
    BID["bid from config/bid.toml"]
    ENC["encode and validate · liq-plan"]
    TRIG{"Trigger"}
    VER["ExecJob marked rpc_verify<br/>simulated on the node at the target block"]
    SVR["MEV-Share backrun<br/>lands right after the hint's oracle update"]
    SKIP(["skipped: needs a pending tx replayed,<br/>no in-process simulator attached"])
    GSIM["liq-bot-gov<br/>queued Aave payload or Sky spell,<br/>simulated with eth_simulateV1"]
    GOV["handle_gov<br/>payload's logs laid over committed state<br/>→ accounts it makes liquidatable"]
    GJOB["GovJob<br/>one tx per account in one bundle,<br/>for the first block the payload can run"]
    WORK["Exec worker<br/>risk allow · nonce resync · sign"]
    REC[("shadow JSONL<br/>every intended submission")]
    GATE{"submit_enabled<br/>and lease held<br/>and nonce resynced"}
    SEND["POST to curated builders or MEV-Share<br/>never the public mempool"]
    INC["Inclusion watcher<br/>Included · Dropped · LostToCompetitor"]
    LEDGER[("PnL ledger")]

    CAND --> ELIG
    ELIG -->|no| PARK
    ELIG -->|yes| SEL --> SIZE --> ASM --> BID --> ENC --> TRIG
    TRIG -->|"state already committed"| VER --> WORK
    TRIG -->|"SVR hint"| SVR --> WORK
    TRIG -->|"pending oracle tx"| SKIP
    GSIM --> GOV --> GJOB --> WORK
    WORK --> REC
    WORK --> GATE
    GATE -->|yes| SEND --> INC --> LEDGER
    GATE -->|"no: recorded only"| REC
```

---

### 7. On chain: `Executor.execute(plan)`

The Executor holds funds only in transit. `OPERATOR` (a hot key) can only call `execute`; `PROFIT_SINK` is immutable, and every token leaving the contract other than flash repayments and liquidation repays goes there. The bid is a share of *realized* net, paid to the block's coinbase, so an optimistic quote shrinks the bid instead of the profit.

```mermaid
flowchart TD
    OP["execute(plan)<br/>OPERATOR only"]
    GOVF{"Plan carries a<br/>governance action?"}
    GOVA["Apply the Aave payload or Sky spell<br/>skipped if another tx in the bundle already did"]
    subgraph grp["For each flash group"]
        direction TB
        FLASH["Borrow from the group's flash source<br/>Aave · Uniswap V3 · Uniswap V4 · Morpho · Sky DSS,<br/>or none for reward-only legs"]
        CB["Provider calls back<br/>caller checked against transient storage"]
        LEG["Liquidate<br/>Aave V3 · Aave V4 · Morpho · Euler · Silo ·<br/>Liquity · Fluid · Gearbox · Compound"]
        SWAP["Swap or unwrap the seized collateral<br/>Uniswap V3 pool · allowlisted router · Uniswap V2 / Sushi pair ·<br/>Curve plain · Curve crypto · ERC-4626 redeem · Pendle PT redeem ·<br/>Curve NG one-coin · Pendle market sell"]
        REPAY["Repay the flash loan"]
        FLASH --> CB --> LEG --> SWAP --> REPAY
    end
    PSWAP["Swap everything left into WETH"]
    NET["net = WETH gained − gas cost − governance cost<br/>bid = net × bidBps · keep = net − bid"]
    CHECK{"keep ≥ minProfit?"}
    REV(["revert · the bundle drops"])
    PAY["Pay the bid to block.coinbase"]
    SWEEPF{"F_SWEEP set?"}
    SINK["WETH to PROFIT_SINK"]
    HOLD["WETH stays in the contract<br/>sweep() moves it to PROFIT_SINK, callable by anyone"]

    OP --> GOVF
    GOVF -->|yes| GOVA --> FLASH
    GOVF -->|no| FLASH
    REPAY --> PSWAP --> NET --> CHECK
    CHECK -->|"no, or net below gas"| REV
    CHECK -->|yes| PAY --> SWEEPF
    SWEEPF -->|yes| SINK
    SWEEPF -->|no| HOLD
```

---

### 8. Offline jobs and companion processes

These run outside the liquidation loop. They decide what the bot watches (the registry and protocol configs) and what the node keeps (the receipts filter), and they measure how the bot does (the watcher, books, digest and replay harness).

```mermaid
flowchart TB
    subgraph reg["What the bot watches · tools/registry"]
        direction TB
        TIMER["liq-discovery.timer<br/>daily 04:30 UTC"]
        DAILY["daily_refresh.py"]
        EXITS["discover_exits.py · discover_unwraps.py<br/>pools and unwraps for known tokens"]
        DISCO["discover.py<br/>every protocol family from its on-chain roots"]
        TRIAGE["triage_*.py · rederive.py<br/>registry corrections, run by hand"]
        GEN["gen_*_toml.py<br/>Aave V3 · Aave V4 · Morpho configs"]
        TIMER --> DAILY
        DAILY --> EXITS
        DAILY --> DISCO
    end
    REGJ[("registry/registry.json")]
    TOML[("config/protocols/*.toml")]
    REVIEW[("data/review/<br/>daily reports · registry-watch.log")]
    D15["tools/d15<br/>regenerates the receipts log filter"]
    RETHT[("ops/reth/reth.toml")]
    BOT["liquidation bot"]

    EXITS -->|"applied"| REGJ
    DISCO -->|"new markets and tokens"| REVIEW
    TRIAGE --> REGJ
    REGJ --> GEN --> TOML
    REGJ --> D15 --> RETHT
    REGJ -->|"live: new exits, no restart"| BOT
    BOT -->|"changes that need a restart"| REVIEW
    TOML -->|"at startup"| BOT
    RETHT -->|"which receipts the node keeps,<br/>so which logs the first replay can read"| BOT

    subgraph judge["How the bot is measured"]
        direction TB
        SHADOW[("shadow JSONL")]
        WATCH["liq-watch<br/>every on-chain liquidation"]
        WDB[("watcher SQLite + JSONL")]
        DIGEST["ops/digest/digest.py<br/>miss taxonomy · pre-flight"]
        REPLAY["liq-replay<br/>archive · health diff · parity · recall · bench · lite"]
        FORGE["Foundry tests<br/>unit · fork · invariant · differential"]
        BOOKS["liq-books<br/>per-bundle and per-liquidation books"]
        CSV[("books/*.csv")]
    end

    BOT --> SHADOW
    SHADOW -->|"what the bot tried"| DIGEST
    WATCH --> WDB
    WDB -->|"what happened on chain"| DIGEST
    WDB --> REPLAY
    BOOKS --> CSV
    REPLAY -->|"liq-diff-health FFI"| FORGE
```

---

### 9. Crate layers

Lower crates know nothing of higher ones; the dependency lint enforces the key edges (for example, `liq-engine` never sees an adapter crate, and `liq-protocol` depends only on `liq-types`). Every crate except `liq-books` depends on `liq-types`; those edges are omitted.

```mermaid
flowchart BT
    TYPES["liq-types<br/>ids · fixed point · prices · halts"]
    PROTO["liq-protocol<br/>Protocol trait · StateWriter · conformance harness"]
    STATEC["liq-state<br/>store · undo · snapshot · drift detector"]
    WIRE["liq-wire<br/>plan wire format"]
    FLASHC["liq-flash<br/>flash sources · index · selection"]
    CONFIG["liq-config<br/>config · registry · boot assertion"]
    SIM["liq-sim<br/>in-process revm"]
    ADAPT["9 adapter crates<br/>aave-v3 · aave-v4 · morpho-blue · euler-v2 · silo-v2 ·<br/>liquity-v2 · fluid · gearbox · compound-v2"]
    NODE["liq-node<br/>ingest · router · reorg · hot thread"]
    ENGINE["liq-engine<br/>health engine"]
    PLAN["liq-plan<br/>plan encoder + validate"]
    RISKC["liq-risk<br/>gate · caps · ledger · treasury"]
    ORACLE["liq-oracle<br/>feeds · derived · MEV-Share · governance"]
    WATCHC["liq-watch<br/>liquidation watcher"]
    ROUTER["liq-router<br/>pools · routing · sizing · bids · assembly"]
    EXECC["liq-exec<br/>builders · submit · nonce · inclusion"]
    OBS["liq-obs<br/>tracing · taxonomy · digest"]
    BOTC["liq-bot<br/>wiring · drain · readers · drift policy"]
    RETHC["liq-reth<br/>Reth + ExEx binary"]
    REPLAYC["liq-replay<br/>replay harness"]
    BOOKSC["liq-books<br/>standalone"]

    PROTO --> TYPES
    STATEC --> PROTO
    WIRE --> PROTO
    FLASHC --> PROTO
    CONFIG --> PROTO
    SIM --> TYPES
    ADAPT --> PROTO
    ADAPT -.->|"euler · gearbox · compound"| CONFIG
    NODE --> STATEC
    ENGINE --> STATEC
    ENGINE --> FLASHC
    PLAN --> WIRE
    PLAN --> FLASHC
    RISKC --> STATEC
    RISKC --> FLASHC
    ORACLE --> CONFIG
    WATCHC --> CONFIG
    ROUTER --> PLAN
    ROUTER --> FLASHC
    EXECC --> WIRE
    EXECC --> ORACLE
    EXECC --> WATCHC
    OBS --> WATCHC
    BOTC --> ADAPT
    BOTC --> NODE
    BOTC --> ENGINE
    BOTC --> ROUTER
    BOTC --> EXECC
    BOTC --> RISKC
    BOTC --> OBS
    BOTC --> SIM
    RETHC --> BOTC
    REPLAYC --> BOTC
```

---

### Built but not wired

Present in the workspace and tested, but not part of the production process today:

| Piece | Where | State |
|---|---|---|
| In-process revm simulator | `liq-sim`, `DrainJoin::with_sim` | Never attached at startup. Committed-state triggers are simulated on the node instead; triggers that need a pending parent transaction replayed are skipped. |
| Public-mempool ingest | `liq-node` `MempoolProducer`, `liq-oracle` `mempool_oracle` | The rings are created at ExEx install and never fed or read. |
| CEX prices, fusion, aggregator simulation | `liq-oracle` `cex`, `fusion`, `aggsim` | Not referenced by `liq-bot`. Chainlink and derived feeds plus MEV-Share hints price the engine. |
| HTTP alarms (ntfy, Telegram) | `liq-obs` `alert` | Not connected. Alerts, including drift, are log lines. |
| Planned thread layout | `config/cores.toml` | Lists slots such as `liq-oracle-fusion`, `liq-engine-recompute` and `liq-sim-worker-*` that nothing spawns. Only `liq-node-hot` is pinned from it. |
