"""Fail-closed reconstruction of winner bid / net from Blockscout.

No guessed prices. A row is kept for β only when:
  - tip is known
  - internal-tx tree was fetched (coinbase sum may be zero)
  - liquidator leftover tokens are only ETH/WETH/USDC/USDT/DAI
  - any stable leftover has that tx's historic ETH/USD
  - net_before_bid > 0
Mixed / unpriceable leftovers are recorded with drop_reason, never filled.
"""

from __future__ import annotations

import http.client
import json
import os
import time
import urllib.parse
from collections import defaultdict
from pathlib import Path
import socket

socket.setdefaulttimeout(45)
_real_getaddrinfo = socket.getaddrinfo


def _ipv4_getaddrinfo(host, port, family=0, type=0, proto=0, flags=0):
    return _real_getaddrinfo(host, port, socket.AF_INET, type, proto, flags)


socket.getaddrinfo = _ipv4_getaddrinfo

BASE = "https://eth.blockscout.com"
AAVE_T0 = "0xe413a321e8681d831f4dbccbca790d2952b56f977908e45be37335533e005286"
MORPHO_T0 = "0xa4946ede45d0c6f06a0f5ce92c9ad3b4751452d2fe0e25010783bcab57a67e41"

POOLS = [
    ("aave-v3", "0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2", AAVE_T0),
    ("spark", "0xC13e21B648A5Ee794902342038FF3aDAB66BE987", AAVE_T0),
    ("morpho-blue", "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb", MORPHO_T0),
]

WETH = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
USDC = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
USDT = "0xdac17f958d2ee523a2206206994597c13d831ec7"
DAI = "0x6b175474e89094c44da98b954eedeac495271d0f"
PRICED = {WETH, USDC, USDT, DAI}

# ~46 days at 12s/block. Expand only after this window yields a fail-closed table.
LOOKBACK_BLOCKS = 400_000
PAGE = 20_000

OUT = Path(__file__).resolve().parent / "out"
# Floor until a response advertises a budget. Not a second sleep on top of it.
SLEEP = 0.2
# Blockscout keyset pages are 50 items. 200 pages = 10_000 rows; beyond that
# the tree is still incomplete and must drop, not reconstruct from a prefix.
MAX_PAGES = 200
PAGE_TIMEOUT = 15.0
GET_RETRIES = 4

# Prior run stubs: first internals page was partial, or the GET died.
# Refetch after the walker exists. Do not refetch priced/unpriced reconstructs.
STALE_DROPS = {
    "internals_paginated_incomplete",
    "internals_timeout",
    "token_transfers_overflow_mixed",
}


class IncompletePages(RuntimeError):
    def __init__(self, kind: str, url: str) -> None:
        self.kind = kind
        super().__init__(f"{kind} page cap {MAX_PAGES} {url}")


def cache_usable(rec: dict) -> bool:
    d = rec.get("drop_reason")
    if d in STALE_DROPS:
        return False
    if isinstance(d, str) and d.startswith("fetch_error:"):
        return False
    return True


_HOST = "eth.blockscout.com"
_conn: http.client.HTTPSConnection | None = None
_pace_interval = SLEEP
_pace_next = 0.0


def _seconds_until(headers: dict[str, str]) -> float | None:
    """Seconds to wait. Absolute reset timestamps are not treated as durations."""
    retry = headers.get("retry-after")
    if retry is not None:
        try:
            return max(0.0, float(retry))
        except ValueError:
            return None
    raw = headers.get("x-ratelimit-reset") or headers.get("ratelimit-reset")
    if raw is None:
        return None
    try:
        v = float(raw)
    except ValueError:
        return None
    now = time.time()
    if v > 1e12:
        return max(0.0, v / 1000.0 - now)
    if v > 1e9:
        return max(0.0, v - now)
    return max(0.0, v)


def _pace_wait() -> None:
    global _pace_next
    now = time.monotonic()
    if now < _pace_next:
        time.sleep(_pace_next - now)
    _pace_next = time.monotonic() + _pace_interval


