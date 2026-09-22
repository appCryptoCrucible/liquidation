"""WP C2 triage: apply the verified C1 fixes to registry/registry.json.

All values are read from chain at one pinned block. Nothing is inferred:
  - Aave V4 spokes/hubs come from getSpokeCount/getSpokeAddress (canonical accessor),
    replacing C1's windowed-log guess.
  - Morpho debt uses expectedTotalBorrowAssets (market() + IRM.borrowRateView accrual,
    Morpho MathLib.wTaylorCompounded) instead of the pre-accrual market() slot.
  - Silo debt uses getDebtAssets() (interest-accrued) instead of the stale storage total.
  - Euler totalBorrows() / Ajna debtInfo() are already accrued; refreshed at the same block.
  - A token that cannot be priced from an on-chain source stays un-admitted and is logged.
  - Tokens whose on-chain symbol collides with a different canonical address get an
    explicit identity flag so nothing downstream can key on the symbol alone.

Run:  python tools/registry/triage_apply_fixes.py [--write]
"""

from __future__ import annotations

import json
import sys
import urllib.request
from collections import Counter
from pathlib import Path

from eth_abi import decode, encode
from web3 import Web3

ROOT = Path(__file__).resolve().parents[2]
REG = ROOT / "registry" / "registry.json"
META = ROOT / "registry" / "registry.meta.json"
C1 = json.loads(REG.read_text())
M1 = json.loads(META.read_text())
BLOCK = C1["generated_at_block"]
THRESH = 50_000.0
WAD = 10**18

w3 = Web3(Web3.HTTPProvider("https://gateway.tenderly.co/public/mainnet", request_kwargs={"timeout": 60}))
MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
MORPHO = "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb"
FEED_REGISTRY = "0x47Fb2585D2C56Fe188D0E6ec628a38b74fCeeeDf"
USD_DENOM = "0x0000000000000000000000000000000000000348"
ETH_ALIAS = {"0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2": "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE"}
AAVE_V4_HUBS = {
    "core": "0xcca852bc40e560adc3b1cc58ca5b55638ce826c9",
    "plus": "0x06002e9c4412cb7814a791ea3666d905871e536a",
    "prime": "0x943827dca022d0f354a8a8c332da1e5eb9f9f931",
    "paxos": "0x62d63197660c080236193ca60b70e49a08e90368",
}
DEBT_FIELD = {"morpho-blue": "loan_token", "euler-v2": "asset", "silo-v2": "asset", "ajna": "quote_token"}
ORACLES = list(
    dict.fromkeys(
        p["price_oracle"]
        for p in C1["protocols"].values()
        if p.get("family") in ("aave-v3", "spark") and p.get("price_oracle")
    )
)
failures: dict[str, str] = {}
notes: list[str] = []


def cs(a):
    return Web3.to_checksum_address(a)


def lc(a):
    return a.lower() if isinstance(a, str) else a


def sel(s):
    return Web3.keccak(text=s)[:4]


def multicall(calls):
    out = []
    for i in range(0, len(calls), 250):
        part = calls[i : i + 250]
        data = sel("tryAggregate(bool,(address,bytes)[])") + encode(
            ["bool", "(address,bytes)[]"], [False, [(cs(a), d) for a, d in part]]
        )
        raw = w3.eth.call({"to": cs(MULTICALL3), "data": data}, block_identifier=BLOCK)
        out.extend(decode(["(bool,bytes)[]"], raw)[0])
    return out


def u256(b):
    return int.from_bytes(b[:32], "big") if b and len(b) >= 32 else None


def addr_word(b):
    if not b or len(b) < 32:
        return None
    a = "0x" + b[12:32].hex()
    return None if int(a, 16) == 0 else a


def w_taylor_compounded(rate: int, t: int) -> int:
    x = rate * t
    return x + (x * x) // (2 * WAD) + (x * x * x) // (6 * WAD * WAD)


TS = w3.eth.get_block(BLOCK)["timestamp"]
print(f"pinned block {BLOCK} ts={TS}")

# ================================================================= 1. prices
fam_b = {k: p for k, p in C1["protocols"].items() if p.get("family") in DEBT_FIELD}
debt_tokens = sorted({lc(p.get(DEBT_FIELD[p["family"]])) for p in fam_b.values() if p.get(DEBT_FIELD[p["family"]])})
print(f"Family B entries={len(fam_b)} distinct debt tokens={len(debt_tokens)}")

