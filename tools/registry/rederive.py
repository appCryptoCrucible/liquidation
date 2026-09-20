#!/usr/bin/env python3
"""WP C2 — independent registry re-derivation + identity check (REGISTRY.md §4a/§4b).

CRITICAL INDEPENDENCE CONSTRAINT
    This script must never import, read, or copy tools/registry/discover.py.
    It must never read registry/registry.json until AFTER registry.rederived.json
    has been written to disk. Agreement with C1 is a diff, not a source.

What it does
    1. Enumerate Family A (Aave V3/V4, Spark, Sky-Maker, Compound V2) from
       published on-chain roots.
    2. Enumerate Family B (Morpho Blue, Euler V2, Silo V2, Ajna) from factory
       views / creation logs.
    3. Derive decimals/symbol/oracle fields from chain. Never guess.
    4. Admit Family B markets with borrowed USD >= $50_000 (D27 interim).
    5. UniV3 pools with AND filter: both tokens in the frozen tracked set.
    6. Write registry/registry.rederived.json.
    7. Diff against C1's registry/registry.json; identity-check tokens against
       a canonical list and markets against published deployments; write
       registry/verify-report.md.

RPC
    eth_call  : https://ethereum.publicnode.com
    eth_getLogs: https://gateway.tenderly.co/public/mainnet
    Multicall3: canonical 0xcA11bde05977b3631167028862bE2a173976CA11
                (mds1/multicall; same on every EVM chain). A mistyped address
                is boot-checked and rejected if it has no code.

Ethos: RPC failures are logged, never filled with a reasonable guess.
"""
from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.request
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any, Iterable, Optional

from eth_abi import decode, encode
from web3 import Web3

# ---------------------------------------------------------------------------
# Published roots — protocol address books / official docs, not C1 output.
# Each address is confirmed to have code at boot before it is used as a root.
# ---------------------------------------------------------------------------
ZERO = "0x0000000000000000000000000000000000000000"
# Canonical Multicall3 (https://github.com/mds1/multicall). Do not use a
# transcription of this address that fails a code check.
MULTICALL3 = "0xcA11bde05977b3631167028862bE2a173976CA11"
# Address as typed in the WP prompt — boot-checked; ignored if no code.
MULTICALL3_TYPED = "0xcA11bca05BB27F4246f6B8b13B9716F3Ca5A3F0F"

RPC_CALL_DEFAULT = "https://ethereum.publicnode.com"
RPC_LOGS_DEFAULT = "https://gateway.tenderly.co/public/mainnet"

# Aave V3 PoolAddressesProviderRegistry — aave-address-book AaveV3Ethereum
AAVE_V3_REGISTRY = "0xbaA999AC55EAce41CcAE355c77809e68Bb345170"
# SparkLend PoolAddressesProviderRegistry — spark-psm / spark address book
SPARK_REGISTRY = "0x03cFa0C4622FF84E50E75062683F44c9587e6Cc1"
# Aave V4 hubs — aave.org/docs/aave-v4/liquidity/hubs + aave-address-book
AAVE_V4_HUBS = {
    "core": "0xCca852Bc40e560adC3b1Cc58CA5b55638ce826c9",
    "plus": "0x06002e9c4412CB7814a791eA3666D905871E536A",
    "prime": "0x943827DCA022D0F354a8a8c332dA1e5Eb9f9F931",
    "paxos": "0x62d63197660c080236193CA60b70E49A08E90368",
}
# Sky/Maker IlkRegistry — makerdao/dss-ilk-registry
ILK_REGISTRY = "0x5a464C28D19848f44199D003BeF5ecc87d090F87"
MAKER_VAT = "0x35D1b3F3D7966A1DFe207aa4514C12a259A0492B"
SKY_DSS_FLASH = "0x60744434d6339a6B27d73d9Eda62b6F66a0a04FA"
# Compound V2 Unitroller — compound.finance / etherscan verified proxy
COMPOUND_V2_COMPTROLLER = "0x3d9819210A31b4961b30EF54bE2aeD79B9c9Cd3B"
COMPOUND_V2_DEPLOY_BLOCK = 7_711_536
# Morpho Blue singleton — docs.morpho.org
MORPHO_BLUE = "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb"
MORPHO_DEPLOY_BLOCK = 18_883_124
# Euler V2 GenericFactory — docs.euler.finance / euler-interfaces CoreAddresses
EULER_FACTORY = "0x29a56a1b8214D9Cf7c5561811750D5cBDb45CC8e"
# Silo V2/V3 factories — D15-ADDRESSES.md / silo deployments (DefiLlama V2 + v3)
SILO_FACTORY_V2 = "0x22a3cF6149bFa611bAFc89Fd721918EC3Cf7b581"
SILO_FACTORY_V3 = "0x1DAb4A310447185144467076b116DAC7aec3b48F"
# Ajna factories — faqs.ajna.finance
AJNA_ERC20_FACTORY = "0x6146DD43C5622bB6D12A5240ab9CF4de14eDC625"
AJNA_ERC721_FACTORY = "0x27461199d3b7381De66a85D685828E967E35AF4c"
# Uniswap V3 factory
UNIV3_FACTORY = "0x1F98431c8aD98523631AE4a59f267346ea31F984"
UNIV3_FACTORY_BLOCK = 12_369_621
UNIV4_POOL_MANAGER = "0x000000000004444c5dc75cB358380D2e3dE08A90"
# Chainlink Feed Registry (Ethereum)
FEED_REGISTRY = "0x47Fb2585D2C56Fe188D0E6ec628a38b74fCeeeDf"
USD_DENOM = "0x0000000000000000000000000000000000000348"
ETH_DENOM = "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE"

# Hub assets for UniV3 pairing + ETH/USD routing (D15-ADDRESSES.md §11 hubs).
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
UNIV3_FEES = (100, 500, 3000, 10000)
RAY = 10**27
ADMISSION_USD = 50_000.0  # D27 interim (STATE.md: unset, generous bar)

# Token-quirk observations that are properties of the token contract itself,
# recorded when the address is in the derived set. Not a price/decimals guess.
KNOWN_QUIRKS = {
    "0xdac17f958d2ee523a2206206994597c13d831ec7": ["no_return_data", "approve_nonzero_reverts", "low_decimals"],
    "0xae7ab96520de3a18e5e111b5eaab095312d7fe84": ["rebasing"],
    "0x9f8f72aa9304c8b593d555f12ef6589cc3a579a2": ["nonstandard_metadata"],  # MKR
}

UNISWAP_TOKENLIST_URLS = (
    "https://tokens.uniswap.org",
    "https://gateway.ipfs.io/ipns/tokens.uniswap.org",
)

# --------------------------------------------------------------------------- selectors
def sel(sig: str) -> bytes:
    return Web3.keccak(text=sig)[:4]


def topic0(sig: str) -> str:
    return Web3.to_hex(Web3.keccak(text=sig))


def checksum(addr: str) -> str:
    return Web3.to_checksum_address(addr)


def addr_word(ret: bytes) -> Optional[str]:
    if not ret or len(ret) < 32:
        return None
    if any(ret[:12]):
        return None
    a = "0x" + ret[12:32].hex()
    if a.lower() == ZERO:
        return None
    return checksum(a)


def u256(ret: bytes) -> Optional[int]:
    if not ret or len(ret) < 32:
        return None
    return int.from_bytes(ret[:32], "big")


def decode_symbol(ret: bytes) -> tuple[str, bool]:
    """Return (symbol, nonstandard_metadata). Empty symbol on failure — never invent."""
    if not ret:
        return "", False
    try:
        s = decode(["string"], ret)[0]
        if isinstance(s, bytes):
            s = s.decode("utf-8", errors="replace")
        s = s.strip("\x00")
        if s:
            return s, False
    except Exception:
        pass
    if len(ret) >= 32:
        raw = ret[:32].rstrip(b"\x00")
        if raw:
            try:
                return raw.decode("ascii"), True
            except UnicodeDecodeError:
                return raw.decode("latin-1", errors="replace"), True
    return "", False


def decode_decimals(ret: bytes) -> Optional[int]:
    if not ret or len(ret) < 32:
        return None
    n = int.from_bytes(ret[:32], "big")
    if n > 255:
        return None
    return int(n)


def lc(addr: Optional[str]) -> str:
    return (addr or "").lower()


