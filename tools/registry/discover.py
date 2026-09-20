#!/usr/bin/env python3
"""
Registry discovery (REGISTRY.md §3–§3c, §5) — Ethereum mainnet only (D01).

Enumerates Family A/B protocols from on-chain roots and events; derives per-address
fields; applies interim Family B admission (D27 unset). Writes registry/registry.json
and registry/registry.meta.json.

RPC: eth_call → publicnode; eth_getLogs → Tenderly public (publicnode blocks logs).
"""
from __future__ import annotations

import argparse
import json
import sys
import time
from collections import defaultdict
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

from eth_abi import decode, encode
from web3 import Web3

# --- roots (protocol singletons — confirm at H1; not market hand-lists) ---
AAVE_V3_REGISTRY = "0xbaA999AC55EAce41CcAE355c77809e68Bb345170"
SPARK_REGISTRY = "0x03cFa0C4622FF84E50E75062683F44c9587e6Cc1"
MORPHO_BLUE = "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb"
ILK_REGISTRY = "0x5a464C28D19848f44199D003BeF5ecc87d090F87"
EULER_FACTORY = "0x29a56a1b8214D9Cf7c5561811750D5cBDb45CC8e"
SILO_FACTORY_V2 = "0x22a3cF6149bFa611bAFc89Fd721918EC3Cf7b581"
SILO_FACTORY_V3 = "0x1DAb4A310447185144467076b116DAC7aec3b48F"
AJNA_ERC20_FACTORY = "0x6146DD43C5622bB6D12A5240ab9CF4de14eDC625"
AJNA_ERC721_FACTORY = "0x27461199d3b7381De66a85D685828E967E35AF4c"
UNIV3_FACTORY = "0x1F98431c8aD98523631AE4a59f267346ea31F984"
MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
CHAINLINK_FEED_REGISTRY = "0x47Fb2585D2C56Fe188D0E6ec628a38b74fCeeeDf"
USD_DENOM = "0x0000000000000000000000000000000000000348"
ZERO = "0x0000000000000000000000000000000000000000"

# Hub candidates verified on-chain via getAssetCount() before use (REGISTRY §3 Aave V4).
AAVE_V4_HUB_CANDIDATES = [
    "0xCca852Bc40e560adC3b1Cc58CA5b55638ce826c9",
    "0x06002e9c4412CB7814a791eA3666D905871E536A",
    "0x943827DCA022D0F354a8a8c332dA1e5Eb9f9F931",
    "0x62d63197660C080236193CA60b70E49A08E90368",
]

HUB_ASSETS = [
    "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",  # WETH
    "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",  # USDC
    "0xdAC17F958D2ee523a2206206994597C13D831ec7",  # USDT
    "0x6B175474E89094C44Da98b954EedeAC495271d0F",  # DAI
    "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599",  # WBTC
]
UNIV3_FEES = [100, 500, 3000, 10000]

DISCOVERY_STEP_NAMES = [
    "aave-v3",
    "spark",
    "aave-v4",
    "compound-v2",
    "sky",
    "morpho",
    "euler",
    "silo",
    "ajna",
    "univ3",
]
# step name → family key used in protocols[*].family / counts / oracle source prefix
STEP_FAMILY = {
    "aave-v3": "aave-v3",
    "spark": "spark",
    "aave-v4": "aave-v4",
    "compound-v2": "compound-v2",
    "sky": "sky-maker",
    "morpho": "morpho-blue",
    "euler": "euler-v2",
    "silo": "silo-v2",
    "ajna": "ajna",
    "univ3": "univ3",
}
# Feed Registry denominations: WETH has no entry; ETH denomination (0xEeee…) is the feed.
FEED_REGISTRY_BASE_ALIAS = {
    "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2": "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE",
}
WAD = 10**18

# D27 interim (REGISTRY §3b): generous phase-1 bar until D11/D27 set.
INTERIM_ADMISSION_USD = 50_000.0
INTERIM_ADMISSION_NOTE = (
    "INTERIM (D27 unset): admit Family B markets with total borrowed >= "
    f"${INTERIM_ADMISSION_USD:,.0f} USD notional at Chainlink Feed Registry price; "
    "replace when D11/D27 are set."
)

MORPHO_DEPLOY_BLOCK = 18_883_124
UNIV3_FACTORY_DEPLOY = 12_369_621
UNIV3_SWEEP_BLOCKS = 3_000_000  # PoolCreated window behind head; full archive is C3
COMPOUND_V2_FROM_BLOCK = 7_710_000
AAVE_V4_LOGS_FROM = 25_500_000

# Event topic0 (0x-prefixed for RPC)
def topic(sig: str) -> str:
    h = Web3.to_hex(Web3.keccak(text=sig))
    return h if h.startswith("0x") else "0x" + h


TOPIC_MARKET_LISTED = topic("MarketListed(address)")
TOPIC_CREATE_MARKET = topic("CreateMarket(bytes32,(address,address,address,address,uint256))")
TOPIC_POOL_CREATED = topic("PoolCreated(address,address,uint24,int24,address)")
TOPIC_AAVE_V4_SPOKE_SET = "0xb233dd05ed21346e144167b35a6213bcf04768dbdffdc8339e8b027b94b9f305"


def sel(sig: str) -> bytes:
    return Web3.keccak(text=sig)[:4]


def addr_word(word: bytes) -> Optional[str]:
    if len(word) < 32:
        return None
    a = "0x" + word[12:32].hex()
    if int(a, 16) == 0:
        return None
    return Web3.to_checksum_address(a)


@dataclass
class ProtocolCounts:
    instances: int = 0
    reserves: int = 0
    receipt_tokens: int = 0
    aggregators: int = 0
    admitted_markets: int = 0


