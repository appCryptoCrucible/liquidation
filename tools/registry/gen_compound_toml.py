#!/usr/bin/env python3
"""Generate config/protocols/compound-v2.toml, and intern the forks'
underlyings, from the chain and the registry.

Every registry `compound-v2` Comptroller is read at one pinned block and
classified (docs/coverage/compound-forks.md):

  plain V2   `getAllMarkets`, `oracle`, `closeFactorMantissa` and
             `liquidationIncentiveMantissa` answer; every cToken answers
             `accrualBlockNumber`, `borrowRatePerBlock` and names this
             Comptroller            -> bound, `variant = "compound"`
  Fuse       the same, and its cTokens answer `totalFuseFees()`
                                     -> bound, `variant = "fuse"`
  Moma       the same, its mTokens name the Comptroller `momaMaster()` and
             answer `totalMomaFees()` -> bound, `variant = "moma"`
  DeFiPie    the same as plain V2, its pTokens name the Controller
             `controller()`, and they emit the five-field `AccrueInterest`
             (the adapter takes either) -> bound, `variant = "compound"`
  per-second a cToken accrues by `accrualBlockTimestamp`  -> left out
  not V2     any view above missing                       -> left out

Within a bound fork, each listed cToken is one of:

  own        answers the views above and names this Comptroller -> pinned
  frozen     names this Comptroller, has no `borrowRatePerBlock`, and an
             `eth_call` of `accrueInterest()` reverts: its rate model
             reverts, so its stored totals never move and no liquidation
             touching it can land               -> pinned `frozen = true`
  dropped    anything else: another Comptroller's market (a copycat lists
             it), or code that is not Compound's (Inverse's xINV)
                                                -> not pinned; an account
             that entered it fails closed (its events are not followed)

Every pinned cToken's deployed code must hold the topics the adapter's state
comes from: `Mint`, `Redeem`, `Borrow`, `RepayBorrow`, `Transfer`, and
`AccrueInterest` in one of its three forms (the original 2019 one without
`cashPrior`, the four-field one, DeFiPie's with `totalReserves`). The code
read is the implementation's behind a Compound delegator (`implementation()`),
an EIP-1967 proxy (its slot), an EIP-1167 clone, or DeFiPie's
`ProxyWithRegistry` (`registry().pTokenImplementation()`). One without them is
dropped as not Compound's code: an NFT-collateral `CErc721` market, whose
`Borrow` has other fields, or an accrual we would not see. (`LiquidateBorrow`
only marks its two accounts; their balances move by `Transfer` and
`RepayBorrow`.)

A fork binds when it keeps at least one own cToken.

Every bound fork is followed, borrowed-from or idle (plan 1B: no admission
floor). Each cToken is pinned with its `underlying()`; one that answers no
underlying is CEther, and the fork's `native` is WETH. Underlyings that are
not registry tokens are interned (their own symbol and decimals; ids appended
to `registry/asset-ids.json`). Their exits come from the usual discovery.

Usage: MAINNET_RPC_URL=... python tools/registry/gen_compound_toml.py [--dry-run]
"""
from __future__ import annotations

import argparse
import collections
import json
import os
import sys
import time
from pathlib import Path

from eth_abi import decode
from eth_utils import keccak

sys.path.insert(0, str(Path(__file__).parent))
from discover_exits import Chain, sel, word  # noqa: E402

REG = Path("registry/registry.json")
LEDGER = Path("registry/asset-ids.json")
OUT = Path("config/protocols/compound-v2.toml")
WETH = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
PROTOCOL = 3
CSIGS = ("getAllMarkets()", "oracle()", "closeFactorMantissa()", "liquidationIncentiveMantissa()")
TSIGS = ("underlying()", "accrualBlockNumber()", "accrualBlockTimestamp()", "comptroller()",
         "borrowRatePerBlock()", "totalFuseFees()", "totalBorrows()", "momaMaster()",
         "totalMomaFees()", "controller()")