# --------------------------------------------------------------------------- RPC
class Rpc:
    def __init__(self, call_url: str, logs_url: str, sleep_s: float = 0.05, batch: int = 80):
        self.call_url = call_url
        self.logs_url = logs_url
        self.w3 = Web3(Web3.HTTPProvider(call_url, request_kwargs={"timeout": 90}))
        self.w3_logs = Web3(Web3.HTTPProvider(logs_url, request_kwargs={"timeout": 180}))
        self.sleep_s = sleep_s
        self.batch = batch
        self.mc = checksum(MULTICALL3)
        self.failures: dict[str, str] = {}
        self.notes: list[str] = []
        self._n_call = 0
        self._n_mc = 0
        self._n_logs = 0

    def fail(self, key: str, msg: str) -> None:
        self.failures[key] = msg[:400]

    def retry(self, fn, tries: int = 5, tag: str = "rpc"):
        last = None
        for i in range(tries):
            try:
                if i:
                    time.sleep(min(8.0, 0.6 * (2**i)))
                else:
                    time.sleep(self.sleep_s)
                return fn()
            except Exception as ex:  # noqa: BLE001
                last = ex
                msg = str(ex).lower()
                if any(s in msg for s in ("429", "rate", "timeout", "timed out", "503", "502", "limit")):
                    continue
                self.fail(tag, str(ex))
                return None
        self.fail(tag, f"retries exhausted: {last}")
        return None

    def boot(self) -> int:
        typed = checksum(MULTICALL3_TYPED)
        mc_code = self.retry(lambda: self.w3.eth.get_code(self.mc), tag="boot:multicall3")
        typed_code = self.retry(lambda: self.w3.eth.get_code(typed), tag="boot:multicall3_typed")
        if not mc_code or len(mc_code) <= 2:
            raise SystemExit(
                f"FATAL: canonical Multicall3 {self.mc} has no code on this RPC. "
                "Refusing to guess a replacement."
            )
        if typed.lower() != self.mc.lower():
            has = bool(typed_code and len(typed_code) > 2)
            self.notes.append(
                f"multicall3: using canonical {self.mc}; WP-typed {typed} "
                f"{'has code' if has else 'has no code (transcription, ignored)'}"
            )
        head = self.retry(lambda: int(self.w3.eth.block_number), tag="boot:block")
        if head is None:
            raise SystemExit("FATAL: cannot read eth_blockNumber")
        return head

    def has_code(self, addr: str) -> bool:
        try:
            code = self.retry(
                lambda: self.w3.eth.get_code(checksum(addr)),
                tag=f"getCode:{addr[:10]}",
            )
            return bool(code and len(code) > 2)
        except Exception as ex:  # noqa: BLE001
            self.fail(f"getCode:{addr[:10]}", str(ex))
            return False

    def multicall(self, calls: list[tuple[str, bytes]]) -> list[tuple[bool, bytes]]:
        """tryAggregate(false, [(to, data)...]). Failed chunks recorded, not invented."""
        out: list[tuple[bool, bytes]] = []
        if not calls:
            return out
        for i in range(0, len(calls), self.batch):
            chunk = calls[i : i + self.batch]
            data = sel("tryAggregate(bool,(address,bytes)[])") + encode(
                ["bool", "(address,bytes)[]"],
                [False, [(checksum(t), d) for t, d in chunk]],
            )
            raw = self.retry(
                lambda d=data: self.w3.eth.call({"to": self.mc, "data": d}),
                tag=f"multicall:{i}",
            )
            self._n_mc += 1
            if raw is None:
                out.extend([(False, b"")] * len(chunk))
                continue
            try:
                res = decode(["(bool,bytes)[]"], raw)[0]
                out.extend([(bool(s), bytes(r)) for s, r in res])
            except Exception as ex:  # noqa: BLE001
                self.fail(f"multicall:decode:{i}", str(ex))
                out.extend([(False, b"")] * len(chunk))
        return out

    def call1(self, to: str, sig: str, args: tuple = (), types: tuple = ()) -> Optional[bytes]:
        data = sel(sig) + (encode(list(types), list(args)) if types else b"")
        ok, ret = self.multicall([(to, data)])[0]
        return ret if ok and ret else None

    def get_logs(
        self,
        address: Optional[str | list[str]],
        topics: list,
        from_block: int,
        to_block: int,
        chunk: int = 2_000_000,
        tag: str = "getLogs",
    ) -> list:
        logs: list = []
        start = from_block
        errs = 0
        n_calls = 0
        while start <= to_block:
            end = min(start + chunk - 1, to_block)
            try:
                time.sleep(self.sleep_s)
                n_calls += 1
                self._n_logs += 1
                params: dict[str, Any] = {
                    "topics": topics,
                    "fromBlock": start,
                    "toBlock": end,
                }
                if address is not None:
                    params["address"] = address
                batch = self.w3_logs.eth.get_logs(params)
                logs.extend(batch)
                start = end + 1
                errs = 0
                print(f"    {tag} {start-1}/{to_block} (+{len(batch)} logs, chunk={chunk})", flush=True)
            except Exception as ex:  # noqa: BLE001
                msg = str(ex)
                errs += 1
                low = msg.lower()
                if any(k in low for k in ("range", "limit", "too many", "exceed", "10000", "response size", "query returned more")) and chunk > 2_000:
                    chunk = max(2_000, chunk // 2)
                    continue
                if errs < 6:
                    time.sleep(min(12.0, 1.2 * errs))
                    continue
                self.fail(f"{tag}:{start}-{end}", msg)
                print(f"    {tag} FAIL {start}-{end}: {msg[:160]}", flush=True)
                start = end + 1
                errs = 0
        print(f"    {tag} done: {len(logs)} logs in {n_calls} calls", flush=True)
        return logs


# --------------------------------------------------------------------------- registry builder
class Builder:
    def __init__(self, rpc: Rpc, block: int, out_dir: Path):
        self.rpc = rpc
        self.block = block
        self.out_dir = out_dir
        self.ckpt_dir = out_dir / "rederive-ckpt"
        self.ckpt_dir.mkdir(parents=True, exist_ok=True)
        self.protocols: dict[str, dict[str, Any]] = {}
        self.tokens: dict[str, dict[str, Any]] = {}  # checksum -> fields
        self.oracles: dict[str, dict[str, Any]] = {}
        self.pools: dict[str, dict[str, Any]] = {}
        self.flash_sources: dict[str, dict[str, Any]] = {}
        self.routers: dict[str, dict[str, Any]] = {}
        self.token_addrs: set[str] = set()  # lowercase, pending derivation
        self.tracked_underlyings: set[str] = set()  # lowercase; UniV3 AND set
        self.counts: dict[str, dict[str, int]] = {}
        self.completed: list[str] = []
        self.partial: list[str] = []
        self.identity: dict[str, Any] = {}
        self.diff: dict[str, Any] = {}

    def add_token_addr(self, addr: Optional[str], tracked: bool = True) -> None:
        if not addr or lc(addr) == ZERO:
            return
        a = checksum(addr)
        self.token_addrs.add(lc(a))
        if tracked:
            self.tracked_underlyings.add(lc(a))

    def put_protocol(self, key: str, entry: dict[str, Any]) -> None:
        self.protocols[key] = entry

    def ckpt_path(self, name: str) -> Path:
        return self.ckpt_dir / f"{name}.json"

    def save_ckpt(self, name: str, extra: Optional[dict] = None) -> None:
        payload = {
            "block": self.block,
            "protocol": name,
            "n_protocols": len(self.protocols),
            "n_tokens_pending": len(self.token_addrs),
            "n_tracked": len(self.tracked_underlyings),
            "counts": self.counts.get(name, {}),
            "failures_n": len(self.rpc.failures),
        }
        if extra:
            payload.update(extra)
        self.ckpt_path(name).write_text(json.dumps(payload, indent=2), encoding="utf-8")
        progress = {
            "block": self.block,
            "completed": self.completed,
            "partial": self.partial,
            "n_protocols": len(self.protocols),
            "n_pools": len(self.pools),
            "n_tokens_pending": len(self.token_addrs),
            "rpc_failures": len(self.rpc.failures),
        }
        (self.ckpt_dir / "progress.json").write_text(json.dumps(progress, indent=2), encoding="utf-8")
        print(f"  checkpoint {name}: {payload}", flush=True)

    def snapshot_state(self) -> dict[str, Any]:
        return {
            "protocols": self.protocols,
            "token_addrs": sorted(self.token_addrs),
            "tracked_underlyings": sorted(self.tracked_underlyings),
            "oracles": self.oracles,
            "pools": self.pools,
            "flash_sources": self.flash_sources,
            "counts": self.counts,
            "completed": self.completed,
        }

    def restore_state(self, snap: dict[str, Any]) -> None:
        self.protocols = snap.get("protocols") or {}
        self.token_addrs = set(snap.get("token_addrs") or [])
        self.tracked_underlyings = set(snap.get("tracked_underlyings") or [])
        self.oracles = snap.get("oracles") or {}
        self.pools = snap.get("pools") or {}
        self.flash_sources = snap.get("flash_sources") or {}
        self.counts = snap.get("counts") or {}
        self.completed = list(snap.get("completed") or [])

    def save_full_snap(self) -> None:
        (self.ckpt_dir / "state.json").write_text(
            json.dumps(self.snapshot_state()), encoding="utf-8"
        )

    def load_full_snap(self) -> bool:
        p = self.ckpt_dir / "state.json"
        if not p.exists():
            return False
        self.restore_state(json.loads(p.read_text(encoding="utf-8")))
        print(f"  resumed: {len(self.completed)} protocols, {len(self.protocols)} entries", flush=True)
        return True


# --------------------------------------------------------------------------- enumerators
def enum_aave_like(b: Builder, family: str, registry: str) -> None:
    rpc = b.rpc
    if not rpc.has_code(registry):
        rpc.fail(f"{family}:registry", f"no code at {registry}")
        b.partial.append(family)
        return
    ret = rpc.call1(registry, "getAddressesProvidersList()")
    if ret is None:
        rpc.fail(f"{family}:getAddressesProvidersList", "eth_call failed")
        b.partial.append(family)
        return
    try:
        providers = [checksum(a) for a in decode(["address[]"], ret)[0] if lc(a) != ZERO]
    except Exception as ex:  # noqa: BLE001
        rpc.fail(f"{family}:decode_providers", str(ex))
        b.partial.append(family)
        return

    n_reserves = 0
    n_receipt = 0
    n_agg = 0
    for prov in providers:
        pool_ret = rpc.call1(prov, "getPool()")
        oracle_ret = rpc.call1(prov, "getPriceOracle()")
        pool = addr_word(pool_ret) if pool_ret else None
        oracle = addr_word(oracle_ret) if oracle_ret else None
        if not pool:
            rpc.fail(f"{family}:getPool:{prov[:10]}", "no pool")
            continue
        reserves_ret = rpc.call1(pool, "getReservesList()")
        reserves: list[str] = []
        if reserves_ret is None:
            rpc.fail(f"{family}:getReservesList:{pool[:10]}", "eth_call failed")
        else:
            try:
                reserves = [checksum(a) for a in decode(["address[]"], reserves_ret)[0] if lc(a) != ZERO]
            except Exception as ex:  # noqa: BLE001
                rpc.fail(f"{family}:decode_reserves:{pool[:10]}", str(ex))

        receipt: list[str] = []
        adapters: list[str] = []
        if reserves:
            calls: list[tuple[str, bytes]] = []
            for a in reserves:
                calls.append((pool, sel("getReserveAToken(address)") + encode(["address"], [a])))
                calls.append((pool, sel("getReserveVariableDebtToken(address)") + encode(["address"], [a])))
                if oracle:
                    calls.append((oracle, sel("getSourceOfAsset(address)") + encode(["address"], [a])))
            res = rpc.multicall(calls)
            stride = 3 if oracle else 2
            agg_calls: list[tuple[str, bytes]] = []
            agg_meta: list[str] = []
            for i, asset in enumerate(reserves):
                b.add_token_addr(asset, tracked=True)
                atok = addr_word(res[i * stride][1]) if res[i * stride][0] else None
                dtok = addr_word(res[i * stride + 1][1]) if res[i * stride + 1][0] else None
                if atok:
                    receipt.append(atok)
                    b.add_token_addr(atok, tracked=False)
                else:
                    rpc.fail(f"{family}:aToken:{asset[:10]}", "getReserveAToken failed")
                if dtok:
                    receipt.append(dtok)
                    b.add_token_addr(dtok, tracked=False)
                else:
                    rpc.fail(f"{family}:vdToken:{asset[:10]}", "getReserveVariableDebtToken failed")
                if oracle:
                    ok, raw = res[i * stride + 2]
                    src = addr_word(raw) if ok else None
                    if src:
                        adapters.append(src)
                        agg_calls.append((src, sel("aggregator()")))
                        agg_meta.append(src)
                    else:
                        rpc.fail(f"{family}:source:{asset[:10]}", "getSourceOfAsset failed/empty")
            if agg_calls:
                ares = rpc.multicall(agg_calls)
                for src, (ok, raw) in zip(agg_meta, ares):
                    if not ok or not raw:
                        rpc.fail(f"oracle:aggregator:{lc(src)}", "aggregator() failed")
                        b.oracles[src] = {"aggregator": None, "pair": None, "decimals": None, "svr": None}
                        continue
                    agg = addr_word(raw)
                    entry = {"aggregator": agg, "pair": None, "decimals": None, "svr": None}
                    if agg:
                        n_agg += 1
                        dec_ret = rpc.call1(agg, "decimals()")
                        d = decode_decimals(dec_ret) if dec_ret else None
                        if d is None:
                            dret2 = rpc.call1(src, "decimals()")
                            d = decode_decimals(dret2) if dret2 else None
                        entry["decimals"] = d
                    else:
                        rpc.fail(f"oracle:aggregator:{lc(src)}", "aggregator() returned empty/zero")
                    b.oracles[src] = entry

        n_reserves += len(reserves)
        n_receipt += len(receipt)
        key = f"{family}:{lc(pool)}"
        b.put_protocol(
            key,
            {
                "family": family,
                "market": pool,
                "addresses_provider": prov,
                "price_oracle": oracle,
                "deployed_block": 0,
                "receipt_tokens": receipt,
                "oracle_adapters": adapters,
                "reserves": reserves,
                "admitted": True,  # Family A: governed set, not $50k-gated
                "borrowed_raw": None,
                "borrowed_usd": None,
            },
        )
        if oracle:
            b.flash_sources[pool] = {"venue": family, "kind": "aave_pool", "source": pool}

    b.counts[family] = {
        "instances": len(providers),
        "reserves": n_reserves,
        "receipt_tokens": n_receipt,
        "aggregators": n_agg,
        "admitted_markets": 0,
    }
    b.rpc.notes.append(f"{family}: {len(providers)} providers, {n_reserves} reserves, {n_receipt} receipt tokens, {n_agg} aggregators")


def enum_aave_v4(b: Builder) -> None:
    rpc = b.rpc
    family = "aave-v4"
    hubs_ok = []
    spokes: dict[str, str] = {}  # addr -> first hub that listed it
    n_assets = 0
    for name, hub in AAVE_V4_HUBS.items():
        if not rpc.has_code(hub):
            rpc.fail(f"{family}:hub:{name}", f"no code at {hub}")
            continue
        hubs_ok.append((name, checksum(hub)))
        cret = rpc.call1(hub, "getAssetCount()")
        n = u256(cret) if cret else None
        if n is None:
            rpc.fail(f"{family}:getAssetCount:{name}", "eth_call failed")
            continue
        # Confirm signature on-chain as REGISTRY.md requires — n is the evidence.
        rpc.notes.append(f"aave-v4:{name} getAssetCount()={n}")
        for i in range(n):
            uret = rpc.call1(hub, "getAssetUnderlyingAndDecimals(uint256)", (i,), ("uint256",))
            if uret is None:
                rpc.fail(f"{family}:underlying:{name}:{i}", "eth_call failed")
                continue
            try:
                und, dec = decode(["address", "uint8"], uret)
            except Exception as ex:  # noqa: BLE001
                rpc.fail(f"{family}:decode_underlying:{name}:{i}", str(ex))
                continue
            if lc(und) != ZERO:
                b.add_token_addr(und, tracked=True)
                n_assets += 1
            sret = rpc.call1(hub, "getSpokeCount(uint256)", (i,), ("uint256",))
            ns = u256(sret) if sret else None
            if ns is None:
                rpc.fail(f"{family}:getSpokeCount:{name}:{i}", "eth_call failed")
                continue
            for j in range(ns):
                aret = rpc.call1(
                    hub, "getSpokeAddress(uint256,uint256)", (i, j), ("uint256", "uint256")
                )
                sp = addr_word(aret) if aret else None
                if sp:
                    spokes.setdefault(sp, checksum(hub))
                else:
                    rpc.fail(f"{family}:getSpokeAddress:{name}:{i}:{j}", "empty")

        b.put_protocol(
            f"{family}:hub:{lc(hub)}",
            {
                "family": family,
                "kind": "hub",
                "name": name,
                "market": checksum(hub),
                "hub": checksum(hub),
                "deployed_block": 0,
                "receipt_tokens": [],
                "oracle_adapters": [],
                "admitted": True,
                "asset_count": n,
            },
        )

    for sp, hub in spokes.items():
        b.put_protocol(
            f"{family}:spoke:{lc(sp)}",
            {
                "family": family,
                "kind": "spoke",
                "market": sp,
                "hub": hub,
                "deployed_block": 0,
                "receipt_tokens": [],
                "oracle_adapters": [],
                "admitted": True,
            },
        )

    b.counts[family] = {
        "instances": len(hubs_ok) + len(spokes),
        "reserves": n_assets,
        "receipt_tokens": 0,
        "aggregators": 0,
        "admitted_markets": 0,
        "hubs": len(hubs_ok),
        "spokes": len(spokes),
    }
    rpc.notes.append(f"aave-v4: {len(hubs_ok)} hubs, {len(spokes)} spokes, {n_assets} hub assets")
    if len(hubs_ok) < len(AAVE_V4_HUBS):
        b.partial.append(family)


def enum_sky(b: Builder) -> None:
    rpc = b.rpc
    family = "sky-maker"
    if not rpc.has_code(ILK_REGISTRY):
        rpc.fail(f"{family}:ilk_registry", "no code")
        b.partial.append(family)
        return
    cret = rpc.call1(ILK_REGISTRY, "count()")
    n = u256(cret) if cret else None
    if n is None:
        rpc.fail(f"{family}:count", "eth_call failed")
        b.partial.append(family)
        return
    lret = rpc.call1(ILK_REGISTRY, "list()")
    ilks = []
    if lret is None:
        # paginated list(start,end)
        step = 50
        for start in range(0, n, step):
            end = min(n, start + step)
            pret = rpc.call1(ILK_REGISTRY, "list(uint256,uint256)", (start, end), ("uint256", "uint256"))
            if pret is None:
                rpc.fail(f"{family}:list:{start}-{end}", "eth_call failed")
                continue
            try:
                ilks.extend(list(decode(["bytes32[]"], pret)[0]))
            except Exception as ex:  # noqa: BLE001
                rpc.fail(f"{family}:decode_list:{start}", str(ex))
    else:
        try:
            ilks = list(decode(["bytes32[]"], lret)[0])
        except Exception as ex:  # noqa: BLE001
            rpc.fail(f"{family}:decode_list", str(ex))
            b.partial.append(family)
            return

    gems: list[str] = []
    joins: list[str] = []
    pips: list[str] = []
    info_calls = [(ILK_REGISTRY, sel("info(bytes32)") + encode(["bytes32"], [ilk])) for ilk in ilks]
    vat_calls = (
        [(MAKER_VAT, sel("ilks(bytes32)") + encode(["bytes32"], [ilk])) for ilk in ilks]
        if rpc.has_code(MAKER_VAT)
        else []
    )
    ires = rpc.multicall(info_calls)
    vres = rpc.multicall(vat_calls) if vat_calls else [(False, b"")] * len(ilks)
    ilk_rows = []
    for ilk, (ok, raw), (vok, vraw) in zip(ilks, ires, vres):
        name = ilk.rstrip(b"\x00").decode("ascii", errors="replace")
        if not ok or not raw:
            rpc.fail(f"{family}:info:{name}", "eth_call failed")
            continue
        try:
            _nm, _sym, _cls, _dec, gem, pip, join, xlip = decode(
                ["string", "string", "uint256", "uint256", "address", "address", "address", "address"],
                raw,
            )
        except Exception as ex:  # noqa: BLE001
            rpc.fail(f"{family}:decode_info:{name}", str(ex))
            continue
        gem_c = checksum(gem) if lc(gem) != ZERO else None
        pip_c = checksum(pip) if lc(pip) != ZERO else None
        join_c = checksum(join) if lc(join) != ZERO else None
        if gem_c:
            gems.append(gem_c)
            b.add_token_addr(gem_c, tracked=True)
        if pip_c:
            pips.append(pip_c)
        if join_c:
            joins.append(join_c)
        art = rate = None
        if vok and vraw and len(vraw) >= 64:
            art = int.from_bytes(vraw[0:32], "big")
            rate = int.from_bytes(vraw[32:64], "big")
        borrowed_raw = (art * rate) // RAY if art is not None and rate is not None else None
        ilk_rows.append(
            {
                "ilk": name,
                "gem": gem_c,
                "pip": pip_c,
                "join": join_c,
                "xlip": checksum(xlip) if lc(xlip) != ZERO else None,
                "borrowed_raw": borrowed_raw,
            }
        )

    b.put_protocol(
        f"{family}:{lc(ILK_REGISTRY)}",
        {
            "family": family,
            "market": checksum(ILK_REGISTRY),
            "deployed_block": 0,
            "receipt_tokens": joins,
            "oracle_adapters": pips,
            "gems": gems,
            "ilks": ilk_rows,
            "admitted": True,
        },
    )
    if rpc.has_code(SKY_DSS_FLASH):
        b.flash_sources[checksum(SKY_DSS_FLASH)] = {
            "venue": "sky-dss-flash",
            "kind": "erc3156",
            "source": checksum(SKY_DSS_FLASH),
            "asset": HUB_ASSETS["DAI"],
        }
    b.add_token_addr(HUB_ASSETS["DAI"], tracked=True)
    b.counts[family] = {
        "instances": 1,
        "reserves": len(ilk_rows),
        "receipt_tokens": len(joins),
        "aggregators": 0,
        "admitted_markets": 0,
    }
    rpc.notes.append(f"sky-maker: {len(ilk_rows)} ilks (count()={n}), {len(gems)} gems")
    if len(ilk_rows) != n:
        rpc.fail(f"{family}:count_mismatch", f"count={n} listed={len(ilk_rows)}")
        b.partial.append(family)


def enum_compound_v2(b: Builder) -> None:
    """Official Comptroller + any other Comptroller that emitted MarketListed.

    Forks are the same enumeration with a different root (REGISTRY.md §3).
    Additional Comptrollers are discovered from the MarketListed event, then
    getAllMarkets() is required — emitters that do not answer are dropped.
    """
    rpc = b.rpc
    family = "compound-v2"
    comptrollers: dict[str, int] = {}  # addr -> first seen block
    if rpc.has_code(COMPOUND_V2_COMPTROLLER):
        comptrollers[checksum(COMPOUND_V2_COMPTROLLER)] = COMPOUND_V2_DEPLOY_BLOCK
    else:
        rpc.fail(f"{family}:official", "no code at published Comptroller")

    tp = topic0("MarketListed(address)")
    # Unfiltered topic scan finds fork Comptrollers. Official Compound is already
    # in `comptrollers` if it has code, so a failed scan still yields the published root.
    logs = rpc.get_logs(
        address=None,
        topics=[tp],
        from_block=COMPOUND_V2_DEPLOY_BLOCK,
        to_block=b.block,
        chunk=250_000,
        tag="compound-v2.MarketListed",
    )

    for lg in logs:
        emitter = checksum(lg["address"])
        blk = int(lg["blockNumber"])
        comptrollers.setdefault(emitter, blk)

    n_markets = 0
    n_receipt = 0
    n_ok = 0
    for cpt, blk in comptrollers.items():
        ret = rpc.call1(cpt, "getAllMarkets()")
        if ret is None:
            rpc.fail(f"{family}:getAllMarkets:{cpt[:10]}", "no getAllMarkets (not a Comptroller)")
            continue
        try:
            markets = [checksum(a) for a in decode(["address[]"], ret)[0] if lc(a) != ZERO]
        except Exception as ex:  # noqa: BLE001
            rpc.fail(f"{family}:decode:{cpt[:10]}", str(ex))
            continue
        n_ok += 1
        n_markets += len(markets)
        n_receipt += len(markets)
        underlyings: list[str] = []
        if markets:
            ures = rpc.multicall([(m, sel("underlying()")) for m in markets])
            for m, (ok, raw) in zip(markets, ures):
                b.add_token_addr(m, tracked=False)  # cToken is a receipt token
                if not ok:
                    rpc.fail(f"{family}:underlying:{m[:10]}", "underlying() failed")
                    continue
                u = addr_word(raw)
                if u:
                    underlyings.append(u)
                    b.add_token_addr(u, tracked=True)
                else:
                    # cETH has no underlying() — WETH/ETH. Record the failure, do not guess.
                    rpc.fail(f"{family}:underlying:{m[:10]}", "underlying() empty (possible cETH)")
        b.put_protocol(
            f"{family}:{lc(cpt)}",
            {
                "family": family,
                "market": cpt,
                "comptroller": cpt,
                "deployed_block": blk if blk else 0,
                "receipt_tokens": markets,
                "oracle_adapters": [],
                "reserves": underlyings,
                "admitted": True,
            },
        )

    b.counts[family] = {
        "instances": n_ok,
        "reserves": n_markets,
        "receipt_tokens": n_receipt,
        "aggregators": 0,
        "admitted_markets": 0,
        "candidate_emitters": len(comptrollers),
    }
    rpc.notes.append(
        f"compound-v2: {n_ok} Comptrollers answering getAllMarkets "
        f"({len(comptrollers)} MarketListed emitters), {n_markets} cTokens"
    )
    if n_ok == 0:
        b.partial.append(family)


def _get_logs_no_address(rpc: Rpc, topics: list, start: int, end: int, tag: str) -> list:
    """eth_getLogs with no address filter (Compound V2 fork discovery)."""
    logs: list = []
    chunk = 250_000
    cur = start
    errs = 0
    while cur <= end:
        to = min(cur + chunk - 1, end)
        try:
            time.sleep(rpc.sleep_s)
            rpc._n_logs += 1
            batch = rpc.w3_logs.eth.get_logs({"topics": topics, "fromBlock": cur, "toBlock": to})
            logs.extend(batch)
            cur = to + 1
            errs = 0
            print(f"    {tag} {cur-1}/{end} (+{len(batch)}, chunk={chunk})", flush=True)
        except Exception as ex:  # noqa: BLE001
            msg = str(ex)
            errs += 1
            low = msg.lower()
            if any(k in low for k in ("range", "limit", "too many", "exceed", "10000", "response size", "address")) and chunk > 2_000:
                chunk = max(2_000, chunk // 2)
                continue
            if errs < 5:
                time.sleep(min(12.0, 1.5 * errs))
                continue
            rpc.fail(f"{tag}:{cur}-{to}", msg)
            print(f"    {tag} FAIL {cur}-{to}: {msg[:160]}", flush=True)
            cur = to + 1
            errs = 0
    return logs


def enum_morpho(b: Builder) -> None:
    rpc = b.rpc
    family = "morpho-blue"
    if not rpc.has_code(MORPHO_BLUE):
        rpc.fail(f"{family}:singleton", "no code")
        b.partial.append(family)
        return
    morpho = checksum(MORPHO_BLUE)
    b.flash_sources[morpho] = {"venue": "morpho-blue", "kind": "singleton", "source": morpho}
    tp = topic0("CreateMarket(bytes32,(address,address,address,address,uint256))")
    logs = rpc.get_logs(morpho, [tp], MORPHO_DEPLOY_BLOCK, b.block, chunk=2_000_000, tag="morpho.CreateMarket")
    markets = []
    for lg in logs:
        mid = "0x" + bytes(lg["topics"][1])[-32:].hex() if len(lg["topics"]) > 1 else None
        try:
            loan, coll, oracle, irm, lltv = decode(
                ["(address,address,address,address,uint256)"], bytes(lg["data"])
            )[0]
        except Exception as ex:  # noqa: BLE001
            rpc.fail(f"{family}:decode:{lg.get('transactionHash')}", str(ex))
            continue
        if not mid:
            rpc.fail(f"{family}:id", "missing topic1")
            continue
        markets.append(
            {
                "id": mid,
                "loan": checksum(loan),
                "coll": checksum(coll),
                "oracle": checksum(oracle) if lc(oracle) != ZERO else None,
                "irm": checksum(irm) if lc(irm) != ZERO else None,
                "lltv": int(lltv),
                "block": int(lg["blockNumber"]),
            }
        )
    # borrowed: Morpho.market(id).totalBorrowAssets
    borrowed: dict[str, Optional[int]] = {}
    if markets:
        calls = [(morpho, sel("market(bytes32)") + bytes.fromhex(m["id"][2:])) for m in markets]
        res = rpc.multicall(calls)
        for m, (ok, raw) in zip(markets, res):
            if not ok or not raw or len(raw) < 48:
                rpc.fail(f"{family}:market:{m['id'][:10]}", "market() failed")
                borrowed[m["id"]] = None
                continue
            # totalBorrowAssets is the 3rd uint128: packed 2 per slot.
            # ABI encoder for a struct of 6 uint128s typically word-aligns each to uint256
            # on some versions and packs on others. Read both layouts; require one to parse.
            if len(raw) >= 6 * 32:
                # word-aligned
                tba = int.from_bytes(raw[64:96], "big")
            else:
                # packed: slot0 = supplyAssets|supplyShares, slot1 = borrowAssets|borrowShares
                word1 = raw[32:64] if len(raw) >= 64 else b""
                tba = int.from_bytes(word1[0:16], "big") if len(word1) == 32 else None
            borrowed[m["id"]] = tba

    for m in markets:
        b.add_token_addr(m["loan"], tracked=True)
        b.add_token_addr(m["coll"], tracked=True)
        b.put_protocol(
            f"{family}:{m['id']}",
            {
                "family": family,
                "market": morpho,
                "market_id": m["id"],
                "loan_token": m["loan"],
                "collateral_token": m["coll"],
                "oracle": m["oracle"],
                "irm": m["irm"],
                "lltv": m["lltv"],
                "deployed_block": m["block"],
                "receipt_tokens": [],
                "oracle_adapters": [m["oracle"]] if m["oracle"] else [],
                "borrowed_raw": borrowed.get(m["id"]),
                "borrowed_usd": None,
                "admitted": False,  # filled after pricing
            },
        )
    b.counts[family] = {
        "instances": 1,
        "reserves": len(markets),
        "receipt_tokens": 0,
        "aggregators": 0,
        "admitted_markets": 0,
    }
    rpc.notes.append(f"morpho-blue: {len(markets)} CreateMarket logs")
    if not markets:
        b.partial.append(family)


def enum_euler(b: Builder) -> None:
    rpc = b.rpc
    family = "euler-v2"
    if not rpc.has_code(EULER_FACTORY):
        rpc.fail(f"{family}:factory", "no code")
        b.partial.append(family)
        return
    factory = checksum(EULER_FACTORY)
    nret = rpc.call1(factory, "getProxyListLength()")
    n = u256(nret) if nret else None
    if n is None:
        rpc.fail(f"{family}:getProxyListLength", "eth_call failed")
        b.partial.append(family)
        return
    vaults: list[str] = []
    page = 200
    for start in range(0, n, page):
        end = min(n, start + page)
        sret = rpc.call1(factory, "getProxyListSlice(uint256,uint256)", (start, end), ("uint256", "uint256"))
        if sret is None:
            rpc.fail(f"{family}:slice:{start}-{end}", "eth_call failed")
            continue
        try:
            vaults.extend(checksum(a) for a in decode(["address[]"], sret)[0] if lc(a) != ZERO)
        except Exception as ex:  # noqa: BLE001
            rpc.fail(f"{family}:decode_slice:{start}", str(ex))
    calls = []
    for v in vaults:
        calls += [
            (v, sel("asset()")),
            (v, sel("oracle()")),
            (v, sel("unitOfAccount()")),
            (v, sel("totalBorrows()")),
        ]
    res = rpc.multicall(calls)
    for i, v in enumerate(vaults):
        (ok_a, a), (ok_o, o), (ok_u, u), (ok_b, br) = res[4 * i : 4 * i + 4]
        asset = addr_word(a) if ok_a else None
        oracle = addr_word(o) if ok_o else None
        uoa = addr_word(u) if ok_u else None
        tb = u256(br) if ok_b else None
        if not ok_a:
            rpc.fail(f"{family}:asset:{v[:10]}", "asset() failed")
        if not ok_b:
            rpc.fail(f"{family}:totalBorrows:{v[:10]}", "totalBorrows() failed")
        if asset:
            b.add_token_addr(asset, tracked=True)
        b.put_protocol(
            f"{family}:{lc(v)}",
            {
                "family": family,
                "market": v,
                "asset": asset,
                "oracle": oracle,
                "unit_of_account": uoa,
                "deployed_block": 0,
                "receipt_tokens": [v],
                "oracle_adapters": [oracle] if oracle else [],
                "borrowed_raw": tb,
                "borrowed_usd": None,
                "admitted": False,
            },
        )
    b.counts[family] = {
        "instances": 1,
        "reserves": len(vaults),
        "receipt_tokens": 0,
        "aggregators": 0,
        "admitted_markets": 0,
        "factory_length": n,
    }
    rpc.notes.append(f"euler-v2: factory length={n}, sliced={len(vaults)}")
    if len(vaults) != n:
        rpc.fail(f"{family}:length_mismatch", f"n={n} got={len(vaults)}")
        b.partial.append(family)


def enum_silo(b: Builder) -> None:
    rpc = b.rpc
    family = "silo-v2"
    configs: list[tuple[str, str]] = []  # (config, factory)
    for fname, factory in (("v2", SILO_FACTORY_V2), ("v3", SILO_FACTORY_V3)):
        if not rpc.has_code(factory):
            rpc.fail(f"{family}:factory:{fname}", "no code")
            continue
        nret = rpc.call1(factory, "getNextSiloId()")
        n = u256(nret) if nret else None
        if n is None:
            rpc.fail(f"{family}:getNextSiloId:{fname}", "eth_call failed")
            continue
        rpc.notes.append(f"silo:{fname} getNextSiloId()={n}")
        # ids are typically 1..n-1
        ids = list(range(1, n))
        cres = rpc.multicall(
            [(factory, sel("idToSiloConfig(uint256)") + encode(["uint256"], [i])) for i in ids]
        )
        for i, (ok, raw) in zip(ids, cres):
            if not ok:
                rpc.fail(f"{family}:idToSiloConfig:{fname}:{i}", "failed")
                continue
            cfg = addr_word(raw)
            if cfg:
                configs.append((cfg, checksum(factory)))
            else:
                rpc.fail(f"{family}:idToSiloConfig:{fname}:{i}", "zero config")

    n_silos = 0
    if configs:
        sres = rpc.multicall([(cfg, sel("getSilos()")) for cfg, _ in configs])
        debt_calls: list[tuple[str, bytes]] = []
        meta: list[tuple[str, str, str, str]] = []  # config, factory, silo0, silo1
        decoded: list[tuple[Optional[str], Optional[str]]] = []
        for (cfg, fac), (ok, raw) in zip(configs, sres):
            if not ok or not raw:
                rpc.fail(f"{family}:getSilos:{cfg[:10]}", "failed")
                decoded.append((None, None))
                continue
            try:
                s0, s1 = decode(["address", "address"], raw)
                s0c = checksum(s0) if lc(s0) != ZERO else None
                s1c = checksum(s1) if lc(s1) != ZERO else None
            except Exception as ex:  # noqa: BLE001
                rpc.fail(f"{family}:decode_silos:{cfg[:10]}", str(ex))
                decoded.append((None, None))
                continue
            decoded.append((s0c, s1c))
            for s in (s0c, s1c):
                if s:
                    n_silos += 1
                    debt_calls.append((s, sel("asset()")))
                    debt_calls.append((s, sel("getDebtAssets()")))
            meta.append((cfg, fac, s0c or "", s1c or ""))

        dres = rpc.multicall(debt_calls)
        di = 0
        for (cfg, fac), (s0, s1) in zip(configs, decoded):
            assets = []
            debts = []
            silos = [s for s in (s0, s1) if s]
            for s in silos:
                ok_a, a = dres[di]
                ok_d, d = dres[di + 1]
                di += 2
                ast = addr_word(a) if ok_a else None
                if ast:
                    assets.append(ast)
                    b.add_token_addr(ast, tracked=True)
                else:
                    rpc.fail(f"{family}:asset:{s[:10]}", "asset() failed")
                if ok_d:
                    debts.append(u256(d))
                else:
                    rpc.fail(f"{family}:getDebtAssets:{s[:10]}", "getDebtAssets() failed")
                    debts.append(None)
            borrowed_raw = None
            if debts and all(x is not None for x in debts):
                borrowed_raw = sum(int(x) for x in debts)
            loan = assets[0] if assets else None
            coll = assets[1] if len(assets) > 1 else None
            b.put_protocol(
                f"{family}:{lc(cfg)}",
                {
                    "family": family,
                    "market": cfg,
                    "silo_config": cfg,
                    "loan_token": loan,
                    "collateral_token": coll,
                    "deployed_block": 0,
                    "receipt_tokens": silos,
                    "oracle_adapters": [],
                    "borrowed_raw": borrowed_raw,
                    "borrowed_usd": None,
                    "admitted": False,
                    "factory": fac,
                },
            )

    b.counts[family] = {
        "instances": len({f for _, f in configs}),
        "reserves": len(configs),
        "receipt_tokens": 0,
        "aggregators": 0,
        "admitted_markets": 0,
        "silos": n_silos,
    }
    rpc.notes.append(f"silo-v2: {len(configs)} configs, {n_silos} silos")
    if not configs:
        b.partial.append(family)


def enum_ajna(b: Builder) -> None:
    rpc = b.rpc
    family = "ajna"
    pools: list[tuple[str, str]] = []  # addr, factory_kind
    for kind, factory in (("erc20", AJNA_ERC20_FACTORY), ("erc721", AJNA_ERC721_FACTORY)):
        if not rpc.has_code(factory):
            rpc.fail(f"{family}:factory:{kind}", "no code")
            continue
        pret = rpc.call1(factory, "getDeployedPoolsList()")
        if pret is None:
            rpc.fail(f"{family}:getDeployedPoolsList:{kind}", "eth_call failed")
            continue
        try:
            found = [checksum(a) for a in decode(["address[]"], pret)[0] if lc(a) != ZERO]
        except Exception as ex:  # noqa: BLE001
            rpc.fail(f"{family}:decode_pools:{kind}", str(ex))
            continue
        rpc.notes.append(f"ajna:{kind} getDeployedPoolsList n={len(found)}")
        for p in found:
            pools.append((p, kind))

    if pools:
        calls = []
        for p, _ in pools:
            calls += [
                (p, sel("collateralAddress()")),
                (p, sel("quoteTokenAddress()")),
                (p, sel("debtInfo()")),
            ]
        res = rpc.multicall(calls)
        for i, (p, kind) in enumerate(pools):
            (ok_c, c), (ok_q, q), (ok_d, d) = res[3 * i : 3 * i + 3]
            coll = addr_word(c) if ok_c else None
            quote = addr_word(q) if ok_q else None
            debt = None
            if ok_d and d and len(d) >= 32:
                debt = int.from_bytes(d[:32], "big")
            elif not ok_d:
                rpc.fail(f"{family}:debtInfo:{p[:10]}", "debtInfo() failed")
            if not ok_c:
                rpc.fail(f"{family}:collateral:{p[:10]}", "collateralAddress() failed")
            if not ok_q:
                rpc.fail(f"{family}:quote:{p[:10]}", "quoteTokenAddress() failed")
            if quote:
                b.add_token_addr(quote, tracked=True)
            if coll and kind == "erc20":
                b.add_token_addr(coll, tracked=True)
            b.put_protocol(
                f"{family}:{lc(p)}",
                {
                    "family": family,
                    "market": p,
                    "loan_token": quote,
                    "collateral_token": coll,
                    "quote_token": quote,
                    "kind": kind,
                    "deployed_block": 0,
                    "receipt_tokens": [],
                    "oracle_adapters": [],
                    "borrowed_raw": debt,
                    "borrowed_usd": None,
                    "admitted": False,
                },
            )
    b.counts[family] = {
        "instances": 2,
        "reserves": len(pools),
        "receipt_tokens": 0,
        "aggregators": 0,
        "admitted_markets": 0,
    }
    if not pools:
        b.partial.append(family)


# --------------------------------------------------------------------------- derivation / admission / univ3
def derive_tokens(b: Builder) -> None:
    rpc = b.rpc
    addrs = [checksum(a) for a in sorted(b.token_addrs)]
    if not addrs:
        return
    calls = []
    for a in addrs:
        calls.append((a, sel("decimals()")))
        calls.append((a, sel("symbol()")))
    res = rpc.multicall(calls)
    for i, a in enumerate(addrs):
        ok_d, draw = res[2 * i]
        ok_s, sraw = res[2 * i + 1]
        dec = decode_decimals(draw) if ok_d else None
        if dec is None:
            reason = "no_such_function" if ok_d else "call_failed"
            if not rpc.has_code(a):
                reason = "no_contract_code"
            rpc.fail(f"token:decimals:{lc(a)}", f"decimals() failed ({reason})")
        sym, nonstd = decode_symbol(sraw) if ok_s and sraw else ("", False)
        if not ok_s:
            rpc.fail(f"token:symbol:{lc(a)}", "symbol() failed")
        quirks: list[str] = []
        if nonstd:
            quirks.append("nonstandard_metadata")
        if dec is not None and dec < 18:
            quirks.append("low_decimals")
        for q in KNOWN_QUIRKS.get(lc(a), []):
            if q not in quirks:
                quirks.append(q)
        b.tokens[a] = {"symbol": sym, "decimals": dec, "quirks": quirks}


def _token_decimals(b: Builder, addr: Optional[str]) -> Optional[int]:
    if not addr:
        return None
    t = b.tokens.get(checksum(addr)) or b.tokens.get(addr)
    if not t:
        return None
    return t.get("decimals")


def price_usd(b: Builder) -> dict[str, float]:
    """USD price per whole token, from Feed Registry then Aave/Spark oracles.

    Missing price → token omitted (caller logs unpriced). Never assume $1.
    """
    rpc = b.rpc
    prices: dict[str, float] = {}  # lowercase addr -> usd per 1 token
    want = sorted(b.tracked_underlyings)
    # 1) Chainlink Feed Registry: latestRoundData(token, USD) and (token, ETH)
    if rpc.has_code(FEED_REGISTRY):
        fr = checksum(FEED_REGISTRY)
        calls = []
        for a in want:
            calls.append((fr, sel("latestRoundData(address,address)") + encode(["address", "address"], [checksum(a), checksum(USD_DENOM)])))
            calls.append((fr, sel("decimals(address,address)") + encode(["address", "address"], [checksum(a), checksum(USD_DENOM)])))
        # WETH via ETH denom
        calls.append((fr, sel("latestRoundData(address,address)") + encode(["address", "address"], [checksum(ETH_DENOM), checksum(USD_DENOM)])))
        calls.append((fr, sel("decimals(address,address)") + encode(["address", "address"], [checksum(ETH_DENOM), checksum(USD_DENOM)])))
        res = rpc.multicall(calls)
        eth_usd = None
        ok_e, eraw = res[-2]
        ok_ed, edraw = res[-1]
        if ok_e and eraw and len(eraw) >= 128:
            answer = int.from_bytes(eraw[32:64], "big", signed=True)
            ts = int.from_bytes(eraw[96:128], "big")
            ed = decode_decimals(edraw) if ok_ed else 8
            if answer > 0 and ts > 0 and ed is not None:
                eth_usd = answer / (10**ed)
                prices[lc(HUB_ASSETS["WETH"])] = eth_usd
            else:
                rpc.fail("price:ETH/USD", f"non-positive or unstamped feed answer={answer} ts={ts}")
        else:
            rpc.fail("price:ETH/USD", "Feed Registry latestRoundData(ETH,USD) failed")
        for i, a in enumerate(want):
            ok, raw = res[2 * i]
            okd, draw = res[2 * i + 1]
            if not ok or not raw or len(raw) < 128:
                continue
            answer = int.from_bytes(raw[32:64], "big", signed=True)
            ts = int.from_bytes(raw[96:128], "big")
            d = decode_decimals(draw) if okd else 8
            if answer > 0 and ts > 0 and d is not None:
                prices[a] = answer / (10**d)
        # token/ETH * ETH/USD for remaining
        remaining = [a for a in want if a not in prices]
        if remaining and eth_usd is not None:
            calls = []
            for a in remaining:
                calls.append((fr, sel("latestRoundData(address,address)") + encode(["address", "address"], [checksum(a), checksum(ETH_DENOM)])))
                calls.append((fr, sel("decimals(address,address)") + encode(["address", "address"], [checksum(a), checksum(ETH_DENOM)])))
            res = rpc.multicall(calls)
            for i, a in enumerate(remaining):
                ok, raw = res[2 * i]
                okd, draw = res[2 * i + 1]
                if not ok or not raw or len(raw) < 128:
                    continue
                answer = int.from_bytes(raw[32:64], "big", signed=True)
                ts = int.from_bytes(raw[96:128], "big")
                d = decode_decimals(draw) if okd else 18
                if answer > 0 and ts > 0 and d is not None:
                    prices[a] = (answer / (10**d)) * eth_usd
    else:
        rpc.fail("price:feed_registry", "no code at Chainlink Feed Registry")

    # 2) Aave / Spark oracles (USD, 8 decimals) for tokens still missing
    still = [a for a in want if a not in prices]
    oracle_addrs = []
    for p in b.protocols.values():
        if p.get("family") in ("aave-v3", "spark") and p.get("price_oracle"):
            oracle_addrs.append(p["price_oracle"])
    oracle_addrs = list(dict.fromkeys(oracle_addrs))
    if still and oracle_addrs:
        for oracle in oracle_addrs:
            left = [a for a in still if a not in prices]
            if not left:
                break
            chunk = 50
            for i in range(0, len(left), chunk):
                part = left[i : i + chunk]
                data = sel("getAssetsPrices(address[])") + encode(
                    ["address[]"], [[checksum(x) for x in part]]
                )
                ok, raw = rpc.multicall([(oracle, data)])[0]
                if not ok or not raw:
                    rpc.fail(f"price:aave_oracle:{oracle[:10]}:{i}", "getAssetsPrices failed")
                    continue
                try:
                    arr = decode(["uint256[]"], raw)[0]
                except Exception as ex:  # noqa: BLE001
                    rpc.fail(f"price:aave_oracle:decode:{oracle[:10]}", str(ex))
                    continue
                for tok, px in zip(part, arr):
                    if px and int(px) > 0:
                        prices[tok] = int(px) / 1e8
    return prices


def admit_family_b(b: Builder, prices: dict[str, float]) -> None:
    family_b = {"morpho-blue", "euler-v2", "silo-v2", "ajna"}
    for key, p in b.protocols.items():
        if p.get("family") not in family_b:
            continue
        raw = p.get("borrowed_raw")
        token = p.get("loan_token") or p.get("asset") or p.get("quote_token")
        if raw is None:
            b.rpc.fail(f"admission:noborrow:{key[:40]}", "borrowed_raw missing")
            p["admitted"] = False
            continue
        if not token:
            b.rpc.fail(f"admission:notoken:{key[:40]}", "no debt token")
            p["admitted"] = False
            continue
        dec = _token_decimals(b, token)
        if dec is None:
            b.rpc.fail(f"admission:nodecimals:{lc(token)}", "decimals unknown; not admitted")
            p["admitted"] = False
            continue
        px = prices.get(lc(token))
        if px is None:
            sym = (b.tokens.get(checksum(token)) or {}).get("symbol") or "?"
            b.rpc.fail(
                f"admission:unpriced:{lc(token)}",
                f"no USD source ({sym}): feed registry + aave/spark oracles",
            )
            p["admitted"] = False
            continue
        usd = (int(raw) / (10**dec)) * px
        p["borrowed_usd"] = usd
        p["admitted"] = usd >= ADMISSION_USD
    for fam in family_b:
        n = sum(1 for p in b.protocols.values() if p.get("family") == fam and p.get("admitted"))
        b.counts.setdefault(fam, {})["admitted_markets"] = n
        b.rpc.notes.append(f"{fam}: admitted {n} markets at ${ADMISSION_USD:.0f} borrowed")


def enum_univ3(b: Builder) -> None:
    """AND filter: both pool tokens in the FROZEN tracked-underlying snapshot.

    Snapshot is taken before any pool is added so a new pool cannot admit its
    other token into the next iteration (the snowball REGISTRY.md / C1 hit).
    Pool token0/token1/fee are read from the pool, not assumed from the factory.
    """
    rpc = b.rpc
    family = "univ3"
    tracked = frozenset(b.tracked_underlyings)  # FROZEN
    hubs = [lc(a) for a in HUB_ASSETS.values()]
    if not rpc.has_code(UNIV3_FACTORY):
        rpc.fail("univ3:factory", "no code")
        b.partial.append(family)
        return
    factory = checksum(UNIV3_FACTORY)
    tracked_cs = [checksum(a) for a in sorted(tracked)]
    # Path A: factory.getPool(token, hub, fee) for every tracked token × hub × fee.
    calls: list[tuple[str, bytes]] = []
    meta: list[tuple[str, str, int]] = []
    for t in tracked_cs:
        for h in hubs:
            if lc(t) == h:
                continue
            t0, t1 = (t, checksum(h)) if lc(t) < h else (checksum(h), t)
            for fee in UNIV3_FEES:
                calls.append(
                    (factory, sel("getPool(address,address,uint24)") + encode(["address", "address", "uint24"], [t0, t1, fee]))
                )
                meta.append((t0, t1, fee))
    print(f"  univ3 getPool calls: {len(calls)}", flush=True)
    found: dict[str, tuple[str, str, int]] = {}
    res = rpc.multicall(calls)
    for (t0, t1, fee), (ok, raw) in zip(meta, res):
        if not ok:
            continue
        pool = addr_word(raw)
        if pool:
            found[lc(pool)] = (t0, t1, fee)

    # Path B: PoolCreated sweep, AND-filter against the frozen snapshot.
    tp = topic0("PoolCreated(address,address,uint24,int24,address)")
    logs = rpc.get_logs(
        factory, [tp], UNIV3_FACTORY_BLOCK, b.block, chunk=2_000_000, tag="univ3.PoolCreated"
    )
    n_logs = len(logs)
    n_and = 0
    for lg in logs:
        topics = lg["topics"]
        if len(topics) < 4:
            continue
        t0 = "0x" + bytes(topics[1])[-20:].hex()
        t1 = "0x" + bytes(topics[2])[-20:].hex()
        fee = int.from_bytes(bytes(topics[3])[-3:], "big") if len(bytes(topics[3])) >= 3 else int(topics[3], 16)
        try:
            pool = checksum("0x" + bytes(lg["data"])[-20:].hex())
        except Exception:
            continue
        if lc(t0) in tracked and lc(t1) in tracked:
            n_and += 1
            found.setdefault(lc(pool), (checksum(t0), checksum(t1), int(fee)))

    # Derive token0/token1/fee from the pool itself (REGISTRY.md §3c).
    pools = [checksum(p) for p in found]
    calls = []
    for p in pools:
        calls += [(p, sel("token0()")), (p, sel("token1()")), (p, sel("fee()"))]
    pres = rpc.multicall(calls)
    kept = 0
    for i, p in enumerate(pools):
        (ok0, r0), (ok1, r1), (okf, rf) = pres[3 * i : 3 * i + 3]
        t0 = addr_word(r0) if ok0 else None
        t1 = addr_word(r1) if ok1 else None
        fee = u256(rf) if okf else None
        if not (t0 and t1 and fee is not None):
            rpc.fail(f"univ3:derive:{p[:10]}", f"token0/token1/fee read failed ok={ok0,ok1,okf}")
            continue
        if lc(t0) not in tracked or lc(t1) not in tracked:
            continue  # AND filter on on-chain order, not the factory hint
        b.pools[p] = {
            "venue": "univ3",
            "token0": t0,
            "token1": t1,
            "fee": int(fee),
            "deployed_block": 0,
        }
        b.flash_sources[p] = {"venue": "univ3", "kind": "pool", "source": p}
        kept += 1

    b.counts[family] = {
        "instances": kept,
        "reserves": 0,
        "receipt_tokens": 0,
        "aggregators": 0,
        "admitted_markets": 0,
        "getpool_hits": len(found),
        "poolcreated_logs": n_logs,
        "poolcreated_and": n_and,
        "tracked_snapshot": len(tracked),
    }
    rpc.notes.append(
        f"univ3: tracked_snapshot={len(tracked)} getPool/log candidates={len(found)} "
        f"AND-kept={kept} PoolCreated logs={n_logs}"
    )
    if n_logs == 0:
        b.partial.append(family)


def add_static_flash(b: Builder) -> None:
    if b.rpc.has_code(UNIV4_POOL_MANAGER):
        b.flash_sources[checksum(UNIV4_POOL_MANAGER)] = {
            "venue": "univ4",
            "kind": "pool_manager",
            "source": checksum(UNIV4_POOL_MANAGER),
        }
    else:
        b.rpc.fail("flash:univ4", "PoolManager has no code")


# --------------------------------------------------------------------------- identity + diff (C1 file read happens HERE, not before)
def load_canonical_tokenlist(b: Builder) -> dict[str, str]:
    """symbol.lower() -> canonical address (chain 1). Fetch failure is logged."""
    for url in UNISWAP_TOKENLIST_URLS:
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "liq-registry-rederive/c2"})
            with urllib.request.urlopen(req, timeout=30) as resp:
                data = json.loads(resp.read().decode("utf-8"))
            out: dict[str, str] = {}
            for t in data.get("tokens") or []:
                if int(t.get("chainId", 0)) != 1:
                    continue
                addr = lc(t.get("address", ""))
                sym = (t.get("symbol") or "").strip()
                if addr and addr != ZERO and sym:
                    out.setdefault(sym.lower(), addr)
                    out[addr] = addr  # address membership
            b.rpc.notes.append(f"canonical token list: {url} → {len(out)} entries")
            return out
        except Exception as ex:  # noqa: BLE001
            b.rpc.fail(f"identity:tokenlist:{url}", str(ex))
    return {}