prices: dict[str, float] = {}
sources: dict[str, str] = {}
calls = []
for a in debt_tokens:
    args = encode(["address", "address"], [cs(ETH_ALIAS.get(a, a)), cs(USD_DENOM)])
    calls.append((FEED_REGISTRY, sel("latestRoundData(address,address)") + args))
    calls.append((FEED_REGISTRY, sel("decimals(address,address)") + args))
r = multicall(calls)
for i, a in enumerate(debt_tokens):
    ok, raw = r[2 * i]
    okd, drw = r[2 * i + 1]
    if ok and len(raw) >= 160 and okd and len(drw) >= 32:
        ans = int.from_bytes(raw[32:64], "big", signed=True)
        if ans > 0:
            prices[a] = ans / 10 ** u256(drw)
            sources[a] = "chainlink-feed-registry"
for o in ORACLES:
    left = [a for a in debt_tokens if a not in prices]
    if not left:
        break
    unit = u256(multicall([(o, sel("BASE_CURRENCY_UNIT()"))])[0][1]) or 0
    if unit <= 0:
        continue
    res = multicall([(o, sel("getAssetPrice(address)") + encode(["address"], [cs(a)])) for a in left])
    for a, (ok, raw) in zip(left, res):
        if ok and (v := u256(raw)) and v > 0:
            prices[a] = v / unit
            sources[a] = f"aave-oracle:{o}"
print(f"priced {len(prices)}/{len(debt_tokens)} debt tokens on-chain")
for a in debt_tokens:
    if a not in prices:
        sym = (C1["tokens"].get(a) or {}).get("symbol") or "?"
        failures[f"admission:unpriced:{a}"] = f"no USD source ({sym}): feed registry + aave/spark oracles"

# ================================================================= 2. debt, per family
new_raw: dict[str, int | None] = {}

# --- morpho: accrued (expectedTotalBorrowAssets) -------------------------------
mk = [(k, p) for k, p in fam_b.items() if p["family"] == "morpho-blue"]
res = multicall([(MORPHO, sel("market(bytes32)") + encode(["bytes32"], [bytes.fromhex(p["market"][2:])])) for _, p in mk])
state = {}
for (k, p), (ok, raw) in zip(mk, res):
    if ok and len(raw) >= 192:
        state[k] = decode(["uint128"] * 6, raw)
    else:
        failures[f"morpho-blue:market:{p['market']}"] = "market() failed"
calls, order = [], []
for k, p in mk:
    if k not in state or not p.get("irm") or int(p["irm"], 16) == 0:
        continue
    mp = (cs(p["loan_token"]), cs(p["collateral_token"]), cs(p["oracle"]), cs(p["irm"]), int(p["lltv"]))
    calls.append(
        (
            p["irm"],
            sel(
                "borrowRateView((address,address,address,address,uint256),"
                "(uint128,uint128,uint128,uint128,uint128,uint128))"
            )
            + encode(
                [
                    "(address,address,address,address,uint256)",
                    "(uint128,uint128,uint128,uint128,uint128,uint128)",
                ],
                [mp, state[k]],
            ),
        )
    )
    order.append(k)
res = multicall(calls)
rate = {}
for k, (ok, raw) in zip(order, res):
    if ok and (v := u256(raw)) is not None:
        rate[k] = v
    else:
        failures[f"morpho-blue:borrowRateView:{k.split(':')[1]}"] = "borrowRateView() failed"
n_accrued = 0
for k, p in mk:
    if k not in state:
        new_raw[k] = None
        continue
    ba, lu = state[k][2], state[k][4]
    if k in rate:
        ba2 = ba + (ba * w_taylor_compounded(rate[k], max(0, TS - lu))) // WAD
        n_accrued += ba2 != ba
    else:
        ba2 = ba  # rate unavailable: keep the stored figure, logged above
    new_raw[k] = ba2
no_irm = [k for k, p in mk if k in state and (not p.get("irm") or int(p["irm"], 16) == 0)]
print(
    f"morpho: {len(state)} markets read, {len(rate)} rates, {n_accrued} accrued upward; "
    f"{len(no_irm)} markets have irm=0 (no interest accrues, stored == accrued), "
    f"{len(order) - len(rate)} borrowRateView call failures"
)

