"""Admit the registry changes a person approved from a daily refresh.

    python tools/registry/admit_reviewed.py data/review/<date>-candidates.json [--dry-run]

`daily_refresh.py` proposes; this applies only the entries whose `approve`
is true, into `registry/registry.json`, copying each pool or unwrap exactly
as the refresh's saved candidate registry (`source`) recorded it. Nothing
else in the registry changes: tokens, protocols and oracles are checked
untouched. The bot reads the registry at startup, so one restart takes the
whole batch.

Refused, the file left as it was:
- an approved entry whose `source` file is missing or no longer holds it;
- an added pool whose tokens are not all registry tokens (asset ids are
  fixed at startup: a new token is a separate decision);
- an unwrap into a token that is not in the registry.
"""

import argparse
import datetime
import json
import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def load(p: Path) -> dict:
    return json.loads(p.read_text(encoding="utf-8"))


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("candidates", type=Path)
    ap.add_argument("--registry", type=Path, default=ROOT / "registry" / "registry.json")
    ap.add_argument("--dry-run", action="store_true", help="say what would change; write nothing")
    args = ap.parse_args()

    cands = load(args.candidates)
    reg = load(args.registry)
    before = json.dumps({k: reg[k] for k in ("protocols", "oracles")}, sort_keys=True)
    tokens_before = {t: {k: v for k, v in e.items() if k != "unwrap"} for t, e in reg["tokens"].items()}
    sources: dict[str, dict] = {}

    def source(e: dict) -> dict:
        s = e.get("source")
        if not s:
            raise SystemExit(f"approved entry has no source: {e}")
        if s not in sources:
            p = Path(s)
            if not p.exists():
                raise SystemExit(f"source {s} is missing")
            sources[s] = load(p)
        return sources[s]

    done: list[str] = []
    for e in cands.get("entries", []):
        if e.get("approve") is not True:
            continue
        kind, action = e["kind"], e["action"]
        if kind == "pool":
            a = e["address"]
            if action == "add":
                p = source(e)["pools"].get(a)
                if p is None:
                    raise SystemExit(f"pool {a} is not in its source")
                toks = [t for t in [p.get("token0"), p.get("token1")] + p.get("coins", []) if t]
                missing = [t for t in toks if t not in reg["tokens"]]
                if missing:
                    raise SystemExit(f"pool {a} holds tokens outside the registry: {missing}")
                reg["pools"][a] = p
                done.append(f"+ pool {a} ({p.get('venue')})")
            elif action == "remove":
                if reg["pools"].pop(a, None) is not None:
                    done.append(f"- pool {a}")
        elif kind == "unwrap":
            t = e["token"]
            if t not in reg["tokens"]:
                raise SystemExit(f"unwrap for {t}: not a registry token")
            if action in ("add", "change"):
                u = source(e)["tokens"].get(t, {}).get("unwrap")
                if u is None:
                    raise SystemExit(f"unwrap for {t} is not in its source")
                if u.get("into") not in reg["tokens"]:
                    raise SystemExit(f"unwrap for {t} goes into {u.get('into')}, not a registry token")
                reg["tokens"][t]["unwrap"] = u
                done.append(f"{'+' if action == 'add' else '~'} unwrap {e.get('symbol')} {t} -> {u.get('kind')}")
            elif action == "remove":
                if reg["tokens"][t].pop("unwrap", None) is not None:
                    done.append(f"- unwrap {e.get('symbol')} {t}")
        else:
            raise SystemExit(f"unknown entry kind {kind}")

    after = json.dumps({k: reg[k] for k in ("protocols", "oracles")}, sort_keys=True)
    tokens_after = {t: {k: v for k, v in e.items() if k != "unwrap"} for t, e in reg["tokens"].items()}
    if after != before or tokens_after != tokens_before:
        raise SystemExit("protocols, oracles or token entries would change: refused")

    for line in done:
        print(line)
    if not done:
        print("nothing approved")
        return 0
    if args.dry_run:
        print(f"dry run: {len(done)} changes not written")
        return 0
    tmp = args.registry.with_name(args.registry.name + ".tmp")
    tmp.write_text(json.dumps(reg, indent=1) + "\n", encoding="utf-8")
    os.replace(tmp, args.registry)
    log = ROOT / "data" / "review" / "admitted.log"
    log.parent.mkdir(parents=True, exist_ok=True)
    with log.open("a", encoding="utf-8") as f:
        f.write(f"{datetime.datetime.now().isoformat(timespec='seconds')} {args.candidates}\n")
        f.writelines(f"  {line}\n" for line in done)
    print(f"{len(done)} changes written to {args.registry}; restart the bot to take them")
    return 0


if __name__ == "__main__":
    sys.exit(main())
