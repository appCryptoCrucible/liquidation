#!/usr/bin/env python3
"""Daily registry refresh: proposes, never applies (decision 2026-10-07).

Nothing this script finds reaches the bot without a person. It never writes
`registry/registry.json`, and the bot reads the registry only at startup (no
registry watch, no Uniswap V3 `PoolCreated` discovery).

1. Exits. `discover_exits.py` (V2 pairs, Curve plain / NG / crypto pools) and
   `discover_unwraps.py` (vaults, Curve LPs, Pendle PTs) run on a working
   copy of the registry, with every gate they have. Only tokens already in
   the registry are considered, so no asset id changes.
2. Markets, tokens and Uniswap V3 pools. `discover.py` enumerates every
   protocol family from its on-chain roots into a scratch directory.

Written to `data/review/`:
- `<date>-candidates.json`: every proposed change, one entry each (a pool or
  unwrap added, changed or removed; a new Uniswap V3 pool), with what it is
  and `"approve": false`. Check each on chain, set `"approve": true` on those
  to admit, then run `tools/registry/admit_reviewed.py <that file>` and
  restart the bot once for the batch.
- `<date>-registry.candidate.json` / `<date>-markets.candidate.json`: the
  working registries the entries come from, so admission copies exactly
  what was proposed.
- `<date>.md`: the readable report (markets, tokens, coverage, problems).

Safety valve: a run that would drop more than `--max-drop` (default 10 %) of
an exit kind, and at least `--min-drop` (default 3), is flagged in the
report: a flaky RPC fails gates, it does not retire pools.

Needs `MAINNET_RPC_URL` on an endpoint with Alchemy's transfer index (the
unwrap gates find real holders with `alchemy_getAssetTransfers`), `forge
build` run once in contracts/ (the Pendle probes), and web3 / eth_abi /
requests.

Usage: MAINNET_RPC_URL=... python tools/registry/daily_refresh.py
           [--skip-markets] [--max-drop 0.10]
"""
from __future__ import annotations

import argparse
import datetime
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TOOLS = ROOT / "tools" / "registry"


def load(p: Path) -> dict:
    return json.loads(p.read_text(encoding="utf-8"))


def run(step: str, argv: list[str], log: list[str]) -> bool:
    print(f"== {step}", flush=True)
    r = subprocess.run([sys.executable, *argv], cwd=ROOT, capture_output=True, text=True)
    tail = (r.stdout + r.stderr).strip().splitlines()[-15:]
    for line in tail:
        print("   " + line)
    if r.returncode != 0:
        log.append(f"**{step} failed** (exit {r.returncode}):\n\n```\n" + "\n".join(tail) + "\n```")
        return False
    return True


def exits(reg: dict) -> tuple[dict, dict]:
    pools = {a: p["venue"] for a, p in reg["pools"].items() if p["venue"] != "univ3"}
    unwraps = {t: e["unwrap"] for t, e in reg["tokens"].items() if e.get("unwrap")}
    return pools, unwraps


