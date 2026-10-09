"""Scan a block range for liquidations on every protocol the bot has an
adapter for, and say which layer would stop the bot on each.

    MAINNET_RPC_URL=... python tools/history/scan_liquidations.py [--from N] [--to N]

Writes `crates/liq-replay/tests/historical/liquidations-scan.json`: every
event in the shape `liquidations.json` uses (so the replay can load it), plus
a `blocked_by` verdict and the repaid amount in USD where it can be priced.
Answers are cached in `data/history-cache/scan.jsonl`, so a rerun or a wider
range asks the node only for what it has not seen.

The verdict is static: it reads the committed config and registry, not the
bot. It says what would stop the bot first, in the order a position meets
the layers:

    untracked   the market is not one the bot follows
    unpriced    tracked, but the bot has no price for it (Morpho: oracle not pinned)
    no-exit     the seized collateral has no pool and no unwrap in the registry
    candidate   none of the above: the replay decides (job, dust, lost, late)

Event signatures are the adapters' own (`crates/liq-adapters/*/src/events.rs`),
except Fluid's, whose adapter decodes none: `LogLiquidate(address,uint256,
uint256,address)` is from Fluid's vault source and is UNVERIFIED here; a scan
that finds no Fluid event says nothing about Fluid.
"""

import argparse
import json
import os
import re
import sys
import time
import urllib.request
from pathlib import Path

from Crypto.Hash import keccak

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "crates/liq-replay/tests/historical/liquidations-scan.json"
CACHE = ROOT / "data/history-cache/scan.jsonl"


def k256(sig: str) -> str:
    h = keccak.new(digest_bits=256)
    h.update(sig.encode())
    return "0x" + h.hexdigest()


TOPICS = {
    k256("LiquidationCall(address,address,address,uint256,uint256,address,bool)"): "aave",
    k256("Liquidate(bytes32,address,address,uint256,uint256,uint256,uint256,uint256)"): "morpho-blue",
    k256("Liquidate(address,address,address,uint256,uint256)"): "euler-v2",
    k256("LiquidateBorrow(address,address,uint256,address,uint256)"): "compound",
    k256("LiquidationCall(address,address,address,uint256,uint256,bool)"): "silo-v2",
    k256("LiquidateCreditAccount(address,address,address,uint256)"): "gearbox",
    k256("PartiallyLiquidateCreditAccount(address,address,address,uint256,uint256,uint256)"): "gearbox",
    k256("Liquidation(uint256,uint256,uint256,uint256,uint256,uint256,uint256,uint256,uint256,uint256)"): "liquity-v2",
    k256(
        "LiquidationCall(uint256,uint256,address,address,bool,uint256,uint256,(int256,int256,uint256),uint256,uint256,uint256)"
    ): "aave-v4",
    k256("LogLiquidate(address,uint256,uint256,address)"): "fluid",
}

AAVE_V2_POOL = "0x7d2768de32b0b80b7a3454c06bdac94a69ddc7a9"
AAVE_V3_CORE = "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2"
WETH = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"


# ── RPC with a disk cache ──────────────────────────────────────────────────

class Rpc:
    def __init__(self, url: str):
        self.url = url
        self.mem = {}
        CACHE.parent.mkdir(parents=True, exist_ok=True)
        if CACHE.exists():
            for line in CACHE.read_text(encoding="utf-8").splitlines():
                try:
                    o = json.loads(line)
                    self.mem[o["k"]] = o["v"]
                except Exception:
                    pass
        self.disk = open(CACHE, "a", encoding="utf-8")
        self.sent = 0

    def _post(self, body):
        data = json.dumps(body).encode()
        wait = 0.2
        for _ in range(20):
            try:
                req = urllib.request.Request(self.url, data=data, headers={"Content-Type": "application/json"})
                with urllib.request.urlopen(req, timeout=90) as r:
                    self.sent += 1
                    out = json.loads(r.read())
                if isinstance(out, list):
                    if any("error" in x and transient(x["error"]) for x in out):
                        raise RuntimeError("throttled")
                elif "error" in out and transient(out["error"]):
                    raise RuntimeError("throttled")
                return out
            except Exception:
                time.sleep(wait)
                wait = min(wait * 2, 30)
        raise RuntimeError("retries exhausted")

    def many(self, calls):
        """[(method, params)] -> [result or {'error': ...}], cached."""
        keys = [f"{m} {json.dumps(p, sort_keys=True)}" for m, p in calls]
        out = [self.mem.get(k) for k in keys]
        miss = [i for i, v in enumerate(out) if v is None]
        for chunk in range(0, len(miss), 10):
            idx = miss[chunk:chunk + 10]
            body = [{"jsonrpc": "2.0", "id": i, "method": calls[i][0], "params": calls[i][1]} for i in idx]
            for r in self._post(body):
                i = r["id"]
                v = {"error": r["error"]} if "error" in r else r["result"]
                out[i] = v
                if "error" not in r:
                    self.mem[keys[i]] = v
                    self.disk.write(json.dumps({"k": keys[i], "v": v}) + "\n")
            self.disk.flush()
        return out

    def one(self, method, params):
        return self.many([(method, params)])[0]


