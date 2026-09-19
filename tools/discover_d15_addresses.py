#!/usr/bin/env python3
"""
D15 receipts_log_filter address discovery for Ethereum mainnet.

Walks on-chain registries / factory events per REGISTRY.md §3 and GUIDE-16 Essential tier.
Never hand-invents protocol markets — discovers via eth_call / eth_getLogs from known roots.

Usage:
  python3 discover_d15_addresses.py --rpc $RPC_URL --out-dir /path/to/liquidator-guides
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from collections import defaultdict
from pathlib import Path
from typing import Any, Callable, Optional

from web3 import Web3
from web3.exceptions import ContractLogicError

# ---------------------------------------------------------------------------
# Roots — confirm against official deployments before baking into production
# ---------------------------------------------------------------------------
ROOTS = {
    # Aave V3 PoolAddressesProviderRegistry (aave-address-book / Etherscan)
    "aave_v3_registry": "0xbaA999AC55EAce41CcAE355c77809e68Bb345170",
    # Spark (Aave V3 fork) PoolAddressesProviderRegistry
    "spark_registry": "0x03cFa0C4622FF84E50E75062683F44c9587e6Cc1",
    # Morpho Blue singleton
    "morpho_blue": "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb",
    # Sky/Maker IlkRegistry
    "ilk_registry": "0x5a464C28D19848f44199D003BeF5ecc87d090F87",
    # Fluid (Instadapp) — CREATE2 same on many chains
    "fluid_vault_factory": "0x324c5Dc1fC42c7a4D43d92df1eBA58a54d13Bf2d",
    "fluid_vault_resolver": "0xA5C3E16523eeeDDcC34706b0E6bE88b4c6EA95cC",
    # Euler V2 EVault GenericFactory (euler-interfaces addresses/1/CoreAddresses.json)
    "euler_evault_factory": "0x29a56a1b8214D9Cf7c5561811750D5cBDb45CC8e",
    # Silo Ethereum factory (devdocs.silo.finance — Main Silo; V3 may not be on ETH yet)
    "silo_factory_v2": "0x22a3cF6149bFa611bAFc89Fd721918EC3Cf7b581",
    "silo_factory_v3": "0x1DAb4A310447185144467076b116DAC7aec3b48F",
    "silo_factory": "0xB7d391192080674281bAAB8B3083154a5f64cd0a",  # legacy V1
    "silo_repository": "0xbACBBefda6fD1FbF5a2d6A79916F4B6124eD2D49",  # legacy V1
    "silo_llama_factory": "0x2c0fA05281730EFd3ef71172d8992500B36b56eA",
    # Gearbox V3 ContractsRegister
    "gearbox_contracts_register": "0xA50d4E7D8946a7c90652339CDBd262c375d54D99",
    # Ajna factories (faqs.ajna.finance)
    "ajna_erc20_factory": "0x6146DD43C5622bB6D12A5240ab9CF4de14eDC625",
    "ajna_erc721_factory": "0x27461199d3b7381De66a85D685828E967E35AF4c",
    # Compound V3
    "compound_v3_usdc_comet": "0xc3d688B66703497DAA19211EEdff47f25384cdc3",
    "compound_v3_usdt_comet": "0x3Afdc9BCA9213A35503b077a6072F3D0d5AB0840",
    "compound_v3_weth_comet": "0xA17581A9E3356d9A858b789D68B4d866e593aE94",
    "compound_v3_configurator": "0x316f9708bB98af7dA9c68C1C3b5e79039cD336E3",
    # Liquity V2 / BOLD CollateralRegistry (liquity/bold addresses/1.json)
    "liquity_v2_collateral_registry": "0xf949982b91c8c61e952b3ba942cbbfaef5386684",
}

# Aave V4 hubs + spokes from aave-dao/aave-address-book AaveV4Ethereum*.sol
# confirm against official deployments / on-chain code
AAVE_V4_HUBS = [
    ("core_hub", "0xCca852Bc40e560adC3b1Cc58CA5b55638ce826c9"),
    ("plus_hub", "0x06002e9c4412CB7814a791eA3666D905871E536A"),
    ("prime_hub", "0x943827DCA022D0F354a8a8c332dA1e5Eb9f9F931"),
    ("global_dollar_hub", "0x62d63197660c080236193CA60b70E49A08E90368"),
]

AAVE_V4_SPOKES = [
    ("treasury_spoke", "0xB9B0b8616f6Bf6841972a52058132BE08d723155"),
    ("bluechip_spoke", "0x973a023A77420ba610f06b3858aD991Df6d85A08"),
    ("ethena_correlated_spoke", "0x58131E79531caB1d52301228d1f7b842F26B9649"),
    ("ethena_ecosystem_spoke", "0xba1B3D55D249692b669A164024A838309B7508AF"),
    ("forex_spoke", "0xD8B93635b8C6d0fF98CbE90b5988E3F2d1Cd9da1"),
    ("gold_spoke", "0x65407b940966954b23dfA3caA5C0702bB42984DC"),
    ("lombard_btc_spoke", "0x7EC68b5695e803e98a21a9A05d744F28b0a7753D"),
    ("main_spoke", "0x94e7A5dCbE816e498b89aB752661904E2F56c485"),
    ("paxg_gold_spoke", "0xAD75cE6354f87F3135cE10621d385d8D1e2562C2"),
    ("usdg_pendle_spoke", "0x956d8e0A89cfa3744428C4641b5a53B56167a7f9"),
    ("etherfi_espoke", "0xbF10BDfE177dE0336aFD7fcCF80A904E15386219"),
    ("kelp_espoke", "0x3131FE68C4722e726fe6B2819ED68e514395B9a4"),
    ("lido_espoke", "0xe1900480ac69f0B296841Cd01cC37546d92F35Cd"),
    ("usdg_maple_espoke", "0x774b9655413c34809c1f1b16b654465A89EBE989"),
]

AAVE_V4_SPOKE_ORACLES = [
    ("bluechip_spoke_oracle", "0xdA1266a7b8620819dAE3F8bd6B546Da36e505bB8"),
    ("ethena_correlated_spoke_oracle", "0x9b91a0943CADf554742E8Fb358B1cC4ae4F85F01"),
    ("ethena_ecosystem_spoke_oracle", "0xc390dbe9fc00D6db73C52d375642b47008C33c90"),
    ("forex_spoke_oracle", "0xB3CE6E7b6d389a66eA4a3777bA07219d00FB3a9D"),
    ("gold_spoke_oracle", "0x0083421fd178749af2201ddA5A7C3feB5790B80c"),
    ("lombard_btc_spoke_oracle", "0x198Cac7f54FFc7d709Ac0FEc4B6454CE73e21D3D"),
    ("main_spoke_oracle", "0x99B2B6CEa9C3D2fd8F4d90f86741C44B212a6127"),
    ("paxg_gold_spoke_oracle", "0x8CEcC12b23ED45EC2A9b9EB57EA6974c0cae850B"),
    ("usdg_pendle_spoke_oracle", "0x692cD2F7653680aFf316Ac309ce825FCF573B7Ee"),
    ("etherfi_espoke_oracle", "0xd8B153FaAA8f2b1bC774916FEd333A4F3dE48792"),
    ("kelp_espoke_oracle", "0x37C316996C714Bf906743071e04E62220b3271ac"),
    ("lido_espoke_oracle", "0x664D73b6C3591333Fd79510f7ce9ef81228824F5"),
    ("usdg_maple_espoke_oracle", "0x47a7cC7Fd47aCed15087a8b6e0ACFddCD63C811A"),
]

# Liquity V2 branches from liquity/bold contracts/addresses/1.json
LIQUITY_V2_BRANCHES = [
    {
        "coll": "WETH",
        "troveManager": "0x7bcb64b2c9206a5b699ed43363f6f98d4776cf5a",
        "sortedTroves": "0xa25269e41bd072513849f2e64ad221e84f3063f4",
        "borrowerOperations": "0x372abd1810eaf23cb9d941bbe7596dfb2c46bc65",
        "priceFeed": "0xcc5f8102eb670c89a4a3c567c13851260303c24f",
        "troveNFT": "0x1a0fc0b843afd9140267d25d4e575cb37a838013",
        "stabilityPool": "0x5721cbbd64fc7ae3ef44a0a3f9a790a9264cf9bf",
        "activePool": "0xeb5a8c825582965f1d84606e078620a84ab16afe",
    },
    {
        "coll": "wstETH",
        "troveManager": "0xa2895d6a3bf110561dfe4b71ca539d84e1928b22",
        "sortedTroves": "0x84eb85a8c25049255614f0536bea8f31682e86f1",
        "borrowerOperations": "0xa741a32f9dcfe6adba088fd0f97e90742d7d5da3",
        "priceFeed": "0xe7aa2ba9e086a379d3beb224098bc634a46e314e",
        "troveNFT": "0x857aecebf75f1012dc18e15020c97096aea31b04",
        "stabilityPool": "0x9502b7c397e9aa22fe9db7ef7daf21cd2aebe56b",
        "activePool": "0x531a8f99c70d6a56a7cee02d6b4281650d7919a0",
    },
    {
        "coll": "rETH",
        "troveManager": "0xb2b2abeb5c357a234363ff5d180912d319e3e19e",
        "sortedTroves": "0x14d8d8011df2b396ed2bbc4959bb73250324f386",
        "borrowerOperations": "0xe8119fc02953b27a1b48d2573855738485a17329",
        "priceFeed": "0x34f1e9c7dcc279ec70d3c4488eb2d80fba8b7b2b",
        "troveNFT": "0x7ae430e25b67f19b431e1d1dc048a5bcf24c0873",
        "stabilityPool": "0xd442e41019b7f5c4dd78f50dc03726c446148695",
        "activePool": "0x9074d72cc82dad1e13e454755aa8f144c479532f",
    },
]

# Well-known deploy / first-seen blocks (optional; TODO if unknown)
KNOWN_BEFORE = {
    "0xbbbbbbbbbb9cc5e90e3b3af64bdaf62c37eeffcb": 18883124,  # Morpho Blue
    "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2": 16291127,  # Aave V3 Pool Core
    "0xc3d688b66703497daa19211eedff47f25384cdc3": 15331586,  # Compound V3 USDC
    "0xa17581a9e3356d9a858b789d68b4d866e593ae94": 17181117,  # Compound V3 WETH
    "0x316f9708bb98af7da9c68c1c3b5e79039cd336e3": 15331586,  # Compound V3 Configurator
    "0xbaa999ac55eace41ccae355c77809e68bb345170": 16291124,
    "0x5a464c28d19848f44199d003bef5ecc87d090f87": 12807083,  # IlkRegistry
    "0xf949982b91c8c61e952b3ba942cbbfaef5386684": 22283450,  # Liquity V2 approx
    "0x6146dd43c5622bb6d12a5240ab9cf4de14edc625": 18980720,  # Ajna ERC20 factory approx
    "0x29a56a1b8214d9cf7c5561811750d5cbdb45cc8e": 20529207,  # Euler factory approx
}

ZERO = "0x0000000000000000000000000000000000000000"

# Minimal ABIs
ABI_REGISTRY = [
    {
        "name": "getAddressesProvidersList",
        "inputs": [],
        "outputs": [{"type": "address[]"}],
        "stateMutability": "view",
        "type": "function",
    }
]
ABI_PROVIDER = [
    {
        "name": "getPool",
        "inputs": [],
        "outputs": [{"type": "address"}],
        "stateMutability": "view",
        "type": "function",
    },
    {
        "name": "getPriceOracle",
        "inputs": [],
        "outputs": [{"type": "address"}],
        "stateMutability": "view",
        "type": "function",
    },
]
ABI_POOL = [
    {
        "name": "getReservesList",
        "inputs": [],
        "outputs": [{"type": "address[]"}],
        "stateMutability": "view",
        "type": "function",
    },
    # Aave V3 ReserveData — use getReserveData returning raw; decode aToken/variableDebt by ABI
    {
        "name": "getReserveData",
        "inputs": [{"name": "asset", "type": "address"}],
        "outputs": [
            {
                "components": [
                    {"name": "configuration", "type": "uint256"},
                    {"name": "liquidityIndex", "type": "uint128"},
                    {"name": "currentLiquidityRate", "type": "uint128"},
                    {"name": "variableBorrowIndex", "type": "uint128"},
                    {"name": "currentVariableBorrowRate", "type": "uint128"},
                    {"name": "currentStableBorrowRate", "type": "uint128"},
                    {"name": "lastUpdateTimestamp", "type": "uint40"},
                    {"name": "id", "type": "uint16"},
                    {"name": "aTokenAddress", "type": "address"},
                    {"name": "stableDebtTokenAddress", "type": "address"},
                    {"name": "variableDebtTokenAddress", "type": "address"},
                    {"name": "interestRateStrategyAddress", "type": "address"},
                    {"name": "accruedToTreasury", "type": "uint128"},
                    {"name": "unbacked", "type": "uint128"},
                    {"name": "isolationModeTotalDebt", "type": "uint128"},
                ],
                "name": "",
                "type": "tuple",
            }
        ],
        "stateMutability": "view",
        "type": "function",
    },
]
# Newer Aave V3.2+ getReserveData layout differs; also try getReserveAToken / getReserveVariableDebtToken
ABI_POOL_V32 = [
    {
        "name": "getReserveAToken",
        "inputs": [{"name": "asset", "type": "address"}],
        "outputs": [{"type": "address"}],
        "stateMutability": "view",
        "type": "function",
    },
    {
        "name": "getReserveVariableDebtToken",
        "inputs": [{"name": "asset", "type": "address"}],
        "outputs": [{"type": "address"}],
        "stateMutability": "view",
        "type": "function",
    },
]
ABI_ORACLE = [
    {
        "name": "getSourceOfAsset",
        "inputs": [{"name": "asset", "type": "address"}],
        "outputs": [{"type": "address"}],
        "stateMutability": "view",
        "type": "function",
    },
    {
        "name": "getAssetsPrices",
        "inputs": [{"name": "assets", "type": "address[]"}],
        "outputs": [{"type": "uint256[]"}],
        "stateMutability": "view",
        "type": "function",
    },
]
ABI_AGG_PROXY = [
    {
        "name": "aggregator",
        "inputs": [],
        "outputs": [{"type": "address"}],
        "stateMutability": "view",
        "type": "function",
    }
]
ABI_ILK = [
    {
        "name": "count",
        "inputs": [],
        "outputs": [{"type": "uint256"}],
        "stateMutability": "view",
        "type": "function",
    },
    {
        "name": "list",
        "inputs": [],
        "outputs": [{"type": "bytes32[]"}],
        "stateMutability": "view",
        "type": "function",
    },
    {
        "name": "info",
        "inputs": [{"name": "ilk", "type": "bytes32"}],
        "outputs": [
            {"name": "name", "type": "string"},
            {"name": "symbol", "type": "string"},
            {"name": "class", "type": "uint256"},
            {"name": "dec", "type": "uint256"},
            {"name": "gem", "type": "address"},
            {"name": "pip", "type": "address"},
            {"name": "join", "type": "address"},
            {"name": "xlip", "type": "address"},
        ],
        "stateMutability": "view",
        "type": "function",
    },
]
ABI_FLUID_RESOLVER = [
    {
        "name": "getAllVaultsAddresses",
        "inputs": [],
        "outputs": [{"type": "address[]"}],
        "stateMutability": "view",
        "type": "function",
    },
    {
        "name": "getTotalVaults",
        "inputs": [],
        "outputs": [{"type": "uint256"}],
        "stateMutability": "view",
        "type": "function",
    },
    {
        "name": "getVaultAddress",
        "inputs": [{"name": "vaultId_", "type": "uint256"}],
        "outputs": [{"type": "address"}],
        "stateMutability": "view",
        "type": "function",
    },
]
ABI_EULER_FACTORY = [
    {
        "name": "getProxyListLength",
        "inputs": [],
        "outputs": [{"type": "uint256"}],
        "stateMutability": "view",
        "type": "function",
    },
    {
        "name": "getProxyListSlice",
        "inputs": [
            {"name": "start", "type": "uint256"},
            {"name": "end", "type": "uint256"},
        ],
        "outputs": [{"type": "address[]"}],
        "stateMutability": "view",
        "type": "function",
    },
]

ABI_SILO_FACTORY_V2 = [
    {"name": "getNextSiloId", "inputs": [], "outputs": [{"type": "uint256"}], "stateMutability": "view", "type": "function"},
    {"name": "idToSiloConfig", "inputs": [{"name": "id", "type": "uint256"}], "outputs": [{"type": "address"}], "stateMutability": "view", "type": "function"},
]
ABI_SILO_CONFIG_V2 = [
    {"name": "getSilos", "inputs": [], "outputs": [{"type": "address"}, {"type": "address"}], "stateMutability": "view", "type": "function"},
    {"name": "getShareTokens", "inputs": [{"name": "_silo", "type": "address"}], "outputs": [{"type": "address"}, {"type": "address"}, {"type": "address"}], "stateMutability": "view", "type": "function"},
]
ABI_AJNA_FACTORY = [
    {"name": "getDeployedPoolsList", "inputs": [], "outputs": [{"type": "address[]"}], "stateMutability": "view", "type": "function"},
]
ABI_MULTICALL3 = [
    {"name": "aggregate3", "inputs": [{"name": "calls", "type": "tuple[]", "components": [
        {"name": "target", "type": "address"}, {"name": "allowFailure", "type": "bool"}, {"name": "callData", "type": "bytes"}
    ]}], "outputs": [{"name": "returnData", "type": "tuple[]", "components": [
        {"name": "success", "type": "bool"}, {"name": "returnData", "type": "bytes"}
    ]}], "stateMutability": "payable", "type": "function"},
]

ABI_SILO_REPO = [
    {
        "name": "getSilos",
        "inputs": [{"name": "asset", "type": "address"}],
        "outputs": [{"type": "address[]"}],
        "stateMutability": "view",
        "type": "function",
    },
    {
        "name": "getAssets",
        "inputs": [],
        "outputs": [{"type": "address[]"}],
        "stateMutability": "view",
        "type": "function",
    },
]
ABI_SILO_CONFIG = [
    {
        "name": "getSilos",
        "inputs": [],
        "outputs": [{"type": "address"}, {"type": "address"}],
        "stateMutability": "view",
        "type": "function",
    }
]
ABI_GEARBOX = [
    {
        "name": "getCreditManagers",
        "inputs": [],
        "outputs": [{"type": "address[]"}],
        "stateMutability": "view",
        "type": "function",
    },
    {
        "name": "getPools",
        "inputs": [],
        "outputs": [{"type": "address[]"}],
        "stateMutability": "view",
        "type": "function",
    },
    {
        "name": "getCreditManagersList",
        "inputs": [],
        "outputs": [{"type": "address[]"}],
        "stateMutability": "view",
        "type": "function",
    },
    {
        "name": "getPoolsList",
        "inputs": [],
        "outputs": [{"type": "address[]"}],
        "stateMutability": "view",
        "type": "function",
    },
]
ABI_AAVE_V4_HUB = [
    {
        "name": "getAssetCount",
        "inputs": [],
        "outputs": [{"type": "uint256"}],
        "stateMutability": "view",
        "type": "function",
    },
]

# Event topic0s
TOPIC_POOL_CREATED = Web3.keccak(text="PoolCreated(address,address,address)").hex()
# Ajna may use PoolCreated(address indexed pool, ...) differently
TOPIC_AJNA_POOL_CREATED = Web3.keccak(text="PoolCreated(address)").hex()
TOPIC_NEW_SILO = Web3.keccak(
    text="NewSilo(address,address,address,address,address)"
).hex()
TOPIC_VAULT_DEPLOYED = Web3.keccak(text="VaultDeployed(address,uint256)").hex()
TOPIC_PROXY_CREATED = Web3.keccak(
    text="ProxyCreated(address,bool,address,bytes)"
).hex()


class Discoverer:
    def __init__(self, w3: Web3, sleep_s: float = 0.15):
        self.w3 = w3
        self.sleep_s = sleep_s
        self.entries: list[dict[str, Any]] = []
        self.failures: dict[str, str] = {}
        self.notes: list[str] = []
        self._seen: set[tuple[str, str]] = set()  # (protocol, address_lower)

    def _sleep(self):
        if self.sleep_s:
            time.sleep(self.sleep_s)

    def add(
        self,
        protocol: str,
        kind: str,
        address: str,
        source: str,
        before_block: Optional[int] = None,
    ):
        if not address or address.lower() == ZERO:
            return
        addr = Web3.to_checksum_address(address)
        key = (protocol, addr.lower())
        if key in self._seen:
            return
        self._seen.add(key)
        known = KNOWN_BEFORE.get(addr.lower())
        bb = before_block if before_block is not None else known
        e: dict[str, Any] = {
            "protocol": protocol,
            "kind": kind,
            "address": addr,
            "source": source,
        }
        if bb is not None:
            e["before_block"] = int(bb)
        self.entries.append(e)

    def call(self, fn, *args, retries: int = 3):
        last = None
        for i in range(retries):
            try:
                self._sleep()
                return fn(*args)
            except Exception as ex:  # noqa: BLE001
                last = ex
                time.sleep(0.5 * (i + 1))
        raise last  # type: ignore[misc]

    def has_code(self, address: str) -> bool:
        try:
            self._sleep()
            code = self.w3.eth.get_code(Web3.to_checksum_address(address))
            return code is not None and len(code) > 2
        except Exception:
            return False

    def try_aggregator(self, protocol: str, proxy: str, source: str):
        self.add(protocol, "oracle_proxy", proxy, source)
        try:
            c = self.w3.eth.contract(
                address=Web3.to_checksum_address(proxy), abi=ABI_AGG_PROXY
            )
            agg = self.call(c.functions.aggregator().call)
            if agg and agg.lower() != ZERO and agg.lower() != proxy.lower():
                self.add(protocol, "oracle_aggregator", agg, f"{source}→aggregator()")
        except Exception:
            pass  # not every source is a Chainlink proxy

    # ----- Family A: Aave-like -----
    def discover_aave_family(self, protocol: str, registry_addr: str):
        print(f"[{protocol}] registry {registry_addr}")
        try:
            reg = self.w3.eth.contract(
                address=Web3.to_checksum_address(registry_addr), abi=ABI_REGISTRY
            )
            providers = self.call(reg.functions.getAddressesProvidersList().call)
            print(f"[{protocol}] providers={len(providers)}")
            self.add(protocol, "provider_registry", registry_addr, "root")
            for p in providers:
                if not p or p.lower() == ZERO:
                    continue
                self.add(protocol, "addresses_provider", p, "registry.getAddressesProvidersList")
                try:
                    prov = self.w3.eth.contract(
                        address=Web3.to_checksum_address(p), abi=ABI_PROVIDER
                    )
                    pool = self.call(prov.functions.getPool().call)
                    oracle = self.call(prov.functions.getPriceOracle().call)
                    self.add(protocol, "pool", pool, f"provider({p}).getPool")
                    self.add(protocol, "price_oracle", oracle, f"provider({p}).getPriceOracle")
                    self._aave_pool_reserves(protocol, pool, oracle)
                except Exception as ex:  # noqa: BLE001
                    print(f"[{protocol}] provider {p} failed: {ex}")
                    self.failures[f"{protocol}:provider:{p}"] = str(ex)
        except Exception as ex:  # noqa: BLE001
            self.failures[protocol] = str(ex)
            print(f"[{protocol}] FAILED: {ex}")

    def _aave_pool_reserves(self, protocol: str, pool_addr: str, oracle_addr: str):
        pool = self.w3.eth.contract(
            address=Web3.to_checksum_address(pool_addr), abi=ABI_POOL + ABI_POOL_V32
        )
        reserves = self.call(pool.functions.getReservesList().call)
        print(f"[{protocol}] pool {pool_addr} reserves={len(reserves)}")
        oracle = self.w3.eth.contract(
            address=Web3.to_checksum_address(oracle_addr), abi=ABI_ORACLE
        )
        for asset in reserves:
            a_token = None
            v_debt = None
            try:
                a_token = self.call(pool.functions.getReserveAToken(asset).call)
                v_debt = self.call(pool.functions.getReserveVariableDebtToken(asset).call)
            except Exception:
                try:
                    rd = self.call(pool.functions.getReserveData(asset).call)
                    # tuple or AttributeDict
                    if hasattr(rd, "aTokenAddress"):
                        a_token = rd.aTokenAddress
                        v_debt = rd.variableDebtTokenAddress
                    else:
                        a_token = rd[8]
                        v_debt = rd[10]
                except Exception as ex:  # noqa: BLE001
                    print(f"[{protocol}] getReserveData({asset}) failed: {ex}")
                    continue
            if a_token:
                self.add(protocol, "aToken", a_token, f"pool.getReserve…({asset})")
            if v_debt:
                self.add(
                    protocol, "variableDebtToken", v_debt, f"pool.getReserve…({asset})"
                )
            try:
                src = self.call(oracle.functions.getSourceOfAsset(asset).call)
                if src and src.lower() != ZERO:
                    self.try_aggregator(
                        protocol, src, f"oracle.getSourceOfAsset({asset})"
                    )
            except Exception:
                pass

    # ----- Aave V4 -----
    def discover_aave_v4(self):
        protocol = "aave-v4"
        print(f"[{protocol}] hubs/spokes from address-book (confirm on-chain)")
        try:
            ok_hubs = 0
            for name, addr in AAVE_V4_HUBS:
                if self.has_code(addr):
                    self.add(protocol, "hub", addr, f"address-book:{name}")
                    ok_hubs += 1
                    # Confirm IHub signature
                    try:
                        hub = self.w3.eth.contract(
                            address=Web3.to_checksum_address(addr), abi=ABI_AAVE_V4_HUB
                        )
                        n = self.call(hub.functions.getAssetCount().call)
                        print(f"[{protocol}] hub {name} getAssetCount={n}")
                    except Exception as ex:  # noqa: BLE001
                        self.notes.append(
                            f"aave-v4 hub {name} code present but getAssetCount failed: {ex}"
                        )
                else:
                    self.notes.append(f"aave-v4 hub {name} has no code at {addr}")
            for name, addr in AAVE_V4_SPOKES:
                if self.has_code(addr):
                    self.add(protocol, "spoke", addr, f"address-book:{name}")
                else:
                    self.notes.append(f"aave-v4 spoke {name} missing code")
            for name, addr in AAVE_V4_SPOKE_ORACLES:
                if self.has_code(addr):
                    self.add(protocol, "spoke_oracle", addr, f"address-book:{name}")
                    # Try to pull per-asset sources if AaveOracle-compatible
                    try:
                        # No getReservesList on spoke oracle easily; leave oracle itself
                        pass
                    except Exception:
                        pass
            # Position managers / tokenization spokes emit relevant logs — include PMs
            for kind, addr in [
                ("giver_position_manager", "0x17A54b8d6D9C68e7fa1C7112AC998EA1BA51d11e"),
                ("taker_position_manager", "0x6c044c0D3801499bCAbfAd458B70880bc518e9F7"),
            ]:
                if self.has_code(addr):
                    self.add(protocol, kind, addr, "address-book")
            if ok_hubs == 0:
                self.failures[protocol] = "no hub code found on-chain"
            print(f"[{protocol}] hubs_ok={ok_hubs}")
        except Exception as ex:  # noqa: BLE001
            self.failures[protocol] = str(ex)

    # ----- Morpho -----
    def discover_morpho(self):
        protocol = "morpho-blue"
        addr = ROOTS["morpho_blue"]
        print(f"[{protocol}] singleton {addr}")
        if self.has_code(addr):
            self.add(protocol, "singleton", addr, "root:well-known Morpho Blue")
        else:
            self.failures[protocol] = "no code at Morpho singleton"

    # ----- Sky / Maker -----
    def discover_sky(self):
        protocol = "sky-maker"
        print(f"[{protocol}] IlkRegistry")
        try:
            reg = self.w3.eth.contract(
                address=Web3.to_checksum_address(ROOTS["ilk_registry"]), abi=ABI_ILK
            )
            self.add(protocol, "ilk_registry", ROOTS["ilk_registry"], "root")
            n = self.call(reg.functions.count().call)
            ilks = self.call(reg.functions.list().call)
            print(f"[{protocol}] ilks={len(ilks)} count={n}")
            for ilk in ilks:
                try:
                    info = self.call(reg.functions.info(ilk).call)
                    # name, symbol, class, dec, gem, pip, join, xlip
                    gem, pip, join = info[4], info[5], info[6]
                    if join and join.lower() != ZERO:
                        self.add(
                            protocol, "join", join, f"IlkRegistry.info({ilk.hex()})"
                        )
                    if pip and pip.lower() != ZERO:
                        self.add(protocol, "pip", pip, f"IlkRegistry.info({ilk.hex()})")
                        self.try_aggregator(
                            protocol, pip, f"IlkRegistry.pip({ilk.hex()})"
                        )
                    # gem only if needed — skip plain ERC-20 underlyings per Essential
                except Exception as ex:  # noqa: BLE001
                    self.notes.append(f"sky info failed for ilk: {ex}")
        except Exception as ex:  # noqa: BLE001
            self.failures[protocol] = str(ex)
            print(f"[{protocol}] FAILED: {ex}")

    # ----- Fluid -----
    def discover_fluid(self):
        protocol = "fluid"
        print(f"[{protocol}] VaultFactory / Resolver")
        try:
            factory = ROOTS["fluid_vault_factory"]
            resolver_addr = ROOTS["fluid_vault_resolver"]
            self.add(protocol, "vault_factory", factory, "root")
            # Factory is also the position NFT (ERC721)
            self.add(protocol, "position_nft", factory, "root:VaultFactory=ERC721")
            vaults: list[str] = []
            try:
                res = self.w3.eth.contract(
                    address=Web3.to_checksum_address(resolver_addr),
                    abi=ABI_FLUID_RESOLVER,
                )
                vaults = list(self.call(res.functions.getAllVaultsAddresses().call))
            except Exception as ex1:  # noqa: BLE001
                print(f"[{protocol}] getAllVaultsAddresses failed: {ex1}; trying total")
                try:
                    res = self.w3.eth.contract(
                        address=Web3.to_checksum_address(resolver_addr),
                        abi=ABI_FLUID_RESOLVER,
                    )
                    total = int(self.call(res.functions.getTotalVaults().call))
                    for i in range(1, total + 1):
                        v = self.call(res.functions.getVaultAddress(i).call)
                        if v and v.lower() != ZERO:
                            vaults.append(v)
                except Exception as ex2:  # noqa: BLE001
                    print(f"[{protocol}] resolver total failed: {ex2}")
                    self.notes.append(
                        f"fluid: resolver enumeration failed ({ex2}); "
                        "VaultDeployed log sweep deferred"
                    )
                    vaults = []
            print(f"[{protocol}] vaults={len(vaults)}")
            for v in vaults:
                self.add(protocol, "vault", v, "FluidVaultResolver / factory logs")
            if not vaults:
                self.failures[protocol] = "no vaults discovered"
        except Exception as ex:  # noqa: BLE001
            self.failures[protocol] = str(ex)

    def _logs_addresses(
        self, address: str, topic0: str, from_block: int, chunk: int = 50_000
    ) -> list[str]:
        """Paginated eth_getLogs; returns unique address from topic1 or data."""
        out: list[str] = []
        seen: set[str] = set()
        latest = self.w3.eth.block_number
        start = from_block
        t0 = topic0 if topic0.startswith("0x") else "0x" + topic0
        while start <= latest:
            end = min(start + chunk - 1, latest)
            try:
                self._sleep()
                logs = self.w3.eth.get_logs(
                    {
                        "fromBlock": start,
                        "toBlock": end,
                        "address": Web3.to_checksum_address(address),
                        "topics": [t0],
                    }
                )
                for lg in logs:
                    # Prefer indexed topic1 as address
                    addr = None
                    if len(lg["topics"]) > 1:
                        addr = "0x" + lg["topics"][1].hex()[-40:]
                    elif lg["data"] and len(lg["data"]) >= 32:
                        addr = "0x" + lg["data"].hex()[-40:]
                    if addr and addr.lower() not in seen and addr.lower() != ZERO:
                        seen.add(addr.lower())
                        out.append(Web3.to_checksum_address(addr))
            except Exception as ex:  # noqa: BLE001
                # shrink chunk on range errors
                if chunk > 5_000:
                    chunk = chunk // 2
                    print(f"  log chunk shrink→{chunk} ({ex})")
                    continue
                print(f"  getLogs {start}-{end} failed: {ex}")
            start = end + 1
        return out

    # ----- Euler V2 -----
    def discover_euler(self):
        protocol = "euler-v2"
        factory = ROOTS["euler_evault_factory"]
        print(f"[{protocol}] GenericFactory {factory}")
        try:
            self.add(protocol, "generic_factory", factory, "root:euler-interfaces")
            fac = self.w3.eth.contract(
                address=Web3.to_checksum_address(factory), abi=ABI_EULER_FACTORY
            )
            n = int(self.call(fac.functions.getProxyListLength().call))
            print(f"[{protocol}] proxy_list_length={n}")
            # Batch slices of 100
            for start in range(0, n, 100):
                end = min(start + 100, n)
                proxies = self.call(fac.functions.getProxyListSlice(start, end).call)
                for p in proxies:
                    self.add(protocol, "vault", p, "GenericFactory.getProxyListSlice")
            if n == 0:
                self.notes.append("euler-v2: empty proxy list")
        except Exception as ex:  # noqa: BLE001
            self.failures[protocol] = str(ex)
            print(f"[{protocol}] FAILED: {ex}")

    # ----- Silo -----
    def discover_silo(self):
        protocol = "silo-v2"
        print(f"[{protocol}] V2/V3 factories via idToSiloConfig")
        try:
            for kind, key, src in [
                ("factory_v2", "silo_factory_v2", "root:defillama/silo-v2"),
                ("factory_v3", "silo_factory_v3", "root:silo-contracts-v3"),
                ("factory", "silo_factory", "root:legacy-v1"),
                ("repository", "silo_repository", "root:legacy-v1"),
                ("llama_factory", "silo_llama_factory", "root:Silo Llama"),
            ]:
                addr = ROOTS.get(key)
                if not addr:
                    continue
                if self.has_code(addr):
                    self.add(protocol, kind, addr, src)

            mc = self.w3.eth.contract(
                address=Web3.to_checksum_address("0xcA11bde05977b3631167028862bE2a173976CA11"),
                abi=ABI_MULTICALL3,
            )
            id_sel = bytes(self.w3.keccak(text="idToSiloConfig(uint256)")[:4])

            def enum_factory(factory_addr: str, label: str):
                fac = self.w3.eth.contract(
                    address=Web3.to_checksum_address(factory_addr), abi=ABI_SILO_FACTORY_V2
                )
                n = int(self.call(fac.functions.getNextSiloId().call))
                print(f"[{protocol}] {label} getNextSiloId={n}")
                configs: list[str] = []
                for start in range(1, n, 250):
                    end = min(start + 250, n)
                    calls = []
                    for i in range(start, end):
                        calls.append((Web3.to_checksum_address(factory_addr), True, id_sel + int(i).to_bytes(32, "big")))
                    try:
                        results = self.call(mc.functions.aggregate3(calls).call)
                        for ok, ret in results:
                            if ok and ret and len(ret) >= 32:
                                cfg = Web3.to_checksum_address(ret[-20:])
                                if int(cfg, 16) != 0:
                                    configs.append(cfg)
                    except Exception as ex:
                        self.notes.append(f"silo {label} multicall {start}-{end}: {ex}")
                        for i in range(start, end):
                            try:
                                cfg = self.call(fac.functions.idToSiloConfig(i).call)
                                if int(cfg, 16) != 0:
                                    configs.append(cfg)
                            except Exception:
                                pass
                # dedupe
                uniq = []
                seen_c = set()
                for c in configs:
                    cl = c.lower()
                    if cl not in seen_c:
                        seen_c.add(cl)
                        uniq.append(c)
                print(f"[{protocol}] {label} configs={len(uniq)}")
                for cfg in uniq:
                    self.add(protocol, "silo_config", cfg, f"{label}.idToSiloConfig")
                    try:
                        sc = self.w3.eth.contract(
                            address=Web3.to_checksum_address(cfg), abi=ABI_SILO_CONFIG_V2
                        )
                        s0, s1 = self.call(sc.functions.getSilos().call)
                        for s in (s0, s1):
                            if int(s, 16) == 0:
                                continue
                            self.add(protocol, "silo", s, f"SiloConfig.getSilos({cfg})")
                            try:
                                coll, prot, debt = self.call(sc.functions.getShareTokens(s).call)
                                for tok, kind in (
                                    (coll, "share_collateral"),
                                    (prot, "share_protected"),
                                    (debt, "share_debt"),
                                ):
                                    if int(tok, 16) != 0 and tok.lower() != s.lower():
                                        self.add(protocol, kind, tok, f"SiloConfig.getShareTokens({s})")
                            except Exception:
                                pass
                    except Exception as ex:
                        self.notes.append(f"silo config {cfg} getSilos failed: {ex}")

            if ROOTS.get("silo_factory_v2"):
                enum_factory(ROOTS["silo_factory_v2"], "factory_v2")
            if ROOTS.get("silo_factory_v3"):
                enum_factory(ROOTS["silo_factory_v3"], "factory_v3")

            silo_count = sum(1 for e in self.entries if e["protocol"] == protocol and e["kind"] == "silo")
            print(f"[{protocol}] silos_found={silo_count}")
            if silo_count == 0:
                self.failures[protocol] = "no silos discovered after V2/V3 factory enum"
            elif protocol in self.failures:
                del self.failures[protocol]
        except Exception as ex:  # noqa: BLE001
            self.failures[protocol] = str(ex)


    def discover_liquity_v2(self):
        protocol = "liquity-v2"
        print(f"[{protocol}] branches from official bold deployments")
        try:
            cr = ROOTS["liquity_v2_collateral_registry"]
            if self.has_code(cr):
                self.add(protocol, "collateral_registry", cr, "root:liquity/bold")
            else:
                self.notes.append("liquity-v2 CollateralRegistry no code?")
            for br in LIQUITY_V2_BRANCHES:
                coll = br["coll"]
                for kind in (
                    "troveManager",
                    "sortedTroves",
                    "borrowerOperations",
                    "priceFeed",
                    "troveNFT",
                    "stabilityPool",
                    "activePool",
                ):
                    addr = br[kind]
                    if self.has_code(addr):
                        self.add(
                            protocol,
                            kind,
                            addr,
                            f"bold deployments branch={coll}",
                        )
                    else:
                        self.notes.append(f"liquity-v2 {coll}.{kind} no code")
        except Exception as ex:  # noqa: BLE001
            self.failures[protocol] = str(ex)

    # ----- Gearbox -----
    def discover_gearbox(self):
        protocol = "gearbox-v3"
        reg_addr = ROOTS["gearbox_contracts_register"]
        print(f"[{protocol}] ContractsRegister {reg_addr}")
        try:
            self.add(protocol, "contracts_register", reg_addr, "root")
            reg = self.w3.eth.contract(
                address=Web3.to_checksum_address(reg_addr), abi=ABI_GEARBOX
            )
            cms: list[str] = []
            pools: list[str] = []
            for fn_name in ("getCreditManagers", "getCreditManagersList"):
                try:
                    fn = getattr(reg.functions, fn_name)
                    cms = list(self.call(fn().call))
                    break
                except Exception:
                    continue
            for fn_name in ("getPools", "getPoolsList"):
                try:
                    fn = getattr(reg.functions, fn_name)
                    pools = list(self.call(fn().call))
                    break
                except Exception:
                    continue
            print(f"[{protocol}] credit_managers={len(cms)} pools={len(pools)}")
            for cm in cms:
                self.add(protocol, "credit_manager", cm, "ContractsRegister")
            for p in pools:
                self.add(protocol, "pool", p, "ContractsRegister")
            if not cms and not pools:
                self.failures[protocol] = "empty register lists"
        except Exception as ex:  # noqa: BLE001
            self.failures[protocol] = str(ex)

    # ----- Ajna -----
    def discover_ajna(self):
        protocol = "ajna"
        print(f"[{protocol}] factories + getDeployedPoolsList")
        try:
            f20 = ROOTS["ajna_erc20_factory"]
            f721 = ROOTS["ajna_erc721_factory"]
            self.add(protocol, "erc20_pool_factory", f20, "root:ajna deployments")
            self.add(protocol, "erc721_pool_factory", f721, "root:ajna deployments")
            for kind, factory in (("erc20_pool", f20), ("erc721_pool", f721)):
                try:
                    fac = self.w3.eth.contract(
                        address=Web3.to_checksum_address(factory), abi=ABI_AJNA_FACTORY
                    )
                    pools = list(self.call(fac.functions.getDeployedPoolsList().call))
                    print(f"[{protocol}] {kind}s={len(pools)}")
                    for p in pools:
                        if int(p, 16) != 0:
                            self.add(protocol, kind, p, f"factory.getDeployedPoolsList({factory})")
                except Exception as ex:
                    self.notes.append(f"ajna {kind} list failed: {ex}")
            pool_n = sum(
                1 for e in self.entries
                if e["protocol"] == protocol and e["kind"] in ("erc20_pool", "erc721_pool")
            )
            if pool_n == 0:
                self.failures[protocol] = "factories only; getDeployedPoolsList returned no pools"
            elif protocol in self.failures:
                del self.failures[protocol]
        except Exception as ex:  # noqa: BLE001
            self.failures[protocol] = str(ex)


    def discover_compound_v3(self):
        protocol = "compound-v3"
        print(f"[{protocol}] Comet + Configurator roots")
        try:
            for kind, key in [
                ("comet", "compound_v3_usdc_comet"),
                ("comet", "compound_v3_weth_comet"),
                ("configurator", "compound_v3_configurator"),
            ]:
                addr = ROOTS[key]
                if self.has_code(addr):
                    self.add(protocol, kind, addr, f"root:{key}")
                else:
                    self.notes.append(f"compound-v3 {key} no code")
        except Exception as ex:  # noqa: BLE001
            self.failures[protocol] = str(ex)

    def run_all(self):
        self.discover_aave_family("aave-v3", ROOTS["aave_v3_registry"])
        self.discover_aave_family("spark", ROOTS["spark_registry"])
        self.discover_aave_v4()
        self.discover_morpho()
        self.discover_sky()
        self.discover_fluid()
        self.discover_euler()
        self.discover_silo()
        self.discover_liquity_v2()
        self.discover_gearbox()
        self.discover_ajna()
        self.discover_compound_v3()

    def counts(self) -> dict[str, int]:
        c: dict[str, int] = defaultdict(int)
        for e in self.entries:
            c[e["protocol"]] += 1
        return dict(sorted(c.items()))


def write_outputs(d: Discoverer, out_dir: Path, rpc_used: str):
    out_dir.mkdir(parents=True, exist_ok=True)
    counts = d.counts()

    # JSON
    payload = {
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "chain_id": 1,
        "rpc": rpc_used,
        "tier": "Essential",
        "counts": counts,
        "failures": d.failures,
        "notes": d.notes,
        "addresses": d.entries,
        "useful_optional": [
            "Skip bulk UniV3 pool sweep unless cheap; optional second pass for "
            "route-quality backtest (GUIDE-16 Useful tier)."
        ],
    }
    json_path = out_dir / "d15_addresses.json"
    json_path.write_text(json.dumps(payload, indent=2) + "\n")
    print(f"Wrote {json_path}")

    # TOML
    toml_lines = [
        "# Generated by discover_d15_addresses.py — Essential tier for receipts_log_filter",
        "# Paste under [prune.segments.receipts_log_filter] in reth.toml",
        "# Draft — human review before sync (D15).",
        "",
        "[prune.segments.receipts_log_filter]",
    ]
    # Sort by protocol then address
    sorted_entries = sorted(d.entries, key=lambda e: (e["protocol"], e["address"].lower()))
    for e in sorted_entries:
        addr = e["address"].lower()
        comment = f"  # {e['protocol']} {e['kind']} — {e['source'][:60]}"
        if "before_block" in e:
            toml_lines.append(f'"{addr}" = {{ before = {e["before_block"]} }}{comment}')
        else:
            toml_lines.append(f'# TODO before_block for {addr}')
            toml_lines.append(f'"{addr}" = {{ before = 0 }}{comment}  # TODO: set deploy/first-seen')
    toml_path = out_dir / "d15_receipts_log_filter.toml"
    toml_path.write_text("\n".join(toml_lines) + "\n")
    print(f"Wrote {toml_path}")

    # MD summary
    md = []
    md.append("# D15 — receipts_log_filter address discovery (draft)")
    md.append("")
    md.append("**Status:** draft generated; human review before sync.")
    md.append("")
    md.append("## How generated")
    md.append("")
    md.append(
        "Script: `tools/discover_d15_addresses.py`. Walks on-chain roots per "
        "`REGISTRY.md` §3 (Family A registries + Family B factory/event enumeration). "
        "GUIDE-16 Essential tier only (markets, spokes, receipt tokens, oracle "
        "proxies + aggregators). Excludes routers, plain ERC-20 underlyings, "
        "flash-only helpers, and Balancer."
    )
    md.append("")
    md.append(f"- RPC used: `{rpc_used}`")
    md.append(f"- Generated: {payload['generated_at']}")
    md.append(f"- Total Essential addresses: **{len(d.entries)}**")
    md.append("")
    md.append("## Counts per protocol")
    md.append("")
    md.append("| Protocol | Addresses |")
    md.append("|---|---:|")
    for p, n in counts.items():
        md.append(f"| {p} | {n} |")
    md.append(f"| **total** | **{len(d.entries)}** |")
    md.append("")
    md.append("## Failures / incomplete")
    md.append("")
    if d.failures:
        for k, v in d.failures.items():
            md.append(f"- `{k}`: {v}")
    else:
        md.append("- (none recorded)")
    md.append("")
    md.append("## Notes / caveats")
    md.append("")
    md.append(
        "- Roots are cached in script constants with comment "
        '"confirm against official deployments".'
    )
    md.append(
        "- `before` blocks: known deploy heights filled where available; "
        "others marked TODO (`before = 0` placeholder — replace before sync)."
    )
    md.append(
        "- Permissionless sets (Euler, Silo, Ajna, Fluid vaults) grow over time; "
        "re-run discovery on a schedule (REGISTRY.md §3)."
    )
    md.append(
        "- Oracle aggregators: proxy + current `aggregator()` when callable; "
        "feed upgrades leave holes if only the proxy is listed."
    )
    md.append(
        "- Aave V4 hubs/spokes seeded from aave-address-book then verified "
        "`eth_getCode` / `getAssetCount` on-chain."
    )
    md.append(
        "- Morpho Blue: singleton only (one filter entry covers all markets)."
    )
    md.append(
        "- Sky/Maker: join + pip from IlkRegistry; gems (underlyings) omitted."
    )
    md.append(
        "- Useful tier (UniV3 pools): skipped — optional second pass if cheap."
    )
    if d.notes:
        md.append("")
        md.append("### Discovery notes")
        md.append("")
        for n in d.notes:
            md.append(f"- {n}")
    md.append("")
    md.append("## Artifacts")
    md.append("")
    md.append("- `d15_addresses.json` — structured list")
    md.append("- `d15_receipts_log_filter.toml` — ready-to-paste filter entries")
    md.append("- `tools/discover_d15_addresses.py` — regenerator")
    md.append("")
    md_path = out_dir / "D15-ADDRESSES.md"
    md_path.write_text("\n".join(md) + "\n")
    print(f"Wrote {md_path}")
    return counts


def pick_rpc(explicit: Optional[str]) -> str:
    candidates = []
    if explicit:
        candidates.append(explicit)
    candidates.extend(
        [
            "https://ethereum.publicnode.com",
            "https://eth.drpc.org",
            "https://rpc.mevblocker.io",
            "https://eth.merkle.io",
        ]
    )
    for url in candidates:
        try:
            w3 = Web3(Web3.HTTPProvider(url, request_kwargs={"timeout": 30}))
            bn = w3.eth.block_number
            print(f"RPC OK: {url} (block {bn})")
            return url
        except Exception as ex:  # noqa: BLE001
            print(f"RPC fail {url}: {ex}")
    raise SystemExit("No working RPC")


def main():
    ap = argparse.ArgumentParser(description="D15 receipts_log_filter address discovery")
    ap.add_argument("--rpc", default=None, help="Ethereum mainnet HTTPS RPC URL")
    ap.add_argument(
        "--out-dir",
        default="/workspace/liquidator-docs/liquidator-guides",
        help="Output directory for json/toml/md",
    )
    ap.add_argument("--sleep", type=float, default=0.12, help="Delay between RPC calls")
    args = ap.parse_args()

    rpc = pick_rpc(args.rpc)
    w3 = Web3(Web3.HTTPProvider(rpc, request_kwargs={"timeout": 60}))
    # PoA middleware not needed for mainnet
    d = Discoverer(w3, sleep_s=args.sleep)
    print("=== D15 Essential address discovery ===")
    d.run_all()
    counts = write_outputs(d, Path(args.out_dir), rpc)
    print("=== DONE ===")
    print("Counts:", json.dumps(counts, indent=2))
    if d.failures:
        print("Failures:", json.dumps(d.failures, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
