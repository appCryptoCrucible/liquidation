#!/usr/bin/env python3
"""Discover collateral wrappers the Executor can unwrap instantly and record
them on their registry token entries as `"unwrap": {...}`.

A seized collateral with no pool of its own is exited by unwrapping it and
selling what it unwraps into. Every read is pinned to one block. Candidates
have no pool in the registry and no rebasing / fee-on-transfer quirk, and what
they unwrap into must be a registry token (same quirk rule) that has a pool.

ERC-4626 vaults (swap venue 5, `{"kind": "erc4626", "into": <asset>}`):
    - they answer asset(), totalAssets(), previewRedeem(), convertToAssets();
    - a real holder's redeem(shares, holder, holder), simulated from that
      holder, pays exactly previewRedeem(shares) both for one share and for the
      holder's whole balance (no queue, cooldown, fee or liquidity shortfall at
      that size).

Expired Pendle PTs (swap venue 6, `{"kind": "pendle_pt", "into", "yt", "sy"}`):
    - PT.YT() is a YT that names the PT back (YT.PT()) and is expired, and
      YT.SY() is the SY;
    - `into` is a token of SY.getTokensOut();
    - a real holder's PT, run through the Executor's venue-6 path (PT to the
      YT, redeemPY, SY.redeem) by `eth_call` with the holder's code overridden
      by `PendleRedeemProbe`, pays exactly the bot's quote
      SY.previewRedeem(into, pt * 1e18 / max(SY.exchangeRate(),
      YT.pyIndexStored())) for one PT and for the holder's whole balance.
      Needs `forge build` in contracts/ (the probe's artifact).

Both kinds must also quote linearly: the answer for K whole units is K times
the answer for one, to rounding, at K = 1_000 and 100_000 — the bot reads one
rate per block and scales it, so a vault whose redemption slips with size
(e.g. one that unwinds positions on withdraw) would be overquoted.

The entries of the kinds run are rebuilt from scratch; `--kinds` limits a run
and keeps the other kinds' entries. `--recheck` only re-applies the linearity
gate to the committed entries and drops those that fail.

Usage: MAINNET_RPC_URL=... python tools/registry/discover_unwraps.py
           [--dry-run] [--kinds erc4626,pendle_pt]
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path

import requests
from eth_abi import decode, encode

sys.path.insert(0, str(Path(__file__).parent))
from discover_exits import EXCLUDED_QUIRKS, Chain, sel, word  # noqa: E402

REG = Path("registry/registry.json")
META = Path("registry/registry.meta.json")
PROBE = Path("contracts/out/PendleRedeemProbe.sol/PendleRedeemProbe.json")
QUIRKS_OUT = EXCLUDED_QUIRKS | {"fee_on_transfer"}
HOLDERS = 15


def holders(url: str, token: str, block: int) -> list[str]:
    """Recent recipients of `token` (Alchemy transfer index), newest first."""
    body = {"jsonrpc": "2.0", "id": 1, "method": "alchemy_getAssetTransfers", "params": [{
        "fromBlock": "0x0", "toBlock": hex(block), "contractAddresses": [token],
        "category": ["erc20"], "order": "desc", "maxCount": "0x32", "withMetadata": False}]}
    for attempt in range(5):
        try:
            r = requests.post(url, json=body, timeout=60).json()
            break
        except Exception:  # noqa: BLE001 - retry any transport error
            time.sleep(1.5 * 2**attempt)
    else:
        return []
    out: list[str] = []
    for t in r.get("result", {}).get("transfers", []):
        to = (t.get("to") or "").lower()
        if to and to != "0x0000000000000000000000000000000000000000" and to not in out:
            out.append(to)
    return out


def call_from(chain: Chain, to: str, data: bytes, frm: str) -> int | None:
    tx = {"to": chain.w3.to_checksum_address(to), "from": chain.w3.to_checksum_address(frm),
          "data": "0x" + data.hex()}
    for attempt in range(4):
        try:
            raw = chain.w3.eth.call(tx, block_identifier=chain.block)
            return int.from_bytes(raw[:32], "big") if len(raw) >= 32 else None
        except Exception as ex:  # noqa: BLE001
            if "revert" in str(ex).lower() or "execution" in str(ex).lower():
                return None
            time.sleep(1.5 * 2**attempt)
    return None


def one_token_views(chain: Chain, t: str, sigs: list[tuple[str, bytes]]) -> list[tuple[bool, bytes]]:
    """`sigs` on one token in one multicall: a token whose fallback burns the
    whole gas cap sinks only itself."""
    try:
        return chain.multicall([(t, sel(v) + arg) for v, arg in sigs])
    except RuntimeError:
        return [(False, b"")] * len(sigs)


def balance(chain: Chain, token: str, who: str) -> int:
    return word(chain.multicall([(token, sel("balanceOf(address)") + encode(["address"], [who]))])[0]) or 0


LINEAR_KS = (1_000, 100_000)


def linear(quote, unit: int) -> bool:
    """`quote(k * unit) ≈ k * quote(unit)`: each unit rounds down at most one
    wei, so the gap is at most `k` (plus a wei for the split read)."""
    one = quote(unit)
    if not one:
        return False
    for k in LINEAR_KS:
        big = quote(k * unit)
        if big is None or not (k * one - 1 <= big <= k * one + k):
            return False
    return True


def vault_quote(chain: Chain, vault: str):
    return lambda shares: word(chain.multicall(
        [(vault, sel("previewRedeem(uint256)") + encode(["uint256"], [shares]))])[0])


# ── ERC-4626 ────────────────────────────────────────────────────────────────


def redeem_pays_preview(chain: Chain, url: str, vault: str, unit: int) -> dict | None:
    """First holder with shares whose redeem pays previewRedeem exactly."""
    for h in holders(url, vault, chain.block)[:HOLDERS]:
        bal = balance(chain, vault, h)
        if not bal:
            continue
        ok = True
        for shares in sorted({min(bal, unit), bal}):
            prev = word(chain.multicall(
                [(vault, sel("previewRedeem(uint256)") + encode(["uint256"], [shares]))])[0])
            got = call_from(chain, vault, sel("redeem(uint256,address,address)")
                            + encode(["uint256", "address", "address"], [shares, h, h]), h)
            if not prev or got != prev:
                ok = False
                break
        if ok:
            return {"holder": h, "balance": bal}
        # One holder's redeem failing may be that holder (blocked, locked
        # shares); try the next before giving up on the vault.
    return None


def discover_4626(chain, url, tokens, cands, usable, routed) -> dict[str, dict]:
    found = {}
    for t in cands:
        unit = 10 ** tokens[t]["decimals"]
        u = encode(["uint256"], [unit])
        r = one_token_views(chain, t, [("asset()", b""), ("totalAssets()", b""),
                                       ("previewRedeem(uint256)", u), ("convertToAssets(uint256)", u)])
        asset = word(r[0], "address")
        if asset is None or any(word(x) is None for x in r[1:]) or not word(r[2]):
            continue
        asset = asset.lower()
        if asset not in usable or asset not in routed:
            continue
        sym = tokens[t].get("symbol") or t
        if not linear(vault_quote(chain, t), unit):
            print(f"  skip {sym:24} {t}: previewRedeem is not linear in size")
            continue
        proof = redeem_pays_preview(chain, url, t, unit)
        if proof is None:
            print(f"  skip {sym:24} {t}: no holder's redeem paid previewRedeem")
            continue
        print(f"  unwrap {sym:22} {t} -> {tokens[asset].get('symbol')} (holder {proof['holder']})")
        found[t] = {"kind": "erc4626", "into": asset}
    return found


# ── Pendle PT ───────────────────────────────────────────────────────────────


def pt_quote(chain: Chain, yt: str, sy: str, out: str, amount: int) -> int | None:
    """The bot's quote (`pool_seed::read_unwrap_rates`)."""
    r = chain.multicall([(sy, sel("exchangeRate()")), (yt, sel("pyIndexStored()"))])
    rate, stored = word(r[0]), word(r[1])
    if not rate or stored is None:
        return None
    shares = amount * 10**18 // max(rate, stored)
    if shares == 0:
        return None
    return word(chain.multicall([(sy, sel("previewRedeem(address,uint256)")
                                  + encode(["address", "uint256"], [out, shares]))])[0])


