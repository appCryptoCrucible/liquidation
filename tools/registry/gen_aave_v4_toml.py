#!/usr/bin/env python3
"""Generate config/protocols/aave-v4.toml from the chain and the registry.

Registry `aave-v4` entries are hubs (`kind = hub`) and spokes (`kind =
spoke`). At one pinned block, per spoke (aave/aave-v4 @ 40232a0a):
  oracle   = Spoke.ORACLE()
  reserves = 0..Spoke.getReserveCount(); Spoke.getReserve(id) gives the
             underlying and its hub; AaveOracle.getReserveSource(id) the source.
  debt     = Spoke.getReserveTotalDebt(id) at AaveOracle.getReservesPrices
             (8-decimal USD) — used only to rank spokes.

The adapter tracks at most 32 spokes (`SpokeFlags::MAX_SPOKES`): the 32 with
the most debt are kept, the rest are listed in the header. Every hub a kept
spoke uses is kept.

Ids reproduce `liq_config::Intern::from_registry` (see gen_aave_v3_toml.py).
A reserve whose token is not in the registry has no intern id: its spoke is
still tracked (the reserve fails closed as unpriced) and it is reported.

Usage: MAINNET_RPC_URL=... python tools/registry/gen_aave_v4_toml.py [--dry-run]
"""
from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path

from eth_abi import decode, encode

sys.path.insert(0, str(Path(__file__).parent))
from discover_exits import Chain, sel, word  # noqa: E402
from gen_aave_v3_toml import intern_ids  # noqa: E402

