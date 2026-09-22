"""Full §4b identity scan over C1's registry: every token whose on-chain symbol matches a
canonical Uniswap-list symbol at a DIFFERENT address, with its blast radius
(admitted markets, debt vs collateral side, UniV3 pools).
"""

from __future__ import annotations

import json
import urllib.request
from pathlib import Path

from eth_abi import decode, encode
from web3 import Web3

ROOT = Path(__file__).resolve().parents[2]
C1 = json.loads((ROOT / "registry" / "registry.json").read_text())
BLOCK = C1["generated_at_block"]
w3 = Web3(Web3.HTTPProvider("https://gateway.tenderly.co/public/mainnet", request_kwargs={"timeout": 60}))
MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"


def cs(a):
    return Web3.to_checksum_address(a)


def sel(s):
    return Web3.keccak(text=s)[:4]


def multicall(calls, block=BLOCK):
    out = []
    for i in range(0, len(calls), 250):
        part = calls[i : i + 250]
        data = sel("tryAggregate(bool,(address,bytes)[])") + encode(
            ["bool", "(address,bytes)[]"], [False, [(cs(a), d) for a, d in part]]
        )
        raw = w3.eth.call({"to": cs(MULTICALL3), "data": data}, block_identifier=block)
        out.extend(decode(["(bool,bytes)[]"], raw)[0])
    return out


req = urllib.request.Request("https://tokens.uniswap.org", headers={"User-Agent": "Mozilla/5.0"})
with urllib.request.urlopen(req, timeout=60) as r:
    tl = json.load(r)
CANON = {t["symbol"]: t["address"].lower() for t in tl["tokens"] if t.get("chainId") == 1}
CANON_ADDRS = set(CANON.values())
print(f"canonical mainnet entries: {len(CANON)}")

toks = sorted(C1["tokens"])
print(f"C1 tokens: {len(toks)}")
# collisions purely from C1's own recorded symbols, then verified on-chain
cand = [a for a in toks if (C1["tokens"][a].get("symbol") in CANON) and a not in CANON_ADDRS]
print(f"candidate symbol collisions (C1 symbol in canonical list, address differs): {len(cand)}")

calls = [(a, sel("symbol()")) for a in cand] + [(a, sel("totalSupply()")) for a in cand]
res = multicall(calls)
verified = []
for i, a in enumerate(cand):
    ok, raw = res[i]
    s_on = None
    if ok and raw:
        try:
            s_on = decode(["string"], raw)[0]
        except Exception:
            s_on = raw.rstrip(b"\x00").decode("utf8", "replace").strip("\x00")
    okt, rawt = res[len(cand) + i]
    ts = int.from_bytes(rawt[:32], "big") if okt and rawt else None
    if s_on in CANON and CANON[s_on] != a:
        verified.append({"addr": a, "symbol": s_on, "canonical": CANON[s_on], "total_supply": ts})

print(f"on-chain-verified collisions: {len(verified)}")

FAM_B = {"morpho-blue", "euler-v2", "silo-v2", "ajna"}
DEBT = {"morpho-blue": "loan_token", "euler-v2": "asset", "silo-v2": "asset", "ajna": "quote_token"}
pools = C1.get("pools") or {}

rows = []
for v in verified:
    a = v["addr"]
    as_debt, as_coll, as_debt_adm, as_coll_adm = [], [], [], []
    for k, p in C1["protocols"].items():
        fam = p.get("family")
        if a == (p.get(DEBT.get(fam, "")) or ""):
            as_debt.append(k)
            if p.get("admitted"):
                as_debt_adm.append((k, p.get("borrowed_usd")))
        if a == (p.get("collateral_token") or ""):
            as_coll.append(k)
            if p.get("admitted"):
                as_coll_adm.append((k, p.get("borrowed_usd")))
    pl = [pa for pa, q in pools.items() if a in (q.get("token0"), q.get("token1"))]
    rows.append({**v, "as_debt": len(as_debt), "as_coll": len(as_coll),
                 "admitted_as_debt": as_debt_adm, "admitted_as_coll": as_coll_adm, "univ3_pools": pl})

rows.sort(key=lambda r: -(len(r["admitted_as_debt"]) + len(r["admitted_as_coll"])))
print(f"\n{'addr':44}{'sym':10}{'canonical':44}dbt coll  adm_dbt adm_coll pools")
for r in rows:
    print(f"{r['addr']:44}{r['symbol']:10}{r['canonical']:44}{r['as_debt']:3} {r['as_coll']:4}  "
          f"{len(r['admitted_as_debt']):7} {len(r['admitted_as_coll']):8} {len(r['univ3_pools'])}")

hot = [r for r in rows if r["admitted_as_debt"] or r["admitted_as_coll"] or r["univ3_pools"]]
print("\n=== collisions that touch an ADMITTED market or a routable UniV3 pool ===")
print(json.dumps(hot, indent=1))
Path(__file__).parent.joinpath("_triage_identity.json").write_text(json.dumps(rows, indent=1))
print("wrote _triage_identity.json")
