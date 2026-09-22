"""Price every Silo with non-zero debt under BOTH debt views, using C1's own price sources.

Answers: does C1 under-admit Silo markets by reading the stale storage total instead of
the interest-accrued getDebtAssets()?
"""

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
FEED_REGISTRY = "0x47Fb2585D2C56Fe188D0E6ec628a38b74fCeeeDf"
USD_DENOM = "0x0000000000000000000000000000000000000348"
ETH_ALIAS = {"0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2": "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE"}
ORACLES = list(
    dict.fromkeys(
        p["price_oracle"]
        for p in C1["protocols"].values()
        if p.get("family") in ("aave-v3", "spark") and p.get("price_oracle")
    )
)


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


def usd_prices(tokens: list[str]) -> dict[str, tuple[float, str]]:
    """C1's exact sources, per-token (no all-or-nothing batching)."""
    px: dict[str, tuple[float, str]] = {}
    calls = []
    for a in tokens:
        args = encode(["address", "address"], [cs(ETH_ALIAS.get(a, a)), cs(USD_DENOM)])
        calls.append((FEED_REGISTRY, sel("latestRoundData(address,address)") + args))
        calls.append((FEED_REGISTRY, sel("decimals(address,address)") + args))
    r = multicall(calls)
    for i, a in enumerate(tokens):
        ok, raw = r[2 * i]
        okd, drw = r[2 * i + 1]
        if ok and len(raw) >= 160 and okd and len(drw) >= 32:
            ans = int.from_bytes(raw[32:64], "big", signed=True)
            if ans > 0:
                px[a] = (ans / 10 ** int.from_bytes(drw[:32], "big"), "feed-registry")
    for o in ORACLES:
        left = [a for a in tokens if a not in px]
        if not left:
            break
        u = multicall([(o, sel("BASE_CURRENCY_UNIT()"))])[0]
        unit = int.from_bytes(u[1][:32], "big") if u[0] and u[1] else 0
        if unit <= 0:
            continue
        rr = multicall([(o, sel("getAssetPrice(address)") + encode(["address"], [cs(a)])) for a in left])
        for a, (ok, raw) in zip(left, rr):
            if ok and len(raw) >= 32:
                ans = int.from_bytes(raw[:32], "big")
                if ans > 0:
                    px[a] = (ans / unit, f"aave-oracle:{o[:10]}")
    return px


silos = [p for p in C1["protocols"].values() if p.get("family") == "silo-v2"]
calls = []
for p in silos:
    m = p["market"]
    calls.append((m, sel("getCollateralAndDebtTotalsStorage()")))
    calls.append((m, sel("getDebtAssets()")))
res = multicall(calls)
live = []
for i, p in enumerate(silos):
    okT, rawT = res[2 * i]
    okD, rawD = res[2 * i + 1]
    tot = int.from_bytes(rawT[32:64], "big") if okT and len(rawT) >= 64 else None
    dbt = int.from_bytes(rawD[:32], "big") if okD and rawD else None
    if (tot or 0) > 0 or (dbt or 0) > 0:
        live.append({**p, "totals": tot, "debtAssets": dbt})

toks = sorted({p["asset"] for p in live if p.get("asset")})
px = usd_prices(toks)
print(f"silos with debt>0: {len(live)}; distinct assets: {len(toks)}; priced: {len(px)}")
print("\nunpriced assets:", [a for a in toks if a not in px])

print(f"\n{'silo':44} {'sym':10} {'stale USD':>16} {'accrued USD':>16}  C1adm  ->stale ->accrued")
n_stale = n_accr = 0
changes = []
for p in sorted(live, key=lambda x: -(x["debtAssets"] or 0)):
    a = p.get("asset")
    t = C1["tokens"].get(a) or {}
    d = t.get("decimals")
    pr = px.get(a)
    if d is None or pr is None:
        print(f"{p['market']:44} {str(t.get('symbol')):10} {'UNPRICED':>16} {'UNPRICED':>16}  {p['admitted']}")
        continue
    su = p["totals"] / 10**d * pr[0]
    au = (p["debtAssets"] or 0) / 10**d * pr[0]
    a_s, a_a = su >= THRESH, au >= THRESH
    n_stale += a_s
    n_accr += a_a
    if a_s != a_a:
        changes.append((p["market"], t.get("symbol"), su, au))
    print(
        f"{p['market']:44} {str(t.get('symbol')):10} {su:16,.2f} {au:16,.2f}  "
        f"{str(p['admitted']):5}  {a_s!s:6} {a_a!s}"
    )
print(f"\nadmitted under stale view: {n_stale} (C1 says 3) | under accrued view: {n_accr}")
print("admission flips:", changes)