def _pace_observe(status: int, headers: dict[str, str]) -> float | None:
    """Tighten the gap from the advertised budget. 429 returns how long to sit out."""
    global _pace_interval
    limit = headers.get("x-ratelimit-limit") or headers.get("ratelimit-limit")
    remaining_raw = headers.get("x-ratelimit-remaining") or headers.get("ratelimit-remaining")
    try:
        lim = float(limit) if limit is not None else None
    except ValueError:
        lim = None
    try:
        remaining = float(remaining_raw) if remaining_raw is not None else None
    except ValueError:
        remaining = None
    window = _seconds_until(headers)
    if (
        status != 429
        and lim is not None
        and lim > 0
        and window is not None
        and window > 0
        and (remaining is None or remaining > 1)
    ):
        budget = remaining if remaining is not None else lim
        # Spread the remaining budget across the rest of the window. Floor 20/s.
        _pace_interval = max(window / max(budget, 1.0) * 1.25, 0.05)
    if status == 429 or (remaining is not None and remaining <= 1):
        _pace_interval = min(1.0, max(_pace_interval * 2, SLEEP))
        wait = _seconds_until(headers)
        if wait is None:
            wait = 2.0
        return min(wait + 0.25, 30.0)
    return None


def _http_get(url: str, timeout: float) -> tuple[int, dict[str, str], bytes]:
    """One keep-alive GET. A dead socket is reopened once, then the error propagates."""
    global _conn
    parsed = urllib.parse.urlsplit(url)
    if parsed.netloc and parsed.netloc != _HOST:
        raise RuntimeError(f"unexpected host {parsed.netloc}")
    path = parsed.path or "/"
    if parsed.query:
        path = f"{path}?{parsed.query}"
    last: Exception | None = None
    for attempt in (1, 2):
        if _conn is None:
            _conn = http.client.HTTPSConnection(_HOST, timeout=timeout)
        _conn.timeout = timeout
        try:
            _conn.request(
                "GET",
                path,
                headers={
                    "User-Agent": "liq-sleuth/1",
                    "Accept": "application/json",
                    "Connection": "keep-alive",
                },
            )
            resp = _conn.getresponse()
            body = resp.read()
            headers = {k.lower(): v for k, v in resp.getheaders()}
            return resp.status, headers, body
        except Exception as e:  # noqa: BLE001 — reconnect once; caller retries
            last = e
            try:
                _conn.close()
            except Exception:
                pass
            _conn = None
            if attempt == 2:
                raise
    raise RuntimeError(f"GET transport {url}: {last}")


def get(url: str, retries: int = GET_RETRIES, timeout: float = PAGE_TIMEOUT) -> object:
    last: Exception | None = None
    attempts = 0
    rate_waits = 0
    while attempts < retries:
        _pace_wait()
        try:
            status, headers, body = _http_get(url, timeout)
        except (TimeoutError, socket.timeout, OSError, http.client.HTTPException) as e:
            last = e
            attempts += 1
            wait = min(8.0, 0.5 * (2 ** (attempts - 1)))
            print(
                f"timeout {type(e).__name__} try={attempts}/{retries} sleep {wait:.1f}s {url[:80]}",
                flush=True,
            )
            time.sleep(wait)
            continue
        extra = _pace_observe(status, headers)
        if status == 429:
            rate_waits += 1
            wait = extra if extra is not None else 2.0
            print(f"429 sleep {wait:.1f}s {url[:80]}", flush=True)
            if rate_waits > 8:
                raise RuntimeError(f"GET rate-limited {url}")
            time.sleep(wait)
            continue
        if status in (500, 502, 503, 504):
            last = RuntimeError(f"HTTP {status}")
            attempts += 1
            time.sleep(min(8.0, 0.5 * (2 ** (attempts - 1))))
            continue
        if status != 200:
            raise RuntimeError(f"GET failed {url}: HTTP {status}")
        if extra:
            time.sleep(extra)
        try:
            return json.loads(body.decode("utf-8"))
        except json.JSONDecodeError as e:
            last = e
            attempts += 1
            time.sleep(min(8.0, 0.5 * (2 ** (attempts - 1))))
    raise RuntimeError(f"GET failed {url}: {last}")


