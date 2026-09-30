"""Exact port of Pendle's PT → SY market sale (`MarketMathCore.swapExactPtForSy`
with `LogExpMath` and `PMath`), pendle-core-v2-public @ 7c15b66e —
`PendleMarketV6`, the market every live mainnet PT at 2026-09-30 trades on.

Solidity int256 semantics: `/` truncates toward zero; every revert in the
market's path raises `ValueError`. The Rust port is `liq_router::pendle`.
"""
from __future__ import annotations

ONE_18 = 10**18
ONE_20 = 10**20
ONE_36 = 10**36
MAX_NATURAL_EXPONENT = 130 * ONE_18
MIN_NATURAL_EXPONENT = -41 * ONE_18
LN_36_LOWER_BOUND = ONE_18 - 10**17
LN_36_UPPER_BOUND = ONE_18 + 10**17

x0, a0 = 128000000000000000000, 38877084059945950922200000000000000000000000000000000000
x1, a1 = 64000000000000000000, 6235149080811616882910000000
x2, a2 = 3200000000000000000000, 7896296018268069516100000000000000
x3, a3 = 1600000000000000000000, 888611052050787263676000000
x4, a4 = 800000000000000000000, 298095798704172827474000
x5, a5 = 400000000000000000000, 5459815003314423907810
x6, a6 = 200000000000000000000, 738905609893065022723
x7, a7 = 100000000000000000000, 271828182845904523536
x8, a8 = 50000000000000000000, 164872127070012814685
x9, a9 = 25000000000000000000, 128402541668774148407
x10, a10 = 12500000000000000000, 113314845306682631683
x11, a11 = 6250000000000000000, 106449445891785942956

IMPLIED_RATE_TIME = 365 * 86400
MAX_MARKET_PROPORTION = ONE_18 * 96 // 100


def sdiv(a: int, b: int) -> int:
    """int256 `/`: truncates toward zero."""
    if b == 0:
        raise ValueError("div by zero")
    q = abs(a) // abs(b)
    return q if (a >= 0) == (b > 0) else -q


def exp(x: int) -> int:
    if not (MIN_NATURAL_EXPONENT <= x <= MAX_NATURAL_EXPONENT):
        raise ValueError("Invalid exponent")
    if x < 0:
        return sdiv(ONE_18 * ONE_18, exp(-x))
    if x >= x0:
        x -= x0
        first = a0
    elif x >= x1:
        x -= x1
        first = a1
    else:
        first = 1
    x *= 100
    product = ONE_20
    for xn, an in ((x2, a2), (x3, a3), (x4, a4), (x5, a5), (x6, a6), (x7, a7), (x8, a8), (x9, a9)):
        if x >= xn:
            x -= xn
            product = sdiv(product * an, ONE_20)
    series = ONE_20
    term = x
    series += term
    for k in range(2, 13):
        term = sdiv(sdiv(term * x, ONE_20), k)
        series += term
    return sdiv(sdiv(product * series, ONE_20) * first, 100)


def _ln(a: int) -> int:
    if a < ONE_18:
        return -_ln(sdiv(ONE_18 * ONE_18, a))
    s = 0
    if a >= a0 * ONE_18:
        a = sdiv(a, a0)
        s += x0
    if a >= a1 * ONE_18:
        a = sdiv(a, a1)
        s += x1
    s *= 100
    a *= 100
    for xn, an in ((x2, a2), (x3, a3), (x4, a4), (x5, a5), (x6, a6), (x7, a7), (x8, a8), (x9, a9),
                   (x10, a10), (x11, a11)):
        if a >= an:
            a = sdiv(a * ONE_20, an)
            s += xn
    z = sdiv((a - ONE_20) * ONE_20, a + ONE_20)
    z2 = sdiv(z * z, ONE_20)
    num = z
    series = num
    for k in (3, 5, 7, 9, 11):
        num = sdiv(num * z2, ONE_20)
        series += sdiv(num, k)
    series *= 2
    return sdiv(s + series, 100)


def _ln_36(x: int) -> int:
    x *= ONE_18
    z = sdiv((x - ONE_36) * ONE_36, x + ONE_36)
    z2 = sdiv(z * z, ONE_36)
    num = z
    series = num
    for k in (3, 5, 7, 9, 11, 13, 15):
        num = sdiv(num * z2, ONE_36)
        series += sdiv(num, k)
    return series * 2


