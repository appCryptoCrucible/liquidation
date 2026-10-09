"""Fluid DEX T1 `swapIn`, ported to exact integer arithmetic.

Sources (verified, read from Sourcify 2026-10-08):
- `FluidDexT1` core module (`main.sol` `_swapIn`) and `CoreHelpers`
  (`_getPricesAndExchangePrices`, `_getCollateralReserves`,
  `_getDebtReserves`, `_swapRoutingIn`, `_updateOracle`, the reserve checks),
  pool `0x0B1a513ee24972DAEf112bC777a5610d4325C9e7` (wstETH/ETH);
- the Liquidity layer's `FluidLiquidityUserModule` (`operate`,
  `_supplyOrWithdraw`, `_borrowOrPayback`, the ratio and utilization checks)
  and `LiquidityCalcs` (`calcExchangePrices`, withdrawal / borrow limits and
  their "after operate" updates, the V1 / V2 borrow-rate curves), at
  `0x4bDC…3Ab7`, the implementation behind `operate`.

A swap is two `LIQUIDITY.operate` calls by the pool (deposit/payback of the
input token; withdraw/borrow of the output token), so a quote is only as good
as the model of both layers. `FluidSnapshot` holds every raw word the model
reads; `swap_in` returns the output and the snapshot after the swap, or
raises `Revert` with the reason the chain would give.

Not modelled, and refused at snapshot time (the pool is then not followed):
an active range, threshold or center-price shift (their implementation is a
separate contract), a pool hook, a paused pool or token.
"""
from __future__ import annotations

from dataclasses import dataclass, field, replace

X = {n: (1 << n) - 1 for n in range(1, 257)}
E27 = 10**27
E54 = 10**54
EIGHT = 10**8
SIX = 10**6
THREE = 10**3
FOUR = 10**4
TWELVE = 10**12
EXCHANGE_PRICES_PRECISION = 10**12
ORACLE_PRECISION = 10**18
ORACLE_LIMIT = 5 * 10**16
MINIMUM_LIQUIDITY_SWAP = 10**4
SECONDS_PER_YEAR = 365 * 24 * 3600
FORCE_STORAGE_WRITE_AFTER_TIME = 24 * 3600
MAX_INPUT_AMOUNT_EXCESS = 100
MAX_TOKEN_AMOUNT_CAP = 2**127 - 1
RATIO_DEPOSIT_BORROW = 10_000
RATIO_WITHDRAW_PAYBACK = 2
MAX_NEW_AMOUNT_WHEN_RATIO_CHECK = 2**80
TOTAL_DECAY_CHECKPOINTS = 1000
MIN_DECAY_DURATION_CHECKPOINTS = 80
DECAY_CHECKPOINT_DURATION_SCALEDX10 = 36
DECAY_COEFFICIENT_SIZE = 18
DEFAULT_COEFFICIENT_SIZE = 56
DEFAULT_EXPONENT_SIZE = 8
DEFAULT_EXPONENT_MASK = 0xFF
M256 = 1 << 256


class Revert(Exception):
    """A revert, with the reason as the contracts name it."""


def chk(v: int) -> int:
    """A checked uint256 result."""
    if v < 0 or v >= M256:
        raise Revert("arithmetic over/underflow")
    return v


def isqrt(n: int) -> int:
    import math

    return math.isqrt(n)


# ---------------------------------------------------------------- BigNumber

def from_big(v: int, exp_size: int = DEFAULT_EXPONENT_SIZE) -> int:
    return (v >> exp_size) << (v & ((1 << exp_size) - 1))


def to_big(normal: int, coef_size: int, exp_size: int, round_up: bool) -> int:
    """`BigMathMinified.toBigNumber`."""
    last = normal.bit_length()
    if last < coef_size:
        last = coef_size
    exponent = last - coef_size
    coefficient = normal >> exponent
    if round_up and exponent > 0:
        coefficient += 1
        if coefficient == 1 << coef_size:
            coefficient = 1 << (coef_size - 1)
            exponent += 1
    if exponent >= 1 << exp_size:
        raise Revert("big number overflow")
    return (coefficient << exp_size) + exponent


def to_default_big(normal: int, round_up: bool) -> int:
    return to_big(normal, DEFAULT_COEFFICIENT_SIZE, DEFAULT_EXPONENT_SIZE, round_up)


# ------------------------------------------------------- Liquidity layer reads

SUPPLY_BITS = dict(mode=0, amount=1, prev_wd=65, ts=129, expand_pct=162, expand_dur=176, base_wd=200,
                   decay_amt=218, decay_dur=244, paused=255)
BORROW_BITS = dict(mode=0, amount=1, prev_limit=65, ts=129, expand_pct=162, expand_dur=176, base_limit=200,
                   max_limit=218, paused=255)
