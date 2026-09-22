"""Remaining C1 assertions: no admitted market rests on a missing price; admission-boundary
risk from stale accounting; Aave V4 hub attribution for the spokes C1 missed."""

from __future__ import annotations

import json
from pathlib import Path

from eth_abi import decode, encode
from web3 import Web3

ROOT = Path(__file__).resolve().parents[2]
C1 = json.loads((ROOT / "registry" / "registry.json").read_text())
BLOCK = C1["generated_at_block"]
THRESH = 50_000.0
w3 = Web3(Web3.HTTPProvider("https://gateway.tenderly.co/public/mainnet", request_kwargs={"timeout": 60}))
MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
AAVE_V4_HUBS = {
    "core": "0xCca852Bc40e560adC3b1Cc58CA5b55638ce826c9",
    "plus": "0x06002e9c4412CB7814a791eA3666D905871E536A",
    "prime": "0x943827DCA022D0F354a8a8c332dA1e5Eb9f9F931",
    "paxos": "0x62d63197660C080236193CA60b70E49A08E90368",
}
FAM_B = {"morpho-blue", "euler-v2", "silo-v2", "ajna"}


def cs(a):
    return Web3.to_checksum_address(a)


def sel(s):
    return Web3.keccak(text=s)[:4]


def multicall(calls, block=BLOCK):
    out = []
    for i in range(0, len(calls), 300):
        part = calls[i : i + 300]
        data = sel("tryAggregate(bool,(address,bytes)[])") + encode(
            ["bool", "(address,bytes)[]"], [False, [(cs(a), d) for a, d in part]]
        )
        raw = w3.eth.call({"to": cs(MULTICALL3), "data": data}, block_identifier=block)
        out.extend(decode(["(bool,bytes)[]"], raw)[0])
    return out


# ---- A: does any admitted Family B entry lack a real USD figure? ---------------
adm = [(k, p) for k, p in C1["protocols"].items() if p.get("family") in FAM_B and p.get("admitted")]
bad = [(k, p.get("borrowed_usd"), p.get("borrowed_raw")) for k, p in adm
       if p.get("borrowed_usd") is None or (p.get("borrowed_usd") or 0) < THRESH or not p.get("borrowed_raw")]
print(f"A. admitted Family B: {len(adm)}; admitted with null/sub-threshold USD: {len(bad)}")
for b in bad:
    print("   ", b)

# ---- B: admission boundary -- how close is the nearest call? -------------------
near = sorted(
    ((p.get("borrowed_usd") or 0), k, p.get("family"))
    for k, p in C1["protocols"].items()
    if p.get("family") in FAM_B and p.get("borrowed_usd") is not None
)
just_over = [x for x in near if THRESH <= x[0] < THRESH * 1.2]
just_under = [x for x in near if THRESH * 0.8 <= x[0] < THRESH]
print(f"\nB. within +20% of the bar: {len(just_over)}  within -20%: {len(just_under)}")
for x in just_over + just_under:
    print(f"   {x[0]:12,.0f}  {x[2]:12} {x[1][:60]}")

# ---- C: markets C1 could not price at all (coverage gap, correctly not admitted)
unpriced = [
    (k, p)
    for k, p in C1["protocols"].items()
    if p.get("family") in FAM_B and p.get("borrowed_usd") is None and (p.get("borrowed_raw") or 0) > 0
]
print(f"\nC. Family B markets with borrowed_raw>0 but NO usd (unpriced, not admitted): {len(unpriced)}")
from collections import Counter

DEBT = {"morpho-blue": "loan_token", "euler-v2": "asset", "silo-v2": "asset", "ajna": "quote_token"}
c = Counter((C1["tokens"].get(p.get(DEBT[p["family"]]) or "") or {}).get("symbol") for _, p in unpriced)
print("   top unpriced debt-token symbols:", c.most_common(14))
print("   by family:", Counter(p["family"] for _, p in unpriced))

# ---- D: placeholder symbols in C1 tokens --------------------------------------
ph = [a for a, t in C1["tokens"].items() if t.get("symbol") in ("?", "", None)]
print(f"\nD. tokens with placeholder/empty symbol: {len(ph)} (e.g. {ph[:6]})")
print("   '?' specifically:", sum(1 for a, t in C1["tokens"].items() if t.get("symbol") == "?"))

# ---- E: Aave V4 hub attribution for every on-chain spoke ----------------------
print("\nE. Aave V4 spoke -> hub attribution (getSpokeAddress)")
attrib: dict[str, list[str]] = {}
assets: dict[str, str] = {}
for name, hub in AAVE_V4_HUBS.items():
    n = int.from_bytes(w3.eth.call({"to": cs(hub), "data": sel("getAssetCount()")}, block_identifier=BLOCK)[:32], "big")
    for i in range(n):
        u = w3.eth.call(
            {"to": cs(hub), "data": sel("getAssetUnderlyingAndDecimals(uint256)") + encode(["uint256"], [i])},
            block_identifier=BLOCK,
        )
        und = "0x" + u[12:32].hex()
        ns = int.from_bytes(
            w3.eth.call(
                {"to": cs(hub), "data": sel("getSpokeCount(uint256)") + encode(["uint256"], [i])},
                block_identifier=BLOCK,
            )[:32],
            "big",
        )
        if not ns:
            continue
        calls = [
            (hub, sel("getSpokeAddress(uint256,uint256)") + encode(["uint256", "uint256"], [i, j]))
            for j in range(ns)
        ]
        for ok, raw in multicall(calls):
            if ok and len(raw) >= 32:
                sp = ("0x" + raw[12:32].hex()).lower()
                if int(sp, 16):
                    attrib.setdefault(sp, []).append(hub.lower())
                    assets.setdefault(sp, und)
c1_v4 = {k.split(":", 1)[1] for k in C1["protocols"] if k.startswith("aave-v4:")}
missing = sorted(set(attrib) - c1_v4)
print(f"   on-chain spokes={len(attrib)} c1={len(c1_v4)} missing={len(missing)}")
mismatch = [(s, C1["protocols"][f"aave-v4:{s}"]["hub"], attrib[s]) for s in sorted(c1_v4 & set(attrib))
            if C1["protocols"][f"aave-v4:{s}"]["hub"] not in attrib[s]]
print(f"   existing C1 entries whose recorded hub is not an on-chain lister: {len(mismatch)}")
for m in mismatch[:10]:
    print("     ", m)
multi = {s: h for s, h in attrib.items() if len(set(h)) > 1}
print(f"   spokes listed by >1 hub: {len(multi)}")
out = {
    "missing_spokes": {s: {"hubs": sorted(set(attrib[s])), "hub_asset": assets.get(s)} for s in missing},
    "hubs": {k: v.lower() for k, v in AAVE_V4_HUBS.items()},
}
Path(__file__).parent.joinpath("_triage_v4_attrib.json").write_text(json.dumps(out, indent=1))
print("   wrote _triage_v4_attrib.json")
