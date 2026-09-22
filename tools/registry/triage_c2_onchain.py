"""WP C2 triage step 2: verify every C1-vs-C2 discrepancy against on-chain state.

Read-only. Pins to C1's block (26015175) on an archive endpoint; cross-checks at head
on ethereum.publicnode.com. Never substitutes a guessed price: an unpriceable token is
reported UNPRICED, not assumed.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

from eth_abi import decode, encode
from web3 import Web3

ROOT = Path(__file__).resolve().parents[2]
SETS = json.loads((Path(__file__).parent / "_triage_sets.json").read_text())
C1 = json.loads((ROOT / "registry" / "registry.json").read_text())

RPC_HEAD = "https://ethereum.publicnode.com"
RPC_ARCHIVE = "https://gateway.tenderly.co/public/mainnet"
C1_BLOCK = C1["generated_at_block"]

MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
MORPHO = "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb"
FEED_REGISTRY = "0x47Fb2585D2C56Fe188D0E6ec628a38b74fCeeeDf"
USD_DENOM = "0x0000000000000000000000000000000000000348"
ETH_DENOM = "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE"
AAVE_V4_HUBS = {
    "core": "0xCca852Bc40e560adC3b1Cc58CA5b55638ce826c9",
    "plus": "0x06002e9c4412CB7814a791eA3666D905871E536A",
    "prime": "0x943827DCA022D0F354a8a8c332dA1e5Eb9f9F931",
    "paxos": "0x62d63197660C080236193CA60b70E49A08E90368",
}
AAVE_V4_LOGS_FROM = 25_500_000
TOPIC_SPOKE_SET = "0xb233dd05ed21346e144167b35a6213bcf04768dbdffdc8339e8b027b94b9f305"

w_head = Web3(Web3.HTTPProvider(RPC_HEAD, request_kwargs={"timeout": 40}))
w_arch = Web3(Web3.HTTPProvider(RPC_ARCHIVE, request_kwargs={"timeout": 60}))


def cs(a: str) -> str:
    return Web3.to_checksum_address(a)


def sel(sig: str) -> bytes:
    return Web3.keccak(text=sig)[:4]


def addr_word(w: bytes) -> str | None:
    if len(w) < 32:
        return None
    a = "0x" + w[12:32].hex()
    return None if int(a, 16) == 0 else a


def multicall(w3: Web3, calls: list[tuple[str, bytes]], block: int | str) -> list[tuple[bool, bytes]]:
    """Multicall3.tryAggregate(false, calls) — one revert does not kill the batch."""
    out: list[tuple[bool, bytes]] = []
    CH = 400
    for i in range(0, len(calls), CH):
        part = calls[i : i + CH]
        data = sel("tryAggregate(bool,(address,bytes)[])") + encode(
            ["bool", "(address,bytes)[]"], [False, [(cs(a), d) for a, d in part]]
        )
        raw = w3.eth.call({"to": cs(MULTICALL3), "data": data}, block_identifier=block)
        out.extend(decode(["(bool,bytes)[]"], raw)[0])
    return out


def call1(w3: Web3, to: str, data: bytes, block: int | str) -> bytes | None:
    try:
        return w3.eth.call({"to": cs(to), "data": data}, block_identifier=block)
    except Exception:
        return None


# ---------------------------------------------------------------- prices
DISPUTED_TOKENS: dict[str, str] = {}
for fam in ("morpho-blue", "euler-v2", "silo-v2", "ajna"):
    for r in SETS.get(f"only_c1_admitted:{fam}", []) + SETS.get(f"only_c2_admitted:{fam}", []):
        if r.get("debt_token") or r.get("asset"):
            DISPUTED_TOKENS[(r.get("debt_token") or r.get("asset"))] = r.get("symbol") or "?"

AAVE_ORACLES = [
    p["price_oracle"]
    for p in C1["protocols"].values()
    if p.get("family") in ("aave-v3", "spark") and p.get("price_oracle")
]
AAVE_ORACLES = list(dict.fromkeys(AAVE_ORACLES))


def price_report(block: int | str, w3: Web3) -> dict[str, dict]:
    res: dict[str, dict] = {}
    toks = sorted(DISPUTED_TOKENS)
    calls: list[tuple[str, bytes]] = []
    for a in toks:
        args = encode(["address", "address"], [cs(a), cs(USD_DENOM)])
        calls.append((FEED_REGISTRY, sel("latestRoundData(address,address)") + args))
        calls.append((FEED_REGISTRY, sel("decimals(address,address)") + args))
    fr = multicall(w3, calls, block)
    for i, a in enumerate(toks):
        ok, raw = fr[2 * i]
        okd, draw = fr[2 * i + 1]
        px = None
        if ok and len(raw) >= 160 and okd and len(draw) >= 32:
            ans = int.from_bytes(raw[32:64], "big", signed=True)
            if ans > 0:
                px = ans / 10 ** int.from_bytes(draw[:32], "big")
        res[a] = {"symbol": DISPUTED_TOKENS[a], "feed_registry_usd": px, "aave_oracle_usd": {}}
    for oracle in AAVE_ORACLES:
        u = call1(w3, oracle, sel("BASE_CURRENCY_UNIT()"), block)
        unit = int.from_bytes(u[:32], "big") if u else 0
        if unit <= 0:
            continue
        calls = [
            (oracle, sel("getAssetPrice(address)") + encode(["address"], [cs(a)])) for a in toks
        ]
        rr = multicall(w3, calls, block)
        for a, (ok, raw) in zip(toks, rr):
            if ok and len(raw) >= 32:
                ans = int.from_bytes(raw[:32], "big")
                if ans > 0:
                    res[a]["aave_oracle_usd"][oracle] = ans / unit
    return res


# ---------------------------------------------------------------- morpho
def morpho_check(block: int | str, w3: Web3) -> list[dict]:
    rows = SETS["only_c1_admitted:morpho-blue"]
    ids = [bytes.fromhex(r["id"][2:]) for r in rows]
    calls: list[tuple[str, bytes]] = []
    for i in ids:
        calls.append((MORPHO, sel("market(bytes32)") + encode(["bytes32"], [i])))
        calls.append((MORPHO, sel("idToMarketParams(bytes32)") + encode(["bytes32"], [i])))
    res = multicall(w3, calls, block)
    out = []
    for n, r in enumerate(rows):
        ok, raw = res[2 * n]
        okp, rawp = res[2 * n + 1]
        tba = int.from_bytes(raw[64:96], "big") if ok and len(raw) >= 192 else None
        loan = addr_word(rawp[:32]) if okp and len(rawp) >= 160 else None
        out.append({**r, "onchain_total_borrow_assets": tba, "onchain_loan_token": loan})
    return out


# ---------------------------------------------------------------- euler
def euler_check(block: int | str, w3: Web3) -> list[dict]:
    rows = SETS["only_c1_admitted:euler-v2"]
    calls: list[tuple[str, bytes]] = []
    for r in rows:
        calls.append((r["id"], sel("totalBorrows()")))
        calls.append((r["id"], sel("asset()")))
    res = multicall(w3, calls, block)
    out = []
    for n, r in enumerate(rows):
        ok, raw = res[2 * n]
        oka, rawa = res[2 * n + 1]
        out.append(
            {
                **r,
                "onchain_total_borrows": int.from_bytes(raw[:32], "big") if ok and raw else None,
                "onchain_asset": addr_word(rawa[:32]) if oka and rawa else None,
            }
        )
    return out


# ---------------------------------------------------------------- ajna
def ajna_check(block: int | str, w3: Web3) -> list[dict]:
    rows = SETS["only_c2_admitted:ajna"]
    calls: list[tuple[str, bytes]] = []
    for r in rows:
        calls.append((r["id"], sel("debtInfo()")))
        calls.append((r["id"], sel("quoteTokenAddress()")))
        calls.append((r["id"], sel("quoteTokenScale()")))
    res = multicall(w3, calls, block)
    out = []
    for n, r in enumerate(rows):
        ok, raw = res[3 * n]
        okq, rawq = res[3 * n + 1]
        oks, raws = res[3 * n + 2]
        out.append(
            {
                **r,
                "onchain_debt_wad": int.from_bytes(raw[:32], "big") if ok and len(raw) >= 128 else None,
                "onchain_quote": addr_word(rawq[:32]) if okq and rawq else None,
                "onchain_quote_scale": int.from_bytes(raws[:32], "big") if oks and raws else None,
            }
        )
    return out


# ---------------------------------------------------------------- silo
def silo_check(block: int | str, w3: Web3) -> list[dict]:
    rows = SETS["only_c1_admitted:silo-v2"]
    calls: list[tuple[str, bytes]] = []
    for r in rows:
        calls.append((r["silo"], sel("getCollateralAndDebtTotalsStorage()")))
        calls.append((r["silo"], sel("getDebtAssets()")))
        calls.append((r["silo"], sel("asset()")))
        calls.append((r["silo"], sel("totalAssets()")))
    res = multicall(w3, calls, block)
    out = []
    for n, r in enumerate(rows):
        ok, raw = res[4 * n]
        okd, rawd = res[4 * n + 1]
        oka, rawa = res[4 * n + 2]
        okt, rawt = res[4 * n + 3]
        out.append(
            {
                **r,
                "onchain_totals_debt": int.from_bytes(raw[32:64], "big") if ok and len(raw) >= 64 else None,
                "onchain_getDebtAssets": int.from_bytes(rawd[:32], "big") if okd and rawd else None,
                "onchain_asset": addr_word(rawa[:32]) if oka and rawa else None,
                "onchain_totalAssets": int.from_bytes(rawt[:32], "big") if okt and rawt else None,
            }
        )
    return out


# ---------------------------------------------------------------- aave v4 spokes
def aave_v4_check(block: int | str, w3: Web3) -> dict:
    listed: dict[str, list[str]] = {}
    for name, hub in AAVE_V4_HUBS.items():
        cret = call1(w3, hub, sel("getAssetCount()"), block)
        if not cret:
            listed[name] = []
            continue
        n = int.from_bytes(cret[:32], "big")
        for i in range(n):
            sret = call1(w3, hub, sel("getSpokeCount(uint256)") + encode(["uint256"], [i]), block)
            if not sret:
                continue
            ns = int.from_bytes(sret[:32], "big")
            calls = [
                (hub, sel("getSpokeAddress(uint256,uint256)") + encode(["uint256", "uint256"], [i, j]))
                for j in range(ns)
            ]
            for ok, raw in multicall(w3, calls, block) if calls else []:
                sp = addr_word(raw[:32]) if ok and raw else None
                if sp:
                    listed.setdefault(name, []).append(sp.lower())
    return listed


def spoke_liveness(addrs: list[str], block: int | str, w3: Web3) -> list[dict]:
    """A spoke is a real market iff it has code and answers its own views with non-empty state."""
    calls: list[tuple[str, bytes]] = []
    for a in addrs:
        calls.append((a, sel("HUB()")))
        calls.append((a, sel("asset()")))
        calls.append((a, sel("totalSupply()")))
        calls.append((a, sel("totalAssets()")))
    res = multicall(w3, calls, block)
    out = []
    for n, a in enumerate(addrs):
        okh, rawh = res[4 * n]
        oka, rawa = res[4 * n + 1]
        oks, raws = res[4 * n + 2]
        okt, rawt = res[4 * n + 3]
        code = len(w3.eth.get_code(cs(a), block_identifier=block))
        out.append(
            {
                "addr": a,
                "code_len": code,
                "hub": addr_word(rawh[:32]) if okh and rawh else None,
                "asset": addr_word(rawa[:32]) if oka and rawa else None,
                "total_supply": int.from_bytes(raws[:32], "big") if oks and raws else None,
                "total_assets": int.from_bytes(rawt[:32], "big") if okt and rawt else None,
            }
        )
    return out


# ---------------------------------------------------------------- collisions
def collision_check(block: int | str, w3: Web3) -> list[dict]:
    keys = [k for k in SETS if k.startswith("collision:")]
    out = []
    for k in keys:
        a = k.split(":", 1)[1]
        info = SETS[k]
        raw_s = call1(w3, a, sel("symbol()"), block)
        raw_n = call1(w3, a, sel("name()"), block)
        raw_d = call1(w3, a, sel("decimals()"), block)
        raw_t = call1(w3, a, sel("totalSupply()"), block)

        def txt(b: bytes | None) -> str | None:
            if not b:
                return None
            try:
                return decode(["string"], b)[0]
            except Exception:
                return b.rstrip(b"\x00").decode("utf8", "replace")

        out.append(
            {
                "addr": a,
                "claimed_symbol": info["symbol_claimed"],
                "onchain_symbol": txt(raw_s),
                "onchain_name": txt(raw_n),
                "onchain_decimals": int.from_bytes(raw_d[:32], "big") if raw_d else None,
                "total_supply": int.from_bytes(raw_t[:32], "big") if raw_t else None,
                "c1_refs": info["refs"],
            }
        )
    return out


if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    block = int(sys.argv[2]) if len(sys.argv) > 2 else C1_BLOCK
    w3 = w_arch if block != "latest" else w_head
    print(f"# block={block} rpc={'archive' if w3 is w_arch else 'publicnode'}")
    res: dict[str, object] = {"block": block}
    if which in ("all", "price"):
        res["prices"] = price_report(block, w3)
    if which in ("all", "morpho"):
        res["morpho"] = morpho_check(block, w3)
    if which in ("all", "euler"):
        res["euler"] = euler_check(block, w3)
    if which in ("all", "ajna"):
        res["ajna"] = ajna_check(block, w3)
    if which in ("all", "silo"):
        res["silo"] = silo_check(block, w3)
    if which in ("all", "v4"):
        listed = aave_v4_check(block, w3)
        allsp = sorted({a for v in listed.values() for a in v})
        c1 = {e for e in {k.split(":")[1] for k in C1["protocols"] if k.startswith("aave-v4:")}}
        res["aave_v4"] = {
            "listed_per_hub": {k: len(v) for k, v in listed.items()},
            "unique_spokes": len(allsp),
            "c1_entries": len(c1),
            "only_onchain": sorted(set(allsp) - c1),
            "only_c1": sorted(c1 - set(allsp)),
            "liveness": spoke_liveness(sorted(set(allsp) - c1), block, w3),
        }
    if which in ("all", "collisions"):
        res["collisions"] = collision_check(block, w3)
    p = Path(__file__).parent / f"_triage_onchain_{which}_{block}.json"
    p.write_text(json.dumps(res, indent=1, default=str))
    print("wrote", p.name)
    print(json.dumps(res, indent=1, default=str)[:12000])