def save_cache(path: Path, cache: dict) -> None:
    """Replace the cache only after the new body is complete. A kill mid-write keeps the previous file."""
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(cache), encoding="utf-8")
    os.replace(tmp, path)


def encode_page_params(params: dict, extra: dict | None = None) -> str:
    merged: dict = {}
    if extra:
        merged.update(extra)
    merged.update(params)
    q: list[tuple[str, str]] = []
    for k, v in merged.items():
        if v is None:
            continue
        if isinstance(v, (dict, list)):
            raise RuntimeError(f"nested page param {k}")
        if isinstance(v, bool):
            v = "true" if v else "false"
        q.append((str(k), str(v)))
    return urllib.parse.urlencode(q)


def fetch_pages(
    url: str,
    extra: dict | None = None,
    timeout: float = PAGE_TIMEOUT,
    kind: str = "page",
) -> tuple[list, int]:
    """Walk Blockscout keyset pages. Incomplete walk raises; never returns a prefix."""
    items: list = []
    params: dict | None = None
    pages = 0
    while True:
        pages += 1
        if pages > MAX_PAGES:
            raise IncompletePages(kind, url)
        if params is not None:
            qs = encode_page_params(params, extra)
        elif extra:
            qs = encode_page_params({}, extra)
        else:
            qs = ""
        full = f"{url}?{qs}" if qs else url
        raw = get(full, timeout=timeout)
        if isinstance(raw, list):
            if pages != 1 or params is not None:
                raise RuntimeError(f"list body after page 1 {url}")
            return raw, 1
        if not isinstance(raw, dict):
            raise RuntimeError(f"unexpected page body {url}")
        page = raw.get("items")
        if not isinstance(page, list):
            raise RuntimeError(f"items missing {url}")
        nxt = raw.get("next_page_params")
        if not page:
            if nxt:
                raise RuntimeError(f"empty page with next_page_params {url}")
            return items, pages
        items.extend(page)
        if not nxt:
            return items, pages
        if not isinstance(nxt, dict):
            raise RuntimeError(f"next_page_params not object {url}")
        if params is not None and nxt == params:
            raise RuntimeError(f"pagination cursor stuck {url}")
        params = nxt


def fetch_internals(txh: str) -> tuple[list, int]:
    items, pages = fetch_pages(
        f"{BASE}/api/v2/transactions/{txh}/internal-transactions",
        extra={"include_zero_value": True},
        kind="internals",
    )
    seen: set[int] = set()
    for it in items:
        if not isinstance(it, dict):
            raise RuntimeError(f"internal item not object {txh}")
        idx = it.get("index")
        if idx is None:
            raise RuntimeError(f"internal missing index {txh}")
        try:
            idx_i = int(idx)
        except (TypeError, ValueError) as e:
            raise RuntimeError(f"internal index undecodable {txh}") from e
        if idx_i in seen:
            raise RuntimeError(f"duplicate internal index {idx_i} {txh}")
        seen.add(idx_i)
    return items, pages


def fetch_token_transfers(txh: str) -> tuple[list, int]:
    items, pages = fetch_pages(
        f"{BASE}/api/v2/transactions/{txh}/token-transfers",
        kind="token_transfers",
    )
    seen: set[tuple] = set()
    out: list = []
    for t in items:
        if not isinstance(t, dict):
            raise RuntimeError(f"token transfer not object {txh}")
        th = token_hash(t)
        amt = transfer_amt(t)
        fr, to = party_hash(t.get("from")), party_hash(t.get("to"))
        key = (t.get("log_index"), th, fr, to, amt)
        if key in seen:
            if key[0] is None:
                raise RuntimeError(f"duplicate token transfer without log_index {txh}")
            continue
        seen.add(key)
        out.append(t)
    return out, pages


def hex_int(x: str | int | None) -> int:
    if x is None:
        raise ValueError("missing hex int")
    if isinstance(x, int):
        return x
    return int(x, 16) if str(x).startswith("0x") else int(x)


def addr(x: str) -> str:
    return x.lower()


def topic_addr(t: str) -> str:
    return addr("0x" + t[-40:])


