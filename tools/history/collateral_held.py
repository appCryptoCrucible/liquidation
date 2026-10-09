"""Collateral held, by token, across the lending protocols the bot liquidates
on: what could be liquidated in size, read from the protocols at the head.

    MAINNET_RPC_URL=... python tools/history/collateral_held.py

Reads (all through Multicall3 `aggregate3`, pinned to one block):
- Aave V3 Core / Prime / EtherFi, Spark: each reserve's aToken `totalSupply`
  (supplied, which is collateral or idle) and variable-debt `totalSupply`.
- Aave V2: the same from its `LendingPool`.
- Aave V4: each hub asset's `total_assets` from the registry (its hubs hold
  the supply; a spoke's own share is not read).
- Compound V2 (official Comptroller): each cToken's `totalSupply` ×
  `exchangeRateStored`.
- Euler V2: each registry vault's `totalAssets()`.
- Silo V2: each silo's `getCollateralAssets()`.
- Morpho Blue: collateral is per position, not per market, so markets are
  listed by `borrowed_usd` from the registry (what the collateral backs).

Everything is priced by Aave V3 Core's oracle (USD, 8 decimals) at the
block; a token it does not list is reported unpriced. Writes
`docs/coverage/collateral-held.md`: tokens by USD held (and the protocols
holding them), the pairs large positions would sell, and what is unpriced.
"""

import json
import os
import re
import sys
import time
import urllib.request
from collections import defaultdict
from pathlib import Path

from Crypto.Hash import keccak

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "docs/coverage/collateral-held.md"
MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
AAVE_V3_CORE = "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2"
AAVE_V2_POOL = "0x7d2768de32b0b80b7a3454c06bdac94a69ddc7a9"
COMPOUND_COMPTROLLER = "0x3d9819210a31b4961b30ef54be2aed79b9c9cd3d"
WETH = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"


def sel(sig: str) -> str:
    h = keccak.new(digest_bits=256)
    h.update(sig.encode())
    return "0x" + h.hexdigest()[:8]


S = {k: sel(v) for k, v in {
    "reserves": "getReservesList()", "reserve": "getReserveData(address)",
    "supply": "totalSupply()", "exrate": "exchangeRateStored()", "underlying": "underlying()",
    "markets": "getAllMarkets()", "totalAssets": "totalAssets()", "asset": "asset()",
    "collAssets": "getCollateralAssets()", "decimals": "decimals()", "symbol": "symbol()",
    "provider": "ADDRESSES_PROVIDER()", "oracle": "getPriceOracle()", "price": "getAssetPrice(address)",
}.items()}


class Rpc:
    def __init__(self, url):
        self.url = url
        self.block = None

    def post(self, body):
        data = json.dumps(body).encode()
        wait = 0.5
        for _ in range(12):
            try:
                req = urllib.request.Request(self.url, data=data, headers={"Content-Type": "application/json"})
                with urllib.request.urlopen(req, timeout=90) as r:
                    out = json.loads(r.read())
                if "error" in out and "429" in json.dumps(out["error"]):
                    raise RuntimeError("429")
                return out
            except Exception:
                time.sleep(wait)
                wait = min(wait * 2, 16)
        raise RuntimeError("rpc retries exhausted")

    def call(self, to, data):
        r = self.post({"jsonrpc": "2.0", "id": 1, "method": "eth_call", "params": [{"to": to, "data": data}, self.block]})
        return r.get("result")

    def multicall(self, calls):
        """[(to, data)] -> [returndata or None], 200 per request."""
        out = []
        for i in range(0, len(calls), 200):
            part = calls[i:i + 200]
            # aggregate3((address,bool,bytes)[])
            head = "0x82ad56cb" + (32).to_bytes(32, "big").hex() + len(part).to_bytes(32, "big").hex()
            offs, bodies = [], []
            base = 32 * len(part)
            for to, data in part:
                d = bytes.fromhex(data[2:])
                body = (bytes.fromhex(to[2:].rjust(64, "0")) + (1).to_bytes(32, "big") + (96).to_bytes(32, "big")
                        + len(d).to_bytes(32, "big") + d + b"\0" * ((32 - len(d) % 32) % 32))
                offs.append(base + sum(len(b) for b in bodies))
                bodies.append(body)
            enc = head + "".join(o.to_bytes(32, "big").hex() for o in offs) + "".join(b.hex() for b in bodies)
            raw = self.call(MULTICALL3, enc)
            if not raw:
                out.extend([None] * len(part))
                continue
            b = bytes.fromhex(raw[2:])
            n = int.from_bytes(b[32:64], "big")
            for k in range(n):
                off = int.from_bytes(b[64 + 32 * k:96 + 32 * k], "big") + 64
                ok = int.from_bytes(b[off:off + 32], "big")
                doff = int.from_bytes(b[off + 32:off + 64], "big") + off
                ln = int.from_bytes(b[doff:doff + 32], "big")
                out.append(("0x" + b[doff + 32:doff + 32 + ln].hex()) if ok and ln else None)
        return out