REG = Path("registry/registry.json")
OUT = Path("config/protocols/aave-v4.toml")
MAX_SPOKES = 32
RESERVE_T = "(address,address,uint16,uint8,uint24,uint8,uint32)"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()
    chain = Chain(os.environ["MAINNET_RPC_URL"])
    reg = json.loads(REG.read_text(encoding="utf-8"))
    tokens, protocols, markets, feeds = intern_ids(reg)
    entries = {k: p for k, p in reg["protocols"].items() if p["family"] == "aave-v4"}
    hub_keys = {p["market"].lower(): k for k, p in entries.items() if p.get("kind") == "hub"}
    spoke_keys = {p["market"].lower(): k for k, p in entries.items() if p.get("kind") == "spoke"}

    spokes = []
    not_lending = []
    for spoke, key in sorted(spoke_keys.items()):
        r = chain.multicall([(spoke, sel("ORACLE()")), (spoke, sel("getReserveCount()"))])
        oracle, n = word(r[0], "address"), word(r[1])
        if oracle is None or n is None:
            # Registered on a hub but not a lending Spoke (no ORACLE /
            # reserves): nothing to liquidate there.
            not_lending.append(spoke)
            continue
        oracle = oracle.lower()
        calls = []
        for i in range(n):
            calls.append((spoke, sel("getReserve(uint256)") + encode(["uint256"], [i])))
            calls.append((oracle, sel("getReserveSource(uint256)") + encode(["uint256"], [i])))
            calls.append((spoke, sel("getReserveTotalDebt(uint256)") + encode(["uint256"], [i])))
        res = chain.multicall(calls) if calls else []
        prices_raw = chain.multicall([(oracle, sel("getReservesPrices(uint256[])")
                                       + encode(["uint256[]"], [list(range(n))]))])[0] if n else (True, b"")
        prices = decode(["uint256[]"], prices_raw[1])[0] if n and prices_raw[0] else [0] * n
        reserves = []
        debt_usd = 0.0
        for i in range(n):
            ok, raw = res[3 * i]
            src = word(res[3 * i + 1], "address")
            debt = word(res[3 * i + 2]) or 0
            if not ok or src is None:
                print(f"{spoke}: reserve {i} reads failed", file=sys.stderr)
                return 1
            underlying, hub, _asset_id, decimals, *_ = decode([RESERVE_T], raw)[0]
            debt_usd += debt * (prices[i] if i < len(prices) else 0) / 10 ** (decimals + 8)
            reserves.append({"id": i, "underlying": underlying.lower(), "hub": hub.lower(),
                             "source": src.lower(), "decimals": decimals})
        spokes.append({"address": spoke, "key": key, "oracle": oracle, "reserves": reserves,
                       "debt_usd": debt_usd})

    spokes.sort(key=lambda s: -s["debt_usd"])
    kept, dropped = spokes[:MAX_SPOKES], spokes[MAX_SPOKES:]
    used_hubs = sorted({r["hub"] for s in kept for r in s["reserves"]})
    unknown_hubs = [h for h in used_hubs if h not in hub_keys]
    if unknown_hubs:
        print(f"hubs not in registry: {unknown_hubs}", file=sys.stderr)
        return 1

    assets: dict[str, dict] = {}
    sources = []
    missing = []
    extra_feeds: dict[str, int] = {}
    for s in kept:
        for r in s["reserves"]:
            u = r["underlying"]
            if u not in tokens:
                missing.append((s["address"], r["id"], u))
                continue
            src = r["source"]
            if src not in feeds and src not in extra_feeds:
                extra_feeds[src] = len(feeds) + len(extra_feeds)
            assets.setdefault(u, {"underlying": u, "asset": tokens[u],
                                  "feed": feeds.get(src, extra_feeds.get(src))})
            sources.append({"spoke": s["address"], "reserve_id": r["id"], "source": src})

    lines = [
        "# Aave V4 mainnet (family `aave-v4` in registry/registry.json).",
        "# Generated by tools/registry/gen_aave_v4_toml.py — do not hand-edit.",
        f"# Values read on-chain at block {chain.block} (aave/aave-v4 @ 40232a0a views).",
        f"# {len(kept)} of {len(spokes)} spokes kept (adapter max {MAX_SPOKES}), ranked by debt USD.",
        f"# Feed ids >= {len(feeds)} label sources absent from registry.oracles (informational).",
    ]
    if not_lending:
        lines.append("# Hub-registered addresses that are not lending Spokes (no ORACLE()):")
        lines += [f"#   {a}" for a in not_lending]
    if dropped:
        lines.append("# Spokes not tracked (lowest debt):")
        lines += [f"#   {s['address']} debt ${s['debt_usd']:,.0f}" for s in dropped]
    if missing:
        lines.append("# Reserves without an intern id (token not in registry; unpriced):")
        lines += [f"#   {sp} reserve {i} {u}" for sp, i, u in missing]
    lines += ["", f"protocol = {protocols['aave-v4']}", f"pinned_through = {chain.block}", ""]
    for h in used_hubs:
        lines += ["[[hubs]]", f'address = "{h}"', f"market = {markets[hub_keys[h]]}", ""]
    for s in kept:
        lines += ["[[spokes]]", f'address = "{s["address"]}"', f"market = {markets[s['key']]}",
                  f'oracle = "{s["oracle"]}"', f"# debt ${s['debt_usd']:,.0f}", ""]
    for a in sorted(assets.values(), key=lambda a: a["asset"]):
        lines += ["[[assets]]", f'underlying = "{a["underlying"]}"', f"asset = {a['asset']}",
                  f"feed = {a['feed']}", ""]
    for p in sources:
        lines += ["[[price_sources]]", f'spoke = "{p["spoke"]}"', f"reserve_id = {p['reserve_id']}",
                  f'source = "{p["source"]}"', ""]
    total = sum(s["debt_usd"] for s in spokes)
    kept_usd = sum(s["debt_usd"] for s in kept)
    print(f"{len(not_lending)} non-lending; {len(spokes)} spokes (${total:,.0f} debt); kept {len(kept)} (${kept_usd:,.0f}); "
          f"{len(used_hubs)} hubs, {len(assets)} assets, {len(sources)} sources, {len(missing)} unpriced")
    if args.dry_run:
        return 0
    OUT.write_text("\n".join(lines), encoding="utf-8")
    print(f"wrote {OUT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
