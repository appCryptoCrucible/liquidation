#!/usr/bin/env python3
"""Propose pools of the venues added with the third swap module and the V3
forks: SushiSwap V3, PancakeSwap V3, Balancer V2 weighted pools and Fluid
DEX. Proposes only: nothing here writes `registry/registry.json`.

Output (`data/review/`, the same shape `daily_refresh.py` writes, so
`admit_reviewed.py` takes it unchanged):
- `<stem>-registry.candidate.json`: the registry with every proposed pool
  added, the file replays and fork tests read with `LIQ_REGISTRY_FILE`;
- `<stem>-candidates.json`: one entry per pool, `"approve": false`.

Every gate is on chain, at one block:
- V3 forks: the factory's `getPool(a, b, fee)` over every pair of the
  large-swap tokens (`config/large-swap.toml`) and each fork's fee tiers; the
  pool holds liquidity, and its address is the CREATE2 address the Executor
  derives for it (`MainnetVenues.v3ForkPool`), so a plan's pool-direct leg is
  accepted.
- Balancer: the pool ids the Balancer API lists as two-token weighted V2
  pools over registry tokens (the API is a source of candidates only); each
  must be one the Executor's `DexModule` allows (the v4 weighted factory's, or
  one of the reviewed 2021 pools), hold the Vault's tokens in the entry's
  order, be unpaused, and have every `querySwap` answer reproduced by the
  integer port in `balancer_math.py`, both ways, at several sizes.
- Fluid DEX: the factory's pools over registry tokens (native ETH named
  WETH) that `fluid_math.py` follows (no pause, shift or hook) and whose
  answers reproduce the pool's own `swapIn` estimate, both ways, at several
  sizes.

Usage: MAINNET_RPC_URL=... python tools/registry/discover_dex.py
           [--only v3forks,balancer,fluid] [--stem 2026-10-08-dex]
"""
from __future__ import annotations

import argparse
import datetime
import json
import os
import shutil
import sys
import tomllib
import urllib.request
from pathlib import Path

from eth_abi import decode, encode
from web3 import Web3

sys.path.insert(0, str(Path(__file__).parent))
import balancer_math as bm  # noqa: E402
import discover_exits as d  # noqa: E402
import fluid_math as fm  # noqa: E402
import fluid_vectors as fv  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
REVIEW = ROOT / "data" / "review"

# ---- anchors (each checked on chain before it was written here) ----
V3_FORKS = {
    "sushi": {
        "factory": "0xbACEB8eC6b9355Dfc0269C18bac9d6E2Bdc29C4F",
        "deployer": "0xbACEB8eC6b9355Dfc0269C18bac9d6E2Bdc29C4F",
        "init_hash": "0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54",
        "fees": [100, 500, 3000, 10000],
    },
    "pancake": {
        "factory": "0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865",
        "deployer": "0x41ff9AA7e16B8B1a8a8dc4f0eFacd93D02d071c9",
        "init_hash": "0x6ce8eb472fa82df5469c6ab6d485f17c3ad13c8cd7af59b3d4a8026c5ce0f7e2",
        "fees": [100, 500, 2500, 10000],
    },
}
VAULT = "0xBA12222222228d8Ba445958a75a0704d566BF2C8"
QUERIES = "0xE39B5e3B6D74016b2F6A9673D7d7493B6DF549d5"
WEIGHTED_V4_FACTORY = "0x897888115Ada5773E02aA29F775430BFB5F34c51"
# MainnetVenues.balancerLegacyPool
LEGACY_POOLS = {
    "0xa6f548df93de924d73be7d25dc02554c6bd66db500020000000000000000000e",
    "0x96646936b91d6b9d7d0c47c496afbf3d6ec7b6f8000200000000000000000019",
    "0x0b09dea16768f0799065c475be02919503cb2a3500020000000000000000001a",
}
BALANCER_API = "https://api-v3.balancer.fi/"
ME = "0x0000000000000000000000000000000000000001"
ZERO = "0x0000000000000000000000000000000000000000"


