"""Fetch Blockscout liquidation logs via v2 (local v1 getLogs is 429-capped).

Writes tools/sleuth/out/logs.json in the same shape bid_buckets.fetch_logs uses.
Does not invent events; pagination stops at lookback floor. Fail-closed reconstruct
rules stay in bid_buckets.py.
"""

from __future__ import annotations

import json
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

from bid_buckets import LOOKBACK_BLOCKS, OUT, PAGE, POOLS, SLEEP, addr, fetch_tip, hex_int

print("fetch_logs_v2 start", flush=True)


def get(url: str, retries: int = 20) -> object:
    last = None
    for i in range(retries):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "liq-sleuth/1"})
            with urllib.request.urlopen(req, timeout=45) as r:
                return json.loads(r.read().decode("utf-8"))
        except urllib.error.HTTPError as e:
            last = e
            wait = 0.4 * (2 ** min(i, 6))
            if e.code == 429:
                reset = e.headers.get("x-ratelimit-reset")
                if reset is not None:
                    try:
                        wait = max(wait, (int(reset) / 1000.0) + 0.75)
                    except ValueError:
                        wait = max(wait, 8.0)
                print(f"429 sleep {wait:.1f}s", flush=True)
            time.sleep(wait)
        except Exception as e:  # noqa: BLE001
            last = e
            time.sleep(0.4 * (2 ** min(i, 6)))
    raise RuntimeError(f"GET failed {url}: {last}")


def v2_to_ev(item: dict) -> dict:
    a = item.get("address")
    address = a.get("hash") if isinstance(a, dict) else a
    txh = item.get("transaction_hash")
    idx = item.get("index")
    data = item.get("data")
    topics = item.get("topics")
    bn = item.get("block_number")
    if not address or not txh or idx is None or data is None or not topics or bn is None:
        raise ValueError("incomplete v2 log item")
    return {
        "address": address,
        "blockNumber": hex(int(bn)),
        "data": data,
        "logIndex": hex(int(idx)),
        "topics": topics,
        "transactionHash": txh,
    }


def fetch_family(family: str, pool: str, t0: str, start_floor: int) -> list[dict]:
    rows: list[dict] = []
    params: dict[str, object] = {"topic": t0}
    pages = 0
    while True:
        q = urllib.parse.urlencode(params)
        url = f"https://eth.blockscout.com/api/v2/addresses/{pool}/logs?{q}"
        data = get(url)
        time.sleep(SLEEP)
        pages += 1
        if not isinstance(data, dict):
            raise RuntimeError(f"{family}: unexpected logs body")
        items = data.get("items") or []
        stop = False
        for it in items:
            bn = it.get("block_number")
            if bn is None:
                raise RuntimeError(f"{family}: log missing block_number")
            if int(bn) < start_floor:
                stop = True
                continue
            ev = v2_to_ev(it)
            rows.append({"family": family, "pool": addr(pool), "ev": ev})
        nxt = data.get("next_page_params")
        print(
            f"logs {family} page={pages} +{len(items)} kept={len(rows)} next={bool(nxt)}",
            flush=True,
        )
        if stop or not nxt:
            break
        params = dict(nxt)
        if "topic" not in params:
            params["topic"] = t0
    return rows


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    tip = fetch_tip()
    start_floor = max(1, tip - LOOKBACK_BLOCKS)
    print(f"tip={tip} from={start_floor} page_unused={PAGE}", flush=True)
    rows: list[dict] = []
    seen: set[tuple[str, int]] = set()
    for family, pool, t0 in POOLS:
        part = fetch_family(family, pool, t0, start_floor)
        for row in part:
            ev = row["ev"]
            key = (addr(ev["transactionHash"]), hex_int(ev.get("logIndex") or "0x0"))
            if key in seen:
                continue
            seen.add(key)
            rows.append(row)
    path = OUT / "logs.json"
    path.write_text(json.dumps(rows))
    print(f"wrote {len(rows)} logs", flush=True)


if __name__ == "__main__":
    main()
