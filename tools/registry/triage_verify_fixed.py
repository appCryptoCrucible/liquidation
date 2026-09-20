"""Post-fix verification: re-read the fixed registry and re-check every claim on-chain,
independently of the fixer (fresh RPC reads, no reuse of its intermediate state)."""

from __future__ import annotations

import json
from pathlib import Path

from eth_abi import decode, encode
from web3 import Web3

ROOT = Path(__file__).resolve().parents[2]
R = json.loads((ROOT / "registry" / "registry.json").read_text())
M = json.loads((ROOT / "registry" / "registry.meta.json").read_text())
PRE = json.loads((Path(__file__).parent / "_pre_triage_registry.json").read_text())
BLOCK = R["generated_at_block"]
THRESH = 50_000.0
WAD = 10**18
w3 = Web3(Web3.HTTPProvider("https://gateway.tenderly.co/public/mainnet", request_kwargs={"timeout": 60}))
MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
MORPHO = "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb"
AAVE_V4_HUBS = [
    "0xcca852bc40e560adc3b1cc58ca5b55638ce826c9",
    "0x06002e9c4412cb7814a791ea3666d905871e536a",
    "0x943827dca022d0f354a8a8c332da1e5eb9f9f931",
    "0x62d63197660c080236193ca60b70e49a08e90368",
]
FAM_B = {"morpho-blue": "loan_token", "euler-v2": "asset", "silo-v2": "asset", "ajna": "quote_token"}


def cs(a):
    return Web3.to_checksum_address(a)


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


ok_all = True


def chk(label, cond, detail=""):
    global ok_all
    ok_all &= bool(cond)
    print(f"{'PASS' if cond else 'FAIL'}  {label} {detail}")


# 1. every admitted Family B entry has a real, on-threshold USD figure
adm = [(k, p) for k, p in R["protocols"].items() if p.get("family") in FAM_B and p.get("admitted")]
chk("no admitted Family B entry is unpriced or sub-threshold",
    all(p.get("borrowed_usd") is not None and p["borrowed_usd"] >= THRESH for _, p in adm),
    f"(n={len(adm)})")

# 2. no entry lost: pre-fix keys are all still present
chk("no protocol entry dropped", set(PRE["protocols"]) <= set(R["protocols"]),
    f"(pre={len(PRE['protocols'])} now={len(R['protocols'])})")
chk("no previously admitted Family B entry silently de-admitted",
    not [k for k, p in PRE["protocols"].items()
         if p.get("family") in FAM_B and p.get("admitted") and not R["protocols"][k].get("admitted")])

# 3. Morpho: recompute accrual from scratch and compare to the stored borrowed_raw
mk = [(k, p) for k, p in R["protocols"].items() if p.get("family") == "morpho-blue"]
ts = w3.eth.get_block(BLOCK)["timestamp"]
res = multicall([(MORPHO, sel("market(bytes32)") + encode(["bytes32"], [bytes.fromhex(p["market"][2:])])) for _, p in mk])
st = {}
for (k, p), (ok, raw) in zip(mk, res):
    if ok and len(raw) >= 192:
        st[k] = decode(["uint128"] * 6, raw)
calls, order = [], []
for k, p in mk:
    if k in st and p.get("irm") and int(p["irm"], 16):
        calls.append((p["irm"], sel(
            "borrowRateView((address,address,address,address,uint256),"
            "(uint128,uint128,uint128,uint128,uint128,uint128))") + encode(
            ["(address,address,address,address,uint256)", "(uint128,uint128,uint128,uint128,uint128,uint128)"],
            [(cs(p["loan_token"]), cs(p["collateral_token"]), cs(p["oracle"]), cs(p["irm"]), int(p["lltv"])), st[k]])))
        order.append(k)
