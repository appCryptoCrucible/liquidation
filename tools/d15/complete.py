#!/usr/bin/env python3
"""
D15 completion pass — fills the classes the Essential-tier discovery does not cover.

Reads d15_addresses.json (output of discover_d15_addresses.py) and adds, from chain:

  1. Oracle sources resolved RECURSIVELY to the OCR aggregators that emit
     `AnswerUpdated`, through Capo / synchronicity / SVR / Morpho / Euler / Gearbox
     wrapper getters, plus every HISTORICAL phase aggregator (AggregatorProxy
     `phaseAggregators(i)` and Chainlink FeedRegistry `getPhaseFeed`). A replay
     over the archive needs the aggregator that was live then, not only now.
  2. Aave V4 spoke-oracle sources (UpdateReserveSource logs, getReserveSource).
  3. Compound V3: every comet from the Configurator's CometDeployed logs, each
     comet's asset price feeds and base feed.
  4. Morpho Blue: every market's oracle and IRM from CreateMarket logs.
  5. Euler V2: each vault's router → configured adapter per (asset, unitOfAccount)
     for the vault asset and each collateral.
  6. Liquity V2 price-feed underlying aggregators.
  7. Sky: Vat, Jug, Pot, Spotter, Dog, Flash, every ilk's Clipper, every OSM's
     medianizer (`src()`), every gem.
  8. Gearbox V3: PriceOracleV3 per credit manager and its per-token feeds.
  9. Silo V2: solvency / maxLtv oracles per silo config.
 10. Flash sources: Uniswap V4 PoolManager, Sky DSS Flash (Aave pools, Morpho
     singleton already present).
 11. Exit venues: Uniswap V3 pools, Curve pools (MetaRegistry), Kyber Elastic
     pools for every tracked asset against the hub assets.
 12. Rate providers for derived pricing (GUIDE 06 Step 7).

Never invents an address: constants are protocol singletons with a "confirm"
flag; everything else is read from chain. Writes separate *.complete.* files and
never overwrites the drafts.

Usage:
  python d15_complete.py --rpc https://ethereum.publicnode.com --guides ../liquidator-guides
"""
from __future__ import annotations

import re
import time
from collections import defaultdict
from typing import Any, Iterable, Optional

from eth_abi import decode, encode
from web3 import Web3

from .roots import ROOTS

ZERO = "0x0000000000000000000000000000000000000000"
MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
# lowercase: eth_abi rejects a mixed-case string that is not the exact EIP-55 form
ETH_DENOM = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
USD_DENOM = "0x0000000000000000000000000000000000000348"
BTC_DENOM = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

# --------------------------------------------------------------------------- constants
# Every entry here is a well-known singleton. `confirm` means the human at H1
# checks it against the protocol's published deployments; nothing here is guessed.
CONSTANTS = [
    # flash sources (D09)
    ("uniswap-v4", "pool_manager", "0x000000000004444c5dc75cB358380D2e3dE08A90", "flash source + all V4 pool events"),
    ("sky-maker", "dss_flash", "0x60744434d6339a6B27d73d9Eda62b6F66a0a04FA", "flash source (ERC-3156, DAI)"),
    # Sky core (liquidation events: Dog.Bark, Clipper.Kick/Take; rates: Jug/Pot; prices: Spotter.Poke)
    ("sky-maker", "vat", "0x35D1b3F3D7966A1DFe207aa4514C12a259A0492B", "core"),
    ("sky-maker", "jug", "0x19c0976f590D67707E62397C87829d896Dc0f1F1", "stability fee drip"),
    ("sky-maker", "pot", "0x197E90f9FAD81970bA7976f33CbD77088E5D7cf7", "chi — sDAI rate provider"),
    ("sky-maker", "spotter", "0x65C79fcB50Ca1594B025960e539eD7A9a6D434A3", "Poke events"),
    ("sky-maker", "dog", "0x135954d155898D42C90D2a57824C690e0c7BEf1B", "Bark — liquidation ground truth"),
    ("sky-maker", "susds", "0xa3931d71877C0E7a3148CB7Eb4463524FEc27fbD", "rate provider (ERC-4626)"),
    # DEX factories / registries (events used for pool discovery; pools themselves added below)
    ("uniswap-v3", "factory", "0x1F98431c8aD98523631AE4a59f267346ea31F984", "PoolCreated"),
    ("curve", "meta_registry", "0xF98B45FA17DE75FB1aD0e7aFD971b0ca00e379fC", "pool lookup only"),
    ("kyber-elastic", "factory", "0x5F1dddbf348aC2fbe22a163e30F99F9ECE3DD50a", "PoolCreated"),
    # rate providers for derived pricing (GUIDE 06 Step 7)
    ("rate-provider", "lido_steth", "0xae7ab96520DE3A18E5E111B5EaAb095312D7fE84", "TokenRebased"),
    ("rate-provider", "wsteth", "0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0", "stEthPerToken"),
    ("rate-provider", "rocket_network_balances", "0x6Cc65bF618F55ce2433f9D8d827Fc44117D81399", "BalancesUpdated — rETH rate"),
    ("rate-provider", "reth", "0xae78736Cd615f374D3085123A210448E74Fc6393", "getExchangeRate"),
    ("rate-provider", "cbeth", "0xBe9895146f7AF43049ca1c1AE358B0541Ea49704", "ExchangeRateUpdated"),
    ("rate-provider", "sdai", "0x83F20F44975D03b1b09e64809B757c47f942BEeA", "ERC-4626 over Pot"),
    ("rate-provider", "susde", "0x9D39A5DE30e57443BfF2A8307A4256c8797A3497", "ERC-4626"),
    ("rate-provider", "weeth", "0xCd5fE23C85820F7B72D0926FC9b05b43E359b7ee", "getRate"),
    ("rate-provider", "etherfi_liquidity_pool", "0x308861A430be4cce5502d0A12724771Fc6DaF216", "eETH rate source"),
    ("rate-provider", "sfrxeth", "0xac3E018457B222d93114458476f3E3416Abbe38F", "ERC-4626"),
    ("rate-provider", "rseth_lrt_oracle", "0x349A73444b1a310BAe67ef67973022020d70020d", "rsETHPrice"),
    ("rate-provider", "renzo_restake_manager", "0x74a09653A083691711cF8215a6ab074BB4e99ef5", "ezETH TVL"),
    # oracle networks that push on-chain (pull-based: their update tx is the trigger)
    ("oracle-network", "pyth", "0x4305FB66699C3B2702D4d05CF36551390A4c69C6", "PriceFeedUpdate"),
    ("oracle-network", "chainlink_feed_registry", "0x47Fb2585D2C56Fe188D0E6ec628a38b74fCeeeDf", "lookup only"),
    ("fluid", "liquidity_layer", "0x52Aa899454998Be5b000Ad077a46Bbe360F4e497", "LogOperate — all Fluid flows"),
    ("morpho-blue", "adaptive_curve_irm", "0x870aC11D48B15DB9a138Cf899d20F13F79Ba00BC", "BorrowRateUpdate"),
]

