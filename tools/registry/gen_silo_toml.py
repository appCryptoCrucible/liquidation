#!/usr/bin/env python3
"""Generate config/protocols/silo-v2.toml from the chain and the registry.

Every registry `silo-v2` market (all 252 silos, not an admission list: plan 1B
tracks every market) is read at one pinned block, grouped by its
`SiloConfig`:

  pairs     = per SiloConfig, `getSilos()` (slot 0, slot 1) and
              `getConfig(silo)` for each side: token, share tokens, solvency
              oracle, lt, liquidation fee and target LTV, `daoFee +
              deployerFee` (the interest the depositors do not get), hook
              receiver (one per pair; both sides must agree). `market` is the registry's
              intern id of the pair's slot-1 silo (registry market ids follow
              the sorted protocol keys), which keeps the three pairs bound
              before this generator their ids.
  factories = each pair's silo `factory()`: the `NewSilo` that lists it.
  assets    = every pair token (all registry tokens; one missing stops the
              run, as `discover_unwraps.py` and the registry must add it).

`feed` is informational (Silo prices health through each pair's own solvency
oracles, read every block): FeedId 0, as Morpho and Euler do.

Usage: MAINNET_RPC_URL=... python tools/registry/gen_silo_toml.py [--dry-run]
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

REG = Path("registry/registry.json")
LEDGER = Path("registry/asset-ids.json")
OUT = Path("config/protocols/silo-v2.toml")
PROTOCOL = 6
ZERO = "0x" + "0" * 40
# `ISiloConfig.ConfigData` @ pin 570a668a.
CONFIG = ("(uint256,uint256,address,address,address,address,address,address,address,address,"
          "uint256,uint256,uint256,uint256,uint256,address,bool)")
# Pairs bound before this generator: their market ids must not move.
KEPT = {
    "0x10930071079d2cfff317aba9d2dd997309dd9985": 3427,
    "0xb0b37f551e18c540cf7748589d24589945fc1f61": 3254,
    "0x74b21d458b9d5cf59f4b4e10a2e829c221670ee3": 3468,
}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()
    chain = Chain(os.environ["MAINNET_RPC_URL"])
    reg = json.loads(REG.read_text(encoding="utf-8"))
    ledger = json.loads(LEDGER.read_text(encoding="utf-8"))
    ids = {a.lower(): int(i) for a, i in ledger["ids"].items()}
    market_of = {k: i for i, k in enumerate(sorted(reg["protocols"]))}
    silos = {v["market"].lower(): v for v in reg["protocols"].values() if v["family"] == "silo-v2"}
    configs = sorted({v["silo_config"].lower() for v in silos.values()})

    got = chain.multicall([(c, sel("getSilos()")) for c in configs])
    pairs = []
    for c, (ok, raw) in zip(configs, got):
        if not ok:
            raise SystemExit(f"getSilos() reverted on {c}")
        s0, s1 = (a.lower() for a in decode(["address", "address"], raw))
        if s0 not in silos or s1 not in silos:
            raise SystemExit(f"config {c}: a silo is not a registry market")
        pairs.append({"config": c, "silos": (s0, s1)})
    calls = []
    for p in pairs:
        for s in p["silos"]:
            calls.append((p["config"], sel("getConfig(address)") + encode(["address"], [s])))
            calls.append((s, sel("factory()")))
    res = iter(chain.multicall(calls))
    factories: list[str] = []
    for p in pairs:
        p["sides"] = []
        for s in p["silos"]:
            ok, raw = next(res)
            if not ok:
                raise SystemExit(f"getConfig({s}) reverted on {p['config']}")
            c = decode([CONFIG], raw)[0]
            f = (word(next(res), "address") or ZERO).lower()
            if f == ZERO:
                raise SystemExit(f"silo {s}: no factory()")
            if f not in factories:
                factories.append(f)
            if c[2].lower() != s:
                raise SystemExit(f"getConfig({s}).silo is {c[2]}")
            p["sides"].append({
                "silo": s, "token": c[3].lower(), "protected_share": c[4].lower(),
                "collateral_share": c[5].lower(), "debt_share": c[6].lower(),
                "solvency_oracle": c[7].lower(), "lt": c[11], "liquidation_target_ltv": c[12],
                "liquidation_fee": c[13], "hook": c[15].lower(), "interest_fee": c[0] + c[1],
            })
        a, b = p["sides"]
        if a["hook"] != b["hook"] or a["hook"] == ZERO:
            raise SystemExit(f"config {p['config']}: hooks {a['hook']} / {b['hook']}")
        for side in p["sides"]:
            if side["collateral_share"] != side["silo"]:
                raise SystemExit(f"silo {side['silo']}: collateral share is not the silo")
            if side["token"] not in ids or side["token"] not in reg["tokens"]:
                raise SystemExit(f"silo {side['silo']}: token {side['token']} not interned")
        p["market"] = market_of[f"silo-v2:{p['silos'][1]}"]
    for c, m in KEPT.items():
        have = next((p["market"] for p in pairs if p["config"] == c), None)
        if have != m:
            raise SystemExit(f"bound pair {c} would move from market {m} to {have}")
    if len({p["market"] for p in pairs}) != len(pairs):
        raise SystemExit("two pairs share a market id")

    tokens = sorted({s["token"] for p in pairs for s in p["sides"]}, key=lambda t: ids[t])
    oracles = sum(1 for p in pairs for s in p["sides"] if s["solvency_oracle"] != ZERO)
    print(f"block {chain.block}: {len(pairs)} pairs, {len(tokens)} tokens, "
          f"{oracles} sides with a solvency oracle, factories {factories}")
    if args.dry_run:
        return 0

    lines = [
        "# Silo V2 mainnet (family `silo-v2` in registry/registry.json): every pair.",
        "# GENERATED by tools/registry/gen_silo_toml.py — edit the generator, not this file.",
        f"# Read at block {chain.block}: {len(pairs)} SiloConfigs, getSilos / getConfig each.",
        "# Pin: silo-finance/silo-contracts-v2 @ 570a668a98a88a6a2b92697e7b9a3b1c6299dce7.",
        "# Fills `liq_adapters_silo_v2::Config`: one MarketId per SiloConfig (the registry",
        "# intern id of its slot-1 silo). Slot 0 = getSilos().0, slot 1 = getSilos().1.",
        "# Liquidation is on the hook receiver, not the Silo. Health prices through each",
        "# pair's own solvency oracles (one read per pair per block) and grows the totals",
        "# at the interest each block's totals reads measure.",
        "",
        f"protocol = {PROTOCOL}",
        f"pinned_through = {chain.block}",
        "",
        "factories = [",
        *[f'    "{f}",' for f in factories],
        "]",
    ]
    for p in sorted(pairs, key=lambda p: p["market"]):
        lines += ["", "[[pairs]]", f'silo_config = "{p["config"]}"',
                  f'hook_receiver = "{p["sides"][0]["hook"]}"', f"market = {p['market']}"]
        for slot, s in enumerate(p["sides"]):
            lines += ["", f"[pairs.silo{slot}]", f'silo = "{s["silo"]}"', f'token = "{s["token"]}"',
                      f'protected_share = "{s["protected_share"]}"', f'debt_share = "{s["debt_share"]}"',
                      f'solvency_oracle = "{s["solvency_oracle"]}"', f"lt = {s['lt']}",
                      f"liquidation_fee = {s['liquidation_fee']}",
                      f"liquidation_target_ltv = {s['liquidation_target_ltv']}",
                      f"interest_fee = {s['interest_fee']}"]
    for t in tokens:
        lines += ["", "[[assets]]", f'underlying = "{t}"', f"asset = {ids[t]}", "feed = 0",
                  f"decimals = {reg['tokens'][t]['decimals']}"]
    OUT.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"wrote {OUT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
