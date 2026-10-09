#!/usr/bin/env python3
"""Execute real Fluid DEX `swapIn` calls (eth_call with state overrides that
fund the caller and approve the pool) at every recorded vector's size and
compare them with `fluid_math.swap_in`:

- model answers, chain answers: the outputs must be equal;
- model answers, chain reverts: a plan built on the model would fail — must
  not happen;
- model refuses, chain reverts: the limits the pool's estimate cannot see.
- model refuses, chain answers: capacity the model gives up (reported).

Usage: MAINNET_RPC_URL=... python tools/registry/fluid_exec_check.py
(reads crates/liq-router/tests/fixtures/fluid_vectors.json; the chain is
queried at the block the fixture was recorded at.)
"""
from __future__ import annotations

import json
import os
import sys
from pathlib import Path

from eth_abi import decode, encode
from eth_utils import keccak
from web3 import Web3

sys.path.insert(0, str(Path(__file__).parent))
import discover_exits as d  # noqa: E402
import fluid_math as fm  # noqa: E402

FIX = Path("crates/liq-router/tests/fixtures/fluid_vectors.json")
NATIVE = "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE"
ME = Web3.to_checksum_address("0x000000000000000000000000000000000000f101")
SWAP_IN = d.sel("swapIn(bool,uint256,uint256,address)")
BIG = 1 << 200


def rpc(w3, method, params):
    import time

    for attempt in range(12):
        try:
            return w3.provider.make_request(method, params)
        except Exception as e:  # noqa: BLE001 - 429 on the free tier
            if "429" not in str(e) and "Too Many" not in str(e):
                raise
            time.sleep(1.5 * (attempt + 1))
    raise RuntimeError("rate limited")


def slot_of(key: str, base: int) -> int:
    return int.from_bytes(keccak(encode(["address", "uint256"], [key, base])), "big")


class Funder:
    """Finds, per token, the balance and allowance storage slots by probing."""

    def __init__(self, w3, block):
        self.w3, self.block, self.cache = w3, block, {}

    def keys_touched(self, token: str, data: str):
        r = rpc(self.w3, 
            "eth_createAccessList", [{"from": ME, "to": token, "data": data}, hex(self.block)])
        if "result" not in r:
            return []
        out = []
        for e in r["result"]["accessList"]:
            if e["address"].lower() == token.lower():
                out += e["storageKeys"]
        return out

    def probe(self, token: str, data: str, want_view: str):
        """The storage slot that the view call `data` reads, found among the
        slots the call touches by writing BIG there and re-reading."""
        for key in self.keys_touched(token, data):
            ov = {token: {"stateDiff": {key: "0x" + BIG.to_bytes(32, "big").hex()}}}
            r = rpc(self.w3, "eth_call", [{"to": token, "data": data}, hex(self.block), ov])
            if "result" in r and r["result"] not in ("0x", "") and int(r["result"], 16) == BIG:
                return key
        return None

    def overrides(self, token: str, pool: str):
        """`(balance_slot_key, allowance_slot_key)` for ME / pool, or None."""
        ck = (token, pool)
        if ck in self.cache:
            return self.cache[ck]
        bal = self.probe(token, "0x70a08231" + encode(["address"], [ME]).hex(), "balance")
        al = self.probe(token, "0xdd62ed3e" + encode(["address", "address"], [ME, pool]).hex(), "allowance")
        self.cache[ck] = (bal, al) if bal and al else None
        return self.cache[ck]