def transient(e) -> bool:
    s = json.dumps(e)
    return any(x in s for x in ("429", "exceeded", "rate limit", "compute units", "-32001", "Unable to complete"))


def word(data: str, i: int) -> str:
    d = data[2:]
    return d[64 * i:64 * (i + 1)]


def addr_of(w: str) -> str:
    return "0x" + w[-40:]


# ── config and registry, as the bot reads them ─────────────────────────────

def load_static():
    reg = json.loads((ROOT / "registry/registry.json").read_text(encoding="utf-8"))
    tokens = {k.lower(): v for k, v in reg["tokens"].items()}
    has_pool = set()
    lp_pools = {k.lower() for k, p in reg["pools"].items() if p.get("venue") == "curve_ng"}
    for p in reg["pools"].values():
        for t in [p["token0"], p["token1"]] + p.get("coins", []):
            has_pool.add(t.lower())
    cfg = {f: (ROOT / "config/protocols" / f).read_text(encoding="utf-8").lower()
           for f in os.listdir(ROOT / "config/protocols") if f.endswith(".toml")}
    aave_pools = set(re.findall(r'address = "(0x[0-9a-f]{40})"', cfg["aave-v3.toml"].split("[[assets]]")[0]))
    aave_pools |= set(re.findall(r'address = "(0x[0-9a-f]{40})"', cfg["spark.toml"].split("[[assets]]")[0]))
    euler_vaults = set(re.findall(r'"(0x[0-9a-f]{40})"', cfg["euler-v2.toml"].split("[[assets]]")[0]))
    ctokens = set(re.findall(r'address = "(0x[0-9a-f]{40})"', cfg["compound-v2.toml"]))
    aave_v2_pools = set(re.findall(r'address = "(0x[0-9a-f]{40})"', cfg["aave-v2.toml"].split("[[assets]]")[0]))
    spokes = set(re.findall(r'\[\[spokes\]\]\naddress = "(0x[0-9a-f]{40})"', cfg["aave-v4.toml"]))
    silo = cfg["silo-v2.toml"]
    morpho_pins = set(re.findall(
        r'\[\[price_sources\]\]\noracle = "(0x[0-9a-f]{40})"\ncollateral = (\d+)\nloan = (\d+)', cfg["morpho-blue.toml"]))
    morpho_assets = dict(re.findall(r'underlying = "(0x[0-9a-f]{40})"\nasset = (\d+)', cfg["morpho-blue.toml"]))
    return dict(tokens=tokens, has_pool=has_pool, lp_pools=lp_pools, aave_pools=aave_pools,
                aave_v2_pools=aave_v2_pools, euler_vaults=euler_vaults,
                ctokens=ctokens, spokes=spokes, silo=silo, morpho_pins=morpho_pins, morpho_assets=morpho_assets)


def exit_of(st, token):
    if token is None:
        return "unknown"
    t = token.lower()
    if t == WETH or t in st["has_pool"]:
        return "pool"
    if t in st["tokens"]:
        u = st["tokens"][t].get("unwrap")
        if not u:
            return "none"
        # As the bot's loader takes them (`liq-bot` `index::build_unwrap`):
        # a Pendle PT needs its YT and SY, a Pendle market its market too,
        # a Curve LP its own pool in the registry holding the coin.
        kind = u.get("kind")
        if kind == "pendle_pt" and not (u.get("yt") and u.get("sy")):
            return "unwrap-refused"
        if kind == "pendle_market" and not (u.get("yt") and u.get("sy") and u.get("market")):
            return "unwrap-refused"
        if kind == "curve_lp" and t not in st["lp_pools"]:
            return "unwrap-refused"
        return "unwrap"
    return "not-in-registry"