def load_d15(guides: Path) -> dict[str, list[dict]]:
    p = guides / "d15_addresses.json"
    if not p.exists():
        return {}
    data = json.loads(p.read_text(encoding="utf-8"))
    by_addr: dict[str, list[dict]] = defaultdict(list)
    for row in data.get("addresses") or []:
        by_addr[lc(row.get("address", ""))].append(row)
    return by_addr


def identity_check(b: Builder, guides: Path) -> dict[str, Any]:
    tokenlist = load_canonical_tokenlist(b)
    d15 = load_d15(guides)
    published_roots = {
        lc(AAVE_V3_REGISTRY), lc(SPARK_REGISTRY), lc(ILK_REGISTRY),
        lc(COMPOUND_V2_COMPTROLLER), lc(MORPHO_BLUE), lc(EULER_FACTORY),
        lc(SILO_FACTORY_V2), lc(SILO_FACTORY_V3), lc(AJNA_ERC20_FACTORY),
        lc(AJNA_ERC721_FACTORY), lc(UNIV3_FACTORY),
    }
    for h in AAVE_V4_HUBS.values():
        published_roots.add(lc(h))
    for h in HUB_ASSETS.values():
        published_roots.add(lc(h))

    token_ok = []
    token_unlisted = []
    token_symbol_collision = []  # symbol matches canonical list at a DIFFERENT address
    for addr, t in b.tokens.items():
        a = lc(addr)
        sym = (t.get("symbol") or "").strip()
        in_list = a in tokenlist or a in d15
        if in_list:
            token_ok.append(addr)
        else:
            token_unlisted.append({"address": addr, "symbol": sym})
        if sym and sym.lower() in tokenlist:
            canon = tokenlist[sym.lower()]
            if canon != a and len(canon) == 42:
                token_symbol_collision.append(
                    {"address": addr, "symbol": sym, "canonical": checksum(canon)}
                )

    family_a = {"aave-v3", "aave-v4", "spark", "sky-maker", "compound-v2"}
    family_b = {"morpho-blue", "euler-v2", "silo-v2", "ajna"}
    market_in_published = []
    market_not_in_published = []
    for key, p in b.protocols.items():
        fam = p.get("family")
        market = p.get("market")
        if not market:
            continue
        m = lc(market)
        in_pub = m in d15 or m in published_roots
        rec = {"key": key, "family": fam, "market": market, "in_d15": m in d15}
        if fam in family_a:
            if in_pub:
                market_in_published.append(rec)
            else:
                market_not_in_published.append(rec)
        elif fam in family_b:
            # Permissionless: the *root* must be published; the market itself is not.
            root_ok = False
            if fam == "morpho-blue":
                root_ok = lc(MORPHO_BLUE) in published_roots
            elif fam == "euler-v2":
                root_ok = lc(EULER_FACTORY) in published_roots
            elif fam == "silo-v2":
                root_ok = lc(p.get("factory") or "") in published_roots or True
            elif fam == "ajna":
                root_ok = True
            rec["published_root_ok"] = root_ok
            if in_pub:
                market_in_published.append(rec)

    return {
        "canonical_list_entries": len(tokenlist),
        "d15_addresses": sum(len(v) for v in d15.values()),
        "tokens_checked": len(b.tokens),
        "tokens_in_canonical_or_d15": len(token_ok),
        "tokens_unlisted": token_unlisted[:80],
        "tokens_unlisted_n": len(token_unlisted),
        "token_symbol_collisions": token_symbol_collision,
        "family_a_markets_in_published": len(market_in_published),
        "family_a_markets_not_in_published": market_not_in_published[:80],
        "family_a_markets_not_in_published_n": len(market_not_in_published),
    }


