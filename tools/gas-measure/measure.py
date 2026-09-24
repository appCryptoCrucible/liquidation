"""Measure real per-protocol liquidation gas and per-venue swap gas on mainnet.

Scans recent blocks for each tracked protocol's liquidation event, replays
each transaction locally with `cast run --quick` (no trace API needed), and
records:

  * the gas of the call frame into the contract that emitted the liquidation
    event — the protocol's own cost for one liquidation, independent of the
    liquidator's wrapper logic;
  * every pool swap frame in the same transaction (UniV3 `swap` includes its
    payment callback; UniV2 `swap`; Curve `exchange`).

Everything is cached under `cache/` so an interrupted run resumes.

    MAINNET_RPC_URL=... python tools/gas-measure/measure.py --blocks 150000 --per-protocol 15

Prints median / p75 / p90 per protocol and per venue. The numbers feed
`config/liq-gas.toml` (fixed liquidation gas) and pool hop gas.
"""

import argparse
import json
import os
import re
import statistics
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
CACHE = HERE / "cache"

# topic0 -> (protocol, liquidation function names accepted on the emitter frame)
PROTOCOLS = {
    "0xe413a321e8681d831f4dbccbca790d2952b56f977908e45be37335533e005286": ("aave-v3", {"liquidationCall"}),
    "0x2a1f12d996f530f89d8038aa293f9fde81cac44b6dfd6225e3358d09b78a4a37": ("aave-v4", {"liquidationCall"}),
    "0xa4946ede45d0c6f06a0f5ce92c9ad3b4751452d2fe0e25010783bcab57a67e41": ("morpho-blue", {"liquidate"}),
    "0x8246cc71ab01533b5bebc672a636df812f10637ad720797319d5741d5ebb3962": ("euler-v2", {"liquidate"}),
    "0x298637f684da70674f26509b10f07ec2fbc77a335ab1e7d6215a4b2484d8bb52": ("compound-v2", {"liquidateBorrow"}),
    "0x7243af9a1cff94d3429b2ee00b78c1c10589259f20dc167cb67704f38f9e824e": ("liquity-v2", {"batchLiquidateTroves", "liquidate"}),
    "0x80fd9cc6b1821f4a510e45ffce6852ea3404807b5d3d833ffa85664408afcb66": ("fluid", {"liquidate"}),
    "0x04d7a59a828995563eaa48eb65f11b681f7fec2fb7d6bc1a5426243882f9d249": ("gearbox", {"partiallyLiquidateCreditAccount"}),
    "0x7dfecd8419723a9d3954585a30c2a270165d70aafa146c11c1e1b88ae1439064": ("gearbox-full", {"liquidateCreditAccount"}),
    "0xaefcad9348418778869cbe7787be2b032ebe2d17110bb74b610fa3d8593afd1d": ("silo-v2", {"liquidationCall"}),
}

FRAME = re.compile(r"\[(\d+)\] (0x[0-9a-fA-F]{40})::(\w+)\((.*)")


def rpc(url, method, params, tries=10):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    out = None
    for i in range(tries):
        req = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json"})
        try:
            out = json.load(urllib.request.urlopen(req, timeout=90))
        except urllib.error.HTTPError as e:
            out = {"error": e.read().decode()[:300]}
        except Exception as e:  # network hiccup
            out = {"error": str(e)}
        err = json.dumps(out.get("error", ""))
        if "429" in err or "compute units" in err or "timed out" in err:
            time.sleep(min(30, 1.5 * (i + 1)))
            continue
        return out
    return out


def load(name, default):
    p = CACHE / name
    return json.loads(p.read_text()) if p.exists() else default


def save(name, obj):
    CACHE.mkdir(parents=True, exist_ok=True)
    (CACHE / name).write_text(json.dumps(obj, indent=1))