# ── per-event decoding ─────────────────────────────────────────────────────

SEL = {
    "asset": "0x38d52e0f",
    "underlying": "0x6f307dc3",
    "idToMarketParams": k256("idToMarketParams(bytes32)")[:10],
    "getAssetPrice": "0xb3596f07",
    "decimals": "0x313ce567",
}


def eth_call(rpc, to, data, block):
    return rpc.one("eth_call", [{"to": to, "data": data}, hex(block)])


def decode(rpc, st, e, fam):
    """(family, tracked, priced, collateral, debt, repaid raw) for one log."""
    em = e["address"].lower()
    t = e["topics"]
    d = e["data"]
    b = int(e["blockNumber"], 16)
    if fam == "aave":
        coll, debt = addr_of(t[1]), addr_of(t[2])
        repaid = int(word(d, 0), 16)
        if em == AAVE_V2_POOL:
            return "aave-v2", em in st["aave_v2_pools"], True, coll, debt, repaid
        return ("aave-v3" if em in st["aave_pools"] else "aave-fork"), em in st["aave_pools"], True, coll, debt, repaid
    if fam == "morpho-blue":
        r = eth_call(rpc, em, SEL["idToMarketParams"] + t[1][2:], b)
        if isinstance(r, dict):
            return fam, True, None, None, None, int(word(d, 0), 16)
        loan, coll, oracle = addr_of(word(r, 0)), addr_of(word(r, 1)), addr_of(word(r, 2))
        la, ca = st["morpho_assets"].get(loan), st["morpho_assets"].get(coll)
        priced = la is not None and ca is not None and (oracle, ca, la) in st["morpho_pins"]
        return fam, True, priced, coll, loan, int(word(d, 0), 16)
    if fam == "euler-v2":
        collv = addr_of(word(d, 0))
        debt = eth_call(rpc, em, SEL["asset"], b)
        coll = eth_call(rpc, collv, SEL["asset"], b)
        debt = None if isinstance(debt, dict) else addr_of(word(debt, 0))
        coll = None if isinstance(coll, dict) else addr_of(word(coll, 0))
        # Every followed vault's own oracle is pinned (`price_sources`); the
        # replay decides whether it priced.
        return fam, em in st["euler_vaults"], None, coll, debt, int(word(d, 1), 16)
    if fam == "compound":
        if len(t) != 1:
            return "compound-like", False, None, None, None, None
        collc = addr_of(word(d, 3))

        def under(c):
            r = eth_call(rpc, c, SEL["underlying"], b)
            return WETH if isinstance(r, dict) else addr_of(word(r, 0))
        tracked = em in st["ctokens"] and collc in st["ctokens"]
        return "compound-v2", tracked, True, under(collc), under(em), int(word(d, 2), 16)
    if fam == "silo-v2":
        return fam, em in st["silo"], None, None, None, int(word(d, 0), 16)
    if fam == "aave-v4":
        return fam, em in st["spokes"], True, None, None, None
    # Discovered at bind: tracked by construction.
    return fam, True, True, None, None, None


def usd_of(rpc, token, raw, block):
    """Repaid amount in USD at the event's block, through Aave V3 Core's oracle.
    None when Core does not price the token."""
    if token is None or raw is None:
        return None
    prov = eth_call(rpc, AAVE_V3_CORE, "0x0542975c", block)  # ADDRESSES_PROVIDER()
    if isinstance(prov, dict):
        return None
    oracle = eth_call(rpc, addr_of(word(prov, 0)), "0xfca513a8", block)  # getPriceOracle()
    if isinstance(oracle, dict):
        return None
    px = eth_call(rpc, addr_of(word(oracle, 0)), SEL["getAssetPrice"] + token[2:].rjust(64, "0"), block)
    dec = eth_call(rpc, token, SEL["decimals"], block)
    if isinstance(px, dict) or isinstance(dec, dict) or len(word(dec, 0)) < 64 or len(word(px, 0)) < 64:
        return None
    p = int(word(px, 0), 16)
    if p == 0:
        return None
    return raw / 10 ** int(word(dec, 0), 16) * p / 1e8