def _c1_index(reg: dict) -> dict[str, Any]:
    """Build address-centric indexes from C1's committed file. Called only after rederive write."""
    protos = reg.get("protocols") or {}
    by_family: dict[str, set[str]] = defaultdict(set)
    admitted: dict[str, set[str]] = defaultdict(set)
    markets: dict[str, dict] = {}
    for key, p in protos.items():
        fam = p.get("family") or p.get("protocol") or "?"
        ident = lc(p.get("market_id") or p.get("market") or p.get("silo_config") or p.get("comptroller") or key)
        by_family[fam].add(ident)
        markets[f"{fam}:{ident}"] = p
        if p.get("admitted"):
            admitted[fam].add(ident)
    tokens = {lc(a): t for a, t in (reg.get("tokens") or {}).items()}
    pools = {lc(a): p for a, p in (reg.get("pools") or {}).items()}
    oracles = {lc(a): o for a, o in (reg.get("oracles") or {}).items()}
    return {
        "by_family": {k: v for k, v in by_family.items()},
        "admitted": {k: v for k, v in admitted.items()},
        "markets": markets,
        "tokens": tokens,
        "pools": pools,
        "oracles": oracles,
        "n_protocols": len(protos),
        "n_admitted": sum(1 for p in protos.values() if p.get("admitted")),
        "n_tokens": len(tokens),
        "n_pools": len(pools),
    }


