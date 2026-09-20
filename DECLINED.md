<!-- declined-index date=2026-09-20 -->
# DECLINED.md

Lending declines and deferrals for GUIDE-15. Swap/flash venue exclusions (D08/D09) listed so they are not re-litigated as adapters.

| protocol | status | decision | one-line |
|---|---|---|---|
| Curve lending (LlamaLend) | **declined** | GUIDE-15 §5 | LLAMMA continuously rebalances bands; no HF<1 seize-and-bonus stream. `docs/mechanism/curve.md` |
| LLAMMA | **declined** | GUIDE-01 `SoftLiquidating` | AMM converts coll↔debt as oracle crosses bands; model SoftLiquidating, never quote Liquidatable. `docs/mechanism/llamma.md` |
| Compound V3 (Comet) | **deferred** | **D25** | `absorb` then `buyCollateral` is a second opportunity model (`AuctionOpen`); sequenced after the single-mechanism path. Not a quality decline. `docs/mechanism/compound-v3.md` |
| Sky Lending (Maker CDP) | **deferred** | **D14** | Clipper Dutch `bark`/`take`; `AuctionOpen` + `TimeDescending`. Largest Tier 2; adapter blocked until auction machinery exists. Not a quality decline. `docs/mechanism/sky-lending.md` |
| Balancer (swap + flash) | **excluded venue** | **D08** / **D09** | Not a lending protocol. Swap venues = UniV3 + Curve (+ Kyber Elastic). Flash arenas = Aave, UniV3, UniV4, Morpho, Sky DSS. Balancer out (wound down). |
| UniV4 as **swap** | **excluded venue** | **D08** | Hooks. UniV4 **flash** kept (D09, 07A PoolManager). |

Not declined (15B pages exist): Euler V2 (26 admitted), Silo V2 (3 admitted), Ajna (0 admitted — bar, not mechanism; `docs/mechanism/ajna.md`), Fluid (hard vault-level `liquidate`, not in `registry.json`; `docs/mechanism/fluid.md`), Compound V2 forks (266 instances / 0 Family-B admitted_markets; `docs/mechanism/compound-v2.md`), Liquity V2 / trove forks (hard `batchLiquidateTroves`, SP offset; `docs/mechanism/liquity-v2.md`), Gearbox V3 (hard credit-account liq; `docs/mechanism/gearbox.md`). Tier 1 (Aave V4/V3, Morpho Blue, Spark) is WP 15A, not this file.

Ajna stays on the roster: Dutch `kick`/`take` is hard-auction (D14 defers the machinery), and registry max borrowed_usd = $467.25 < $50k interim bar. Revisit if D27 drops and D14 lifts — still a new ABI / `10R-n`.

Sky is the same D14 gate as Ajna's auction half, but Sky is Family A (35 ilks already enumerated) and DAI-flashable via DSS. The deferral is mechanism sequencing, not discovery.
