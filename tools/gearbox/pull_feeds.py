"""Which live Gearbox v3.1 debt depends on pull (on-demand) price feeds.

Walks address provider -> market configurators -> contracts registers ->
credit managers (version 310..399) -> price oracle -> every token's main feed,
recursing through composite/wrapper feeds (priceFeed0/1, priceFeed,
underlyingPriceFeed) to leaves. A leaf whose contractType is
PRICE_FEED::REDSTONE or PRICE_FEED::PYTH, or that answers updatable() = true,
is a pull feed. Then, for every manager with debt, lists accounts whose
enabled-token mask includes a pull-fed token: that is the debt a liquidation
could only reach with a signed price payload.

`PriceOracleV3.getUpdatablePriceFeeds()` does not exist on v3.1 (reverts),
so the walk is by contractType.

Usage: MAINNET_RPC_URL=... python tools/gearbox/pull_feeds.py [out.json]
Needs `cast` (Foundry) on PATH. Slow on a rate-limited RPC (~20 min).
"""
import json
import os
import subprocess
import sys
import time

RPC = os.environ["MAINNET_RPC_URL"]
ADDRESS_PROVIDER = "0xF7f0a609BfAb9a0A98786951ef10e5FE26cC1E38"
ZERO = "0x0000000000000000000000000000000000000000"


def call(to, sig, *args):
    for _ in range(4):
        p = subprocess.run(["cast", "call", to, sig, *args, "--rpc-url", RPC],
                           capture_output=True, text=True)
        if p.returncode == 0:
            return p.stdout.strip()
        if "revert" in p.stderr.lower():
            return None
        time.sleep(1)
    return None


def addrs(s):
    return [] if not s else [a for a in s.strip("[]").replace(",", " ").split() if a]


def first_int(s):
    return int(s.split()[0]) if s else 0


def ctype(feed):
    raw = call(feed, "contractType()(bytes32)")
    if not raw:
        return "?"
    try:
        return bytes.fromhex(raw[2:]).rstrip(b"\0").decode()
    except ValueError:
        return raw


def pull_leaves(feed, depth=0):
    kids = []
    for sig in ("priceFeed0()(address)", "priceFeed1()(address)",
                "priceFeed()(address)", "underlyingPriceFeed()(address)"):
        k = call(feed, sig)
        if k and k != ZERO:
            kids.append(k)
    if kids and depth < 5:
        return [x for k in kids for x in pull_leaves(k, depth + 1)]
    t = ctype(feed)
    if "REDSTONE" in t or "PYTH" in t or call(feed, "updatable()(bool)") == "true":
        return [(t, feed)]
    return []


def main():
    key = subprocess.run(["cast", "--format-bytes32-string", "MARKET_CONFIGURATOR_FACTORY"],
                         capture_output=True, text=True).stdout.strip()
    factory = call(ADDRESS_PROVIDER, "getAddressOrRevert(bytes32,uint256)(address)", key, "0")
    managers = []
    for mc in addrs(call(factory, "getMarketConfigurators()(address[])")):
        reg = call(mc, "contractsRegister()(address)")
        for cm in addrs(call(reg, "getCreditManagers()(address[])")) if reg else []:
            if not 310 <= first_int(call(cm, "version()(uint256)")) <= 399:
                continue
            pool = call(cm, "pool()(address)")
            debt = first_int(call(pool, "creditManagerBorrowed(address)(uint256)", cm)) if pool else 0
            managers.append(dict(manager=cm, oracle=call(cm, "priceOracle()(address)"), debt_raw=debt))

    feeds = {}  # (oracle, token) -> pull leaves
    exposed = []
    for m in managers:
        if m["debt_raw"] == 0:
            continue
        cm = m["manager"]
        n = first_int(call(cm, "collateralTokensCount()(uint8)"))
        pull_masks = {}
        for i in range(n):
            tok = call(cm, "getTokenByMask(uint256)(address)", str(1 << i))
            if not tok:
                continue
            k = (m["oracle"], tok)
            if k not in feeds:
                main_feed = call(m["oracle"], "priceFeeds(address)(address)", tok)
                feeds[k] = pull_leaves(main_feed) if main_feed else []
            if feeds[k]:
                pull_masks[1 << i] = (tok, feeds[k])
        if not pull_masks:
            continue
        for ca in addrs(call(cm, "creditAccounts()(address[])")):
            enabled = first_int(call(cm, "enabledTokensMaskOf(address)(uint256)", ca))
            hits = [v for mask, v in pull_masks.items() if enabled & mask]
            if hits:
                exposed.append(dict(manager=cm, account=ca, tokens=hits))
                print("EXPOSED", cm, ca, [h[0] for h in hits], flush=True)

    out = dict(managers=managers, exposed_accounts=exposed,
               pull_fed={f"{o}:{t}": v for (o, t), v in feeds.items() if v})
    print(f"managers with debt: {sum(1 for m in managers if m['debt_raw'])}; "
          f"accounts needing a pull payload: {len(exposed)}")
    if len(sys.argv) > 1:
        json.dump(out, open(sys.argv[1], "w", encoding="utf-8"), indent=1)


if __name__ == "__main__":
    main()