def ln(a: int) -> int:
    if a <= 0:
        raise ValueError("out of bounds")
    if LN_36_LOWER_BOUND < a < LN_36_UPPER_BOUND:
        return sdiv(_ln_36(a), ONE_18)
    return _ln(a)


def div_down(a: int, b: int) -> int:
    return sdiv(a * ONE_18, b)


def sub_no_neg(a: int, b: int) -> int:
    if a < b:
        raise ValueError("negative")
    return a - b


def sy_to_asset(index: int, sy: int) -> int:
    sign = -1 if sy < 0 else 1
    return sign * (abs(sy) * index // ONE_18)


def asset_to_sy(index: int, asset: int) -> int:
    sign = -1 if asset < 0 else 1
    return sign * (abs(asset) * ONE_18 // index)


def asset_to_sy_up(index: int, asset: int) -> int:
    sign = -1 if asset < 0 else 1
    return sign * ((abs(asset) * ONE_18 + index - 1) // index)


def log_proportion(p: int) -> int:
    if p == ONE_18:
        raise ValueError("proportion must not equal one")
    return ln(div_down(p, ONE_18 - p))


def exchange_rate(total_pt, total_asset, rate_scalar, rate_anchor, net_pt) -> int:
    num = sub_no_neg(total_pt, net_pt)
    p = div_down(num, total_pt + total_asset)
    if p > MAX_MARKET_PROPORTION:
        raise ValueError("proportion too high")
    r = div_down(log_proportion(p), rate_scalar) + rate_anchor
    if r < ONE_18:
        raise ValueError("exchange rate below one")
    return r


def rate_from_implied(ln_rate: int, tte: int) -> int:
    return exp(ln_rate * tte // IMPLIED_RATE_TIME)


def sell_pt(st: dict, pt_in: int, block_time: int) -> tuple[int, int, int]:
    """`swapExactPtForSy(pt_in)` at `block_time`: `(net_sy_out, sy_fee,
    sy_to_reserve)`, raising where the market reverts."""
    expiry = st["expiry"]
    if expiry <= block_time:
        raise ValueError("expired")
    total_pt, total_sy, index = st["total_pt"], st["total_sy"], st["index"]
    net_pt = -pt_in
    if total_pt <= net_pt:
        raise ValueError("insufficient pt")
    tte = expiry - block_time
    rate_scalar = sdiv(st["scalar_root"] * IMPLIED_RATE_TIME, tte)
    if rate_scalar <= 0:
        raise ValueError("rate scalar")
    total_asset = sy_to_asset(index, total_sy)
    if total_pt == 0 or total_asset == 0:
        raise ValueError("zero totals")
    new_rate = rate_from_implied(st["last_ln_implied_rate"], tte)
    if new_rate < ONE_18:
        raise ValueError("exchange rate below one")
    anchor = new_rate - div_down(log_proportion(div_down(total_pt, total_pt + total_asset)), rate_scalar)
    fee_rate = rate_from_implied(st["ln_fee_rate_root"], tte)
    pre = exchange_rate(total_pt, total_asset, rate_scalar, anchor, net_pt)
    pre_asset = -div_down(net_pt, pre)
    fee = -sdiv(pre_asset * (ONE_18 - fee_rate), fee_rate)
    reserve = sdiv(fee * st["reserve_fee_percent"], 100)
    net_asset = pre_asset - fee
    net_sy = asset_to_sy_up(index, net_asset) if net_asset < 0 else asset_to_sy(index, net_asset)
    sy_fee = asset_to_sy(index, fee)
    sy_reserve = asset_to_sy(index, reserve)
    # _setNewMarketStateTrade
    new_pt = sub_no_neg(total_pt, net_pt)
    new_sy = sub_no_neg(total_sy, net_sy + sy_reserve)
    r = exchange_rate(new_pt, sy_to_asset(index, new_sy), rate_scalar, anchor, 0)
    if ln(r) < 0 or ln(r) * IMPLIED_RATE_TIME // tte == 0:
        raise ValueError("zero ln implied rate")
    if sy_to_asset(index, sy_fee - sy_reserve) == 0:
        raise ValueError("zero net LP fee")
    if net_sy < 0 or sy_fee < 0 or sy_reserve < 0:
        raise ValueError("negative")
    return net_sy, sy_fee, sy_reserve