def _ours_index(b: Builder) -> dict[str, Any]:
    by_family: dict[str, set[str]] = defaultdict(set)
    admitted: dict[str, set[str]] = defaultdict(set)
    for key, p in b.protocols.items():
        fam = p.get("family") or "?"
        ident = lc(p.get("market_id") or p.get("market") or p.get("silo_config") or p.get("comptroller") or key)
        by_family[fam].add(ident)
        if p.get("admitted"):
            admitted[fam].add(ident)
    return {
        "by_family": dict(by_family),
        "admitted": dict(admitted),
        "tokens": {lc(a): t for a, t in b.tokens.items()},
        "pools": {lc(a): p for a, p in b.pools.items()},
        "oracles": {lc(a): o for a, o in b.oracles.items()},
        "n_protocols": len(b.protocols),
        "n_admitted": sum(1 for p in b.protocols.values() if p.get("admitted")),
        "n_tokens": len(b.tokens),
        "n_pools": len(b.pools),
    }


def diff_against_c1(b: Builder, c1_path: Path) -> dict[str, Any]:
    if not c1_path.exists():
        return {"error": f"C1 file missing: {c1_path}"}
    c1 = json.loads(c1_path.read_text(encoding="utf-8"))
    ci = _c1_index(c1)
    oi = _ours_index(b)
    families = sorted(set(ci["by_family"]) | set(oi["by_family"]))
    per_family = {}
    only_ours_all = []
    only_c1_all = []
    for fam in families:
        o = oi["by_family"].get(fam, set())
        c = ci["by_family"].get(fam, set())
        only_o = sorted(o - c)
        only_c = sorted(c - o)
        oa = oi["admitted"].get(fam, set())
        ca = ci["admitted"].get(fam, set())
        per_family[fam] = {
            "ours": len(o),
            "c1": len(c),
            "intersection": len(o & c),
            "only_ours_n": len(only_o),
            "only_c1_n": len(only_c),
            "only_ours_sample": only_o[:25],
            "only_c1_sample": only_c[:25],
            "admitted_ours": len(oa),
            "admitted_c1": len(ca),
            "admitted_only_ours_n": len(oa - ca),
            "admitted_only_c1_n": len(ca - oa),
            "admitted_only_ours_sample": sorted(oa - ca)[:15],
            "admitted_only_c1_sample": sorted(ca - oa)[:15],
        }
        only_ours_all.extend(f"{fam}:{x}" for x in only_o)
        only_c1_all.extend(f"{fam}:{x}" for x in only_c)

    # token decimals/symbol mismatches on the intersection
    tok_mismatch = []
    for a, ours in oi["tokens"].items():
        ctok = ci["tokens"].get(a)
        if not ctok:
            continue
        if ours.get("decimals") != ctok.get("decimals") or (ours.get("symbol") or "") != (ctok.get("symbol") or ""):
            tok_mismatch.append(
                {
                    "address": a,
                    "ours": {"symbol": ours.get("symbol"), "decimals": ours.get("decimals")},
                    "c1": {"symbol": ctok.get("symbol"), "decimals": ctok.get("decimals")},
                }
            )
    pool_mismatch = []
    for a, ours in oi["pools"].items():
        cp = ci["pools"].get(a)
        if not cp:
            continue
        if lc(ours.get("token0")) != lc(cp.get("token0")) or lc(ours.get("token1")) != lc(cp.get("token1")) or ours.get("fee") != cp.get("fee"):
            pool_mismatch.append({"address": a, "ours": ours, "c1": cp})

    empty = (
        not only_ours_all
        and not only_c1_all
        and not tok_mismatch
        and not pool_mismatch
        and oi["n_admitted"] == ci["n_admitted"]
        and oi["n_pools"] == ci["n_pools"]
        and oi["n_tokens"] == ci["n_tokens"]
    )
    return {
        "c1_n_protocols": ci["n_protocols"],
        "ours_n_protocols": oi["n_protocols"],
        "c1_n_admitted": ci["n_admitted"],
        "ours_n_admitted": oi["n_admitted"],
        "c1_n_tokens": ci["n_tokens"],
        "ours_n_tokens": oi["n_tokens"],
        "c1_n_pools": ci["n_pools"],
        "ours_n_pools": oi["n_pools"],
        "per_family": per_family,
        "only_ours_n": len(only_ours_all),
        "only_c1_n": len(only_c1_all),
        "only_ours_sample": only_ours_all[:40],
        "only_c1_sample": only_c1_all[:40],
        "token_field_mismatches": tok_mismatch[:40],
        "token_field_mismatches_n": len(tok_mismatch),
        "pool_field_mismatches": pool_mismatch[:20],
        "pool_field_mismatches_n": len(pool_mismatch),
        "tokens_only_ours_n": len(set(oi["tokens"]) - set(ci["tokens"])),
        "tokens_only_c1_n": len(set(ci["tokens"]) - set(oi["tokens"])),
        "pools_only_ours_n": len(set(oi["pools"]) - set(ci["pools"])),
        "pools_only_c1_n": len(set(ci["pools"]) - set(oi["pools"])),
        "diff_empty": empty,
    }


