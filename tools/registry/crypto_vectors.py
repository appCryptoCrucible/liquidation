#!/usr/bin/env python3
"""Record live Curve crypto-pool states and their own `get_dy` outputs as a
test fixture for the Rust port (`crates/liq-router/tests/crypto_vectors.rs`).

Every discovery-admissible crypto pool (twocrypto-ng v2.0.0 / v2.1.0,
tricrypto-ng v2.0.0, the original CurveCryptoSwap2) is read at one block;
for every ordered coin pair, `get_dy` at 1/1000, 1/20 and 1/3 of the input
coin's balance. A size the pool refuses is recorded as `null`. The Python
port (`crypto_math.py`) must agree with every recorded value, so the fixture
only contains pools it reproduces.

Usage: MAINNET_RPC_URL=... python tools/registry/crypto_vectors.py
"""
from __future__ import annotations

import json
import os
import sys
from pathlib import Path

from eth_abi import decode, encode

sys.path.insert(0, str(Path(__file__).parent))
import crypto_math as cm  # noqa: E402
import discover_exits as d  # noqa: E402

OUT = Path("crates/liq-router/tests/fixtures/crypto_vectors.json")


def pool_kind(chain: d.Chain, pools: list[str]) -> dict[str, str]:
    res = chain.multicall([c for p in pools for c in ((p, d.sel("version()")), (p, d.sel("MATH()")))])
    out = {}
    for k, p in enumerate(pools):
        vr, mr = res[2 * k], res[2 * k + 1]
        ver = None
        if vr[0] and len(vr[1]) >= 64:
            try:
                ver = decode(["string"], vr[1])[0]
            except Exception:  # noqa: BLE001
                ver = None
        out[p] = ver if mr[0] else "v1"
    return out


def read_state(chain: d.Chain, p: str, n: int, tokens: dict, coins: list[str]):
    calls = [(p, d.sel("balances(uint256)") + encode(["uint256"], [i])) for i in range(n)]
    calls += [(p, d.sel(s)) for s in ("D()", "A()", "gamma()", "mid_fee()", "out_fee()",
                                      "fee_gamma()", "future_A_gamma_time()")]
    if n == 2:
        calls.append((p, d.sel("price_scale()")))
    else:
        calls += [(p, d.sel("price_scale(uint256)") + encode(["uint256"], [k])) for k in (0, 1)]
    r = chain.multicall(calls)
    bal = [d.word(x) for x in r[:n]]
    D, A, G, mid, out, fg, fut = (d.word(x) for x in r[n:n + 7])
    ps = [d.word(x) for x in r[n + 7:]]
    prec = [10 ** (18 - tokens[c]["decimals"]) for c in coins]
    return bal, D, A, G, mid, out, fg, fut, ps, prec


def main() -> int:
    chain = d.Chain(os.environ["MAINNET_RPC_URL"])
    reg = json.loads(d.REG.read_text(encoding="utf-8"))
    tokens = reg["tokens"]
    cands = [(a, p) for a, p in reg["pools"].items() if p["venue"] == "curve_crypto"]
    kinds = {a: p["crypto_kind"] for a, p in cands}
    vectors = []
    for addr, entry in cands:
        coins = entry["coins"]
        n = len(coins)
        bal, D, A, G, mid, out, fg, fut, ps, prec = read_state(chain, addr, n, tokens, coins)
        if None in bal or None in ps or D is None or (fut and fut > chain.timestamp):
            continue
        cases = []
        for i in range(n):
            for j in range(n):
                if i == j:
                    continue
                for div in (1000, 20, 3):
                    cases.append((i, j, max(bal[i] // div, 1)))
        res = chain.multicall([(addr, d.sel("get_dy(uint256,uint256,uint256)")
                                + encode(["uint256"] * 3, [i, j, dx])) for i, j, dx in cases])
        rows = []
        for (i, j, dx), rr in zip(cases, res):
            want = d.word(rr)
            kind = kinds[addr]
            try:
                if kind == "tri":
                    got = cm.tri_get_dy(i, j, dx, bal, prec, ps, D, A, G, mid, out, fg)
                elif kind == "two_v1":
                    got = cm.two_v1_get_dy(i, j, dx, bal, prec, ps[0], D, A, G, mid, out, fg)
                elif kind == "two_stable":
                    got = cm.two_stable_get_dy(i, j, dx, bal, prec, ps[0], D, A, mid, out, fg)
                else:
                    got = cm.two_get_dy(i, j, dx, bal, prec, ps[0], D, A, G, mid, out, fg,
                                        kind == "two_v210")
            except cm.Revert:
                got = None
            if got != want:
                print(f"skip {addr}: port {got} chain {want} ({i}->{j} {dx})", file=sys.stderr)
                rows = None
                break
            rows.append([i, j, str(dx), None if want is None else str(want)])
        if rows is None:
            continue
        vectors.append({
            "pool": addr, "kind": kinds[addr],
            "balances": [str(b) for b in bal], "precisions": [str(x) for x in prec],
            "price_scale": [str(x) for x in ps], "d": str(D), "ann": str(A), "gamma": str(G),
            "mid_fee": str(mid), "out_fee": str(out), "fee_gamma": str(fg), "cases": rows,
        })
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps({"block": chain.block, "pools": vectors}, indent=0) + "\n",
                   encoding="utf-8")
    print(f"{len(vectors)} pools, {sum(len(v['cases']) for v in vectors)} cases at block {chain.block}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
