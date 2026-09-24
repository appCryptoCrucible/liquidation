"""Split each Executor.execute() in a `forge test --isolate -vvvv` trace into
swap gas and everything else ("fixed": flash wrap + executor logic + the
protocol's liquidation call + profit transfer).

    forge test --match-test ... --isolate -vvvv > fork-trace.txt
    python tools/gas-measure/fork_decompose.py tools/gas-measure/fork-trace.txt
"""

import re
import sys

FRAME = re.compile(r"^(?P<indent>[ │├└─]*)\[(?P<gas>\d+)\] (?P<to>[^:]+)::(?P<fn>\w+)\(")
LIQ = {"liquidationCall", "liquidate", "liquidateBorrow", "batchLiquidateTroves", "partiallyLiquidateCreditAccount", "liquidateCreditAccount"}


def depth(indent):
    return len(indent)


def main(path):
    lines = open(path, encoding="utf-8", errors="replace").read().splitlines()
    test = None
    out = []
    i = 0
    while i < len(lines):
        line = lines[i]
        if line.startswith("[PASS]") or line.startswith("[FAIL]"):
            test = line.split()[1]
        m = FRAME.match(line)
        if m and m.group("to").endswith("Executor") and m.group("fn") == "execute":
            d0 = depth(m.group("indent"))
            total = int(m.group("gas"))
            swaps, liq = [], None
            j = i + 1
            while j < len(lines):
                n = FRAME.match(lines[j])
                if n:
                    if depth(n.group("indent")) <= d0:
                        break
                    if n.group("fn") in ("swap", "exchange"):
                        swaps.append(int(n.group("gas")))
                    elif n.group("fn") in LIQ and liq is None:
                        liq = int(n.group("gas"))
                j += 1
            out.append((test, total, liq, swaps))
            i = j
            continue
        i += 1
    for test, total, liq, swaps in out:
        fixed = total - sum(swaps)
        print(f"{test}: execute={total} liq_frame={liq} swaps={swaps} fixed(non-swap)={fixed}")


if __name__ == "__main__":
    main(sys.argv[1])