def discover(url, blocks, per_protocol):
    """tx hash -> {protocol, emitter} over the last `blocks` blocks."""
    state = load("discover.json", {"txs": {}, "scanned_to": None, "head": None})
    head = state["head"] or int(rpc(url, "eth_blockNumber", [])["result"], 16)
    state["head"] = head
    lo = head - blocks
    cursor = state["scanned_to"] or head
    topics = list(PROTOCOLS)
    failures = 0
    while cursor > lo:
        counts = {}
        for t in state["txs"].values():
            counts[t["protocol"]] = counts.get(t["protocol"], 0) + 1
        if all(counts.get(p, 0) >= per_protocol for p, _ in PROTOCOLS.values()):
            break
        frm = max(lo, cursor - 9)
        res = rpc(url, "eth_getLogs", [{"fromBlock": hex(frm), "toBlock": hex(cursor), "topics": [topics]}])
        if "error" in res:
            failures += 1
            print(f"getLogs error ({failures}) at {frm}: {str(res['error'])[:200]}", file=sys.stderr)
            if failures >= 20:
                print("getLogs failing persistently, stopping scan", file=sys.stderr)
                break
            time.sleep(min(60, 5 * failures))
            continue
        failures = 0
        for log in res["result"]:
            proto, _ = PROTOCOLS[log["topics"][0]]
            if counts.get(proto, 0) >= per_protocol:
                continue
            tx = log["transactionHash"]
            if tx not in state["txs"]:
                state["txs"][tx] = {"protocol": proto, "emitter": log["address"].lower(), "block": int(log["blockNumber"], 16)}
                counts[proto] = counts.get(proto, 0) + 1
        cursor = frm - 1
        state["scanned_to"] = cursor
        if (head - cursor) % 2000 < 10:
            save("discover.json", state)
            print(f"scanned {head - cursor}/{blocks} blocks, {counts}", file=sys.stderr)
        time.sleep(0.12)
    save("discover.json", state)
    return state["txs"]


def replay(url, tx):
    cached = load(f"trace-{tx}.json", None)
    if cached is not None:
        return cached
    for i in range(6):
        p = subprocess.run(
            ["cast", "run", tx, "--quick", "--rpc-url", url, "--compute-units-per-second", "25"],
            capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=1800,
        )
        if p.returncode == 0:
            break
        time.sleep(10 * (i + 1))
    frames = []
    for line in (p.stdout or "").splitlines():
        m = FRAME.search(line)
        if m:
            depth = len(line) - len(line.lstrip(" │├└─"))
            frames.append({"gas": int(m.group(1)), "to": m.group(2).lower(), "fn": m.group(3), "args": m.group(4)[:200], "depth": depth})
    save(f"trace-{tx}.json", frames)
    return frames


def venue(frame):
    if frame["fn"] == "exchange":
        return "curve"
    if frame["fn"] != "swap":
        return None
    args = [a.strip() for a in frame["args"].split(",")]
    return "univ3" if len(args) >= 2 and args[1] in ("true", "false") else "univ2"


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * (len(xs) - 1) + 0.5))]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--blocks", type=int, default=150_000)
    ap.add_argument("--per-protocol", type=int, default=15)
    args = ap.parse_args()
    url = os.environ.get("MAINNET_RPC_URL")
    if not url:
        sys.exit("MAINNET_RPC_URL unset")
    txs = discover(url, args.blocks, args.per_protocol)
    liq, swaps = {}, {}
    for i, (tx, meta) in enumerate(sorted(txs.items())):
        frames = replay(url, tx)
        names = next(n for p, n in PROTOCOLS.values() if p == meta["protocol"])
        hit = [f for f in frames if f["to"] == meta["emitter"] and f["fn"] in names]
        if hit:
            liq.setdefault(meta["protocol"], []).append(max(f["gas"] for f in hit))
        for f in frames:
            v = venue(f)
            if v:
                swaps.setdefault(v, []).append(f["gas"])
        print(f"[{i + 1}/{len(txs)}] {meta['protocol']} {tx} liq={hit[0]['gas'] if hit else None}", file=sys.stderr)
    report = {"liquidation_frame_gas": {}, "swap_frame_gas": {}}
    for k, xs in sorted(liq.items()):
        report["liquidation_frame_gas"][k] = {"n": len(xs), "median": int(statistics.median(xs)), "p75": pct(xs, 0.75), "p90": pct(xs, 0.9)}
    for k, xs in sorted(swaps.items()):
        report["swap_frame_gas"][k] = {"n": len(xs), "median": int(statistics.median(xs)), "p75": pct(xs, 0.75), "p90": pct(xs, 0.9)}
    save("report.json", report)
    print(json.dumps(report, indent=1))


if __name__ == "__main__":
    main()