# Hub assets: profit converges on WETH (D29); the stables and BTC are the other
# side of nearly every exit route. Pools of every tracked asset against these.
HUB_ASSETS = {
    "WETH": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
    "USDC": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
    "USDT": "0xdAC17F958D2ee523a2206206994597C13D831ec7",
    "DAI": "0x6B175474E89094C44Da98b954EedeAC495271d0F",
    "WBTC": "0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599",
    "wstETH": "0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0",
    "USDe": "0x4c9EDD5852cd905f086C759E8383e09bff1E68B3",
    "cbBTC": "0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf",
}
UNIV3_FEES = [100, 500, 3000, 10000]
KYBER_FEES = [8, 10, 40, 300, 1000]

# Getters that return an address (first word) pointing at an upstream price source.
# Order matters only for labelling; every one is tried.
FEED_GETTERS = [
    # Chainlink proxy
    "aggregator()",
    # Aave Capo / synchronicity / SVR wrappers
    "ASSET_TO_USD_AGGREGATOR()", "BASE_TO_USD_AGGREGATOR()", "RATIO_PROVIDER()",
    "ASSET_TO_PEG()", "PEG_TO_BASE()", "BASE_TO_USD()", "ASSET_TO_BASE()",
    "ASSET_TO_ETH_AGGREGATOR()", "ETH_TO_USD_AGGREGATOR()",
    # Morpho ChainlinkOracleV2
    "BASE_FEED_1()", "BASE_FEED_2()", "QUOTE_FEED_1()", "QUOTE_FEED_2()", "BASE_VAULT()", "QUOTE_VAULT()",
    # Euler adapters
    "feed()", "oracleBaseCross()", "oracleCrossQuote()", "fallbackOracle()", "pyth()",
    # Gearbox wrappers
    "priceFeed()", "targetToBasePriceFeed()", "baseToUSDPriceFeed()", "priceFeed0()", "priceFeed1()", "priceFeed2()",
    # Liquity V2 price feeds (struct — first word is the aggregator)
    "ethUsdOracle()", "stEthUsdOracle()", "rEthEthOracle()", "stEthEthOracle()", "wstEthStEthOracle()",
    # Compound / generic
    "underlyingPriceFeed()", "underlying()", "source()", "oracle()", "FEED()", "chainlinkFeed()", "primaryAggregator()", "secondaryAggregator()",
    # Sky OSM → medianizer
    "src()",
    # Silo
    "oracleConfig()",
]


def sel(sig: str) -> bytes:
    return Web3.keccak(text=sig)[:4]


def is_addr_word(word: bytes) -> Optional[str]:
    if len(word) < 32 or any(word[:12]):
        return None
    a = "0x" + word[12:32].hex()
    return None if a == ZERO else Web3.to_checksum_address(a)


def topic(sig: str) -> str:
    # web3 v7 HexBytes.hex() has no 0x prefix; strict RPCs reject it.
    return Web3.to_hex(Web3.keccak(text=sig))


