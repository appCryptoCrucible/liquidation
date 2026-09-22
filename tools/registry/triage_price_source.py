"""For every C1-ADMITTED Family B market, resolve the exact USD price source of its debt
token and flag symbol-collision impostors that received a price.

A priced impostor is the dangerous-wrong-token class: the market gets admitted on a
fabricated-looking USD figure derived from an unrelated feed.
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
FAM_B = {"morpho-blue", "euler-v2", "silo-v2", "ajna"}
TOKFIELD = {"morpho-blue": "loan_token", "euler-v2": "asset", "silo-v2": "asset", "ajna": "quote_token"}


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


# ---- canonical Uniswap token list (symbol -> address) --------------------------
req = urllib.request.Request(
    "https://tokens.uniswap.org", headers={"User-Agent": "Mozilla/5.0", "Accept": "application/json"}
)
with urllib.request.urlopen(req, timeout=60) as r:
    tl = json.load(r)
CANON: dict[str, str] = {}
for t in tl["tokens"]:
    if t.get("chainId") == 1:
        CANON[t["symbol"]] = t["address"].lower()

admitted = [
    p for p in C1["protocols"].values() if p.get("family") in FAM_B and p.get("admitted")
]
toks = sorted({(p.get(TOKFIELD[p["family"]]) or "") for p in admitted} - {""})
print(f"C1-admitted Family B markets: {len(admitted)}; distinct debt tokens: {len(toks)}")

# on-chain symbol for each
calls = [(a, sel("symbol()")) for a in toks] + [(a, sel("decimals()")) for a in toks]
r = multicall(calls)
sym: dict[str, str] = {}
dec: dict[str, int] = {}
for i, a in enumerate(toks):
    ok, raw = r[i]
    if ok and raw:
        try:
            sym[a] = decode(["string"], raw)[0]
        except Exception:
            sym[a] = raw.rstrip(b"\x00").decode("utf8", "replace").strip("\x00")
    okd, rawd = r[len(toks) + i]
    if okd and rawd:
        dec[a] = int.from_bytes(rawd[:32], "big")

# price + source per token
src: dict[str, tuple[float, str]] = {}
calls = []
for a in toks:
    args = encode(["address", "address"], [cs(ETH_ALIAS.get(a, a)), cs(USD_DENOM)])
    calls.append((FEED_REGISTRY, sel("latestRoundData(address,address)") + args))
    calls.append((FEED_REGISTRY, sel("decimals(address,address)") + args))
rr = multicall(calls)
for i, a in enumerate(toks):
    ok, raw = rr[2 * i]
    okd, drw = rr[2 * i + 1]
    if ok and len(raw) >= 160 and okd and len(drw) >= 32:
        ans = int.from_bytes(raw[32:64], "big", signed=True)
        if ans > 0:
            src[a] = (ans / 10 ** int.from_bytes(drw[:32], "big"), "chainlink-feed-registry")
for o in ORACLES:
    left = [a for a in toks if a not in src]
    if not left:
        break
    u = multicall([(o, sel("BASE_CURRENCY_UNIT()"))])[0]
    unit = int.from_bytes(u[1][:32], "big") if u[0] and u[1] else 0
    if unit <= 0:
        continue
    res = multicall([(o, sel("getAssetPrice(address)") + encode(["address"], [cs(a)])) for a in left])
    for a, (ok, raw) in zip(left, res):
        if ok and len(raw) >= 32:
            ans = int.from_bytes(raw[:32], "big")
            if ans > 0:
                src[a] = (ans / unit, f"aave-oracle:{o}")

print("\n=== admitted-market debt tokens: identity vs canonical list ===")
bad = []
for a in toks:
    s = sym.get(a, "?")
    canon = CANON.get(s)
    collision = canon is not None and canon != a
    n = sum(1 for p in admitted if (p.get(TOKFIELD[p["family"]]) or "") == a)
    usd = sum(p.get("borrowed_usd") or 0 for p in admitted if (p.get(TOKFIELD[p["family"]]) or "") == a)
    px, source = src.get(a, (None, "UNPRICED"))
    flag = "  <== SYMBOL COLLISION, PRICED" if collision and px else ("  <== collision, unpriced" if collision else "")
    print(f"{a} {s:12} dec={dec.get(a)} px={px} src={source[:34]:34} mkts={n:3} usd={usd:,.0f}{flag}")
    if collision and px:
        bad.append(
            {
                "token": a,
                "symbol": s,
                "canonical": canon,
                "price": px,
                "source": source,
                "admitted_markets": [
                    k
                    for k, p in C1["protocols"].items()
                    if p.get("family") in FAM_B
                    and p.get("admitted")
                    and (p.get(TOKFIELD[p["family"]]) or "") == a
                ],
                "admitted_usd": usd,
            }
        )

print("\n=== DANGEROUS: admitted on a symbol-colliding token ===")
print(json.dumps(bad, indent=1))
Path(__file__).parent.joinpath("_triage_price_source.json").write_text(
    json.dumps({"bad": bad, "prices": {k: list(v) for k, v in src.items()}, "symbols": sym}, indent=1)
)
