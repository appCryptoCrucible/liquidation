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

Curve StableSwap-NG LPs (swap venue 7, `{"kind": "curve_lp", "into": <coin>}`):
    - the token is a `curve_ng` registry pool (NG pools are their own LP);
    - this port of `_calc_withdraw_one_coin` (CurveStableSwapNG.vy v7.0.0),
      on the pool's `balances`, `A_precise`, `fee`, `stored_rates`,
      `offpeg_fee_multiplier` and the LP `totalSupply`, reproduces the
      pool's own `calc_withdraw_one_coin` for every coin at 1 LP and at a
      tenth of the supply;
    - a real holder's `remove_liquidity_one_coin(amount, i, 0)`, simulated
      from the holder, pays exactly `calc_withdraw_one_coin`;
    - `into` is the coin with the deepest normalized balance (the withdrawal
      that imbalances the pool least).
    The quote is exact and nonlinear, so the linearity gate below does not
    apply to it.

Live Pendle PTs (swap venue 8, `{"kind": "pendle_market", "into", "market",
"yt", "sy"}`):
    - the PT's market (Pendle's API, `/core/v1/1/markets/active`) is valid on
      `PendleMarketFactoryV6` (the Executor's anchor) and its `readTokens()`
      are this PT, its YT and SY;
    - `pendle_math.sell_pt` (the port of `MarketMathCore.swapExactPtForSy`)
      pays exactly what a real holder's sale pays — `swapExactPtForSy` run
      by `eth_call` with the holder's code overridden by `PendleSellProbe` —
      at one PT and at the holder's balance;
    - `into` is a token of `SY.getTokensOut()` that the holder's SY redeems
      into without reverting, and whose `SY.previewRedeem` is linear.

Vaults, expired PTs and a live PT's SY redemption must also quote linearly: the answer for K whole units is K times
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
import pendle_math as pm  # noqa: E402
from discover_exits import EXCLUDED_QUIRKS, Chain, ng_dynamic_fee, ng_get_d, sel, word  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
REG = ROOT / "registry" / "registry.json"
META = ROOT / "registry" / "registry.meta.json"
PROBE = ROOT / "contracts/out/PendleRedeemProbe.sol/PendleRedeemProbe.json"
SELL_PROBE = ROOT / "contracts/out/PendleSellProbe.sol/PendleSellProbe.json"
PENDLE_API = "https://api-v2.pendle.finance/core/v1/1/markets/active"
PENDLE_MARKET_FACTORY_V6 = "0x6d247b1c044fa1e22e6b04fa9f71baf99eb29a9f"
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


# ── Curve NG LP ─────────────────────────────────────────────────────────────


def ng_get_y_d(i: int, xp: list[int], amp: int, d: int) -> int:
    n = len(xp)
    ann = amp * n
    c = d
    s_ = 0
    for k in range(n):
        if k == i:
            continue
        s_ += xp[k]
        c = c * d // (xp[k] * n)
    c = c * d * 100 // (ann * n)
    b = s_ + d * 100 // ann
    y = d
    for _ in range(255):
        prev = y
        y = (y * y + c) // (2 * y + b - d)
        if abs(y - prev) <= 1:
            return y
    raise ValueError("y_D did not converge")


def ng_withdraw_one_coin(burn: int, i: int, st: dict) -> int:
    """`CurveStableSwapNG._calc_withdraw_one_coin(burn, i).dy` (v7.0.0)."""
    rates, amp, fee, m = st["rates"], st["amp"], st["fee"], st["offpeg"]
    xp = [r * b // 10**18 for r, b in zip(rates, st["balances"])]
    n = len(xp)
    d0 = ng_get_d(xp, amp)
    d1 = d0 - burn * d0 // st["supply"]
    new_y = ng_get_y_d(i, xp, amp, d1)
    base_fee = fee * n // (4 * (n - 1))
    ys = (d0 + d1) // (2 * n)
    xr = list(xp)
    for j in range(n):
        if j == i:
            dx_expected = xp[j] * d1 // d0 - new_y
            xavg = (xp[j] + new_y) // 2
        else:
            dx_expected = xp[j] - xp[j] * d1 // d0
            xavg = xp[j]
        if dx_expected < 0:
            raise ValueError("pool reverts")
        xr[j] = xp[j] - ng_dynamic_fee(xavg, ys, base_fee, m) * dx_expected // 10**10
    dy = xr[i] - ng_get_y_d(i, xr, amp, d1)
    return (dy - 1) * 10**18 // rates[i]


def ng_state(chain: Chain, pool: str, n: int) -> dict | None:
    calls = [(pool, sel("balances(uint256)") + encode(["uint256"], [i])) for i in range(n)]
    calls += [(pool, sel(v)) for v in ("A_precise()", "fee()", "stored_rates()",
                                       "offpeg_fee_multiplier()", "totalSupply()")]
    r = chain.multicall(calls)
    bal = [word(x) for x in r[:n]]
    amp, fee, rates_r, offpeg, supply = word(r[n]), word(r[n + 1]), r[n + 2], word(r[n + 3]), word(r[n + 4])
    if None in bal or None in (amp, fee, offpeg, supply) or not rates_r[0]:
        return None
    return {"balances": bal, "amp": amp, "fee": fee, "offpeg": offpeg, "supply": supply,
            "rates": list(decode(["uint256[]"], rates_r[1])[0])}


def discover_curve_lp(chain, url, tokens, cands, usable, routed, pools) -> dict[str, dict]:
    found = {}
    for t in cands:
        p = pools.get(t)
        if not p or p["venue"] != "curve_ng":
            continue
        coins = [c.lower() for c in p["coins"]]
        sym = tokens[t].get("symbol") or t
        st = ng_state(chain, t, len(coins))
        if st is None or st["supply"] == 0:
            print(f"  skip {sym:24} {t}: pool state unreadable")
            continue
        sizes = sorted({10 ** tokens[t]["decimals"], st["supply"] // 10} - {0})
        exact = True
        for i in range(len(coins)):
            for amt in sizes:
                want = word(chain.multicall([(t, sel("calc_withdraw_one_coin(uint256,int128)")
                                              + encode(["uint256", "int128"], [amt, i]))])[0])
                try:
                    got = ng_withdraw_one_coin(amt, i, st)
                except (ValueError, ZeroDivisionError):
                    got = None
                if want != got:
                    exact = False
        if not exact:
            print(f"  skip {sym:24} {t}: port != calc_withdraw_one_coin")
            continue
        xp = [r * b // 10**18 for r, b in zip(st["rates"], st["balances"])]
        order = sorted(range(len(coins)), key=lambda k: -xp[k])
        i = next((k for k in order if coins[k] in usable and coins[k] in routed), None)
        if i is None:
            continue
        proof = None
        for h in holders(url, t, chain.block)[:HOLDERS]:
            bal = balance(chain, t, h)
            if not bal:
                continue
            want = word(chain.multicall([(t, sel("calc_withdraw_one_coin(uint256,int128)")
                                          + encode(["uint256", "int128"], [bal, i]))])[0])
            got = call_from(chain, t, sel("remove_liquidity_one_coin(uint256,int128,uint256)")
                            + encode(["uint256", "int128", "uint256"], [bal, i, 0]), h)
            if want and got == want:
                proof = h
                break
        if proof is None:
            print(f"  skip {sym:24} {t}: no holder's withdrawal paid calc_withdraw_one_coin")
            continue
        print(f"  unwrap {sym:22} {t} -> {tokens[coins[i]].get('symbol')} (holder {proof})")
        found[t] = {"kind": "curve_lp", "into": coins[i]}
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


def market_state(chain: Chain, mk: str, sy: str, yt: str) -> dict:
    r = chain.multicall([(mk, sel("readState(address)") + encode(["address"], ["0x" + "00" * 20])),
                         (sy, sel("exchangeRate()")), (yt, sel("pyIndexStored()"))])
    f = decode(["int256", "int256", "int256", "address", "int256", "uint256", "uint256", "uint256",
                "uint256"], r[0][1])
    return {"total_pt": f[0], "total_sy": f[1], "scalar_root": f[4], "expiry": f[5],
            "ln_fee_rate_root": f[6], "reserve_fee_percent": f[7], "last_ln_implied_rate": f[8],
            "index": max(word(r[1]), word(r[2]))}


def sell_probe(chain: Chain, code: str, holder, pt, mk, sy, out, amount):
    cs = chain.w3.to_checksum_address
    data = sel("probe(address,address,address,address,uint256)") + encode(
        ["address"] * 4 + ["uint256"], [cs(pt), cs(mk), cs(sy), cs(out), amount])
    for attempt in range(4):
        try:
            raw = chain.w3.eth.call({"to": cs(holder), "data": "0x" + data.hex()}, chain.block,
                                    {cs(holder): {"code": code}})
            return decode(["uint256", "uint256"], raw)
        except Exception as ex:  # noqa: BLE001
            if "revert" in str(ex).lower() or "execution" in str(ex).lower():
                return None
            time.sleep(1.5 * 2**attempt)
    return None


def discover_pendle_market(chain, url, tokens, cands, usable, routed) -> dict[str, dict]:
    if not SELL_PROBE.exists():
        raise SystemExit(f"{SELL_PROBE} missing: run `forge build` in contracts/")
    code = json.loads(SELL_PROBE.read_text(encoding="utf-8"))["deployedBytecode"]["object"]
    api = requests.get(PENDLE_API, timeout=60).json()["markets"]
    by_pt = {m["pt"].split("-", 1)[1].lower(): m["address"].lower() for m in api}
    ts = chain.w3.eth.get_block(chain.block)["timestamp"]
    found = {}
    for t in cands:
        r = one_token_views(chain, t, [("YT()", b""), ("SY()", b""), ("expiry()", b"")])
        yt, sy = word(r[0], "address"), word(r[1], "address")
        if yt is None or sy is None or word(r[2]) is None:
            continue
        yt, sy = yt.lower(), sy.lower()
        if word(chain.multicall([(yt, sel("isExpired()"))])[0], "bool"):
            continue  # expired: the `pendle_pt` pass redeems it
        sym = tokens[t].get("symbol") or t
        mk = by_pt.get(t)
        if not mk:
            print(f"  skip {sym:24} {t}: no active market")
            continue
        v = chain.multicall([(PENDLE_MARKET_FACTORY_V6, sel("isValidMarket(address)") + encode(["address"], [mk])),
                             (mk, sel("readTokens()"))])
        toks = decode(["address", "address", "address"], v[1][1]) if v[1][0] else ()
        if not word(v[0], "bool") or [x.lower() for x in toks] != [sy, t, yt]:
            print(f"  skip {sym:24} {t}: market {mk} not a V6 market for this PT")
            continue
        st = market_state(chain, mk, sy, yt)
        outs = decode(["address[]"], chain.multicall([(sy, sel("getTokensOut()"))])[0][1])[0]
        outs = [o.lower() for o in outs if o.lower() in usable and o.lower() in routed]
        unit = 10 ** tokens[t]["decimals"]
        admitted = None
        hs = [h for h in holders(url, t, chain.block) if h != mk][:HOLDERS] if outs else []
        for out in outs:
            sy_quote = (lambda a, out=out: word(chain.multicall(
                [(sy, sel("previewRedeem(address,uint256)") + encode(["address", "uint256"], [out, a]))])[0]))
            if not linear(sy_quote, unit):
                continue
            for h in hs:
                bal = balance(chain, t, h)
                if not bal:
                    continue
                ok = True
                for amt in sorted({min(bal, unit), bal}):
                    got = sell_probe(chain, code, h, t, mk, sy, out, amt)
                    try:
                        want = pm.sell_pt(st, amt, ts)[0]
                    except ValueError:
                        want = None
                    if got is None or want is None or got[0] != want or got[1] == 2**256 - 1:
                        ok = False
                        break
                if ok:
                    admitted = (out, h)
                    break
            if admitted:
                break
        if admitted is None:
            print(f"  skip {sym:24} {t}: no routed SY output sold and redeemed at the quote")
            continue
        out, h = admitted
        print(f"  unwrap {sym:22} {t} -> {tokens[out].get('symbol')} via {mk} (holder {h})")
        found[t] = {"kind": "pendle_market", "into": out, "market": mk, "yt": yt, "sy": sy}
    return found


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--registry", type=Path, default=None,
                    help="registry.json to read and write (default: the repo's); its meta is the sibling registry.meta.json")
    ap.add_argument("--kinds", default="erc4626,pendle_pt,curve_lp,pendle_market")
    ap.add_argument("--recheck", action="store_true")
    args = ap.parse_args()
    global REG, META
    if args.registry is not None:
        REG = args.registry
        META = args.registry.with_name("registry.meta.json")
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
            if u["kind"] in ("curve_lp", "pendle_market"):
                continue  # exact market math, nonlinear by design
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
    if "curve_lp" in kinds:
        found.update(discover_curve_lp(chain, url, tokens, cands, usable, routed, reg["pools"]))
    if "pendle_pt" in kinds:
        found.update(discover_pendle(chain, url, tokens, cands, usable, routed))
    if "pendle_market" in kinds:
        found.update(discover_pendle_market(chain, url, tokens, cands, usable, routed))
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