@dataclass
class Builder:
    w3: Web3
    w3_logs: Web3
    sleep_s: float = 0.06
    batch: int = 120
    head: int = 0
    failures: dict[str, str] = field(default_factory=dict)
    notes: list[str] = field(default_factory=list)
    tokens: dict[str, dict[str, Any]] = field(default_factory=dict)
    protocols: dict[str, dict[str, Any]] = field(default_factory=dict)
    oracles: dict[str, dict[str, Any]] = field(default_factory=dict)
    pools: dict[str, dict[str, Any]] = field(default_factory=dict)
    flash_sources: dict[str, dict[str, Any]] = field(default_factory=dict)
    routers: dict[str, dict[str, Any]] = field(default_factory=dict)
    counts: dict[str, ProtocolCounts] = field(default_factory=lambda: defaultdict(ProtocolCounts))
    tracked_assets: set[str] = field(default_factory=set)
    done_steps: set[str] = field(default_factory=set)
    _code: dict[str, bool] = field(default_factory=dict)
    _usd_cache: dict[str, Optional[float]] = field(default_factory=dict)
    _usd_source: dict[str, str] = field(default_factory=dict)
    _unit_cache: dict[str, int] = field(default_factory=dict)
    _checkpoint_dir: Optional[Path] = None

    def cs(self, a: str) -> str:
        return Web3.to_checksum_address(a)

    def al(self, a: str) -> str:
        return self.cs(a).lower()

    def has_code(self, a: str) -> bool:
        k = a.lower()
        if k not in self._code:
            try:
                time.sleep(self.sleep_s)
                self._code[k] = len(self.w3.eth.get_code(self.cs(a))) > 2
            except Exception:
                self._code[k] = False
        return self._code[k]

    @staticmethod
    def _is_rate_limited(exc: BaseException) -> bool:
        msg = str(exc).lower()
        return any(
            k in msg
            for k in (
                "429",
                "rate limit",
                "rate-limit",
                "too many request",
                "timeout",
                "timed out",
                "connection",
                "503",
                "502",
                "504",
            )
        )

    def multicall(self, calls: list[tuple[str, bytes]]) -> list[tuple[bool, bytes]]:
        out: list[tuple[bool, bytes]] = []
        for i in range(0, len(calls), self.batch):
            chunk = calls[i : i + self.batch]
            data = sel("tryAggregate(bool,(address,bytes)[])") + encode(
                ["bool", "(address,bytes)[]"],
                [False, [(self.cs(t), d) for t, d in chunk]],
            )
            for attempt in range(3):
                try:
                    time.sleep(self.sleep_s)
                    raw = self.w3.eth.call({"to": MULTICALL3, "data": data})
                    res = decode(["(bool,bytes)[]"], raw)[0]
                    out.extend([(bool(s), bytes(r)) for s, r in res])
                    break
                except Exception as ex:
                    if attempt >= 2:
                        self.failures[f"multicall:{i}"] = str(ex)[:200]
                        out.extend([(False, b"")] * len(chunk))
                    else:
                        delay = 1.5 * (2**attempt)
                        if self._is_rate_limited(ex):
                            delay *= 2
                        time.sleep(delay)
        return out

    def call1(self, to: str, sig: str, args: tuple = (), types: tuple = ()) -> Optional[bytes]:
        data = sel(sig) + (encode(list(types), list(args)) if types else b"")
        ok, ret = self.multicall([(to, data)])[0]
        return ret if ok and ret else None

    def _classify_call_failure(self, address: str, sig: str, rate_limited: bool) -> str:
        if rate_limited:
            return "rate_limited"
        if not self.has_code(address):
            return "no_contract_code"
        return "no_such_function"

    def _record_token_decimals_failure(self, a: str, reason: str):
        self.failures[f"token:decimals:{a}"] = reason

    def _record_oracle_aggregator_failure(self, p: str, reason: str):
        self.failures[f"oracle:aggregator:{p}"] = reason

    def get_logs(
        self,
        address: str | list[str],
        topics: list,
        from_block: int,
        to_block: int,
        chunk: int = 50_000,
    ) -> list:
        logs: list = []
        start = from_block
        errs = 0
        addr = self.cs(address) if isinstance(address, str) else [self.cs(a) for a in address]
        while start <= to_block:
            end = min(start + chunk - 1, to_block)
            try:
                time.sleep(self.sleep_s)
                logs.extend(
                    self.w3_logs.eth.get_logs(
                        {"address": addr, "topics": topics, "fromBlock": start, "toBlock": end}
                    )
                )
                start = end + 1
                errs = 0
            except Exception as ex:
                msg = str(ex).lower()
                if any(k in msg for k in ("range", "limit", "too many", "exceed", "size")) and chunk > 2_000:
                    chunk //= 2
                    continue
                errs += 1
                if errs < 6:
                    time.sleep(1.5 * errs)
                    continue
                self.failures[f"getLogs:{start}"] = str(ex)[:180]
                start = end + 1
                errs = 0
        return logs

    def _price_oracles(self) -> list[str]:
        """Aave-family price oracles discovered via getPriceOracle() (aave-v3 first, then spark)."""
        return [
            p["price_oracle"]
            for fam in ("aave-v3", "spark")
            for p in self.protocols.values()
            if p.get("family") == fam and p.get("price_oracle")
        ]

    def usd_price(self, token: str) -> Optional[float]:
        """USD price from chain: Chainlink Feed Registry (registry is whitelisted on
        access-controlled aggregators, so query via latestRoundData(base,quote) — a direct
        aggregator.latestRoundData() through Multicall3 reverts 'No access'), then the
        discovered Aave/Spark AaveOracle.getAssetPrice (BASE_CURRENCY_UNIT scaled).
        None = no on-chain USD source; caller logs it."""
        k = self.al(token)
        if k in self._usd_cache:
            return self._usd_cache[k]
        base = self.cs(FEED_REGISTRY_BASE_ALIAS.get(k, k))
        args = encode(["address", "address"], [base, self.cs(USD_DENOM)])
        (ok, ret), (ok_d, ret_d) = self.multicall(
            [
                (CHAINLINK_FEED_REGISTRY, sel("latestRoundData(address,address)") + args),
                (CHAINLINK_FEED_REGISTRY, sel("decimals(address,address)") + args),
            ]
        )
        px: Optional[float] = None
        if ok and len(ret) >= 160 and ok_d and len(ret_d) >= 32:
            answer = int.from_bytes(ret[32:64], "big", signed=True)
            if answer > 0:
                px = answer / (10 ** int.from_bytes(ret_d[:32], "big"))
                self._usd_source[k] = "chainlink-feed-registry"
        if px is None:
            t = self.cs(token)
            for oracle in self._price_oracles():
                if oracle not in self._unit_cache:
                    u = self.call1(oracle, "BASE_CURRENCY_UNIT()")
                    self._unit_cache[oracle] = int.from_bytes(u[:32], "big") if u else 0
                unit = self._unit_cache[oracle]
                if unit <= 0:
                    continue
                r = self.call1(oracle, "getAssetPrice(address)", (t,), ("address",))
                if r and len(r) >= 32:
                    answer = int.from_bytes(r[:32], "big")
                    if answer > 0:
                        px = answer / unit
                        self._usd_source[k] = f"aave-oracle:{oracle}"
                        break
        self._usd_cache[k] = px
        return px

    def ensure_tokens_bulk(self, addresses: list[str]):
        """Batch decimals()/symbol() via Multicall3 (one round-trip per chunk)."""
        pending = [self.al(a) for a in addresses if self.al(a) not in self.tokens and self.al(a) != ZERO]
        if not pending:
            return
        for a in pending:
            self.tracked_assets.add(a)
        dec_sel = sel("decimals()")
        sym_sel = sel("symbol()")
        calls: list[tuple[str, bytes]] = []
        order: list[tuple[str, str]] = []
        for a in pending:
            calls.append((a, dec_sel))
            calls.append((a, sym_sel))
            order.append((a, "dec"))
            order.append((a, "sym"))
        res = self.multicall(calls)
        by_addr: dict[str, dict[str, Any]] = {}
        for (a, kind), (ok, ret) in zip(order, res):
            st = by_addr.setdefault(a, {"dec_ok": False, "dec_r": None, "sym_ok": False, "sym_r": None})
            if kind == "dec":
                st["dec_ok"] = ok and bool(ret)
                st["dec_r"] = ret if ok else None
            else:
                st["sym_ok"] = ok and bool(ret)
                st["sym_r"] = ret if ok else None
        for a, st in by_addr.items():
            dec_r = st["dec_r"]
            if not st["dec_ok"] or not dec_r:
                reason = self._classify_call_failure(a, "decimals()", False)
                self._record_token_decimals_failure(a, f"decimals() failed ({reason})")
                continue
            decimals = int.from_bytes(dec_r[:32], "big")
            sym_r = st["sym_r"]
            symbol = "?"
            quirks: list[str] = []
            if st["sym_ok"] and sym_r:
                try:
                    if len(sym_r) == 32:
                        symbol = decode(["string"], sym_r)[0]
                    elif len(sym_r) >= 64:
                        symbol = decode(["string"], sym_r)[0]
                    else:
                        symbol = decode(["bytes32"], sym_r)[0].rstrip(b"\x00").decode("utf-8", errors="replace")
                except Exception:
                    try:
                        symbol = decode(["bytes32"], sym_r[:32])[0].rstrip(b"\x00").decode("utf-8", errors="replace")
                        quirks.append("nonstandard_metadata")
                    except Exception as ex:
                        self.failures[f"token:symbol:{a}"] = str(ex)[:120]
            if decimals <= 8 and decimals != 18:
                quirks.append("low_decimals")
            self._detect_quirks(a, quirks)
            self.tokens[a] = {"symbol": symbol, "decimals": decimals, "quirks": sorted(set(quirks))}

    def ensure_token(self, address: str):
        a = self.al(address)
        if a in self.tokens or a == ZERO:
            return
        self.ensure_tokens_bulk([a])

    def _detect_quirks(self, a: str, quirks: list[str]):
        known = {
            "0xdac17f958d2ee523a2206206994597c13d831ec7": ["no_return_data", "approve_nonzero_reverts"],
            "0xbb4cdb9cbd36b01bd1cbaebf2de08d9173bc095c": ["no_return_data"],
            "0xba100000625a3754423978a0c95f58a9819d9ac": ["no_return_data"],
            "0xae7ab96520de3a18e5e111b5eaab095312d7fe84": ["rebasing"],
            "0x9f8f72aa9304c8b593d555f12ef6589cc3a579a2": ["nonstandard_metadata"],
        }
        for q in known.get(a, []):
            if q not in quirks:
                quirks.append(q)

    def add_oracle_proxies_bulk(self, items: list[tuple[str, str, str]]):
        """Batch aggregator() on proxy addresses. items: (proxy, pair, source)."""
        pending = [(self.al(p), pair, source) for p, pair, source in items if self.al(p) not in self.oracles]
        if not pending:
            return
        agg_sel = sel("aggregator()")
        calls = [(p, agg_sel) for p, _, _ in pending]
        res = self.multicall(calls)
        for (p, pair, source), (ok, agg_r) in zip(pending, res):
            if not ok or not agg_r:
                reason = self._classify_call_failure(p, "aggregator()", False)
                self._record_oracle_aggregator_failure(p, f"aggregator() failed ({reason})")
                continue
            agg = addr_word(agg_r[:32])
            if not agg:
                self._record_oracle_aggregator_failure(p, "aggregator() failed (empty_return)")
                continue
            dec_r = self.call1(agg, "decimals()")
            decimals = int.from_bytes(dec_r[:32], "big") if dec_r else 8
            self.oracles[p] = {
                "aggregator": self.al(agg),
                "pair": pair,
                "decimals": decimals,
                "svr": False,
                "source": source,
            }
            fam = source.split(":")[0] if source else "unknown"
            self.counts[fam].aggregators += 1

    def add_oracle_proxy(self, proxy: str, pair: str = "", source: str = ""):
        p = self.al(proxy)
        if p in self.oracles:
            return
        self.add_oracle_proxies_bulk([(proxy, pair, source)])

    # --- Family A ---
    def discover_aave_v3_spark(self, family: str, registry: str):
        pc = self.counts[family]
        providers_r = self.call1(registry, "getAddressesProvidersList()")
        if not providers_r:
            self.failures[family] = "getAddressesProvidersList failed"
            return
        providers = decode(["address[]"], providers_r)[0]
        pc.instances = len(providers)
        for prov in providers:
            pool = addr_word(self.call1(prov, "getPool()") or b"")
            oracle = addr_word(self.call1(prov, "getPriceOracle()") or b"")
            if not pool:
                continue
            key = f"{family}:{self.al(pool)}"
            reserves_r = self.call1(pool, "getReservesList()")
            if not reserves_r:
                continue
            reserves = decode(["address[]"], reserves_r)[0]
            pc.reserves += len(reserves)
            receipts: list[str] = []
            adapters: list[str] = []
            calls = []
            for asset in reserves:
                # getReserveData(asset) → ReserveData/ReserveDataLegacy (15 words on every V3
                # release): word 8 = aToken, word 10 = variableDebtToken. The per-token getters
                # (getReserveAToken…) only exist on V3.2+, and revert on Spark.
                calls.append((pool, sel("getReserveData(address)") + encode(["address"], [asset])))
                if oracle:
                    calls.append(
                        (oracle, sel("getSourceOfAsset(address)") + encode(["address"], [asset]))
                    )
            res = self.multicall(calls)
            idx = 0
            for asset in reserves:
                self.ensure_token(asset)
                r_ok, r_ret = res[idx]
                idx += 1
                if r_ok and len(r_ret) >= 32 * 11:
                    at = addr_word(r_ret[32 * 8 : 32 * 9])
                    vt = addr_word(r_ret[32 * 10 : 32 * 11])
                    if at:
                        receipts.append(self.al(at))
                        pc.receipt_tokens += 1
                    if vt:
                        receipts.append(self.al(vt))
                        pc.receipt_tokens += 1
                else:
                    self.failures[f"{family}:getReserveData:{self.al(asset)}"] = "getReserveData failed"
                if oracle:
                    o_ok, o_ret = res[idx]
                    idx += 1
                    if o_ok and o_ret:
                        src = addr_word(o_ret[:32])
                        if src:
                            adapters.append(self.al(src))
                            self.add_oracle_proxy(src, pair=f"{family}/{self.al(asset)[:10]}", source=f"{family}:getSourceOfAsset")
            self.protocols[key] = {
                "family": family,
                "market": self.al(pool),
                "addresses_provider": self.al(prov),
                "price_oracle": self.al(oracle) if oracle else None,
                "deployed_block": 0,
                "receipt_tokens": sorted(set(receipts)),
                "oracle_adapters": sorted(set(adapters)),
                "reserve_count": len(reserves),
                "admitted": True,
            }

    def discover_aave_v4(self):
        family = "aave-v4"
        pc = self.counts[family]
        hubs: list[str] = []
        for cand in AAVE_V4_HUB_CANDIDATES:
            r = self.call1(cand, "getAssetCount()")
            if r:
                hubs.append(self.al(cand))
        pc.instances = len(hubs)
        spokes: dict[str, str] = {}
        for hub in hubs:
            logs = self.get_logs(hub, [TOPIC_AAVE_V4_SPOKE_SET], AAVE_V4_LOGS_FROM, self.head)
            for lg in logs:
                if len(lg["topics"]) >= 3:
                    t = lg["topics"][2].hex() if hasattr(lg["topics"][2], "hex") else lg["topics"][2]
                    if not str(t).startswith("0x"):
                        t = "0x" + t
                    sp = self.cs("0x" + t[-40:])
                    if self.has_code(sp):
                        spokes[self.al(sp)] = hub
        pc.reserves = 0
        for sp in sorted(spokes):
            key = f"aave-v4:{sp}"
            self.protocols[key] = {
                "family": family,
                "market": sp,
                "hub": spokes[sp],
                "deployed_block": 0,
                "receipt_tokens": [],
                "oracle_adapters": [],
                "admitted": True,
            }
        # spokes count as instances alongside hubs
        pc.instances = len(hubs) + len(spokes)
        self.notes.append(
            f"aave-v4: {len(hubs)} hubs (getAssetCount verified), {len(spokes)} spokes from hub logs"
        )

    def discover_compound_v2(self):
        family = "compound-v2"
        pc = self.counts[family]
        comptrollers: set[str] = set()
        start = COMPOUND_V2_FROM_BLOCK
        chunk = 2_000_000
        while start <= self.head:
            end = min(start + chunk - 1, self.head)
            try:
                time.sleep(self.sleep_s)
                batch = self.w3_logs.eth.get_logs(
                    {"topics": [TOPIC_MARKET_LISTED], "fromBlock": start, "toBlock": end}
                )
                for lg in batch:
                    comptrollers.add(self.al(lg["address"]))
                if start % (chunk * 5) == 0:
                    print(f"  compound-v2 logs through {end}, comptrollers={len(comptrollers)}", flush=True)
                start = end + 1
            except Exception as ex:
                if chunk > 5_000:
                    chunk //= 2
                    continue
                self.failures[f"compound-v2:logs:{start}"] = str(ex)[:160]
                start = end + 1
        verified: dict[str, list] = {}
        for c in sorted(comptrollers):
            mk = self.call1(c, "getAllMarkets()")
            if mk:
                verified[c] = decode(["address[]"], mk)[0]
        pc.instances = len(verified)
        for c, markets in verified.items():
            pc.reserves += len(markets)
            receipts = []
            for m in markets:
                receipts.append(self.al(m))
                pc.receipt_tokens += 1
            self.protocols[f"compound-v2:{c}"] = {
                "family": family,
                "market": c,
                "comptroller": c,
                "deployed_block": 0,
                "receipt_tokens": receipts,
                "oracle_adapters": [],
                "ctoken_count": len(markets),
                "admitted": True,
            }

    def discover_sky(self):
        family = "sky-maker"
        pc = self.counts[family]
        n_r = self.call1(ILK_REGISTRY, "count()")
        if not n_r:
            self.failures[family] = "IlkRegistry.count failed"
            return
        ilks_r = self.call1(ILK_REGISTRY, "list()")
        if not ilks_r:
            self.failures[family] = "IlkRegistry.list failed"
            return
        ilks = decode(["bytes32[]"], ilks_r)[0]
        pc.instances = 1
        pc.reserves = len(ilks)
        calls = [(ILK_REGISTRY, sel("info(bytes32)") + encode(["bytes32"], [ilk])) for ilk in ilks]
        res = self.multicall(calls)
        # info(ilk) → (string name, string symbol, uint class, uint dec, address gem,
        # address pip, address join, address xlip); ABI-decode, never slice head words
        # (the two dynamic strings put gem at word 4, not 5).
        INFO_TYPES = ["string", "string", "uint256", "uint256", "address", "address", "address", "address"]
        gems: list[str] = []
        pips: list[str] = []
        joins: list[str] = []
        for ilk, (ok, ret) in zip(ilks, res):
            if not ok or not ret:
                self.failures[f"{family}:info:{ilk.hex()}"] = "IlkRegistry.info failed"
                continue
            _name, _sym, _cls, _dec, gem, pip, join, _xlip = decode(INFO_TYPES, ret)
            if int(gem, 16):
                gems.append(self.al(gem))
                self.ensure_token(gem)
            if int(pip, 16):
                pips.append(self.al(pip))
                self.add_oracle_proxy(pip, pair=f"ilk/{ilk.hex()[:16]}", source=f"{family}:pip")
            if int(join, 16):
                joins.append(self.al(join))
                pc.receipt_tokens += 1
        self.protocols[f"sky-maker:ilk-registry"] = {
            "family": family,
            "market": self.al(ILK_REGISTRY),
            "deployed_block": 0,
            "receipt_tokens": sorted(set(joins)),
            "oracle_adapters": sorted(set(pips)),
            "ilk_count": len(ilks),
            "gems": sorted(set(gems)),
            "admitted": True,
        }

    # --- Family B ---
    def _borrowed_usd(self, amount_raw: int, token: str) -> Optional[float]:
        """None = cannot price (logged under admission:unpriced); market stays un-admitted."""
        self.ensure_token(token)
        k = self.al(token)
        t = self.tokens.get(k)
        if not t:
            self.failures[f"admission:unpriced:{k}"] = "token decimals unknown (see token:decimals)"
            return None
        px = self.usd_price(token)
        if px is None:
            self.failures[f"admission:unpriced:{k}"] = (
                f"no USD source ({t['symbol']}): feed registry + aave/spark oracles"
            )
            return None
        return (amount_raw / (10 ** t["decimals"])) * px

    def discover_morpho(self):
        family = "morpho-blue"
        pc = self.counts[family]
        logs = self.get_logs(MORPHO_BLUE, [TOPIC_CREATE_MARKET], MORPHO_DEPLOY_BLOCK, self.head)
        market_ids: list[bytes] = []
        params: list[tuple] = []
        for lg in logs:
            mid = lg["topics"][1]
            market_ids.append(bytes(mid))
            loan, coll, oracle, irm, lltv = decode(
                ["(address,address,address,address,uint256)"], bytes(lg["data"])
            )[0]
            params.append((loan, coll, oracle, irm, lltv))
        pc.instances = 1
        admitted = 0
        mcalls = [
            (MORPHO_BLUE, sel("market(bytes32)") + encode(["bytes32"], [mid])) for mid in market_ids
        ]
        mres = self.multicall(mcalls)
        for mid, (loan, coll, oracle, irm, lltv), (ok_b, bor_r) in zip(market_ids, params, mres):
            self.ensure_token(loan)
            self.ensure_token(coll)
            mkey = "0x" + mid.hex()
            borrowed_raw = 0
            if ok_b and bor_r and len(bor_r) >= 96:
                borrowed_raw = int.from_bytes(bor_r[64:96], "big")
            borrowed_usd = self._borrowed_usd(borrowed_raw, loan) if borrowed_raw else 0.0
            admit = borrowed_usd is not None and borrowed_usd >= INTERIM_ADMISSION_USD
            if admit:
                admitted += 1
                if oracle and self.has_code(oracle):
                    self.add_oracle_proxy(oracle, pair="morpho-market", source=f"{family}:oracle")
            key = f"morpho-blue:{mkey}"
            self.protocols[key] = {
                "family": family,
                "market": mkey,
                "loan_token": self.al(loan),
                "collateral_token": self.al(coll),
                "oracle": self.al(oracle) if oracle else None,
                "irm": self.al(irm) if irm else None,
                "lltv": int(lltv),
                "borrowed_raw": borrowed_raw,
                "borrowed_usd": borrowed_usd,
                "deployed_block": 0,
                "receipt_tokens": [],
                "oracle_adapters": [self.al(oracle)] if oracle else [],
                "admitted": admit,
            }
        # fix deployed_block per market from logs
        for lg, key_i in zip(logs, range(len(market_ids))):
            mkey = "0x" + bytes(lg["topics"][1]).hex()
            k = f"morpho-blue:{mkey}"
            if k in self.protocols:
                self.protocols[k]["deployed_block"] = lg["blockNumber"]
        pc.admitted_markets = admitted
        pc.reserves = len(market_ids)
        self.notes.append(f"morpho-blue: {len(market_ids)} markets, {admitted} admitted (interim USD bar)")

    def discover_euler(self):
        family = "euler-v2"
        pc = self.counts[family]
        n_r = self.call1(EULER_FACTORY, "getProxyListLength()")
        if not n_r:
            self.failures[family] = "getProxyListLength failed"
            return
        n = int.from_bytes(n_r[:32], "big")
        pc.instances = 1
        vaults: list[str] = []
        for start in range(0, n, 100):
            end = min(start + 100, n)
            r = self.call1(EULER_FACTORY, "getProxyListSlice(uint256,uint256)", (start, end), ("uint256", "uint256"))
            if r:
                vaults.extend(self.al(v) for v in decode(["address[]"], r)[0])
        admitted = 0
        for v in vaults:
            asset_r = self.call1(v, "asset()")
            if not asset_r:
                continue
            asset = addr_word(asset_r[:32])
            if not asset:
                continue
            self.ensure_token(asset)
            bor_r = self.call1(v, "totalBorrows()")
            borrowed_raw = int.from_bytes(bor_r[:32], "big") if bor_r else 0
            borrowed_usd = self._borrowed_usd(borrowed_raw, asset) if borrowed_raw else 0.0
            admit = borrowed_usd is not None and borrowed_usd >= INTERIM_ADMISSION_USD
            if admit:
                admitted += 1
            self.protocols[f"euler-v2:{self.al(v)}"] = {
                "family": family,
                "market": self.al(v),
                "asset": self.al(asset),
                "borrowed_raw": borrowed_raw,
                "borrowed_usd": borrowed_usd,
                "deployed_block": 0,
                "receipt_tokens": [],
                "oracle_adapters": [],
                "admitted": admit,
            }
        pc.reserves = len(vaults)
        pc.admitted_markets = admitted

    def _enum_silo_factory(self, factory: str, label: str) -> list[str]:
        configs: list[str] = []
        n_r = self.call1(factory, "getNextSiloId()")
        if not n_r:
            return configs
        n = int.from_bytes(n_r[:32], "big")
        id_sel = sel("idToSiloConfig(uint256)")
        for start in range(1, n, 200):
            end = min(start + 200, n)
            calls = [
                (factory, id_sel + encode(["uint256"], [i])) for i in range(start, end)
            ]
            for ok, ret in self.multicall(calls):
                if ok and ret:
                    cfg = addr_word(ret[:32])
                    if cfg:
                        configs.append(self.al(cfg))
        return list(dict.fromkeys(configs))

    def discover_silo(self):
        family = "silo-v2"
        pc = self.counts[family]
        configs = self._enum_silo_factory(SILO_FACTORY_V2, "v2") + self._enum_silo_factory(
            SILO_FACTORY_V3, "v3"
        )
        configs = list(dict.fromkeys(configs))
        pc.instances = 2
        admitted = 0
        markets = 0
        for cfg in configs:
            silo_r = self.call1(cfg, "getSilos()")
            if not silo_r or len(silo_r) < 64:
                continue
            s0 = addr_word(silo_r[:32])
            s1 = addr_word(silo_r[32:64])
            for silo in (s0, s1):
                if not silo:
                    continue
                markets += 1
                asset_r = self.call1(silo, "asset()")
                asset = addr_word(asset_r[:32]) if asset_r else None
                borrowed_raw = 0
                if asset:
                    self.ensure_token(asset)
                    # ISilo.getCollateralAndDebtTotalsStorage() → (totalCollateralAssets, totalDebtAssets)
                    tot = self.call1(silo, "getCollateralAndDebtTotalsStorage()")
                    if tot and len(tot) >= 64:
                        borrowed_raw = int.from_bytes(tot[32:64], "big")
                    else:
                        self.failures[f"{family}:totals:{self.al(silo)}"] = (
                            "getCollateralAndDebtTotalsStorage() failed"
                        )
                else:
                    self.failures[f"{family}:asset:{self.al(silo)}"] = "asset() failed"
                borrowed_usd = (
                    self._borrowed_usd(borrowed_raw, asset) if asset and borrowed_raw else 0.0
                )
                admit = borrowed_usd is not None and borrowed_usd >= INTERIM_ADMISSION_USD
                if admit:
                    admitted += 1
                self.protocols[f"silo-v2:{self.al(silo)}"] = {
                    "family": family,
                    "market": self.al(silo),
                    "silo_config": cfg,
                    "asset": self.al(asset) if asset else None,
                    "borrowed_raw": borrowed_raw,
                    "borrowed_usd": borrowed_usd,
                    "deployed_block": 0,
                    "receipt_tokens": [],
                    "oracle_adapters": [],
                    "admitted": admit,
                }
        pc.reserves = markets
        pc.admitted_markets = admitted

    def discover_ajna(self):
        family = "ajna"
        pc = self.counts[family]
        admitted = 0
        total = 0
        for fac, kind in ((AJNA_ERC20_FACTORY, "erc20"), (AJNA_ERC721_FACTORY, "erc721")):
            r = self.call1(fac, "getDeployedPoolsList()")
            if not r:
                self.failures[f"ajna:{kind}"] = "getDeployedPoolsList failed"
                continue
            pools = decode(["address[]"], r)[0]
            total += len(pools)
            for p in pools:
                # Ajna pool: quote/collateral via pool contract
                quote_r = self.call1(p, "quoteTokenAddress()")
                coll_r = self.call1(p, "collateralAddress()")
                quote = addr_word(quote_r[:32]) if quote_r else None
                coll = addr_word(coll_r[:32]) if coll_r else None
                if quote:
                    self.ensure_token(quote)
                if coll:
                    self.ensure_token(coll)
                # IPoolState.debtInfo() → (debt_, accruedDebt_, debtInAuction_, t0Debt2ToCollateral_);
                # Ajna normalises all quote amounts to WAD (18 dec) regardless of quote decimals.
                debt_r = self.call1(p, "debtInfo()")
                debt_wad = 0
                if debt_r and len(debt_r) >= 128:
                    debt_wad = int.from_bytes(debt_r[:32], "big")
                else:
                    self.failures[f"{family}:debtInfo:{self.al(p)}"] = "debtInfo() failed"
                qtok = self.tokens.get(self.al(quote)) if quote else None
                borrowed_raw = (debt_wad * 10 ** qtok["decimals"]) // WAD if qtok else 0
                borrowed_usd = (
                    self._borrowed_usd(borrowed_raw, quote) if quote and debt_wad else 0.0
                )
                admit = borrowed_usd is not None and borrowed_usd >= INTERIM_ADMISSION_USD
                if admit:
                    admitted += 1
                self.protocols[f"ajna:{self.al(p)}"] = {
                    "family": family,
                    "market": self.al(p),
                    "pool_kind": kind,
                    "quote_token": self.al(quote) if quote else None,
                    "collateral_token": self.al(coll) if coll else None,
                    "debt_wad": debt_wad,
                    "borrowed_raw": borrowed_raw,
                    "borrowed_usd": borrowed_usd,
                    "deployed_block": 0,
                    "receipt_tokens": [],
                    "oracle_adapters": [],
                    "admitted": admit,
                }
        pc.instances = 2
        pc.reserves = total
        pc.admitted_markets = admitted

    def _univ3_pool_count(self) -> int:
        return sum(1 for p in self.pools.values() if p.get("venue") == "univ3")

    def _set_univ3_counts(self):
        pc = self.counts["univ3"]
        n = self._univ3_pool_count()
        pc.instances = n
        pc.reserves = 0
        pc.receipt_tokens = 0
        pc.aggregators = 0
        pc.admitted_markets = 0

    def discover_univ3_pools(self):
        """Tracked assets × hub pairs; derive pool via factory.getPool (§3c)."""
        pc = self.counts["univ3"]
        existing = self._univ3_pool_count()
        if existing > 0 and "univ3" not in self.done_steps:
            self._set_univ3_counts()
            self.notes.append(
                f"univ3: counts-only refresh ({existing} pools from checkpoint, scan skipped)"
            )
            return
        assets = sorted(self.tracked_assets | {a.lower() for a in HUB_ASSETS})
        hub_l = [a.lower() for a in HUB_ASSETS]
        pairs: list[tuple[str, str, int]] = []
        for a in assets:
            if a in hub_l:
                continue
            for h in hub_l:
                if a == h:
                    continue
                for fee in UNIV3_FEES:
                    pairs.append((a, h, fee))
                    pairs.append((h, a, fee))
        # dedupe token order as on-chain: factory sorts token0 < token1
        seen_triple: set[tuple[str, str, int]] = set()
        calls: list[tuple[str, bytes]] = []
        meta: list[tuple[str, str, int]] = []
        get_pool_sel = sel("getPool(address,address,uint24)")
        for t0, t1, fee in pairs:
            a0, a1 = (t0, t1) if int(t0, 16) < int(t1, 16) else (t1, t0)
            key = (a0, a1, fee)
            if key in seen_triple:
                continue
            seen_triple.add(key)
            calls.append(
                (
                    UNIV3_FACTORY,
                    get_pool_sel
                    + encode(
                        ["address", "address", "uint24"],
                        [self.cs(a0), self.cs(a1), fee],
                    ),
                )
            )
            meta.append((a0, a1, fee))
        res = self.multicall(calls)
        factory = self.al(UNIV3_FACTORY)
        for (a0, a1, fee), (ok, ret) in zip(meta, res):
            if not ok or len(ret) < 32:
                continue
            pool = addr_word(ret[:32])
            if not pool:
                continue
            t0_r = self.call1(pool, "token0()")
            t1_r = self.call1(pool, "token1()")
            f_r = self.call1(pool, "fee()")
            if not t0_r or not t1_r or not f_r:
                continue
            on_t0 = self.al(addr_word(t0_r[:32]) or "")
            on_t1 = self.al(addr_word(t1_r[:32]) or "")
            on_fee = int.from_bytes(f_r[:32], "big")
            if on_t0 != a0 or on_t1 != a1:
                self.failures[f"univ3:order:{pool}"] = f"factory triple mismatch ({a0},{a1}) vs ({on_t0},{on_t1})"
            self.pools[self.al(pool)] = {
                "venue": "univ3",
                "token0": on_t0,
                "token1": on_t1,
                "fee": on_fee,
                "factory": factory,
                "deployed_block": 0,
                "derived_via": "factory.getPool",
            }
            self.ensure_token(on_t0)
            self.ensure_token(on_t1)
        # PoolCreated sweep over the last 3M blocks (~14 months); full-archive sweep from
        # UNIV3_FACTORY_DEPLOY is C3-scale volume. Keep only pools touching a *protocol*
        # tracked asset. The membership test runs against a frozen snapshot: ensure_token()
        # below grows tracked_assets, and testing the live set lets each new pool's other
        # token admit the next pool (observed: 30k pools / 27k tokens of noise).
        sweep_from = max(UNIV3_FACTORY_DEPLOY, self.head - UNIV3_SWEEP_BLOCKS)
        tracked = frozenset(self.tracked_assets)
        logs = self.get_logs(
            UNIV3_FACTORY, [TOPIC_POOL_CREATED], sweep_from, self.head, chunk=80_000
        )
        self.notes.append(
            f"univ3: PoolCreated sweep blocks {sweep_from}-{self.head}, logs={len(logs)}"
        )
        for lg in logs:
            if len(lg["topics"]) < 4:
                continue
            t0 = self.al("0x" + lg["topics"][1].hex()[-40:])
            t1 = self.al("0x" + lg["topics"][2].hex()[-40:])
            if t0 not in tracked or t1 not in tracked:
                continue
            fee = int.from_bytes(lg["topics"][3][-4:], "big")
            pool = addr_word(bytes(lg["data"])[-32:])
            if not pool or self.al(pool) in self.pools:
                continue
            a0, a1 = (t0, t1) if int(t0, 16) < int(t1, 16) else (t1, t0)
            derived = self.call1(
                UNIV3_FACTORY,
                "getPool(address,address,uint24)",
                (self.cs(a0), self.cs(a1), fee),
                ("address", "address", "uint24"),
            )
            dp = addr_word(derived[:32]) if derived else None
            if not dp or self.al(dp) != self.al(pool):
                self.failures[f"univ3:derive:{pool}"] = "factory.getPool mismatch vs PoolCreated"
                continue
            self.pools[self.al(pool)] = {
                "venue": "univ3",
                "token0": a0,
                "token1": a1,
                "fee": fee,
                "factory": factory,
                "deployed_block": lg["blockNumber"],
                "derived_via": "factory.getPool",
            }
            self.ensure_token(a0)
            self.ensure_token(a1)
        self._set_univ3_counts()

    def _referenced_tokens(self) -> set[str]:
        """Every token address a protocol entry or pool refers to (what decimals must exist for)."""
        refs: set[str] = set()
        for p in self.protocols.values():
            for f in ("loan_token", "collateral_token", "asset", "quote_token"):
                if p.get(f):
                    refs.add(p[f])
            refs.update(p.get("gems") or [])
        for pool in self.pools.values():
            refs.add(pool["token0"])
            refs.add(pool["token1"])
        refs.discard(ZERO)  # Morpho idle markets carry collateral_token = address(0)
        return refs

    def _referenced_oracles(self) -> set[str]:
        refs: set[str] = set()
        for p in self.protocols.values():
            refs.update(p.get("oracle_adapters") or [])
        return refs

    def retry_failed_enrichment(self):
        """Re-batch decimals/aggregator for prior failures (rate-limit recovery). Failures for
        addresses no longer referenced by any entry (stale after --rerun) are dropped, not retried."""
        token_addrs: list[str] = []
        oracle_addrs: list[str] = []
        drop: list[str] = []
        tok_refs = self.tracked_assets | self._referenced_tokens()
        ora_refs = self._referenced_oracles()
        stale = 0
        for k in self.failures:
            if k.startswith("token:decimals:"):
                a = k.split(":", 2)[2]
                drop.append(k)
                if a in tok_refs:
                    token_addrs.append(a)
                else:
                    stale += 1
            elif k.startswith("oracle:aggregator:"):
                a = k.split(":", 2)[2]
                drop.append(k)
                if a in ora_refs:
                    oracle_addrs.append(a)
                else:
                    stale += 1
            elif k.startswith("admission:unpriced:") and k.split(":", 2)[2] not in tok_refs:
                drop.append(k)  # token no longer referenced by any market (post --rerun)
                stale += 1
        if not drop:
            return
        for k in drop:
            self.failures.pop(k, None)
        self.notes.append(
            f"retry_enrichment: {len(token_addrs)} token:decimals, {len(oracle_addrs)} oracle:aggregator, "
            f"{stale} stale (unreferenced) dropped"
        )
        if token_addrs:
            self.ensure_tokens_bulk(token_addrs)
        if oracle_addrs:
            self.add_oracle_proxies_bulk(
                [(a, "", "enrichment-retry:aggregator") for a in oracle_addrs]
            )

    def _counts_to_dict(self) -> dict[str, dict[str, int]]:
        return {
            fam: {
                "instances": c.instances,
                "reserves": c.reserves,
                "receipt_tokens": c.receipt_tokens,
                "aggregators": c.aggregators,
                "admitted_markets": c.admitted_markets,
            }
            for fam, c in self.counts.items()
        }

    def _counts_from_dict(self, per: dict[str, dict[str, int]]):
        self.counts.clear()
        for fam, d in per.items():
            self.counts[fam] = ProtocolCounts(
                instances=int(d.get("instances", 0)),
                reserves=int(d.get("reserves", 0)),
                receipt_tokens=int(d.get("receipt_tokens", 0)),
                aggregators=int(d.get("aggregators", 0)),
                admitted_markets=int(d.get("admitted_markets", 0)),
            )

    def save_checkpoint(self, out_dir: Path):
        if not out_dir:
            return
        out_dir.mkdir(parents=True, exist_ok=True)
        reg = self.to_registry()
        partial = {
            "done_steps": sorted(self.done_steps),
            "registry": reg,
            "counts_by_protocol": self._counts_to_dict(),
            "failures": self.failures,
            "notes": self.notes,
            "tracked_assets": sorted(self.tracked_assets),
            "_code": {k: v for k, v in self._code.items()},
        }
        p_reg = out_dir / "registry.partial.json"
        p_meta = out_dir / "registry.partial.meta.json"
        p_reg.write_text(json.dumps(partial, indent=2) + "\n", encoding="utf-8")
        p_meta.write_text(
            json.dumps(
                {
                    "done_steps": sorted(self.done_steps),
                    "generated_at_block": self.head,
                    "token_count": len(self.tokens),
                    "pool_count": len(self.pools),
                    "failure_count": len(self.failures),
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )

    def load_checkpoint(self, out_dir: Path) -> bool:
        p_reg = out_dir / "registry.partial.json"
        if not p_reg.is_file():
            return False
        data = json.loads(p_reg.read_text(encoding="utf-8"))
        reg = data.get("registry") or {}
        self.head = int(reg.get("generated_at_block") or 0)
        self.tokens = reg.get("tokens") or {}
        self.protocols = reg.get("protocols") or {}
        self.oracles = reg.get("oracles") or {}
        self.pools = reg.get("pools") or {}
        self.flash_sources = reg.get("flash_sources") or {}
        self.routers = reg.get("routers") or {}
        self.failures = dict(data.get("failures") or {})
        self.notes = list(data.get("notes") or [])
        self.tracked_assets = set(data.get("tracked_assets") or self.tokens.keys())
        self._code = dict(data.get("_code") or {})
        self.done_steps = set(data.get("done_steps") or [])
        self._counts_from_dict(data.get("counts_by_protocol") or {})
        print(
            f"[checkpoint] loaded partial: steps_done={sorted(self.done_steps)} "
            f"tokens={len(self.tokens)} pools={len(self.pools)} failures={len(self.failures)}",
            flush=True,
        )
        return True

    def bootstrap_from_final_registry(self, out_dir: Path) -> bool:
        """Seed builder from completed registry.json (skip re-discovery of finished steps)."""
        reg_path = out_dir / "registry.json"
        meta_path = out_dir / "registry.meta.json"
        if not reg_path.is_file() or not meta_path.is_file():
            return False
        reg = json.loads(reg_path.read_text(encoding="utf-8"))
        meta = json.loads(meta_path.read_text(encoding="utf-8"))
        self.head = int(reg.get("generated_at_block") or meta.get("generated_at_block") or 0)
        self.tokens = reg.get("tokens") or {}
        self.protocols = reg.get("protocols") or {}
        self.oracles = reg.get("oracles") or {}
        self.pools = reg.get("pools") or {}
        self.flash_sources = reg.get("flash_sources") or {}
        self.routers = reg.get("routers") or {}
        self.failures = dict(meta.get("failures") or {})
        self.notes = list(meta.get("notes") or [])
        self.tracked_assets = set(self.tokens.keys()) | self._referenced_tokens()
        self._counts_from_dict(meta.get("counts_by_protocol") or {})
        skip = [s for s in DISCOVERY_STEP_NAMES if s != "univ3"]
        self.done_steps = set(skip)
        self.notes.append(
            "bootstrap: loaded registry.json + meta; skipping completed protocol steps except univ3"
        )
        print(
            f"[bootstrap] from final registry: will skip {skip}, run univ3 + enrichment retry",
            flush=True,
        )
        return True

    def reset_steps(self, steps: list[str]):
        """Forget a step's output so run() re-discovers it (--rerun). Drops that family's
        protocol entries, counts, family-sourced oracles and family-keyed failures; univ3
        drops the pools. Enrichment failures are re-scoped in retry_failed_enrichment."""
        for step in steps:
            fam = STEP_FAMILY[step]
            self.done_steps.discard(step)
            self.counts.pop(fam, None)
            if step == "univ3":
                self.pools = {k: v for k, v in self.pools.items() if v.get("venue") != "univ3"}
                # tokens known only through dropped pools must not seed the next getPool scan
                keep = self._referenced_tokens() | {a.lower() for a in HUB_ASSETS}
                self.tokens = {k: v for k, v in self.tokens.items() if k in keep}
            else:
                self.protocols = {k: v for k, v in self.protocols.items() if v.get("family") != fam}
            self.oracles = {
                k: v for k, v in self.oracles.items() if not str(v.get("source", "")).startswith(fam + ":")
            }
            self.failures = {
                k: v
                for k, v in self.failures.items()
                if not (k == fam or k.startswith(fam + ":") or k.startswith(step + ":"))
            }
            self.notes = [n for n in self.notes if not n.startswith(fam + ":") and not n.startswith(step + ":")]
        # re-scope after the drop so tokens only the dropped entries referenced are not re-fetched
        self.tracked_assets = set(self.tokens.keys()) | self._referenced_tokens()
        self.notes.append(f"rerun: reset {sorted(steps)}")
        print(f"[rerun] reset steps {sorted(steps)}", flush=True)

    def run(self, out_dir: Optional[Path] = None):
        self._checkpoint_dir = out_dir
        live_head = self.w3.eth.block_number
        if self.head <= 0:
            self.head = live_head
        else:
            self.head = max(self.head, live_head)
        steps = [
            ("aave-v3", lambda: self.discover_aave_v3_spark("aave-v3", AAVE_V3_REGISTRY)),
            ("spark", lambda: self.discover_aave_v3_spark("spark", SPARK_REGISTRY)),
            ("aave-v4", self.discover_aave_v4),
            ("compound-v2", self.discover_compound_v2),
            ("sky", self.discover_sky),
            ("morpho", self.discover_morpho),
            ("euler", self.discover_euler),
            ("silo", self.discover_silo),
            ("ajna", self.discover_ajna),
            ("univ3", self.discover_univ3_pools),
        ]
        for name, fn in steps:
            if name in self.done_steps:
                print(f"[step] {name} SKIP (checkpoint)", flush=True)
                continue
            print(f"[step] {name}…", flush=True)
            fn()
            self.done_steps.add(name)
            print(f"[step] {name} done", flush=True)
            if out_dir:
                self.save_checkpoint(out_dir)
        missing = [a for a in sorted(self.tracked_assets) if a not in self.tokens]
        if missing:
            self.ensure_tokens_bulk(missing)
        print("[step] retry_failed_enrichment…", flush=True)
        self.retry_failed_enrichment()
        if out_dir:
            self.save_checkpoint(out_dir)

    def to_registry(self) -> dict[str, Any]:
        return {
            "chain_id": 1,
            "generated_at_block": self.head,
            "tokens": self.tokens,
            "protocols": self.protocols,
            "oracles": self.oracles,
            "pools": self.pools,
            "flash_sources": self.flash_sources,
            "routers": self.routers,
        }

    def to_meta(self, rpc_call: str, rpc_logs: str) -> dict[str, Any]:
        per: dict[str, Any] = {}
        for fam, c in self.counts.items():
            per[fam] = {
                "instances": c.instances,
                "reserves": c.reserves,
                "receipt_tokens": c.receipt_tokens,
                "aggregators": c.aggregators,
                "admitted_markets": c.admitted_markets,
            }
        admitted_total = sum(c.admitted_markets for c in self.counts.values())
        return {
            "chain_id": 1,
            "generated_at_block": self.head,
            "rpc_call": rpc_call,
            "rpc_logs": rpc_logs,
            "admission": {
                "interim": True,
                "threshold_usd_borrowed": INTERIM_ADMISSION_USD,
                "note": INTERIM_ADMISSION_NOTE,
                "admitted_markets_total": admitted_total,
            },
            "counts_by_protocol": per,
            "token_count": len(self.tokens),
            "pool_count": len(self.pools),
            "oracle_proxy_count": len(self.oracles),
            "failures": self.failures,
            "notes": self.notes,
        }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--rpc", default="https://ethereum.publicnode.com")
    ap.add_argument("--rpc-logs", default="https://gateway.tenderly.co/public/mainnet")
    ap.add_argument("--out", type=Path, default=Path(__file__).resolve().parents[2] / "registry")
    ap.add_argument("--sleep", type=float, default=0.06)
    ap.add_argument(
        "--rerun",
        default="",
        help="comma-separated steps to re-discover after loading the checkpoint/final registry "
        f"(choices: {','.join(DISCOVERY_STEP_NAMES)})",
    )
    args = ap.parse_args()
    rerun = [s.strip() for s in args.rerun.split(",") if s.strip()]
    bad = [s for s in rerun if s not in STEP_FAMILY]
    if bad:
        print(f"unknown --rerun step(s): {bad}", file=sys.stderr)
        return 2
    w3 = Web3(Web3.HTTPProvider(args.rpc, request_kwargs={"timeout": 120}))
    w3_logs = Web3(Web3.HTTPProvider(args.rpc_logs, request_kwargs={"timeout": 120}))
    for attempt in range(8):
        if w3.is_connected() and w3_logs.is_connected():
            break
        time.sleep(2.0 * (attempt + 1))
    if not w3.is_connected():
        print("RPC not connected", file=sys.stderr)
        return 1
    b = Builder(w3=w3, w3_logs=w3_logs, sleep_s=args.sleep)
    partial_loaded = b.load_checkpoint(args.out)
    if not partial_loaded:
        b.bootstrap_from_final_registry(args.out)
    if rerun:
        b.reset_steps(rerun)
    print(f"head={w3.eth.block_number} discovering…", flush=True)
    t0 = time.time()
    b.run(out_dir=args.out)
    args.out.mkdir(parents=True, exist_ok=True)
    reg_path = args.out / "registry.json"
    meta_path = args.out / "registry.meta.json"
    reg_path.write_text(json.dumps(b.to_registry(), indent=2) + "\n", encoding="utf-8")
    meta_path.write_text(json.dumps(b.to_meta(args.rpc, args.rpc_logs), indent=2) + "\n", encoding="utf-8")
    partial_reg = args.out / "registry.partial.json"
    partial_meta = args.out / "registry.partial.meta.json"
    if partial_reg.is_file():
        partial_reg.unlink()
    if partial_meta.is_file():
        partial_meta.unlink()
    print(f"Wrote {reg_path} ({reg_path.stat().st_size} bytes) in {time.time()-t0:.0f}s", flush=True)
    print(f"Wrote {meta_path} ({meta_path.stat().st_size} bytes)", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