def word(data, i):
    return (data or "0x")[2 + 64 * i:2 + 64 * (i + 1)]


def addr(w):
    return "0x" + w[-40:]


def u(w):
    return int(w, 16) if w and len(w) == 64 else None


def main():
    url = os.environ.get("MAINNET_RPC_URL")
    if not url:
        sys.exit("MAINNET_RPC_URL is not set")
    rpc = Rpc(url)
    head = rpc.post({"jsonrpc": "2.0", "id": 1, "method": "eth_blockNumber", "params": []})["result"]
    rpc.block = hex(int(head, 16) - 2)
    reg = json.loads((ROOT / "registry/registry.json").read_text(encoding="utf-8"))
    tokens = {k.lower(): v for k, v in reg["tokens"].items()}
    cfg = {f: (ROOT / "config/protocols" / f).read_text(encoding="utf-8").lower()
           for f in os.listdir(ROOT / "config/protocols") if f.endswith(".toml")}

    held = defaultdict(lambda: defaultdict(int))   # token -> protocol -> raw amount supplied
    debt = defaultdict(lambda: defaultdict(int))
    # ---- Aave V3 pools and Spark: reserves, aToken / debt token supplies.
    pools = re.findall(r'\[\[pools\]\]\s*address = "(0x[0-9a-f]{40})"', cfg["aave-v3.toml"])
    pools += re.findall(r'\[\[pools\]\]\s*address = "(0x[0-9a-f]{40})"', cfg["spark.toml"])
    names = {p: ("spark" if p in cfg["spark.toml"] else "aave-v3") for p in pools}
    for pool in pools:
        lst = rpc.call(pool, S["reserves"])
        n = u(word(lst, 1)) or 0
        reserves = [addr(word(lst, 2 + i)) for i in range(n)]
        rd = rpc.multicall([(pool, S["reserve"] + r[2:].rjust(64, "0")) for r in reserves])
        atoks, dtoks = [], []
        for r, d in zip(reserves, rd):
            # V3 ReserveData: configuration, liquidityIndex, currentLiquidityRate, variableBorrowIndex,
            # currentVariableBorrowRate, currentStableBorrowRate, lastUpdateTimestamp, id,
            # aTokenAddress (8), stableDebtTokenAddress, variableDebtTokenAddress (10) ...
            atoks.append(addr(word(d, 8)))
            dtoks.append(addr(word(d, 10)))
        sup = rpc.multicall([(a, S["supply"]) for a in atoks] + [(d, S["supply"]) for d in dtoks])
        for i, r in enumerate(reserves):
            held[r][names[pool]] += u(word(sup[i], 0)) or 0
            debt[r][names[pool]] += u(word(sup[n + i], 0)) or 0
    # ---- Aave V2: ReserveData layout differs (aToken at 7, variable debt at 9).
    lst = rpc.call(AAVE_V2_POOL, S["reserves"])
    n = u(word(lst, 1)) or 0
    reserves = [addr(word(lst, 2 + i)) for i in range(n)]
    rd = rpc.multicall([(AAVE_V2_POOL, S["reserve"] + r[2:].rjust(64, "0")) for r in reserves])
    atoks = [addr(word(d, 7)) for d in rd]
    dtoks = [addr(word(d, 9)) for d in rd]
    sup = rpc.multicall([(a, S["supply"]) for a in atoks] + [(d, S["supply"]) for d in dtoks])
    for i, r in enumerate(reserves):
        held[r]["aave-v2"] += u(word(sup[i], 0)) or 0
        debt[r]["aave-v2"] += u(word(sup[n + i], 0)) or 0
    # ---- Aave V4 hubs: the registry's total_assets per hub asset (pinned at generation).
    for v in reg["protocols"].values():
        if v.get("family") == "aave-v4" and v.get("asset") and v.get("total_assets"):
            try:
                held[v["asset"].lower()]["aave-v4"] += int(v["total_assets"])
            except (TypeError, ValueError):
                pass
    # ---- Compound V2 official: cToken supply × exchange rate.
    mk = rpc.call(COMPOUND_COMPTROLLER, S["markets"])
    n = u(word(mk, 1)) or 0
    ctoks = [addr(word(mk, 2 + i)) for i in range(n)]
    res = rpc.multicall([(c, S["supply"]) for c in ctoks] + [(c, S["exrate"]) for c in ctoks] + [(c, S["underlying"]) for c in ctoks])
    for i, c in enumerate(ctoks):
        s_, x, und = u(word(res[i], 0)), u(word(res[n + i], 0)), res[2 * n + i]
        under = addr(word(und, 0)) if und else WETH
        if s_ and x:
            held[under]["compound-v2"] += s_ * x // 10**18
    # ---- Euler V2: every registry vault's totalAssets, by its asset.
    ev = [v for v in reg["protocols"].values() if v.get("family") == "euler-v2" and v.get("asset")]
    res = rpc.multicall([(v["market"], S["totalAssets"]) for v in ev])
    for v, r in zip(ev, res):
        if (a := u(word(r, 0))):
            held[v["asset"].lower()]["euler-v2"] += a
    # ---- Silo V2: each silo's collateral assets, by its asset.
    sv = [v for v in reg["protocols"].values() if v.get("family") == "silo-v2" and v.get("asset")]
    res = rpc.multicall([(v["market"], S["collAssets"]) for v in sv])
    for v, r in zip(sv, res):
        if (a := u(word(r, 0))):
            held[v["asset"].lower()]["silo-v2"] += a
    # ---- Morpho: borrowed USD per collateral token from the registry.
    morpho = defaultdict(float)
    for v in reg["protocols"].values():
        if v.get("family") == "morpho-blue" and v.get("collateral_token") and v.get("borrowed_usd"):
            morpho[v["collateral_token"].lower()] += float(v["borrowed_usd"])

    # ---- Prices and decimals: Aave V3 Core's oracle.
    prov = addr(word(rpc.call(AAVE_V3_CORE, S["provider"]), 0))
    oracle = addr(word(rpc.call(prov, S["oracle"]), 0))
    all_tokens = sorted(set(held) | set(debt))
    px = rpc.multicall([(oracle, S["price"] + t[2:].rjust(64, "0")) for t in all_tokens])
    dec = rpc.multicall([(t, S["decimals"]) for t in all_tokens])
    price = {t: u(word(p, 0)) for t, p in zip(all_tokens, px)}
    decimals = {t: (u(word(d, 0)) if d else (tokens.get(t) or {}).get("decimals")) for t, d in zip(all_tokens, dec)}

    def usd(t, raw):
        p, d = price.get(t), decimals.get(t)
        if not p or d is None:
            return None
        return raw / 10**d * p / 1e8

    rows = []
    unpriced = []
    for t in all_tokens:
        h = sum(held[t].values())
        hv = usd(t, h)
        if hv is None:
            if h:
                unpriced.append((t, h))
            continue
        dv = usd(t, sum(debt[t].values())) or 0.0
        sym = (tokens.get(t) or {}).get("symbol") or t[:10]
        rows.append((hv, dv, sym, t, dict(held[t])))
    rows.sort(reverse=True)
    total = sum(r[0] for r in rows)

    L = ["# Collateral held across the protocols we liquidate on", "",
         f"Read at block {int(rpc.block, 16):,} by `tools/history/collateral_held.py`; priced by Aave V3 Core's oracle. "
         "Supplied balances (collateral or idle) per token; Morpho is listed separately by the debt its collateral backs.", "",
         f"Total priced supply: ${total:,.0f}.", "",
         "## Tokens by USD supplied", "",
         "| token | supplied USD | share | debt USD | held at |", "|---|---:|---:|---:|---|"]
    for hv, dv, sym, t, by in rows[:60]:
        at = ", ".join(f"{k} {usd(t, v) / 1e6:,.0f}M" for k, v in sorted(by.items(), key=lambda x: -x[1]) if usd(t, v))
        L.append(f"| {sym} `{t[:10]}` | {hv:,.0f} | {hv / total:.1%} | {dv:,.0f} | {at} |")
    L += ["", "## Morpho Blue: borrowed USD by collateral token (registry snapshot)", "",
          "| collateral | borrowed USD |", "|---|---:|"]
    for t, v in sorted(morpho.items(), key=lambda x: -x[1])[:30]:
        L.append(f"| {(tokens.get(t) or {}).get('symbol') or t[:10]} `{t[:10]}` | {v:,.0f} |")
    L += ["", "## Unpriced by Aave's oracle (supplied, raw units)", ""]
    L += [f"- {(tokens.get(t) or {}).get('symbol') or t} `{t}`: {h}" for t, h in sorted(unpriced, key=lambda x: -x[1])[:40]] or ["- none"]
    OUT.write_text("\n".join(L) + "\n", encoding="utf-8")
    print(f"{OUT}: {len(rows)} priced tokens, ${total:,.0f} supplied; {len(unpriced)} unpriced")
    for hv, dv, sym, t, by in rows[:25]:
        print(f"  {sym:12} ${hv/1e6:10,.1f}M  debt ${dv/1e6:8,.1f}M  {', '.join(sorted(by))}")


if __name__ == "__main__":
    main()