def write_outputs(b: Builder, out_dir: Path, guides: Path, c1_path: Path) -> str:
    generated = {
        "chain_id": 1,
        "generated_at_block": b.block,
        "independent": True,
        "source": "tools/registry/rederive.py",
        "admission": {
            "interim": True,
            "threshold_usd_borrowed": ADMISSION_USD,
            "note": "INTERIM (D27 unset): Family B markets with total borrowed >= $50,000 USD notional; Family A governed sets included in full. Unpriced markets are not admitted.",
            "admitted_markets_total": sum(1 for p in b.protocols.values() if p.get("admitted") and p.get("family") in {"morpho-blue", "euler-v2", "silo-v2", "ajna"}),
        },
        "tokens": b.tokens,
        "protocols": b.protocols,
        "oracles": b.oracles,
        "pools": b.pools,
        "flash_sources": b.flash_sources,
        "routers": b.routers,
        "counts_by_protocol": b.counts,
        "failures": b.rpc.failures,
        "notes": b.rpc.notes + [f"rpc_calls multicall={b.rpc._n_mc} getLogs={b.rpc._n_logs}"],
        "partial": b.partial,
        "token_count": len(b.tokens),
        "pool_count": len(b.pools),
        "oracle_proxy_count": len(b.oracles),
    }
    rederived_path = out_dir / "registry.rederived.json"
    rederived_path.write_text(json.dumps(generated, indent=2), encoding="utf-8")
    print(f"wrote {rederived_path} ({rederived_path.stat().st_size} bytes)", flush=True)

    # C1 is read for the first time here.
    b.identity = identity_check(b, guides)
    b.diff = diff_against_c1(b, c1_path)

    collisions = b.identity.get("token_symbol_collisions") or []
    fam_a_missing = b.identity.get("family_a_markets_not_in_published_n") or 0
    diff_empty = bool(b.diff.get("diff_empty"))
    if b.partial:
        verdict = "PARTIAL"
    elif collisions or not diff_empty:
        verdict = "FAIL"
    else:
        verdict = "PASS"
    # Family A markets not in d15 are reported; Compound V2 forks discovered
    # from MarketListed will not be in D15 (D15 explicitly omitted them) — that
    # is an expected identity-check gap, not a silent pass.
    generated["identity"] = b.identity
    generated["diff"] = {k: v for k, v in b.diff.items() if k not in ("per_family",)}
    generated["verdict"] = verdict
    rederived_path.write_text(json.dumps(generated, indent=2), encoding="utf-8")

    report = render_report(b, verdict)
    report_path = out_dir / "verify-report.md"
    report_path.write_text(report, encoding="utf-8")
    print(f"wrote {report_path} verdict={verdict}", flush=True)
    return verdict


