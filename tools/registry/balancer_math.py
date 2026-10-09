"""Balancer V2 weighted-pool swap math, exact integer ports.

`LogExpMath.sol`, `FixedPoint.sol` and `WeightedMath._calcOutGivenIn` /
`_calcInGivenOut` of `balancer-v2-monorepo`, as the deployed pools run them
(`WeightedPool` v4 and `WeightedPool2Tokens`, read from verified source via
Sourcify 2026-10-08). Discovery checks each admitted pool against
`BalancerQueries.querySwap`, and the Rust port (`liq_router::balancer`) is
checked against the same chain answers; this file is the independent second
implementation both are compared with.

Solidity 0.7 `/` on int256 truncates toward zero and arithmetic does not
check overflow; every operation the contracts could overflow is guarded by
`_require`, so a range error here is `Revert`.
"""
from __future__ import annotations

ONE = 10**18
MAX_POW_RELATIVE_ERROR = 10_000
MAX_RATIO = 3 * 10**17

ONE_20 = 10**20
ONE_36 = 10**36
MAX_NATURAL_EXPONENT = 130 * 10**18
MIN_NATURAL_EXPONENT = -41 * 10**18
LN_36_LOWER_BOUND = ONE - 10**17
LN_36_UPPER_BOUND = ONE + 10**17
MILD_EXPONENT_BOUND = 2**254 // ONE_20

A = [
    38877084059945950922200000000000000000000000000000000000,
    6235149080811616882910000000,
    7896296018268069516100000000000000,
    888611052050787263676000000,
    298095798704172827474000,
    5459815003314423907810,
    738905609893065022723,
    271828182845904523536,
    164872127070012814685,
    128402541668774148407,
    113314845306682631683,
    106449445891785942956,
]
# x0 and x1 are 18-decimal values, x2.. are 20-decimal ones (the contract's
# `x0 = 128000000000000000000` is 2^7 with 18 decimals, `x2 = 32e20` 2^5 with 20).
X = [128 * 10**18, 64 * 10**18] + [
    32 * 10**20, 16 * 10**20, 8 * 10**20, 4 * 10**20, 2 * 10**20, 10**20,
    5 * 10**19, 25 * 10**18, 125 * 10**17, 625 * 10**16,
]


class Revert(Exception):
    pass


def tdiv(a: int, b: int) -> int:
    """Solidity int256 `/`: truncate toward zero."""
    if b == 0:
        raise Revert("division by zero")
    q = abs(a) // abs(b)
    return q if (a >= 0) == (b > 0) else -q


def trem(a: int, b: int) -> int:
    """Solidity int256 `%`: the sign of the dividend."""
    if b == 0:
        raise Revert("modulo by zero")
    r = abs(a) % abs(b)
    return r if a >= 0 else -r


def _ln_36(x: int) -> int:
    x *= ONE
    z = tdiv((x - ONE_36) * ONE_36, x + ONE_36)
    z_squared = tdiv(z * z, ONE_36)
    num = z
    series = num
    for d in (3, 5, 7, 9, 11, 13, 15):
        num = tdiv(num * z_squared, ONE_36)
        series += tdiv(num, d)
    return series * 2


def _ln(a: int) -> int:
    if a < ONE:
        return -_ln(tdiv(ONE * ONE, a))
    s = 0
    if a >= A[0] * ONE:
        a = tdiv(a, A[0])
        s += X[0]
    if a >= A[1] * ONE:
        a = tdiv(a, A[1])
        s += X[1]
    s *= 100
    a *= 100
    for n in range(2, 12):
        if a >= A[n]:
            a = tdiv(a * ONE_20, A[n])
            s += X[n]
    z = tdiv((a - ONE_20) * ONE_20, a + ONE_20)
    z_squared = tdiv(z * z, ONE_20)
    num = z
    series = num
    for d in (3, 5, 7, 9, 11):
        num = tdiv(num * z_squared, ONE_20)
        series += tdiv(num, d)
    series *= 2
    return tdiv(s + series, 100)