def sym(reg: dict, t: str) -> str:
    return (reg["tokens"].get(t) or {}).get("symbol") or t


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--registry", type=Path, default=ROOT / "registry" / "registry.json")
    ap.add_argument("--review-dir", type=Path, default=ROOT / "data" / "review")
    ap.add_argument("--skip-markets", action="store_true", help="skip the discover.py market scan")
    ap.add_argument("--max-drop", type=float, default=0.10)
    ap.add_argument("--min-drop", type=int, default=3,
                    help="the valve needs at least this many exits of a kind to drop, as well as --max-drop")
    args = ap.parse_args()
    if not os.environ.get("MAINNET_RPC_URL"):
        print("MAINNET_RPC_URL is required", file=sys.stderr)
        return 2
    live_path: Path = args.registry
    live = load(live_path)
    errors: list[str] = []
    work = Path(tempfile.mkdtemp(prefix="liq-refresh-"))
    try:
        wreg = work / "registry"
        wreg.mkdir()
        for name in ("registry.json", "registry.meta.json", "asset-ids.json"):
            src = live_path.with_name(name)
            if src.exists():
                shutil.copy2(src, wreg / name)
        cand_path = wreg / "registry.json"
        ok = run("exits: V2 / Curve pools", [str(TOOLS / "discover_exits.py"), "--registry", str(cand_path)], errors)
        ok = run("exits: unwraps", [str(TOOLS / "discover_unwraps.py"), "--registry", str(cand_path)], errors) and ok
        cand = load(cand_path)

        # The exit scripts only touch pools and unwrap fields.
        def strip(r: dict) -> dict:
            t = {a: {k: v for k, v in e.items() if k != "unwrap"} for a, e in r["tokens"].items()}
            return {"tokens": t, "protocols": r["protocols"], "oracles": r["oracles"]}
        if strip(cand) != strip(live):
            errors.append("the exit scripts changed tokens / protocols / oracles — nothing applied")
            ok = False

        lp, lu = exits(live)
        cp, cu = exits(cand)
        pools_added = sorted(a for a in cp if a not in lp)
        pools_gone = sorted(a for a in lp if a not in cp)
        un_added = sorted(t for t in cu if t not in lu)
        un_changed = sorted(t for t in cu if t in lu and cu[t] != lu[t])
        un_gone = sorted(t for t in lu if t not in cu)

        valve = []
        for venue in sorted(set(lp.values())):
            have = sum(1 for v in lp.values() if v == venue)
            lost = sum(1 for a in pools_gone if lp[a] == venue)
            if lost >= args.min_drop and lost / have > args.max_drop:
                valve.append(f"{lost} of {have} `{venue}` pools would drop")
        kinds = sorted({u["kind"] for u in lu.values()})
        for k in kinds:
            have = sum(1 for u in lu.values() if u["kind"] == k)
            lost = sum(1 for t in un_gone if lu[t]["kind"] == k)
            if lost >= args.min_drop and lost / have > args.max_drop:
                valve.append(f"{lost} of {have} `{k}` unwraps would drop")

        # Markets / tokens for review.
        new_tokens: list[str] = []
        new_markets: list[str] = []
        gone_markets: list[str] = []
        new_v3: list[str] = []
        on_chain: dict[str, int] = {}
        if not args.skip_markets:
            mdir = work / "markets"
            mdir.mkdir()
            if run("markets: discover.py", [str(TOOLS / "discover.py"), "--out", str(mdir)], errors):
                m = load(mdir / "registry.json")
                new_tokens = sorted(t for t in m["tokens"] if t not in live["tokens"])
                new_markets = sorted(k for k in m["protocols"] if k not in live["protocols"])
                gone_markets = sorted(k for k in live["protocols"] if k not in m["protocols"])
                new_v3 = sorted(a for a, p in m["pools"].items()
                                if p["venue"] == "univ3" and a not in live["pools"])
                mtok = m["tokens"]
                for v in m["protocols"].values():
                    on_chain[v.get("family", "?")] = on_chain.get(v.get("family", "?"), 0) + 1
            else:
                mtok = {}
        else:
            mtok = {}

        today = datetime.date.today().isoformat()
        args.review_dir.mkdir(parents=True, exist_ok=True)
        # The working registries, kept so admission copies exactly what was
        # proposed.
        cand_copy = args.review_dir / f"{today}-registry.candidate.json"
        shutil.copy2(cand_path, cand_copy)
        markets_copy = None
        if not args.skip_markets and (work / "markets" / "registry.json").exists():
            markets_copy = args.review_dir / f"{today}-markets.candidate.json"
            shutil.copy2(work / "markets" / "registry.json", markets_copy)

        def pool_entry(reg: dict, a: str, action: str, src) -> dict:
            p = reg["pools"][a]
            toks = [t for t in [p.get("token0"), p.get("token1")] + p.get("coins", []) if t]
            return {"kind": "pool", "action": action, "address": a, "venue": p.get("venue"),
                    "tokens": [f"{sym(reg, t)} {t}" for t in dict.fromkeys(toks)],
                    "source": str(src) if src else None, "approve": False}

        entries = []
        entries += [pool_entry(cand, a, "add", cand_copy) for a in pools_added]
        entries += [pool_entry(live, a, "remove", None) for a in pools_gone]
        entries += [{"kind": "unwrap", "action": "add", "token": t, "symbol": sym(cand, t),
                     "unwrap": cu[t], "source": str(cand_copy), "approve": False} for t in un_added]
        entries += [{"kind": "unwrap", "action": "change", "token": t, "symbol": sym(cand, t),
                     "from": lu[t], "unwrap": cu[t], "source": str(cand_copy), "approve": False}
                    for t in un_changed]
        entries += [{"kind": "unwrap", "action": "remove", "token": t, "symbol": sym(live, t),
                     "unwrap": lu[t], "approve": False} for t in un_gone]
        if markets_copy:
            mreg = load(markets_copy)
            entries += [pool_entry(mreg, a, "add", markets_copy) for a in new_v3]
        cands_path = args.review_dir / f"{today}-candidates.json"
        cands_path.write_text(json.dumps({
            "date": today,
            "note": "Set approve: true on each entry checked on chain, then "
                    "`python tools/registry/admit_reviewed.py <this file>` and restart the bot once.",
            "safety_valve": valve,
            "problems": errors,
            "entries": entries,
        }, indent=1) + "\n", encoding="utf-8")

        # Report.
        L = [f"# Registry refresh {today}", ""]
        L.append(f"Nothing was applied: {len(entries)} proposed changes are in `{cands_path.name}` for review."
                 if entries else "No changes proposed.")
        L += ["", "## Proposed exits for known tokens (review, then `admit_reviewed.py`)", ""]
        L += [f"- new `{cp[a]}` pool {a}" for a in pools_added] or ["- none"]
        L += [f"- new unwrap {sym(cand, t)} {t} → `{cu[t]['kind']}` into {sym(cand, cu[t]['into'])}" for t in un_added]
        L += [f"- unwrap changed {sym(cand, t)} {t}: {lu[t]} → {cu[t]}" for t in un_changed]
        L += ["", "## Needs your decision (config + restart to add)", ""]
        if args.skip_markets:
            L.append("- market scan skipped (`--skip-markets`)")
        L += [f"- new protocol market `{k}`" for k in new_markets]
        L += [f"- new token {t} ({(mtok.get(t) or {}).get('symbol')}, {(mtok.get(t) or {}).get('decimals')} decimals)"
              for t in new_tokens]
        L += [f"- new Uniswap V3 pool {a} (in the candidates file)" for a in new_v3]
        if not (new_markets or new_tokens or new_v3 or args.skip_markets):
            L.append("- none")
        # Coverage (plan 1B): per family, markets discovery finds on chain
        # against those in the registry. The bot's startup log says which
        # bind and which are priced (`target: coverage`, "protocol prices").
        L += ["", "## Coverage: markets on chain against the registry", ""]
        if on_chain:
            in_reg: dict[str, int] = {}
            for v in live["protocols"].values():
                in_reg[v.get("family", "?")] = in_reg.get(v.get("family", "?"), 0) + 1
            fam_of = lambda k: k.split(":", 1)[0]
            L += ["| family | on chain | in registry | not tracked | gone |", "|---|---:|---:|---:|---:|"]
            for f in sorted(set(on_chain) | set(in_reg)):
                new_f = sum(1 for k in new_markets if fam_of(k) == f)
                gone_f = sum(1 for k in gone_markets if fam_of(k) == f)
                L.append(f"| {f} | {on_chain.get(f, 0)} | {in_reg.get(f, 0)} | {new_f} | {gone_f} |")
            L += ["", "Target: no market on chain untracked. A family the bot has no adapter for (ajna, sky-maker) is listed for completeness."]
        else:
            L.append("- market scan skipped or failed: no coverage counts")
        L += ["", "## No longer admitted", ""]
        L += [f"- `{lp[a]}` pool {a} (proposed removal)" for a in pools_gone]
        L += [f"- unwrap {sym(live, t)} {t} (`{lu[t]['kind']}`) (proposed removal)" for t in un_gone]
        L += [f"- protocol market `{k}` no longer found by discovery" for k in gone_markets]
        if not (pools_gone or un_gone or gone_markets):
            L.append("- none")
        if valve or errors:
            L += ["", "## Problems", ""]
            L += [f"- safety valve: {v} — check the RPC before admitting any removal" for v in valve]
            L += [f"- {e}" for e in errors]
        report = args.review_dir / f"{today}.md"
        report.write_text("\n".join(L) + "\n", encoding="utf-8")
        print(f"report: {report}")
        print(f"proposed, not applied ({cands_path}): +{len(pools_added)} pools, +{len(un_added)} ~{len(un_changed)} "
              f"-{len(un_gone)} unwraps; review: {len(new_markets)} markets, {len(new_tokens)} tokens, "
              f"{len(new_v3)} V3 pools")
        return 0 if (ok and not valve) else 1
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