# --- euler: totalBorrows() (already accrued) -----------------------------------
ek = [(k, p) for k, p in fam_b.items() if p["family"] == "euler-v2"]
res = multicall([(p["market"], sel("totalBorrows()")) for _, p in ek])
for (k, p), (ok, raw) in zip(ek, res):
    v = u256(raw) if ok else None
    if v is None:
        failures[f"euler-v2:totalBorrows:{p['market']}"] = "totalBorrows() failed"
    new_raw[k] = v

# --- silo: getDebtAssets() (interest-accrued) ----------------------------------
sk = [(k, p) for k, p in fam_b.items() if p["family"] == "silo-v2"]
res = multicall([(p["market"], sel("getDebtAssets()")) for _, p in sk])
for (k, p), (ok, raw) in zip(sk, res):
    v = u256(raw) if ok else None
    if v is None:
        failures[f"silo-v2:getDebtAssets:{p['market']}"] = "getDebtAssets() failed"
    new_raw[k] = v

# --- ajna: debtInfo() debt_ is WAD-normalised, convert to quote-token units ----
ak = [(k, p) for k, p in fam_b.items() if p["family"] == "ajna"]
res = multicall([(p["market"], sel("debtInfo()")) for _, p in ak])
ajna_wad: dict[str, int] = {}
for (k, p), (ok, raw) in zip(ak, res):
    if not ok or len(raw) < 128:
        failures[f"ajna:debtInfo:{p['market']}"] = "debtInfo() failed"
        new_raw[k] = None
        continue
    wad = u256(raw)
    ajna_wad[k] = wad
    q = lc(p.get("quote_token"))
    d = (C1["tokens"].get(q) or {}).get("decimals") if q else None
    new_raw[k] = (wad * 10**d) // WAD if d is not None else None
    if d is None:
        failures[f"ajna:quote_decimals:{p['market']}"] = "quote token decimals unknown"

# ================================================================= 3. reprice + admit
before = Counter(p["family"] for p in fam_b.values() if p.get("admitted"))
changes = {"raw": 0, "usd": 0, "admit_add": [], "admit_drop": []}
for k, p in fam_b.items():
    tok = lc(p.get(DEBT_FIELD[p["family"]]))
    d = (C1["tokens"].get(tok) or {}).get("decimals") if tok else None
    raw = new_raw.get(k)
    old_raw, old_usd, old_adm = p.get("borrowed_raw"), p.get("borrowed_usd"), p.get("admitted")
    if raw is None:
        usd = None
    elif raw == 0:
        usd = 0.0
    elif d is None or tok not in prices:
        usd = None
        if d is None and tok:
            failures[f"admission:nodecimals:{tok}"] = "token decimals unknown; not admitted"
    else:
        usd = raw / 10**d * prices[tok]
    adm = usd is not None and usd >= THRESH
    if k in ajna_wad:
        p["debt_wad"] = ajna_wad[k]
    if raw != old_raw:
        changes["raw"] += 1
    if usd != old_usd:
        changes["usd"] += 1
    if adm and not old_adm:
        changes["admit_add"].append((k, p["family"], usd))
    if old_adm and not adm:
        changes["admit_drop"].append((k, p["family"], old_usd, usd))
    p["borrowed_raw"] = raw
    p["borrowed_usd"] = usd
    p["admitted"] = adm

after = Counter(p["family"] for p in fam_b.values() if p.get("admitted"))
print(f"\nFamily B admitted before={dict(before)} after={dict(after)}")
print(f"borrowed_raw changed on {changes['raw']} entries; borrowed_usd on {changes['usd']}")
print(f"newly admitted: {len(changes['admit_add'])}")
for c in changes["admit_add"]:
    print(f"   + {c[1]:12} {c[0][:60]} ${c[2]:,.0f}")
print(f"de-admitted: {len(changes['admit_drop'])}")
for c in changes["admit_drop"]:
    print(f"   - {c[1]:12} {c[0][:60]} was ${c[2]:,.0f} now {c[3]}")