rates = {k: int.from_bytes(raw[:32], "big") for k, (ok, raw) in zip(order, multicall(calls)) if ok and raw}
bad = []
for k, p in mk:
    if k not in st:
        continue
    ba, lu = st[k][2], st[k][4]
    if k in rates:
        x = rates[k] * max(0, ts - lu)
        ba = ba + (ba * (x + x * x // (2 * WAD) + x * x * x // (6 * WAD * WAD))) // WAD
    if p.get("borrowed_raw") != ba:
        bad.append((k, p.get("borrowed_raw"), ba))
chk("morpho borrowed_raw == independently recomputed accrued debt", not bad, f"(mismatch={len(bad)})")
for b in bad[:5]:
    print("     ", b)

# 4. Silo: stored borrowed_raw == getDebtAssets()
sk = [(k, p) for k, p in R["protocols"].items() if p.get("family") == "silo-v2"]
res = multicall([(p["market"], sel("getDebtAssets()")) for _, p in sk])
bad = [(k, p.get("borrowed_raw"), int.from_bytes(raw[:32], "big") if ok and raw else None)
       for (k, p), (ok, raw) in zip(sk, res)
       if p.get("borrowed_raw") != (int.from_bytes(raw[:32], "big") if ok and raw else None)]
chk("silo borrowed_raw == getDebtAssets()", not bad, f"(mismatch={len(bad)})")

# 5. Ajna: WAD debt converted with quote decimals, never treated as token units
ak = [(k, p) for k, p in R["protocols"].items() if p.get("family") == "ajna"]
res = multicall([(p["market"], sel("debtInfo()")) for _, p in ak])
bad = []
for (k, p), (ok, raw) in zip(ak, res):
    if not ok or len(raw) < 128:
        continue
    wad = int.from_bytes(raw[:32], "big")
    d = (R["tokens"].get(p["quote_token"]) or {}).get("decimals")
    if d is not None and p.get("borrowed_raw") != (wad * 10**d) // WAD:
        bad.append((k, p.get("borrowed_raw"), (wad * 10**d) // WAD))
chk("ajna borrowed_raw == debtInfo WAD scaled to quote decimals", not bad, f"(mismatch={len(bad)})")
chk("ajna admits 0 markets (max borrowed is dust)",
    not [p for _, p in ak if p.get("admitted")],
    f"(max usd={max((p.get('borrowed_usd') or 0) for _, p in ak):,.2f})")

# 6. Aave V4: registry set == on-chain listing
listed = set()
for hub in AAVE_V4_HUBS:
    n = int.from_bytes(w3.eth.call({"to": cs(hub), "data": sel("getAssetCount()")}, block_identifier=BLOCK)[:32], "big")
    cnt = multicall([(hub, sel("getSpokeCount(uint256)") + encode(["uint256"], [i])) for i in range(n)])
    calls = []
    for i in range(n):
        if cnt[i][0]:
            for j in range(int.from_bytes(cnt[i][1][:32], "big")):
                calls.append((hub, sel("getSpokeAddress(uint256,uint256)") + encode(["uint256", "uint256"], [i, j])))
    for ok, raw in multicall(calls):
        if ok and len(raw) >= 32 and int(raw[12:32].hex(), 16):
            listed.add("0x" + raw[12:32].hex())
reg_spokes = {p["market"] for p in R["protocols"].values() if p.get("family") == "aave-v4" and p.get("kind") == "spoke"}
reg_hubs = {p["market"] for p in R["protocols"].values() if p.get("family") == "aave-v4" and p.get("kind") == "hub"}
chk("aave-v4 spoke set == getSpokeAddress listing", reg_spokes == listed,
    f"(reg={len(reg_spokes)} chain={len(listed)} only_reg={len(reg_spokes-listed)} only_chain={len(listed-reg_spokes)})")
chk("aave-v4 all 4 hubs present", reg_hubs == set(AAVE_V4_HUBS), f"({len(reg_hubs)})")
chk("every aave-v4 entry typed hub|spoke",
    all(p.get("kind") in ("hub", "spoke") for p in R["protocols"].values() if p.get("family") == "aave-v4"))

# 7. identity flags
flagged = {a for a, t in R["tokens"].items() if t.get("symbol_collision")}
chk("fake 'Tether USD' flagged", "0x3d4762b4bb4b4c922377fe5b887e900d7fb64cdf" in flagged)
chk("Hastra 'PRIME' flagged", "0x19ebb35279a16207ec4ba82799cc64715065f7f6" in flagged)
chk("canonical USDC/USDT/DAI/WETH NOT flagged",
    not (flagged & {"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", "0xdac17f958d2ee523a2206206994597c13d831ec7",
                    "0x6b175474e89094c44da98b954eedeac495271d0f",
                    "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"}))
marked = [k for k, p in R["protocols"].items() if p.get("symbol_collision_tokens")]
chk("markets holding a colliding token carry the flag", len(marked) >= 25, f"({len(marked)})")
chk("no admitted market has a colliding DEBT token",
    not [k for k, p in adm if p.get(FAM_B[p["family"]]) in flagged])
chk("no placeholder '?' symbols remain", not [a for a, t in R["tokens"].items() if t.get("symbol") == "?"])

# 8. meta consistency
from collections import Counter

c = Counter(p["family"] for p in R["protocols"].values() if p.get("family") in FAM_B and p.get("admitted"))
chk("meta admitted_markets_total matches entries",
    M["admission"]["admitted_markets_total"] == sum(c.values()), f"({M['admission']['admitted_markets_total']} vs {sum(c.values())})")
for fam in FAM_B:
    chk(f"meta {fam} admitted_markets", M["counts_by_protocol"][fam]["admitted_markets"] == c[fam],
        f"({M['counts_by_protocol'][fam]['admitted_markets']} vs {c[fam]})")
chk("meta aave-v4 instances matches entries",
    M["counts_by_protocol"]["aave-v4"]["instances"]
    == sum(1 for p in R["protocols"].values() if p.get("family") == "aave-v4"))

print(f"\nadmitted Family B by family: {dict(c)}  total={sum(c.values())}")
print(f"protocol entries: {len(R['protocols'])}  tokens: {len(R['tokens'])}  pools: {len(R['pools'])}")
print("\nOVERALL:", "PASS" if ok_all else "FAIL")