def create2(deployer: str, a: str, b: str, fee: int, init_hash: str) -> str:
    t0, t1 = sorted([a, b], key=lambda x: int(x, 16))
    salt = Web3.keccak(encode(["address", "address", "uint24"], [Web3.to_checksum_address(t0), Web3.to_checksum_address(t1), fee]))
    h = Web3.keccak(b"\xff" + bytes.fromhex(deployer[2:]) + salt + bytes.fromhex(init_hash[2:]))
    return Web3.to_checksum_address(h[12:]).lower()


# ---- depth: no dust pool is proposed ----
# Chainlink ETH/USD and BTC/USD (8 decimals); dollar stables at 1.
CHAINLINK = {"eth": "0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419", "btc": "0xF4030086522a5bEEa4988F8cA5B36dbC97BeE88c"}
USD_STABLES = {
    "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",  # USDC
    "0xdac17f958d2ee523a2206206994597c13d831ec7",  # USDT
    "0x6b175474e89094c44da98b954eedeac495271d0f",  # DAI
}
ETH_LIKE = {"0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"}  # WETH (native ETH is named WETH)
BTC_LIKE = {"0x2260fac5e5542a773aa44fbcfedf7c193bc2c599", "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf"}  # WBTC, cbBTC


class Pricer:
    """USD per whole token for the few tokens priced without a guess:
    the three dollar stables, WETH and WBTC/cbBTC from Chainlink at the
    discovery block. Everything else is valued through its pool's other
    side (a two-sided pool holds about as much of each)."""

    def __init__(self, chain: d.Chain, reg: dict):
        self.reg = reg
        rd = d.sel("latestRoundData()")
        res = chain.multicall([(CHAINLINK["eth"], rd), (CHAINLINK["btc"], rd)])
        eth, btc = (decode(["uint80", "int256", "uint256", "uint256", "uint80"], r[1])[1] / 1e8 for r in res)
        self.usd = {t: 1.0 for t in USD_STABLES} | {t: eth for t in ETH_LIKE} | {t: btc for t in BTC_LIKE}

    def depth(self, held: list[tuple[str, int]]) -> float | None:
        """USD held, from `(token, raw amount)` per side; `None` when no side
        is priced."""
        vals = []
        for t, raw in held:
            px = self.usd.get(t.lower())
            if px is not None:
                vals.append(raw / 10 ** self.reg["tokens"][t.lower()].get("decimals", 18) * px)
        if not vals:
            return None
        return sum(vals) if len(vals) == len(held) else 2 * vals[0]


def deep_enough(pricer: Pricer, held: list[tuple[str, int]], min_usd: float, tag: str) -> tuple[bool, str]:
    usd = pricer.depth(held)
    if usd is None:
        print(f"  skip {tag}: no side priced, depth unknown", file=sys.stderr)
        return False, ""
    if usd < min_usd:
        print(f"  skip {tag}: ${usd:,.0f} deep, under ${min_usd:,.0f}", file=sys.stderr)
        return False, ""
    return True, f"${usd:,.0f}"