# ================================================================= 4. aave v4 spokes
v4_listed: dict[str, dict] = {}
for name, hub in AAVE_V4_HUBS.items():
    n = u256(multicall([(hub, sel("getAssetCount()"))])[0][1])
    if n is None:
        failures[f"aave-v4:getAssetCount:{name}"] = "eth_call failed"
        continue
    und = multicall([(hub, sel("getAssetUnderlyingAndDecimals(uint256)") + encode(["uint256"], [i])) for i in range(n)])
    cnt = multicall([(hub, sel("getSpokeCount(uint256)") + encode(["uint256"], [i])) for i in range(n)])
    calls, idx = [], []
    for i in range(n):
        ns = u256(cnt[i][1]) if cnt[i][0] else None
        if ns is None:
            failures[f"aave-v4:getSpokeCount:{name}:{i}"] = "eth_call failed"
            continue
        for j in range(ns):
            calls.append((hub, sel("getSpokeAddress(uint256,uint256)") + encode(["uint256", "uint256"], [i, j])))
            idx.append(i)
    for (ok, raw), i in zip(multicall(calls), idx):
        sp = lc(addr_word(raw)) if ok else None
        if not sp:
            failures[f"aave-v4:getSpokeAddress:{name}:{i}"] = "empty"
            continue
        e = v4_listed.setdefault(sp, {"hubs": [], "hub_assets": []})
        if hub not in e["hubs"]:
            e["hubs"].append(hub)
        a = lc(addr_word(und[i][1])) if und[i][0] else None
        if a and a not in e["hub_assets"]:
            e["hub_assets"].append(a)
notes.append(f"aave-v4: {len(AAVE_V4_HUBS)} hubs, {len(v4_listed)} spokes via getSpokeCount/getSpokeAddress")

res = multicall([(s, sel("asset()")) for s in v4_listed] + [(s, sel("totalAssets()")) for s in v4_listed])
spokes = list(v4_listed)
for i, s in enumerate(spokes):
    v4_listed[s]["asset"] = lc(addr_word(res[i][1])) if res[i][0] else None
    v4_listed[s]["total_assets"] = u256(res[len(spokes) + i][1]) if res[len(spokes) + i][0] else None
    if v4_listed[s]["asset"] is None:
        failures[f"aave-v4:spoke_asset:{s}"] = "asset() failed"

existing = {k.split(":", 1)[1] for k in C1["protocols"] if k.startswith("aave-v4:")}
added_spokes, added_hubs = [], []
for s, info in v4_listed.items():
    key = f"aave-v4:{s}"
    if key in C1["protocols"]:
        e = C1["protocols"][key]
        e["kind"] = "spoke"
        # keep C1's attribution when it is one of the on-chain listers; record them all
        e["hub"] = e["hub"] if lc(e.get("hub")) in info["hubs"] else info["hubs"][0]
        e["hubs"] = info["hubs"]
        e["hub_assets"] = info["hub_assets"]
        e["asset"] = info["asset"]
        e["total_assets"] = info["total_assets"]
        continue
    C1["protocols"][key] = {
        "family": "aave-v4",
        "market": s,
        "kind": "spoke",
        "hub": info["hubs"][0],
        "hubs": info["hubs"],
        "hub_assets": info["hub_assets"],
        "asset": info["asset"],
        "total_assets": info["total_assets"],
        "deployed_block": 0,
        "receipt_tokens": [],
        "oracle_adapters": [],
        "admitted": True,
    }
    added_spokes.append(s)
for name, hub in AAVE_V4_HUBS.items():
    key = f"aave-v4:{hub}"
    if key in C1["protocols"]:
        continue
    n = u256(multicall([(hub, sel("getAssetCount()"))])[0][1])
    C1["protocols"][key] = {
        "family": "aave-v4",
        "market": hub,
        "kind": "hub",
        "name": name,
        "hub": hub,
        "asset_count": n,
        "deployed_block": 0,
        "receipt_tokens": [],
        "oracle_adapters": [],
        "admitted": True,
    }
    added_hubs.append(hub)
stale_v4 = sorted(existing - set(v4_listed))
print(f"\naave-v4: on-chain spokes={len(v4_listed)} added_spokes={len(added_spokes)} "
      f"added_hubs={len(added_hubs)} c1_entries_not_listed_on_chain={len(stale_v4)}")
if stale_v4:
    print("   not listed on-chain (left in place, flagged):", stale_v4)
for s in stale_v4:
    failures[f"aave-v4:unlisted:{s}"] = "C1 entry not returned by getSpokeAddress at pinned block"

# ================================================================= 5. §4b identity
req = urllib.request.Request("https://tokens.uniswap.org", headers={"User-Agent": "Mozilla/5.0"})
with urllib.request.urlopen(req, timeout=60) as r:
    tl = json.load(r)