def fetch_tip() -> int:
    b = get(f"{BASE}/api/v2/blocks?type=block")
    items = b.get("items") if isinstance(b, dict) else b
    if not items:
        raise RuntimeError("tip: empty block list")
    return int(items[0]["height"])


def fetch_logs() -> list[dict]:
    tip = fetch_tip()
    start_floor = max(1, tip - LOOKBACK_BLOCKS)
    print(f"tip={tip} from={start_floor}", flush=True)
    rows: list[dict] = []
    seen: set[tuple[str, int]] = set()
    for family, pool, t0 in POOLS:
        start = start_floor
        while start <= tip:
            end = min(start + PAGE - 1, tip)
            url = (
                f"{BASE}/api?module=logs&action=getLogs"
                f"&fromBlock={start}&toBlock={end}"
                f"&address={pool}&topic0={t0}"
            )
            data = get(url)
            if data.get("message") not in ("OK", "No records found") and data.get("status") not in (
                "1",
                "0",
            ):
                raise RuntimeError(f"logs error {family} {start}-{end}: {data}")
            result = data.get("result") or []
            if isinstance(result, str):
                result = []
            for ev in result:
                tx = addr(ev["transactionHash"])
                li = hex_int(ev.get("logIndex") or "0x0")
                key = (tx, li)
                if key in seen:
                    continue
                seen.add(key)
                rows.append({"family": family, "pool": addr(pool), "ev": ev})
            start = end + 1
            print(f"logs {family} {start-PAGE}-{end} +{len(result)} total={len(rows)}", flush=True)
    return rows


def decode_aave(ev: dict) -> dict:
    topics = ev["topics"]
    data = ev["data"][2:]
    if len(data) < 256:
        raise ValueError("short aave data")
    debt_to_cover = int(data[0:64], 16)
    seized = int(data[64:128], 16)
    liquidator = addr("0x" + data[128:192][-40:])
    receive_atoken = int(data[192:256], 16) != 0
    return {
        "coll": topic_addr(topics[1]),
        "debt": topic_addr(topics[2]),
        "user": topic_addr(topics[3]),
        "repay": debt_to_cover,
        "seize": seized,
        "liquidator": liquidator,
        "receive_atoken": receive_atoken,
    }


def decode_morpho(ev: dict) -> dict:
    topics = ev["topics"]
    data = ev["data"][2:]
    repaid = int(data[0:64], 16)
    seized = int(data[128:192], 16) if len(data) >= 192 else None
    return {
        "coll": None,
        "debt": None,
        "user": topic_addr(topics[3]),
        "repay": repaid,
        "seize": seized,
        "liquidator": topic_addr(topics[2]),
        "receive_atoken": False,
        "market_id": topics[1],
    }


def token_hash(t: dict) -> str | None:
    tok = t.get("token") or {}
    h = tok.get("address_hash") or tok.get("address") or t.get("token_address")
    return addr(h) if h else None


def transfer_amt(t: dict) -> int | None:
    total = t.get("total")
    if isinstance(total, dict) and total.get("value") is not None:
        return int(total["value"])
    if t.get("amount") is not None:
        try:
            return int(t["amount"])
        except ValueError:
            return None
    return None


def party_hash(p: object) -> str | None:
    if isinstance(p, dict):
        h = p.get("hash")
        return addr(h) if h else None
    if isinstance(p, str):
        return addr(p)
    return None


