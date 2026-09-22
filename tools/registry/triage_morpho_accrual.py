"""Morpho market() is pre-accrual. Recompute totalBorrowAssets with accrued interest
(IRM.borrowRateView + Morpho MathLib wTaylorCompounded) and re-test the $50k bar.

Determines whether C1's 161 admitted Morpho markets is the exact on-chain answer.
"""

from __future__ import annotations

import json
from pathlib import Path

from eth_abi import decode, encode
from web3 import Web3

ROOT = Path(__file__).resolve().parents[2]
C1 = json.loads((ROOT / "registry" / "registry.json").read_text())
PRICES = json.loads((Path(__file__).parent / "_triage_price_source.json").read_text())["prices"]
BLOCK = C1["generated_at_block"]
THRESH = 50_000.0
WAD = 10**18
w3 = Web3(Web3.HTTPProvider("https://gateway.tenderly.co/public/mainnet", request_kwargs={"timeout": 60}))
MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
MORPHO = "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb"


def cs(a):
    return Web3.to_checksum_address(a)


def sel(s):
    return Web3.keccak(text=s)[:4]


def multicall(calls, block=BLOCK):
    out = []
    for i in range(0, len(calls), 250):
        part = calls[i : i + 250]
        data = sel("tryAggregate(bool,(address,bytes)[])") + encode(
            ["bool", "(address,bytes)[]"], [False, [(cs(a), d) for a, d in part]]
        )
        raw = w3.eth.call({"to": cs(MULTICALL3), "data": data}, block_identifier=block)
        out.extend(decode(["(bool,bytes)[]"], raw)[0])
    return out


def w_taylor_compounded(rate: int, t: int) -> int:
    """Morpho MathLib.wTaylorCompounded — 3rd-order Taylor of e^(rate*t)-1, WAD."""
    x = rate * t
    return x + (x * x) // (2 * WAD) + (x * x * x) // (6 * WAD * WAD)


ts = w3.eth.get_block(BLOCK)["timestamp"]
mk = [(k, p) for k, p in C1["protocols"].items() if p.get("family") == "morpho-blue"]
print(f"morpho markets: {len(mk)}  block ts={ts}")

# state + params
calls = []
for k, p in mk:
    i = bytes.fromhex(p["market"][2:])
    calls.append((MORPHO, sel("market(bytes32)") + encode(["bytes32"], [i])))
res = multicall(calls)
state = {}
for (k, p), (ok, raw) in zip(mk, res):
    if ok and len(raw) >= 192:
        sa, ss, ba, bs, lu, fee = decode(["uint128"] * 6, raw)
        state[k] = (sa, ss, ba, bs, lu, fee)

# borrowRateView per market (IRM from C1's stored params, verified against chain above)
calls = []
order = []
for k, p in mk:
    if k not in state or not p.get("irm") or int(p["irm"], 16) == 0:
        continue
    sa, ss, ba, bs, lu, fee = state[k]
    mp = (cs(p["loan_token"]), cs(p["collateral_token"]), cs(p["oracle"]), cs(p["irm"]), int(p["lltv"]))
    data = sel("borrowRateView((address,address,address,address,uint256),(uint128,uint128,uint128,uint128,uint128,uint128))") + encode(
        ["(address,address,address,address,uint256)", "(uint128,uint128,uint128,uint128,uint128,uint128)"],
        [mp, (sa, ss, ba, bs, lu, fee)],
    )
    calls.append((p["irm"], data))
    order.append(k)
res = multicall(calls)
rates = {}
fails = 0
for k, (ok, raw) in zip(order, res):
    if ok and len(raw) >= 32:
        rates[k] = int.from_bytes(raw[:32], "big")
    else:
        fails += 1
print(f"borrowRateView ok={len(rates)} failed={fails}")

flip_up, flip_down, drift = [], [], []
adm_stale = adm_accr = 0
for k, p in mk:
    if k not in state:
        continue
    sa, ss, ba, bs, lu, fee = state[k]
    tok = p.get("loan_token")
    t = C1["tokens"].get(tok) or {}
    d = t.get("decimals")
    px = PRICES.get(tok)
    if d is None or px is None:
        continue
    rate = rates.get(k)
    elapsed = max(0, ts - lu)
    accrued = ba + (ba * w_taylor_compounded(rate, elapsed)) // WAD if rate else None
    su = ba / 10**d * px[0]
    au = (accrued / 10**d * px[0]) if accrued is not None else None
    a_s = su >= THRESH
    adm_stale += a_s
    if au is None:
        continue
    a_a = au >= THRESH
    adm_accr += a_a
    if a_s != a_a:
        (flip_up if a_a else flip_down).append((k, t.get("symbol"), su, au, elapsed / 86400))
    if su > 0 and au / max(su, 1e-9) > 1.02:
        drift.append((k, t.get("symbol"), su, au, elapsed / 86400))

print(f"\nadmitted, stale market() view : {adm_stale}   (C1 registry says 161)")
print(f"admitted, accrued view        : {adm_accr}")
print(f"\nmarkets that CROSS the bar once interest is accrued: {len(flip_up)}")
for f in sorted(flip_up, key=lambda x: -x[3]):
    print(f"   {f[0][:58]} {f[1]:8} stale={f[2]:12,.0f} accrued={f[3]:12,.0f} stale_for={f[4]:.1f}d")
print(f"markets that fall below once accrued: {len(flip_down)}")
print(f"\nmarkets whose debt is understated by >2%: {len(drift)}")
for f in sorted(drift, key=lambda x: -(x[3] - x[2]))[:12]:
    print(f"   {f[0][:58]} {f[1]:8} stale={f[2]:14,.0f} accrued={f[3]:14,.0f} (+{100*(f[3]/f[2]-1):.1f}%, {f[4]:.1f}d stale)")