def sequences(w3, doc, fund):
    """Three consecutive swaps per pool in one simulated block: the second and
    third must equal the model applied after each earlier swap."""
    block = doc["block"]
    res = dict(pools=0, equal=0, differ=0, bad=[])
    rec = []
    for p in doc["pools"]:
        t0, t1 = p["token0"], p["token1"]
        if NATIVE in (t0, t1):
            continue
        fs = [fund.overrides(Web3.to_checksum_address(t), p["pool"]) for t in (t0, t1)]
        if any(f is None for f in fs):
            continue
        snap = fm.FluidSnapshot(
            t0, t1, *[int(x) for x in p["precisions"]], int(p["dex_vars"]), int(p["dex_vars2"]),
            None if p["center_ext"] is None else int(p["center_ext"]),
            [fm.LiqToken(*[int(t[k]) for k in ("ep_cfg", "totals", "configs2", "rate_data", "supply", "borrow", "balance")]) for t in p["tokens"]],
            (False, False))
        # a size the model accepts: the 100 bps case of each direction
        sizes = {}
        for c in p["cases"]:
            if "out" in c and c["zero_to_one"] not in sizes and int(c["amount"]) > 0:
                try:
                    fm.swap_in(snap, c["zero_to_one"], int(c["amount"]), p["timestamp"])
                    sizes[c["zero_to_one"]] = int(c["amount"])
                except fm.Revert:
                    pass
        if not sizes:
            continue
        plan = [(True, sizes.get(True)), (True, sizes.get(True)), (False, sizes.get(False))]
        plan = [(z, a) for z, a in plan if a]
        ov = {ME: {"balance": hex(BIG)}}
        for t, f in zip((t0, t1), fs):
            ov[Web3.to_checksum_address(t)] = {"stateDiff": {f[0]: "0x" + BIG.to_bytes(32, "big").hex(), f[1]: "0x" + BIG.to_bytes(32, "big").hex()}}
        calls = [{"from": ME, "to": p["pool"], "gas": hex(5_000_000),
                  "data": "0x" + (SWAP_IN + encode(["bool", "uint256", "uint256", "address"], [z, a, 0, ME])).hex()} for z, a in plan]
        r = rpc(w3, "eth_simulateV1", [{"blockStateCalls": [{"calls": calls, "stateOverrides": ov}], "validation": False}, hex(block)])
        if "result" not in r:
            continue
        outs = r["result"][0]["calls"]
        res["pools"] += 1
        rec.append({'pool_id': p['id'], 'swaps': [{'zero_to_one': z, 'amount': str(a), 'chain': str(int(o['returnData'], 16)) if o['status'] == '0x1' else None} for (z, a), o in zip(plan, outs)]})
        cur = snap
        for (z, a), o in zip(plan, outs):
            try:
                m, cur = fm.swap_in(cur, z, a, p["timestamp"] + int(os.environ.get("FLUID_SIM_DT", "12")))
            except fm.Revert:
                m = None
            c = int(o["returnData"], 16) if o["status"] == "0x1" else None
            if m == c:
                res["equal"] += 1
            else:
                res["differ"] += 1
                res["bad"].append(f"SEQUENCE pool {p['id']} zfo={z} amount={a}: model {m} chain {c}")
                break
    Path('crates/liq-router/tests/fixtures/fluid_sequences.json').write_text(json.dumps({'block': block, 'time_offset': 12, 'sequences': rec}, indent=1))
    return res


def main() -> int:
    chain = d.Chain(os.environ["MAINNET_RPC_URL"])
    w3 = chain.w3
    doc = json.loads(FIX.read_text())
    block = doc["block"]
    fund = Funder(w3, block)
    exec_rec = {}
    stats = dict(equal=0, differ=0, model_ok_chain_reverts=0, both_refuse=0, model_refuses_chain_ok=0, skipped=0)
    bad = []
    for p in doc["pools"]:
        snap = fm.FluidSnapshot(
            p["token0"], p["token1"], *[int(x) for x in p["precisions"]], int(p["dex_vars"]), int(p["dex_vars2"]),
            None if p["center_ext"] is None else int(p["center_ext"]),
            [fm.LiqToken(*[int(t[k]) for k in ("ep_cfg", "totals", "configs2", "rate_data", "supply", "borrow", "balance")]) for t in p["tokens"]],
            (p["token0"] == NATIVE, p["token1"] == NATIVE))
        for c in p["cases"]:
            zfo = c["zero_to_one"]
            tin = p["token0"] if zfo else p["token1"]
            amount = int(c["amount"])
            ov = {ME: {"balance": hex(BIG)}}
            value = 0
            if tin == NATIVE:
                value = amount
            else:
                f = fund.overrides(Web3.to_checksum_address(tin), p["pool"])
                if f is None:
                    stats["skipped"] += 1
                    continue
                bal_key, al_key = f
                ov[Web3.to_checksum_address(tin)] = {"stateDiff": {
                    bal_key: "0x" + BIG.to_bytes(32, "big").hex(),
                    al_key: "0x" + BIG.to_bytes(32, "big").hex()}}
            data = SWAP_IN + encode(["bool", "uint256", "uint256", "address"], [zfo, amount, 0, ME])
            tx = {"from": ME, "to": p["pool"], "data": "0x" + data.hex(), "value": hex(value)}
            r = rpc(w3, "eth_call", [tx, hex(block), ov])
            chain_out = None
            if "result" in r:
                chain_out = decode(["uint256"], bytes.fromhex(r["result"][2:]))[0]
            try:
                model_out, _ = fm.swap_in(snap, zfo, amount, p["timestamp"])
            except fm.Revert:
                model_out = None
            exec_rec.setdefault(str(p['id']), []).append({'zero_to_one': zfo, 'amount': str(amount), 'chain': None if chain_out is None else str(chain_out)})
            tag = f"pool {p['id']} zfo={zfo} amount={amount}"
            if model_out is not None and chain_out is not None:
                if model_out == chain_out:
                    stats["equal"] += 1
                else:
                    stats["differ"] += 1
                    bad.append(f"DIFFER {tag}: model {model_out} chain {chain_out}")
            elif model_out is not None:
                stats["model_ok_chain_reverts"] += 1
                bad.append(f"MODEL OK, CHAIN REVERTS {tag}: model {model_out} chain {r.get('error')}")
            elif chain_out is None:
                stats["both_refuse"] += 1
            else:
                stats["model_refuses_chain_ok"] += 1
                print(f"model refuses, chain answers {tag}: chain {chain_out}")
    Path('crates/liq-router/tests/fixtures/fluid_exec.json').write_text(json.dumps({'block': block, 'executions': exec_rec}, indent=1))
    seq = sequences(w3, doc, fund)
    print(stats, seq)
    bad += seq.pop("bad")
    for b in bad:
        print(b)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
