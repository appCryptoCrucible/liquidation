#!/usr/bin/env python3
"""Daily registry refresh for the running bot (ops/systemd/liq-discovery.timer).

1. Exits, applied automatically. `discover_exits.py` (V2 pairs, Curve plain /
   NG / crypto pools) and `discover_unwraps.py` (vaults, Curve LPs, Pendle PTs)
   run on a working copy of `registry/registry.json`, with every gate they
   have. Only tokens already in the registry are considered, so no asset id
   changes. When exits changed, the live file is replaced atomically and the
   running bot's registry watch (`liq-bot/src/registry_watch.rs`) adds them
   within ~15 s — no restart.
2. Markets and tokens, for review only. `discover.py` enumerates every
   protocol family from its on-chain roots into a scratch directory; new
   protocol markets, new tokens and new Uniswap V3 pools are listed in the
   report and never written to the live registry — adding them means a
   config change and a restart, decided by a person.

The report is `data/review/<YYYY-MM-DD>.md`: what was applied, what needs a
decision, what disappeared, and any step that failed.

Safety valve: if a run would drop more than `--max-drop` (default 10 %) of
an exit kind that exists today, and at least `--min-drop` (default 3) of
them, nothing is applied (a flaky RPC fails gates, it does not retire pools)
and the report says so. A pool or two dying in a small set still applies.

Needs `MAINNET_RPC_URL` on an endpoint with Alchemy's transfer index (the
unwrap gates find real holders with `alchemy_getAssetTransfers`), `forge
build` run once in contracts/ (the Pendle probes), and web3 / eth_abi /
requests.

Usage: MAINNET_RPC_URL=... python tools/registry/daily_refresh.py
           [--dry-run] [--skip-markets] [--max-drop 0.10]
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


def atomic_write(path: Path, text: str) -> None:
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    os.replace(tmp, path)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--registry", type=Path, default=ROOT / "registry" / "registry.json")
    ap.add_argument("--review-dir", type=Path, default=ROOT / "data" / "review")
    ap.add_argument("--dry-run", action="store_true", help="report only; never touch the live file")
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
            else:
                mtok = {}
        else:
            mtok = {}

        changed = bool(pools_added or pools_gone or un_added or un_changed or un_gone)
        applied = changed and ok and not valve and not args.dry_run
        if applied:
            atomic_write(live_path.with_name("registry.meta.json"),
                         (wreg / "registry.meta.json").read_text(encoding="utf-8"))
            atomic_write(live_path, cand_path.read_text(encoding="utf-8"))

        # Report.
        today = datetime.date.today().isoformat()
        L = [f"# Registry refresh {today}", ""]
        if applied:
            L.append("Exits were **applied** to the live registry; the running bot adds them without a restart.")
        elif changed and args.dry_run:
            L.append("Dry run: exit changes below were **not applied**.")
        elif changed:
            L.append("Exit changes below were **not applied** (see Problems).")
        else:
            L.append("No exit changes.")
        L += ["", "## Applied automatically (exits for known tokens)", ""]
        L += [f"- new `{cp[a]}` pool {a}" for a in pools_added] or ["- none"]
        L += [f"- new unwrap {sym(cand, t)} {t} → `{cu[t]['kind']}` into {sym(cand, cu[t]['into'])}" for t in un_added]
        L += [f"- unwrap changed {sym(cand, t)} {t}: {lu[t]} → {cu[t]}" for t in un_changed]
        L += ["", "## Needs your decision (config + restart to add)", ""]
        if args.skip_markets:
            L.append("- market scan skipped (`--skip-markets`)")
        L += [f"- new protocol market `{k}`" for k in new_markets]
        L += [f"- new token {t} ({(mtok.get(t) or {}).get('symbol')}, {(mtok.get(t) or {}).get('decimals')} decimals)"
              for t in new_tokens]
        L += [f"- new Uniswap V3 pool {a} (pools created on chain are already followed live)" for a in new_v3]
        if not (new_markets or new_tokens or new_v3 or args.skip_markets):
            L.append("- none")
        L += ["", "## No longer admitted", ""]
        L += [f"- `{lp[a]}` pool {a} (stays routed until a restart)" for a in pools_gone]
        L += [f"- unwrap {sym(live, t)} {t} (`{lu[t]['kind']}`) — removed from the running book" for t in un_gone]
        L += [f"- protocol market `{k}` no longer found by discovery" for k in gone_markets]
        if not (pools_gone or un_gone or gone_markets):
            L.append("- none")
        if valve or errors:
            L += ["", "## Problems", ""]
            L += [f"- safety valve: {v} — nothing applied" for v in valve]
            L += [f"- {e}" for e in errors]
        args.review_dir.mkdir(parents=True, exist_ok=True)
        report = args.review_dir / f"{today}.md"
        report.write_text("\n".join(L) + "\n", encoding="utf-8")
        print(f"report: {report}")
        print(f"applied: {applied}; +{len(pools_added)} pools, +{len(un_added)} ~{len(un_changed)} "
              f"-{len(un_gone)} unwraps; review: {len(new_markets)} markets, {len(new_tokens)} tokens, "
              f"{len(new_v3)} V3 pools")
        return 0 if (ok and not valve) else 1
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