def exp(x: int) -> int:
    if not (MIN_NATURAL_EXPONENT <= x <= MAX_NATURAL_EXPONENT):
        raise Revert("INVALID_EXPONENT")
    if x < 0:
        return tdiv(ONE * ONE, exp(-x))
    if x >= X[0]:
        x -= X[0]
        first_an = A[0]
    elif x >= X[1]:
        x -= X[1]
        first_an = A[1]
    else:
        first_an = 1
    x *= 100
    product = ONE_20
    for n in range(2, 10):
        if x >= X[n]:
            x -= X[n]
            product = tdiv(product * A[n], ONE_20)
    series = ONE_20 + x
    term = x
    for n in range(2, 13):
        term = tdiv(tdiv(term * x, ONE_20), n)
        series += term
    return tdiv(tdiv(product * series, ONE_20) * first_an, 100)


def log_exp_pow(x: int, y: int) -> int:
    if y == 0:
        return ONE
    if x == 0:
        return 0
    if x >> 255 != 0:
        raise Revert("X_OUT_OF_BOUNDS")
    if y >= MILD_EXPONENT_BOUND:
        raise Revert("Y_OUT_OF_BOUNDS")
    if LN_36_LOWER_BOUND < x < LN_36_UPPER_BOUND:
        ln_36_x = _ln_36(x)
        logx_times_y = tdiv(ln_36_x, ONE) * y + tdiv(trem(ln_36_x, ONE) * y, ONE)
    else:
        logx_times_y = _ln(x) * y
    logx_times_y = tdiv(logx_times_y, ONE)
    if not (MIN_NATURAL_EXPONENT <= logx_times_y <= MAX_NATURAL_EXPONENT):
        raise Revert("PRODUCT_OUT_OF_BOUNDS")
    return exp(logx_times_y)


# ---- FixedPoint ----

def mul_down(a: int, b: int) -> int:
    return a * b // ONE


def mul_up(a: int, b: int) -> int:
    p = a * b
    return 0 if p == 0 else (p - 1) // ONE + 1


def div_down(a: int, b: int) -> int:
    if b == 0:
        raise Revert("ZERO_DIVISION")
    return 0 if a == 0 else a * ONE // b


def div_up(a: int, b: int) -> int:
    if b == 0:
        raise Revert("ZERO_DIVISION")
    return 0 if a == 0 else (a * ONE - 1) // b + 1


def complement(x: int) -> int:
    return ONE - x if x < ONE else 0


def pow_up(x: int, y: int, fast: bool) -> int:
    if fast:
        if y == ONE:
            return x
        if y == 2 * ONE:
            return mul_up(x, x)
        if y == 4 * ONE:
            sq = mul_up(x, x)
            return mul_up(sq, sq)
    raw = log_exp_pow(x, y)
    return raw + mul_up(raw, MAX_POW_RELATIVE_ERROR) + 1


# ---- WeightedMath ----

def calc_out_given_in(bal_in, w_in, bal_out, w_out, amount_in, fast):
    if amount_in > mul_down(bal_in, MAX_RATIO):
        raise Revert("MAX_IN_RATIO")
    base = div_up(bal_in, bal_in + amount_in)
    exponent = div_down(w_in, w_out)
    power = pow_up(base, exponent, fast)
    return mul_down(bal_out, complement(power))


def calc_in_given_out(bal_in, w_in, bal_out, w_out, amount_out, fast):
    if amount_out > mul_down(bal_out, MAX_RATIO):
        raise Revert("MAX_OUT_RATIO")
    base = div_up(bal_out, bal_out - amount_out)
    exponent = div_up(w_out, w_in)
    power = pow_up(base, exponent, fast)
    if power < ONE:
        raise Revert("SUB_OVERFLOW")
    return mul_up(bal_in, power - ONE)


# ---- the pool (`BasePool.onSwap`) ----

def swap_given_in(i, j, dx, balances, weights, scaling, fee, fast):
    """Vault `swap(GIVEN_IN)` of `dx` of token `i` into `j`. `scaling[k]` is
    `10 ** (18 - decimals_k)`."""
    bi, bj = balances[i] * scaling[i], balances[j] * scaling[j]
    amount = (dx - mul_up(dx, fee)) * scaling[i]
    out = calc_out_given_in(bi, weights[i], bj, weights[j], amount, fast)
    return out // scaling[j]


def swap_given_out(i, j, dy, balances, weights, scaling, fee, fast):
    """Vault `swap(GIVEN_OUT)` buying `dy` of token `j` with token `i`."""
    bi, bj = balances[i] * scaling[i], balances[j] * scaling[j]
    amount = dy * scaling[j]
    raw = calc_in_given_out(bi, weights[i], bj, weights[j], amount, fast)
    down = 0 if raw == 0 else (raw - 1) // scaling[i] + 1
    return div_up(down, complement(fee))
