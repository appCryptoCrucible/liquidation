#!/usr/bin/env python3
"""Clean the snowballed univ3 pools from registry.json in-place.

Applies the frozenset filter that discover.py's fixed code would apply on a fresh
scan: keep a univ3 pool only if token0 OR token1 is in the clean tracked set
(protocol-referenced tokens + HUB_ASSETS). Then drops tokens no longer referenced
by any protocol, pool, oracle, flash_source, or router. Updates registry.meta.json.

This is equivalent to `discover.py --rerun univ3` on a de-contaminated checkpoint,
without the 3M-block RPC sweep.
"""
import json, sys
from pathlib import Path

REG_DIR = Path("registry")
REG = REG_DIR / "registry.json"
META = REG_DIR / "registry.meta.json"

HUB_ASSETS = {
    "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",  # WETH
    "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",  # USDC
    "0xdac17f958d2ee523a2206206994597c13d831ec7",  # USDT
    "0x6b175474e89094c44da98b954eedeac495271d0f",  # DAI
    "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599",  # WBTC
}
ZERO = "0x0000000000000000000000000000000000000000"

def main():
    d = json.loads(REG.read_text(encoding="utf-8"))
    meta = json.loads(META.read_text(encoding="utf-8"))
    protos = d.get("protocols", {})
    pools = d.get("pools", {})
    tokens = d.get("tokens", {})

    # 1. Compute clean_tracked from protocol entries ONLY (not from self.tokens).
    clean = set(HUB_ASSETS)
    for p in protos.values():
        for f in ("loan_token", "collateral_token", "asset", "quote_token"):
            if p.get(f):
                clean.add(p[f].lower())
        for g in (p.get("gems") or []):
            clean.add(g.lower())
    clean.discard(ZERO)

    # 2. Filter pools: keep if token0 OR token1 in clean_tracked (matches discover.py L988).
    kept_pools = {}
    dropped_pools = 0
    for addr, pool in pools.items():
        t0 = pool.get("token0", "").lower()
        t1 = pool.get("token1", "").lower()
        if t0 in clean and t1 in clean:
            kept_pools[addr] = pool
        else:
            dropped_pools += 1

    # 3. Compute referenced tokens from protocols + kept pools + oracles + flash_sources + routers.
    referenced = set()
    for p in protos.values():
        for f in ("loan_token", "collateral_token", "asset", "quote_token", "market",
                   "comptroller", "addresses_provider", "price_oracle", "hub",
                   "silo_config", "irm", "oracle"):
            if p.get(f):
                referenced.add(p[f].lower())
        for g in (p.get("gems") or []):
            referenced.add(g.lower())
        for r in (p.get("receipt_tokens") or []):
            referenced.add(r.lower())
        for o in (p.get("oracle_adapters") or []):
            referenced.add(o.lower())
    for pool in kept_pools.values():
        referenced.add(pool.get("token0", "").lower())
        referenced.add(pool.get("token1", "").lower())
    for o in d.get("oracles", {}).values():
        for f in ("proxy", "aggregator", "underlying"):
            if o.get(f):
                referenced.add(o[f].lower())
    for fs in d.get("flash_sources", {}).values():
        for f in ("source", "asset"):
            if fs.get(f):
                referenced.add(fs[f].lower())
    for r in d.get("routers", {}).values():
        if r.get("address"):
            referenced.add(r["address"].lower())
    referenced |= HUB_ASSETS
    referenced.discard(ZERO)

    # 4. Drop tokens not referenced.
    kept_tokens = {k: v for k, v in tokens.items() if k.lower() in referenced}
    dropped_tokens = len(tokens) - len(kept_tokens)

    # 5. Write back.
    d["pools"] = kept_pools
    d["tokens"] = kept_tokens
    REG.write_text(json.dumps(d, indent=2) + "\n", encoding="utf-8")

    # 6. Update meta.
    meta["token_count"] = len(kept_tokens)
    meta["pool_count"] = len(kept_pools)
    meta["counts_by_protocol"]["univ3"]["instances"] = len(kept_pools)
    # Drop failures for tokens/pools that no longer exist.
    kept_failures = {}
    for k, v in meta.get("failures", {}).items():
        addr = k.split(":")[-1] if ":" in k else ""
        if addr and addr.lower() not in referenced and addr.lower() not in kept_pools:
            continue
        kept_failures[k] = v
    meta["failures"] = kept_failures
    meta.setdefault("notes", []).append(
        f"clean_univ3: frozenset filter applied in-place "
        f"(pools {len(pools)}->{len(kept_pools)}, tokens {len(tokens)}->{len(kept_tokens)}, "
        f"dropped {dropped_pools} pools + {dropped_tokens} noise tokens, "
        f"removed {len(meta.get('failures',{})) - len(kept_failures)} stale failures)"
    )
    META.write_text(json.dumps(meta, indent=2) + "\n", encoding="utf-8")

    print(f"clean_tracked: {len(clean)} tokens")
    print(f"pools: {len(pools)} -> {len(kept_pools)} (dropped {dropped_pools})")
    print(f"tokens: {len(tokens)} -> {len(kept_tokens)} (dropped {dropped_tokens})")
    print(f"failures: {len(meta.get('failures', {}))} (after stale removal)")
    print("DONE")

if __name__ == "__main__":
    main()