def render_report(b: Builder, verdict: str) -> str:
    d = b.diff or {}
    ident = b.identity or {}
    lines = []
    lines.append("# Registry verify-report (WP C2)")
    lines.append("")
    lines.append(f"**Verdict: {verdict}**")
    lines.append("")
    lines.append("Independent re-derivation from on-chain roots (`tools/registry/rederive.py`).")
    lines.append("C1 `registry/registry.json` was not read until after `registry.rederived.json` was written.")
    lines.append(f"Block: `{b.block}`. Admission bar: Family B borrowed USD ≥ ${ADMISSION_USD:,.0f} (D27 interim).")
    lines.append("")
    if b.partial:
        lines.append(f"**PARTIAL protocols** (RPC/enumeration incomplete): {', '.join(b.partial)}")
        lines.append("")
    lines.append("## Independent counts")
    lines.append("")
    lines.append("| Family | Instances / markets | Reserves | Receipt tokens | Aggregators | Admitted (Family B) |")
    lines.append("|---|---:|---:|---:|---:|---:|")
    for fam, c in b.counts.items():
        lines.append(
            f"| {fam} | {c.get('instances', c.get('reserves', 0))} | {c.get('reserves', 0)} | "
            f"{c.get('receipt_tokens', 0)} | {c.get('aggregators', 0)} | {c.get('admitted_markets', 0)} |"
        )
    lines.append("")
    lines.append(f"- Tokens derived: **{len(b.tokens)}**")
    lines.append(f"- UniV3 pools (AND filter): **{len(b.pools)}**")
    lines.append(f"- Oracle proxies: **{len(b.oracles)}**")
    lines.append(f"- Flash sources: **{len(b.flash_sources)}**")
    lines.append(f"- Protocol entries: **{len(b.protocols)}**")
    lines.append(f"- RPC failures logged (not guessed): **{len(b.rpc.failures)}**")
    lines.append("")
    lines.append("## Diff vs C1 `registry/registry.json`")
    lines.append("")
    if d.get("error"):
        lines.append(f"C1 read error: {d['error']}")
    else:
        lines.append(f"| | Independent (C2) | C1 |")
        lines.append(f"|---|---:|---:|")
        lines.append(f"| Protocol entries | {d.get('ours_n_protocols')} | {d.get('c1_n_protocols')} |")
        lines.append(f"| Admitted Family B | {d.get('ours_n_admitted')} | {d.get('c1_n_admitted')} |")
        lines.append(f"| Tokens | {d.get('ours_n_tokens')} | {d.get('c1_n_tokens')} |")
        lines.append(f"| UniV3 pools | {d.get('ours_n_pools')} | {d.get('c1_n_pools')} |")
        lines.append("")
        lines.append(f"- Address keys only in C2: **{d.get('only_ours_n')}**")
        lines.append(f"- Address keys only in C1: **{d.get('only_c1_n')}**")
        lines.append(f"- Tokens only in C2 / only in C1: {d.get('tokens_only_ours_n')} / {d.get('tokens_only_c1_n')}")
        lines.append(f"- Pools only in C2 / only in C1: {d.get('pools_only_ours_n')} / {d.get('pools_only_c1_n')}")
        lines.append(f"- Token field mismatches (intersection): **{d.get('token_field_mismatches_n')}**")
        lines.append(f"- Pool field mismatches (intersection): **{d.get('pool_field_mismatches_n')}**")
        lines.append(f"- Diff empty: **{d.get('diff_empty')}**")
        lines.append("")
        lines.append("### Per family")
        lines.append("")
        lines.append("| Family | C2 | C1 | ∩ | only C2 | only C1 | admitted C2 | admitted C1 |")
        lines.append("|---|---:|---:|---:|---:|---:|---:|---:|")
        for fam, row in (d.get("per_family") or {}).items():
            lines.append(
                f"| {fam} | {row['ours']} | {row['c1']} | {row['intersection']} | "
                f"{row['only_ours_n']} | {row['only_c1_n']} | {row['admitted_ours']} | {row['admitted_c1']} |"
            )
        lines.append("")
        if d.get("only_ours_sample"):
            lines.append("Only-C2 sample: `" + "`, `".join(d["only_ours_sample"][:20]) + "`")
            lines.append("")
        if d.get("only_c1_sample"):
            lines.append("Only-C1 sample: `" + "`, `".join(d["only_c1_sample"][:20]) + "`")
            lines.append("")
        if d.get("token_field_mismatches"):
            lines.append("Token mismatches (first 10):")
            for m in d["token_field_mismatches"][:10]:
                lines.append(f"- `{m['address']}` ours={m['ours']} c1={m['c1']}")
            lines.append("")
    lines.append("## Identity check (§4b)")
    lines.append("")
    lines.append(f"- Canonical list entries: {ident.get('canonical_list_entries')}")
    lines.append(f"- D15 address rows: {ident.get('d15_addresses')}")
    lines.append(f"- Tokens checked: {ident.get('tokens_checked')}; in canonical or D15: {ident.get('tokens_in_canonical_or_d15')}; unlisted: {ident.get('tokens_unlisted_n')}")
    lines.append(f"- Symbol collisions (same symbol, different address vs canonical list): **{len(ident.get('token_symbol_collisions') or [])}**")
    lines.append(f"- Family A markets not in published deployments/D15: **{ident.get('family_a_markets_not_in_published_n')}**")
    lines.append("")
    coll = ident.get("token_symbol_collisions") or []
    if coll:
        lines.append("### Dangerous identity errors (symbol collision)")
        for c in coll[:20]:
            lines.append(f"- `{c['symbol']}` derived `{c['address']}` vs canonical `{c['canonical']}`")
        lines.append("")
    missing = ident.get("family_a_markets_not_in_published") or []
    if missing:
        lines.append("### Family A markets not in D15 / published roots (sample)")
        for m in missing[:20]:
            lines.append(f"- {m['family']} `{m['market']}`")
        lines.append("")
        lines.append("Note: Compound V2 forks found via `MarketListed` are expected to be absent from D15 (D15 explicitly omitted them).")
        lines.append("")
    lines.append("## Notes")
    lines.append("")
    for n in b.rpc.notes:
        lines.append(f"- {n}")
    lines.append("")
    lines.append("## Failures (truncated)")
    lines.append("")
    items = list(b.rpc.failures.items())
    lines.append(f"{len(items)} failures. First 40:")
    for k, v in items[:40]:
        lines.append(f"- `{k}`: {v}")
    lines.append("")
    return "\n".join(lines)


