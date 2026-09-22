"""WP C2 triage: isolate the exact C1-vs-C2 admission deltas for on-chain re-check.

Read-only. Writes tools/registry/_triage_sets.json (scratch) for the RPC verifier.
"""

from __future__ import annotations

import json
import sys
from collections import Counter, defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
C1 = json.loads((ROOT / "registry" / "registry.json").read_text())
C2 = json.loads((ROOT / "registry" / "registry.rederived.json").read_text())


def lc(a: str | None) -> str | None:
    return a.lower() if isinstance(a, str) else a


def ident(p: dict, key: str) -> str:
    return lc(p.get("market_id") or p.get("market") or p.get("silo_config") or p.get("comptroller") or key)


def index(reg: dict) -> tuple[dict, dict]:
    by_fam: dict[str, dict[str, dict]] = defaultdict(dict)
    for k, p in (reg.get("protocols") or {}).items():
        by_fam[p.get("family") or "?"][ident(p, k)] = p
    return by_fam, {lc(a): t for a, t in (reg.get("tokens") or {}).items()}


F1, T1 = index(C1)
F2, T2 = index(C2)

out: dict[str, object] = {}
print("family        | C1 n  C2 n | C1 adm C2 adm | onlyC1adm onlyC2adm")
for fam in sorted(set(F1) | set(F2)):
    a1 = {i for i, p in F1.get(fam, {}).items() if p.get("admitted")}
    a2 = {i for i, p in F2.get(fam, {}).items() if p.get("admitted")}
    print(
        f"{fam:13} | {len(F1.get(fam, {})):5} {len(F2.get(fam, {})):5} |"
        f" {len(a1):6} {len(a2):6} | {len(a1 - a2):9} {len(a2 - a1):9}"
    )

# ---- 1/2: Morpho + Euler: markets C1 admits that C2 does not -------------------
for fam, tok_field in (("morpho-blue", "loan_token"), ("euler-v2", "asset"), ("ajna", "quote_token")):
    a1 = {i for i, p in F1.get(fam, {}).items() if p.get("admitted")}
    a2 = {i for i, p in F2.get(fam, {}).items() if p.get("admitted")}
    rows = []
    for i in sorted(a1 - a2):
        p1, p2 = F1[fam][i], F2.get(fam, {}).get(i) or {}
        rows.append(
            {
                "id": i,
                "debt_token": lc(p1.get(tok_field)),
                "symbol": (T1.get(lc(p1.get(tok_field))) or {}).get("symbol"),
                "decimals": (T1.get(lc(p1.get(tok_field))) or {}).get("decimals"),
                "c1_borrowed_raw": p1.get("borrowed_raw"),
                "c1_borrowed_usd": p1.get("borrowed_usd"),
                "c2_borrowed_raw": p2.get("borrowed_raw"),
                "c2_borrowed_usd": p2.get("borrowed_usd"),
                "c2_present": bool(p2),
            }
        )
    out[f"only_c1_admitted:{fam}"] = rows
    rows2 = []
    for i in sorted(a2 - a1):
        p2, p1 = F2[fam][i], F1.get(fam, {}).get(i) or {}
        rows2.append(
            {
                "id": i,
                "debt_token": lc(p2.get(tok_field)),
                "symbol": (T2.get(lc(p2.get(tok_field))) or {}).get("symbol"),
                "decimals": (T2.get(lc(p2.get(tok_field))) or {}).get("decimals"),
                "c2_borrowed_raw": p2.get("borrowed_raw"),
                "c2_borrowed_usd": p2.get("borrowed_usd"),
                "c1_borrowed_raw": p1.get("borrowed_raw"),
                "c1_borrowed_usd": p1.get("borrowed_usd"),
                "c1_present": bool(p1),
            }
        )
    out[f"only_c2_admitted:{fam}"] = rows2
    print(f"\n{fam}: onlyC1={len(rows)} onlyC2={len(rows2)}")
    for r in rows[:30]:
        print("  C1+", r["id"][:18], r["symbol"], r["c1_borrowed_raw"], "->", r["c1_borrowed_usd"],
              "| c2raw", r["c2_borrowed_raw"], "c2usd", r["c2_borrowed_usd"])
    for r in rows2[:30]:
        print("  C2+", r["id"][:18], r["symbol"], r["c2_borrowed_raw"], "->", r["c2_borrowed_usd"],
              "| c1raw", r["c1_borrowed_raw"], "c1usd", r["c1_borrowed_usd"])

