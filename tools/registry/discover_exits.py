#!/usr/bin/env python3
"""Discover UniswapV2 / SushiSwap pairs and Curve plain StableSwap pools for
exits, and write them into registry/registry.json (venues "univ2" / "curve").

Every read is pinned to one block. UniV3 entries are left untouched; every
univ2/curve entry is rebuilt from scratch on each run.

V2: getPair(token, hub) on both factories for every tracked token x HUB_ASSETS.
    Kept when the pair address is the CREATE2 address the Executor re-derives,
    factory() agrees, and the hub-side reserve clears HUB_MIN_RESERVE.

Curve: every MetaRegistry pool. Kept only when
    - not a metapool, 2..4 coins, every coin tracked, no native-ETH placeholder,
      no rebasing coin;
    - coins(uint256)/balances(uint256)/A()/fee() answer and gamma() does not
      (crypto pools use different math);
    - plain ("curve"): stored_rates() and offpeg_fee_multiplier() do not
      answer; StableSwap-NG ("curve_ng"): both answer and the NG factory
      reports its asset types (no rebasing coin);
    - A is not ramping at the pinned block;
    - the bot's own math for that kind (plain: RATES = 10^(36-decimals),
      A_PRECISION 1 or 100; NG: stored_rates(), get_D dividing by N^N once,
      the dynamic fee) reproduces the pool's own get_dy exactly for every
      ordered coin pair at two sizes. Anything else is left out, not guessed.

Curve crypto ("curve_crypto"): pools whose gamma() answers, 2 or 3 coins, not
    ramping, of a kind `crypto_math.py` ports (twocrypto-ng on
    CurveTwocryptoMathOptimized v2.0.0 / v2.1.0, tricrypto-ng on
    CurveTricryptoMathOptimized v2.0.0, the original CurveCryptoSwap2), and
    whose own get_dy(uint256,uint256,uint256) the port reproduces exactly for
    every ordered pair at two sizes.

Usage: MAINNET_RPC_URL=... python tools/registry/discover_exits.py [--dry-run]
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path

from eth_abi import decode, encode
from web3 import Web3

sys.path.insert(0, str(Path(__file__).parent))
import crypto_math as cm  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
REG = ROOT / "registry" / "registry.json"
META = ROOT / "registry" / "registry.meta.json"

MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
UNIV2_FACTORY = "0x5c69bee701ef814a2b6a3edd4b1652cb9cc5aa6f"
UNIV2_INIT_HASH = "96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f"
SUSHI_FACTORY = "0xc0aee478e3658e2610c5f7a4a2e1777ce9e4f2ac"
SUSHI_INIT_HASH = "e18a34eb0e04b04f7a0ac29a6e80748dca96319b42c54d679cb821dca90c6303"
CURVE_META_REGISTRY = "0xf98b45fa17de75fb1ad0e7afd971b0ca00e379fc"
# StableSwap-NG factory: `get_pool_asset_types(pool)`.
CURVE_NG_FACTORY = "0x6a8cbed756804b16e05e741edabd5cb544ae21bf"
NATIVE_ETH = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
ZERO = "0x0000000000000000000000000000000000000000"

HUB_ASSETS = {
    "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2": 10**18,  # WETH: 1
    "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48": 2_000 * 10**6,  # USDC
    "0xdac17f958d2ee523a2206206994597c13d831ec7": 2_000 * 10**6,  # USDT
    "0x6b175474e89094c44da98b954eedeac495271d0f": 2_000 * 10**18,  # DAI
    "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599": 3 * 10**6,  # WBTC: 0.03
}
# Minimum sum of normalized (1e18) Curve balances: drops dead pools only.
CURVE_MIN_UNITS = 10 * 10**18
EXCLUDED_QUIRKS = {"rebasing"}
BATCH = 300


def sel(sig: str) -> bytes:
    return Web3.keccak(text=sig)[:4]


class Chain:
    def __init__(self, url: str):
        self.w3 = Web3(Web3.HTTPProvider(url, request_kwargs={"timeout": 60}))
        self.block = self.w3.eth.block_number
        self.timestamp = self.w3.eth.get_block(self.block)["timestamp"]

    def multicall(self, calls: list[tuple[str, bytes]]) -> list[tuple[bool, bytes]]:
        out: list[tuple[bool, bytes]] = []
        for i in range(0, len(calls), BATCH):
            chunk = calls[i : i + BATCH]
            data = sel("tryAggregate(bool,(address,bytes)[])") + encode(
                ["bool", "(address,bytes)[]"],
                [False, [(Web3.to_checksum_address(t), d) for t, d in chunk]],
            )
            for attempt in range(6):
                try:
                    raw = self.w3.eth.call(
                        {"to": MULTICALL3, "data": data}, block_identifier=self.block
                    )
                    res = decode(["(bool,bytes)[]"], raw)[0]
                    out.extend((bool(s), bytes(r)) for s, r in res)
                    break
                except Exception as ex:  # noqa: BLE001 - retry any transport error
                    if attempt == 5:
                        raise RuntimeError(f"multicall chunk {i} failed: {ex}") from ex
                    time.sleep(1.5 * 2**attempt)
        return out


def word(ok_ret: tuple[bool, bytes], typ: str = "uint256"):
    ok, ret = ok_ret
    if not ok or len(ret) < 32:
        return None
    return decode([typ], ret)[0]


def create2_pair(factory: str, init_hash: str, a: str, b: str) -> str:
    t0, t1 = sorted([a.lower(), b.lower()])
    salt = Web3.keccak(bytes.fromhex(t0[2:]) + bytes.fromhex(t1[2:]))
    h = Web3.keccak(
        b"\xff" + bytes.fromhex(factory[2:]) + salt + bytes.fromhex(init_hash)
    )
    return "0x" + h[-20:].hex()


# ── V2 ──────────────────────────────────────────────────────────────────────


def discover_v2(chain: Chain, tracked: set[str]) -> dict[str, dict]:
    pairs = sorted(
        {tuple(sorted((t, h))) for t in tracked for h in HUB_ASSETS if t != h}
    )
    found: dict[str, dict] = {}
    for factory, init_hash in ((UNIV2_FACTORY, UNIV2_INIT_HASH), (SUSHI_FACTORY, SUSHI_INIT_HASH)):
        calls = [
            (factory, sel("getPair(address,address)") + encode(["address", "address"], [a, b]))
            for a, b in pairs
        ]
        got = chain.multicall(calls)
        cands = []
        for (a, b), r in zip(pairs, got):
            addr = word(r, "address")
            if addr is None or addr.lower() == ZERO:
                continue
            addr = addr.lower()
            if addr != create2_pair(factory, init_hash, a, b):
                print(f"  skip {addr}: not the CREATE2 pair of {factory}", file=sys.stderr)
                continue
            cands.append((addr, a, b))
        probe = []
        for addr, _, _ in cands:
            probe.append((addr, sel("getReserves()")))
            probe.append((addr, sel("factory()")))
        res = chain.multicall(probe)
        kept = 0
        for k, (addr, t0, t1) in enumerate(cands):
            rv, fac = res[2 * k], res[2 * k + 1]
            if not rv[0] or word(fac, "address") is None:
                continue
            if word(fac, "address").lower() != factory:
                continue
            r0, r1, _ = decode(["uint112", "uint112", "uint32"], rv[1])
            hub_ok = (t0 in HUB_ASSETS and r0 >= HUB_ASSETS[t0]) or (
                t1 in HUB_ASSETS and r1 >= HUB_ASSETS[t1]
            )
            if not hub_ok:
                continue
            found[addr] = {
                "venue": "univ2",
                "token0": t0,
                "token1": t1,
                "fee": 3000,
                "factory": factory,
                "deployed_block": 0,
                "derived_via": "factory.getPair",
            }
            kept += 1
        print(f"v2 {factory}: {len(cands)} pairs exist, {kept} kept")
    return found


# ── Curve ───────────────────────────────────────────────────────────────────


def curve_get_d(xp: list[int], amp: int, a_prec: int) -> int:
    n = len(xp)
    s = sum(xp)
    if s == 0:
        return 0
    d = s
    ann = amp * n
    for _ in range(255):
        d_p = d
        for x in xp:
            d_p = d_p * d // (x * n)
        prev = d
        d = (ann * s // a_prec + d_p * n) * d // ((ann - a_prec) * d // a_prec + (n + 1) * d_p)
        if abs(d - prev) <= 1:
            return d
    raise ValueError("D did not converge")


def curve_get_y(i: int, j: int, x: int, xp: list[int], amp: int, a_prec: int) -> int:
    n = len(xp)
    d = curve_get_d(xp, amp, a_prec)
    ann = amp * n
    c = d
    s_ = 0
    for k in range(n):
        if k == j:
            continue
        _x = x if k == i else xp[k]
        s_ += _x
        c = c * d // (_x * n)
    c = c * d * a_prec // (ann * n)
    b = s_ + d * a_prec // ann
    y = d
    for _ in range(255):
        prev = y
        y = (y * y + c) // (2 * y + b - d)
        if abs(y - prev) <= 1:
            return y
    raise ValueError("y did not converge")


def curve_get_dy(i, j, dx, balances, rates, amp, a_prec, fee) -> int:
    xp = [r * b // 10**18 for r, b in zip(rates, balances)]
    x = xp[i] + dx * rates[i] // 10**18
    y = curve_get_y(i, j, x, xp, amp, a_prec)
    dy = (xp[j] - y - 1) * 10**18 // rates[j]
    return dy - fee * dy // 10**10


# StableSwap-NG (`CurveStableSwapNG.vy` / `CurveStableSwapNGViews.vy`).


def ng_get_d(xp: list[int], amp: int) -> int:
    n = len(xp)
    s = sum(xp)
    if s == 0:
        return 0
    d = s
    ann = amp * n
    for _ in range(255):
        d_p = d
        for x in xp:
            d_p = d_p * d // x
        d_p //= n**n
        prev = d
        d = (ann * s // 100 + d_p * n) * d // ((ann - 100) * d // 100 + (n + 1) * d_p)
        if abs(d - prev) <= 1:
            return d
    raise ValueError("D did not converge")


def ng_get_y(i: int, j: int, x: int, xp: list[int], amp: int, d: int) -> int:
    n = len(xp)
    ann = amp * n
    c = d
    s_ = 0
    for k in range(n):
        if k == j:
            continue
        _x = x if k == i else xp[k]
        s_ += _x
        c = c * d // (_x * n)
    c = c * d * 100 // (ann * n)
    b = s_ + d * 100 // ann
    y = d
    for _ in range(255):
        prev = y
        y = (y * y + c) // (2 * y + b - d)
        if abs(y - prev) <= 1:
            return y
    raise ValueError("y did not converge")


def ng_dynamic_fee(xpi: int, xpj: int, fee: int, m: int) -> int:
    if m <= 10**10:
        return fee
    xps2 = (xpi + xpj) ** 2
    return m * fee // ((m - 10**10) * 4 * xpi * xpj // xps2 + 10**10)


def ng_get_dy(i, j, dx, balances, rates, amp, fee, offpeg) -> int:
    xp = [r * b // 10**18 for r, b in zip(rates, balances)]
    d = ng_get_d(xp, amp)
    x = xp[i] + dx * rates[i] // 10**18
    y = ng_get_y(i, j, x, xp, amp, d)
    dy = xp[j] - y - 1
    f = ng_dynamic_fee((xp[i] + x) // 2, (xp[j] + y) // 2, fee, offpeg) * dy // 10**10
    return (dy - f) * 10**18 // rates[j]


def discover_curve(chain: Chain, tracked: set[str], tokens: dict) -> dict[str, dict]:
    reg = CURVE_META_REGISTRY
    count = word(chain.multicall([(reg, sel("pool_count()"))])[0])
    pools = [
        word(r, "address").lower()
        for r in chain.multicall(
            [(reg, sel("pool_list(uint256)") + encode(["uint256"], [i])) for i in range(count)]
        )
        if word(r, "address") is not None
    ]
    enc = lambda p: encode(["address"], [p])  # noqa: E731
    meta = chain.multicall(
        [c for p in pools for c in (
            (reg, sel("get_n_coins(address)") + enc(p)),
            (reg, sel("get_coins(address)") + enc(p)),
            (reg, sel("is_meta(address)") + enc(p)),
        )]
    )
    stage1 = []
    for k, p in enumerate(pools):
        n = word(meta[3 * k])
        coins_r = meta[3 * k + 1]
        is_meta = word(meta[3 * k + 2], "bool")
        if n is None or not coins_r[0] or is_meta is None or is_meta or not 2 <= n <= 4:
            continue
        coins = [c.lower() for c in decode(["address[8]"], coins_r[1])[0][:n]]
        if any(
            c in (NATIVE_ETH, ZERO)
            or c not in tracked
            or EXCLUDED_QUIRKS & set(tokens[c]["quirks"])
            for c in coins
        ):
            continue
        stage1.append((p, coins))
    print(f"curve: {len(pools)} registered, {len(stage1)} non-meta with tracked ERC-20 coins")

    # Structural probe.
    probes = []
    layout = []
    for p, coins in stage1:
        start = len(probes)
        for i in range(len(coins)):
            probes.append((p, sel("coins(uint256)") + encode(["uint256"], [i])))
            probes.append((p, sel("balances(uint256)") + encode(["uint256"], [i])))
        for sig in ("A()", "A_precise()", "fee()", "gamma()", "stored_rates()",
                    "offpeg_fee_multiplier()", "future_A_time()"):
            probes.append((p, sel(sig)))
        layout.append(start)
    res = chain.multicall(probes)
    # NG asset types from the NG factory (empty for a pool it did not deploy).
    types_res = chain.multicall(
        [(CURVE_NG_FACTORY, sel("get_pool_asset_types(address)") + enc(p)) for p, _ in stage1]
    )
    stage2 = []
    for (p, coins), start, tr in zip(stage1, layout, types_res):
        n = len(coins)
        r = res[start : start + 2 * n + 7]
        onchain = [word(r[2 * i], "address") for i in range(n)]
        bals = [word(r[2 * i + 1]) for i in range(n)]
        a, a_prec_v, fee, gamma = (word(x) for x in r[2 * n : 2 * n + 4])
        stored_r, offpeg_r, fut_r = r[2 * n + 4 : 2 * n + 7]
        offpeg, fut = word(offpeg_r), word(fut_r)
        stored = None
        if stored_r[0] and len(stored_r[1]) >= 64:
            try:
                stored = list(decode(["uint256[]"], stored_r[1])[0])
            except Exception:  # noqa: BLE001 - a non-array answer is not NG
                stored = None
        if any(c is None or c.lower() != coins[i] for i, c in enumerate(onchain)):
            continue
        if any(b is None or b == 0 for b in bals) or a is None or fee is None:
            continue
        if gamma is not None or fee >= 2**32:
            continue
        if fut is not None and fut > chain.timestamp:
            print(f"  skip {p}: A ramping")
            continue
        if stored is None and offpeg is None:
            kind, types = "curve", []
            amp, a_prec = (a_prec_v, 100) if a_prec_v else (a, 1)
            rates = [10 ** (36 - tokens[c]["decimals"]) for c in coins]
        elif stored is not None and offpeg is not None and a_prec_v:
            types = []
            if tr[0] and len(tr[1]) >= 64:
                types = [int(t) for t in decode(["uint8[]"], tr[1])[0]]
            if len(types) != n or 2 in types or len(stored) != n or 0 in stored:
                print(f"  skip {p}: NG without asset types, or rebasing")
                continue
            kind, amp, a_prec, rates = "curve_ng", a_prec_v, 100, stored
        else:
            continue
        if sum(r_ * b // 10**18 for r_, b in zip(rates, bals)) < CURVE_MIN_UNITS:
            continue
        stage2.append((p, coins, bals, rates, amp, a_prec, fee, kind, offpeg, types))
    print(f"curve: {len(stage2)} pass the structural probe "
          f"({sum(1 for s in stage2 if s[7] == 'curve_ng')} NG)")

    # Exact-math check against the pool's own get_dy.
    checks = []
    for p, coins, bals, *_ in stage2:
        n = len(coins)
        for i in range(n):
            for j in range(n):
                if i == j:
                    continue
                for div in (1_000, 20):
                    dx = max(bals[i] // div, 1)
                    checks.append((p, sel("get_dy(int128,int128,uint256)")
                                   + encode(["int128", "int128", "uint256"], [i, j, dx])))
    res = iter(chain.multicall(checks))
    found: dict[str, dict] = {}
    for p, coins, bals, rates, amp, a_prec, fee, kind, offpeg, types in stage2:
        n = len(coins)
        exact = True
        for i in range(n):
            for j in range(n):
                if i == j:
                    continue
                for div in (1_000, 20):
                    dx = max(bals[i] // div, 1)
                    want = word(next(res))
                    try:
                        if kind == "curve_ng":
                            got = ng_get_dy(i, j, dx, bals, rates, amp, fee, offpeg)
                        else:
                            got = curve_get_dy(i, j, dx, bals, rates, amp, a_prec, fee)
                    except (ValueError, ZeroDivisionError):
                        got = None
                    if want is None or got != want:
                        exact = False
        if not exact:
            print(f"  skip {p}: {kind} math does not reproduce get_dy")
            continue
        found[p] = {
            "venue": kind,
            "token0": coins[0],
            "token1": coins[1],
            "fee": fee,
            "factory": reg,
            "deployed_block": 0,
            "derived_via": "metaregistry.pool_list+get_dy",
            "coins": coins,
        }
        if kind == "curve_ng":
            found[p]["asset_types"] = types
    ng = sum(1 for f in found.values() if f["venue"] == "curve_ng")
    print(f"curve: {len(found)} kept (exact get_dy match), {ng} NG")
    return found


# ── Curve crypto ────────────────────────────────────────────────────────────


def crypto_kind(n: int, version, has_math: bool):
    if n == 2 and not has_math and version is None:
        return "two_v1"
    if n == 2 and has_math and version == "v2.0.0":
        return "two_v200"
    if n == 2 and has_math and version == "v2.1.0":
        return "two_v210"
    if n == 3 and has_math and version == "v2.0.0":
        return "tri"
    return None


def crypto_dy(kind, i, j, dx, bal, prec, ps, D, A, G, mid, out, fg):
    if kind == "tri":
        return cm.tri_get_dy(i, j, dx, bal, prec, ps, D, A, G, mid, out, fg)
    if kind == "two_v1":
        return cm.two_v1_get_dy(i, j, dx, bal, prec, ps[0], D, A, G, mid, out, fg)
    return cm.two_get_dy(i, j, dx, bal, prec, ps[0], D, A, G, mid, out, fg, kind == "two_v210")


def discover_crypto(chain: Chain, tracked: set[str], tokens: dict) -> dict[str, dict]:
    reg = CURVE_META_REGISTRY
    count = word(chain.multicall([(reg, sel("pool_count()"))])[0])
    pools = [
        word(r, "address").lower()
        for r in chain.multicall(
            [(reg, sel("pool_list(uint256)") + encode(["uint256"], [i])) for i in range(count)]
        )
        if word(r, "address") is not None
    ]
    enc = lambda p: encode(["address"], [p])  # noqa: E731
    meta = chain.multicall(
        [c for p in pools for c in (
            (reg, sel("get_n_coins(address)") + enc(p)),
            (reg, sel("get_coins(address)") + enc(p)),
        )]
    )
    stage1 = []
    for k, p in enumerate(pools):
        n = word(meta[2 * k])
        coins_r = meta[2 * k + 1]
        if n is None or not coins_r[0] or not 2 <= n <= 3:
            continue
        coins = [c.lower() for c in decode(["address[8]"], coins_r[1])[0][:n]]
        if any(c in (NATIVE_ETH, ZERO) or c not in tracked
               or EXCLUDED_QUIRKS & set(tokens[c]["quirks"]) for c in coins):
            continue
        stage1.append((p, coins))
    probes = []
    for p, coins in stage1:
        n = len(coins)
        probes += [(p, sel("coins(uint256)") + encode(["uint256"], [i])) for i in range(n)]
        probes += [(p, sel("balances(uint256)") + encode(["uint256"], [i])) for i in range(n)]
        probes += [(p, sel(x)) for x in ("gamma()", "version()", "MATH()", "D()", "A()", "mid_fee()",
                                          "out_fee()", "fee_gamma()", "future_A_gamma_time()",
                                          "price_scale()")]
        probes += [(p, sel("price_scale(uint256)") + encode(["uint256"], [k])) for k in (0, 1)]
    res = chain.multicall(probes)
    stage2 = []
    at = 0
    for p, coins in stage1:
        n = len(coins)
        width = 2 * n + 12
        r = res[at: at + width]
        at += width
        onchain = [word(x, "address") for x in r[:n]]
        bal = [word(x) for x in r[n:2 * n]]
        (gamma_r, ver_r, math_r, d_r, a_r, mid_r, out_r, fg_r, fut_r, ps_r, ps0_r, ps1_r) = r[2 * n:]
        if word(gamma_r) is None:
            continue
        if any(c is None or c.lower() != coins[i] for i, c in enumerate(onchain)):
            continue
        version = None
        if ver_r[0] and len(ver_r[1]) >= 64:
            try:
                version = decode(["string"], ver_r[1])[0]
            except Exception:  # noqa: BLE001
                version = None
        kind = crypto_kind(n, version, math_r[0] and len(math_r[1]) >= 32)
        if kind is None:
            continue
        D, A, G, mid, out, fg, fut = (word(x) for x in (d_r, a_r, gamma_r, mid_r, out_r, fg_r, fut_r))
        ps = [word(ps_r)] if n == 2 else [word(ps0_r), word(ps1_r)]
        if None in (D, A, G, mid, out, fg) or None in ps or any(b is None or b == 0 for b in bal):
            continue
        if fut is not None and fut > chain.timestamp:
            print(f"  skip {p}: A/gamma ramping")
            continue
        prec = [10 ** (18 - tokens[c]["decimals"]) for c in coins]
        stage2.append((p, coins, bal, prec, ps, D, A, G, mid, out, fg, kind))
    print(f"crypto: {len(stage2)} ported kinds pass the structural probe")
    checks = []
    for p, coins, bal, *_ in stage2:
        n = len(coins)
        for i in range(n):
            for j in range(n):
                if i != j:
                    for div in (1_000, 20):
                        checks.append((p, sel("get_dy(uint256,uint256,uint256)")
                                       + encode(["uint256"] * 3, [i, j, max(bal[i] // div, 1)])))
    res = iter(chain.multicall(checks))
    found: dict[str, dict] = {}
    for p, coins, bal, prec, ps, D, A, G, mid, out, fg, kind in stage2:
        n = len(coins)
        exact = True
        for i in range(n):
            for j in range(n):
                if i == j:
                    continue
                for div in (1_000, 20):
                    dx = max(bal[i] // div, 1)
                    want = word(next(res))
                    try:
                        got = crypto_dy(kind, i, j, dx, bal, prec, ps, D, A, G, mid, out, fg)
                    except cm.Revert:
                        got = None
                    # A size the pool's own safety bounds refuse must be
                    # refused by the port too; the small size must answer.
                    if got != want or (div == 1_000 and want is None):
                        exact = False
        if not exact:
            print(f"  skip {p}: {kind} math does not reproduce get_dy")
            continue
        found[p] = {
            "venue": "curve_crypto",
            "token0": coins[0],
            "token1": coins[1],
            "fee": 0,
            "factory": reg,
            "deployed_block": 0,
            "derived_via": "metaregistry.pool_list+get_dy",
            "coins": coins,
            "crypto_kind": kind,
        }
    print(f"crypto: {len(found)} kept (exact get_dy match)")
    return found


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--registry", type=Path, default=None,
                    help="registry.json to read and write (default: the repo's); its meta is the sibling registry.meta.json")
    args = ap.parse_args()
    global REG, META
    if args.registry is not None:
        REG = args.registry
        META = args.registry.with_name("registry.meta.json")
    url = os.environ.get("MAINNET_RPC_URL")
    if not url:
        print("MAINNET_RPC_URL is required", file=sys.stderr)
        return 2
    chain = Chain(url)
    print(f"pinned block {chain.block}")
    reg = json.loads(REG.read_text(encoding="utf-8"))
    tokens = reg["tokens"]
    tracked = set(tokens) - {
        t for t, e in tokens.items() if EXCLUDED_QUIRKS & set(e["quirks"])
    }
    v2 = discover_v2(chain, tracked)
    curve = discover_curve(chain, tracked, tokens)
    crypto = discover_crypto(chain, tracked, tokens)
    pools = {
        a: p for a, p in reg["pools"].items()
        if p["venue"] not in ("univ2", "curve", "curve_ng", "curve_crypto")
    }
    overlap = (set(v2) | set(curve) | set(crypto)) & set(pools)
    if overlap:
        print(f"address collision with existing pools: {sorted(overlap)}", file=sys.stderr)
        return 1
    pools.update(v2)
    pools.update(curve)
    pools.update(crypto)
    reg["pools"] = pools
    print(f"pools: {len(pools)} total ({len(v2)} univ2, {len(curve)} curve incl. NG, "
          f"{len(crypto)} crypto)")
    if args.dry_run:
        return 0
    REG.write_text(json.dumps(reg, indent=1) + "\n", encoding="utf-8")
    meta = json.loads(META.read_text(encoding="utf-8"))
    meta["pool_count"] = len(pools)
    note = (
        f"exits: univ2/curve/curve_ng/curve_crypto discovered by tools/registry/discover_exits.py "
        f"at block {chain.block}"
    )
    meta["notes"] = [n for n in meta.get("notes", []) if not n.startswith("exits: ")] + [note]
    META.write_text(json.dumps(meta, indent=2) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