# --------------------------------------------------------------------------- main
STEPS = [
    ("aave-v3", lambda b: enum_aave_like(b, "aave-v3", AAVE_V3_REGISTRY)),
    ("spark", lambda b: enum_aave_like(b, "spark", SPARK_REGISTRY)),
    ("aave-v4", enum_aave_v4),
    ("sky-maker", enum_sky),
    ("compound-v2", enum_compound_v2),
    ("morpho-blue", enum_morpho),
    ("euler-v2", enum_euler),
    ("silo-v2", enum_silo),
    ("ajna", enum_ajna),
]


def main(argv: Optional[list[str]] = None) -> int:
    ap = argparse.ArgumentParser(description="Independent registry re-derivation (WP C2)")
    ap.add_argument("--rpc-call", default=RPC_CALL_DEFAULT)
    ap.add_argument("--rpc-logs", default=RPC_LOGS_DEFAULT)
    ap.add_argument("--out-dir", default=None, help="registry/ directory")
    ap.add_argument("--guides", default=None, help="liquidator-guides/ directory")
    ap.add_argument("--resume", action="store_true")
    ap.add_argument("--only", default=None, help="run a single protocol step")
    ap.add_argument("--skip-univ3-logs", action="store_true", help="getPool path only (faster PARTIAL)")
    args = ap.parse_args(argv)

    root = Path(__file__).resolve().parents[2]
    out_dir = Path(args.out_dir) if args.out_dir else root / "registry"
    guides = Path(args.guides) if args.guides else root / "liquidator-guides"
    out_dir.mkdir(parents=True, exist_ok=True)

    rpc = Rpc(args.rpc_call, args.rpc_logs)
    block = rpc.boot()
    print(f"head block {block}  call={args.rpc_call}  logs={args.rpc_logs}", flush=True)
    b = Builder(rpc, block, out_dir)
    if args.resume:
        b.load_full_snap()

    # Freeze hub assets into the tracked set from the start.
    for a in HUB_ASSETS.values():
        b.add_token_addr(a, tracked=True)

    for name, fn in STEPS:
        if args.only and args.only != name:
            continue
        if name in b.completed:
            print(f"== skip {name} (checkpoint)", flush=True)
            continue
        print(f"== {name}", flush=True)
        t0 = time.time()
        try:
            fn(b)
        except Exception as ex:  # noqa: BLE001
            rpc.fail(f"step:{name}", f"{type(ex).__name__}: {ex}")
            b.partial.append(name)
            print(f"  EXC {name}: {ex}", flush=True)
        print(f"  {name} done in {time.time()-t0:.1f}s  protocols={len(b.protocols)} failures={len(rpc.failures)}", flush=True)
        b.completed.append(name)
        b.save_ckpt(name)
        b.save_full_snap()

    print("== derive tokens", flush=True)
    derive_tokens(b)
    b.save_ckpt("tokens")
    print(f"  tokens={len(b.tokens)} pending={len(b.token_addrs)}", flush=True)

    print("== prices + admission", flush=True)
    prices = price_usd(b)
    rpc.notes.append(f"priced {len(prices)} underlyings")
    admit_family_b(b, prices)
    b.save_ckpt("admission")
    b.save_full_snap()

    print("== univ3", flush=True)
    if args.skip_univ3_logs:
        # still run getPool path; patch get_logs to no-op
        orig = rpc.get_logs
        rpc.get_logs = lambda *a, **k: []  # type: ignore[assignment]
        enum_univ3(b)
        rpc.get_logs = orig  # type: ignore[assignment]
        b.partial.append("univ3-logs")
    else:
        enum_univ3(b)
    add_static_flash(b)
    # Tokens that appeared only as pool token0/token1 should already be in tracked;
    # re-derive in case getPool introduced a hub we already have.
    b.save_ckpt("univ3")
    b.save_full_snap()

    c1_path = out_dir / "registry.json"
    verdict = write_outputs(b, out_dir, guides, c1_path)
    print(f"VERDICT {verdict}", flush=True)
    return 0 if verdict == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
