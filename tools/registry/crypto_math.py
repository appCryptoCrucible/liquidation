"""Exact integer ports of Curve's crypto-pool swap math, for discovery.

- twocrypto-ng: `CurveTwocryptoMathOptimized.vy` (v2.0.0 / v2.1.0, the MATH
  contracts 0x2005…64df / 0x1fd8…f4a1) and `CurveTwocryptoOptimized.vy`
  `_exchange` / `_fee`.
- tricrypto-ng: `CurveTricryptoMathOptimized.vy` (v2.0.0, 0xcbff…d6ee) and
  `CurveTricryptoOptimizedWETH.vy` `_exchange` / `_fee`.

Vyper semantics: `uint256` arithmetic is checked (a violation raises `Revert`),
`unsafe_*` wraps, `int256` `/` truncates toward zero. The bot's Rust solver
(`liq-router`, `crypto.rs`) is a line-for-line port of this file; discovery
admits a pool only when this reproduces the pool's own `get_dy`.
"""
from __future__ import annotations

M = 2**256
I_MAX = 2**255 - 1
I_MIN = -(2**255)


class Revert(Exception):
    pass


def uc(x: int) -> int:
    """Checked uint256."""
    if x < 0 or x >= M:
        raise Revert("uint256 range")
    return x


def ic(x: int) -> int:
    """Checked int256."""
    if x < I_MIN or x > I_MAX:
        raise Revert("int256 range")
    return x


def uw(x: int) -> int:
    """Wrapping uint256 (`unsafe_*`)."""
    return x % M


def iw(x: int) -> int:
    """Wrapping int256 (`unsafe_*`)."""
    x %= M
    return x - M if x > I_MAX else x


def udiv(a: int, b: int) -> int:
    if b == 0:
        raise Revert("div by zero")
    return a // b


def udiv_unsafe(a: int, b: int) -> int:
    return 0 if b == 0 else a // b


def tdiv(a: int, b: int) -> int:
    """int256 `/`: truncates toward zero; checked."""
    if b == 0:
        raise Revert("div by zero")
    if a == I_MIN and b == -1:
        raise Revert("int256 overflow")
    q = abs(a) // abs(b)
    return q if (a >= 0) == (b > 0) else -q


def tdiv_unsafe(a: int, b: int) -> int:
    if b == 0:
        return 0
    q = abs(a) // abs(b)
    return iw(q if (a >= 0) == (b > 0) else -q)


def isqrt(x: int) -> int:
    import math

    return math.isqrt(x)


def log2(x: int) -> int:
    return 0 if x == 0 else x.bit_length() - 1


CBRT_T = 115792089237316195423570985008687907853269