# ---- 5: Silo (different identity key: C1 per-silo, C2 per-config) --------------
silo1 = F1.get("silo-v2", {})
adm1 = [p for p in C1["protocols"].values() if p.get("family") == "silo-v2" and p.get("admitted")]
silo_rows = []
for p in adm1:
    cfg = lc(p.get("silo_config"))
    p2 = F2.get("silo-v2", {}).get(cfg) or {}
    silo_rows.append(
        {
            "silo": lc(p.get("market")),
            "silo_config": cfg,
            "asset": lc(p.get("asset")),
            "symbol": (T1.get(lc(p.get("asset"))) or {}).get("symbol"),
            "decimals": (T1.get(lc(p.get("asset"))) or {}).get("decimals"),
            "c1_borrowed_raw": p.get("borrowed_raw"),
            "c1_borrowed_usd": p.get("borrowed_usd"),
            "c2_config_borrowed_raw": p2.get("borrowed_raw"),
            "c2_config_borrowed_usd": p2.get("borrowed_usd"),
            "c2_config_present": bool(p2),
        }
    )
out["only_c1_admitted:silo-v2"] = silo_rows
print("\nsilo-v2 C1-admitted:")
for r in silo_rows:
    print("  ", r["silo"], r["symbol"], r["c1_borrowed_raw"], "->", r["c1_borrowed_usd"],
          "| cfg", r["silo_config"], "c2raw", r["c2_config_borrowed_raw"])

# ---- 4: Aave V4 spokes only in C2 ---------------------------------------------
s1 = set(F1.get("aave-v4", {}))
s2 = set(F2.get("aave-v4", {}))
only2 = sorted(s2 - s1)
out["only_c2:aave-v4"] = [{"addr": a, "entry": F2["aave-v4"][a]} for a in only2]
print(f"\naave-v4 only-C2 spokes: {len(only2)}")

# ---- 6: symbol collisions ------------------------------------------------------
out["c1_tokens"] = {a: {"symbol": t.get("symbol"), "decimals": t.get("decimals")} for a, t in T1.items()}

# which C1 protocol entries reference collision addresses
COLLIDERS = {
    "0x3d4762b4bb4b4c922377fe5b887e900d7fb64cdf": "USDT",
    "0x564fa4e3eee769b911643ddc637e8fb8489cc1a2": "USDC",
    "0x59d5c6f5fdf8fa53c6fe44bb053b41ab1eaaaa23": "USDC",
    "0x89d24a6b4ccb1b6faa2625fe562bdd9a23260359": "DAI",
    "0xb6a9c3375b3a57da78f7c467bd7b91cb8665a61e": "USDC",
    "0xcbfb9b444d9735c345df3a0f66cd89bd741692e9": "USDC",
}
print("\ncollision-address references in C1:")
for a, sym in COLLIDERS.items():
    refs = [
        (k, p.get("family"), p.get("admitted"), p.get("borrowed_usd"))
        for k, p in C1["protocols"].items()
        if a in json.dumps(p).lower()
    ]
    print(f"  {a} ({sym}) in_tokens={a in T1} c1_symbol={(T1.get(a) or {}).get('symbol')!r} refs={len(refs)}")
    for r in refs[:6]:
        print("     ", r[0][:60], r[1], "admitted=", r[2], "usd=", r[3])
    out[f"collision:{a}"] = {"symbol_claimed": sym, "refs": [r[0] for r in refs]}

(Path(__file__).parent / "_triage_sets.json").write_text(json.dumps(out, indent=1))
print("\nwrote _triage_sets.json")
