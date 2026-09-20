"""15-class completeness checklist (D15-ADDRESSES.md)."""

from __future__ import annotations

from collections import Counter
from typing import Any


def _has_kind(entries: list[dict], kinds: set[str], protocols: set[str] | None = None) -> int:
    n = 0
    for e in entries:
        if e.get("kind") not in kinds:
            continue
        if protocols and e.get("protocol") not in protocols:
            continue
        n += 1
    return n


def evaluate(entries: list[dict], failures: dict[str, str]) -> list[dict[str, Any]]:
    """Return list of {class, name, pass, detail}."""
    kinds = Counter((e["protocol"], e["kind"]) for e in entries)
    flat_kinds = Counter(e["kind"] for e in entries)

    rows: list[dict[str, Any]] = []

    def row(n: int, name: str, ok: bool, detail: str):
        rows.append({"class": n, "name": name, "pass": ok, "detail": detail})

    agg = flat_kinds.get("oracle_aggregator", 0) + flat_kinds.get("oracle_aggregator_phase", 0)
    unresolved = sum(
        1
        for k in failures
        if k.startswith("oracle-unresolved:") or k.startswith("compound-v2:") or k.startswith("fluid:")
    )
    row(1, "OCR aggregators (recursive)", agg > 0, f"{agg} aggregator rows; unresolved in failures: {unresolved}")

    phase = flat_kinds.get("oracle_aggregator_phase", 0)
    row(2, "Historical phase aggregators", phase > 0, f"{phase} phase aggregator rows")

    v4 = _has_kind(entries, {"oracle_source", "spoke_oracle"}, {"aave-v4"})
    row(3, "Aave V4 spoke-oracle sources", v4 > 0, f"{v4} aave-v4 oracle/spoke_oracle rows")

    c3 = _has_kind(entries, {"comet", "oracle_source"}, {"compound-v3"})
    c3_step_ok = "step:compound-v3" not in failures
    row(4, "Compound V3 comets + feeds", c3 > 0 and c3_step_ok, f"{c3} compound-v3 rows; step_ok={c3_step_ok}")

    morpho_o = _has_kind(entries, {"market_oracle", "irm"}, {"morpho-blue"})
    row(5, "Morpho market oracles + IRMs", morpho_o > 0, f"{morpho_o} morpho oracle/irm rows")

    euler_o = _has_kind(entries, {"oracle_adapter", "oracle_router", "oracle_source"}, {"euler-v2"})
    row(6, "Euler router + adapters", euler_o > 0, f"{euler_o} euler-v2 oracle rows")

    gb = _has_kind(entries, {"price_oracle", "oracle_source", "credit_manager", "pool"}, {"gearbox-v3"})
    row(7, "Gearbox price oracle + feeds", gb > 0, f"{gb} gearbox-v3 rows")

    silo_o = _has_kind(entries, {"oracle_source"}, {"silo-v2"})
    row(8, "Silo solvency/maxLtv oracles", silo_o > 0, f"{silo_o} silo-v2 oracle_source rows")

    liq_pf = _has_kind(entries, {"priceFeed"}, {"liquity-v2"})
    liq_agg = _has_kind(entries, {"oracle_aggregator", "oracle_aggregator_phase"}, {"liquity-v2"})
    # Aggregators are already in the filter via Aave V3 (add() dedup); PriceFeeds are the Liquity-owned gap.
    row(
        9,
        "Liquity underlying aggregators",
        liq_pf >= 3 or liq_agg > 0,
        f"{liq_pf} liquity-v2 priceFeed rows; {liq_agg} liquity-v2 aggregator rows",
    )

    sky = _has_kind(entries, {"dog", "clipper", "pip", "vat", "jug", "pot", "spotter", "dss_flash"}, {"sky-maker"})
    row(10, "Sky Dog + Clippers + medianizers", sky > 0, f"{sky} sky-maker core rows")

    flash = _has_kind(entries, {"pool_manager", "dss_flash", "pool", "singleton"}, None)
    row(11, "Flash sources", flash > 0, f"pool_manager/dss_flash/singleton/pool rows contributing to flash")

    pools = kinds.get(("uniswap-v3", "pool"), 0) + kinds.get(("curve", "pool"), 0) + kinds.get(("kyber-elastic", "pool"), 0)
    row(12, "Exit venues UniV3/Curve/Kyber", pools > 0, f"{pools} DEX pool rows")

    rp = sum(
        1
        for e in entries
        if e.get("protocol") == "rate-provider" or (e.get("protocol") == "morpho-blue" and e.get("kind") == "rate_provider")
    )
    row(13, "Rate providers", rp >= 12, f"{rp} rate-provider rows (expect ≥12)")

    pyth = sum(1 for e in entries if e.get("protocol") == "oracle-network")
    row(14, "Push oracle networks (Pyth)", pyth > 0, f"{pyth} oracle-network rows")

    erc20 = flat_kinds.get("erc20", 0)
    row(15, "Tracked ERC-20 underlyings", erc20 > 0, f"{erc20} asset/erc20 rows")

    return rows
