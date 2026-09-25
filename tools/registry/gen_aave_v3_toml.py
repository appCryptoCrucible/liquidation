#!/usr/bin/env python3
"""Generate config/protocols/aave-v3.toml from the chain and the registry.

Every `aave-v3` registry entry is one pool. At one pinned block, per pool:
  provider  = registry `addresses_provider`; oracle / configurator / sentinel
              read from it (oracle must equal registry `price_oracle`);
              sequencer oracle from the sentinel when one is set.
  reserves  = Pool.getReservesList(); decimals from getConfiguration bits
              48..55; Oracle.getSourceOfAsset(reserve) is the price source.
  oracle decimals = log10(Oracle.BASE_CURRENCY_UNIT()).

Ids reproduce `liq_config::Intern::from_registry` exactly:
  AssetId   = registry/asset-ids.json (append-only; not the token's sort index)
  ProtocolId= index of the family in the sorted family list
  MarketId  = index of the entry in registry `protocols` (BTreeMap: key order)
  FeedId    = index of the source in registry `oracles` (BTreeMap: proxy order);
              a source the registry does not list gets a file-local label
              above that range. Nothing joins on `MarketRow.price_feed` — health
              prices come from the price vector by AssetId.

Pool revision 11 (aave-v3-origin 3.7, the adapter's pin 8305565) removed
isolation mode and siloed borrowing: every reserve is siloed = false,
isolated = false, debt_ceiling = 0. The generator refuses a pool at any other
revision. Liquidation constants are LiquidationLogic's at that pin (and
aave.com/help/borrowing/liquidations): close factor 50 % above HF 0.95 when
both sides are >= $2,000; else 100 %; a partial must leave >= $1,000.

A reserve whose token is not in the registry is left out and reported
(no intern id to give it).

Usage: MAINNET_RPC_URL=... python tools/registry/gen_aave_v3_toml.py [--dry-run]
"""
from __future__ import annotations

import argparse
import json
import math
import os
import sys
from pathlib import Path

from eth_abi import decode, encode

sys.path.insert(0, str(Path(__file__).parent))
from discover_exits import Chain, sel, word  # noqa: E402

REG = Path("registry/registry.json")
LEDGER = Path("registry/asset-ids.json")
OUT = Path("config/protocols/aave-v3.toml")
ZERO = "0x0000000000000000000000000000000000000000"
POOL_REVISION = 11

CLOSE_FACTOR_BPS = 5_000
CLOSE_FACTOR_HF_WAD = 950_000_000_000_000_000
MIN_BASE_MAX_CLOSE = 2_000 * 10**8


def intern_ids(reg: dict):
    ledger = json.loads(LEDGER.read_text(encoding="utf-8"))
    by_addr = {a.lower(): int(i) for a, i in ledger["ids"].items()}
    missing = [a for a in reg["tokens"] if a.lower() not in by_addr]
    if missing:
        raise SystemExit(
            f"asset-ids.json has no id for {len(missing)} registry tokens "
            f"(first {missing[0]}); refusing to renumber by sort order"
        )
    tokens = {a: by_addr[a.lower()] for a in reg["tokens"]}
    families = sorted({p["family"] for p in reg["protocols"].values()})
    protocols = {f: i for i, f in enumerate(families)}
    markets = {k: i for i, k in enumerate(sorted(reg["protocols"]))}
    feeds = {a: i for i, a in enumerate(sorted(reg["oracles"], key=lambda a: int(a, 16)))}
    return tokens, protocols, markets, feeds


