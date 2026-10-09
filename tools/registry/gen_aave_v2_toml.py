#!/usr/bin/env python3
"""Generate config/protocols/aave-v2.toml from the chain and the registry.

Aave V2 Ethereum runs on the Aave V3 adapter's V2 version (`version = "v2"`).
It is not a registry family: its protocol id (12) and market id (65000) are
config-defined, after Liquity 9 / Fluid 10 / Gearbox 11 and above every
other market range, so nothing interned is renumbered.

Read at one pinned block:
  - the pool's `getReservesList()` and each reserve's V2 `getReserveData`
    (aToken, stable and variable debt tokens: the token logs the adapter
    reads), and its decimals;
  - the price oracle's `getSourceOfAsset` per reserve (WETH is priced by
    the oracle itself at exactly 1e18);
  - the collateral manager's `LIQUIDATIONS_GRACE_SENTINEL`.
Reserve underlyings that are not registry tokens are interned (ids appended
to `registry/asset-ids.json`).

Liquidation constants, from the deployed collateral manager `0xcc963272…`:
the whole of the reserve's debt (stable + variable) may be repaid, no dust
rule, half-up WadRayMath, an 18-decimal ETH oracle.

Usage: MAINNET_RPC_URL=... python tools/registry/gen_aave_v2_toml.py [--dry-run]
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
OUT = Path("config/protocols/aave-v2.toml")
POOL = "0x7d2768de32b0b80b7a3454c06bdac94a69ddc7a9"
PROVIDER = "0xb53c1a33016b2dc2ff3653530bff1848a515c8c5"
PROTOCOL = 12
MARKET = 65_000
# Left out: AMPL rebases, and its V2 aToken is a special implementation
# (`AAmplToken`) this adapter does not model. Neither its asset nor its
# tokens are configured, so a position holding it fails closed.
SKIP = {"0xd46ba6d942050d489dbd938a2c909a5d5039a161": "AMPL: rebasing, special aToken"}
RESERVE = "(uint256,uint128,uint128,uint128,uint128,uint128,uint40,address,address,address,address,uint8)"


def text(ok_ret) -> str | None:
    ok, raw = ok_ret
    if not ok or len(raw) < 64:
        return None
    try:
        return decode(["string"], raw)[0]
    except Exception:  # noqa: BLE001
        return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()
    chain = Chain(os.environ["MAINNET_RPC_URL"])
    reg = json.loads(REG.read_text(encoding="utf-8"))
    ledger = json.loads(LEDGER.read_text(encoding="utf-8"))
    ids = {a.lower(): int(i) for a, i in ledger["ids"].items()}

    one = lambda to, sig, args_t=(), args_v=(): chain.multicall(
        [(to, sel(sig) + (encode(list(args_t), list(args_v)) if args_t else b""))])[0]
    configurator = word(one(PROVIDER, "getLendingPoolConfigurator()"), "address").lower()
    oracle = word(one(PROVIDER, "getPriceOracle()"), "address").lower()
    cm = word(one(PROVIDER, "getLendingPoolCollateralManager()"), "address").lower()
    grace = (word(one(cm, "LIQUIDATIONS_GRACE_SENTINEL()"), "address") or "0x" + "0" * 40).lower()
    ok, raw = one(POOL, "getReservesList()")
    reserves = [r.lower() for r in decode(["address[]"], raw)[0] if r.lower() not in SKIP]

    rd = chain.multicall([(POOL, sel("getReserveData(address)") + encode(["address"], [r]))
                          for r in reserves])
    src = chain.multicall([(oracle, sel("getSourceOfAsset(address)") + encode(["address"], [r]))
                           for r in reserves])
    meta = chain.multicall([(r, sel(s)) for r in reserves for s in ("symbol()", "decimals()")])
    rows, tokens = [], []
    for i, r in enumerate(reserves):
        d = decode([RESERVE], rd[i][1])[0]
        a_tok, s_tok, v_tok = d[7].lower(), d[8].lower(), d[9].lower()
        dec = (int(d[0]) >> 48) & 0xff
        rows.append({"asset": r, "decimals": dec, "source": (word(src[i], "address") or "0x" + "0" * 40).lower(),
                     "symbol": text(meta[2 * i]), "erc_dec": word(meta[2 * i + 1])})
        tokens += [t for t in (a_tok, s_tok, v_tok) if int(t, 16)]

    new = [x for x in rows if x["asset"] not in ids]
    print(f"block {chain.block}: {len(reserves)} reserves, {len(tokens)} tokens; "
          f"oracle {oracle}, configurator {configurator}, grace sentinel {grace}")
    print(f"  reserves outside the registry: {len(new)} {[x['symbol'] for x in new]}")
    mismatched = [x["symbol"] for x in rows if x["erc_dec"] is not None and x["erc_dec"] != x["decimals"]]
    if mismatched:
        raise SystemExit(f"reserve decimals differ from the token's: {mismatched}")
    if args.dry_run:
        return 0

    nxt = int(ledger["next"])
    for x in new:
        if x["symbol"] is None:
            raise SystemExit(f"reserve {x['asset']} has no string symbol")
        reg["tokens"][x["asset"]] = {"symbol": x["symbol"], "decimals": x["decimals"],
                                     "quirks": ["low_decimals"] if x["decimals"] < 18 else []}
        ledger["ids"][x["asset"]] = nxt
        ids[x["asset"]] = nxt
        nxt += 1
    ledger["next"] = nxt
    if new:
        REG.write_text(json.dumps(reg, indent=1) + "\n", encoding="utf-8")
        LEDGER.write_text(json.dumps(ledger, indent=2) + "\n", encoding="utf-8")

    lines = [
        "# Aave V2 Ethereum (not a registry family; ids config-defined).",
        "# GENERATED by tools/registry/gen_aave_v2_toml.py — edit the generator, not this file.",
        f"# Read at block {chain.block}: {len(reserves)} reserves. The Aave V3 adapter's V2 version:",
        "# events from the pool, configurator, oracle, provider, grace sentinel and every",
        "# aToken / stable / variable debt token; balances from the tokens' own events.",
        "# Left out: " + "; ".join(f"{a} ({why})" for a, why in SKIP.items()) + ".",
        "# Liquidation constants from the deployed LendingPoolCollateralManager",
        f"# {cm}: the reserve's whole debt may be repaid; no dust rule.",
        "",
        f"protocol = {PROTOCOL}",
        f"pinned_through = {chain.block}",
        "",
        "[[pools]]",
        f'address = "{POOL}"',
        f"market = {MARKET}",
        f'oracle = "{oracle}"',
        f'provider = "{PROVIDER}"',
        f'configurator = "{configurator}"',
        'sentinel = "0x0000000000000000000000000000000000000000"',
        'sequencer_oracle = "0x0000000000000000000000000000000000000000"',
        f'grace_sentinel = "{grace}"',
        "tokens = [",
        *[f'    "{t}",' for t in tokens],
        "]",
        "",
        "[liquidation]",
        "close_factor_bps = 10000",
        "close_factor_hf_wad = 0",
        "min_base_max_close = 0",
        "oracle_decimals = 18",
        'balance_model = "wad-ray-half-up"',
        'close_factor_scope = "reserve-debt"',
        'version = "v2"',
    ]
    for x in sorted(rows, key=lambda x: ids[x["asset"]]):
        lines += ["", "[[assets]]", f'underlying = "{x["asset"]}"', f"asset = {ids[x['asset']]}", "feed = 0",
                  "siloed = false", "isolated = false", "debt_ceiling = 0", f"decimals = {x['decimals']}"]
    for x in rows:
        lines += ["", "[[price_sources]]", f'pool = "{POOL}"', f'underlying = "{x["asset"]}"',
                  f'source = "{x["source"]}"']
    OUT.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"wrote {OUT}; {len(new)} tokens interned, ledger next {nxt}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