class Completer:
    def __init__(self, w3: Web3, w3_logs: Web3, sleep_s: float = 0.08, batch: int = 150):
        self.w3 = w3
        self.w3_logs = w3_logs
        self.sleep_s = sleep_s
        self.batch = batch
        self.entries: list[dict[str, Any]] = []
        self.seen: set[str] = set()
        self.failures: dict[str, str] = {}
        self.notes: list[str] = []
        self.assets: dict[str, str] = {}  # lower addr -> label/source
        self.code_cache: dict[str, bool] = {}
        self.resolved: set[str] = set()

    # ------------------------------------------------------------------ plumbing
    def add(self, protocol: str, kind: str, address: str, source: str, **extra):
        if not address or address.lower() == ZERO:
            return
        addr = Web3.to_checksum_address(address)
        if addr.lower() in self.seen:
            return
        self.seen.add(addr.lower())
        e = {"protocol": protocol, "kind": kind, "address": addr, "source": source}
        e.update(extra)
        self.entries.append(e)

    def add_asset(self, address: str, label: str):
        if address and address.lower() != ZERO:
            self.assets.setdefault(address.lower(), label)

    def try_erc20_asset(self, address: str, label: str):
        if not address or address.lower() == ZERO or address.lower() == ETH_DENOM.lower():
            return
        if address.lower() in self.assets:
            return
        ret = self.call1(address, "decimals()")
        if ret and len(ret) >= 32:
            self.add_asset(address, label)
        else:
            self.failures[f"asset:{address[:10]}"] = f"decimals() failed — not ERC-20 ({label})"

    def multicall(self, calls: list[tuple[str, bytes]]) -> list[tuple[bool, bytes]]:
        """tryAggregate(false, calls) in batches. Returns (success, returndata) per call."""
        out: list[tuple[bool, bytes]] = []
        for i in range(0, len(calls), self.batch):
            chunk = calls[i : i + self.batch]
            data = sel("tryAggregate(bool,(address,bytes)[])") + encode(
                ["bool", "(address,bytes)[]"],
                [False, [(Web3.to_checksum_address(t), d) for t, d in chunk]],
            )
            for attempt in range(4):
                try:
                    time.sleep(self.sleep_s)
                    raw = self.w3.eth.call({"to": MULTICALL3, "data": data})
                    res = decode(["(bool,bytes)[]"], raw)[0]
                    out.extend([(bool(s), bytes(r)) for s, r in res])
                    break
                except Exception as ex:  # noqa: BLE001
                    if attempt == 3:
                        # degrade: mark the whole chunk failed rather than abort
                        self.failures[f"multicall:{i}"] = str(ex)[:200]
                        out.extend([(False, b"")] * len(chunk))
                    else:
                        time.sleep(1.0 * (attempt + 1))
        return out

    def call1(self, to: str, sig: str, args: tuple = (), types: tuple = ()) -> Optional[bytes]:
        data = sel(sig) + (encode(list(types), list(args)) if types else b"")
        ok, ret = self.multicall([(to, data)])[0]
        return ret if ok and ret else None

    def prefetch_code(self, addrs: Iterable[str]):
        """One multicall per 150 addresses: Multicall3.getCodeHash? no — use
        tryAggregate against each address with empty calldata; an address with
        no code returns success + empty data, a contract usually reverts or returns
        data. That is ambiguous, so use JSON-RPC batching of eth_getCode instead."""
        todo = [Web3.to_checksum_address(a) for a in {a.lower() for a in addrs} if a.lower() not in self.code_cache]
        for i in range(0, len(todo), 100):
            chunk = todo[i : i + 100]
            try:
                time.sleep(self.sleep_s)
                with self.w3.batch_requests() as batch:
                    for a in chunk:
                        batch.add(self.w3.eth.get_code(a))
                    res = batch.execute()
                for a, code in zip(chunk, res):
                    self.code_cache[a.lower()] = len(code) > 2
            except Exception:  # noqa: BLE001
                for a in chunk:  # fall back to singles
                    self.has_code(a)

    def has_code(self, addr: str) -> bool:
        k = addr.lower()
        if k not in self.code_cache:
            try:
                time.sleep(self.sleep_s)
                self.code_cache[k] = len(self.w3.eth.get_code(Web3.to_checksum_address(addr))) > 2
            except Exception:  # noqa: BLE001
                self.code_cache[k] = False
        return self.code_cache[k]

    def get_logs(self, address: str | list[str], topics: list, from_block: int, to_block: int, chunk: int = 1_000_000):
        logs = []
        start = from_block
        errs = 0
        n_calls = 0
        while start <= to_block:
            end = min(start + chunk - 1, to_block)
            try:
                time.sleep(self.sleep_s)
                n_calls += 1
                logs.extend(self.w3_logs.eth.get_logs({"address": address, "topics": topics, "fromBlock": start, "toBlock": end}))
                start = end + 1
                errs = 0
            except Exception as ex:  # noqa: BLE001
                msg = str(ex)
                errs += 1
                # result-size / range limits: halve the window. Rate limits: wait, same window.
                if any(k in msg.lower() for k in ("range", "limit", "too many results", "exceed", "10000", "response size")) and chunk > 1_000:
                    chunk //= 2
                    continue
                if errs < 6:
                    time.sleep(1.5 * errs)
                    continue
                self.failures[f"getLogs:{str(address)[:44]}:{start}"] = msg[:200]
                start = end + 1
                errs = 0
        print(f"    getLogs {str(address)[:14]}… {from_block}-{to_block}: {len(logs)} logs in {n_calls} calls", flush=True)
        return logs

    # ------------------------------------------------------------------ oracle recursion
    def resolve_feeds(self, protocol: str, roots: Iterable[tuple[str, str]], max_depth: int = 5):
        """roots: (address, source-label). Walk every FEED_GETTER, add children, recurse.
        Chainlink proxies additionally yield every historical phase aggregator."""
        root_list = [(Web3.to_checksum_address(a), s) for a, s in roots if a and a.lower() != ZERO]
        root_addrs = {a.lower() for a, _ in root_list}
        agg_before = sum(
            1
            for e in self.entries
            if e.get("protocol") == protocol and str(e.get("kind", "")).startswith("oracle_aggregator")
        )
        frontier = [(a, s, 0) for a, s in root_list]
        while frontier:
            layer = [(a, s, d) for a, s, d in frontier if a.lower() not in self.resolved and d <= max_depth]
            frontier = []
            if not layer:
                break
            for a, _, _ in layer:
                self.resolved.add(a.lower())
            calls = [(a, sel(g)) for a, _, _ in layer for g in FEED_GETTERS]
            calls += [(a, sel("phaseId()")) for a, _, _ in layer]
            res = self.multicall(calls)
            n = len(FEED_GETTERS)
            # batch the code checks for every candidate child in this layer
            cand = []
            for idx in range(len(layer)):
                for g_i in range(n):
                    ok, ret = res[idx * n + g_i]
                    if ok and len(ret) >= 32:
                        ch = is_addr_word(ret[:32])
                        if ch:
                            cand.append(ch)
            self.prefetch_code(cand)
            phase_calls: list[tuple[str, bytes]] = []
            phase_meta: list[tuple[str, str, int]] = []
            for idx, (a, s, d) in enumerate(layer):
                children: list[tuple[str, str]] = []
                for g_i, g in enumerate(FEED_GETTERS):
                    ok, ret = res[idx * n + g_i]
                    if not ok or len(ret) < 32:
                        continue
                    child = is_addr_word(ret[:32])
                    if child and child.lower() != a.lower():
                        children.append((child, g))
                ok, ret = res[len(layer) * n + idx]
                phase_id = int.from_bytes(ret[:32], "big") if ok and len(ret) >= 32 else 0
                if 0 < phase_id <= 64:
                    for p in range(1, phase_id + 1):
                        phase_calls.append((a, sel("phaseAggregators(uint16)") + encode(["uint16"], [p])))
                        phase_meta.append((a, s, p))
                for child, g in children:
                    if not self.has_code(child):
                        continue
                    kind = "oracle_aggregator" if g == "aggregator()" else "oracle_source"
                    if g in ("BASE_VAULT()", "QUOTE_VAULT()"):
                        kind = "rate_provider"
                    self.add(protocol, kind, child, f"{s}→{g}", depth=d + 1)
                    frontier.append((child, f"{s}→{g}", d + 1))
            if phase_calls:
                pres = self.multicall(phase_calls)
                self.prefetch_code([x for x in (is_addr_word(r[:32]) if ok else None for ok, r in pres) if x])
                for (a, s, p), (ok, ret) in zip(phase_meta, pres):
                    if not ok:
                        continue
                    agg = is_addr_word(ret[:32])
                    if agg and self.has_code(agg):
                        self.add(protocol, "oracle_aggregator_phase", agg, f"{s}→phaseAggregators({p})", phase=p)
        agg_after = sum(
            1
            for e in self.entries
            if e.get("protocol") == protocol and str(e.get("kind", "")).startswith("oracle_aggregator")
        )
        if root_addrs and agg_after <= agg_before:
            for a, s in root_list:
                if a.lower() in root_addrs:
                    self.failures[f"oracle-unresolved:{a[:10]}"] = (
                        f"{s}: no oracle_aggregator reached within depth {max_depth}"
                    )

    # ------------------------------------------------------------------ classes
    def constants(self):
        for proto, kind, addr, why in CONSTANTS:
            if self.has_code(addr):
                self.add(proto, kind, addr, f"constant — confirm: {why}", confirm=True)
            else:
                self.failures[f"constant:{kind}"] = f"{addr} has no code — wrong address or wrong chain"
        if self.has_code(ROOTS["fluid_vault_factory"]):
            self.add(
                "fluid",
                "vault_factory",
                ROOTS["fluid_vault_factory"],
                "constant — confirm: Fluid VaultFactory",
                confirm=True,
            )

    def aave_v4_spoke_oracles(self, base_entries: list[dict]):
        spokes = [e["address"] for e in base_entries if e["protocol"] == "aave-v4" and e.get("kind") == "spoke"]
        if not spokes:
            return
        res = self.multicall([(s, sel("ORACLE()")) for s in spokes])
        for s, (ok, ret) in zip(spokes, res):
            o = is_addr_word(ret[:32]) if ok else None
            if o and self.has_code(o):
                self.add("aave-v4", "spoke_oracle", o, f"{Web3.to_checksum_address(s)[:10]}.ORACLE()")
            else:
                self.failures[f"aave-v4:{s[:10]}"] = "spoke.ORACLE() failed"

    def aave_v3_like(self, base_entries: list[dict]):
        """Resolve every oracle_proxy / price_oracle source from the draft; collect assets."""
        for proto in ("aave-v3", "spark"):
            roots = []
            for e in base_entries:
                if e["protocol"] != proto:
                    continue
                m = re.search(r"getSourceOfAsset\((0x[0-9a-fA-F]{40})\)", e.get("source", ""))
                if m:
                    self.try_erc20_asset(m.group(1), f"{proto} reserve")
                if e["kind"] in ("oracle_proxy",):
                    roots.append((e["address"], f"{proto}:{e['source'][:48]}"))
            self.resolve_feeds(proto, roots)

    def aave_v4(self, base_entries: list[dict], from_block: int):
        # V4 IAaveOracle emits UpdateReserveSource(uint256 indexed reserveId, address indexed source).
        # topics[1] is reserveId, NOT an address — never pass it to add_asset.
        tp = topic("UpdateReserveSource(uint256,address)")
        oracle_seen: dict[str, str] = {}
        for e in base_entries + self.entries:
            if e["protocol"] == "aave-v4" and e["kind"] == "spoke_oracle":
                oracle_seen[e["address"].lower()] = e["address"]
        oracles = [Web3.to_checksum_address(a) for a in oracle_seen.values()]
        head = self.w3.eth.block_number
        roots = []
        pending_asset: list[tuple[str, int]] = []
        seen_rid: set[tuple[str, int]] = set()
        n_logs = 0
        for lg in self.get_logs(oracles, [tp], from_block, head):
            n_logs += 1
            o = Web3.to_checksum_address(lg["address"])
            topics = lg["topics"]
            if len(topics) < 3:
                self.failures[f"aave-v4:UpdateReserveSource:{n_logs}"] = (
                    "UpdateReserveSource log missing indexed reserveId/source"
                )
                continue
            rid = int.from_bytes(bytes(topics[1]), "big")
            src = is_addr_word(bytes(topics[2]))
            if src:
                roots.append((src, f"aave-v4:{o[:10]}.UpdateReserveSource({rid})"))
            k = (o.lower(), rid)
            if k not in seen_rid:
                seen_rid.add(k)
                pending_asset.append((o, rid))
        if pending_asset:
            ares = self.multicall(
                [(o, sel("getReserveAsset(uint256)") + encode(["uint256"], [rid])) for o, rid in pending_asset]
            )
            for (o, rid), (ok, ret) in zip(pending_asset, ares):
                asset = is_addr_word(ret[:32]) if ok else None
                if asset:
                    self.try_erc20_asset(asset, "aave-v4 reserve")
                else:
                    self.failures[f"aave-v4:getReserveAsset:{o[:10]}:{rid}"] = (
                        "getReserveAsset(reserveId) failed for UpdateReserveSource log"
                    )
        self.notes.append(f"aave-v4: {n_logs} UpdateReserveSource logs")
        for o in oracles:
            # Current-state enumeration unions with historical log sources (deduped by add()).
            # V4 SpokeOracle is keyed by reserveId, not asset. Verified on-chain 2026-09-19:
            # selector e4337e38 == getReserveSource(uint256) returns the source address
            # (main_spoke_oracle reserve 0 -> 0x5424384B…, the WETH SVR feed); reserve ids
            # past the end return zero. Enumerate until zero.
            calls = [(o, sel("getReserveSource(uint256)") + encode(["uint256"], [i])) for i in range(96)]
            res = self.multicall(calls)
            found = 0
            for i, (ok, ret) in enumerate(res):
                src = is_addr_word(ret[:32]) if ok else None
                if src:
                    found += 1
                    roots.append((src, f"aave-v4:{o[:10]}.getReserveSource({i})"))
                    if (o.lower(), i) not in seen_rid:
                        asset_ret = self.call1(o, "getReserveAsset(uint256)", (i,), ("uint256",))
                        asset = is_addr_word(asset_ret[:32]) if asset_ret else None
                        if asset:
                            self.try_erc20_asset(asset, "aave-v4 reserve")
            if found == 0:
                self.failures[f"aave-v4:{o}"] = "getReserveSource(uint256) returned no sources — confirm this oracle's ABI at C3"
        if not roots:
            self.failures["aave-v4:oracle_sources"] = (
                "no UpdateReserveSource logs and getReserveSource failed — confirm V4 oracle ABI"
            )
        for src, s in roots:
            self.add("aave-v4", "oracle_source", src, s)
        self.resolve_feeds("aave-v4", roots)

    def compound_v3(self, configurator: str, from_block: int):
        tp = topic("CometDeployed(address,address)")
        head = self.w3.eth.block_number
        comets = set()
        for lg in self.get_logs(Web3.to_checksum_address(configurator), [tp], from_block, head):
            proxy = is_addr_word(bytes(lg["topics"][1]))
            if proxy:
                comets.add(proxy)
        for c in sorted(comets):
            self.add("compound-v3", "comet", c, "Configurator.CometDeployed")
        roots = []
        for c in sorted(comets):
            ret = self.call1(c, "numAssets()")
            n = int.from_bytes(ret[:32], "big") if ret else 0
            calls = [(c, sel("getAssetInfo(uint8)") + encode(["uint8"], [i])) for i in range(n)]
            calls += [(c, sel("baseTokenPriceFeed()")), (c, sel("baseToken()"))]
            res = self.multicall(calls)
            for i in range(n):
                ok, ret = res[i]
                if ok and len(ret) >= 3 * 32:
                    asset = is_addr_word(ret[32:64])
                    feed = is_addr_word(ret[64:96])
                    if asset:
                        self.try_erc20_asset(asset, "compound-v3 collateral")
                    if feed:
                        roots.append((feed, f"compound-v3:{c[:10]}.getAssetInfo({i}).priceFeed"))
            ok, ret = res[n]
            if ok:
                f = is_addr_word(ret[:32])
                if f:
                    roots.append((f, f"compound-v3:{c[:10]}.baseTokenPriceFeed"))
            ok, ret = res[n + 1]
            if ok:
                b = is_addr_word(ret[:32])
                if b:
                    self.try_erc20_asset(b, "compound-v3 base")
        for f, s in roots:
            self.add("compound-v3", "oracle_source", f, s)
        self.resolve_feeds("compound-v3", roots)

    def compound_v2(self, comptrollers: list[str], c_tokens: list[str]):
        roots: list[tuple[str, str]] = []
        ores = self.multicall([(c, sel("oracle()")) for c in comptrollers])
        for c, (ok, ret) in zip(comptrollers, ores):
            o = is_addr_word(ret[:32]) if ok else None
            if o and self.has_code(o):
                self.add("compound-v2", "price_oracle", o, f"comptroller({c[:10]}).oracle()")
                roots.append((o, f"compound-v2:{c[:10]}"))
            else:
                self.failures[f"compound-v2:{c[:10]}"] = "comptroller.oracle() failed"
        self.resolve_feeds("compound-v2", roots)
        ures = self.multicall([(t, sel("underlying()")) for t in c_tokens])
        for t, (ok, ret) in zip(c_tokens, ures):
            u = is_addr_word(ret[:32]) if ok else None
            if u:
                self.try_erc20_asset(u, f"compound-v2 cToken({t[:10]}).underlying()")

    def morpho(self, singleton: str, from_block: int):
        tp = topic("CreateMarket(bytes32,(address,address,address,address,uint256))")
        head = self.w3.eth.block_number
        roots = []
        n = 0
        logs = self.get_logs(Web3.to_checksum_address(singleton), [tp], from_block, head)
        self.prefetch_code([decode(["(address,address,address,address,uint256)"], bytes(lg["data"]))[0][2] for lg in logs])
        for lg in logs:
            loan, coll, oracle, irm, lltv = decode(["(address,address,address,address,uint256)"], bytes(lg["data"]))[0]
            n += 1
            self.try_erc20_asset(loan, "morpho loan")
            self.try_erc20_asset(coll, "morpho collateral")
            if irm and irm.lower() != ZERO:
                self.add("morpho-blue", "irm", irm, "CreateMarket.irm")
            if oracle and oracle.lower() != ZERO and self.has_code(oracle):
                self.add("morpho-blue", "market_oracle", oracle, f"CreateMarket({lg['topics'][1].hex()[:10]})")
                roots.append((oracle, f"morpho:{oracle[:10]}"))
        self.notes.append(f"morpho-blue: {n} markets from CreateMarket; {len(roots)} distinct-or-not oracles resolved")
        self.resolve_feeds("morpho-blue", roots)

    def euler(self, vaults: list[str]):
        calls = []
        for v in vaults:
            calls += [(v, sel("asset()")), (v, sel("oracle()")), (v, sel("unitOfAccount()")), (v, sel("LTVList()"))]
        res = self.multicall(calls)
        pair_calls: list[tuple[str, bytes]] = []
        pair_meta: list[str] = []
        coll_asset_calls: list[tuple[str, bytes]] = []
        coll_meta: list[tuple[str, str, str]] = []  # (router, uoa, collVault)
        routers = set()
        for i, v in enumerate(vaults):
            (ok_a, a), (ok_o, o), (ok_u, u), (ok_l, l) = res[4 * i : 4 * i + 4]
            asset = is_addr_word(a[:32]) if ok_a else None
            router = is_addr_word(o[:32]) if ok_o else None
            uoa = is_addr_word(u[:32]) if ok_u else None
            if asset:
                self.try_erc20_asset(asset, "euler vault asset")
            if not (router and uoa):
                continue
            routers.add(router)
            if asset:
                pair_calls.append((router, sel("getConfiguredOracle(address,address)") + encode(["address", "address"], [asset, uoa])))
                pair_meta.append(f"euler:{v[:10]} router.getConfiguredOracle(asset,uoa)")
            if ok_l and len(l) >= 64:
                try:
                    colls = decode(["address[]"], l)[0]
                except Exception:  # noqa: BLE001
                    colls = []
                for cv in colls:
                    coll_asset_calls.append((cv, sel("asset()")))
                    coll_meta.append((router, uoa, cv))
        for r in sorted(routers):
            self.add("euler-v2", "oracle_router", r, "vault.oracle()")
        cres = self.multicall(coll_asset_calls)
        for (router, uoa, cv), (ok, ret) in zip(coll_meta, cres):
            ca = is_addr_word(ret[:32]) if ok else None
            if ca:
                self.try_erc20_asset(ca, "euler collateral asset")
                pair_calls.append((router, sel("getConfiguredOracle(address,address)") + encode(["address", "address"], [ca, uoa])))
                pair_meta.append(f"euler:{router[:10]}.getConfiguredOracle({ca[:10]},uoa)")
        pres = self.multicall(pair_calls)
        self.prefetch_code([x for x in (is_addr_word(r[:32]) if ok else None for ok, r in pres) if x])
        roots = []
        for s, (ok, ret) in zip(pair_meta, pres):
            ad = is_addr_word(ret[:32]) if ok else None
            if ad and self.has_code(ad):
                self.add("euler-v2", "oracle_adapter", ad, s)
                roots.append((ad, s))
        # routers' fallback oracles too
        self.resolve_feeds("euler-v2", roots + [(r, f"euler:router:{r[:10]}") for r in sorted(routers)])

    def liquity_v2(self, collateral_registry: str, from_block: int):
        cr = Web3.to_checksum_address(collateral_registry)
        ret = self.call1(cr, "totalCollaterals()")
        n = int.from_bytes(ret[:32], "big") if ret else 0
        if n == 0:
            self.failures["liquity-v2:totalCollaterals"] = "totalCollaterals() failed or zero"
            return
        idx_calls: list[tuple[str, bytes]] = []
        for i in range(n):
            idx_calls.append((cr, sel("getTroveManager(uint256)") + encode(["uint256"], [i])))
            idx_calls.append((cr, sel("getToken(uint256)") + encode(["uint256"], [i])))
        res = self.multicall(idx_calls)
        tm_getters = [
            ("stabilityPool", "stabilityPool()"),
            ("activePool", "activePool()"),
            ("borrowerOperations", "borrowerOperations()"),
            ("sortedTroves", "sortedTroves()"),
            ("troveNFT", "troveNFT()"),
        ]
        feed_roots: list[tuple[str, str]] = []
        tms: list[str] = []
        for i in range(n):
            ok_tm, tm_ret = res[2 * i]
            ok_tok, tok_ret = res[2 * i + 1]
            tm = is_addr_word(tm_ret[:32]) if ok_tm else None
            tok = is_addr_word(tok_ret[:32]) if ok_tok else None
            if tok:
                self.try_erc20_asset(tok, f"liquity-v2 collateral {i}")
            if not tm:
                self.failures[f"liquity-v2:tm:{i}"] = "getTroveManager failed"
                continue
            tms.append(tm)
            self.add("liquity-v2", "troveManager", tm, f"CollateralRegistry.getTroveManager({i})")
            gres = self.multicall([(tm, sel(sig)) for _, sig in tm_getters])
            for (kind, sig), (ok, gret) in zip(tm_getters, gres):
                ad = is_addr_word(gret[:32]) if ok else None
                if ad and self.has_code(ad):
                    self.add("liquity-v2", kind, ad, f"TroveManager({tm[:10]}).{sig}")
        # priceFeed is internal on LiquityBase; constructor emits PriceFeedAddressChanged(address)
        # (not indexed — address is in log data). Do not call TroveManager.priceFeed().
        tp = topic("PriceFeedAddressChanged(address)")
        head = self.w3.eth.block_number
        logs = self.get_logs(tms, [tp], from_block, head) if tms else []
        seen_pf: set[str] = set()
        for lg in logs:
            data = bytes(lg["data"])
            ad = is_addr_word(data[:32]) if len(data) >= 32 else None
            if not ad:
                self.failures[f"liquity-v2:priceFeedLog:{lg['blockNumber']}"] = (
                    "PriceFeedAddressChanged log has no address in data"
                )
                continue
            if ad.lower() in seen_pf:
                continue
            seen_pf.add(ad.lower())
            if self.has_code(ad):
                self.add("liquity-v2", "priceFeed", ad, "PriceFeedAddressChanged constructor log")
                feed_roots.append((ad, f"liquity-v2:{ad[:10]}"))
            else:
                self.failures[f"liquity-v2:priceFeed:{ad[:10]}"] = (
                    "PriceFeedAddressChanged address has no code"
                )
        if tms and len(seen_pf) < len(tms):
            self.failures["liquity-v2:priceFeed"] = (
                f"PriceFeedAddressChanged resolved {len(seen_pf)} feeds for {len(tms)} TroveManagers"
            )
        self.resolve_feeds("liquity-v2", feed_roots)

    def sky(self, ilk_registry: str, dog: str, spotter: str):
        ret = self.call1(ilk_registry, "list()")
        ilks = decode(["bytes32[]"], ret)[0] if ret else []
        calls = []
        for ilk in ilks:
            calls += [
                (dog, sel("ilks(bytes32)") + encode(["bytes32"], [ilk])),
                (spotter, sel("ilks(bytes32)") + encode(["bytes32"], [ilk])),
                (ilk_registry, sel("gem(bytes32)") + encode(["bytes32"], [ilk])),
            ]
        res = self.multicall(calls)
        pips = []
        for i, ilk in enumerate(ilks):
            name = ilk.rstrip(b"\x00").decode(errors="replace")
            (ok_d, d), (ok_s, s), (ok_g, g) = res[3 * i : 3 * i + 3]
            clip = is_addr_word(d[:32]) if ok_d else None
            pip = is_addr_word(s[:32]) if ok_s else None
            gem = is_addr_word(g[:32]) if ok_g else None
            if clip and self.has_code(clip):
                self.add("sky-maker", "clipper", clip, f"Dog.ilks({name}).clip")
            if pip:
                pips.append((pip, f"sky:{name}.pip"))
            if gem:
                self.try_erc20_asset(gem, f"sky gem {name}")
        self.resolve_feeds("sky-maker", pips)

    def gearbox(self, credit_managers: list[str]):
        res = self.multicall([(cm, sel("priceOracle()")) for cm in credit_managers] + [(cm, sel("collateralTokensCount()")) for cm in credit_managers])
        n = len(credit_managers)
        oracles: dict[str, set[str]] = defaultdict(set)
        tok_calls = []
        tok_meta = []
        for i, cm in enumerate(credit_managers):
            ok, ret = res[i]
            po = is_addr_word(ret[:32]) if ok else None
            ok2, ret2 = res[n + i]
            cnt = int.from_bytes(ret2[:32], "big") if ok2 else 0
            if not po:
                continue
            self.add("gearbox-v3", "price_oracle", po, f"creditManager({cm[:10]}).priceOracle")
            for k in range(min(cnt, 64)):
                tok_calls.append((cm, sel("getTokenByMask(uint256)") + encode(["uint256"], [1 << k])))
                tok_meta.append(po)
        tres = self.multicall(tok_calls)
        feed_calls = []
        feed_meta = []
        for po, (ok, ret) in zip(tok_meta, tres):
            t = is_addr_word(ret[:32]) if ok else None
            if t:
                self.try_erc20_asset(t, "gearbox collateral")
                feed_calls.append((po, sel("priceFeeds(address)") + encode(["address"], [t])))
                feed_meta.append((po, t))
        fres = self.multicall(feed_calls)
        roots = []
        for (po, t), (ok, ret) in zip(feed_meta, fres):
            f = is_addr_word(ret[:32]) if ok else None
            if f and self.has_code(f):
                self.add("gearbox-v3", "oracle_source", f, f"priceOracle({po[:10]}).priceFeeds({t[:10]})")
                roots.append((f, f"gearbox:{f[:10]}"))
        self.resolve_feeds("gearbox-v3", roots)

    def gearbox_discover(self, register: str) -> list[str]:
        reg = Web3.to_checksum_address(register)
        self.add("gearbox-v3", "contracts_register", reg, "root:ContractsRegister (registry C1 gap fallback)")
        cms: list[str] = []
        pools: list[str] = []
        for fn in ("getCreditManagers()", "getCreditManagersList()"):
            ret = self.call1(reg, fn)
            if ret:
                try:
                    cms = list(decode(["address[]"], ret)[0])
                    if cms:
                        break
                except Exception:  # noqa: BLE001
                    pass
        for fn in ("getPools()", "getPoolsList()"):
            ret = self.call1(reg, fn)
            if ret:
                try:
                    pools = list(decode(["address[]"], ret)[0])
                    if pools:
                        break
                except Exception:  # noqa: BLE001
                    pass
        for cm in cms:
            self.add("gearbox-v3", "credit_manager", cm, "ContractsRegister.getCreditManagers()")
        for p in pools:
            self.add("gearbox-v3", "pool", p, "ContractsRegister.getPools()")
        if not cms:
            self.failures["gearbox-v3:register"] = "ContractsRegister returned no credit managers"
        return cms

    def ajna(self):
        for kind, factory in (
            ("erc20_pool", ROOTS["ajna_erc20_factory"]),
            ("erc721_pool", ROOTS["ajna_erc721_factory"]),
        ):
            fac = Web3.to_checksum_address(factory)
            self.add("ajna", f"{kind}_factory", fac, f"root:ajna {kind}_factory")
            ret = self.call1(fac, "getDeployedPoolsList()")
            if not ret:
                self.failures[f"ajna:{kind}"] = "getDeployedPoolsList() failed"
                continue
            try:
                pools = decode(["address[]"], ret)[0]
            except Exception as ex:  # noqa: BLE001
                self.failures[f"ajna:{kind}"] = str(ex)[:200]
                continue
            for p in pools:
                if p and int(p, 16) != 0:
                    self.add("ajna", kind, p, f"factory.getDeployedPoolsList({fac[:10]})")

    def silo(self, silo_configs: list[str], silos_by_config: dict[str, list[str]]):
        calls = []
        meta = []
        for cfg in sorted(silo_configs):
            for s in silos_by_config.get(cfg.lower(), []):
                calls.append((cfg, sel("getConfig(address)") + encode(["address"], [s])))
                meta.append((cfg, s))
        res = self.multicall(calls)
        roots = []
        for (cfg, s), (ok, ret) in zip(meta, res):
            if not ok or len(ret) < 9 * 32:
                continue
            # struct order: daoFee, deployerFee, silo, token, protectedShareToken, collateralShareToken,
            #               debtShareToken, solvencyOracle, maxLtvOracle, ...
            token = is_addr_word(ret[3 * 32 : 4 * 32])
            for off, label, kind in (
                (4, "protectedShareToken", "share_protected"),
                (5, "collateralShareToken", "share_collateral"),
                (6, "debtShareToken", "share_debt"),
            ):
                st = is_addr_word(ret[off * 32 : (off + 1) * 32])
                if st:
                    self.add("silo-v2", kind, st, f"siloConfig({cfg[:10]}).getConfig({s[:10]}).{label}")
            for off, label in ((7, "solvencyOracle"), (8, "maxLtvOracle")):
                o = is_addr_word(ret[off * 32 : (off + 1) * 32])
                if o and self.has_code(o):
                    self.add("silo-v2", "oracle_source", o, f"siloConfig({cfg[:10]}).getConfig({s[:10]}).{label}")
                    roots.append((o, f"silo:{o[:10]}"))
            if token:
                self.try_erc20_asset(token, "silo asset")
        self.resolve_feeds("silo-v2", roots)

    def fluid(self, vaults: list[str], resolver: str | None = None):
        """VaultResolver.getVaultEntireData(vault).configs.oracle + token assets."""
        resolver = Web3.to_checksum_address(resolver or ROOTS["fluid_vault_resolver"])
        if not vaults:
            ret = self.call1(resolver, "getAllVaultsAddresses()")
            if ret:
                try:
                    vaults = decode(["address[]"], ret)[0]
                except Exception as ex:  # noqa: BLE001
                    self.failures["fluid:getAllVaultsAddresses"] = str(ex)[:200]
                    vaults = []
            else:
                self.failures["fluid:getAllVaultsAddresses"] = "empty return"
                vaults = []
        for v in vaults:
            self.add("fluid", "vault", v, "VaultResolver.getAllVaultsAddresses()")
        roots: list[tuple[str, str]] = []
        # VaultResolver.getVaultEntireData(vault): configs.oracle is word index 29 (on-chain layout verified).
        calls = [
            (resolver, sel("getVaultEntireData(address)") + encode(["address"], [Web3.to_checksum_address(v)]))
            for v in vaults
        ]
        vres = self.multicall(calls)
        for v, (ok, ret) in zip(vaults, vres):
            if not ok or len(ret) < 30 * 32:
                self.failures[f"fluid:{v[:10]}"] = "getVaultEntireData failed"
                continue
            for off in (11, 13):
                if len(ret) >= (off + 1) * 32:
                    t = is_addr_word(ret[off * 32 : (off + 1) * 32])
                    if t:
                        self.try_erc20_asset(t, "fluid vault supply/borrow token")
            o = is_addr_word(ret[29 * 32 : 30 * 32])
            if o and self.has_code(o):
                self.add("fluid", "oracle_source", o, f"VaultResolver.getVaultEntireData({v[:10]}).configs.oracle")
                roots.append((o, f"fluid:{v[:10]}"))
            else:
                self.failures[f"fluid:{v[:10]}"] = "configs.oracle empty or no code at word 29"
        if roots:
            self.resolve_feeds("fluid", roots)
        self.notes.append(f"fluid: {len(vaults)} vaults via VaultResolver; {len(roots)} oracle roots resolved")

    def feed_registry(self, registry: str):
        """Chainlink FeedRegistry: for every tracked asset × {USD, ETH, BTC}, every phase feed."""
        assets = list(self.assets) + [a.lower() for a in HUB_ASSETS.values()]
        assets = sorted(set(assets))
        denoms = [USD_DENOM, ETH_DENOM, BTC_DENOM]
        calls = [(registry, sel("getCurrentPhaseId(address,address)") + encode(["address", "address"], [Web3.to_checksum_address(a), d])) for a in assets for d in denoms]
        res = self.multicall(calls)
        phase_calls = []
        phase_meta = []
        k = 0
        for a in assets:
            for d in denoms:
                ok, ret = res[k]
                k += 1
                pid = int.from_bytes(ret[:32], "big") if ok and len(ret) >= 32 else 0
                for p in range(1, min(pid, 64) + 1):
                    phase_calls.append((registry, sel("getPhaseFeed(address,address,uint16)") + encode(["address", "address", "uint16"], [Web3.to_checksum_address(a), d, p])))
                    phase_meta.append((a, d, p, pid))
        pres = self.multicall(phase_calls)
        self.prefetch_code([x for x in (is_addr_word(r[:32]) if ok else None for ok, r in pres) if x])
        dn = {USD_DENOM: "USD", ETH_DENOM: "ETH", BTC_DENOM: "BTC"}
        for (a, d, p, pid), (ok, ret) in zip(phase_meta, pres):
            agg = is_addr_word(ret[:32]) if ok else None
            if agg and self.has_code(agg):
                kind = "oracle_aggregator" if p == pid else "oracle_aggregator_phase"
                self.add("chainlink", kind, agg, f"FeedRegistry.getPhaseFeed({a[:10]},{dn[d]},{p})", phase=p, current=(p == pid))

    def dex_pools(self, univ3_factory: str, curve_meta: str, kyber_factory: str):
        assets = sorted(set(list(self.assets) + [a.lower() for a in HUB_ASSETS.values()]))
        hubs = [a.lower() for a in HUB_ASSETS.values()]
        pairs = set()
        for a in assets:
            for h in hubs:
                if a != h:
                    pairs.add(tuple(sorted((a, h))))
        pairs = sorted(pairs)
        # Uniswap V3
        calls = [(univ3_factory, sel("getPool(address,address,uint24)") + encode(["address", "address", "uint24"], [Web3.to_checksum_address(a), Web3.to_checksum_address(b), f])) for a, b in pairs for f in UNIV3_FEES]
        res = self.multicall(calls)
        n = 0
        for (a, b), i in zip((p for p in pairs for _ in UNIV3_FEES), range(len(calls))):
            ok, ret = res[i]
            pool = is_addr_word(ret[:32]) if ok else None
            if pool:
                n += 1
                self.add("uniswap-v3", "pool", pool, f"factory.getPool({a[:10]},{b[:10]},{UNIV3_FEES[i % 4]})")
        self.notes.append(f"uniswap-v3: {n} pools for {len(pairs)} asset×hub pairs × 4 fee tiers (flash sources + exit venues)")
        # Curve
        calls = [(curve_meta, sel("find_pools_for_coins(address,address)") + encode(["address", "address"], [Web3.to_checksum_address(a), Web3.to_checksum_address(b)])) for a, b in pairs]
        # ETH-native curve pools use 0xEeee
        eth_pairs = [(a, ETH_DENOM.lower()) for a in assets if a != HUB_ASSETS["WETH"].lower()]
        calls += [(curve_meta, sel("find_pools_for_coins(address,address)") + encode(["address", "address"], [Web3.to_checksum_address(a), ETH_DENOM])) for a, _ in eth_pairs]
        res = self.multicall(calls)
        n = 0
        for (ok, ret), (a, b) in zip(res, pairs + eth_pairs):
            if not ok or len(ret) < 64:
                continue
            try:
                pools = decode(["address[]"], ret)[0]
            except Exception:  # noqa: BLE001
                continue
            for p in pools:
                n += 1
                self.add("curve", "pool", p, f"MetaRegistry.find_pools_for_coins({a[:10]},{b[:10]})")
        self.notes.append(f"curve: {n} pool hits from MetaRegistry for tracked pairs (deduped on address)")
        # Kyber Elastic
        calls = [(kyber_factory, sel("getPool(address,address,uint24)") + encode(["address", "address", "uint24"], [Web3.to_checksum_address(a), Web3.to_checksum_address(b), f])) for a, b in pairs for f in KYBER_FEES]
        res = self.multicall(calls)
        n = 0
        for i, (ok, ret) in enumerate(res):
            pool = is_addr_word(ret[:32]) if ok else None
            if pool:
                n += 1
                a, b = pairs[i // len(KYBER_FEES)]
                self.add("kyber-elastic", "pool", pool, f"factory.getPool({a[:10]},{b[:10]},{KYBER_FEES[i % len(KYBER_FEES)]})")
        self.notes.append(f"kyber-elastic: {n} pools")

    def assets_as_entries(self):
        for a, label in sorted(self.assets.items()):
            self.add("asset", "erc20", a, label)


def _sky_dog() -> str:
    for proto, kind, addr, _ in CONSTANTS:
        if proto == "sky-maker" and kind == "dog":
            return addr
    return ""


def _sky_spotter() -> str:
    for proto, kind, addr, _ in CONSTANTS:
        if proto == "sky-maker" and kind == "spotter":
            return addr
    return ""


def _chainlink_feed_registry() -> str:
    for proto, kind, addr, _ in CONSTANTS:
        if kind == "chainlink_feed_registry":
            return addr
    return ""


def _dex_factories() -> tuple[str, str, str]:
    u3 = curve = kyber = ""
    for proto, kind, addr, _ in CONSTANTS:
        if proto == "uniswap-v3" and kind == "factory":
            u3 = addr
        elif proto == "curve" and kind == "meta_registry":
            curve = addr
        elif proto == "kyber-elastic" and kind == "factory":
            kyber = addr
    return u3, curve, kyber


def run_completion(c: Completer, base_entries: list[dict], skip_dex: bool) -> None:
    by = defaultdict(list)
    for e in base_entries:
        by[(e["protocol"], e["kind"])].append(e["address"])

    def step(name, fn, *a):
        t = time.time()
        try:
            fn(*a)
        except Exception as ex:  # noqa: BLE001
            c.failures[f"step:{name}"] = str(ex)[:300]
            print(f"[{name}] FAILED {ex}")
        print(f"[{name}] +{len(c.entries)} new, {time.time() - t:.0f}s", flush=True)

    step("constants", c.constants)
    step("aave-v3/spark oracles", c.aave_v3_like, base_entries)
    step("aave-v4 spoke oracles", c.aave_v4_spoke_oracles, base_entries)
    step("aave-v4 oracles", c.aave_v4, base_entries, 24_500_000)
    if by[("compound-v3", "configurator")]:
        step("compound-v3", c.compound_v3, by[("compound-v3", "configurator")][0], 15_331_586)
    else:
        step("compound-v3", c.compound_v3, ROOTS["compound_v3_configurator"], 15_331_586)
    step(
        "compound-v2",
        c.compound_v2,
        by[("compound-v2", "comptroller")],
        by[("compound-v2", "cToken")],
    )
    step("morpho", c.morpho, ROOTS["morpho_blue"], 18_883_124)
    step("euler", c.euler, by[("euler-v2", "vault")])
    step("liquity-v2", c.liquity_v2, ROOTS["liquity_v2_collateral_registry"], 22_283_450)
    ilk_reg = by[("sky-maker", "ilk_registry")][0] if by[("sky-maker", "ilk_registry")] else ROOTS["ilk_registry"]
    step("sky", c.sky, ilk_reg, _sky_dog(), _sky_spotter())
    cms = by[("gearbox-v3", "credit_manager")]
    if not cms:
        cms = c.gearbox_discover(ROOTS["gearbox_contracts_register"])
    step("gearbox", c.gearbox, cms)
    silos_by_cfg: dict[str, list[str]] = defaultdict(list)
    for e in base_entries:
        if e["protocol"] == "silo-v2" and e["kind"] == "silo":
            m = re.search(r"(0x[0-9a-fA-F]{40})", e.get("source", ""))
            if m:
                silos_by_cfg[m.group(1).lower()].append(e["address"])
    step("silo", c.silo, by[("silo-v2", "silo_config")], silos_by_cfg)
    step("fluid", c.fluid, by[("fluid", "vault")])
    step("ajna", c.ajna)
    step("feed-registry", c.feed_registry, _chainlink_feed_registry())
    if not skip_dex:
        u3, curve, kyber = _dex_factories()
        step("dex-pools", c.dex_pools, u3, curve, kyber)
    c.assets_as_entries()