def addr(chain: Chain, to: str, sig: str) -> str:
    r = word(chain.multicall([(to, sel(sig))])[0], "address")
    if r is None:
        raise RuntimeError(f"{sig} failed on {to}")
    return r.lower()


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()
    chain = Chain(os.environ["MAINNET_RPC_URL"])
    reg = json.loads(REG.read_text(encoding="utf-8"))
    tokens, protocols, markets, feeds = intern_ids(reg)

    pools = []
    assets: dict[str, dict] = {}
    sources = []
    extra_feeds: dict[str, int] = {}
    missing = []
    oracle_decimals = None
    for key in sorted(k for k, p in reg["protocols"].items() if p["family"] == "aave-v3"):
        entry = reg["protocols"][key]
        pool = entry["market"].lower()
        provider = entry["addresses_provider"].lower()
        rev = word(chain.multicall([(pool, sel("POOL_REVISION()"))])[0])
        if rev != POOL_REVISION:
            print(f"refusing {pool}: POOL_REVISION {rev} != {POOL_REVISION}", file=sys.stderr)
            return 1
        oracle = addr(chain, provider, "getPriceOracle()")
        if oracle != entry["price_oracle"].lower():
            print(f"{pool}: provider oracle {oracle} != registry {entry['price_oracle']}", file=sys.stderr)
            return 1
        configurator = addr(chain, provider, "getPoolConfigurator()")
        sentinel = addr(chain, provider, "getPriceOracleSentinel()")
        sequencer = addr(chain, sentinel, "getSequencerOracle()") if sentinel != ZERO else ZERO
        unit = word(chain.multicall([(oracle, sel("BASE_CURRENCY_UNIT()"))])[0])
        dec = round(math.log10(unit))
        if 10**dec != unit or (oracle_decimals is not None and dec != oracle_decimals):
            print(f"{pool}: BASE_CURRENCY_UNIT {unit} not a shared power of ten", file=sys.stderr)
            return 1
        oracle_decimals = dec
        pools.append({
            "address": pool, "market": markets[key], "oracle": oracle, "provider": provider,
            "configurator": configurator, "sentinel": sentinel, "sequencer_oracle": sequencer,
        })

        raw = chain.multicall([(pool, sel("getReservesList()"))])[0]
        reserves = [a.lower() for a in decode(["address[]"], raw[1])[0]]
        calls = []
        for r in reserves:
            calls.append((pool, sel("getConfiguration(address)") + encode(["address"], [r])))
            calls.append((oracle, sel("getSourceOfAsset(address)") + encode(["address"], [r])))
        res = chain.multicall(calls)
        for i, r in enumerate(reserves):
            cfg = word(res[2 * i])
            src = word(res[2 * i + 1], "address")
            if cfg is None or src is None:
                print(f"{pool}: reads failed for reserve {r}", file=sys.stderr)
                return 1
            src = src.lower()
            if r not in tokens:
                missing.append((pool, r))
                continue
            decimals = (cfg >> 48) & 0xFF
            if src not in feeds and src not in extra_feeds:
                extra_feeds[src] = len(feeds) + len(extra_feeds)
            feed = feeds.get(src, extra_feeds.get(src))
            if r in assets and assets[r]["decimals"] != decimals:
                print(f"{r}: decimals disagree across pools", file=sys.stderr)
                return 1
            assets.setdefault(r, {"underlying": r, "asset": tokens[r], "feed": feed, "decimals": decimals})
            sources.append({"pool": pool, "underlying": r, "source": src})
        print(f"{key}: {len(reserves)} reserves, market {markets[key]}")

    lines = [
        "# Aave V3 mainnet (family `aave-v3` in registry/registry.json): Core, Prime,",
        "# EtherFi. Generated by tools/registry/gen_aave_v3_toml.py — do not hand-edit.",
        f"# Values read on-chain at block {chain.block}. Pools at POOL_REVISION {POOL_REVISION}",
        "# (aave-v3-origin 3.7 @ 8305565): no isolation mode / siloed borrowing.",
        "# Liquidation constants: LiquidationLogic @ 8305565 = aave.com liquidation docs.",
        "# Asset ids are registry/asset-ids.json. Market and feed ids are sort order. Feed ids",
        f"# >= {len(feeds)} label sources absent from registry.oracles (informational).",
    ]
    if missing:
        lines.append(f"# Left out (token not in registry): {len(missing)} reserve(s):")
        lines += [f"#   {p} {r}" for p, r in missing]
    lines += ["", f"protocol = {protocols['aave-v3']}", f"pinned_through = {chain.block}", ""]
    for p in pools:
        lines += ["[[pools]]"] + [
            f'{k} = "{v}"' if isinstance(v, str) else f"{k} = {v}" for k, v in p.items()
        ] + [""]
    lines += [
        "[liquidation]",
        f"close_factor_bps = {CLOSE_FACTOR_BPS}",
        f"close_factor_hf_wad = {CLOSE_FACTOR_HF_WAD}",
        f"min_base_max_close = {MIN_BASE_MAX_CLOSE}",
        f"oracle_decimals = {oracle_decimals}",
        "",
    ]
    for a in sorted(assets.values(), key=lambda a: a["asset"]):
        lines += [
            "[[assets]]",
            f'underlying = "{a["underlying"]}"',
            f"asset = {a['asset']}",
            f"feed = {a['feed']}",
            "siloed = false",
            "isolated = false",
            "debt_ceiling = 0",
            f"decimals = {a['decimals']}",
            "",
        ]
    for s in sources:
        lines += ["[[price_sources]]", f'pool = "{s["pool"]}"', f'underlying = "{s["underlying"]}"',
                  f'source = "{s["source"]}"', ""]
    text = "\n".join(lines)
    print(f"{len(pools)} pools, {len(assets)} assets, {len(sources)} sources, {len(missing)} left out")
    if args.dry_run:
        return 0
    OUT.write_text(text, encoding="utf-8")
    print(f"wrote {OUT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