def pt_probe(chain: Chain, code: str, holder: str, pt, yt, sy, out, amount) -> int | None:
    """Venue 6 run on the holder's own PT (its code overridden by the probe)."""
    cs = chain.w3.to_checksum_address
    data = sel("probe(address,address,address,address,uint256)") + encode(
        ["address"] * 4 + ["uint256"], [cs(pt), cs(yt), cs(sy), cs(out), amount])
    for attempt in range(4):
        try:
            raw = chain.w3.eth.call({"to": cs(holder), "data": "0x" + data.hex()}, chain.block,
                                    {cs(holder): {"code": code}})
            return int.from_bytes(raw[:32], "big") if len(raw) >= 32 else None
        except Exception as ex:  # noqa: BLE001
            if "revert" in str(ex).lower() or "execution" in str(ex).lower():
                return None
            time.sleep(1.5 * 2**attempt)
    return None


def discover_pendle(chain, url, tokens, cands, usable, routed) -> dict[str, dict]:
    if not PROBE.exists():
        raise SystemExit(f"{PROBE} missing: run `forge build` in contracts/")
    code = json.loads(PROBE.read_text(encoding="utf-8"))["deployedBytecode"]["object"]
    found = {}
    for t in cands:
        r = one_token_views(chain, t, [("YT()", b""), ("SY()", b""), ("expiry()", b"")])
        yt, sy = word(r[0], "address"), word(r[1], "address")
        if yt is None or sy is None or word(r[2]) is None:
            continue
        yt, sy = yt.lower(), sy.lower()
        y = chain.multicall([(yt, sel("PT()")), (yt, sel("isExpired()")), (yt, sel("SY()")),
                             (sy, sel("getTokensOut()"))])
        sym = tokens[t].get("symbol") or t
        if (word(y[0], "address") or "").lower() != t or (word(y[2], "address") or "").lower() != sy:
            continue
        if not word(y[1], "bool"):
            print(f"  skip {sym:24} {t}: not expired")
            continue
        outs = [a.lower() for a in decode(["address[]"], y[3][1])[0]] if y[3][0] else []
        outs = [o for o in outs if o in usable and o in routed]
        unit = 10 ** tokens[t]["decimals"]
        admitted = None
        outs = [o for o in outs if linear(lambda a, o=o: pt_quote(chain, yt, sy, o, a), unit)]
        hs = holders(url, t, chain.block)[:HOLDERS] if outs else []
        for out in outs:
            for h in hs:
                bal = balance(chain, t, h)
                if not bal:
                    continue
                ok = True
                for amt in sorted({min(bal, unit), bal}):
                    want = pt_quote(chain, yt, sy, out, amt)
                    got = pt_probe(chain, code, h, t, yt, sy, out, amt)
                    if not want or got != want:
                        ok = False
                        break
                if ok:
                    admitted = (out, h)
                    break
            if admitted:
                break
        if admitted is None:
            print(f"  skip {sym:24} {t}: no routed SY output redeemed at the quote")
            continue
        out, h = admitted
        print(f"  unwrap {sym:22} {t} -> {tokens[out].get('symbol')} (holder {h})")
        found[t] = {"kind": "pendle_pt", "into": out, "yt": yt, "sy": sy}
    return found


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--kinds", default="erc4626,pendle_pt")
    ap.add_argument("--recheck", action="store_true")
    args = ap.parse_args()
    kinds = set(args.kinds.split(","))
    url = os.environ.get("MAINNET_RPC_URL")
    if not url:
        print("MAINNET_RPC_URL is required", file=sys.stderr)
        return 2
    chain = Chain(url)
    print(f"pinned block {chain.block}")
    reg = json.loads(REG.read_text(encoding="utf-8"))
    tokens = reg["tokens"]
    usable = {t for t, e in tokens.items() if not QUIRKS_OUT & set(e.get("quirks", []))}
    routed = set()
    for p in reg["pools"].values():
        routed.update(c.lower() for c in (p.get("coins") or [p["token0"], p["token1"]]))
    cands = sorted(t for t in usable if t not in routed)
    print(f"tokens {len(tokens)}, without a pool {len(cands)}")

    if args.recheck:
        dropped = []
        for t, e in tokens.items():
            u = e.get("unwrap")
            if not u:
                continue
            unit = 10 ** e["decimals"]
            q = (vault_quote(chain, t) if u["kind"] == "erc4626"
                 else (lambda a, u=u, t=t: pt_quote(chain, u["yt"], u["sy"], u["into"], a)))
            if not linear(q, unit):
                dropped.append(t)
                print(f"  drop {e.get('symbol') or t:24} {t}: not linear in size")
                e.pop("unwrap")
        print(f"recheck dropped {len(dropped)}")
        if args.dry_run or not dropped:
            return 0
        REG.write_text(json.dumps(reg, indent=1) + "\n", encoding="utf-8")
        return 0

    found: dict[str, dict] = {}
    if "pendle_pt" in kinds:
        found.update(discover_pendle(chain, url, tokens, cands, usable, routed))
    if "erc4626" in kinds:
        rest = [c for c in cands if c not in found]
        found.update(discover_4626(chain, url, tokens, rest, usable, routed))
    for t, e in tokens.items():
        if e.get("unwrap", {}).get("kind") in kinds:
            e.pop("unwrap")
        if t in found:
            e["unwrap"] = found[t]
    total: dict[str, int] = {}
    for e in tokens.values():
        if "unwrap" in e:
            total[e["unwrap"]["kind"]] = total.get(e["unwrap"]["kind"], 0) + 1
    print(f"unwraps: {total}")
    if args.dry_run:
        return 0
    REG.write_text(json.dumps(reg, indent=1) + "\n", encoding="utf-8")
    meta = json.loads(META.read_text(encoding="utf-8"))
    counts = ", ".join(f"{n} {k}" for k, n in sorted(total.items()))
    note = (f"unwraps: {counts} discovered by tools/registry/discover_unwraps.py "
            f"at block {chain.block}")
    meta["notes"] = [n for n in meta.get("notes", []) if not n.startswith("unwraps: ")] + [note]
    META.write_text(json.dumps(meta, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
