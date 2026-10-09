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


def callback_gas(lines, at, d):
    """Gas of the `uniswapV3SwapCallback` frame directly inside the frame at
    `lines[at]` (depth `d`): a flash swap's. Zero for an ordinary swap, whose
    callback only pays the pool and is the pool's own cost."""
    child = None  # the trace nests a frame's children at one indent step
    for k in range(at + 1, len(lines)):
        n = FRAME.match(lines[k])
        if not n:
            continue
        dk = depth(n.group("indent"))
        if dk <= d:
            return 0
        if child is None:
            child = dk
        if dk == child and n.group("fn") == "uniswapV3SwapCallback":
            gas = int(n.group("gas"))
            # Our own transfer back to the pool is the pool's cost either way.
            return gas if gas > 20_000 else 0
    return 0


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
                        # A flash swap's lender frame holds our callback, and
                        # in it the liquidation and the other swaps: count
                        # the pool's own work only (the frame net of the
                        # callback), the rest is found as frames of its own.
                        swaps.append(int(n.group("gas")) - callback_gas(lines, j, depth(n.group("indent"))))
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
