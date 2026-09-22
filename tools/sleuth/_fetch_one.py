"""Fetch and reconstruct the next uncached tx. Fresh process per tx.

Uses bid_buckets.reconstruct unchanged. Prints immediately so hangs are visible.
"""

from __future__ import annotations

import json
import time
from collections import defaultdict

print("fetch_one start", flush=True)

from bid_buckets import (  # noqa: E402
    OUT,
    addr,
    get,
    process as _unused,  # keep import side effects (IPv4)
    reconstruct,
    SLEEP,
)

BASE = "https://eth.blockscout.com"


def main() -> None:
    cache_path = OUT / "tx_cache.json"
    logs_path = OUT / "logs.json"
    cache = json.loads(cache_path.read_text(encoding="utf-8")) if cache_path.exists() else {}
    logs = json.loads(logs_path.read_text(encoding="utf-8"))
    by_tx: dict[str, list[dict]] = defaultdict(list)
    for row in logs:
        by_tx[addr(row["ev"]["transactionHash"])].append(row)
    pending = [txh for txh in by_tx if txh not in cache]
    print(f"cache={len(cache)} pending={len(pending)}", flush=True)
    if not pending:
        print("done all txs", flush=True)
        return
    txh = pending[0]
    evs = by_tx[txh]
    print(f"fetch {txh}", flush=True)
    try:
        t0 = time.time()
        tx = get(f"{BASE}/api/v2/transactions/{txh}")
        print(f"tx_ok {time.time()-t0:.2f}s", flush=True)
        time.sleep(SLEEP)
        t1 = time.time()
        try:
            internals_raw = get(f"{BASE}/api/v2/transactions/{txh}/internal-transactions")
        except Exception as e:  # noqa: BLE001 — timeout: drop, never estimate coinbase
            rec = {
                "tx": txh,
                "family": evs[0]["family"],
                "drop_reason": "internals_timeout",
                "error": f"{type(e).__name__}",
            }
            cache[txh] = rec
            cache_path.write_text(json.dumps(cache))
            print(f"cached drop=internals_timeout cache={len(cache)} {time.time()-t1:.2f}s", flush=True)
            return
        print(f"int_ok {time.time()-t1:.2f}s", flush=True)
        time.sleep(SLEEP)
        if isinstance(internals_raw, dict):
            internals = internals_raw.get("items") or []
            if internals_raw.get("next_page_params"):
                rec = {
                    "tx": txh,
                    "drop_reason": "internals_paginated_incomplete",
                    "family": evs[0]["family"],
                }
                cache[txh] = rec
                cache_path.write_text(json.dumps(cache))
                print(f"cached drop=internals_paginated_incomplete cache={len(cache)}", flush=True)
                return
        else:
            internals = internals_raw if isinstance(internals_raw, list) else []
        blk = tx.get("block_number")
        miner = None
        if blk is not None:
            t2 = time.time()
            block = get(f"{BASE}/api/v2/blocks/{blk}")
            print(f"blk_ok {time.time()-t2:.2f}s", flush=True)
            time.sleep(SLEEP)
            m = block.get("miner") if isinstance(block, dict) else None
            from bid_buckets import party_hash

            miner = party_hash(m) if m else None
        rec = reconstruct(evs[0]["family"], evs[0]["ev"], tx, internals, miner)
        rec["tx"] = txh
        rec["family"] = evs[0]["family"]
        rec["n_liq_events"] = len(evs)
        if len(evs) > 1:
            rec["multi_liq"] = True
        cache[txh] = rec
        cache_path.write_text(json.dumps(cache))
        print(f"cached drop={rec.get('drop_reason')} cache={len(cache)}", flush=True)
    except Exception as e:  # noqa: BLE001
        rec = {
            "tx": txh,
            "family": evs[0]["family"],
            "drop_reason": f"fetch_error:{type(e).__name__}",
        }
        cache[txh] = rec
        cache_path.write_text(json.dumps(cache))
        print(f"cached drop={rec['drop_reason']} cache={len(cache)}", flush=True)


if __name__ == "__main__":
    main()