def cbrt(x: int) -> int:
    if x >= CBRT_T * 10**18:
        xx = x
    elif x >= CBRT_T:
        xx = uw(x * 10**18)
    else:
        xx = uw(x * 10**36)
    lg = log2(xx)
    rem = lg % 3
    a = udiv_unsafe(uw(uw(pow(2, lg // 3, M)) * uw(pow(1260, rem, M))), uw(pow(1000, rem, M)))
    for _ in range(7):
        a = udiv_unsafe(uw(uw(2 * a) + udiv_unsafe(xx, uw(a * a))), 3)
    if x >= CBRT_T * 10**18:
        a = uw(a * 10**12)
    elif x >= CBRT_T:
        a = uw(a * 10**6)
    return a


A_MULTIPLIER = 10_000


# ── twocrypto-ng ────────────────────────────────────────────────────────────


def _two_lim_mul(gamma: int, v210: bool) -> int:
    lim = 100 * 10**18
    if v210 and gamma > 2 * 10**16:
        lim = udiv_unsafe(uw(lim * 2 * 10**16), gamma)
    return lim


def two_newton_y(ann: int, gamma: int, x: list[int], d: int, i: int, lim_mul: int, v210: bool) -> int:
    n = 2
    x_j = x[1 - i]
    y = udiv(uc(d**2), uc(x_j * n**2))
    k0_i = udiv(uc(10**18 * n * x_j), d)
    if v210:
        if not (k0_i >= udiv_unsafe(10**36, lim_mul) and k0_i <= lim_mul):
            raise Revert("unsafe values x[i]")
    else:
        if not (k0_i > 10**16 * n - 1 and k0_i < 10**20 * n + 1):
            raise Revert("unsafe values x[i]")
    conv = max(max(x_j // 10**14, d // 10**14), 100)
    for _ in range(255):
        y_prev = y
        k0 = udiv(uc(uc(k0_i * y) * n), d)
        s = uc(x_j + y)
        g1k0 = uc(gamma + 10**18)
        if g1k0 > k0:
            g1k0 = uc(g1k0 - k0 + 1)
        else:
            g1k0 = uc(k0 - g1k0 + 1)
        # 10**18 * D / gamma * _g1k0 / gamma * _g1k0 * A_MULTIPLIER / ANN, left to right
        t = udiv(uc(10**18 * d), gamma)
        t = udiv(uc(t * g1k0), gamma)
        mul1 = udiv(uc(uc(t * g1k0) * A_MULTIPLIER), ann)
        mul2 = uc(10**18 + udiv(uc(2 * 10**18 * k0), g1k0))
        yfprime = uc(uc(10**18 * y) + uc(s * mul2) + mul1)
        dyfprime = uc(d * mul2)
        if yfprime < dyfprime:
            y = y_prev // 2
            continue
        yfprime -= dyfprime
        fprime = udiv(yfprime, y)
        y_minus = udiv(mul1, fprime)
        y_plus = uc(udiv(uc(yfprime + 10**18 * d), fprime) + udiv(uc(y_minus * 10**18), k0))
        y_minus = uc(y_minus + udiv(uc(10**18 * s), fprime))
        if y_plus < y_minus:
            y = y_prev // 2
        else:
            y = y_plus - y_minus
        diff = y - y_prev if y > y_prev else y_prev - y
        if diff < max(conv, y // 10**14):
            return y
    raise Revert("did not converge")


def two_get_y(ann_u: int, gamma_u: int, x: list[int], d_u: int, i: int, v210: bool) -> tuple[int, int]:
    n = 2
    max_gamma = 199 * 10**15 if v210 else 2 * 10**15
    min_a = n**n * A_MULTIPLIER // 10
    max_a = n**n * A_MULTIPLIER * 1000
    if not (ann_u > min_a - 1 and ann_u < max_a + 1):
        raise Revert("unsafe values A")
    if not (gamma_u > 10**10 - 1 and gamma_u < max_gamma + 1):
        raise Revert("unsafe values gamma")
    if not (d_u > 10**17 - 1 and d_u < 10**15 * 10**18 + 1):
        raise Revert("unsafe values D")
    lim_mul = _two_lim_mul(gamma_u, v210)
    ann, gamma, d = ann_u, gamma_u, d_u
    x_j = x[1 - i]
    gamma2 = iw(gamma * gamma)
    y = tdiv(ic(d**2), ic(x_j * n**2))
    k0_i = tdiv_unsafe(ic(10**18 * n * x_j), d)
    if v210:
        if not (k0_i >= tdiv_unsafe(10**36, lim_mul) and k0_i <= lim_mul):
            raise Revert("unsafe values x[i]")
    else:
        if not (k0_i > 10**16 * n - 1 and k0_i < 10**20 * n + 1):
            raise Revert("unsafe values x[i]")
    ann_gamma2 = ic(ann * gamma2)
    a = 10**32
    b = ic(ic(tdiv(tdiv(ic(d * ann_gamma2), 400000000), x_j) - iw(10**32 * 3)) - iw(iw(2 * gamma) * 10**14))
    c = ic(
        ic(ic(ic(iw(10**32 * 3) + iw(iw(4 * gamma) * 10**14)) + tdiv_unsafe(gamma2, 10**4))
           + tdiv_unsafe(ic(tdiv_unsafe(iw(4 * ann_gamma2), 400000000) * x_j), d))
        - tdiv_unsafe(iw(4 * ann_gamma2), 400000000)
    )
    dd = -tdiv_unsafe(ic(iw(10**18 + gamma) ** 2), 10**4)
    delta0 = ic(tdiv(ic(ic(3 * a) * c), b) - b)
    delta1 = ic(ic(ic(3 * delta0) + b) - tdiv(ic(tdiv(ic(27 * ic(a**2)), b) * dd), b))
    divider = 1
    threshold = min(min(abs(delta0), abs(delta1)), a)
    for lim, dv in ((10**48, 10**30), (10**46, 10**28), (10**44, 10**26), (10**42, 10**24),
                    (10**40, 10**22), (10**38, 10**20), (10**36, 10**18), (10**34, 10**16),
                    (10**32, 10**14), (10**30, 10**12), (10**28, 10**10), (10**26, 10**8),
                    (10**24, 10**6), (10**20, 10**2)):
        if threshold > lim:
            divider = dv
            break
    a = tdiv_unsafe(a, divider)
    b = tdiv_unsafe(b, divider)
    c = tdiv_unsafe(c, divider)
    dd = tdiv_unsafe(dd, divider)
    delta0 = ic(tdiv_unsafe(iw(iw(3 * a) * c), b) - b)
    delta1 = ic(ic(ic(3 * delta0) + b) - tdiv_unsafe(iw(tdiv_unsafe(iw(27 * ic(a**2)), b) * dd), b))
    sqrt_arg = ic(ic(delta1**2) + iw(tdiv_unsafe(ic(4 * ic(delta0**2)), b) * delta0))
    if sqrt_arg > 0:
        sqrt_val = isqrt(sqrt_arg)
    else:
        return two_newton_y(ann_u, gamma_u, x, d_u, i, lim_mul, v210), 0
    if b > 0:
        b_cbrt = cbrt(b)
    else:
        b_cbrt = -cbrt(-b)
    if delta1 > 0:
        second_cbrt = cbrt(uw(delta1 + sqrt_val) // 2)
    else:
        second_cbrt = -cbrt(udiv_unsafe(uc(iw(sqrt_val - delta1)), 2))
    c1 = tdiv_unsafe(iw(tdiv_unsafe(ic(b_cbrt**2), 10**18) * second_cbrt), 10**18)
    root = tdiv(ic(ic(iw(10**18 * c1) - iw(10**18 * b)) - ic(tdiv(iw(10**18 * b), c1) * delta0)), iw(3 * a))
    y0 = tdiv_unsafe(tdiv_unsafe(iw(tdiv_unsafe(ic(d**2), x_j) * root), 4), 10**18)
    y_out = (uc(y0), uc(root))
    frac = udiv_unsafe(uc(y_out[0] * 10**18), d_u)
    if v210:
        if not (frac >= udiv_unsafe(10**36 // n, lim_mul) and frac <= udiv_unsafe(lim_mul, n)):
            raise Revert("unsafe value for y")
    else:
        if not (frac >= 10**16 - 1 and frac < 10**20 + 1):
            raise Revert("unsafe value for y")
    return y_out


def two_fee(xp: list[int], mid_fee: int, out_fee: int, fee_gamma: int) -> int:
    f = uc(xp[0] + xp[1])
    f = udiv(uc(fee_gamma * 10**18),
             uc(uc(fee_gamma + 10**18) - udiv(uc(udiv(uc(10**18 * 4 * xp[0]), f) * xp[1]), f)))
    return udiv_unsafe(uc(uc(mid_fee * f) + uc(out_fee * uc(10**18 - f))), 10**18)


def two_get_dy(i, j, dx, balances, precisions, price_scale, d, ann, gamma,
               mid_fee, out_fee, fee_gamma, v210) -> int:
    """`_exchange` output for `dx` of coin `i` (A, gamma not ramping)."""
    bal = list(balances)
    bal[i] = uc(bal[i] + dx)
    xp = [uc(bal[0] * precisions[0]), udiv_unsafe(uc(uc(bal[1] * price_scale) * precisions[1]), 10**18)]
    y_out = two_get_y(ann, gamma, xp, d, j, v210)
    dy = uc(xp[j] - y_out[0])
    xp[j] = uc(xp[j] - dy)
    dy = uc(dy - 1)
    if j > 0:
        dy = udiv(uc(dy * 10**18), price_scale)
    dy = udiv(dy, precisions[j])
    fee = udiv_unsafe(uc(two_fee(xp, mid_fee, out_fee, fee_gamma) * dy), 10**10)
    return uc(dy - fee)


# ── tricrypto-ng ────────────────────────────────────────────────────────────


def _sort_desc(x: list[int]) -> list[int]:
    return sorted(x, reverse=True)


def tri_newton_y(ann: int, gamma: int, x: list[int], d: int, i: int) -> int:
    n = 3
    for k in range(3):
        if k != i:
            frac = udiv(uc(x[k] * 10**18), d)
            if not (frac > 10**16 - 1 and frac < 10**20 + 1):
                raise Revert("unsafe values x[i]")
    y = d // n
    k0_i = 10**18
    s_i = 0
    xs = list(x)
    xs[i] = 0
    xs = _sort_desc(xs)
    conv = max(max(xs[0] // 10**14, d // 10**14), 100)
    for jj in range(2, n + 1):
        _x = xs[n - jj]
        y = udiv(uc(y * d), uc(_x * n))
        s_i = uc(s_i + _x)
    for jj in range(n - 1):
        k0_i = udiv(uc(uc(k0_i * xs[jj]) * n), d)
    for _ in range(255):
        y_prev = y
        k0 = udiv(uc(uc(k0_i * y) * n), d)
        s = uc(s_i + y)
        g1k0 = uc(gamma + 10**18)
        if g1k0 > k0:
            g1k0 = uc(g1k0 - k0 + 1)
        else:
            g1k0 = uc(k0 - g1k0 + 1)
        # 10**18 * D / gamma * _g1k0 / gamma * _g1k0 * A_MULTIPLIER / ANN, left to right
        t = udiv(uc(10**18 * d), gamma)
        t = udiv(uc(t * g1k0), gamma)
        mul1 = udiv(uc(uc(t * g1k0) * A_MULTIPLIER), ann)
        mul2 = uc(10**18 + udiv(uc(2 * 10**18 * k0), g1k0))
        yfprime = uc(uc(10**18 * y) + uc(s * mul2) + mul1)
        dyfprime = uc(d * mul2)
        if yfprime < dyfprime:
            y = y_prev // 2
            continue
        yfprime -= dyfprime
        fprime = udiv(yfprime, y)
        y_minus = udiv(mul1, fprime)
        y_plus = uc(udiv(uc(yfprime + 10**18 * d), fprime) + udiv(uc(y_minus * 10**18), k0))
        y_minus = uc(y_minus + udiv(uc(10**18 * s), fprime))
        if y_plus < y_minus:
            y = y_prev // 2
        else:
            y = y_plus - y_minus
        diff = y - y_prev if y > y_prev else y_prev - y
        if diff < max(conv, y // 10**14):
            frac = udiv(uc(y * 10**18), d)
            if not (frac > 10**16 - 1 and frac < 10**20 + 1):
                raise Revert("unsafe value for y")
            return y
    raise Revert("did not converge")


def tri_get_y(ann_u: int, gamma_u: int, x: list[int], d_u: int, i: int) -> tuple[int, int]:
    n = 3
    if not (ann_u > n**n * A_MULTIPLIER // 100 - 1 and ann_u < n**n * A_MULTIPLIER * 1000 + 1):
        raise Revert("unsafe values A")
    if not (gamma_u > 10**10 - 1 and gamma_u < 5 * 10**16 + 1):
        raise Revert("unsafe values gamma")
    if not (d_u > 10**17 - 1 and d_u < 10**15 * 10**18 + 1):
        raise Revert("unsafe values D")
    for k in range(3):
        if k != i:
            frac = udiv(uc(x[k] * 10**18), d_u)
            if not (frac > 10**16 - 1 and frac < 10**20 + 1):
                raise Revert("unsafe values x[i]")
    j, k = {0: (1, 2), 1: (0, 2), 2: (0, 1)}[i]
    ann, gamma, d = ann_u, gamma_u, d_u
    x_j, x_k = x[j], x[k]
    gamma2 = iw(gamma * gamma)
    a = 10**36 // 27
    b = ic(
        iw(10**36 // 9 + tdiv_unsafe(iw(2 * 10**18 * gamma), 27))
        - tdiv_unsafe(tdiv_unsafe(tdiv_unsafe(ic(iw(tdiv_unsafe(iw(d * d), x_j) * gamma2) * ann), 27**2),
                                  A_MULTIPLIER), x_k)
    )
    c = ic(
        iw(10**36 // 9 + tdiv_unsafe(iw(gamma * iw(gamma + 4 * 10**18)), 27))
        + tdiv_unsafe(tdiv_unsafe(iw(tdiv_unsafe(ic(gamma2 * iw(iw(x_j + x_k) - d)), d) * ann), 27),
                      A_MULTIPLIER)
    )
    dd = tdiv_unsafe(ic(iw(10**18 + gamma) ** 2), 27)
    d0 = abs(ic(tdiv(ic(iw(3 * a) * c), b) - b))
    divider = 1
    for lim, dv in ((10**48, 10**30), (10**44, 10**26), (10**40, 10**22), (10**36, 10**18),
                    (10**32, 10**14), (10**28, 10**10), (10**24, 10**6), (10**20, 10**2)):
        if d0 > lim:
            divider = dv
            break
    if abs(a) > abs(b):
        ap = abs(tdiv_unsafe(a, b))
        a = tdiv_unsafe(iw(a * ap), divider)
        b = tdiv_unsafe(ic(b * ap), divider)
        c = tdiv_unsafe(ic(c * ap), divider)
        dd = tdiv_unsafe(ic(dd * ap), divider)
    else:
        ap = abs(tdiv_unsafe(b, a))
        a = tdiv_unsafe(tdiv(a, ap), divider)
        b = tdiv_unsafe(tdiv_unsafe(b, ap), divider)
        c = tdiv_unsafe(tdiv_unsafe(c, ap), divider)
        dd = tdiv_unsafe(tdiv_unsafe(dd, ap), divider)
    _3ac = ic(iw(3 * a) * c)
    delta0 = ic(tdiv_unsafe(_3ac, b) - b)
    delta1 = ic(ic(tdiv_unsafe(ic(3 * _3ac), b) - iw(2 * b)) - tdiv_unsafe(ic(tdiv_unsafe(ic(27 * ic(a**2)), b) * dd), b))
    sqrt_arg = ic(ic(delta1**2) + ic(tdiv_unsafe(ic(4 * ic(delta0**2)), b) * delta0))
    if sqrt_arg > 0:
        sqrt_val = isqrt(sqrt_arg)
    else:
        return tri_newton_y(ann_u, gamma_u, x, d_u, i), 0
    if b >= 0:
        b_cbrt = cbrt(b)
    else:
        b_cbrt = -cbrt(-b)
    if delta1 > 0:
        second_cbrt = cbrt(udiv_unsafe(uc(ic(delta1 + sqrt_val)), 2))
    else:
        second_cbrt = -cbrt(udiv_unsafe(uc(-ic(delta1 - sqrt_val)), 2))
    c1 = tdiv_unsafe(ic(tdiv_unsafe(ic(b_cbrt * b_cbrt), 10**18) * second_cbrt), 10**18)
    root_k0 = tdiv_unsafe(ic(ic(b + tdiv(ic(b * delta0), c1)) - c1), 3)
    root = tdiv_unsafe(ic(tdiv_unsafe(ic(tdiv_unsafe(tdiv_unsafe(ic(d * d), 27), x_k) * d), x_j) * root_k0), a)
    out = (uc(root), uc(tdiv_unsafe(ic(10**18 * root_k0), a)))
    frac = udiv_unsafe(uc(out[0] * 10**18), d_u)
    if not (frac >= 10**16 - 1 and frac < 10**20 + 1):
        raise Revert("unsafe value for y")
    return out


def tri_reduction_coefficient(x: list[int], fee_gamma: int) -> int:
    n = 3
    s = uc(uc(x[0] + x[1]) + x[2])
    k = udiv(uc(uc(10**18 * n) * x[0]), s)
    k = udiv_unsafe(uc(uc(k * n) * x[1]), s)
    k = udiv_unsafe(uc(uc(k * n) * x[2]), s)
    if fee_gamma > 0:
        k = udiv(uc(fee_gamma * 10**18), uc(uc(fee_gamma + 10**18) - k))
    return k


def tri_fee(xp: list[int], mid_fee: int, out_fee: int, fee_gamma: int) -> int:
    f = tri_reduction_coefficient(xp, fee_gamma)
    return udiv_unsafe(uc(uc(mid_fee * f) + uc(out_fee * uc(10**18 - f))), 10**18)


def tri_get_dy(i, j, dx, balances, precisions, price_scale, d, ann, gamma,
               mid_fee, out_fee, fee_gamma) -> int:
    """`_exchange` output for `dx` of coin `i`; `price_scale` has 2 entries."""
    bal = list(balances)
    bal[i] = uc(bal[i] + dx)
    xp = [uc(bal[0] * precisions[0])]
    for k in (1, 2):
        xp.append(udiv_unsafe(uc(uc(bal[k] * price_scale[k - 1]) * precisions[k]), 10**18))
    y_out = tri_get_y(ann, gamma, xp, d, j)
    dy = uc(xp[j] - y_out[0])
    xp[j] = uc(xp[j] - dy)
    dy = uc(dy - 1)
    if j > 0:
        dy = udiv(uc(dy * 10**18), price_scale[j - 1])
    dy = udiv(dy, precisions[j])
    fee = udiv_unsafe(uc(tri_fee(xp, mid_fee, out_fee, fee_gamma) * dy), 10**10)
    return uc(dy - fee)


# ── original CurveCryptoSwap2 (`newton_y` inside the pool) ──────────────────


def two_v1_get_dy(i, j, dx, balances, precisions, price_scale, d, ann, gamma,
                  mid_fee, out_fee, fee_gamma) -> int:
    """`CurveCryptoSwap2ETH._exchange` output (A, gamma not ramping)."""
    bal = list(balances)
    bal[i] = uc(bal[i] + dx)
    xp = [uc(bal[0] * precisions[0]), udiv(uc(uc(bal[1] * price_scale) * precisions[1]), 10**18)]
    y = two_newton_y(ann, gamma, xp, d, j, 100 * 10**18, False)
    frac = udiv(uc(y * 10**18), d)
    if not (frac > 10**16 - 1 and frac < 10**20 + 1):
        raise Revert("unsafe value for y")
    dy = uc(xp[j] - y)
    xp[j] = uc(xp[j] - dy)
    dy = uc(dy - 1)
    if j > 0:
        dy = udiv(uc(dy * 10**18), price_scale)
    dy = udiv(dy, precisions[j])
    return uc(dy - udiv(uc(two_fee(xp, mid_fee, out_fee, fee_gamma) * dy), 10**10))
