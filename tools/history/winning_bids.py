"""What liquidation winners paid the builder, per protocol.

    python tools/history/winning_bids.py <bids.jsonl> [--min-prize-eth 0.001]

Reads the lines `winning_bids` (crates/liq-replay/tests/historical_liquidations.rs)
writes: each winning liquidation replayed alone on the state before it, with
`captured` (the winner's prize before paying the builder, after the base
fee, priced by Aave's oracle at the tip) and `to_builder` (all the block's
fee recipient got from the transaction: priority fee and direct payment).

The bid share is `to_builder / captured`. Counted only where the replay
reproduced the winner (`success`), every token that moved was priced
(`unpriced` empty: otherwise the prize is understated and the share
overstated) and the prize is at least `--min-prize-eth` (a dust prize makes
the share meaningless). The rest are counted by reason.
"""

import argparse
import json
import statistics
from collections import defaultdict


def q(xs, p):
    xs = sorted(xs)
    if not xs:
        return float("nan")
    k = (len(xs) - 1) * p
    lo = int(k)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("bids")
    ap.add_argument("--min-prize-eth", type=float, default=0.001)
    a = ap.parse_args()
    rows = defaultdict(list)
    skipped = defaultdict(lambda: defaultdict(int))
    for line in open(a.bids, encoding="utf-8"):
        r = json.loads(line)
        fam = r.get("family") or "?"
        if "err" in r:
            skipped[fam]["replay failed"] += 1
            continue
        if not r["success"]:
            skipped[fam]["reverted in replay"] += 1
            continue
        if r["unpriced"]:
            skipped[fam]["unpriced token"] += 1
            continue
        cap = int(r["captured"]) / 1e18
        tb = int(r["to_builder"]) / 1e18
        if cap < a.min_prize_eth:
            skipped[fam]["prize under minimum"] += 1
            continue
        rows[fam].append((cap, tb))

    print(f"Bid share = paid to builder / prize. Prizes under {a.min_prize_eth} ETH left out.\n")
    hdr = f"{'protocol':14} {'n':>5} {'median':>7} {'mean':>7} {'p25':>6} {'p75':>6} {'weighted':>8} {'median prize':>13} {'median paid':>12} {'total prize':>12}"
    print(hdr)
    print("-" * len(hdr))
    for fam in sorted(rows, key=lambda f: -len(rows[f])):
        rs = rows[fam]
        shares = [tb / cap for cap, tb in rs]
        tot_cap = sum(c for c, _ in rs)
        tot_tb = sum(t for _, t in rs)
        print(
            f"{fam:14} {len(rs):5d} {statistics.median(shares):7.1%} {statistics.mean(shares):7.1%} "
            f"{q(shares, .25):6.1%} {q(shares, .75):6.1%} {tot_tb / tot_cap:8.1%} "
            f"{statistics.median(c for c, _ in rs):13.5f} {statistics.median(t for _, t in rs):12.5f} {tot_cap:12.3f}"
        )
    print("\nLeft out:")
    for fam in sorted(skipped):
        print(f"  {fam:14} " + ", ".join(f"{k} {v}" for k, v in sorted(skipped[fam].items())))


if __name__ == "__main__":
    main()