def v3_forks(chain: d.Chain, reg: dict, pricer: Pricer, min_usd: float) -> list[dict]:
    large = [t.lower() for t in tomllib.loads((ROOT / "config" / "large-swap.toml").read_text())["tokens"]]
    toks = [t for t in large if t in reg["tokens"]]
    out = []
    for name, f in V3_FORKS.items():
        calls, keys = [], []
        for i, a in enumerate(toks):
            for b in toks[i + 1:]:
                t0, t1 = sorted([a, b], key=lambda x: int(x, 16))
                for fee in f["fees"]:
                    calls.append((f["factory"], d.sel("getPool(address,address,uint24)")
                                  + encode(["address", "address", "uint24"], [Web3.to_checksum_address(t0), Web3.to_checksum_address(t1), fee])))
                    keys.append((t0, t1, fee))
        res = chain.multicall(calls)
        pools = []
        for (t0, t1, fee), r in zip(keys, res):
            if not r[0] or len(r[1]) < 32:
                continue
            p = decode(["address"], r[1])[0].lower()
            if int(p, 16) != 0:
                pools.append((p, t0, t1, fee))
        liq = chain.multicall([(p, d.sel("liquidity()")) for p, *_ in pools])
        for (p, t0, t1, fee), lr in zip(pools, liq):
            if d.word(lr) in (None, 0):
                continue
            derived = create2(f["deployer"], t0, t1, fee, f["init_hash"])
            if derived != p:
                print(f"  skip {name} {p}: CREATE2 derives {derived}", file=sys.stderr)
                continue
            held = []
            for t in (t0, t1):
                r = chain.multicall([(Web3.to_checksum_address(t), d.sel("balanceOf(address)") + encode(["address"], [Web3.to_checksum_address(p)]))])[0]
                held.append((t, d.word(r) or 0))
            sym = f"{reg['tokens'][t0].get('symbol')}/{reg['tokens'][t1].get('symbol')}"
            ok, usd = deep_enough(pricer, held, min_usd, f"{name} v3 {sym} {fee} {p}")
            if not ok:
                continue
            out.append({
                "address": p,
                "entry": {"venue": "univ3", "token0": t0, "token1": t1, "fee": fee,
                          "factory": f["factory"].lower(), "deployed_block": 0,
                          "derived_via": "factory.getPool; CREATE2 from the Executor's anchors"},
                "label": f"{name} v3 {sym} {fee} ({usd} held)",
            })
    return out


def balancer_candidates(reg: dict) -> list[dict]:
    q = {"query": "{ poolGetPools(first: 500, orderBy: totalLiquidity, orderDirection: desc, "
                  "where:{chainIn:[MAINNET], minTvl: 50000, protocolVersionIn:[2], poolTypeIn:[WEIGHTED]}) "
                  "{ id address version dynamicData { totalLiquidity } poolTokens { address } } }"}
    req = urllib.request.Request(BALANCER_API, data=json.dumps(q).encode(),
                                 headers={"Content-Type": "application/json", "User-Agent": "Mozilla/5.0"})
    pools = json.load(urllib.request.urlopen(req, timeout=60))["data"]["poolGetPools"]
    return [p for p in pools if len(p["poolTokens"]) == 2
            and all(t["address"].lower() in reg["tokens"] for t in p["poolTokens"])]


