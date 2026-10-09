#!/usr/bin/env python3
"""Generate config/protocols/euler-v2.toml, and intern Euler collateral share
tokens, from the chain and the registry.

Every proxy of the EVK GenericFactory is read at one pinned block:
`asset()`, `oracle()`, `unitOfAccount()`, `decimals()`, `symbol()`,
`LTVList()`. From that:

  vaults        = every registry `euler-v2` market (all of them, not an
                  admission list: plan 1B tracks every market). A factory
                  proxy that is not in the registry is reported, not added:
                  registry market ids follow the sorted protocol keys, so a
                  new entry would renumber every market after it.
  registry      = each factory vault some vault names in its `LTVList()`,
                  whose asset is a registry token, is interned as a token
                  (its share ERC-20: the vault's own symbol and decimals),
                  with a new id appended to `registry/asset-ids.json`. Its
                  exit (an `erc4626` unwrap into the asset) is added by
                  `discover_unwraps.py --kinds erc4626`, which proves a real
                  holder's redeem first.
  assets        = every registry token a vault lends, takes as collateral
                  (the share token), or uses as its unit of account.
  price_sources = every vault's `oracle()`: the EulerRouter that vault's own
                  liquidation consults. EVK fixes `oracle` in the proxy
                  metadata, so the pin cannot go stale under a vault.

`feed` is informational (Euler health prices through each vault's own
oracle): FeedId 0, as Morpho and Gearbox do.

Usage: MAINNET_RPC_URL=... python tools/registry/gen_euler_toml.py [--dry-run]
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
OUT = Path("config/protocols/euler-v2.toml")
FACTORY = "0x29a56a1b8214d9cf7c5561811750d5cbdb45cc8e"
EVC = "0x0c9a3dd6b8f28529d72d7f9ce918d493519ee383"
USD = "0x0000000000000000000000000000000000000348"
ZERO = "0x" + "0" * 40
PROTOCOL = 4
CATALOG = 3511
FIRST_MARKET = 3512
SIGS = ("asset()", "oracle()", "unitOfAccount()", "decimals()", "symbol()", "LTVList()")


def text(ok_ret) -> str | None:
    ok, raw = ok_ret
    if not ok or len(raw) < 64:
        return None
    try:
        return decode(["string"], raw)[0]
    except Exception:  # noqa: BLE001 - bytes32 metadata is not an EVK share
        return None


def read_vaults(chain: Chain) -> dict[str, dict]:
    n = word(chain.multicall([(FACTORY, sel("getProxyListLength()"))])[0])
    ok, raw = chain.multicall([(FACTORY, sel("getProxyListSlice(uint256,uint256)")
                                + encode(["uint256", "uint256"], [0, n]))])[0]
    if not ok:
        raise SystemExit("getProxyListSlice failed")
    proxies = [v.lower() for v in decode(["address[]"], raw)[0]]
    res = chain.multicall([(v, sel(s)) for v in proxies for s in SIGS])
    out = {}
    for i, v in enumerate(proxies):
        r = res[i * len(SIGS):(i + 1) * len(SIGS)]
        asset = word(r[0], "address")
        if asset is None:
            continue
        ltv = decode(["address[]"], r[5][1])[0] if r[5][0] else []
        out[v] = {
            "asset": asset.lower(),
            "oracle": (word(r[1], "address") or ZERO).lower(),
            "unit": (word(r[2], "address") or ZERO).lower(),
            "decimals": word(r[3]),
            "symbol": text(r[4]),
            "ltv": [x.lower() for x in ltv],
        }
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()
    chain = Chain(os.environ["MAINNET_RPC_URL"])
    reg = json.loads(REG.read_text(encoding="utf-8"))
    ledger = json.loads(LEDGER.read_text(encoding="utf-8"))
    ids = {a.lower(): int(i) for a, i in ledger["ids"].items()}
    tokens = {a.lower() for a in reg["tokens"]}
    vaults = read_vaults(chain)
    registry_vaults = sorted(p["market"].lower() for p in reg["protocols"].values()
                             if p["family"] == "euler-v2")
    not_interned = sorted(set(vaults) - set(registry_vaults))
    gone = sorted(set(registry_vaults) - set(vaults))

    # Share tokens: collaterals some vault recognizes.
    named = sorted({c for v in vaults.values() for c in v["ltv"]})
    shares, skipped = [], {"not a factory vault": [], "asset not a registry token": [],
                           "no string symbol": []}
    for c in named:
        v = vaults.get(c)
        if v is None:
            skipped["not a factory vault"].append(c)
        elif v["asset"] not in tokens:
            skipped["asset not a registry token"].append(c)
        elif v["symbol"] is None or v["decimals"] is None:
            skipped["no string symbol"].append(c)
        else:
            shares.append(c)
    new_shares = [c for c in shares if c not in tokens]

    # Every token the adapter must map: lent, unit of account, share.
    mapped: set[str] = set(shares)
    for v in vaults.values():
        if v["asset"] in tokens:
            mapped.add(v["asset"])
        if v["unit"] in tokens:
            mapped.add(v["unit"])
    oracles = sorted({v["oracle"] for v in vaults.values() if v["oracle"] != ZERO})
    non_usd = sum(1 for v in vaults.values() if v["unit"] not in (USD, ZERO))

    print(f"block {chain.block}: {len(vaults)} factory vaults, {len(registry_vaults)} in the registry")
    print(f"  not in the registry (left out): {len(not_interned)} {not_interned}")
    print(f"  in the registry, gone from the factory: {len(gone)}")
    print(f"  collateral vaults named by LTVList: {len(named)}; share tokens {len(shares)} "
          f"({len(new_shares)} new to the registry)")
    for why, xs in skipped.items():
        print(f"    skipped, {why}: {len(xs)} {xs[:4]}")
    print(f"  assets {len(mapped)}, oracles {len(oracles)}, vaults with a non-USD unit {non_usd}")
    if args.dry_run:
        return 0

    nxt = int(ledger["next"])
    for c in new_shares:
        v = vaults[c]
        dec = int(v["decimals"])
        reg["tokens"][c] = {"symbol": v["symbol"], "decimals": dec,
                            "quirks": ["low_decimals"] if dec < 18 else []}
        if c not in ids:
            ledger["ids"][c] = nxt
            ids[c] = nxt
            nxt += 1
    ledger["next"] = nxt
    if max(ids.values()) >= 0xFFFF:
        raise SystemExit("asset id would reach u16::MAX (the adapter's UNMAPPED sentinel)")
    # Each file's committed formatting (indent 1 and 2), so a run's diff is
    # only what it added.
    REG.write_text(json.dumps(reg, indent=1) + "\n", encoding="utf-8")
    LEDGER.write_text(json.dumps(ledger, indent=2) + "\n", encoding="utf-8")

    def dec_of(a: str) -> int:
        return int(reg["tokens"][a]["decimals"])

    lines = [
        "# Euler V2 EVK mainnet (family `euler-v2` in registry/registry.json).",
        "# Fills `liq_adapters_euler_v2::Config`. GENERATED by",
        "# tools/registry/gen_euler_toml.py — edit the generator, not this file.",
        f"# Factory GenericFactory {FACTORY}; EVC {EVC}",
        "# (euler-xyz/euler-interfaces CoreAddresses).",
        "# Pin: euler-xyz/euler-vault-kit @ bfb325a6e6ca09613d940b46f72ccfe017353933.",
        f"# Read at block {chain.block}: {len(vaults)} factory vaults, {len(registry_vaults)} in the",
        f"# registry. {len(not_interned)} newer proxies are not in the registry and are not",
        "# tracked until a registry refresh interns them.",
        "#",
        "# MarketId allocator (intern-global; StateStore has no ProtocolId):",
        "#   1. Each euler-v2 vault binds to its Intern::from_registry MarketRec.id",
        "#      (Config::load / bind_from_intern).",
        "#   2. catalog = 3511 and first_market = 3512.. stay reserved. Never",
        "#      3481..=3510 (Liquity owns 3508..=3510).",
        "# `vaults`: every registry euler-v2 vault (subscriptions + backfill).",
        "# `assets`: lent tokens, units of account, and collateral share tokens",
        "# (interned by the generator; the share's `underlying` is its own address).",
        "# `price_sources`: every vault's own oracle() (EulerRouter), fixed in the",
        "# proxy metadata.",
        "",
        f"protocol = {PROTOCOL}",
        f'factory = "{FACTORY}"',
        f'evc = "{EVC}"',
        f"catalog = {CATALOG}",
        f"first_market = {FIRST_MARKET}",
        f"pinned_through = {chain.block}",
        "",
        "vaults = [",
        *[f'  "{v}",' for v in registry_vaults],
        "]",
        "",
        "price_sources = [",
        *[f'  "{o}",' for o in oracles],
        "]",
    ]
    for a in sorted(mapped, key=lambda a: ids[a]):
        lines += ["", "[[assets]]", f'underlying = "{a}"', f"asset = {ids[a]}", "feed = 0",
                  f"decimals = {dec_of(a)}"]
    OUT.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"wrote {OUT}, {len(new_shares)} tokens to {REG}, ledger next {nxt}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