def reconstruct(family: str, ev: dict, tx: dict, internals: list, miner: str | None) -> dict:
    drop = None
    decoded = decode_morpho(ev) if family == "morpho-blue" else decode_aave(ev)
    liq = decoded["liquidator"]

    fee = tx.get("fee") or {}
    try:
        gas_used = int(tx["gas_used"])
        gas_price = int(tx["gas_price"])
        base = int(tx["base_fee_per_gas"])
    except (KeyError, TypeError, ValueError):
        return {"drop_reason": "receipt_incomplete", "decoded": decoded}

    if gas_price < base:
        return {"drop_reason": "effective_below_base", "decoded": decoded}

    tip = (gas_price - base) * gas_used
    if tx.get("priority_fee") is not None:
        pf = int(tx["priority_fee"])
        if pf != tip:
            # Blockscout's field is authoritative when present; mismatch = withhold
            if abs(pf - tip) > 1:
                return {
                    "drop_reason": "priority_fee_mismatch",
                    "decoded": decoded,
                    "tip_computed": str(tip),
                    "priority_fee": str(pf),
                }
        tip = pf

    coinbase = 0
    if miner is None:
        drop = "miner_unknown"
    else:
        miner = addr(miner)
        for it in internals:
            to = party_hash(it.get("to"))
            typ = (it.get("type") or it.get("tx_types") or "")
            if isinstance(typ, list):
                typ = ",".join(typ)
            # call-like value transfers only
            val = it.get("value")
            if val is None:
                continue
            try:
                v = int(val)
            except (TypeError, ValueError):
                return {"drop_reason": "internal_value_undecodable", "decoded": decoded}
            if to == miner and v:
                coinbase += v

    if drop:
        return {"drop_reason": drop, "decoded": decoded, "tip": str(tip)}

    bid = tip + coinbase
    hist = tx.get("historic_exchange_rate")
    try:
        eth_usd = float(hist) if hist not in (None, "") else None
    except (TypeError, ValueError):
        eth_usd = None

    transfers = tx.get("token_transfers") or []
    overflow = bool(tx.get("token_transfers_overflow"))
    if overflow:
        return {
            "drop_reason": "token_transfers_overflow_mixed",
            "decoded": decoded,
            "tip": str(tip),
            "coinbase": str(coinbase),
            "bid": str(bid),
            "eth_usd": eth_usd,
        }

    net_tok: dict[str, int] = defaultdict(int)
    for t in transfers:
        th = token_hash(t)
        amt = transfer_amt(t)
        if th is None or amt is None:
            return {
                "drop_reason": "token_transfer_undecodable",
                "decoded": decoded,
                "bid": str(bid),
            }
        fr, to = party_hash(t.get("from")), party_hash(t.get("to"))
        if to == liq:
            net_tok[th] += amt
        if fr == liq:
            net_tok[th] -= amt

    eth_net = 0
    for it in internals:
        v = it.get("value")
        if not v:
            continue
        v = int(v)
        fr, to = party_hash(it.get("from")), party_hash(it.get("to"))
        if to == liq:
            eth_net += v
        if fr == liq:
            # Subtract miner sends too. bid is added back as tip+coinbase so
            # before = leftover_after_all_sends + bid. Skipping the miner
            # debit double-counts coinbase (exact 5000 bps when tip is 0
            # and leftover tokens are 0).
            eth_net -= v

    leftover = {k: v for k, v in net_tok.items() if v != 0}
    unpriced = [k for k in leftover if k not in PRICED]
    if unpriced:
        return {
            "drop_reason": "unpriced_leftover",
            "unpriced": unpriced,
            "decoded": decoded,
            "bid": str(bid),
            "tip": str(tip),
            "coinbase": str(coinbase),
            "eth_usd": eth_usd,
        }

    need_usd = any(k in (USDC, USDT, DAI) and leftover[k] for k in leftover)
    if need_usd and eth_usd is None:
        return {
            "drop_reason": "missing_historic_eth_usd",
            "decoded": decoded,
            "bid": str(bid),
        }

    def stable_to_wei(token: str, amt: int) -> int:
        if eth_usd is None or eth_usd <= 0:
            raise ValueError("eth_usd")
        # USD * 1e18 / eth_usd. USDC/USDT 6 dec, DAI 18.
        if token in (USDC, USDT):
            usd_1e18 = amt * 10**12
        else:
            usd_1e18 = amt
        return (usd_1e18 * 10**18) // int(eth_usd * 10**18)

    after = eth_net + leftover.get(WETH, 0)
    try:
        if leftover.get(USDC, 0):
            after += stable_to_wei(USDC, leftover[USDC])
        if leftover.get(USDT, 0):
            after += stable_to_wei(USDT, leftover[USDT])
        if leftover.get(DAI, 0):
            after += stable_to_wei(DAI, leftover[DAI])
    except ValueError:
        return {"drop_reason": "stable_convert_failed", "decoded": decoded, "bid": str(bid)}

    before = after + bid
    if before <= 0:
        return {
            "drop_reason": "nonpositive_net_before_bid",
            "decoded": decoded,
            "bid": str(bid),
            "net_after": str(after),
            "tip": str(tip),
            "coinbase": str(coinbase),
            "eth_usd": eth_usd,
        }

    # Size in ETH wei from the event when we can price both legs.
    size = None
    size_src = None
    coll, debt = decoded.get("coll"), decoded.get("debt")
    if coll == WETH:
        size = decoded["seize"]
        size_src = "seize_weth"
    elif debt == WETH:
        size = decoded["repay"]
        size_src = "repay_weth"
    elif debt in (USDC, USDT, DAI) and eth_usd:
        try:
            size = stable_to_wei(debt, decoded["repay"])
            size_src = "repay_stable"
        except ValueError:
            size = None
    else:
        size = None
        size_src = None

    beta_bps = (bid * 10_000) // before
    return {
        "drop_reason": None,
        "decoded": decoded,
        "bid": str(bid),
        "tip": str(tip),
        "coinbase": str(coinbase),
        "net_after": str(after),
        "net_before": str(before),
        "beta_bps": int(beta_bps),
        "size_eth_wei": str(size) if size is not None else None,
        "size_src": size_src,
        "eth_usd": eth_usd,
        "gas_used": gas_used,
        "base_fee": base,
        "position": tx.get("position"),
        "block": tx.get("block_number"),
        "timestamp": tx.get("timestamp"),
        "from": party_hash(tx.get("from")),
        "to": party_hash(tx.get("to")),
        "transfer_n": len(transfers),
    }


