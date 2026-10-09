#!/usr/bin/env python3
"""Record real Balancer Vault logs with the pool's Vault balances either side
of their block, as a fixture for the Rust log fold
(`liq_router::solver` `fold_balancer`, test
`a_balancer_pool_folds_vault_swaps_exactly`).

For each weighted pool in the registry, search recent blocks for one in which
the Vault logged exactly one event for the pool, a `Swap`: the pool's balances
at the block before, plus that swap's `amountIn` and minus its `amountOut`,
must be the balances at the block (`Vault.getPoolTokens`). Also records, when
one is found, a block in which the pool saw a join or exit
(`PoolBalanceChanged`), which the fold treats as "re-read".

Usage: MAINNET_RPC_URL=... LIQ_REGISTRY_FILE=... python tools/registry/balancer_swap_fixture.py
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

OUT = Path("crates/liq-router/tests/fixtures/balancer_swap_logs.json")
VAULT = Web3.to_checksum_address("0xBA12222222228d8Ba445958a75a0704d566BF2C8")
SWAP = Web3.keccak(text="Swap(bytes32,address,address,uint256,uint256)")
CHANGED = Web3.keccak(text="PoolBalanceChanged(bytes32,address,address[],int256[],uint256[])")
MANAGED = Web3.keccak(text="PoolBalanceManaged(bytes32,address,address,int256,int256)")
MAX_STEPS = 60


def pool_tokens(w3: Web3, pid: bytes, block: int):
    raw = call_retry(lambda: w3.eth.call({"to": VAULT, "data": d.sel("getPoolTokens(bytes32)") + encode(["bytes32"], [pid])}, block))
    return decode(["address[]", "uint256[]", "uint256"], raw)


def balances(w3: Web3, pid: bytes, block: int) -> list[int]:
    return list(pool_tokens(w3, pid, block)[1])


def call_retry(fn, tries=8):
    import time
    for k in range(tries):
        try:
            return fn()
        except Exception:  # noqa: BLE001 - throttled: back off and ask again
            if k == tries - 1:
                raise
            time.sleep(1.5 * 2**min(k, 4))


def main() -> int:
    w3 = Web3(Web3.HTTPProvider(os.environ["MAINNET_RPC_URL"], request_kwargs={"timeout": 60}))
    head = w3.eth.block_number
    reg = json.loads(d.REG.read_text(encoding="utf-8"))
    pools = [(a, p) for a, p in reg["pools"].items() if p["venue"] == "balancer"]
    out = []
    for addr, entry in pools:
        pid = bytes.fromhex(entry["pool_id"][2:])
        found_swap = found_change = None
        # The Vault says when the pool last changed (`lastChangeBlock`); the
        # block before it says the change before that. Walk back through them.
        blk = pool_tokens(w3, pid, head)[2]
        for _ in range(MAX_STEPS):
            if blk == 0:
                break
            logs = call_retry(lambda: w3.eth.get_logs({
                "address": VAULT, "fromBlock": blk, "toBlock": blk,
                "topics": [[SWAP, CHANGED, MANAGED], "0x" + pid.hex()]}))
            if found_swap is None and len(logs) == 1 and logs[0]["topics"][0] == SWAP:
                found_swap = (blk, logs[0])
            if found_change is None and any(x["topics"][0] == CHANGED for x in logs):
                found_change = (blk, [x for x in logs if x["topics"][0] == CHANGED][0])
            if found_swap and found_change:
                break
            blk = pool_tokens(w3, pid, blk - 1)[2]
        rec = {"pool": addr, "pool_id": entry["pool_id"], "tokens": entry["coins"]}
        if found_swap:
            blk, lg = found_swap
            rec["swap"] = {
                "block": blk,
                "topics": ["0x" + t.hex() for t in lg["topics"]],
                "data": "0x" + lg["data"].hex(),
                "before": [str(b) for b in balances(w3, pid, blk - 1)],
                "after": [str(b) for b in balances(w3, pid, blk)],
            }
        if found_change:
            blk, lg = found_change
            rec["change"] = {
                "block": blk,
                "topics": ["0x" + t.hex() for t in lg["topics"]],
                "data": "0x" + lg["data"].hex(),
            }
        out.append(rec)
        print(addr, "swap" if found_swap else "-", "change" if found_change else "-")
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps({"pools": out}, indent=1) + "\n", encoding="utf-8")
    return 0 if any("swap" in r for r in out) else 1


if __name__ == "__main__":
    sys.exit(main())