def balancer(chain: d.Chain, reg: dict, pricer: Pricer, min_usd: float) -> list[dict]:
    w3 = chain.w3
    out = []

    def view(to, sig, types, args=(), argtypes=()):
        data = d.sel(sig) + (encode(list(argtypes), list(args)) if argtypes else b"")
        return decode(types, w3.eth.call({"to": Web3.to_checksum_address(to), "data": data}, chain.block))

    for cand in balancer_candidates(reg):
        pid = cand["id"].lower()
        pool = "0x" + pid[2:42]
        tag = f"{pool} ({round(float(cand['dynamicData']['totalLiquidity']))} USD)"
        legacy = pid in LEGACY_POOLS
        try:
            v4 = view(WEIGHTED_V4_FACTORY, "isPoolFromFactory(address)", ["bool"], [Web3.to_checksum_address(pool)], ["address"])[0]
        except Exception:  # noqa: BLE001
            v4 = False
        if not (legacy or v4):
            print(f"  skip {tag}: not the v4 weighted factory's and not a reviewed legacy pool", file=sys.stderr)
            continue
        pidb = bytes.fromhex(pid[2:])
        toks, bals, _ = view(VAULT, "getPoolTokens(bytes32)", ["address[]", "uint256[]", "uint256"], [pidb], ["bytes32"])
        toks = [t.lower() for t in toks]
        if len(toks) != 2 or toks != sorted(toks, key=lambda x: int(x, 16)):
            print(f"  skip {tag}: not two tokens in the Vault's ascending order", file=sys.stderr)
            continue
        if not deep_enough(pricer, list(zip(toks, bals)), min_usd, f"balancer {tag}")[0]:
            continue
        if view(pool, "getPausedState()", ["bool", "uint256", "uint256"])[0]:
            print(f"  skip {tag}: paused", file=sys.stderr)
            continue
        weights = view(pool, "getNormalizedWeights()", ["uint256[]"])[0]
        fee = view(pool, "getSwapFeePercentage()", ["uint256"])[0]
        scaling = [10 ** (18 - reg["tokens"][t]["decimals"]) for t in toks]
        fast = not legacy
        # every Vault answer reproduced, both kinds, both ways, several sizes
        ok = True
        for i, j in ((0, 1), (1, 0)):
            for num, den in ((1, 10**7), (1, 10**4), (1, 100), (10, 100), (29, 100)):
                for kind in (0, 1):
                    base = bals[i] if kind == 0 else bals[j]
                    amt = max(base * num // den, 1)
                    data = d.sel("querySwap((bytes32,uint8,address,address,uint256,bytes),(address,bool,address,bool))") + encode(
                        ["(bytes32,uint8,address,address,uint256,bytes)", "(address,bool,address,bool)"],
                        [(pidb, kind, Web3.to_checksum_address(toks[i]), Web3.to_checksum_address(toks[j]), amt, b""),
                         (ME, False, ME, False)])
                    try:
                        want = decode(["uint256"], w3.eth.call({"to": QUERIES, "data": data}, chain.block))[0]
                    except Exception:  # noqa: BLE001
                        want = None
                    try:
                        got = (bm.swap_given_in if kind == 0 else bm.swap_given_out)(
                            i, j, amt, bals, list(weights), scaling, fee, fast)
                    except bm.Revert:
                        got = None
                    if got != want:
                        print(f"  skip {tag}: port {got} chain {want} ({i}->{j} kind {kind} amt {amt})", file=sys.stderr)
                        ok = False
                        break
                if not ok:
                    break
            if not ok:
                break
        if not ok:
            continue
        sym = [reg["tokens"][t].get("symbol") for t in toks]
        out.append({
            "address": pool,
            "entry": {"venue": "balancer", "token0": toks[0], "token1": toks[1], "fee": 0,
                      "factory": WEIGHTED_V4_FACTORY.lower() if v4 else ZERO, "deployed_block": 0,
                      "derived_via": "balancer api candidate; vault.getPoolTokens; querySwap reproduced",
                      "coins": toks, "pool_id": pid, "balancer_kind": "weighted_v4" if v4 else "weighted_v1"},
            "label": f"balancer {sym[0]}/{sym[1]} {'v4' if v4 else 'legacy'} fee {fee / 1e16:.3f}%",
        })
    return out


def fluid(chain: d.Chain, reg: dict, pricer: Pricer, min_usd: float) -> list[dict]:
    """Fluid DEX T1 pools over registry tokens that the model follows (no
    pause, shift or hook) and whose model answers reproduce the pool's own
    `swapIn` estimate at several sizes, both ways."""
    weth = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
    out = []
    for pid, pool in enumerate(fv.all_pools(chain), 1):
        rec = fv.read_pool(chain, pid, pool)
        if rec is None:
            continue
        native = (rec["token0"] == fv.NATIVE, rec["token1"] == fv.NATIVE)
        coins = [weth if n else t.lower() for n, t in zip(native, (rec["token0"], rec["token1"]))]
        if any(c not in reg["tokens"] for c in coins):
            continue
        snap = fv.snapshot_of(rec)
        checked = 0
        ok = True
        for z in (True, False):
            tin = rec["tokens"][0 if z else 1]
            scale = max(int(tin["balance"]), 10**6)
            for bps in (1, 10, 100):
                amt = max(scale * bps // 10_000, 1000)
                est = fv.estimate(chain, pool, z, amt, amt if native[0 if z else 1] else 0)
                if "out" not in est:
                    continue
                try:
                    got, _ = fm.swap_in(snap, z, amt, rec["timestamp"])
                except fm.Revert:
                    continue  # a limit the estimate cannot see
                checked += 1
                if got != int(est["out"]):
                    ok = False
        if not ok or checked == 0:
            print(f"  skip fluid {pool}: model {'disagrees' if not ok else 'has no answered size'}", file=sys.stderr)
            continue
        sym = [reg["tokens"][c].get("symbol") for c in coins]
        # What the pool holds in the Liquidity layer: its supply and its debt
        # of each token, in token units.
        held = []
        for k, c in enumerate(coins):
            t = rec["tokens"][k]
            sep, bep = fm.calc_exchange_prices(int(t["ep_cfg"]), rec["timestamp"])
            amt = 0
            for word, ep in ((int(t["supply"]), sep), (int(t["borrow"]), bep)):
                a = fm.from_big((word >> 1) & fm.X[64])
                amt += a * ep // 10**12 if word & 1 else a
            held.append((c, amt))
        ok, usd = deep_enough(pricer, held, min_usd, f"fluid #{pid} {sym[0]}/{sym[1]} {pool}")
        if not ok:
            continue
        out.append({
            "address": pool.lower(),
            "entry": {"venue": "fluid", "token0": coins[0], "token1": coins[1], "fee": 0,
                      "factory": fv.FACTORY.lower(), "deployed_block": 0,
                      "derived_via": "dex factory getDexAddress; model reproduces swapIn estimates",
                      "coins": coins, "native": any(native)},
            "label": f"fluid #{pid} {sym[0]}/{sym[1]}" + (" (native ETH)" if any(native) else "") + f" ({usd} held)",
        })
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", default="v3forks,balancer,fluid")
    ap.add_argument("--stem", default=f"{datetime.date.today().isoformat()}-dex")
    ap.add_argument("--registry", type=Path, default=d.REG)
    ap.add_argument("--min-depth-usd", type=float, default=250_000,
                    help="propose no V3-fork or Fluid pool holding less (USD)")
    args = ap.parse_args()
    only = set(args.only.split(","))
    chain = d.Chain(os.environ["MAINNET_RPC_URL"])
    reg = json.loads(args.registry.read_text(encoding="utf-8"))
    found: list[dict] = []
    pricer = Pricer(chain, reg)
    if "v3forks" in only:
        found += v3_forks(chain, reg, pricer, args.min_depth_usd)
    if "balancer" in only:
        found += balancer(chain, reg, pricer, args.min_depth_usd)
    if "fluid" in only:
        found += fluid(chain, reg, pricer, args.min_depth_usd)
    added = []
    for f in found:
        if f["address"] in reg["pools"]:
            continue
        reg["pools"][f["address"]] = f["entry"]
        added.append(f)
    REVIEW.mkdir(parents=True, exist_ok=True)
    # The loader reads the asset-id ledger beside the registry file; no pool
    # added here adds a token, so the admitted ledger is the candidate's.
    shutil.copyfile(ROOT / "registry" / "asset-ids.json", REVIEW / "asset-ids.json")
    cand_reg = REVIEW / f"{args.stem}-registry.candidate.json"
    cand_reg.write_text(json.dumps(reg, indent=2) + "\n", encoding="utf-8")
    entries = [{
        "kind": "pool", "action": "add", "address": f["address"], "venue": f["entry"]["venue"],
        "tokens": [f["entry"]["token0"], f["entry"]["token1"]], "source": str(cand_reg.relative_to(ROOT)),
        "approve": False, "note": f["label"] + f" (block {chain.block})",
    } for f in added]
    (REVIEW / f"{args.stem}-candidates.json").write_text(json.dumps({
        "date": datetime.date.today().isoformat(),
        "note": "Pools of the Sushi V3, Pancake V3, Balancer V2 and Fluid DEX venues; checked on chain at "
                f"block {chain.block}. Set approve to true on those to admit.",
        "entries": entries}, indent=2) + "\n", encoding="utf-8")
    print(f"{len(added)} pools proposed at block {chain.block}")
    for f in added:
        print("  ", f["label"], f["address"])
    return 0


if __name__ == "__main__":
    sys.exit(main())
