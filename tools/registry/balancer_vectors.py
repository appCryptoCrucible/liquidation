#!/usr/bin/env python3
"""Record live Balancer V2 weighted pools and the Vault's own answers as a
test fixture for the Rust port (`crates/liq-router/tests/balancer_vectors.rs`).

For every weighted pool the registry holds (`venue: balancer`), at one block:
the Vault balances, the pool's normalized weights and swap fee, the tokens'
decimals, and for every ordered token pair the answer of Balancer's own
`BalancerQueries.querySwap` (it runs the Vault's `swap` and reverts the
state), for exact input (`GIVEN_IN`) and exact output (`GIVEN_OUT`) at sizes
from a ten-millionth to past the 30 % ratio the math refuses. A size the pool
refuses is recorded as `null`.

Usage: MAINNET_RPC_URL=... python tools/registry/balancer_vectors.py
"""
from __future__ import annotations

import json
import os
import sys
from pathlib import Path

from eth_abi import decode, encode
from web3 import Web3

sys.path.insert(0, str(Path(__file__).parent))
import discover_exits as d  # noqa: E402

OUT = Path("crates/liq-router/tests/fixtures/balancer_vectors.json")
VAULT = "0xBA12222222228d8Ba445958a75a0704d566BF2C8"
QUERIES = "0xE39B5e3B6D74016b2F6A9673D7d7493B6DF549d5"
ME = "0x0000000000000000000000000000000000000001"
QUERY = d.sel("querySwap((bytes32,uint8,address,address,uint256,bytes),(address,bool,address,bool))")
SIZES = [(1, 10_000_000), (1, 100_000), (1, 1_000), (1, 100), (5, 100), (15, 100), (29, 100), (31, 100)]


def query(chain: d.Chain, pid: bytes, kind: int, tin: str, tout: str, amount: int):
    data = QUERY + encode(
        ["(bytes32,uint8,address,address,uint256,bytes)", "(address,bool,address,bool)"],
        [(pid, kind, Web3.to_checksum_address(tin), Web3.to_checksum_address(tout), amount, b""), (ME, False, ME, False)],
    )
    try:
        raw = chain.w3.eth.call({"to": QUERIES, "data": data}, chain.block)
    except Exception:  # noqa: BLE001 - the pool refuses this size
        return None
    return decode(["uint256"], raw)[0]


def main() -> int:
    chain = d.Chain(os.environ["MAINNET_RPC_URL"])
    w3 = chain.w3
    reg = json.loads(d.REG.read_text(encoding="utf-8"))
    pools = [(a, p) for a, p in reg["pools"].items() if p["venue"] == "balancer"]
    if not pools:
        print("no balancer pools in the registry", file=sys.stderr)
        return 1
    vectors = []
    for addr, entry in pools:
        pid = bytes.fromhex(entry["pool_id"][2:])
        call = lambda to, sig, out, args=(), types=(): decode(  # noqa: E731
            out, w3.eth.call({"to": Web3.to_checksum_address(to), "data": d.sel(sig) + (encode(list(types), list(args)) if types else b"")}, chain.block))
        toks, bals, _ = call(VAULT, "getPoolTokens(bytes32)", ["address[]", "uint256[]", "uint256"], [pid], ["bytes32"])
        weights = call(addr, "getNormalizedWeights()", ["uint256[]"])[0]
        fee = call(addr, "getSwapFeePercentage()", ["uint256"])[0]
        decimals = [call(t, "decimals()", ["uint8"])[0] for t in toks]
        cases = []
        n = len(toks)
        for i in range(n):
            for j in range(n):
                if i == j:
                    continue
                for num, den in SIZES:
                    ain = max(bals[i] * num // den, 1)
                    aout = max(bals[j] * num // den, 1)
                    cases.append([i, j, 0, str(ain), _s(query(chain, pid, 0, toks[i], toks[j], ain))])
                    cases.append([i, j, 1, str(aout), _s(query(chain, pid, 1, toks[i], toks[j], aout))])
        vectors.append({
            "pool": addr, "pool_id": entry["pool_id"], "kind": entry["balancer_kind"],
            "tokens": list(toks), "decimals": decimals, "balances": [str(b) for b in bals],
            "weights": [str(w) for w in weights], "swap_fee": str(fee), "cases": cases,
        })
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps({"block": chain.block, "pools": vectors}, indent=0) + "\n", encoding="utf-8")
    refused = sum(1 for v in vectors for c in v["cases"] if c[4] is None)
    print(f"{len(vectors)} pools, {sum(len(v['cases']) for v in vectors)} cases ({refused} refused) at block {chain.block}")
    return 0


def _s(v):
    return None if v is None else str(v)


if __name__ == "__main__":
    sys.exit(main())
