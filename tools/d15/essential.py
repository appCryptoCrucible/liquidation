"""Build Essential-tier D15 rows from committed registry/registry.json."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from web3 import Web3

from .roots import KNOWN_BEFORE, ROOTS, ZERO


def _cs(addr: str) -> str:
    return Web3.to_checksum_address(addr)


def _before(addr: str, deployed: int = 0) -> int:
    if deployed and deployed > 0:
        return deployed
    return KNOWN_BEFORE.get(addr.lower(), 0)


def essential_from_registry(reg: dict[str, Any]) -> list[dict[str, Any]]:
    entries: list[dict[str, Any]] = []
    seen: set[str] = set()

    def add(protocol: str, kind: str, address: str, source: str, deployed: int = 0):
        if not address or address.lower() == ZERO:
            return
        addr = _cs(address)
        k = addr.lower()
        if k in seen:
            return
        seen.add(k)
        bb = _before(addr, deployed)
        entries.append(
            {
                "protocol": protocol,
                "kind": kind,
                "address": addr,
                "source": source,
                "before_block": bb,
            }
        )

    for proxy, info in reg.get("oracles", {}).items():
        src = info.get("source", "registry.oracles")
        fam = src.split(":")[0] if ":" in src else "chainlink"
        proto = fam if fam in ("aave-v3", "spark") else "aave-v3"
        pair = info.get("pair", "")
        asset_hint = ""
        if "/" in pair:
            asset_hint = pair.split("/", 1)[1]
        add(
            proto,
            "oracle_proxy",
            proxy,
            f"oracle.getSourceOfAsset(0x{asset_hint}…)" if asset_hint else f"registry.oracles:{src}",
        )
        agg = info.get("aggregator")
        if agg:
            add(proto, "oracle_aggregator", agg, f"{proxy[:10]}→aggregator()", 0)

    protocols = reg.get("protocols", {})
    silo_configs: set[str] = set()

    for _key, p in protocols.items():
        fam = p.get("family", "")
        market = p.get("market") or p.get("comptroller")
        dep = int(p.get("deployed_block") or 0)

        if fam in ("aave-v3", "spark"):
            if market:
                add(fam, "pool", market, f"registry:{fam}.market", dep)
            ap = p.get("addresses_provider")
            if ap:
                add(fam, "addresses_provider", ap, "registry.addresses_provider", dep)
            po = p.get("price_oracle")
            if po:
                add(fam, "price_oracle", po, "registry.price_oracle", dep)
            for rt in p.get("receipt_tokens") or []:
                add(fam, "aToken", rt, f"registry.receipt_tokens({market[:10] if market else '?'})", dep)
            for ad in p.get("oracle_adapters") or []:
                add(fam, "oracle_proxy", ad, f"registry.oracle_adapters", dep)

        elif fam == "aave-v4":
            kind = p.get("kind", "spoke")
            if market:
                add("aave-v4", kind, market, f"registry.aave-v4.{kind}", dep)
            for ha in p.get("hub_assets") or []:
                add("asset", "erc20", ha, "registry.aave-v4.hub_assets", 0)
            asset = p.get("asset")
            if asset:
                add("asset", "erc20", asset, "registry.aave-v4.asset", 0)

        elif fam == "compound-v2":
            if market:
                add("compound-v2", "comptroller", market, "registry.comptroller", dep)
            for rt in p.get("receipt_tokens") or []:
                add("compound-v2", "cToken", rt, f"registry.receipt_tokens", dep)

        elif fam == "morpho-blue":
            for ad in p.get("oracle_adapters") or []:
                if ad.lower() != ZERO:
                    add("morpho-blue", "market_oracle", ad, "registry.oracle_adapters", 0)

        elif fam == "euler-v2":
            if market:
                add("euler-v2", "vault", market, "registry.euler-v2.vault", dep)

        elif fam == "silo-v2":
            if market:
                cfg = p.get("silo_config", "")
                add("silo-v2", "silo", market, f"SiloConfig({cfg}).getSilos", dep)
            cfg = p.get("silo_config")
            if cfg:
                silo_configs.add(cfg.lower())

        elif fam == "ajna":
            if market:
                add("ajna", "pool", market, "registry.ajna.pool", dep)
            if p.get("pool_kind") == "erc721":
                ct = p.get("collateral_token")
                if ct:
                    add(
                        "ajna",
                        "erc721_collateral",
                        ct,
                        f"registry.ajna.pool({market[:10] if market else '?'})",
                        0,
                    )

        elif fam == "sky-maker":
            if "ilk-registry" in _key or p.get("ilk_count"):
                add("sky-maker", "ilk_registry", market or ROOTS["ilk_registry"], "registry.ilk_registry", dep)
            for rt in p.get("receipt_tokens") or []:
                add("sky-maker", "join", rt, "registry.receipt_tokens (join)", dep)
            for pip in p.get("oracle_adapters") or []:
                add("sky-maker", "pip", pip, "registry.oracle_adapters (pip)", dep)

    for cfg in sorted(silo_configs):
        add("silo-v2", "silo_config", cfg, "registry.silo_config", 0)

    add("morpho-blue", "singleton", ROOTS["morpho_blue"], "root:Morpho Blue", KNOWN_BEFORE.get(ROOTS["morpho_blue"].lower(), 0))
    add("compound-v3", "configurator", ROOTS["compound_v3_configurator"], "root:Compound V3 Configurator", KNOWN_BEFORE.get(ROOTS["compound_v3_configurator"].lower(), 0))

    add("liquity-v2", "collateral_registry", ROOTS["liquity_v2_collateral_registry"], "root:liquity/bold", KNOWN_BEFORE.get(ROOTS["liquity_v2_collateral_registry"].lower(), 0))

    for pool_addr, pinfo in reg.get("pools", {}).items():
        venue = pinfo.get("venue", "univ3")
        proto = {"univ3": "uniswap-v3", "curve": "curve", "kyber": "kyber-elastic"}.get(venue, venue)
        add(proto, "pool", pool_addr, f"registry.pools[{venue}]", int(pinfo.get("deployed_block") or 0))

    for tok in reg.get("tokens", {}):
        add("asset", "erc20", tok, "registry.tokens", 0)

    return entries


def load_registry(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))
