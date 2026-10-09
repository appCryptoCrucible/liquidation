#!/usr/bin/env python3
"""Record what a Curve crypto pool pays for swaps that follow each other in
one block, as a fixture for the Rust in-transaction model
(`crates/liq-router/tests/crypto_sequence.rs`).

A plan may send several slices of one sale through the same pool, and each
sees the state the earlier ones left: balances, `D` and, when the pool's
`tweak_price` rebalances, `price_scale`. `get_dy` cannot show that; only
running the swaps can. For every admitted pool of a kind the Rust model
applies (`two_stable`: the 2025 Twocrypto pools, which rebalance at most
once per block), `eth_simulateV1` runs, at the fixture block, from a holder
given the input coin by a state override: `approve`, then three
`exchange`s (`i → j` at 1/200 and 1/50 of the pool's balance of `i`, then
`j → i` at 1/100 of its balance of `j`), reading `price_scale`, `D` and the
balances after each. The outputs are the swaps' return values.

The first swap of a block may rebalance `price_scale`; the ones after it
never do (the pool's own guard). The fixture carries the `tweak_price`
state the model needs for that (`storage`) and the simulated block's
timestamp, and records whether the first swap did rebalance.

Usage: MAINNET_RPC_URL=... python tools/registry/crypto_sequence.py
"""
from __future__ import annotations

import json
import os
import sys
from pathlib import Path

from eth_abi import decode, encode
from web3 import Web3

sys.path.insert(0, str(Path(__file__).parent))
import crypto_vectors as cv  # noqa: E402
import discover_exits as d  # noqa: E402

OUT = Path("crates/liq-router/tests/fixtures/crypto_sequence.json")
HOLDER = "0x00000000000000000000000000000000000c0ffe"
KINDS = {"two_stable"}
ETH = 10**18


def balance_slot(w3: Web3, token: str, block: int) -> tuple[str, bool] | None:
    """The storage slot of `balanceOf[HOLDER]`: the mapping's base slot and
    whether the key goes before it (Solidity) or after (Vyper), found by
    overriding each candidate and reading the balance back."""
    want = 123_456_789 * ETH
    for base in range(0, 32):
        for solidity in (True, False):
            key = HOLDER[2:].rjust(64, "0")
            idx = hex(base)[2:].rjust(64, "0")
            slot = Web3.keccak(hexstr=(key + idx) if solidity else (idx + key)).hex()
            override = {token: {"stateDiff": {slot: "0x" + hex(want)[2:].rjust(64, "0")}}}
            data = d.sel("balanceOf(address)") + encode(["address"], [HOLDER])
            try:
                raw = w3.eth.call({"to": token, "data": data}, block, override)
            except Exception:  # noqa: BLE001
                continue
            if len(raw) >= 32 and decode(["uint256"], raw)[0] == want:
                return slot, solidity
    return None


def simulate(w3: Web3, block: int, overrides: dict, calls: list[dict]) -> tuple[int, list[dict]]:
    """The simulated block's timestamp and its calls' results."""
    body = {
        "blockStateCalls": [{"stateOverrides": overrides, "calls": calls}],
        "validation": False,
        "traceTransfers": False,
    }
    res = w3.provider.make_request("eth_simulateV1", [body, hex(block)])
    if "error" in res:
        raise RuntimeError(res["error"])
    blk = res["result"][0]
    return int(blk["timestamp"], 16), blk["calls"]


STORAGE_VIEWS = (
    "last_prices()", "last_timestamp()", "packed_rebalancing_params()", "totalSupply()",
    "donation_shares()", "donation_duration()", "last_donation_release_ts()",
    "donation_protection_expiry_ts()", "donation_protection_period()", "virtual_price()",
    "xcp_profit()", "lp_xcp_profit()", "reserved_profit_fraction()", "admin_fee()",
    "POLICY()", "version()",
)


def read_storage(chain: d.Chain, w3: Web3, addr: str) -> dict | None:
    """The pool's `tweak_price` state: its public views, and the cached
    price oracle from the storage slot before `last_prices`' (it has no
    getter; the `price_oracle()` view applies the EMA to now)."""
    res = chain.multicall([(addr, d.sel(v)) for v in STORAGE_VIEWS])
    st = {}
    for v, r in zip(STORAGE_VIEWS, res):
        key = v[:-2]
        if key == "version":
            st[key] = decode(["string"], r[1])[0] if r[0] else None
        elif key == "POLICY":
            st[key] = d.word(r, "address") if r[0] else None
        else:
            st[key] = d.word(r)
    if st["version"] not in ("v2.1.0d", "v3.0.0") or st["last_prices"] is None:
        return None
    a = Web3.to_checksum_address(addr)
    slot = None
    for k in range(1, 12):
        if int.from_bytes(w3.eth.get_storage_at(a, k, block_identifier=chain.block), "big") == st["last_prices"]:
            slot = k
            break
    if slot is None:
        return None
    st["price_oracle"] = int.from_bytes(w3.eth.get_storage_at(a, slot - 1, block_identifier=chain.block), "big")
    return st


