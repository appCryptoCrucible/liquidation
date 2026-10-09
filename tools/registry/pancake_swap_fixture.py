#!/usr/bin/env python3
"""Record real PancakeSwap V3 Swap logs, with the pool state the chain holds
at the end of the same block, as a test fixture for the Rust log fold
(`crates/liq-router/tests/v3_fork_logs.rs`).

A Pancake V3 Swap event carries two fields more than Uniswap's
(`protocolFeesToken0/1`), so its topic0 differs and Uniswap's decoder never
sees it. The oracle for the fold is the pool itself: when a Swap is the last
event the pool emitted in its block, `slot0()` and `liquidity()` read at that
block are the price, tick and liquidity the event reports.

Usage: MAINNET_RPC_URL=... python tools/registry/pancake_swap_fixture.py
"""
from __future__ import annotations

import json
import os
import sys
from pathlib import Path

from eth_abi import decode
from web3 import Web3

OUT = Path("crates/liq-router/tests/fixtures/pancake_swap_logs.json")
# Pancake V3 WETH/USDC 0.05 % (lmPool set) and 0.25 %-tier / 1 % sample pools.
POOLS = [
    "0x1ac1A8FEaAEa1900C4166dEeed0C11cC10669D36",  # WETH/USDC 0.05 %
    "0x6CA298D2983aB03Aa1dA7679389D955A4eFEE15C",  # WETH/USDT 0.05 %
]
SWAP = Web3.keccak(
    text="Swap(address,address,int256,int256,uint160,uint128,int24,uint128,uint128)"
)
OTHER = [
    Web3.keccak(text=t)
    for t in (
        "Mint(address,address,int24,int24,uint128,uint256,uint256)",
        "Burn(address,int24,int24,uint128,uint256,uint256)",
        "Collect(address,address,int24,int24,uint128,uint128)",
        "Initialize(uint160,int24)",
        "Flash(address,address,uint256,uint256,uint256,uint256)",
    )
]


def main() -> int:
    w3 = Web3(Web3.HTTPProvider(os.environ["MAINNET_RPC_URL"], request_kwargs={"timeout": 60}))
    head = w3.eth.block_number
    sel = lambda s: Web3.keccak(text=s)[:4]  # noqa: E731
    out = []
    for pool in POOLS:
        pool = Web3.to_checksum_address(pool)
        got = []
        for lo in range(head - 400, head - 1, 10):
            try:
                logs = w3.eth.get_logs({"address": pool, "fromBlock": lo, "toBlock": lo + 9})
            except Exception:  # noqa: BLE001 - throttled; skip the window
                continue
            by_block: dict[int, list] = {}
            for lg in logs:
                by_block.setdefault(lg["blockNumber"], []).append(lg)
            for blk, ls in by_block.items():
                last = ls[-1]
                if last["topics"][0] != SWAP:
                    continue
                if any(x["topics"][0] != SWAP for x in ls):
                    continue  # a Mint/Burn in the block: liquidity not comparable
                got.append((blk, last))
        # one swap that is the block's last event, and one with several swaps
        got.sort(key=lambda t: t[0])
        for blk, lg in got[:3]:
            s0 = w3.eth.call({"to": pool, "data": sel("slot0()")}, blk)
            liq = w3.eth.call({"to": pool, "data": sel("liquidity()")}, blk)
            price, tick = decode(["uint160", "int24"], s0[:64])
            out.append({
                "pool": pool, "block": blk,
                "topics": [t.hex() if hasattr(t, "hex") else t for t in lg["topics"]],
                "data": lg["data"].hex() if hasattr(lg["data"], "hex") else lg["data"],
                "slot0_sqrt_price_x96": str(price), "slot0_tick": tick,
                "liquidity": str(decode(["uint128"], liq)[0]),
            })
    if not out:
        print("no swap found", file=sys.stderr)
        return 1
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps({"swap_topic": SWAP.hex(), "logs": out}, indent=1) + "\n", encoding="utf-8")
    print(f"{len(out)} swap logs recorded")
    return 0


if __name__ == "__main__":
    sys.exit(main())
