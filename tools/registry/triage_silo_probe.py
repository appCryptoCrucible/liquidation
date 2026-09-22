"""Probe every Silo V2/V3 debt view for the 3 C1-admitted silos + full 252-silo sweep.

Resolves which view is the true total debt (C1 uses getCollateralAndDebtTotalsStorage,
C2 uses getDebtAssets) and whether C1 under-admits silos.
"""

from __future__ import annotations

import json
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
    for i in range(0, len(calls), 300):
        part = calls[i : i + 300]
        data = sel("tryAggregate(bool,(address,bytes)[])") + encode(
            ["bool", "(address,bytes)[]"], [False, [(cs(a), d) for a, d in part]]
        )
        raw = w3.eth.call({"to": cs(MULTICALL3), "data": data}, block_identifier=block)
        out.extend(decode(["(bool,bytes)[]"], raw)[0])
    return out


TARGETS = [
    "0xce6ab1c71981e79cd30052c521c162674251018a",
    "0x1de3ba67da79a81bc0c3922689c98550e4bd9bc2",
    "0xf82c626e99c68e7af81f4e6afc8cd25ca13702db",
]
VIEWS = [
    "getCollateralAndDebtTotalsStorage()",
    "getDebtAssets()",
    "getCollateralAssets()",
    "getTotalAssetsStorage(uint8)",
    "utilizationData()",
    "getSiloStorage()",
    "totalAssets()",
    "totalSupply()",
    "asset()",
    "config()",
]
print(f"=== raw view dump @ block {BLOCK} ===")
for t in TARGETS:
    print("\nsilo", t)
    calls = []
    for v in VIEWS:
        d = sel(v) + (encode(["uint8"], [2]) if "uint8" in v else b"")
        calls.append((t, d))
    for v, (ok, raw) in zip(VIEWS, multicall(calls)):
        if not ok:
            print(f"  {v:42} REVERT")
            continue
        words = [int.from_bytes(raw[i : i + 32], "big") for i in range(0, len(raw), 32)]
        print(f"  {v:42} {words}")
    # underlying balance of the silo
    a = decode(["address"], multicall([(t, sel("asset()"))])[0][1])[0]
    bal = multicall([(a, sel("balanceOf(address)") + encode(["address"], [cs(t)]))])[0]
    print(f"  asset={a} balanceOf(silo)={int.from_bytes(bal[1][:32], 'big')}")

# ---- full sweep: does C1 under-admit? -----------------------------------------
print("\n=== full 252-silo sweep: totalsStorage[1] vs getDebtAssets ===")
silos = [p for p in C1["protocols"].values() if p.get("family") == "silo-v2"]
calls = []
for p in silos:
    m = p["market"]
    calls.append((m, sel("getCollateralAndDebtTotalsStorage()")))
    calls.append((m, sel("getDebtAssets()")))
    calls.append((m, sel("asset()")))
res = multicall(calls)
rows = []
for i, p in enumerate(silos):
    okT, rawT = res[3 * i]
    okD, rawD = res[3 * i + 1]
    okA, rawA = res[3 * i + 2]
    tot = int.from_bytes(rawT[32:64], "big") if okT and len(rawT) >= 64 else None
    dbt = int.from_bytes(rawD[:32], "big") if okD and rawD else None
    ast = "0x" + rawA[12:32].hex() if okA and len(rawA) >= 32 else None
    rows.append({"silo": p["market"], "c1_raw": p.get("borrowed_raw"), "totals": tot, "debtAssets": dbt, "asset": ast})

mism = [r for r in rows if r["totals"] != r["c1_raw"]]
print(f"silos where C1 borrowed_raw != on-chain totalsStorage[1]: {len(mism)}")
for r in mism[:15]:
    print("  ", r)
diff = [r for r in rows if r["totals"] is not None and r["debtAssets"] is not None and r["totals"] != r["debtAssets"]]
print(f"silos where totalsStorage[1] != getDebtAssets: {len(diff)}")
for r in sorted(diff, key=lambda x: -(x["debtAssets"] - x["totals"]))[:15]:
    print(f"   {r['silo']} totals={r['totals']} debtAssets={r['debtAssets']} ratio={r['debtAssets']/max(r['totals'],1):.3f}")
nz = [r for r in rows if (r["debtAssets"] or 0) > 0]
print(f"silos with getDebtAssets>0: {len(nz)}; with totalsStorage[1]>0: {sum(1 for r in rows if (r['totals'] or 0) > 0)}")
Path(__file__).parent.joinpath("_triage_silo_sweep.json").write_text(json.dumps(rows, indent=1))
print("wrote _triage_silo_sweep.json")
