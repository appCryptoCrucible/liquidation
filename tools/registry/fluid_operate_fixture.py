#!/usr/bin/env python3
"""Record real Fluid Liquidity `LogOperate` events as a fixture for the
router's fold (`crates/liq-router/src/solver.rs`,
`a_fluid_pool_folds_liquidity_operates_exactly`).

A block qualifies when the Liquidity layer logged exactly one `LogOperate`
for the token in it, from a user that is not a Fluid DEX pool, and nothing
else moved the token's balance there. Then the chain itself is the oracle:
the event's `totalAmounts` and `exchangePricesAndConfig` must equal the
token's storage words after the block, and the layer's balance must have
moved by `supplyAmount − borrowAmount` (the fold's rule).

Usage: MAINNET_RPC_URL=... python tools/registry/fluid_operate_fixture.py
"""
from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path

from eth_abi import decode, encode
from eth_utils import keccak
from web3 import Web3

sys.path.insert(0, str(Path(__file__).parent))
import discover_exits as d  # noqa: E402
import fluid_vectors as fv  # noqa: E402

OUT = Path("crates/liq-router/tests/fixtures/fluid_operate_logs.json")
LIQ = fv.LIQ
TOPIC = "0x" + keccak(text="LogOperate(address,address,int256,int256,address,address,uint256,uint256)").hex()
SPAN = 1500


def main() -> int:
    chain = d.Chain(os.environ["MAINNET_RPC_URL"])
    w3 = chain.w3
    pools = {p.lower() for p in fv.all_pools(chain)}
    tokens = set()
    for p in fv.all_pools(chain):
        cv = w3.eth.call({"to": p, "data": d.sel("constantsView()")}, chain.block)
        tokens |= {Web3.to_checksum_address(cv[9 * 32 + 12 : 10 * 32]), Web3.to_checksum_address(cv[10 * 32 + 12 : 11 * 32])}
    head = chain.block
    logs = []
    for lo in range(head - SPAN, head, 10):  # the free tier caps a log query at 10 blocks
        for attempt in range(8):
            try:
                logs += w3.eth.get_logs({"address": LIQ, "topics": [TOPIC], "fromBlock": lo, "toBlock": min(lo + 9, head)})
                break
            except Exception as e:  # noqa: BLE001
                if "429" not in str(e):
                    raise
                time.sleep(1.5 * (attempt + 1))
    by_block: dict[int, list] = {}
    for lg in logs:
        by_block.setdefault(lg["blockNumber"], []).append(lg)
    samples = []
    kinds = set()
    for blk, ls in sorted(by_block.items(), reverse=True):
        for lg in ls:
            user = Web3.to_checksum_address(lg["topics"][1][12:])
            token = Web3.to_checksum_address(lg["topics"][2][12:])
            if token not in tokens or user.lower() in pools:
                continue
            if sum(1 for x in ls if x["topics"][2] == lg["topics"][2]) != 1:
                continue
            supply, borrow, _, _, totals, ep = decode(["int256", "int256", "address", "address", "uint256", "uint256"], bytes(lg["data"]))
            base = lambda m: int.from_bytes(keccak(encode(["address", "uint256"], [token, m])), "big")  # noqa: E731
            at = lambda slot, b: int.from_bytes(w3.eth.get_storage_at(LIQ, slot, block_identifier=b), "big")  # noqa: E731
            if at(base(7), blk) != totals or at(base(5), blk) != ep:
                continue
            if token == fv.NATIVE:
                bal = lambda b: w3.eth.get_balance(LIQ, b)  # noqa: E731
            else:
                bal = lambda b, t=token: decode(["uint256"], w3.eth.call({"to": t, "data": d.sel("balanceOf(address)") + encode(["address"], [LIQ])}, b))[0]  # noqa: E731
            before, after = bal(blk - 1), bal(blk)
            if after - before != supply - borrow:
                continue
            kind = ("supply" if supply > 0 else "withdraw" if supply < 0 else "") + ("borrow" if borrow > 0 else "payback" if borrow < 0 else "")
            if kind in kinds and len(samples) >= 3:
                continue
            kinds.add(kind)
            samples.append({
                "kind": kind, "block": blk, "address": LIQ.lower(),
                "topics": ["0x" + bytes(t).hex() for t in lg["topics"]], "data": "0x" + bytes(lg["data"]).hex(),
                "token": token, "balance_before": str(before), "balance_after": str(after),
                "totals_after": str(at(base(7), blk)), "ep_cfg_after": str(at(base(5), blk)),
                "totals_before": str(at(base(7), blk - 1)), "ep_cfg_before": str(at(base(5), blk - 1)),
            })
        if len(samples) >= 8:
            break
    print(f"{len(samples)} samples: {sorted(kinds)}", file=sys.stderr)
    OUT.write_text(json.dumps({"head": head, "samples": samples}, indent=1))
    return 0 if samples else 1


if __name__ == "__main__":
    sys.exit(main())