# ── the scan ───────────────────────────────────────────────────────────────

def scan_logs(rpc, lo, hi, width=10, batch=10):
    """Every log with one of TOPICS in [lo, hi]. The free endpoint answers
    `eth_getLogs` over at most 10 blocks, so windows are 10 blocks, sent
    `batch` to a JSON-RPC request."""
    windows = [(b, min(hi, b + width - 1)) for b in range(lo, hi + 1, width)]
    out = []
    for i in range(0, len(windows), batch):
        part = windows[i:i + batch]
        calls = [("eth_getLogs", [{"fromBlock": hex(a), "toBlock": hex(b), "topics": [list(TOPICS)]}])
                 for a, b in part]
        for (a, b), r in zip(part, rpc.many(calls)):
            if isinstance(r, dict) and "error" in r:
                raise RuntimeError(f"getLogs {a}..{b} refused: {r['error']}")
            out.extend(r)
        if (i // batch) % 50 == 0:
            print(f"{part[-1][1] - lo + 1}/{hi - lo + 1} blocks, {len(out)} logs, {rpc.sent} requests",
                  file=sys.stderr, flush=True)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--to", type=int, default=26_118_019)
    ap.add_argument("--from", dest="lo", type=int, default=26_118_019 - 648_000)
    a = ap.parse_args()
    url = os.environ.get("MAINNET_RPC_URL")
    if not url:
        sys.exit("MAINNET_RPC_URL is not set")
    rpc = Rpc(url)
    st = load_static()
    logs = scan_logs(rpc, a.lo, a.to)

    events = []
    for i, e in enumerate(logs):
        fam0 = TOPICS[e["topics"][0].lower()]
        fam, tracked, priced, coll, debt, repaid = decode(rpc, st, e, fam0)
        if not tracked:
            blocked = "untracked"
        elif priced is False:
            blocked = "unpriced"
        elif exit_of(st, coll) in ("none", "not-in-registry", "unwrap-refused"):
            blocked = "no-exit"
        else:
            blocked = "candidate"
        b = int(e["blockNumber"], 16)
        events.append({
            "family": fam,
            "block": b,
            "tx": e["transactionHash"],
            "tx_index": int(e["transactionIndex"], 16),
            "log_index": int(e["logIndex"], 16),
            "emitter": e["address"].lower(),
            "topics": e["topics"],
            "data": e["data"],
            "collateral": coll,
            "debt": debt,
            "collateral_exit": exit_of(st, coll),
            "repaid_usd": usd_of(rpc, debt, repaid, b),
            "blocked_by": blocked,
        })
        if i % 50 == 0:
            print(f"\rclassified {i + 1}/{len(logs)}, {rpc.sent} requests", end="", file=sys.stderr)
    print(file=sys.stderr)

    OUT.write_text(json.dumps({
        "note": "Liquidations of every protocol with a bot adapter, found by scan_liquidations.py "
                "(eth_getLogs over the adapters' own event topics, any emitter). `blocked_by` is "
                "static, from the committed config and registry; `repaid_usd` is through Aave V3 "
                "Core's oracle at the event's block, null where Core does not price the debt.",
        "from": a.lo, "to": a.to,
        "liquidations": events,
    }, indent=1), encoding="utf-8")

    # Summary: events and repaid USD per family and verdict.
    from collections import defaultdict
    agg = defaultdict(lambda: [0, 0.0, 0])
    for e in events:
        k = (e["family"], e["blocked_by"])
        agg[k][0] += 1
        if e["repaid_usd"] is not None:
            agg[k][1] += e["repaid_usd"]
        else:
            agg[k][2] += 1
    print(f"{len(events)} events in blocks {a.lo}..{a.to}")
    print(f"{'family':16} {'blocked by':11} {'events':>7} {'repaid USD':>14} {'unpriced':>9}")
    for (f, v), (n, usd, nop) in sorted(agg.items(), key=lambda x: -x[1][1]):
        print(f"{f:16} {v:11} {n:7d} {usd:14,.0f} {nop:9d}")


if __name__ == "__main__":
    main()