def main() -> int:
    url = os.environ["MAINNET_RPC_URL"]
    chain = d.Chain(url)
    w3 = chain.w3
    block = chain.block
    reg = json.loads(d.REG.read_text(encoding="utf-8"))
    tokens = reg["tokens"]
    pools = [(a, p) for a, p in reg["pools"].items()
             if p["venue"] == "curve_crypto" and p.get("crypto_kind") in KINDS]
    slots: dict[str, tuple[str, bool]] = {}
    out = []
    for addr, entry in pools:
        coins = entry["coins"]
        n = len(coins)
        bal, D, A, G, mid, outf, fg, fut, ps, prec = cv.read_state(chain, addr, n, tokens, coins)
        if None in bal or None in ps or D is None or (fut and fut > chain.timestamp):
            continue
        storage = read_storage(chain, w3, addr)
        if storage is None:
            print(f"skip {addr}: tweak_price state unreadable", file=sys.stderr)
            continue
        for i, j in ((0, 1), (1, 0)):
            cin, cout = coins[i], coins[j]
            if cin not in slots:
                found = balance_slot(w3, Web3.to_checksum_address(cin), block)
                if found is None:
                    print(f"skip {addr}: no balance slot for {cin}", file=sys.stderr)
                    break
                slots[cin] = found
            slot, _ = slots[cin]
            overrides = {
                Web3.to_checksum_address(cin): {"stateDiff": {slot: "0x" + hex(bal[i] * 10)[2:].rjust(64, "0")}},
                HOLDER: {"balance": hex(10 * ETH)},
            }
            dxs = [max(bal[i] // 200, 1), max(bal[i] // 50, 1)]
            back = max(bal[j] // 100, 1)
            pool = Web3.to_checksum_address(addr)
            reads = [
                {"from": HOLDER, "to": pool, "data": "0x" + d.sel("price_scale()").hex()},
                {"from": HOLDER, "to": pool, "data": "0x" + d.sel("D()").hex()},
            ] + [
                {"from": HOLDER, "to": pool,
                 "data": "0x" + (d.sel("balances(uint256)") + encode(["uint256"], [k])).hex()}
                for k in range(n)
            ]
            ex = d.sel("exchange(uint256,uint256,uint256,uint256)")
            calls = [
                {"from": HOLDER, "to": Web3.to_checksum_address(cin),
                 "data": "0x" + (d.sel("approve(address,uint256)")
                                 + encode(["address", "uint256"], [pool, 2**256 - 1])).hex()},
                {"from": HOLDER, "to": Web3.to_checksum_address(cout),
                 "data": "0x" + (d.sel("approve(address,uint256)")
                                 + encode(["address", "uint256"], [pool, 2**256 - 1])).hex()},
            ]
            calls += reads
            swaps = [(i, j, dxs[0]), (i, j, dxs[1]), (j, i, back)]
            for si, sj, dx in swaps:
                calls.append({"from": HOLDER, "to": pool, "gas": hex(2_000_000),
                              "data": "0x" + (ex + encode(["uint256"] * 4, [si, sj, dx, 0])).hex()})
                calls += reads
            try:
                ts, res = simulate(w3, block, overrides, calls)
            except RuntimeError as ex_:
                print(f"skip {addr} {i}->{j}: {ex_}", file=sys.stderr)
                continue
            if any(r.get("status") != "0x1" for r in res):
                bad = [k for k, r in enumerate(res) if r.get("status") != "0x1"]
                print(f"skip {addr} {i}->{j}: calls {bad} failed", file=sys.stderr)
                continue
            vals = [int(r["returnData"], 16) if r["returnData"] != "0x" else None for r in res]
            per = 2 + n
            at = 2
            state0 = vals[at:at + per]
            at += per
            steps = []
            for si, sj, dx in swaps:
                dy = vals[at]
                st = vals[at + 1:at + 1 + per]
                at += 1 + per
                steps.append({"i": si, "j": sj, "dx": str(dx), "dy": str(dy),
                              "price_scale": str(st[0]), "d": str(st[1]),
                              "balances": [str(b) for b in st[2:]]})
            out.append({
                "pool": addr, "kind": entry["crypto_kind"], "version": storage["version"],
                "timestamp": ts,
                "precisions": [str(x) for x in prec], "ann": str(A), "gamma": str(G),
                "mid_fee": str(mid), "out_fee": str(outf), "fee_gamma": str(fg),
                "storage": {k: (str(v) if isinstance(v, int) else v) for k, v in storage.items()
                            if k != "version"},
                "before": {"price_scale": str(state0[0]), "d": str(state0[1]),
                           "balances": [str(b) for b in state0[2:]]},
                "first_rebalanced": str(state0[0]) != steps[0]["price_scale"],
                "steps": steps,
            })
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps({"block": block, "sequences": out}, indent=0) + "\n", encoding="utf-8")
    print(f"{len(out)} sequences at block {block}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