def text(ok_ret) -> str | None:
    ok, raw = ok_ret
    if not ok or len(raw) < 64:
        return None
    try:
        return decode(["string"], raw)[0]
    except Exception:  # noqa: BLE001 - bytes32 metadata: left out, reported
        return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()
    chain = Chain(os.environ["MAINNET_RPC_URL"])
    reg = json.loads(REG.read_text(encoding="utf-8"))
    ledger = json.loads(LEDGER.read_text(encoding="utf-8"))
    ids = {a.lower(): int(i) for a, i in ledger["ids"].items()}
    tokens = {a.lower() for a in reg["tokens"]}
    comps = sorted({p["market"].lower() for p in reg["protocols"].values()
                    if p["family"] == "compound-v2"})

    res = chain.multicall([(c, sel(s)) for c in comps for s in CSIGS])
    markets: dict[str, list[str] | None] = {}
    head_ok: dict[str, bool] = {}
    dead: dict[str, bool] = {}
    for i, c in enumerate(comps):
        r = res[i * len(CSIGS):(i + 1) * len(CSIGS)]
        mk = decode(["address[]"], r[0][1])[0] if r[0][0] and len(r[0][1]) >= 64 else None
        markets[c] = [m.lower() for m in mk] if mk is not None else None
        head_ok[c] = mk is not None and all(word(x) is not None for x in r[1:])
        # A zero close factor repays nothing; a zero incentive seizes nothing.
        dead[c] = head_ok[c] and (word(r[2]) == 0 or word(r[3]) == 0)
    pairs = [(c, m) for c in comps for m in (markets[c] or [])]
    res = chain.multicall([(m, sel(s)) for _, m in pairs for s in TSIGS])
    ct: dict[str, list[dict]] = collections.defaultdict(list)
    for i, (c, m) in enumerate(pairs):
        r = res[i * len(TSIGS):(i + 1) * len(TSIGS)]
        under = word(r[0], "address")
        ct[c].append({
            "ctoken": m,
            "underlying": under.lower() if under and int(under, 16) else None,
            "by_block": word(r[1]) is not None,
            "by_time": word(r[2]) is not None,
            # A Moma mToken names its Comptroller `momaMaster()`; a DeFiPie
            # pToken (`PToken` 0xb4ef9b69…) names it `controller()`.
            "comptroller": (word(r[3], "address") or word(r[7], "address")
                            or word(r[9], "address") or "").lower(),
            "moma": word(r[8]) is not None,
            "rate": word(r[4]) is not None,
            "fuse": word(r[5]) is not None,
            "borrows": word(r[6]) or 0,
        })

    # The events the adapter follows, proven from each cToken's code.
    need = [keccak(text=t) for t in (
        "Mint(address,uint256,uint256)", "Redeem(address,uint256,uint256)",
        "Borrow(address,uint256,uint256,uint256)",
        "RepayBorrow(address,address,uint256,uint256,uint256)",
        "Transfer(address,address,uint256)")]
    accrual = [keccak(text="AccrueInterest(uint256,uint256,uint256)"),
               keccak(text="AccrueInterest(uint256,uint256,uint256,uint256)"),
               keccak(text="AccrueInterest(uint256,uint256,uint256,uint256,uint256)")]
    eip1967 = 0x360894A13BA1A3210667C828492DB98DCA3E2076CC3735A920A3CA505D382BBC
    every = [t for c in comps for t in ct.get(c, [])]
    impls = chain.multicall([(t["ctoken"], sel("implementation()")) for t in every])
    code_ok: dict[str, bool] = {}

    def retry(f):
        for attempt in range(8):
            try:
                return f()
            except Exception:  # noqa: BLE001 - a throttled endpoint
                if attempt == 7:
                    raise
                time.sleep(1.5 * 2**attempt)

    def code_of(a: str) -> bytes:
        return bytes(retry(lambda: chain.w3.eth.get_code(chain.w3.to_checksum_address(a), chain.block)))

    def logic(a: str, delegate: str | None) -> bytes:
        if delegate:
            return code_of(delegate)
        code = code_of(a)
        # EIP-1167 clone: 363d3d373d3d3d363d73 <impl> 5af43d82803e903d91602b57fd5bf3
        if len(code) == 45 and code[:10] == bytes.fromhex("363d3d373d3d3d363d73"):
            return code_of("0x" + code[10:30].hex())
        slot = retry(lambda: chain.w3.eth.get_storage_at(chain.w3.to_checksum_address(a), eip1967, chain.block))
        impl = int.from_bytes(bytes(slot)[-20:], "big")
        if impl:
            return code_of(f"0x{impl:040x}")
        # DeFiPie's `ProxyWithRegistry`: `registry().pTokenImplementation()`.
        if len(code) < 2048:
            reg_ = word(chain.multicall([(a, sel("registry()"))])[0], "address")
            if reg_:
                at = word(chain.multicall([(reg_, sel("pTokenImplementation()"))])[0], "address")
                if at:
                    return code_of(at)
        return code

    for t, r in zip(every, impls):
        delegate = word(r, "address")
        key = (delegate or t["ctoken"]).lower()
        if key not in code_ok:
            code = logic(t["ctoken"], delegate)
            code_ok[key] = all(h in code for h in need) and any(h in code for h in accrual)
        t["events_ok"] = code_ok[key]

    # Frozen candidates: own, per-block, no rate. Proven by `accrueInterest`.
    cands = [t for c in comps for t in ct.get(c, [])
             if t["comptroller"] == c and t["by_block"] and not t["rate"]]
    res = chain.multicall([(t["ctoken"], sel("accrueInterest()")) for t in cands])
    for t, (ok, _) in zip(cands, res):
        t["frozen"] = not ok

    bound, left = [], collections.defaultdict(list)
    dropped = collections.Counter()
    for c in comps:
        listed = ct.get(c, [])
        ts = [t for t in listed
              if t["by_block"] and t["comptroller"] == c and (t["rate"] or t.get("frozen"))
              and t["events_ok"]]
        ct[c] = ts
        if not head_ok[c]:
            left["not a V2 Comptroller (views missing)"].append(c)
        elif dead[c]:
            left["close factor or incentive zero (nothing to liquidate)"].append(c)
        elif any(t["by_time"] and not t["by_block"] for t in listed):
            left["accrues per second"].append(c)
        elif not any(not t.get("frozen") for t in ts):
            left["no own Compound market" if listed else "no markets"].append(c)
        else:
            fuse, moma = all(t["fuse"] for t in ts), all(t["moma"] for t in ts)
            if (any(t["fuse"] for t in ts) and not fuse) or (any(t["moma"] for t in ts) and not moma):
                left["mixed fee-accumulator and plain cTokens"].append(c)
                continue
            bound.append((c, "fuse" if fuse else "moma" if moma else "compound"))
            for t in listed:
                if t not in ts:
                    dropped["another Comptroller's" if t["comptroller"] != c else "not Compound's code"] += 1

    # Underlyings to intern.
    under = sorted({t["underlying"] for c, _ in bound for t in ct[c] if t["underlying"]} - tokens)
    meta = chain.multicall([(u, sel(s)) for u in under for s in ("symbol()", "decimals()")])
    new_tokens, bad = {}, []
    for i, u in enumerate(under):
        sym, dec = text(meta[2 * i]), word(meta[2 * i + 1])
        if sym is None or dec is None or dec > 255:
            bad.append(u)
            continue
        new_tokens[u] = {"symbol": sym, "decimals": int(dec),
                         "quirks": ["low_decimals"] if dec < 18 else []}

    kinds = collections.Counter(v for _, v in bound)
    live = sum(1 for c, _ in bound if any(t["borrows"] for t in ct[c]))
    print(f"block {chain.block}: {len(comps)} registry Comptrollers; bound {len(bound)} "
          f"({dict(kinds)}; {live} with borrows), cTokens {sum(len(ct[c]) for c, _ in bound)}")
    for why, xs in left.items():
        print(f"  left out, {why}: {len(xs)}")
    frozen = sum(1 for c, _ in bound for t in ct[c] if t.get("frozen"))
    print(f"  cTokens frozen: {frozen}; dropped from bound forks: {dict(dropped)}")
    print(f"  underlyings outside the registry: {len(under)}; interned {len(new_tokens)}, "
          f"refused (no string symbol or decimals) {len(bad)}")
    if args.dry_run:
        return 0

    nxt = int(ledger["next"])
    for u, t in new_tokens.items():
        reg["tokens"][u] = t
        if u not in ids:
            ledger["ids"][u] = nxt
            ids[u] = nxt
            nxt += 1
    ledger["next"] = nxt
    if max(ids.values()) >= 0xFFFF:
        raise SystemExit("asset id would reach u16::MAX")
    # Each file's committed formatting.
    REG.write_text(json.dumps(reg, indent=1) + "\n", encoding="utf-8")
    LEDGER.write_text(json.dumps(ledger, indent=2) + "\n", encoding="utf-8")

    lines = [
        "# Compound V2 family (family `compound-v2` in registry/registry.json).",
        "# GENERATED by tools/registry/gen_compound_toml.py — edit the generator, not this file.",
        f"# Read at block {chain.block}: {len(comps)} registry Comptrollers, {len(bound)} bound",
        f"# ({kinds.get('compound', 0)} plain V2 @ a3214f67, {kinds.get('fuse', 0)} Rari Fuse,",
        f"# {kinds.get('moma', 0)} Moma Lending Pool).",
        "# Left out: " + "; ".join(f"{len(v)} {k}" for k, v in left.items()) + ".",
        "# closeFactor / liquidationIncentive / oracle are read live at bind",
        "# (`Config::assert_live_registry`), per fork: a fork failing its check is",
        "# logged and left out, the others bind. cToken underlyings are pinned here;",
        "# a cToken with no underlying() is CEther, priced and flashed as `native`.",
        "",
        f"protocol = {PROTOCOL}",
        f"pinned_through = {chain.block}",
    ]
    for c, variant in bound:
        lines += ["", "[[forks]]", f'comptroller = "{c}"']
        if variant != "compound":
            lines.append(f'variant = "{variant}"')
        if any(t["underlying"] is None for t in ct[c]):
            lines.append(f'native = "{WETH}"')
        for t in ct[c]:
            lines += ["", "[[forks.ctokens]]", f'address = "{t["ctoken"]}"']
            if t["underlying"]:
                lines.append(f'underlying = "{t["underlying"]}"')
            if t.get("frozen"):
                lines.append("frozen = true")
    OUT.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"wrote {OUT}, {len(new_tokens)} tokens to {REG}, ledger next {nxt}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
