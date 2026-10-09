#!/usr/bin/env python3
"""Discover Uniswap V4 pools the Executor can swap through (venue 9) and add
them to registry/registry.json as `univ4` pools.

V4 pools live in one PoolManager and are found only by its `Initialize`
events. They are scanned from the PoolManager's deployment in 50k-block
windows on a log endpoint (`--logs-rpc`, default the registry meta's
`rpc_logs`); a window that looks truncated (no logs, or a round count) is
split. Answers are cached in `data/history-cache/v4_init.jsonl`.

A pool is kept when:
  - its hook is allowed (decision 7; `hook_allowed` here must equal
    `MainnetVenues.v4HookAllowed`): none, one with no swap permission bit,
    or one on the reviewed allowlist;
  - its fee is static (a dynamic fee, flag 0x800000, needs a hook);
  - both currencies are registry tokens (native ETH counts as WETH);
  - `StateView` finds it initialized with liquidity in range now
    (`MAINNET_RPC_URL`).

Entries: keyed by the low 20 bytes of the pool id, `venue = "univ4"`,
`token0`/`token1` the plan's tokens (WETH for native ETH, with
`native = true`), `fee` the key's LP fee, `tick_spacing`, `hooks`, `v4_id`.

Usage: MAINNET_RPC_URL=... python tools/registry/discover_v4.py [--dry-run]
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path

from eth_abi import decode, encode
from web3 import Web3

sys.path.insert(0, str(Path(__file__).parent))
from discover_exits import Chain, sel, word  # noqa: E402

REG = Path("registry/registry.json")
META = Path("registry/registry.meta.json")
CACHE = Path("data/history-cache/v4_init.jsonl")
PM = "0x000000000004444c5dc75cb358380d2e3de08a90"
STATE_VIEW = "0x7ffe42c4a5deea5b0fec41c94c136cf115597227"
WETH = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
ZERO = "0x" + "0" * 40
DEPLOY = 21_688_329
WINDOW = 50_000
DYNAMIC_FEE = 0x800000
# MainnetVenues.v4HookAllowed: a hook without any swap permission bit
# (beforeSwap 1<<7, afterSwap 1<<6, their delta returns 1<<3 and 1<<2) is
# never called in a swap; any other must be a reviewed, listed hook.
V4_SWAP_HOOK_FLAGS = (1 << 7) | (1 << 6) | (1 << 3) | (1 << 2)
HOOK_ALLOWLIST: set[str] = set()


def hook_allowed(h: str) -> bool:
    return int(h, 16) & V4_SWAP_HOOK_FLAGS == 0 or h in HOOK_ALLOWLIST
INIT = "0x" + Web3.keccak(
    text="Initialize(bytes32,address,address,uint24,int24,address,uint160,int24)").hex().removeprefix("0x")


def scan(w: Web3, lo: int, hi: int, cache: dict) -> list[dict]:
    key = f"{lo}-{hi}"
    if key in cache:
        return cache[key]
    for attempt in range(6):
        try:
            logs = w.eth.get_logs({"address": Web3.to_checksum_address(PM),
                                   "fromBlock": lo, "toBlock": hi, "topics": [INIT]})
            break
        except Exception as ex:  # noqa: BLE001 - retry, then split
            if attempt == 5 or hi - lo > 2_000:
                if hi - lo <= 2_000:
                    raise
                mid = (lo + hi) // 2
                return scan(w, lo, mid, cache) + scan(w, mid + 1, hi, cache)
            time.sleep(1.5 * 2 ** attempt)
    # A silently truncated window: split it.
    if hi - lo > 2_000 and (not logs or len(logs) % 1000 == 0):
        mid = (lo + hi) // 2
        out = scan(w, lo, mid, cache) + scan(w, mid + 1, hi, cache)
    else:
        out = []
        for l in logs:
            fee, ts, hooks, _sqrt, _tick = decode(["uint24", "int24", "address", "uint160", "int24"],
                                                   bytes(l["data"]))
            out.append({"id": "0x" + l["topics"][1].hex().removeprefix("0x"),
                        "c0": "0x" + l["topics"][2].hex()[-40:], "c1": "0x" + l["topics"][3].hex()[-40:],
                        "fee": fee, "ts": ts, "hooks": hooks.lower(), "block": l["blockNumber"]})
    cache[key] = out
    with open(CACHE, "a", encoding="utf-8") as f:
        f.write(json.dumps({"k": key, "v": out}) + "\n")
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--logs-rpc", default=None)
    args = ap.parse_args()
    reg = json.loads(REG.read_text(encoding="utf-8"))
    logs_url = args.logs_rpc or json.loads(META.read_text(encoding="utf-8"))["rpc_logs"]
    w = Web3(Web3.HTTPProvider(logs_url, request_kwargs={"timeout": 90}))
    chain = Chain(os.environ["MAINNET_RPC_URL"])
    tokens = {a.lower() for a in reg["tokens"]}

    CACHE.parent.mkdir(parents=True, exist_ok=True)
    cache: dict = {}
    if CACHE.exists():
        for line in CACHE.read_text(encoding="utf-8").splitlines():
            r = json.loads(line)
            cache[r["k"]] = r["v"]
    pools = []
    for lo in range(DEPLOY, chain.block + 1, WINDOW):
        pools += scan(w, lo, min(lo + WINDOW - 1, chain.block), cache)
    print(f"Initialize events {DEPLOY}..{chain.block}: {len(pools)}")

    def plan_token(c: str) -> str:
        return WETH if c == ZERO else c

    cands, why = [], {"hook": 0, "dynamic fee": 0, "token outside the registry": 0,
                      "native ETH against WETH": 0}
    for p in pools:
        if not hook_allowed(p["hooks"]):
            why["hook"] += 1
        elif plan_token(p["c0"]) == plan_token(p["c1"]):
            # ETH/WETH: both sides are WETH to the plan, a self-loop, not a swap.
            why["native ETH against WETH"] += 1
        elif p["fee"] & DYNAMIC_FEE:
            why["dynamic fee"] += 1
        elif plan_token(p["c0"]) not in tokens or plan_token(p["c1"]) not in tokens:
            why["token outside the registry"] += 1
        else:
            cands.append(p)
    print(f"  refused: {why}; candidates {len(cands)}")

    calls = []
    for p in cands:
        pid = bytes.fromhex(p["id"][2:])
        calls.append((STATE_VIEW, sel("getSlot0(bytes32)") + encode(["bytes32"], [pid])))
        calls.append((STATE_VIEW, sel("getLiquidity(bytes32)") + encode(["bytes32"], [pid])))
    res = chain.multicall(calls)
    live = []
    for i, p in enumerate(cands):
        s0, liq = res[2 * i], res[2 * i + 1]
        sqrt = decode(["uint160"], s0[1][:32])[0] if s0[0] and len(s0[1]) >= 32 else 0
        if sqrt and (word(liq) or 0) > 0:
            live.append(p)
    print(f"  initialized with liquidity in range: {len(live)}")
    if args.dry_run:
        return 0

    added = 0
    for p in live:
        key = "0x" + p["id"][-40:]
        if key in reg["pools"]:
            continue
        native = p["c0"] == ZERO
        reg["pools"][key] = {
            "venue": "univ4", "token0": plan_token(p["c0"]), "token1": p["c1"], "fee": p["fee"],
            "factory": PM, "deployed_block": p["block"], "derived_via": "PoolManager.Initialize",
            "tick_spacing": p["ts"], "hooks": p["hooks"], "v4_id": p["id"],
            **({"native": True} if native else {}),
        }
        added += 1
    REG.write_text(json.dumps(reg, indent=1) + "\n", encoding="utf-8")
    print(f"added {added} univ4 pools to {REG}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