EP = dict(rate=0, fee=16, util=30, thresh=44, ts=58, supply_ep=91, borrow_ep=155, supply_ratio=219,
          borrow_ratio=234, uses_configs2=249, pause=250)


def calc_exchange_prices(cfg: int, ts: int) -> tuple[int, int]:
    """`LiquidityCalcs.calcExchangePrices`."""
    supply_ep = (cfg >> EP["supply_ep"]) & X[64]
    borrow_ep = (cfg >> EP["borrow_ep"]) & X[64]
    if supply_ep == 0 or borrow_ep == 0:
        raise Revert("exchange price zero")
    rate = cfg & X[16]
    secs = ts - ((cfg >> EP["ts"]) & X[33])
    borrow_ratio = (cfg >> EP["borrow_ratio"]) & X[15]
    if secs == 0 or rate == 0 or borrow_ratio == 1:
        return supply_ep, borrow_ep
    borrow_ep += (borrow_ep * rate * secs) // (SECONDS_PER_YEAR * FOUR)
    t = (cfg >> EP["supply_ratio"]) & X[15]
    if t == 1:
        return supply_ep, borrow_ep
    util = (cfg >> EP["util"]) & X[14]
    if t & 1 == 1:
        t >>= 1
        t = (E27 * FOUR) // t
        t = (util * (E27 + t)) // FOUR
    else:
        t >>= 1
        t = (E27 * util * (FOUR + t)) // (FOUR * FOUR)
    if borrow_ratio & 1 == 1:
        borrow_ratio >>= 1
        borrow_ratio = (borrow_ratio * E27) // (FOUR + borrow_ratio)
    else:
        borrow_ratio >>= 1
        borrow_ratio = E27 - ((borrow_ratio * E27) // (FOUR + borrow_ratio))
    t = (FOUR * t * borrow_ratio) // E54
    t = rate * t * (FOUR - ((cfg >> EP["fee"]) & X[14]))
    supply_ep += (supply_ep * t * secs) // (SECONDS_PER_YEAR * FOUR * FOUR * FOUR)
    return supply_ep, borrow_ep


def calc_withdrawal_limit_before(data: int, user_supply: int, ts: int) -> int:
    """`calcWithdrawalLimitBeforeOperate`: the amount that must stay supplied."""
    last = from_big((data >> SUPPLY_BITS["prev_wd"]) & X[64])
    if last == 0:
        return 0
    max_wd = (((data >> SUPPLY_BITS["expand_pct"]) & X[14]) * user_supply) // FOUR
    elapsed = ts - ((data >> SUPPLY_BITS["ts"]) & X[33])
    dur = (data >> SUPPLY_BITS["expand_dur"]) & X[24]
    t = (max_wd * elapsed) // dur
    cur = last - t if last > t else 0
    minimum = user_supply - max_wd
    return minimum if minimum > cur else cur


def calc_withdrawal_limit_after(data: int, user_supply: int, new_limit: int) -> int:
    base = from_big((data >> SUPPLY_BITS["base_wd"]) & X[18])
    if user_supply < base:
        return 0
    pct = (data >> SUPPLY_BITS["expand_pct"]) & X[14]
    minimum = user_supply - ((user_supply * pct) // FOUR)
    return minimum if minimum > new_limit else new_limit


def calc_borrow_limit_before(data: int, user_borrow: int, ts: int) -> int:
    pct = (data >> BORROW_BITS["expand_pct"]) & X[14]
    max_expansion = (user_borrow * pct) // FOUR
    max_expanded = user_borrow + max_expansion
    cur = from_big((data >> BORROW_BITS["base_limit"]) & X[18])
    if max_expanded < cur:
        return cur
    elapsed = ts - ((data >> BORROW_BITS["ts"]) & X[33])
    dur = (data >> BORROW_BITS["expand_dur"]) & X[24]
    cur = (max_expansion * elapsed) // dur + from_big((data >> BORROW_BITS["prev_limit"]) & X[64])
    if cur > max_expanded:
        cur = max_expanded
    hard = from_big((data >> BORROW_BITS["max_limit"]) & X[18])
    return hard if cur > hard else cur


def calc_borrow_limit_after(data: int, user_borrow: int, new_limit: int) -> int:
    pct = (data >> BORROW_BITS["expand_pct"]) & X[14]
    limit = user_borrow + ((user_borrow * pct) // FOUR)
    base = from_big((data >> BORROW_BITS["base_limit"]) & X[18])
    if limit < base:
        return base
    hard = from_big((data >> BORROW_BITS["max_limit"]) & X[18])
    if limit > hard:
        limit = hard
    return limit if new_limit > limit else new_limit


def _tdiv(a: int, b: int) -> int:
    q = abs(a) // abs(b)
    return q if (a >= 0) == (b > 0) else -q


def _rate_line(rate_data, utilization, y1, y2, x1, x2):
    slope = _tdiv((y2 - y1) * TWELVE, x2 - x1)
    const = y1 * TWELVE - slope * x1
    v = slope * utilization + const
    if v < 0:
        raise Revert("borrow rate negative")
    return v // TWELVE


def calc_borrow_rate(rate_data: int, util: int) -> int:
    ver = rate_data & 0xF
    g = lambda lo: (rate_data >> lo) & X[16]  # noqa: E731
    if ver == 1:
        kink = g(20)
        if util < kink:
            r = _rate_line(rate_data, util, g(4), g(36), 0, kink)
        else:
            r = _rate_line(rate_data, util, g(36), g(52), kink, FOUR)
    elif ver == 2:
        k1, k2 = g(20), g(52)
        if util < k1:
            r = _rate_line(rate_data, util, g(4), g(36), 0, k1)
        elif util < k2:
            r = _rate_line(rate_data, util, g(36), g(68), k1, k2)
        else:
            r = _rate_line(rate_data, util, g(68), g(84), k2, FOUR)
    else:
        raise Revert("unsupported rate version")
    return min(r, X[16])


# ------------------------------------------------------- Liquidity token state

@dataclass
class LiqToken:
    """One token's words in the Liquidity layer, for the pool as a user."""
    ep_cfg: int          # `_exchangePricesAndConfig[token]`
    totals: int          # `_totalAmounts[token]`
    configs2: int        # `_configs2[token]` (max utilization in the low 14 bits)
    rate_data: int       # `_rateData[token]`
    supply: int          # `_userSupplyData[pool][token]`
    borrow: int          # `_userBorrowData[pool][token]`
    balance: int         # the layer's own balance of the token (ETH for native)


def _mask_bits(v, lo, size):
    return v & ~(X[size] << lo)


def operate(tok: LiqToken, supply_amount: int, borrow_amount: int, ts: int, pulled: int = 0) -> LiqToken:
    """`FluidLiquidityUserModule.operate` as the DEX calls it: supply (+) /
    withdraw (-) and borrow (+) / payback (-) of one token, `pulled` tokens
    transferred in. Returns the token's words after, or raises `Revert`."""
    if supply_amount == 0 and borrow_amount == 0:
        raise Revert("operate amounts zero")
    cfg = tok.ep_cfg
    if cfg >> EP["pause"] > 0:
        raise Revert("token paused")
    sep, bep = calc_exchange_prices(cfg, ts)
    totals = tok.totals
    s_raw = from_big(totals & X[64])
    s_free = from_big((totals >> 64) & X[64])
    b_raw = from_big((totals >> 128) & X[64])
    b_free = from_big(totals >> 192)

    supply_data, borrow_data = tok.supply, tok.borrow
    balance = tok.balance + pulled

    def ratio_check(new_amount, existing, is_deposit_borrow):
        existing = RATIO_DEPOSIT_BORROW * existing if is_deposit_borrow else existing // RATIO_WITHDRAW_PAYBACK
        if new_amount > MAX_NEW_AMOUNT_WHEN_RATIO_CHECK and new_amount > existing:
            raise Revert("operate amount ratio excess")

    if supply_amount != 0:
        before = totals
        data = supply_data
        if data == 0:
            raise Revert("user not defined")
        if (data >> SUPPLY_BITS["paused"]) & 1:
            raise Revert("user paused")
        user_supply = from_big((data >> SUPPLY_BITS["amount"]) & X[64])
        decay = from_big((data >> SUPPLY_BITS["decay_amt"]) & X[26])
        decay_cps = (data >> SUPPLY_BITS["decay_dur"]) & X[10]
        if decay > 0:
            decayed = (ts * 10) // DECAY_CHECKPOINT_DURATION_SCALEDX10 - (
                (((data >> SUPPLY_BITS["ts"]) & X[33]) * 10) // DECAY_CHECKPOINT_DURATION_SCALEDX10)
            if decayed < decay_cps:
                decay = decay - (decay * decayed) // decay_cps
                decay_cps = decay_cps - decayed
            else:
                decay = 0
                decay_cps = 0
        wd_before = calc_withdrawal_limit_before(data, user_supply, ts)
        new_raw = new_free = 0
        if data & 1 == 1:
            if supply_amount > 0:
                new_raw = (supply_amount * EXCHANGE_PRICES_PRECISION) // sep
                user_supply += new_raw
            else:
                new_raw = -((-supply_amount * EXCHANGE_PRICES_PRECISION + sep - 1) // sep)  # -mulDivUp
                if -new_raw > user_supply:
                    raise Revert("withdraw more than supply")
                user_supply -= -new_raw
        else:
            new_free = supply_amount
            if new_free > 0:
                user_supply += new_free
            else:
                if -new_free > user_supply:
                    raise Revert("withdraw more than supply")
                user_supply -= -new_free
        check_decay_expansion = False
        if supply_amount < 0:
            if user_supply < wd_before:
                raise Revert("withdrawal limit reached")
            if decay > 0:
                wd_amount = -(new_raw + new_free)
                if wd_amount > decay:
                    wd_before = wd_before - decay if wd_before > decay else 0
                    decay = 0
                else:
                    wd_before = wd_before - wd_amount if wd_before > wd_amount else 0
                    decay -= wd_amount
                check_decay_expansion = True
        wd_after = calc_withdrawal_limit_after(data, user_supply, wd_before)
        if wd_after == 0:
            decay = 0
        elif wd_before != wd_after:
            if supply_amount > 0:
                if wd_before == 0:
                    wd_before = from_big((data >> SUPPLY_BITS["base_wd"]) & X[18])
                if wd_after > wd_before:
                    new_decay = wd_after - wd_before
                    decay_cps = (decay_cps * decay + TOTAL_DECAY_CHECKPOINTS * new_decay) // (decay + new_decay)
                    if decay_cps < MIN_DECAY_DURATION_CHECKPOINTS:
                        decay_cps = MIN_DECAY_DURATION_CHECKPOINTS
                    decay += new_decay
                else:
                    decay = 0
                    decay_cps = 0
            elif check_decay_expansion:
                not_pushed = wd_after - wd_before if wd_after > wd_before else 0
                decay += not_pushed
        if decay < 10:
            decay = 0
            decay_cps = 0
        else:
            decay = to_big(decay, DECAY_COEFFICIENT_SIZE, DEFAULT_EXPONENT_SIZE, False)
            if decay_cps > TOTAL_DECAY_CHECKPOINTS:
                decay_cps = TOTAL_DECAY_CHECKPOINTS
            elif decay_cps == 0:
                decay_cps = 1
        user_big = to_default_big(user_supply, False)
        if (data >> SUPPLY_BITS["amount"]) & X[64] == user_big:
            raise Revert("operate amount insufficient")
        wd_after_big = to_default_big(wd_after, False)
        decay_stored = decay if decay == 0 else decay  # already a big number
        supply_data = (
            (data & 0xC000000003FFFFFFFFFFFFFC0000000000000000000000000000000000000001)
            | (user_big << SUPPLY_BITS["amount"])
            | (wd_after_big << SUPPLY_BITS["prev_wd"])
            | (ts << SUPPLY_BITS["ts"])
            | (decay_stored << SUPPLY_BITS["decay_amt"])
            | (decay_cps << SUPPLY_BITS["decay_dur"])
        )
        # totals
        if new_free == 0:
            if new_raw > 0:
                ratio_check(new_raw, s_raw, True)
                s_raw += new_raw
            else:
                ratio_check(-new_raw, s_raw, False)
                s_raw = s_raw - (-new_raw) if s_raw > -new_raw else 0
            totals = (totals & 0xffffffffffffffffffffffffffffffffffffffffffffffff0000000000000000) | to_default_big(s_raw, False)
        else:
            if new_free > 0:
                ratio_check(new_free, s_free, True)
                s_free += new_free
            else:
                ratio_check(-new_free, s_free, False)
                s_free = s_free - (-new_free) if s_free > -new_free else 0
            if s_free > MAX_TOKEN_AMOUNT_CAP:
                raise Revert("total supply overflow")
            totals = (totals & 0xffffffffffffffffffffffffffffffff0000000000000000ffffffffffffffff) | (
                to_default_big(s_free, False) << 64)
        if before == totals:
            raise Revert("operate amount insufficient")
    if borrow_amount != 0:
        before = totals
        data = borrow_data
        if data == 0:
            raise Revert("user not defined")
        if (data >> BORROW_BITS["paused"]) & 1:
            raise Revert("user paused")
        user_borrow = from_big((data >> BORROW_BITS["amount"]) & X[64])
        new_limit = calc_borrow_limit_before(data, user_borrow, ts)
        new_raw = new_free = 0
        if data & 1 == 1:
            if borrow_amount > 0:
                new_raw = -(-(borrow_amount * EXCHANGE_PRICES_PRECISION) // bep)  # mulDivUp
                user_borrow += new_raw
            else:
                new_raw = _tdiv(borrow_amount * EXCHANGE_PRICES_PRECISION, bep)
                if -new_raw > user_borrow:
                    raise Revert("payback more than borrow")
                user_borrow -= -new_raw
        else:
            new_free = borrow_amount
            if new_free > 0:
                user_borrow += new_free
            else:
                if -new_free > user_borrow:
                    raise Revert("payback more than borrow")
                user_borrow -= -new_free
        if borrow_amount > 0 and user_borrow > new_limit:
            raise Revert("borrow limit reached")
        new_limit = calc_borrow_limit_after(data, user_borrow, new_limit)
        user_big = to_default_big(user_borrow, True)
        if (data >> BORROW_BITS["amount"]) & X[64] == user_big:
            raise Revert("operate amount insufficient")
        limit_big = to_default_big(new_limit, False)
        borrow_data = (
            (data & 0xfffffffffffffffffffffffc0000000000000000000000000000000000000001)
            | (user_big << BORROW_BITS["amount"])
            | (limit_big << BORROW_BITS["prev_limit"])
            | (ts << BORROW_BITS["ts"])
        )
        if new_free == 0:
            if new_raw > 0:
                ratio_check(new_raw, b_raw, True)
                b_raw += new_raw
            else:
                ratio_check(-new_raw, b_raw, False)
                b_raw = b_raw - (-new_raw) if b_raw > -new_raw else 0
            totals = (totals & 0xffffffffffffffff0000000000000000ffffffffffffffffffffffffffffffff) | (
                to_default_big(b_raw, True) << 128)
        else:
            if new_free > 0:
                ratio_check(new_free, b_free, True)
                b_free += new_free
            else:
                ratio_check(-new_free, b_free, False)
                b_free = b_free - (-new_free) if b_free > -new_free else 0
            if b_free > MAX_TOKEN_AMOUNT_CAP:
                raise Revert("total borrow overflow")
            totals = (totals & 0x0000000000000000ffffffffffffffffffffffffffffffffffffffffffffffff) | (
                to_default_big(b_free, True) << 192)
        if before == totals:
            raise Revert("operate amount insufficient")
    # exchange prices / utilization / ratios
    s_with = (s_raw * sep) // EXCHANGE_PRICES_PRECISION
    if s_with > MAX_TOKEN_AMOUNT_CAP and supply_amount > 0:
        raise Revert("total supply overflow")
    total_supply = s_free + s_with
    if s_with > s_free:
        s_ratio = ((s_free * FOUR) // s_with) << 1
    elif s_with < s_free:
        s_ratio = (((s_with * FOUR) // s_free) << 1) | 1
    elif total_supply > 0:
        s_ratio = FOUR << 1
    else:
        s_ratio = 0
    b_with = (b_raw * bep) // EXCHANGE_PRICES_PRECISION
    if b_with > MAX_TOKEN_AMOUNT_CAP and borrow_amount > 0:
        raise Revert("total borrow overflow")
    total_borrow = b_free + b_with
    if b_with > b_free:
        b_ratio = ((b_free * FOUR) // b_with) << 1
    elif b_with < b_free:
        b_ratio = (((b_with * FOUR) // b_free) << 1) | 1
    elif total_borrow > 0:
        b_ratio = FOUR << 1
    else:
        b_ratio = 0
    util = 0
    if total_supply > 0:
        util = (total_borrow * FOUR) // total_supply
        if borrow_amount > 0:
            max_util = (tok.configs2 & X[14]) if (cfg >> EP["uses_configs2"]) & 1 else FOUR
            if util > max_util:
                raise Revert("max utilization reached")
    write = ts > ((cfg >> EP["ts"]) & X[33]) + FORCE_STORAGE_WRITE_AFTER_TIME
    if not write:
        last_util = (cfg >> EP["util"]) & X[14]
        thr = (cfg >> EP["thresh"]) & X[14]
        write = abs(util - last_util) > thr
        if not write:
            last = (cfg >> EP["supply_ratio"]) & X[15]
            if (last & 1) == (s_ratio & 1):
                write = abs((s_ratio >> 1) - (last >> 1)) > thr
            else:
                write = True
            if not write:
                last = (cfg >> EP["borrow_ratio"]) & X[15]
                if (last & 1) == (b_ratio & 1):
                    write = abs((b_ratio >> 1) - (last >> 1)) > thr
                else:
                    write = True
    if write:
        rate = calc_borrow_rate(tok.rate_data, util)
        if sep > X[64] or bep > X[64]:
            raise Revert("exchange price overflow")
        if util > X[14]:
            raise Revert("utilization overflow")
        cfg = (
            (cfg & 0xfe000000000000000000000000000000000000000000000003fff0003fff0000)
            | rate
            | (util << EP["util"])
            | (ts << EP["ts"])
            | (sep << EP["supply_ep"])
            | (bep << EP["borrow_ep"])
            | (s_ratio << EP["supply_ratio"])
            | (b_ratio << EP["borrow_ratio"])
        )
    # the layer must hold what it pays out
    out = (-supply_amount if supply_amount < 0 else 0) + (borrow_amount if borrow_amount > 0 else 0)
    if out > balance:
        raise Revert("liquidity layer balance")
    balance -= out
    return replace(tok, ep_cfg=cfg, totals=totals, supply=supply_data, borrow=borrow_data, balance=balance)


# ------------------------------------------------------------------ the pool

@dataclass
class FluidSnapshot:
    token0: str
    token1: str
    num0: int
    den0: int
    num1: int
    den1: int
    dex_vars: int
    dex_vars2: int
    center_ext: int | None       # the center price hook's `centerPrice()`, if the pool has one
    tokens: list[LiqToken] = field(default_factory=list)
    native: tuple[bool, bool] = (False, False)


def center_and_ranges(snap: FluidSnapshot, ts: int):
    """The first half of `_getPricesAndExchangePrices`, for a pool with no
    active shift. Returns `(center, upper, lower, geometric_mean, last_stored)`."""
    dv1, dv2 = snap.dex_vars, snap.dex_vars2
    if (dv2 >> 248) & 1:
        raise Revert("center price shift active (not followed)")
    if (dv2 >> 26) & 1:
        raise Revert("range shift active (not followed)")
    center = (dv2 >> 112) & X[30]
    if center == 0:
        center = from_big((dv1 >> 81) & X[40])
    else:
        if snap.center_ext is None:
            raise Revert("center price hook unread")
        center = snap.center_ext
    last_stored = from_big((dv1 >> 41) & X[40])
    upper_pct = (dv2 >> 27) & X[20]
    lower_pct = (dv2 >> 47) & X[20]
    upper = (center * SIX) // (SIX - upper_pct)
    lower = (center * (SIX - lower_pct)) // SIX
    changed = False
    if ((dv2 >> 68) & X[20]) > 0:
        if (dv2 >> 67) & 1:
            raise Revert("threshold shift active (not followed)")
        up_thr = (dv2 >> 68) & X[10]
        lo_thr = (dv2 >> 78) & X[10]
        shifting_time = (dv2 >> 88) & X[24]
        if last_stored > center + ((upper - center) * (THREE - up_thr)) // THREE:
            elapsed = ts - ((dv1 >> 121) & X[33])
            if elapsed < shifting_time:
                center = center + ((upper - center) * elapsed) // shifting_time
            else:
                center = upper
            changed = True
        elif last_stored < center - ((center - lower) * (THREE - lo_thr)) // THREE:
            elapsed = ts - ((dv1 >> 121) & X[33])
            if elapsed < shifting_time:
                center = center - ((center - lower) * elapsed) // shifting_time
            else:
                center = lower
            changed = True
    mx = from_big((dv2 >> 172) & X[28])
    if center > mx:
        center = mx
        changed = True
    else:
        mn = from_big((dv2 >> 200) & X[28])
        if center < mn:
            center = mn
            changed = True
    if changed:
        upper = (center * SIX) // (SIX - upper_pct)
        lower = (center * (SIX - lower_pct)) // SIX
    if upper < 10**38:
        gm = isqrt(upper * lower)
    else:
        gm = isqrt((upper // 10**18) * (lower // 10**18)) * 10**18
    return center, upper, lower, gm, last_stored


def reserves_outside_range(gp, pa, rx, ry):
    p1 = pa - gp
    p2 = (gp * rx + ry * E27) // (2 * p1)
    p3 = rx * ry
    p3 = (p3 * E27) // p1 if p3 < 10**50 else (p3 // p1) * E27
    xa = p2 + isqrt(p3 + p2 * p2)
    yb = (xa * gp) // E27
    return xa, yb


def col_reserves(snap, gm, upper, lower, sep0, sep1):
    def supply(tok: LiqToken, ep, num, den):
        data = tok.supply
        amt = from_big((data >> SUPPLY_BITS["amount"]) & X[64])
        if data & 1:
            amt = (amt * ep) // EXCHANGE_PRICES_PRECISION
        return (amt * num) // den

    s0 = supply(snap.tokens[0], sep0, snap.num0, snap.den0)
    s1 = supply(snap.tokens[1], sep1, snap.num1, snap.den1)
    if gm < E27:
        i0, i1 = reserves_outside_range(gm, upper, s0, s1)
    else:
        i1, i0 = reserves_outside_range(E54 // gm, E54 // lower, s1, s0)
    return dict(r0=s0, r1=s1, i0=i0 + s0, i1=i1 + s1)


def calculate_debt_reserves(gp, pb, dx, dy):
    p1 = _tdiv(dx * gp - dy * E27, 2 * E27)
    p2 = dx * dy
    p2 = (p2 * pb) // E27 if p2 < 10**50 else (p2 // E27) * pb
    ry = p1 + isqrt(p2 + p1 * p1)
    if ry < 0:
        raise Revert("negative")
    iry = ry * E27 - dx * pb
    if iry < SIX:
        raise Revert("debt reserves too low")
    if ry < 10**25:
        iry = (ry * ry * E27) // iry
    else:
        iry = (ry * ry) // (iry // E27)
    irx = ((iry * dx) // ry) - dx
    rx = (irx * dy) // (iry + dy)
    return rx, ry, irx, iry


def debt_reserves(snap, gm, upper, lower, bep0, bep1):
    def debt(tok: LiqToken, ep, num, den):
        data = tok.borrow
        amt = from_big((data >> BORROW_BITS["amount"]) & X[64])
        if data & 1:
            amt = (amt * ep) // EXCHANGE_PRICES_PRECISION
        return (amt * num) // den

    d0 = debt(snap.tokens[0], bep0, snap.num0, snap.den0)
    d1 = debt(snap.tokens[1], bep1, snap.num1, snap.den1)
    if gm < E27:
        r0, r1, i0, i1 = calculate_debt_reserves(gm, lower, d0, d1)
    else:
        r1, r0, i1, i0 = calculate_debt_reserves(E54 // gm, E54 // upper, d1, d0)
    return dict(d0=d0, d1=d1, r0=r0, r1=r1, i0=i0, i1=i1)


def amount_out(amount_in, i_in, i_out):
    return (amount_in * i_out) // (i_in + amount_in)


def swap_routing_in(t, x, y, x2, y2):
    xy = isqrt(x * y * 10**18)
    x2y2 = isqrt(x2 * y2 * 10**18)
    return _tdiv(y2 * xy + t * xy - y * x2y2, xy + x2y2)


def price_diff_check(old, new):
    d = ORACLE_PRECISION - (old * ORACLE_PRECISION) // new
    if d > ORACLE_LIMIT or d < -ORACLE_LIMIT:
        raise Revert("oracle update huge swap diff")
    return d


def swap_in(snap: FluidSnapshot, swap0to1: bool, amount_in: int, ts: int):
    """`FluidDexT1._swapIn` through both `LIQUIDITY.operate` calls. Returns
    `(amount_out, snapshot_after)`; raises `Revert`."""
    dv1, dv2 = snap.dex_vars, snap.dex_vars2
    if dv2 >> 255 == 1:
        raise Revert("swap and arbitrage paused")
    if dv1 & 1 == 1:
        raise Revert("already entered")
    if dv2 & 3 == 0:
        raise Revert("pool not initialized")
    if swap0to1:
        n_in, d_in, n_out, d_out = snap.num0, snap.den0, snap.num1, snap.den1
    else:
        n_in, d_in, n_out, d_out = snap.num1, snap.den1, snap.num0, snap.den0
    adj = (amount_in * n_in) // d_in
    if adj < SIX or adj > X[96] or amount_in < 100 or amount_in > X[128]:
        raise Revert("limiting amounts swap and non perfect actions")
    center, upper, lower, gm, last_stored = center_and_ranges(snap, ts)
    sep0, bep0 = calc_exchange_prices(snap.tokens[0].ep_cfg, ts)
    sep1, bep1 = calc_exchange_prices(snap.tokens[1].ep_cfg, ts)
    smart_col = dv2 & 1
    smart_debt = (dv2 >> 1) & 1
    fee_raw = (dv2 >> 2) & X[17]
    revenue_cut = EIGHT - (((dv2 >> 19) & X[7]) * fee_raw)
    fee = SIX - fee_raw

    cs = dict(in_real=0, out_real=0, in_imag=0, out_imag=0)
    ds = dict(in_debt=0, out_debt=0, in_real=0, out_real=0, in_imag=0, out_imag=0)
    if smart_col:
        c = col_reserves(snap, gm, upper, lower, sep0, sep1)
        if swap0to1:
            cs = dict(in_real=c["r0"], out_real=c["r1"], in_imag=c["i0"], out_imag=c["i1"])
        else:
            cs = dict(in_real=c["r1"], out_real=c["r0"], in_imag=c["i1"], out_imag=c["i0"])
    if smart_debt:
        d = debt_reserves(snap, gm, upper, lower, bep0, bep1)
        if swap0to1:
            ds = dict(in_debt=d["d0"], out_debt=d["d1"], in_real=d["r0"], out_real=d["r1"], in_imag=d["i0"], out_imag=d["i1"])
        else:
            ds = dict(in_debt=d["d1"], out_debt=d["d0"], in_real=d["r1"], out_real=d["r0"], in_imag=d["i1"], out_imag=d["i0"])
    if adj > (cs["in_imag"] + ds["in_imag"]) // 2:
        raise Revert("swap in limiting amounts")
    routing = 0
    if smart_col and smart_debt:
        routing = swap_routing_in(adj, cs["out_imag"], cs["in_imag"], ds["out_imag"], ds["in_imag"])
    if adj > routing and routing > 0:
        t_col, t_debt = routing, adj - routing
    elif (smart_col and not smart_debt) or routing >= adj:
        t_col, t_debt = adj, 0
    elif (not smart_col and smart_debt) or routing <= 0:
        t_col, t_debt = 0, adj
    else:
        raise Revert("no swap route")
    o_col = o_debt = 0
    center_price = center

    def verify(real_in, real_out, add, take):
        if swap0to1:
            r0, r1 = real_in + add, chk(real_out - take)
            if r1 < (r0 * center_price) // (E27 * MINIMUM_LIQUIDITY_SWAP):
                raise Revert("token reserves too low")
        else:
            r0, r1 = chk(real_out - take), real_in + add
            if r0 < (r1 * E27) // (center_price * MINIMUM_LIQUIDITY_SWAP):
                raise Revert("token reserves too low")

    if t_col > 0:
        o_col = amount_out((t_col * fee) // SIX, cs["in_imag"], cs["out_imag"])
        verify(cs["in_real"], cs["out_real"], t_col, o_col)
    if t_debt > 0:
        o_debt = amount_out((t_debt * fee) // SIX, ds["in_imag"], ds["out_imag"])
        verify(ds["in_real"], ds["out_real"], t_debt, o_debt)
    t_col = (t_col * revenue_cut) // EIGHT
    t_debt = (t_debt * revenue_cut) // EIGHT
    if t_col > t_debt:
        if swap0to1:
            price = ((cs["out_imag"] - o_col) * E27) // (cs["in_imag"] + t_col)
        else:
            price = ((cs["in_imag"] + t_col) * E27) // (cs["out_imag"] - o_col)
    else:
        if swap0to1:
            price = ((ds["out_imag"] - o_debt) * E27) // (ds["in_imag"] + t_debt)
        else:
            price = ((ds["in_imag"] + t_debt) * E27) // (ds["out_imag"] - o_debt)
    t_col = (t_col * d_in) // n_in
    t_debt = (t_debt * d_in) // n_in
    o_col = (o_col * d_out) // n_out
    o_debt = (o_debt * d_out) // n_out
    out = o_col + o_debt

    # --- the two `LIQUIDITY.operate` calls
    i_in, i_out = (0, 1) if swap0to1 else (1, 0)
    pulled = amount_in  # the pool pulls the whole amount in, revenue cut included
    if (dv2 >> 142) & X[30]:
        raise Revert("pool hook (not followed)")
    toks = list(snap.tokens)
    # input token: deposit `t_col`, pay back `t_debt`; the pulled amount must
    # be within [credited, credited * 1.01] of what is credited
    credited = t_col + t_debt
    if pulled < credited or pulled > (credited * (FOUR + MAX_INPUT_AMOUNT_EXCESS)) // FOUR:
        # `_checkEnforceTotalInputAmount`: outside the band the default input
        # is enforced, and the transfer then differs from it
        raise Revert("transfer amount out of bounds")
    native_in = snap.native[i_in]
    if native_in:
        pass  # msg.value == amountIn; covered by the same band
    toks[i_in] = operate(toks[i_in], t_col, -t_debt, ts, pulled=pulled)
    toks[i_out] = operate(toks[i_out], -o_col, o_debt, ts)
    # utilization limit of the output token, read after both operations
    limit = (dv2 >> 238) & X[10] if swap0to1 else (dv2 >> 228) & X[10]
    if limit < THREE:
        util = (toks[i_out].ep_cfg >> EP["util"]) & X[14]
        if util > limit * 10:
            raise Revert("liquidity layer token utilization cap reached")
    # --- `_updateOracle` (the parts that gate the swap and the next quote)
    time_diff = ts - ((dv1 >> 121) & X[33])
    if time_diff == 0:
        old_center = from_big((dv1 >> 81) & X[40])
        if center < ((EIGHT - 1) * old_center) // EIGHT or center > ((EIGHT + 1) * old_center) // EIGHT:
            raise Revert("center price out of range")
        older = from_big((dv1 >> 1) & X[40])
        price_diff_check(older, price)
        new_dv1 = (dv1 & 0xfffffffffffffffffffffffffffffffffffffffffffe0000000001ffffffffff) | (
            to_big(price, 32, 8, False) << 41)
    else:
        last = from_big((dv1 >> 41) & X[40])
        price_diff_check(last, price)
        if ((dv1 >> 195) & 1) == 0:
            new_dv1 = (
                (dv1 & 0xfffffffffffffffffffffffffc00000000000000000000000000000000000001)
                | (((dv1 >> 41) & X[40]) << 1)
                | (to_big(price, 32, 8, False) << 41)
                | (to_big(center, 32, 8, False) << 81)
                | (ts << 121)
            )
        else:
            # oracle active: the same price fields, plus oracle bookkeeping the
            # quote never reads (slots, time diff)
            td = min(time_diff, X[22])
            new_dv1 = (
                (dv1 & 0xfffffffffffffff8000000000000000000000000000000000000000000000001)
                | (((dv1 >> 41) & X[40]) << 1)
                | (to_big(price, 32, 8, False) << 41)
                | (to_big(center, 32, 8, False) << 81)
                | (ts << 121)
                | (td << 154)
                | (((dv1 >> 176) & X[3]) << 176)
                | (((dv1 >> 179) & X[16]) << 179)
            )
    after = replace(snap, tokens=toks, dex_vars=new_dv1)
    return out, after