CANON = {t["symbol"]: t["address"].lower() for t in tl["tokens"] if t.get("chainId") == 1}
toks = sorted(C1["tokens"])
cand = [a for a in toks if C1["tokens"][a].get("symbol") in CANON and a != CANON[C1["tokens"][a]["symbol"]]]
res = multicall([(a, sel("symbol()")) for a in cand])
flagged = []
for a, (ok, raw) in zip(cand, res):
    s = None
    if ok and raw:
        try:
            s = decode(["string"], raw)[0]
        except Exception:
            s = raw.rstrip(b"\x00").decode("utf8", "replace").strip("\x00")
    if s in CANON and CANON[s] != a:
        C1["tokens"][a]["symbol_collision"] = {"claims": s, "canonical": CANON[s], "source": "tokens.uniswap.org"}
        q = C1["tokens"][a].setdefault("quirks", [])
        if "symbol_collision" not in q:
            q.append("symbol_collision")
        flagged.append(a)
print(f"\n§4b: flagged {len(flagged)} symbol-colliding tokens: {flagged}")

# propagate to markets that hold a flagged token as collateral or debt
touched = 0
for k, p in C1["protocols"].items():
    hits = sorted({t for t in (lc(p.get("collateral_token")), lc(p.get(DEBT_FIELD.get(p.get("family"), "")) or None))
                   if t in flagged})
    if hits:
        p["symbol_collision_tokens"] = hits
        touched += 1
print(f"§4b: {touched} market entries carry a symbol-colliding token")

# honest symbols: a token whose symbol() yields nothing must not carry a placeholder
ph = [a for a, t in C1["tokens"].items() if t.get("symbol") in ("?", "")]
res = multicall([(a, sel("symbol()")) for a in ph])
for a, (ok, raw) in zip(ph, res):
    s = None
    if ok and raw:
        try:
            s = decode(["string"], raw)[0]
        except Exception:
            s = raw.rstrip(b"\x00").decode("utf8", "replace").strip("\x00")
    if not s:
        C1["tokens"][a]["symbol"] = None
        failures[f"token:symbol:{a}"] = "symbol() returned no decodable value (recorded as null, not a placeholder)"
print(f"placeholder symbols cleared: {sum(1 for a in ph if C1['tokens'][a]['symbol'] is None)}/{len(ph)}")

# ================================================================= 6. meta
fam_counts: dict[str, Counter] = {}
for p in C1["protocols"].values():
    fam_counts.setdefault(p["family"], Counter())["instances"] += 1
    if p.get("admitted") and p["family"] in DEBT_FIELD:
        fam_counts[p["family"]]["admitted"] += 1
for fam, c in M1["counts_by_protocol"].items():
    n = sum(1 for p in C1["protocols"].values() if p.get("family") == fam)
    if fam == "aave-v4":
        c["instances"] = n
    if fam in DEBT_FIELD:
        c["admitted_markets"] = fam_counts.get(fam, Counter())["admitted"]
M1["admission"]["admitted_markets_total"] = sum(
    1 for p in C1["protocols"].values() if p.get("family") in DEBT_FIELD and p.get("admitted")
)
M1["admission"]["priced_at_block"] = BLOCK
M1["admission"]["debt_views"] = {
    "morpho-blue": "market() + IRM.borrowRateView accrual (expectedTotalBorrowAssets)",
    "euler-v2": "totalBorrows()",
    "silo-v2": "getDebtAssets()",
    "ajna": "debtInfo().debt_ (WAD) -> quote-token units",
}
M1["failures"].update(failures)
M1["notes"].extend(
    [
        f"WP C2 triage @ block {BLOCK}: aave-v4 re-enumerated via getSpokeCount/getSpokeAddress "
        f"(+{len(added_spokes)} spokes, +{len(added_hubs)} hubs; C1's log sweep from 25.5M found 22 of 53)",
        "WP C2 triage: morpho debt switched to accrued expectedTotalBorrowAssets, silo to getDebtAssets(); "
        f"Family B repriced at one block (admitted {sum(before.values())} -> {sum(after.values())})",
        f"WP C2 triage: §4b flagged {len(flagged)} symbol-colliding tokens on {touched} market entries",
    ]
)
M1["token_count"] = len(C1["tokens"])
M1["pool_count"] = len(C1["pools"])

print("\nfinal per-family admitted:", {f: fam_counts[f]["admitted"] for f in DEBT_FIELD if f in fam_counts})
print("protocol entries:", len(C1["protocols"]), "(was 3446)")

if "--write" in sys.argv:
    REG.write_text(json.dumps(C1, indent=1))
    META.write_text(json.dumps(M1, indent=2))
    print("\nWROTE registry.json + registry.meta.json")
else:
    print("\ndry run (pass --write to persist)")