def process(logs: list[dict], cache: dict, max_new: int | None = None) -> tuple[list[dict], bool]:
    out: list[dict] = []
    by_tx: dict[str, list[dict]] = defaultdict(list)
    for row in logs:
        by_tx[addr(row["ev"]["transactionHash"])].append(row)

    n = 0
    new = 0
    remaining = False
    cache_path = OUT / "tx_cache.json"
    total = len(by_tx)
    miners: dict[int, str | None] = {}
    for txh, evs in by_tx.items():
        n += 1
        if txh in cache and cache_usable(cache[txh]):
            rec = cache[txh]
            out.append(rec)
            continue
        if max_new is not None and new >= max_new:
            remaining = True
            break
        new += 1
        t0 = time.perf_counter()
        print(f"fetch {n}/{total} {txh[:18]}", flush=True)
        try:
            tx = get(f"{BASE}/api/v2/transactions/{txh}")
            internals, internal_pages = fetch_internals(txh)
            if bool(tx.get("token_transfers_overflow")):
                transfers, _tpages = fetch_token_transfers(txh)
                tx = dict(tx)
                tx["token_transfers"] = transfers
                tx["token_transfers_overflow"] = False
            blk = tx.get("block_number")
            miner = None
            if blk is not None:
                blk_i = int(blk)
                if blk_i in miners:
                    miner = miners[blk_i]
                else:
                    block = get(f"{BASE}/api/v2/blocks/{blk_i}")
                    m = block.get("miner") if isinstance(block, dict) else None
                    miner = party_hash(m) if m else None
                    miners[blk_i] = miner
            # one reconstruct per tx (first event); extra events recorded
            rec = reconstruct(evs[0]["family"], evs[0]["ev"], tx, internals, miner)
            rec["tx"] = txh
            rec["family"] = evs[0]["family"]
            rec["n_liq_events"] = len(evs)
            rec["internal_n"] = len(internals)
            rec["internal_pages"] = internal_pages
            if len(evs) > 1:
                rec["multi_liq"] = True
            cache[txh] = rec
        except IncompletePages as e:
            rec = {
                "tx": txh,
                "family": evs[0]["family"],
                "drop_reason": (
                    "internals_paginated_incomplete"
                    if e.kind == "internals"
                    else "token_transfers_overflow_mixed"
                ),
            }
            cache[txh] = rec
        except Exception as e:  # noqa: BLE001
            rec = {
                "tx": txh,
                "family": evs[0]["family"],
                "drop_reason": f"fetch_error:{type(e).__name__}",
            }
            cache[txh] = rec
        save_cache(cache_path, cache)
        dt = time.perf_counter() - t0
        print(
            f"wrote {n}/{total} {dt:.1f}s {txh[:18]} drop={rec.get('drop_reason')} cache={len(cache)}",
            flush=True,
        )
        out.append(rec)
    return out, remaining


