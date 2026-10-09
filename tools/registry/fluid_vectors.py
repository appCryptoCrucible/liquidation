#!/usr/bin/env python3
"""Record live Fluid DEX T1 pools as a fixture for the Python reference port
(`fluid_math.py`) and the Rust port (`crates/liq-router/tests/fluid_vectors.rs`).

Per pool at one block: the raw pool words (`dexVariables`, `dexVariables2`),
the center price hook's answer, token precisions, and every Liquidity-layer
word the model reads (exchange prices and config, totals, rate data,
configs2, the pool's supply and borrow user words, the layer's balance); and
for both directions at several sizes the pool's own `swapIn` estimate
(`to = ADDRESS_DEAD` makes it revert `FluidDexSwapResult(amountOut)` after the
DEX math and before the Liquidity `operate` checks). A size the pool refuses
is recorded as the revert selector.

Usage: MAINNET_RPC_URL=... python tools/registry/fluid_vectors.py [--check]
`--check` also replays every vector through `fluid_math.swap_in` and reports
disagreements (the Liquidity-limit reverts the estimate cannot see are not
compared).
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

OUT = Path("crates/liq-router/tests/fixtures/fluid_vectors.json")
FACTORY = "0x91716C4EDA1Fb55e84Bf8b4c7085f84285c19085"
LIQ = "0x52Aa899454998Be5b000Ad077a46Bbe360F4e497"
DEAD = "0x000000000000000000000000000000000000dEaD"
NATIVE = "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE"
SWAP_IN = d.sel("swapIn(bool,uint256,uint256,address)")
RESULT_SEL = keccak(text="FluidDexSwapResult(uint256)")[:4]
# fractions of the pool's own reserves (token-in side), in basis points
SIZES = [1, 10, 100, 500, 1000, 2000, 4000]


def create_address(deployer: str, nonce: int) -> str:
    """`AddressCalcs.addressCalc`: CREATE address of `deployer` at `nonce`."""
    if nonce == 0:
        raise ValueError("nonce 0")
    if nonce <= 0x7F:
        enc = bytes([0xD6, 0x94]) + bytes.fromhex(deployer[2:]) + bytes([nonce])
    else:
        nb = nonce.to_bytes((nonce.bit_length() + 7) // 8, "big")
        enc = bytes([0xD6 + len(nb), 0x94]) + bytes.fromhex(deployer[2:]) + bytes([0x80 + len(nb)]) + nb
    return Web3.to_checksum_address(keccak(enc)[12:])


def estimate(chain: d.Chain, pool: str, zero_to_one: bool, amount: int, value: int = 0):
    data = SWAP_IN + encode(["bool", "uint256", "uint256", "address"], [zero_to_one, amount, 0, DEAD])
    try:
        chain.w3.eth.call({"to": pool, "data": data, "value": value}, chain.block)
    except Exception as e:  # noqa: BLE001
        raw = getattr(e, "data", None) or (e.args[1] if len(e.args) > 1 else None)
        if isinstance(raw, str) and raw.startswith("0x"):
            b = bytes.fromhex(raw[2:])
            if b[:4] == RESULT_SEL:
                return {"out": str(decode(["uint256"], b[4:])[0])}
            return {"revert": "0x" + b[:36].hex()}
        return {"revert": str(e)[:120]}
    return {"revert": "no revert"}


def all_pools(chain: d.Chain) -> list[str]:
    res = chain.multicall([(FACTORY, d.sel("getDexAddress(uint256)") + encode(["uint256"], [i])) for i in range(1, 51)])
    return [Web3.to_checksum_address(decode(["address"], r[1])[0]) for r in res if r[0]]


def read_pool(chain: d.Chain, pid: int, pool: str) -> dict | None:
    """Every raw word the model reads for one pool, or None for a pool the
    model does not follow (paused, hooked, no smart side, unreadable hook)."""
    w3 = chain.w3
    st = lambda a, s: int.from_bytes(w3.eth.get_storage_at(a, s, block_identifier=chain.block), "big")  # noqa: E731
    cv = w3.eth.call({"to": pool, "data": d.sel("constantsView()")}, chain.block)
    w = [cv[i : i + 32] for i in range(0, len(cv), 32)]
    cv2 = w3.eth.call({"to": pool, "data": d.sel("constantsView2()")}, chain.block)
    prec = list(decode(["uint256"] * 4, cv2))
    t0 = Web3.to_checksum_address(w[9][12:])
    t1 = Web3.to_checksum_address(w[10][12:])
    deployer = Web3.to_checksum_address(w[8][12:])
    dv1, dv2 = st(pool, 0), st(pool, 1)
    if not (dv2 & 1 or (dv2 >> 1) & 1) or dv2 >> 255:
        return None
    if (dv2 >> 142) & fm.X[30]:
        return None
    ctr_id = (dv2 >> 112) & fm.X[30]
    center_ext = None
    if ctr_id:
        try:
            addr = create_address(deployer, ctr_id)
            center_ext = decode(["uint256"], w3.eth.call({"to": addr, "data": d.sel("centerPrice()")}, chain.block))[0]
        except Exception:  # noqa: BLE001
            return None
    slots = {"s0": w[11], "b0": w[12], "s1": w[13], "b1": w[14]}
    slots = {k: int.from_bytes(v, "big") for k, v in slots.items()}
    toks = []
    for idx, tok in enumerate((t0, t1)):
        base = lambda m: int.from_bytes(keccak(encode(["address", "uint256"], [tok, m])), "big")  # noqa: E731
        toks.append({
            "token": tok,
            "ep_cfg": str(st(LIQ, base(5))), "totals": str(st(LIQ, base(7))), "configs2": str(st(LIQ, base(11))),
            "rate_data": str(st(LIQ, base(6))),
            "supply": str(st(LIQ, slots["s0" if idx == 0 else "s1"])),
            "borrow": str(st(LIQ, slots["b0" if idx == 0 else "b1"])),
            "balance": str(w3.eth.get_balance(LIQ, chain.block) if tok == NATIVE else decode(
                ["uint256"], w3.eth.call({"to": tok, "data": d.sel("balanceOf(address)") + encode(["address"], [LIQ])}, chain.block))[0]),
        })
    return {
        "id": pid, "pool": pool, "token0": t0, "token1": t1, "precisions": [str(p) for p in prec],
        "dex_vars": str(dv1), "dex_vars2": str(dv2), "center_ext": None if center_ext is None else str(center_ext),
        "tokens": toks, "block": chain.block, "timestamp": chain.timestamp,
    }


def snapshot_of(rec: dict) -> fm.FluidSnapshot:
    return fm.FluidSnapshot(
        rec["token0"], rec["token1"], *[int(x) for x in rec["precisions"]], int(rec["dex_vars"]), int(rec["dex_vars2"]),
        None if rec["center_ext"] is None else int(rec["center_ext"]),
        [fm.LiqToken(*[int(t[k]) for k in ("ep_cfg", "totals", "configs2", "rate_data", "supply", "borrow", "balance")]) for t in rec["tokens"]],
        (rec["token0"] == NATIVE, rec["token1"] == NATIVE))


def main() -> int:
    chain = d.Chain(os.environ["MAINNET_RPC_URL"])
    ts = chain.timestamp
    out = []
    for pid, pool in enumerate(all_pools(chain), 1):
        rec = read_pool(chain, pid, pool)
        if rec is None:
            continue
        toks = rec["tokens"]
        native = (rec["token0"] == NATIVE, rec["token1"] == NATIVE)
        t0, t1, prec = rec["token0"], rec["token1"], [int(x) for x in rec["precisions"]]
        # sizes relative to the token-in side's liquidity layer supply
        cases = []
        for z in (True, False):
            tin = toks[0 if z else 1]
            scale = max(int(tin["balance"]), 10**6)
            for bps in SIZES:
                amt = max(scale * bps // 10_000, 1000)
                val = amt if native[0 if z else 1] else 0
                cases.append({"zero_to_one": z, "amount": str(amt), **estimate(chain, pool, z, amt, val)})
        out.append({**rec, "cases": cases})
        print(f"pool {pid} {pool} cases={len(cases)}", file=sys.stderr)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps({"block": chain.block, "timestamp": ts, "pools": out}, indent=1))
    if "--check" in sys.argv:
        bad = total = 0
        for p in out:
            snap = fm.FluidSnapshot(p["token0"], p["token1"], *[int(x) for x in p["precisions"]], int(p["dex_vars"]),
                                    int(p["dex_vars2"]), None if p["center_ext"] is None else int(p["center_ext"]),
                                    [fm.LiqToken(*[int(t[k]) for k in ("ep_cfg", "totals", "configs2", "rate_data", "supply", "borrow", "balance")]) for t in p["tokens"]],
                                    (p["token0"] == NATIVE, p["token1"] == NATIVE))
            for c in p["cases"]:
                if "out" not in c:
                    continue
                total += 1
                try:
                    o, _ = fm.swap_in(snap, c["zero_to_one"], int(c["amount"]), p["timestamp"])
                    # the estimate is run at block.timestamp of the next block; compare loosely on exact equality
                    if o != int(c["out"]):
                        bad += 1
                        print("MISMATCH", p["id"], c["zero_to_one"], c["amount"], o, c["out"])
                except fm.Revert as e:
                    print("model reverts", p["id"], c["zero_to_one"], c["amount"], e, "chain out", c["out"])
                    bad += 1
        print(f"checked {total} answered vectors, {bad} disagree")
    return 0


if __name__ == "__main__":
    sys.exit(main())