def deciles(kept: list[dict]) -> list[dict]:
    sized = [r for r in kept if r.get("size_eth_wei") and r.get("beta_bps") is not None]
    sized.sort(key=lambda r: int(r["size_eth_wei"]))
    if len(sized) < 10:
        return []
    n = len(sized)
    buckets = []
    for i in range(10):
        lo = (i * n) // 10
        hi = ((i + 1) * n) // 10
        part = sized[lo:hi]
        if not part:
            continue
        bps = sorted(int(r["beta_bps"]) for r in part)
        def pct(p: float) -> int:
            if not bps:
                raise ValueError("empty")
            idx = min(len(bps) - 1, max(0, int(round((p / 100) * (len(bps) - 1)))))
            return bps[idx]
        buckets.append(
            {
                "decile": i + 1,
                "n": len(part),
                "size_lo_eth": int(part[0]["size_eth_wei"]) / 1e18,
                "size_hi_eth": int(part[-1]["size_eth_wei"]) / 1e18,
                "beta_bps_min": bps[0],
                "beta_bps_p25": pct(25),
                "beta_bps_p50": pct(50),
                "beta_bps_p75": pct(75),
                "beta_bps_max": bps[-1],
                "bid_wei_p50": sorted(int(r["bid"]) for r in part)[len(part) // 2],
                "net_wei_p50": sorted(int(r["net_before"]) for r in part)[len(part) // 2],
            }
        )
    return buckets


def main() -> None:
    import sys

    max_new = None
    if "--max-new" in sys.argv:
        max_new = int(sys.argv[sys.argv.index("--max-new") + 1])
    OUT.mkdir(parents=True, exist_ok=True)
    cache_path = OUT / "tx_cache.json"
    cache = json.loads(cache_path.read_text()) if cache_path.exists() else {}
    logs_path = OUT / "logs.json"
    if logs_path.exists():
        logs = json.loads(logs_path.read_text())
        print(f"loaded {len(logs)} cached logs cache={len(cache)}", flush=True)
    else:
        logs = fetch_logs()
        logs_path.write_text(json.dumps(logs))
        print(f"wrote {len(logs)} logs", flush=True)
    rows, remaining = process(logs, cache, max_new=max_new)
    save_cache(cache_path, cache)
    if remaining:
        print(f"batch done cache={len(cache)} remaining=yes", flush=True)
        return
    (OUT / "rows.json").write_text(json.dumps(rows, indent=2))
    reasons: dict[str, int] = defaultdict(int)
    kept = []
    for r in rows:
        d = r.get("drop_reason")
        reasons[d or "kept"] += 1
        if d is None and r.get("beta_bps") is not None:
            kept.append(r)
    buckets = deciles(kept)
    summary = {
        "n_logs": len(logs),
        "n_txs": len(rows),
        "n_kept": len(kept),
        "drop_reasons": dict(reasons),
        "lookback_blocks": LOOKBACK_BLOCKS,
        "source": "eth.blockscout.com",
        "buckets": buckets,
        "kept_preview": [
            {
                "tx": r["tx"],
                "family": r["family"],
                "beta_bps": r["beta_bps"],
                "bid_eth": int(r["bid"]) / 1e18,
                "net_eth": int(r["net_before"]) / 1e18,
                "size_eth": int(r["size_eth_wei"]) / 1e18 if r.get("size_eth_wei") else None,
                "size_src": r.get("size_src"),
                "block": r.get("block"),
                "position": r.get("position"),
            }
            for r in sorted(kept, key=lambda x: int(x.get("size_eth_wei") or "0"))
        ],
    }
    (OUT / "summary.json").write_text(json.dumps(summary, indent=2))
    print(json.dumps({"n_logs": len(logs), "n_kept": len(kept), "reasons": dict(reasons)}, indent=2))


if __name__ == "__main__":
    main()
